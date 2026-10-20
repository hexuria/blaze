//! rustd as the wrapper cargo runs rustc through for the crates of a workspace, under
//! `cargo rustd` and, once `rustd enable` named it in cargo's configuration, under plain
//! cargo. What rustc built of a crate is recorded for the daemon; a crate that is linked is
//! also asked for its link line and for the objects it was linked from. Under plain cargo
//! a program the daemon has loaded is the daemon's to build when cargo asks for it again,
//! and one it has not is rustc's, after which the daemon loads the build.

use std::{
    ffi::OsString,
    fs::OpenOptions,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread::JoinHandle,
};

use anyhow::Context as _;

use super::{
    ancestor::Cargo,
    cli, daemon, say, script,
    state::{State, place},
    switch::Enabled,
    units::{Copied, Record, stamp_now},
};
use crate::{
    socket::{private_dir, private_file},
    stub::VALUED,
};

/// The variable `cargo rustd` sets for the cargo it runs: the wrapper records what rustc
/// builds and leaves the rest to the command that drives cargo.
pub(crate) const DRIVEN: &str = "RUSTD_CARGO_DRIVES";

/// The variable `cargo rustd` sets in the first build of a workspace: the roots of the
/// crates that are the daemon's to compile, as a list of paths.
pub(crate) const STAGED: &str = "RUSTD_STAGED_CRATES";
/// The file in the record of a unit that says rustc built what stands in for the crate.
const STANDS_IN: &str = "stands-in";

/// What the variables that name the programs of a build start with.
const PROGRAM_OF: &str = "CARGO_BIN_EXE_";
/// The variable that names the directory cargo gives the integration tests of a build.
const SCRATCH: &str = "CARGO_TARGET_TMPDIR";

/// The file in the daemon's directory that has a line for every program plain cargo asked
/// for: who linked it, and why when it was rustc.
const TALLY: &str = "plain-cargo.log";
/// Why rustc links a program the daemon was not asked for: the first build of it.
const NOT_LOADED: &str = "the daemon had not loaded the build yet";
/// The directory there with a file for every cargo whose build is being loaded.
const SETTLING: &str = "settling";
/// The codegen option cargo names rustc's incremental directory with, as its own argument
/// after `-C`.
const INCREMENTAL: &str = "incremental";
/// The directory, in cargo's incremental one, of the checks that go with the daemon's builds.
const CHECKS: &str = "rustd-check";

/// Builds what cargo asked rustc for and ends as rustc would have.
pub(crate) fn command(mut args: impl Iterator<Item = OsString>) -> anyhow::Result<()> {
    let rustc = args.next().context("cargo names rustc first")?;
    let args: Vec<OsString> = args.collect();
    // The wrapper the user had for the workspace's crates still runs rustc.
    let mut compiler = match Enabled::read().and_then(|it| it.chained) {
        Some(chained) => {
            let mut command = Command::new(chained);
            command.arg(&rustc);
            command
        }
        None => Command::new(&rustc),
    };
    compiler.args(&args);
    let Some(unit) = Unit::of(&args) else {
        std::process::exit(compiler.status()?.code().unwrap_or(1));
    };
    let plain = unit.linked && std::env::var_os(DRIVEN).is_none();
    let dir = unit.records.join(&unit.id);
    if unit.is_staged() {
        match unit.stood_in(&rustc, &dir) {
            Ok(code) => std::process::exit(code),
            // rustc compiles the crate as cargo asked, and the daemon links against that.
            Err(why) => eprintln!("warning: rustc compiles `{}`: {why:#}", unit.id),
        }
    }
    let known = if plain { State::of_unit(&dir) } else { Vec::new() };
    if !known.is_empty() {
        match unit.by_daemon(&dir, &known, &rustc) {
            Ok(()) => {
                tally("daemon", &unit.crate_name);
                std::process::exit(0);
            }
            Err(why) => unit.left_to_rustc(&why),
        }
    }
    let stamp = stamp_now();
    let copies = dir.join(stamp.to_string());
    let line = copies.join("link.line");
    if unit.linked {
        std::fs::create_dir_all(&copies)?;
        compiler.arg(format!("--print=link-args={}", line.display())).arg("-Csave-temps");
    }
    let status =
        compiler.status().with_context(|| format!("running {}", Path::new(&rustc).display()))?;
    if status.success()
        && let Err(error) = unit.record(stamp, &copies, unit.linked.then_some(line), &unit.source)
    {
        // The build is good; the daemon has no part in this crate until it is built again.
        eprintln!("warning: rustd did not record `{}`: {error:#}", unit.id);
    }
    if plain && status.success() {
        // The program is rustc's now: the builds that had it are loaded again from this one.
        if known.is_empty() {
            tally("rustc", NOT_LOADED);
        }
        State::forget(&known);
        let _ = std::fs::remove_file(dir.join(State::STATES));
        if let Err(why) = loaded_later() {
            unit.left_to_rustc(&why);
        }
    }
    std::process::exit(status.code().unwrap_or(1));
}

