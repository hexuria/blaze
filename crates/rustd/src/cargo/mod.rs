//! `cargo rustd build|run|test|check`: cargo's commands, with the daemon building what
//! changed since cargo last built. `CARGO.md` at the root of the repository is the design.

pub(crate) mod daemon;
pub(crate) mod wrapper;

mod ancestor;
mod cli;
mod client;
mod package;
pub(crate) mod plan;
pub(crate) mod run;
mod say;
pub(crate) mod script;
mod state;
mod switch;
mod units;

use std::{collections::BTreeMap, path::Path, process::Command, time::Instant};

use anyhow::Context as _;

use cli::{Asked, Invocation, Subcommand};
use client::Start;
use daemon::{Checkout, Reply, Request};
use state::{Built, Counts, Staged, State};

/// `rustd cargo ...`: runs the command and ends with the status cargo would end with.
pub(crate) fn command(args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let args: Vec<String> = args.collect();
    if let [word, cargo, rest @ ..] = args.as_slice()
        && word == SETTLE
    {
        return settle(cargo.parse().context("`settle` is told the cargo to wait for")?, rest);
    }
    let asked = Asked::parse(args);
    if !matches!(asked, Asked::Build(_) | Asked::Cargo(_)) {
        crate::end_with_reader();
    }
    let code = match asked {
        Asked::Build(asked) => match asked.only_cargo() {
            None if asked.flags.doc => run::only_doc_tests(&asked.typed)?,
            Some(reason) => {
                say::fallback(&reason, &format!("this is `cargo {}`", asked.subcommand.name()));
                let typed = std::iter::once(asked.subcommand.name().to_owned()).chain(asked.typed);
                plain_cargo(typed.collect())?
            }
            None => build(&asked)?,
        },
        Asked::Cargo(args) => plain_cargo(args)?,
        Asked::Status => status()?,
        Asked::Stop => stop()?,
        Asked::Enable => switch::enable()?,
        Asked::Disable => switch::disable()?,
        Asked::Help => {
            println!("{HELP}");
            0
        }
    };
    std::process::exit(code)
}

const HELP: &str = "\
cargo rustd: builds with the rustd daemon, with cargo's flags

