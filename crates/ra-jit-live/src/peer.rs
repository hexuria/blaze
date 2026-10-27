//! Who serves the daemon's socket: [`crate::edit`] sends source text only to a daemon of this
//! user. Another local user who bound the socket first could read the edits and forge the
//! answers. The same check as the daemon's own (`crates/ra-jit/src/socket.rs`), without the
//! `libc` crate this one does not depend on.

use std::os::{fd::AsRawFd as _, unix::net::UnixStream};

unsafe extern "C" {
    fn geteuid() -> u32;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn getsockopt(socket: i32, level: i32, name: i32, value: *mut u8, len: *mut u32) -> i32;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn getpeereid(socket: i32, uid: *mut u32, gid: *mut u32) -> i32;
}

#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    any(
        target_arch = "mips",
        target_arch = "mips64",
        target_arch = "sparc",
        target_arch = "sparc64"
    )
))]
const SOL_SOCKET: i32 = 0xffff;
#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    not(any(
        target_arch = "mips",
        target_arch = "mips64",
        target_arch = "sparc",
        target_arch = "sparc64"
    ))
))]
const SOL_SOCKET: i32 = 1;
#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    any(target_arch = "mips", target_arch = "mips64")
))]
const SO_PEERCRED: i32 = 18;
#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    any(target_arch = "powerpc", target_arch = "powerpc64")
))]
const SO_PEERCRED: i32 = 21;
#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    any(target_arch = "sparc", target_arch = "sparc64")
))]
const SO_PEERCRED: i32 = 0x40;
#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    not(any(
        target_arch = "mips",
        target_arch = "mips64",
        target_arch = "powerpc",
        target_arch = "powerpc64",
        target_arch = "sparc",
        target_arch = "sparc64"
    ))
))]
const SO_PEERCRED: i32 = 17;

/// The effective user of this process.
pub(crate) fn user() -> u32 {
    // SAFETY: `geteuid` takes nothing and can't fail.
    unsafe { geteuid() }
}

/// The user at the other end of `stream`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_user(stream: &UnixStream) -> std::io::Result<u32> {
    /// `struct ucred`.
    #[repr(C)]
    struct Ucred {
        pid: i32,
        uid: u32,
        gid: u32,
    }
    let mut peer = Ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = size_of::<Ucred>() as u32;
    // SAFETY: `peer` is a `ucred` and `len` its size, which is what `SO_PEERCRED` writes.
    let failed = unsafe {
        getsockopt(
            stream.as_raw_fd(),
            SOL_SOCKET,
            SO_PEERCRED,
            (&raw mut peer).cast(),
            &raw mut len,
        )
    };
    if failed != 0 { Err(std::io::Error::last_os_error()) } else { Ok(peer.uid) }
}

/// The user at the other end of `stream`.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn peer_user(stream: &UnixStream) -> std::io::Result<u32> {
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: both point at integers of the types `getpeereid` writes.
    let failed = unsafe { getpeereid(stream.as_raw_fd(), &raw mut uid, &raw mut gid) };
    if failed != 0 { Err(std::io::Error::last_os_error()) } else { Ok(uid) }
}

/// Connects to the daemon at `socket` if `user` serves it; another user's socket is an error,
/// before anything is sent.
pub(crate) fn connect(socket: &std::path::Path, user: u32) -> Result<UnixStream, String> {
    let stream = UnixStream::connect(socket)
        .map_err(|it| format!("no daemon at {}: {it}", socket.display()))?;
    let peer =
        peer_user(&stream).map_err(|it| format!("asking who serves {}: {it}", socket.display()))?;
    if peer != user {
        return Err(format!(
            "{} is served by user {peer}, not this one ({user}): the edit is not sent",
            socket.display()
        ));
    }
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use std::{io::Read as _, os::unix::net::UnixListener, path::PathBuf};

    use super::{connect, peer_user, user};

    /// A listener in a fresh directory, kept short for macOS's 104-byte socket paths.
    fn listen(name: &str) -> (PathBuf, UnixListener) {
        let dir = std::env::temp_dir().join(format!("rjl-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let listener = UnixListener::bind(dir.join("daemon.sock")).unwrap();
        (dir, listener)
    }

    #[test]
    fn peer_of_a_socket_pair_is_this_user() {
        let (ours, _theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        assert_eq!(peer_user(&ours).unwrap(), user());
    }

    #[test]
    fn a_daemon_of_this_user_is_connected() {
        let (dir, listener) = listen("same");
        let server = std::thread::spawn(move || peer_user(&listener.accept().unwrap().0).unwrap());
        connect(&dir.join("daemon.sock"), user()).unwrap();
        assert_eq!(server.join().unwrap(), user());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Another user can't be made here, so the daemon's user is taken as this one and the
    /// caller as another: the socket is refused and its server reads nothing before the end.
    #[test]
    fn a_daemon_of_another_user_is_refused_and_sent_nothing() {
        let (dir, listener) = listen("other");
        let server = std::thread::spawn(move || {
            let mut received = Vec::new();
            listener.accept().unwrap().0.read_to_end(&mut received).unwrap();
            received
        });
        let error = connect(&dir.join("daemon.sock"), user().wrapping_add(1)).unwrap_err();
        assert!(error.contains("not this one"), "{error}");
        assert!(server.join().unwrap().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