/// Starts a process that has the daemon load the build of the cargo above this one, when
/// none was started for that cargo yet. `Err` is why the build is not one the daemon loads.
fn loaded_later() -> Result<(), String> {
    let cargo = Cargo::above()?;
    let typed = cargo.args.join(" ");
    let asked =
        cli::of_cargo(&cargo.args).ok_or(format!("`cargo {typed}` is not a build rustd loads"))?;
    if let Some(why) = asked.only_cargo() {
        return Err(why);
    }
    let said = |it: &dyn std::fmt::Display| format!("the build is not loaded: {it:#}");
    let home = daemon::home().map_err(|it| said(&it))?;
    let settling = home.join(SETTLING);
    private_dir(&settling).map_err(|it| said(&it))?;
    let ours = settling.join(cargo.pid.to_string());
    match private_file(OpenOptions::new().write(true).create_new(true), &ours) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => return Err(said(&error)),
    }
    let log = private_file(OpenOptions::new().create(true).append(true), &home.join("settle.log"))
        .map_err(|it| said(&it))?;
    let mut settle = Command::new(std::env::current_exe().map_err(|it| said(&it))?);
    settle.args(["cargo", super::SETTLE]).arg(cargo.pid.to_string()).args(&cargo.args);
    settle.env_clear().envs(cargo.env).env("CARGO", &cargo.executable).current_dir(&cargo.cwd);
    settle.stdin(Stdio::null()).stdout(log.try_clone().map_err(|it| said(&it))?).stderr(log);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut settle, 0);
    settle.spawn().map(drop).map_err(|it| {
        settled(cargo.pid);
        said(&it)
    })
}

/// The build of the cargo `cargo` is loaded, or is not going to be.
pub(crate) fn settled(cargo: u32) {
    if let Ok(home) = daemon::home() {
        let _ = std::fs::remove_file(home.join(SETTLING).join(cargo.to_string()));
    }
}

/// Writes down that `who` linked a program for plain cargo, and `what` of it: its name
/// when it was the daemon, why it was not when it was rustc.
fn tally(who: &str, what: &str) {
    let log = daemon::home().and_then(|home| {
        private_dir(&home)?;
        Ok(private_file(OpenOptions::new().create(true).append(true), &home.join(TALLY))?)
    });
    if let Ok(mut log) = log {
        let _ = writeln!(log, "{who}\t{}", what.replace('\n', " "));
    }
}

/// The lines of `rustd status` that count the programs plain cargo asked for.
pub(crate) fn tallied() -> Vec<String> {
    let log = daemon::home().ok().and_then(|it| std::fs::read_to_string(it.join(TALLY)).ok());
    let Some(log) = log else { return Vec::new() };
    let mut reasons = std::collections::BTreeMap::<&str, u64>::new();
    let (mut daemon, mut rustc) = (0u64, 0u64);
    for line in log.lines() {
        match line.split_once('\t') {
            Some(("daemon", _)) => daemon += 1,
            Some((_, why)) => {
                rustc += 1;
                *reasons.entry(why).or_default() += 1;
            }
            None => {}
        }
    }
    let mut lines = vec![format!("  {daemon} programs linked by the daemon, {rustc} by rustc")];
    lines.extend(reasons.iter().map(|(why, count)| format!("    {count} x {why}")));
    lines
}

