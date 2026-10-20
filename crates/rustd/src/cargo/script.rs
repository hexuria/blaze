//! Build scripts of the workspace. Cargo runs one again when what it said it reads has
//! changed, or when anything in its package has if it said nothing, and then compiles the
//! package again whatever the script printed. The daemon runs the script the same way and
//! looks at what it printed and wrote: when that is what it was, the crates compiled with
//! it are still right, and the build stays the daemon's.
//!
//! To run a script as cargo does, the wrapper puts a few lines of shell where rustc linked
//! it. Cargo runs those, and they run `rustd build-script`, which writes down the
//! environment and the directory cargo ran it in and becomes the script.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// The variable that names the script itself to `rustd build-script`.
const REAL: &str = "RUSTD_BUILD_SCRIPT";

/// The file next to a script's `OUT_DIR` that says how cargo ran it.
const RAN: &str = "rustd-ran.json";

/// How cargo ran a build script.
#[derive(Serialize, Deserialize)]
struct Ran {
    real: PathBuf,
    cwd: PathBuf,
    env: Vec<(String, String)>,
}

/// Has cargo run the script rustc linked at `linked` through `rustd build-script`.
#[cfg(unix)]
pub(crate) fn stand_in_front(linked: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let real = real_of(linked);
    std::fs::rename(linked, &real)?;
    let rustd = std::env::current_exe()?;
    let quoted = |path: &Path| -> anyhow::Result<String> {
        let path = path.to_str().context("a path that is not text")?;
        Ok(shlex::try_quote(path)?.into_owned())
    };
    let lines = format!(
        "#!/bin/sh\n{REAL}={} exec {} build-script \"$@\"\n",
        quoted(&real)?,
        quoted(&rustd)?
    );
    std::fs::write(linked, lines)?;
    Ok(std::fs::set_permissions(linked, std::fs::Permissions::from_mode(0o755))?)
}

/// Has cargo run the script rustc linked at `linked` through `rustd build-script`. There is
/// no shell to say so in a file cargo runs, so this executable is linked there under the
/// script's name, and [`stood_in_front_of`] is how it knows. Where it can't be linked, on
/// another volume, cargo runs the script itself and a change it reruns for is cargo's to
/// build.
#[cfg(windows)]
pub(crate) fn stand_in_front(linked: &Path) -> anyhow::Result<()> {
    let real = real_of(linked);
    std::fs::rename(linked, &real)?;
    if std::fs::hard_link(std::env::current_exe()?, linked).is_err() {
        std::fs::rename(&real, linked)?;
    }
    Ok(())
}

/// The script this executable stands in front of, when it runs under the name of one:
/// the name rustc linked it under, or the one cargo gives it in the same directory.
#[cfg(windows)]
pub(crate) fn stood_in_front_of() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let name = exe.file_stem()?.to_str()?;
    if !name.starts_with("build_script_") && !name.starts_with("build-script-") {
        return None;
    }
    let beside = std::fs::read_dir(exe.parent()?).ok()?.flatten().map(|it| it.path());
    beside.into_iter().find(|it| it.to_str().is_some_and(|it| it.ends_with(".exe.real")))
}

fn real_of(linked: &Path) -> PathBuf {
    let mut real = linked.as_os_str().to_owned();
    real.push(".real");
    PathBuf::from(real)
}

/// `rustd build-script`: writes down how cargo runs the script and becomes it.
pub(crate) fn command(args: impl Iterator<Item = OsString>) -> anyhow::Result<()> {
    let real = PathBuf::from(std::env::var_os(REAL).context("no build script is named")?);
    run_as_cargo_does(real, args)
}

/// Writes down how cargo runs the script at `real` and becomes it.
pub(crate) fn run_as_cargo_does(
    real: PathBuf,
    args: impl Iterator<Item = OsString>,
) -> anyhow::Result<()> {
    let env: Vec<(String, String)> = std::env::vars_os()
        .filter(|(name, _)| name != REAL)
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect();
    let out_dir =
        env.iter().find(|(name, _)| name == "OUT_DIR").map(|(_, value)| PathBuf::from(value));
    if let Some(run_dir) = out_dir.as_deref().and_then(Path::parent) {
        let ran = Ran { real: real.clone(), cwd: std::env::current_dir()?, env };
        // Without it the script is not run again by the daemon, and cargo builds.
        let _ = std::fs::write(run_dir.join(RAN), serde_json::to_vec(&ran)?);
    }
    let mut script = Command::new(&real);
    script.args(args).env_remove(REAL);
    become_it(&mut script)
}

#[cfg(unix)]
fn become_it(script: &mut Command) -> anyhow::Result<()> {
    Err(std::os::unix::process::CommandExt::exec(script).into())
}

#[cfg(not(unix))]
fn become_it(script: &mut Command) -> anyhow::Result<()> {
    std::process::exit(script.status()?.code().unwrap_or(1))
}

/// Whether a script can be run again as cargo ran it into `out_dir`.
pub(crate) fn can_rerun(out_dir: &Path) -> bool {
    out_dir.parent().is_some_and(|it| it.join(RAN).exists())
}

