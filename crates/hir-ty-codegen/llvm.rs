//! LLVM as the producer of a function's machine code.
//!
//! 1. [`lower`] turns the Cranelift IR of the function into a module of one LLVM function.
//!    What the function refers to is declared there under the names of [`Symbol`].
//! 2. LLVM optimizes the module, if asked to, and writes an object file into memory.
//! 3. [`emitted`] reads the object back: the code, the constants LLVM put beside it, the
//!    relocations, with their [`Symbol`]s turned back into requests, and the unwind table.
//!
//! Anything on the way that this has no answer for is an [`Unhandled`], and the function is
//! compiled by Cranelift instead.

use std::{cell::RefCell, collections::hash_map::Entry, fmt, sync::Once};

use cranelift_codegen::ir::{self, Opcode, Type};
use inkwell::{
    OptimizationLevel,
    context::Context,
    llvm_sys::support::LLVMParseCommandLineOptions,
    passes::PassBuilderOptions,
    targets::{
        CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine, TargetTriple,
    },
};
use rustc_hash::FxHashMap;

use super::{
    Arch,
    backend::{Optimization, Passes, Phase, Selection, Tally},
    fragment::{Code, Recorder},
};

mod emitted;
mod lower;

/// The name of the function being compiled, in the module and in the object.
const OWN: &str = "rustd.own";
const PERSONALITY: &str = "rust_eh_personality";

pub(super) fn emit(
    recorder: &mut Recorder<'_>,
    func: &ir::Function,
    optimization: Optimization,
) -> Result<Code, Unhandled> {
    let tally = Tally::global();
    // LLVM's optimizing register allocator takes more than linear time, as Cranelift's does:
    // one function of dx kept it busy for minutes. Such a function is compiled without
    // optimization, and counted.
    let optimization = if func.dfg.num_insts() > super::LARGE && optimization != Optimization::NONE
    {
        tally.large.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Optimization::NONE
    } else {
        optimization
    };
    Machine::with(optimization, |context, machine| {
        let module = context.create_module("rustd");
        let named: Vec<ir::StackSlot> = recorder.named_slots().collect();
        module.set_triple(&machine.get_triple());
        module.set_data_layout(&machine.get_target_data().get_data_layout());
        let constants: FxHashMap<u32, &[u8]> = recorder.constants().collect();
        tally.time(Phase::Lower, || {
            lower::Lowering::lower(context, &module, func, &named, &constants)
        })?;
        if std::env::var_os("RUSTD_DUMP_LLVM").is_some() {
            eprintln!("{}", module.print_to_string().to_string_lossy());
        }
        // LLVM aborts the process on IR it does not accept, so it is checked first.
        tally
            .time(Phase::Verify, || module.verify())
            .map_err(|e| Unhandled::Llvm(e.to_string_lossy().into_owned()))?;
        if let Some(pipeline) = optimization.pipeline() {
            tally
                .time(Phase::Optimize, || {
                    module.run_passes(pipeline, machine, PassBuilderOptions::create())
                })
                .map_err(|e| Unhandled::Llvm(e.to_string_lossy().into_owned()))?;
        }
        let object = tally
            .time(Phase::Emit, || machine.write_to_memory_buffer(&module, FileType::Object))
            .map_err(|e| Unhandled::Llvm(e.to_string_lossy().into_owned()))?;
        tally.time(Phase::Read, || {
            emitted::Emitted::code(object.as_slice(), func.signature.clone(), recorder)
        })
    })
}