/// A run of rustc that compiles a crate, as cargo's arguments describe it.
struct Unit {
    id: String,
    crate_name: String,
    args: Vec<String>,
    source: PathBuf,
    cwd: PathBuf,
    records: PathBuf,
    /// Whether rustc links a program of the workspace from it.
    linked: bool,
    /// Whether rustc links a build script from it.
    script: bool,
}

impl Unit {
    /// `None` for a run that compiles nothing to record: a question to rustc (`--print`,
    /// `-vV`), source on standard input, or arguments that are not text.
    fn of(args: &[OsString]) -> Option<Unit> {
        let args: Vec<String> =
            args.iter().map(|it| it.to_str().map(str::to_owned)).collect::<Option<_>>()?;
        let value = |flag: &str| {
            let at = args.iter().position(|it| it == flag)?;
            args.get(at + 1).cloned()
        };
        let crate_name = value("--crate-name")?;
        let out_dir = value("--out-dir")?;
        if args.iter().any(|it| it == "-" || it.starts_with("--print")) {
            return None;
        }
        // The one argument that is a path to a Rust file and not the value of a flag.
        let source = (0..args.len()).find(|&at| {
            args[at].ends_with(".rs") && !(at > 0 && VALUED.contains(&args[at - 1].as_str()))
        })?;
        let cwd = std::env::current_dir().ok()?;
        let mut unit = Unit {
            id: String::new(),
            crate_name,
            source: cwd.join(&args[source]),
            records: Record::dir_for(&cwd.join(out_dir)),
            args,
            cwd,
            linked: false,
            script: false,
        };
        let record = unit.as_record(0, Vec::new(), None);
        let emits_link = unit.args.iter().any(|it| {
            it.strip_prefix("--emit=").is_some_and(|kinds| kinds.split(',').any(|it| it == "link"))
        });
        // A unit that is only checked leaves nothing the daemon reads.
        if !emits_link {
            return None;
        }
        unit.script = record.is_build_script();
        unit.linked = record.is_linked() && !record.runs_in_build();
        unit.id =
            format!("{}{}", unit.crate_name, record.codegen("extra-filename").unwrap_or_default());
        Some(unit)
    }

    /// Has the daemon build the program and puts it where rustc would have, with the
    /// dep-info rustc would have written. `known` are the builds that have the program, and
    /// `dir` is where the unit is recorded. `Err` is why the program is rustc's to link.
    fn by_daemon(&self, dir: &Path, known: &[String], rustc: &OsString) -> Result<(), String> {
        let mut read = known.iter().map(|key| State::read(key).map_err(|why| (key, why)));
        let mut state = match read.find_map(Result::ok) {
            Some(state) => state,
            None => {
                let why = known.iter().find_map(|it| State::read(it).err().flatten());
                return Err(why.unwrap_or("the build is not one the daemon has loaded").to_owned());
            }
        };
        if let Some(why) = state.refusal.clone().or_else(|| state.broken_guard()) {
            return Err(why);
        }
        let (checkout, at) = state.asked_for(dir)?;
        let deps = self.dep_info(rustc);
        let answer = super::built(checkout).and_then(|programs| {
            programs.get(at).cloned().unwrap_or(Err("the daemon said nothing of it".to_owned()))
        });
        let read = deps.join().map_err(|_| "rustc did not run".to_owned());
        let read = read?.map_err(|it| format!("rustc did not run: {it}"))?;
        // What the daemon was asked for and did not build fails the build: rustc is not
        // given it instead.
        if let Err(why) = answer {
            say::error(&why);
            std::process::exit(super::FAILED);
        }
        if !read.status.success() {
            let _ = std::io::stderr().write_all(&read.stderr);
            std::process::exit(super::FAILED);
        }
        let binary = &checkout.programs[at].binary;
        let record = self.as_record(0, Vec::new(), None);
        let linked = record.outputs().into_iter().next().ok_or("rustc names no executable")?;
        place(binary, &linked).map_err(|it| format!("installing {}: {it}", linked.display()))?;
        state.cargo_has(dir, binary).map_err(|it| format!("keeping {}: {it}", linked.display()))?;
        // Every other build with the program kept the one cargo had before.
        State::forget(known.iter().filter(|it| **it != state.key));
        let _ = std::io::stderr().write_all(&read.stderr);
        Ok(())
    }

