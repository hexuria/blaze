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
//! The daemon also tiers the program up while it runs: when it asks, a thread of this crate
//! samples where the program's threads are, and the daemon compiles the functions they are
//! found in again with LLVM and sends them as patches like any other. [`tiers`] says what it
//! did, for the program to show. `RA_JIT_TIER=off` in the program's environment turns the
//! sampling off.
//!
//! macOS on AArch64 and Linux on x86-64; elsewhere [`Live::start`] says so.

use std::{fmt, path::PathBuf, sync::OnceLock, time::Duration};

pub mod format;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod linux;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod macos;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod sample;

/// The directory [`Live::start`] takes edits through.
static DIR: OnceLock<PathBuf> = OnceLock::new();

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
        /// How long writing it into the program took.
        took: Duration,
        /// How long it waited between arriving and [`Live::poll`] applying it.
        waited: Duration,
    },
    /// The edit needs the program started again, for this reason.
    Restart(String),
    /// The patch could not be applied, for this reason; the program runs on as it was.
    Declined(String),
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::Applied { sequence, functions, statics, restored, took, waited } => {
                write!(f, "edit {sequence} applied: {functions} functions")?;
                if *statics != 0 {
                    write!(f, ", {statics} statics")?;
                }
                if *restored != 0 {
                    write!(f, ", {restored} put back")?;
                }
                write!(f, " in {took:.1?} after waiting {waited:.1?}")
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
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    inner: linux::Live,
}

impl Live {
    /// Sets aside room for edits next to the program's code and starts watching for them.
    pub fn start() -> Result<Live, String> {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        return macos::Live::start().map(|inner| Live { inner });
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        return linux::Live::start().map(|inner| Live { inner });
        #[cfg(not(any(
            all(target_os = "macos", target_arch = "aarch64"),
            all(target_os = "linux", target_arch = "x86_64")
        )))]
        Err("live edits run on macOS AArch64 and Linux x86-64".to_owned())
    }

    /// Applies the newest patch that came in since the last call, if any.
    pub fn poll(&mut self) -> Option<Outcome> {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        return self.inner.poll();
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        return self.inner.poll();
        #[cfg(not(any(
            all(target_os = "macos", target_arch = "aarch64"),
            all(target_os = "linux", target_arch = "x86_64")
        )))]
        None
    }

    /// Applies each patch as soon as it comes, on a thread of its own, instead of at
    /// [`Live::poll`], and tells `told` what became of it. For a program whose functions
    /// can change between any two calls; one of many functions that work together can be
    /// in the middle of a frame when some of them do.
    pub fn apply_as_they_come(self, told: impl FnMut(Outcome) + Send + 'static) {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        self.inner.apply_as_they_come(told);
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        self.inner.apply_as_they_come(told);
        #[cfg(not(any(
            all(target_os = "macos", target_arch = "aarch64"),
            all(target_os = "linux", target_arch = "x86_64")
        )))]
        drop(told);
    }
}

/// The counters that code the daemon compiled to count its branches adds to, which the
/// sampler reports with the samples. That code finds them by calling this, by name.
#[unsafe(no_mangle)]
pub extern "C" fn ra_jit_live_counters() -> *mut u64 {
    static mut COUNTERS: [u64; format::COUNTERS] = [0; format::COUNTERS];
    &raw mut COUNTERS as *mut u64
}

/// What the daemon last said of the tiers of this program: the functions it spends its time
/// in, the level each is compiled at, and what the daemon did of late. Nothing before
/// [`Live::start`], or while the daemon is not tiering the program. It is read from a file
/// the daemon writes a few times a second, so a program that shows it reads it about as often,
/// not every frame.
pub fn tiers() -> Option<format::Tiers> {
    let path = DIR.get()?.join(format::TIERS);
    format::Tiers::decode(&std::fs::read_to_string(path).ok()?)
}

/// Hands the daemon the text of the source file at `path`, as an editor has it, without
/// writing it: the daemon builds from that text at once and a program that runs takes the
/// patch. The answer is the daemon's, as `cargo ra-jit build` prints it: `ok patch=...
/// functions=N files=N ms=N`, `ok unchanged`, or why the edit is not built. The file itself
/// is the editor's to write when it likes; the daemon sees that it has that text already. A socket
/// served by another user is refused; nothing is sent to it.
#[cfg(unix)]
pub fn edit(path: &std::path::Path, text: &str) -> Result<String, String> {
    let socket = daemon_socket().ok_or("no home directory to find the daemon in")?;
    let path = std::path::absolute(path).map_err(|it| format!("{}: {it}", path.display()))?;
    let path = path.to_str().ok_or("a path that is not UTF-8")?;
    let mut line = String::with_capacity(text.len() + path.len() + 8);
    line.push_str("edit\t");
    format::escape(path, &mut line);
    line.push('\t');
    format::escape(text, &mut line);
    line.push('\n');

    ask(&socket, &line, user())
}

