//! Running what was built, as cargo does: the program of `cargo run`, the test executables
//! of `cargo test`.

use std::{
    path::Path,
    process::{Command, ExitStatus},
};

use anyhow::Context as _;

use super::{
    cli::Invocation,
    say,
    state::{Kind, Program, State, as_built, cargo_path},
};

/// The exit status of cargo when a test fails or a program can't be run.
const FAILED: i32 = 101;

/// Runs the program `asked` names, and ends as it ended. `None` when cargo has to say
/// which program that is: there are several, or none.
pub(crate) fn program(
    asked: &Invocation,
    state: &State,
    cwd: &Path,
) -> anyhow::Result<Option<i32>> {
    let runnable = |it: &&Program| !it.test && matches!(it.kind, Kind::Bin | Kind::Example);
    let mut programs: Vec<&Program> = state.programs.iter().filter(runnable).collect();
    if let Some(name) = asked.flags.bin.as_ref().or(asked.flags.example.as_ref()) {
        programs.retain(|it| it.name == *name);
    }
    let &[program] = programs.as_slice() else { return Ok(None) };
    let shown = shown_path(&program.executable, cwd);
    let args = asked.program_args().map(|it| format!(" {it}")).collect::<String>();
    say::status(asked.flags.quiet, "Running", &format!("`{shown}{args}`"));
    let mut command = Command::new(&program.executable);
    command.args(asked.program_args()).envs(program.env.iter().cloned());
    as_built(&mut command)?;
    let status = command.status().with_context(|| format!("could not execute {shown}"))?;
    Ok(Some(exit_code(status)))
}

/// Runs the test executables one after another, each in the directory of its package, and
/// stops at the first that fails unless told not to. The result is cargo's exit status.
pub(crate) fn tests(asked: &Invocation, state: &State, cwd: &Path) -> anyhow::Result<i32> {
    let mut failed = Vec::new();
    for test in state.programs.iter().filter(|it| it.test) {
        let source = test.source.strip_prefix(&test.package_dir).unwrap_or(&test.source);
        let unit = if matches!(test.kind, Kind::Test | Kind::Bench) { "" } else { "unittests " };
        let shown = shown_path(&test.executable, cwd);
        say::status(asked.flags.quiet, "Running", &format!("{unit}{} ({shown})", source.display()));
        let mut command = Command::new(&test.executable);
        command.args(asked.program_args()).envs(test.env.iter().cloned());
        command.current_dir(&test.package_dir);
        as_built(&mut command)?;
        let status = command.status().with_context(|| format!("could not execute {shown}"))?;
        if !status.success() {
            failed.push(rerun_flags(test));
            if !asked.flags.no_fail_fast {
                break;
            }
        }
    }
    match failed.as_slice() {
        [] => Ok(0),
        [one] => {
            say::error(&format!("test failed, to rerun pass `{one}`"));
            Ok(FAILED)
        }
        several => {
            say::error(&format!("{} targets failed:", several.len()));
            several.iter().for_each(|it| eprintln!("    `{it}`"));
            Ok(FAILED)
        }
    }
}

/// Runs the doc tests with cargo, which hands them to rustd in rustdoc's place: rustd
/// builds each example as its own program over its code of the library.
pub(crate) fn doc_tests(asked: &Invocation) -> anyhow::Result<i32> {
    let mut args = vec!["--doc".to_owned()];
    args.extend(asked.build_args.iter().cloned());
    if asked.flags.no_fail_fast {
        args.push("--no-fail-fast".to_owned());
    }
    args.extend(asked.positional.iter().cloned());
    args.push("--".to_owned());
    args.extend(asked.trailing.iter().cloned());
    only_doc_tests(&args)
}

/// Runs `cargo test` with `args`, which ask for the doc tests alone, with rustd in
/// rustdoc's place.
pub(crate) fn only_doc_tests(args: &[String]) -> anyhow::Result<i32> {
    let rustdoc = std::env::var_os("RUSTDOC").unwrap_or_else(|| "rustdoc".into());
    let mut cargo = crate::doctest::as_asked(cargo_path());
    cargo
        .env("RUSTDOC", std::env::current_exe().context("finding rustd itself")?)
        .env(crate::doctest::ENABLED, rustdoc);
    cargo.arg("test").args(args);
    as_built(&mut cargo)?;
    Ok(exit_code(cargo.status().context("running `cargo test --doc`")?))
}

/// The flags that have cargo run this test target alone.
fn rerun_flags(test: &Program) -> String {
    let target = match test.kind {
        Kind::Lib => "--lib".to_owned(),
        Kind::Bin => format!("--bin {}", test.name),
        Kind::Example => format!("--example {}", test.name),
        Kind::Test => format!("--test {}", test.name),
        Kind::Bench => format!("--bench {}", test.name),
    };
    format!("-p {} {target}", test.package)
}

/// The path as cargo shows it: from the directory the command runs in, when it is below.
fn shown_path(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}

/// What this command ends with after a process that ended with `status`: its code, or
/// the code a shell gives one a signal ended.
pub(crate) fn exit_code(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return 128 + signal;
    }
    status.code().unwrap_or(1)
}