/// The object file LLVM's assembler makes of `text`, which is assembly in the syntax
/// `asm!` is written in.
///
/// FIXME: assembly LLVM rejects ends the process: its default diagnostic handler exits on an
/// error, and inkwell has no way to set another.
pub(super) fn assemble(text: &str) -> Result<Vec<u8>, String> {
    Machine::with(Optimization::NONE, |context, machine| {
        let module = context.create_module("rustd_asm");
        module.set_triple(&machine.get_triple());
        // What rustc wraps `global_asm!` in, where there are two syntaxes.
        let text = match super::Target::selected().map_err(Unhandled::Llvm)?.arch {
            Arch::X86_64 => format!(".intel_syntax noprefix\n{text}\n.att_syntax\n"),
            Arch::Aarch64 | Arch::Wasm32 => format!("{text}\n"),
        };
        module.set_inline_assembly(&text);
        let object = machine
            .write_to_memory_buffer(&module, FileType::Object)
            .map_err(|e| Unhandled::Llvm(e.to_string_lossy().into_owned()))?;
        Ok(object.as_slice().to_vec())
    })
    .map_err(|e| format!("inline assembly: {e}"))
}

/// Sets the options of LLVM that only its command line reaches, for the process.
///
/// The SLP vectorizer is off. LLVM 20.1's makes one store of several equal rows of an array
/// of `i128` with lanes of it left as poison, so that part of the array is never written
/// (`breakers/cases/src/repeat_rows.rs`). The `default<O2>` pipeline turns the pass on
/// whatever its options say, which leaves the flag the pass itself reads.
///
/// On AArch64 the quick selection is not the global instruction selector. That one deletes
/// the unused copy of the selector register at a landing pad and leaves the register a
/// live-in of the block, and LLVM 20.1's fast register allocator does not reload a value it
/// has put in a live-in register: after optimization, which keeps values in registers across
/// an unwind edge, a landing pad then reads the selector where it wants a drop flag
/// (`breakers/cases/src/unwind_many_flags.rs`).
fn set_options(arch: Arch) {
    static SET: Once = Once::new();
    SET.call_once(|| {
        let mut arguments = vec![c"rustd".as_ptr(), c"-vectorize-slp=false".as_ptr()];
        if arch == Arch::Aarch64 {
            arguments.push(c"-aarch64-enable-global-isel-at-O=-1".as_ptr());
        }
        // SAFETY: the arguments are C strings that outlive the call, and LLVM is given
        // their number.
        unsafe {
            LLVMParseCommandLineOptions(
                arguments.len() as i32,
                arguments.as_ptr(),
                std::ptr::null(),
            )
        }
    });
}

impl Optimization {
    /// The passes to run. What `lower` writes keeps every value in a stack slot, and a slot
    /// indexed by a number that is itself in a slot is not taken apart until that number is
    /// known; left to the default pipeline, such a slot is deleted whole and the variable it
    /// holds is lost to the debugger. So the slots are taken apart twice first, with the
    /// constants folded in between.
    fn pipeline(self) -> Option<&'static str> {
        match self.passes {
            Passes::None => None,
            Passes::Less => Some("function(sroa,instsimplify,sroa),default<O1>"),
            Passes::Default => Some("function(sroa,instsimplify,sroa),default<O2>"),
            Passes::Aggressive => Some("function(sroa,instsimplify,sroa),default<O3>"),
        }
    }

    /// The level of the code generator, which is not that of the passes.
    fn level(self) -> OptimizationLevel {
        match (self.selection, self.passes) {
            (Selection::Fast, _) | (_, Passes::None) => OptimizationLevel::None,
            (Selection::Optimized, Passes::Less) => OptimizationLevel::Less,
            (Selection::Optimized, Passes::Default) => OptimizationLevel::Default,
            (Selection::Optimized, Passes::Aggressive) => OptimizationLevel::Aggressive,
        }
    }
}

/// The target as LLVM knows it, one per thread and level: making one costs more than a
/// small function does. The context the modules are made in is kept with them, as a
/// machine holds on to things of the last context it compiled a module of: the instruction
/// selector of AArch64 keeps the source location of the last instruction it saw, and lets
/// go of it at the next function, by when a context of that one module would be gone.
#[derive(Default)]
struct Machine {
    // Before the context, which has to outlive them.
    machines: FxHashMap<Optimization, TargetMachine>,
    context: Option<Context>,
    /// How many modules were made in the context.
    modules: usize,
}

