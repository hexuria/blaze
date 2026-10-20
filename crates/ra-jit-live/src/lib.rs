//! Applies ra-jit's edits to a program while it runs.
//!
//! The daemon already answers an edit with a patch: the functions that differ from the
//! executable of the last full link, linked against that executable's symbols. On disk it
//! is spliced into the executable. A program that links this crate and calls
//! [`Live::start`] gets each patch in memory as well: the daemon lays the patch out for a
//! place the program set aside next to its own code, the program maps it there and writes
//! a jump over the entry of each function it replaces. Whatever calls the old function, a
//! call, a vtable or a function pointer handed to a library, arrives at the new one.
//!
//! [`Live::poll`] applies what came in since it was last called. Call it where nothing of
//! the program's is half done, such as between two frames: a function that is running
//! when its patch lands finishes as it was, and the next call runs the new code.
//!
//! What a patch cannot do, it says, and the program has to be started again: an edit that
//! changes a type the rest of the program can see writes the whole object again, and a
//! patch is for one executable, named by its `LC_UUID`.
//!
//! macOS on AArch64 only, for now; elsewhere [`Live::start`] says so.

use std::{fmt, time::Duration};

pub mod format;

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod macos;

/// What became of a patch.
#[derive(Debug)]
pub enum Outcome {
    Applied {
        sequence: u64,
        /// How many functions now jump to new code.
        functions: u64,
        /// How many statics were given another value.
        statics: u64,
        /// How many places an earlier edit wrote were put back as they first were.
        restored: u64,
        took: Duration,
    },
    /// The edit needs the program started again, for this reason.
    Restart(String),
    /// The patch could not be applied, for this reason; the program runs on as it was.
    Declined(String),
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::Applied { sequence, functions, statics, restored, took } => {
                write!(f, "edit {sequence} applied: {functions} functions")?;
                if *statics != 0 {
                    write!(f, ", {statics} statics")?;
                }
                if *restored != 0 {
                    write!(f, ", {restored} put back")?;
                }
                write!(f, " in {took:.1?}")
            }
            Outcome::Restart(why) => write!(f, "restart needed: {why}"),
            Outcome::Declined(why) => write!(f, "edit not applied: {why}"),
        }
    }
}

/// A program that takes edits while it runs.
pub struct Live {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    inner: macos::Live,
}

impl Live {
    /// Sets aside room for edits next to the program's code and starts watching for them.
    pub fn start() -> Result<Live, String> {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        return macos::Live::start().map(|inner| Live { inner });
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        Err("live edits run on macOS on AArch64 only".to_owned())
    }

    /// Applies the newest patch that came in since the last call, if any.
    pub fn poll(&mut self) -> Option<Outcome> {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        return self.inner.poll();
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        None
    }
}