usage: cargo rustd <command> [cargo's flags]

  build, run, test   by the daemon when it has the workspace loaded, by cargo otherwise
  check              `cargo check`
  status             whether rustd is enabled, what the daemon has loaded, what fell back
  stop               ends the daemon; the next build starts one
  enable, disable    plain `cargo build|run|test|check` through rustd, and back

Any other command is cargo's own. CARGO.md is the guide.";

/// What a wrapper runs rustd with once rustc has linked a program under plain cargo.
pub(super) const SETTLE: &str = "settle";

/// Has the daemon load the build `typed` is the cargo command of, which the cargo `cargo`
/// is running: the build is asked of cargo again, which waits for that cargo to be done
/// with the target directory, has nothing left to compile and says what it built. This
/// process has that cargo's directory and environment. Nobody reads what it says but the
/// log.
fn settle(cargo: u32, typed: &[String]) -> anyhow::Result<()> {
    let settled = (|| {
        let asked = cli::of_cargo(typed).context("`settle` is told a build command")?;
        let cwd = std::env::current_dir()?;
        let key = asked.key(&cwd);
        eprintln!("settling {key}: cargo {} in {}", typed.join(" "), cwd.display());
        if let Ok(state) = by_cargo(&asked, &cwd, &key, Counts::default())? {
            state.write()?;
        }
        anyhow::Ok(())
    })();
    wrapper::settled(cargo);
    settled
}

/// Runs cargo with `args` and nothing of rustd's.
fn plain_cargo(args: Vec<String>) -> anyhow::Result<i32> {
    let status = Command::new(state::cargo_path()).args(args).status().context("running cargo")?;
    Ok(run::exit_code(status))
}

/// Builds what `asked` names and then runs what the subcommand runs. Cargo builds what
/// the daemon has not been given yet and what only cargo acts on (a manifest, a build
/// script); a program the daemon was asked for and did not build fails the build.
fn build(asked: &Invocation) -> anyhow::Result<i32> {
    let started = Instant::now();
    let cwd = std::env::current_dir()?;
    let key = asked.key(&cwd);
    let finished = |state: &State| {
        let took = started.elapsed().as_secs_f64();
        let finished = format!("`{}` profile [rustd] target(s) in {took:.2}s", state.profile);
        say::status(asked.flags.quiet, "Finished", &finished);
    };
    let state = match State::read(&key).map(|state| by_daemon(state)) {
        Ok(Ok(state)) => {
            finished(&state);
            state
        }
        Ok(Err(Left::Failed(why))) => {
            say::error(&why);
            return Ok(FAILED);
        }
        Ok(Err(Left::ToCargo(state, reason))) => {
            say::fallback(&reason, "building with cargo");
            let mut counts = state.counts;
            *counts.cargo.entry(reason).or_default() += 1;
            match by_cargo(asked, &cwd, &key, counts)? {
                Ok(state) => state,
                Err(code) => return Ok(code),
            }
        }
        Err(unread) => {
            if let Some(why) = unread {
                say::fallback(why, "building with cargo");
            }
            match staged(asked, &cwd, &key)? {
                Ok(state) => {
                    finished(&state);
                    state
                }
                Err(NotStaged::Failed(code)) => return Ok(code),
                Err(NotStaged::Daemons(why)) => {
                    say::error(&why);
                    return Ok(FAILED);
                }
                Err(NotStaged::Cargos(why)) => {
                    if let Some(why) = why {
                        say::fallback(&why, "building with cargo");
                    }
                    match by_cargo(asked, &cwd, &key, Counts::default())? {
                        Ok(state) => state,
                        Err(code) => return Ok(code),
                    }
                }
            }
        }
    };
    state.write()?;
    after_build(asked, &state, &cwd)
}

/// The status of a build that failed, as cargo's of one rustc turned down.
const FAILED: i32 = 101;

/// Why the first build of a workspace is not rustc's of what the workspace depends on and
/// the daemon's of the workspace.
enum NotStaged {
    /// Cargo failed, with this status: rustc turned the source down.
    Failed(i32),
    /// Cargo builds all of it, which it has not begun to, and why when that is worth a line.
    Cargos(Option<String>),
    /// Cargo built its part and the daemon did not build a program, and why.
    Daemons(String),
}

/// The first build of a workspace: cargo has rustc build what the workspace depends on
/// and check the crates of the workspace, and the daemon compiles those. Every program is
/// the daemon's: cargo linked only what stands in for it.
fn staged(asked: &Invocation, cwd: &Path, key: &str) -> anyhow::Result<Result<State, NotStaged>> {
    if std::env::var_os(STAGED).is_none_or(|it| it == "0") {
        return Ok(Err(NotStaged::Cargos(None)));
    }
    let crates = match Staged::of(asked)? {
        Ok(crates) => crates,
        Err(why) => return Ok(Err(NotStaged::Cargos(Some(why)))),
    };
    State::give_back(key);
    let ahead = crates.ahead.as_ref().map(|ahead| {
        // Without an answer the daemon loads the workspace once cargo is done.
        let _ = client::ask(Request::Ahead(ahead.clone()), Start::IfMissing);
        ahead.config.id()
    });
    let state = match Built::staged(asked, &crates)? {
        Ok(built) => daemons(asked, cwd, key, built)
            .unwrap_or_else(|why| Err(format!("{why:#}")))
            .map_err(NotStaged::Daemons),
        Err(code) => Err(NotStaged::Failed(code)),
    };
    if let (Err(_), Some(id)) = (&state, ahead) {
        let _ = client::ask(Request::Forget(id), Start::Never);
    }
    Ok(state)
}

/// The programs of a build whose workspace crates cargo built what stands in for, as the
/// daemon builds them. `Err` is why it did not build one.
fn daemons(
    asked: &Invocation,
    cwd: &Path,
    key: &str,
    built: Built,
) -> anyhow::Result<Result<State, String>> {
    let state = State::after(asked, cwd, key, built, Counts::default())?;
    Ok(first_by_daemon(state))
}

fn first_by_daemon(mut state: State) -> Result<State, String> {
    if let Some(why) = &state.refusal {
        return Err(why.clone());
    }
    if let Some((program, why)) = state.left_to_cargo().next() {
        return Err(format!("`{}`: {why}", program.name));
    }
    let answers = state.checkouts.iter().map(built).collect::<Result<Vec<_>, _>>()?;
    let cargos = state.settle(&answers)?;
    if let Some((_, why)) = cargos.first() {
        return Err(why.clone());
    }
    // What cargo linked stands in for the program, and is not what it is while its source
    // is unchanged: the daemon's is.
    state.keep_daemons().map_err(|why| format!("{why:#}"))?;
    state.counts.served += 1;
    Ok(state)
}

/// The variable that has the first build of a workspace be the daemon's too.
const STAGED: &str = "RUSTD_FIRST_BUILD";

/// Why the daemon's programs are not the build.
enum Left {
    /// The build is cargo's, with the state so far and the reason: something cargo acts
    /// on changed, or the build is one the daemon is never asked for.
    ToCargo(Box<State>, String),
    /// The daemon did not build a program, and why.
    Failed(String),
}

/// Has the daemon bring the programs of `state` up to date and puts them where cargo has
/// them. Rustc sees none of it: the daemon builds what rust-analyzer passes, which has no
/// borrow checker and few of rustc's lints.
fn by_daemon(mut state: State) -> Result<State, Left> {
    let cargos = state.refusal.clone().or_else(|| state.broken_guard());
    if let Some(reason) = cargos.or_else(|| state.stale_cargos()) {
        return Err(Left::ToCargo(Box::new(state), reason));
    }
    let answers =
        state.checkouts.iter().map(built).collect::<Result<Vec<_>, _>>().map_err(Left::Failed)?;
    let cargos = state.settle(&answers).map_err(Left::Failed)?;
    let served = cargos.len() < state.programs.len() || state.programs.is_empty();
    state.counts.served += u64::from(served);
    Ok(state)
}

/// Has the daemon build the programs of `checkout`, loading it first when it has not.
/// `Err` is why a program is not built.
pub(super) fn built(checkout: &Checkout) -> Result<Vec<Result<(), String>>, String> {
    if checkout.programs.is_empty() {
        return Ok(Vec::new());
    }
    let reply = client::ask(Request::Build(asking(checkout)), Start::IfMissing)
        .map_err(|it| format!("{it:#}"))?;
    let programs = match reply {
        Some(Reply::Synced { programs }) => programs,
        None => return Err(client::why_none()),
        Some(Reply::Cold { reason }) => return Err(reason),
        Some(other) => return Err(format!("the daemon answered {other:?}")),
    };
    std::iter::zip(&checkout.programs, programs)
        .map(|(program, built)| match built {
            Ok(_) => Ok(Ok(())),
            Err(why) => Err(format!("rustd did not build {}: {why}", program.binary.display())),
        })
        .collect()
}

/// `checkout` as a request names it: with the variables of this process, which `env!`
/// reads where cargo set none. The ones that make rustd the wrapper are cargo's alone.
fn asking(checkout: &Checkout) -> Checkout {
    let wraps = |name: &str| name == wrapper::DRIVEN || name == switch::VARIABLE;
    let mut env: BTreeMap<String, String> =
        std::env::vars().filter(|(name, _)| !wraps(name)).collect();
    env.extend(checkout.env.clone());
    Checkout { env, ..checkout.clone() }
}

/// Tells the daemon to load `checkout` as cargo just built it, starting one when there is
/// none. `Err` is why it does not.
fn warmed(checkout: &Checkout) -> Result<(), String> {
    match client::ask(Request::Warm(asking(checkout)), Start::IfMissing) {
        Ok(Some(Reply::Cold { reason })) => Err(reason),
        Ok(_) => Ok(()),
        Err(why) => Err(format!("{why:#}")),
    }
}

/// Builds with cargo, and has the daemon load what cargo built for the builds to come.
/// `Err` is cargo's exit status when it failed.
fn by_cargo(
    asked: &Invocation,
    cwd: &Path,
    key: &str,
    counts: Counts,
) -> anyhow::Result<Result<State, i32>> {
    State::give_back(key);
    let built = match Built::by_cargo(asked)? {
        Ok(built) => built,
        Err(code) => return Ok(Err(code)),
    };
    let mut state = State::after(asked, cwd, key, built, counts)?;
    if state.refusal.is_none() {
        let asked = state.checkouts.iter().filter(|it| !it.programs.is_empty());
        let cold: Vec<String> = asked.filter_map(|it| warmed(it).err()).collect();
        for why in cold {
            say::fallback(&why, "the daemon has no part in the next build");
            state.count_cargo(&why);
        }
        for (program, why) in state.left_to_cargo() {
            say::fallback(why, &format!("`{}` is cargo's to build", program.name));
        }
    }
    Ok(Ok(state))
}

/// What the subcommand does once the programs are built.
fn after_build(asked: &Invocation, state: &State, cwd: &Path) -> anyhow::Result<i32> {
    match asked.subcommand {
        Subcommand::Build => Ok(0),
        Subcommand::Run => match run::program(asked, state, cwd)? {
            Some(code) => Ok(code),
            // Cargo says which programs there are to pick from.
            None => {
                plain_cargo(std::iter::once("run".to_owned()).chain(asked.typed.clone()).collect())
            }
        },
        Subcommand::Test if asked.flags.no_run => Ok(0),
        Subcommand::Test => {
            let code = run::tests(asked, state, cwd)?;
            let stopped = code != 0 && !asked.flags.no_fail_fast;
            // Cargo 1.99 runs no doc tests when tests are picked by target or by name.
            let picked = asked.flags.selects_targets || !asked.positional.is_empty();
            if stopped || picked || !state.doc_tests {
                return Ok(code);
            }
            Ok(code.max(run::doc_tests(asked)?))
        }
    }
}

/// `cargo rustd status`: the daemon, its workers, and how the builds of each checkout went.
fn status() -> anyhow::Result<i32> {
    use std::fmt::Write as _;
    let mut said = String::new();
    for line in switch::status(&std::env::current_dir()?).into_iter().chain(wrapper::tallied()) {
        let _ = writeln!(said, "{line}");
    }
    match client::ask(Request::Status, Start::Never)? {
        Some(Reply::Status(daemon)) => {
            let idle =
                daemon.idle_secs.map_or("never".to_owned(), |it| format!("after {it}s idle"));
            let _ = writeln!(
                said,
                "daemon: pid {}, version {}, exits {idle}",
                daemon.pid, daemon.version
            );
            for worker in daemon.workers {
                let _ = writeln!(
                    said,
                    "  worker: pid {}, {}, log {}",
                    worker.pid,
                    worker.project.display(),
                    worker.log.display()
                );
                for (root, warmth) in worker.checkouts {
                    let _ = writeln!(said, "    {}: {warmth}", root.display());
                }
            }
        }
        Some(Reply::Restart { version }) => {
            let _ = writeln!(
                said,
                "daemon: another build of rustd, version {version}; the next build replaces it"
            );
        }
        _ => said.push_str("daemon: not running\n"),
    }
    for state in State::all() {
        let to_cargo: u64 = state.counts.cargo.values().sum();
        let _ = writeln!(
            said,
            "{} ({}, {}): {} builds by the daemon, {to_cargo} by cargo",
            state.root.display(),
            state.profile,
            state.key,
            state.counts.served
        );
        for (reason, count) in &state.counts.cargo {
            let _ = writeln!(said, "  {count} x {reason}");
        }
        if let Some(refusal) = &state.refusal {
            let _ = writeln!(said, "  every build is cargo's: {refusal}");
        }
    }
    // Whoever reads this may stop reading before its end.
    let _ = std::io::Write::write_all(&mut std::io::stdout(), said.as_bytes());
    Ok(0)
}

/// `cargo rustd stop`: has the daemon stop its workers and exit.
fn stop() -> anyhow::Result<i32> {
    match client::ask(Request::Stop, Start::Never)? {
        Some(_) => {
            client::gone(&daemon::socket_in(&daemon::home()?));
            println!("daemon: stopped");
        }
        None => println!("daemon: not running"),
    }
    Ok(0)
}