impl Machine {
    /// How many modules a context is kept for. The constants and source locations of all
    /// of them stay in it.
    const MODULES: usize = 1024;

    fn with<T>(
        optimization: Optimization,
        work: impl FnOnce(&Context, &TargetMachine) -> Result<T, Unhandled>,
    ) -> Result<T, Unhandled> {
        thread_local! {
            static MACHINE: RefCell<Machine> = RefCell::default();
        }
        MACHINE.with_borrow_mut(|this| {
            if this.modules == Self::MODULES {
                *this = Machine::default();
            }
            this.modules += 1;
            let machine = match this.machines.entry(optimization) {
                Entry::Occupied(known) => known.into_mut(),
                Entry::Vacant(slot) => slot.insert(Self::create(optimization)?),
            };
            work(this.context.get_or_insert_with(Context::create), machine)
        })
    }

    fn create(optimization: Optimization) -> Result<TargetMachine, Unhandled> {
        let emitted = super::Target::selected().map_err(Unhandled::Llvm)?;
        set_options(emitted.arch);
        match emitted.arch {
            Arch::X86_64 => Target::initialize_x86(&InitializationConfig::default()),
            Arch::Aarch64 => Target::initialize_aarch64(&InitializationConfig::default()),
            // The lowering of `wasm.rs` produces a module's functions.
            Arch::Wasm32 => return Err(Unhandled::Llvm("no LLVM producer for wasm32".to_owned())),
        }
        let name = emitted.triple();
        let triple = TargetTriple::create(name);
        let target = Target::from_triple(&triple)
            .map_err(|e| Unhandled::Llvm(e.to_string_lossy().into()))?;
        // The baseline CPU, as rustc compiles for; position-independent, as the rest of
        // the program is.
        target
            .create_target_machine(
                &triple,
                emitted.cpu(),
                "",
                optimization.level(),
                RelocMode::PIC,
                CodeModel::Default,
            )
            .ok_or_else(|| Unhandled::Llvm(format!("no target machine for {name}")))
    }
}

/// What a function or data object the code refers to is called in the module, and so in
/// the relocations of the object: the request with that index. No symbol of a program has
/// such a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Symbol {
    Func(u32),
    Data(u32),
}

impl Symbol {
    /// Cranelift's `User` names: namespace 0 holds the functions, 1 the data objects.
    fn of_user(namespace: u32, index: u32) -> Symbol {
        match namespace {
            0 => Symbol::Func(index),
            _ => Symbol::Data(index),
        }
    }

    fn parse(name: &str) -> Option<Symbol> {
        if let Some(index) = name.strip_prefix("rustd.f.") {
            return index.parse().ok().map(Symbol::Func);
        }
        name.strip_prefix("rustd.d.")?.parse().ok().map(Symbol::Data)
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Symbol::Func(index) => write!(f, "rustd.f.{index}"),
            Symbol::Data(index) => write!(f, "rustd.d.{index}"),
        }
    }
}

/// Why LLVM did not compile a function.
#[derive(Debug)]
pub(super) enum Unhandled {
    Opcode(Opcode),
    Type(Type),
    /// IR or an object of a kind there is no answer for yet.
    Shape(&'static str),
    /// A relocation type of the object format, by its number.
    Relocation(u32),
    Llvm(String),
}

impl fmt::Display for Unhandled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unhandled::Opcode(opcode) => write!(f, "opcode `{opcode}`"),
            Unhandled::Type(ty) => write!(f, "type `{ty}`"),
            Unhandled::Shape(what) => f.write_str(what),
            Unhandled::Relocation(kind) => write!(f, "relocation of type {kind}"),
            // The first line names the problem; the rest names the function's values.
            Unhandled::Llvm(message) => {
                write!(f, "llvm: {}", message.lines().next().unwrap_or_default())
            }
        }
    }
}