/// Runs the script that wrote `out_dir` again, as cargo ran it but into a directory of its
/// own: what cargo's holds is read by the compilers of a build under way, and cargo
/// compiles the package again when a file there is newer than it was. `Ok` when the script
/// printed and wrote what it did then; `Err` is what is different, which a script that
/// writes the name of its directory into a file always is.
pub(crate) fn rerun_is_same(out_dir: &Path) -> Result<(), String> {
    let run_dir = out_dir.parent().ok_or("a build script without a directory")?;
    let read = |name: &str| std::fs::read(run_dir.join(name)).map_err(|it| format!("{name}: {it}"));
    let ran: Ran = serde_json::from_slice(&read(RAN)?).map_err(|it| it.to_string())?;
    let printed = read("output")?;
    let again_dir = run_dir.join(format!("rustd-out-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&again_dir);
    std::fs::create_dir(&again_dir).map_err(|it| it.to_string())?;
    let same = rerun_into(&ran, &again_dir, out_dir, &printed);
    let _ = std::fs::remove_dir_all(&again_dir);
    same
}

fn rerun_into(ran: &Ran, again_dir: &Path, out_dir: &Path, printed: &[u8]) -> Result<(), String> {
    let mut script = Command::new(&ran.real);
    script.env_clear().envs(ran.env.iter().map(|(name, value)| (name, value)));
    script.env("OUT_DIR", again_dir).current_dir(&ran.cwd).stdin(Stdio::null());
    let again = script.output().map_err(|it| it.to_string())?;
    if !again.status.success() {
        return Err(format!("failed ({})", again.status));
    }
    let (Some(named), Some(names)) = (out_dir.to_str(), again_dir.to_str()) else {
        return Err("has a directory whose name is not text".to_owned());
    };
    if String::from_utf8_lossy(&again.stdout).replace(names, named).as_bytes() != printed {
        return Err("printed something else".to_owned());
    }
    if digest_of(again_dir) != digest_of(out_dir) {
        return Err("wrote something else".to_owned());
    }
    Ok(())
}

/// A hash of the names, as they are below `dir`, and the contents of the files there.
fn digest_of(dir: &Path) -> Vec<u8> {
    let mut hash = Sha256::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(below) = pending.pop() {
        let entries = std::fs::read_dir(&below).into_iter().flatten().flatten();
        let mut paths: Vec<PathBuf> = entries.map(|it| it.path()).collect();
        paths.sort();
        for path in paths {
            let name = path.strip_prefix(dir).unwrap_or(&path);
            hash.update(name.as_os_str().as_encoded_bytes());
            match path.is_dir() {
                true => pending.push(path),
                false => hash.update(std::fs::read(&path).unwrap_or_default()),
            }
        }
    }
    hash.finalize().to_vec()
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    /// A run directory as cargo and `rustd build-script` leave one, for a script that
    /// writes `wrote` and prints where.
    fn ran(name: &str, wrote: &str) -> PathBuf {
        let run_dir =
            std::env::temp_dir().join(format!("rustd-script-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&run_dir);
        let out_dir = run_dir.join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let real = run_dir.join("script");
        let lines =
            "#!/bin/sh\nprintf %s \"$WROTE\" > \"$OUT_DIR/made.rs\"\necho \"cargo:in=$OUT_DIR\"\n";
        std::fs::write(&real, lines).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env = vec![
            ("OUT_DIR".to_owned(), out_dir.to_str().unwrap().to_owned()),
            ("WROTE".to_owned(), wrote.to_owned()),
            ("PATH".to_owned(), std::env::var("PATH").unwrap()),
        ];
        let ran = Ran { real, cwd: run_dir.clone(), env };
        std::fs::write(run_dir.join(RAN), serde_json::to_vec(&ran).unwrap()).unwrap();
        std::fs::write(run_dir.join("output"), format!("cargo:in={}\n", out_dir.display()))
            .unwrap();
        out_dir
    }

    #[test]
    fn a_rerun_leaves_what_cargo_has_alone() {
        let out_dir = ran("same", "fn made() {}");
        let made = out_dir.join("made.rs");
        std::fs::write(&made, "fn made() {}").unwrap();
        let written = std::fs::metadata(&made).unwrap().modified().unwrap();
        assert_eq!(rerun_is_same(&out_dir), Ok(()));
        assert_eq!(std::fs::metadata(&made).unwrap().modified().unwrap(), written);
        let left: Vec<_> =
            std::fs::read_dir(out_dir.parent().unwrap()).unwrap().flatten().collect();
        assert_eq!(left.len(), 4, "{left:?}");
    }

    #[test]
    fn a_rerun_that_writes_something_else_is_told() {
        let out_dir = ran("other", "fn made() {}");
        std::fs::write(out_dir.join("made.rs"), "fn made_before() {}").unwrap();
        assert_eq!(rerun_is_same(&out_dir), Err("wrote something else".to_owned()));
        assert_eq!(std::fs::read(out_dir.join("made.rs")).unwrap(), b"fn made_before() {}");
    }
}