    /// Has rustc write the dep-info cargo reads to know what the program was compiled
    /// from, while the daemon builds it. Rustc stops once it has read the source: it
    /// checks nothing, and the daemon builds what rust-analyzer passes.
    fn dep_info(&self, rustc: &OsString) -> JoinHandle<std::io::Result<Output>> {
        let args = self.args.iter().map(|it| match it.split_once('=') {
            Some(("--emit", _)) => "--emit=dep-info".to_owned(),
            Some((INCREMENTAL, dir)) => format!("{INCREMENTAL}={dir}/{CHECKS}"),
            _ => it.clone(),
        });
        let mut command = Command::new(rustc);
        command.args(args).stdin(Stdio::null());
        std::thread::spawn(move || command.output())
    }

    /// Whether the crate is one the first build of the workspace leaves to the daemon.
    fn is_staged(&self) -> bool {
        let Some(staged) = std::env::var_os(STAGED) else { return false };
        let lists = std::env::split_paths(&staged).any(|it| it == self.source);
        lists && !self.script && !self.args.iter().any(|it| it == "--test")
    }

    /// Has rustc do of the crate what it does in a build by the daemon: check it, and
    /// build what stands in for it, which is what cargo links and has no code. `dir` is
    /// where the unit is recorded. `Ok` is the status rustc's check ended with; `Err` is
    /// why nothing stands in for the crate.
    ///
    /// What stands in is built first and from the libraries that stand in for the crates
    /// it depends on, and the check reads the metadata of the checks of those. Cargo
    /// starts the units that depend on this one when the check has written the metadata,
    /// which is when both are there.
    fn stood_in(&self, rustc: &OsString, dir: &Path) -> anyhow::Result<i32> {
        let program = crate::stub::is_program(&self.args).context("not a library or a program")?;
        let stamp = stamp_now();
        let copies = dir.join(stamp.to_string());
        std::fs::create_dir_all(&copies)?;
        let line = copies.join("link.line");
        let extra = self.as_record(0, Vec::new(), None);
        let stem =
            format!("{}{}", self.crate_name, extra.codegen("extra-filename").unwrap_or_default());
        let source = copies.join(format!("{stem}.rustd.rs"));
        std::fs::write(&source, crate::stub::text_of(&self.args, program))?;

        let mut stand_in = Command::new(rustc);
        let mut args = self.args.iter().peekable();
        while let Some(arg) = args.next() {
            let incremental = |it: &&String| it.starts_with(&format!("{INCREMENTAL}="));
            if arg == "-C" && args.peek().is_some_and(incremental) {
                args.next();
            } else if self.cwd.join(arg) == self.source {
                stand_in.arg(&source);
            } else if arg.starts_with("--emit=") {
                stand_in.arg("--emit=link");
            } else {
                stand_in.arg(self.standing_in(arg).unwrap_or_else(|| arg.clone()));
            }
        }
        if program {
            stand_in.arg(format!("--print=link-args={}", line.display())).arg("-Csave-temps");
        }
        let built = stand_in.stdin(Stdio::null()).output().context("running rustc")?;
        anyhow::ensure!(
            built.status.success(),
            "what stands in for it does not build: {}",
            String::from_utf8_lossy(&built.stderr).trim_end()
        );
        std::fs::write(dir.join(STANDS_IN), "")?;

        // The crate is not checked by rustc: the daemon's front end is what checks it, and a
        // second compile beside the daemon's would only compete with it.
        self.record(stamp, &copies, program.then_some(line), &source)?;
        Ok(0)
    }