#[cfg(unix)]
fn ask(socket: &std::path::Path, line: &str, user: u32) -> Result<String, String> {
    use std::io::{BufRead as _, Write as _};

    let mut stream = std::os::unix::net::UnixStream::connect(socket)
        .map_err(|it| format!("no daemon at {}: {it}", socket.display()))?;
    let peer =
        peer_user(&stream).map_err(|it| format!("asking who serves {}: {it}", socket.display()))?;
    if peer != user {
        return Err(format!(
            "{} is served by user {peer}, not this one: the edit is not sent",
            socket.display()
        ));
    }
    let mut reply = String::new();
    stream.write_all(line.as_bytes()).map_err(|it| format!("sending the edit: {it}"))?;
    std::io::BufReader::new(stream)
        .read_line(&mut reply)
        .map_err(|it| format!("reading the daemon's answer: {it}"))?;
    let reply = reply.trim_end();
    match reply.strip_prefix("error: ") {
        _ if reply.is_empty() => Err("the daemon hung up without an answer".to_owned()),
        _ if reply.starts_with("ok") => Ok(reply.to_owned()),
        Some(why) => Err(format::unescape(why)),
        None => Err(reply.to_owned()),
    }
}

#[cfg(unix)]
fn user() -> u32 {
    // SAFETY: `geteuid` takes nothing and can't fail.
    unsafe { libc::geteuid() }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_user(stream: &std::os::unix::net::UnixStream) -> std::io::Result<u32> {
    use std::os::fd::AsRawFd as _;
    let mut peer = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `peer` is a `ucred` and `len` its size, which is what `SO_PEERCRED` writes.
    let failed = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut peer).cast(),
            &raw mut len,
        )
    };
    if failed != 0 { Err(std::io::Error::last_os_error()) } else { Ok(peer.uid) }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn peer_user(stream: &std::os::unix::net::UnixStream) -> std::io::Result<u32> {
    use std::os::fd::AsRawFd as _;
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: both point at integers of the types `getpeereid` writes.
    let failed = unsafe { libc::getpeereid(stream.as_raw_fd(), &raw mut uid, &raw mut gid) };
    if failed != 0 { Err(std::io::Error::last_os_error()) } else { Ok(uid) }
}

/// Where the daemon of this user listens, as ra-jit finds it: in `RA_JIT_HOME`, else in
/// `XDG_RUNTIME_DIR/ra-jit`, else in `~/.cache/ra-jit`.
#[cfg(unix)]
fn daemon_socket() -> Option<std::path::PathBuf> {
    let set = |name| std::env::var_os(name).filter(|it: &std::ffi::OsString| !it.is_empty());
    let home = match (set("RA_JIT_HOME"), set("XDG_RUNTIME_DIR"), set("HOME")) {
        (Some(home), _, _) => std::path::PathBuf::from(home),
        (None, Some(runtime), _) => std::path::Path::new(&runtime).join("ra-jit"),
        (None, None, Some(home)) => std::path::Path::new(&home).join(".cache/ra-jit"),
        (None, None, None) => return None,
    };
    Some(home.join("daemon.sock"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::{ask, peer_user, user};
    use std::{
        io::{BufRead as _, Read as _, Write as _},
        os::unix::net::{UnixListener, UnixStream},
    };

    fn socket(name: &str) -> (std::path::PathBuf, UnixListener) {
        let dir = std::env::temp_dir().join(format!("rjl-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let listener = UnixListener::bind(dir.join("daemon.sock")).unwrap();
        (dir, listener)
    }

    #[test]
    fn refuses_a_daemon_of_another_user() {
        let (dir, listener) = socket("refuse");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut received = [0; 1];
            let size = stream.read(&mut received).unwrap();
            received[..size].to_vec()
        });

        let error =
            ask(&dir.join("daemon.sock"), "edit\tx\ty\n", user().wrapping_add(1)).unwrap_err();
        assert!(error.contains("not this one"), "{error}");
        assert!(server.join().unwrap().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn asks_a_daemon_of_this_user() {
        let (dir, listener) = socket("asks");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let peer = peer_user(&stream).unwrap();
            let mut line = String::new();
            std::io::BufReader::new(stream.try_clone().unwrap()).read_line(&mut line).unwrap();
            stream.write_all(b"ok unchanged\n").unwrap();
            (line, peer)
        });

        assert_eq!(ask(&dir.join("daemon.sock"), "edit\tx\ty\n", user()).unwrap(), "ok unchanged");
        let (line, peer) = server.join().unwrap();
        assert_eq!(line, "edit\tx\ty\n");
        assert_eq!(peer, user());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn peer_user_of_a_connected_pair_is_this_user() {
        let (dir, listener) = socket("peer");
        let server = std::thread::spawn(move || listener.accept().unwrap().0);
        let stream = UnixStream::connect(dir.join("daemon.sock")).unwrap();
        assert_eq!(peer_user(&stream).unwrap(), user());
        drop(stream);
        drop(server.join().unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
