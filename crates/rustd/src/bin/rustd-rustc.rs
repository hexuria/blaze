//! What cargo is told to run rustc through for the crates of a workspace: cargo runs
//! `rustd-rustc rustc ...`, and it becomes `rustd rustc-wrapper rustc ...`, the `rustd`
//! being the one installed next to it. Cargo's configuration names a wrapper by its path
//! alone, so the wrapper has a path of its own.

use std::process::{Command, ExitCode};

pub fn main() -> ExitCode {
    let mut asked = std::env::args_os().skip(1);
    let rustd = std::env::current_exe()
        .map(|this| this.with_file_name(format!("rustd{}", std::env::consts::EXE_SUFFIX)));
    let why = match rustd {
        Ok(rustd) => {
            let mut command = Command::new(&rustd);
            command.arg("rustc-wrapper").args(std::env::args_os().skip(1));
            match ended(&mut command) {
                Ok(code) => return code,
                Err(error) => format!("{} does not run ({error})", rustd.display()),
            }
        }
        Err(error) => format!("rustd-rustc does not know where it is installed ({error})"),
    };
    // The build is the user's, with rustd or without.
    eprintln!("rustd: {why}; rustc compiles");
    let Some(rustc) = asked.next() else { return ExitCode::FAILURE };
    match Command::new(rustc).args(asked).status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => {
            eprintln!("error: could not run rustc: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Becomes the command, where a process can.
#[cfg(unix)]
fn ended(command: &mut Command) -> std::io::Result<ExitCode> {
    Err(std::os::unix::process::CommandExt::exec(command))
}

#[cfg(not(unix))]
fn ended(command: &mut Command) -> std::io::Result<ExitCode> {
    Ok(ExitCode::from(command.status()?.code().unwrap_or(1) as u8))
}