    /// `--extern`'s value `named` with the library that stands in for the crate, when the
    /// value is the metadata of a crate that has one.
    fn standing_in(&self, named: &str) -> Option<String> {
        let (name, path) = named.split_once('=')?;
        let stem = Path::new(path).file_name()?.to_str()?.strip_suffix(".rmeta")?;
        let id = stem.strip_prefix("lib")?;
        self.records
            .join(id)
            .join(STANDS_IN)
            .exists()
            .then(|| format!("{name}={}", Path::new(path).with_extension("rlib").display()))
    }

    /// `--extern`'s value `named` with the metadata rustc's check wrote of the crate, when
    /// the value is the library that stands in for one.
    fn checked_against(&self, named: &str) -> Option<String> {
        let (name, path) = named.split_once('=')?;
        let stem = Path::new(path).file_name()?.to_str()?.strip_suffix(".rlib")?;
        let id = stem.strip_prefix("lib")?;
        self.records
            .join(id)
            .join(STANDS_IN)
            .exists()
            .then(|| format!("{name}={}", Path::new(path).with_extension("rmeta").display()))
    }

    /// Says in one line why rustc links the program and not the daemon, and counts it.
    fn left_to_rustc(&self, why: &str) {
        say::fallback(why, &format!("rustc links `{}`", self.crate_name));
        tally("rustc", why);
    }

    fn as_record(&self, stamp: u128, copies: Vec<Copied>, link_line: Option<PathBuf>) -> Record {
        Record {
            id: self.id.clone(),
            crate_name: self.crate_name.clone(),
            cwd: self.cwd.clone(),
            args: self.args.clone(),
            source: self.source.clone(),
            stamp,
            copies,
            link_line,
            env: std::env::vars()
                .filter(|(name, _)| name.starts_with(PROGRAM_OF) || name == SCRATCH)
                .collect(),
        }
    }

    /// Copies what a later build of the unit writes over and writes the record.
    /// `compiled` is the root of the source rustc was given.
    fn record(
        &self,
        stamp: u128,
        to: &Path,
        link_line: Option<PathBuf>,
        compiled: &Path,
    ) -> anyhow::Result<()> {
        let record = self.as_record(stamp, Vec::new(), link_line.clone());
        // The executable is the daemon's to link; of the outputs it reads the libraries.
        let libraries =
            record.outputs().into_iter().filter(|it| it.extension().is_some_and(|it| it == "rlib"));
        let mut originals: Vec<PathBuf> = libraries.filter(|it| it.exists()).collect();
        if let Some(line) = &link_line {
            let line = std::fs::read_to_string(line)
                .with_context(|| format!("rustc wrote no link line to {}", line.display()))?;
            let args = shlex::split(&line).context("the link line is not quoted as a shell's")?;
            let objects = args.into_iter().filter(|it| it.ends_with(".rcgu.o")).map(PathBuf::from);
            originals.extend(objects.filter(|it| it.exists()));
        }
        std::fs::create_dir_all(to)?;
        let mut copies = Vec::new();
        for original in originals {
            let name = original.file_name().context("an output without a name")?;
            let copy = to.join(name);
            std::fs::copy(&original, &copy)
                .with_context(|| format!("copying {}", original.display()))?;
            copies.push(Copied { original, copy });
        }
        // The daemon tells which crate a file of rustc's is by the dep-info next to it, of
        // which it reads the first source: the root of the crate.
        let stem =
            format!("{}{}", self.crate_name, record.codegen("extra-filename").unwrap_or_default());
        std::fs::write(to.join(format!("{stem}.d")), format!("{stem}: {}\n", compiled.display()))?;
        if self.script
            && let Some(linked) = record.outputs().first()
        {
            script::stand_in_front(linked)?;
        }
        self.as_record(stamp, copies, link_line).write(&self.records)
    }
}
