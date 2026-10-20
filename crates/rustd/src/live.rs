//! Edits applied to a program while it runs. The program, through `ra-jit-live`, says
//! where it is and where it has room; each patch that is spliced into its executable is
//! also laid out for that room and left for it to take. See `crates/ra-jit-live`.

use std::{
    sync::{LazyLock, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, ensure};
use ra_jit_live::format::{self, Body, Patch, Process};
use rustc_hash::FxHashMap;

use crate::{lock, splice::Baseline};

/// What was last laid out for the program that runs each executable, by its UUID.
static PUBLISHED: LazyLock<Mutex<FxHashMap<[u8; 16], Published>>> = LazyLock::new(Default::default);

#[derive(Clone, Copy)]
struct Published {
    pid: u32,
    /// Where the patch ended: what was laid out may not be applied yet, and the next goes
    /// behind it.
    end: u64,
    sequence: u64,
}

/// The program that runs the executable with `uuid` and takes edits, when one does.
fn running(uuid: &[u8; 16]) -> Option<Process> {
    let text = std::fs::read_to_string(format::dir(uuid)?.join("process")).ok()?;
    Process::decode(&text).filter(|it| it.uuid == *uuid && alive(it.pid))
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else { return false };
    // SAFETY: a signal of 0 is only a question.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(not(unix))]
fn alive(_: u32) -> bool {
    false
}

/// Lays `patch` out for the program that runs `baseline`'s executable, if one does, and
/// leaves it for the program. What cannot be laid out leaves the program word to restart.
pub(crate) fn publish(baseline: &Baseline, patch: &std::path::Path) {
    let Some(uuid) = baseline.uuid() else { return };
    let Some(process) = running(&uuid) else { return };
    let mut published = lock(&PUBLISHED);
    let last = published.get(&uuid).copied().filter(|it| it.pid == process.pid);
    let base = last.map_or(process.next, |it| it.end.max(process.next));
    let laid = (|| -> anyhow::Result<(Body, u64)> {
        if let Some((at, why)) = &*lock(&TYPES)
            && *at > process.started
        {
            return Ok((Body::Restart(why.clone()), base));
        }
        let (_, code) = baseline.live(&std::fs::read(patch)?, base)?;
        let end = base + (code.text.len() + code.data.len()).next_multiple_of(0x4000) as u64;
        ensure!(end <= process.end, "the room the program set aside for edits is used up");
        Ok((Body::Code(code), end))
    })();
    let (body, end) = match laid {
        Ok(laid) => laid,
        Err(why) => (Body::Restart(format!("{why:#}")), base),
    };
    let shown = match &body {
        Body::Code(code) => format!("{} functions redirected", code.redirected),
        Body::Restart(why) => format!("restart needed: {why}"),
    };
    let sequence = next_sequence(last);
    match write(&uuid, &Patch { uuid, sequence, body }) {
        Ok(()) => {
            eprintln!("live: edit {sequence} for process {}: {shown}", process.pid);
            published.insert(uuid, Published { pid: process.pid, end, sequence });
        }
        Err(why) => eprintln!("live: cannot leave the edit for process {}: {why:#}", process.pid),
    }
}

/// When an edit last changed a type, and which: a program started before has values
/// laid out as they were, which code written for the new layout misreads.
static TYPES: Mutex<Option<(u64, String)>> = Mutex::new(None);

pub(crate) fn types_changed(names: &[String]) {
    let shown = match names {
        [one] => format!("`{one}`"),
        [first, rest @ ..] => format!("`{first}` and {} other types", rest.len()),
        [] => return,
    };
    let why = format!("the edit changes {shown}, which the running program may have values of");
    *lock(&TYPES) = Some((now(), why));
}

/// The executables that were linked again, with why, whose programs are told to restart
/// at the next edit: the edit that was answered with the new link may still be on its way
/// to them, in the file the word to restart would take the place of.
static SUPERSEDED: Mutex<Vec<([u8; 16], String)>> = Mutex::new(Vec::new());

pub(crate) fn superseded(uuid: [u8; 16], why: String) {
    lock(&SUPERSEDED).push((uuid, why));
}

/// Tells the programs that run an executable that was linked again to restart.
pub(crate) fn tell_superseded() {
    let superseded = std::mem::take(&mut *lock(&SUPERSEDED));
    for (uuid, why) in superseded {
        tell_restart(uuid, &why);
    }
}

/// Tells the program that runs `baseline`'s executable, if one does, that it has to be
/// started again to see the edit.
pub(crate) fn restart(baseline: &Baseline, why: &str) {
    if let Some(uuid) = baseline.uuid() {
        tell_restart(uuid, why);
    }
}

fn tell_restart(uuid: [u8; 16], why: &str) {
    let Some(process) = running(&uuid) else { return };
    let mut published = lock(&PUBLISHED);
    let last = published.get(&uuid).copied().filter(|it| it.pid == process.pid);
    let sequence = next_sequence(last);
    let patch = Patch { uuid, sequence, body: Body::Restart(why.to_owned()) };
    if write(&uuid, &patch).is_ok() {
        eprintln!("live: process {} has to be restarted: {why}", process.pid);
        let end = last.map_or(process.next, |it| it.end);
        published.insert(uuid, Published { pid: process.pid, end, sequence });
    }
}

/// A number above any the program has seen, also from a daemon that ran before this one.
fn next_sequence(last: Option<Published>) -> u64 {
    now().max(last.map_or(0, |it| it.sequence + 1))
}

/// Milliseconds since the epoch, which the program counts in too.
fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |it| it.as_millis() as u64)
}

fn write(uuid: &[u8; 16], patch: &Patch) -> anyhow::Result<()> {
    let dir = format::dir(uuid).context("no home directory")?;
    let partial = dir.join(format!("patch.{}", std::process::id()));
    std::fs::write(&partial, patch.encode())?;
    std::fs::rename(&partial, dir.join("patch"))?;
    Ok(())
}
