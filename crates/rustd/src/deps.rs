//! Sending what cargo built of the crates that are not the workspace's to a place other
//! machines read it from, and fetching theirs: `rustd deps push REMOTE DIR` and
//! `rustd deps pull REMOTE DIR`, in the directory of `Cargo.lock`, where `DIR` is the one
//! cargo builds a profile in, such as `target/debug`.
//!
//! Cargo names all it builds of a crate, with one set of features and dependencies by one
//! compiler, after the crate and a hash of those: the fingerprint, the build script and what
//! it wrote, the rlib. Such a unit of a crate from a registry or from git is the same
//! wherever it is built, so the remote holds one archive per unit, and a list per project of
//! the units its builds used. A machine that starts with nothing fetches the units of the
//! list and cargo then builds the crates of the workspace only. Cargo checks the fingerprint
//! of each unit it is given as it would one of its own, so a unit the build has no use for
//! costs its transfer and nothing else.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, bail};
use rustc_hash::FxHashMap;

use crate::remote::{Digest, Fetched, Remote, Room, Tally, project_name, runtime};

/// The directories of a profile that hold units, the one cargo trusts last.
const PLACES: [&str; 3] = ["deps", "build", ".fingerprint"];

/// What an entry of an archive takes of it besides what the file holds.
const HEADER: u64 = 512;

/// The most the units one `rustd deps pull` fetches unpack to, unless `RUSTD_DEPS_MAX`
/// says: `off` for no limit.
const UNPACKED_MAX: u64 = 64 << 30;

/// The bound on what a pull unpacks, from `RUSTD_DEPS_MAX`.
fn unpacked_max() -> anyhow::Result<Option<u64>> {
    match std::env::var("RUSTD_DEPS_MAX") {
        Err(_) => Ok(Some(UNPACKED_MAX)),
        Ok(text) if text == "off" => Ok(None),
        Ok(text) => Ok(Some(
            hir_ty::mir::codegen::parse_size(&text)
                .with_context(|| format!("RUSTD_DEPS_MAX=`{text}` is neither a size nor `off`"))?,
        )),
    }
}

/// The archives the lines of a list of units name.
pub(crate) fn archives(lines: &str) -> impl Iterator<Item = Digest> {
    Listed::parse(lines).0.into_values()
}

/// The units a project's builds used and the archive of each: `unit hash` a line.
struct Listed(BTreeMap<String, Digest>);

impl Listed {
    fn at(project: &str) -> String {
        format!("deps/{project}")
    }

    fn parse(lines: &str) -> Listed {
        let units = lines
            .lines()
            .filter_map(|line| line.split_once(' '))
            .filter(|(unit, _)| hash_of(unit).is_some())
            .filter_map(|(unit, archive)| Some((unit.to_owned(), Digest::parse(archive)?)));
        Listed(units.collect())
    }

    fn lines(&self) -> String {
        self.0.iter().map(|(unit, archive)| format!("{unit} {archive}\n")).collect()
    }

    async fn fetch(remote: &Remote, project: &str) -> anyhow::Result<Listed> {
        let lines = remote.read_list(&Listed::at(project)).await?;
        Ok(Listed::parse(&lines.unwrap_or_default()))
    }

    /// The list as the machine that adds to it reads it.
    async fn fetch_to_extend(remote: &Remote, project: &str) -> anyhow::Result<Listed> {
        let lines = remote.read_list_to_extend(&Listed::at(project)).await?;
        Ok(Listed::parse(&lines.unwrap_or_default()))
    }
}

fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|it| it.is_ascii_hexdigit())
}

/// The hash at the end of the name of a unit, `serde-33c9cbd578789acd`. A name that has one
/// is safe as the name of a file.
fn hash_of(unit: &str) -> Option<&str> {
    let (krate, hash) = unit.rsplit_once('-')?;
    let plain = |it: u8| it.is_ascii_alphanumeric() || it == b'-' || it == b'_';
    (!krate.is_empty() && krate.bytes().all(plain) && is_hex(hash, 16)).then_some(hash)
}

/// The packages of the lockfile that are read from a path: the workspace's, and the ones a
/// checkout has next to it. Cargo rebuilds those by the times of their files, which say
/// nothing on another machine.
fn local_packages(lock: &str) -> BTreeSet<String> {
    let mut local = BTreeSet::new();
    for package in lock.split("[[package]]").skip(1) {
        let field = |name: &str| {
            let line = package.lines().find_map(|it| it.strip_prefix(name)?.strip_prefix(" = "))?;
            Some(line.trim_matches('"').to_owned())
        };
        if let (Some(name), None) = (field("name"), field("source")) {
            local.insert(name);
        }
    }
    local
}

/// The units in `dir` that are not of a local package, by name.
fn units(dir: &Path, local: &BTreeSet<String>) -> anyhow::Result<BTreeSet<String>> {
    let mut units = BTreeSet::new();
    let fingerprints = dir.join(".fingerprint");
    if !fingerprints.exists() {
        return Ok(units);
    }
    for entry in std::fs::read_dir(fingerprints)? {
        if let Ok(unit) = entry?.file_name().into_string()
            && hash_of(&unit).is_some()
            && let Some((package, _)) = unit.rsplit_once('-')
            && !local.contains(package)
        {
            units.insert(unit);
        }
    }
    Ok(units)
}

/// What `deps` holds of each unit, by the hash of the unit: `libserde-HASH.rlib`.
fn outputs(dir: &Path) -> anyhow::Result<FxHashMap<String, Vec<String>>> {
    let mut outputs: FxHashMap<String, Vec<String>> = FxHashMap::default();
    let Ok(entries) = std::fs::read_dir(dir.join("deps")) else { return Ok(outputs) };
    for entry in entries {
        let Ok(name) = entry?.file_name().into_string() else { continue };
        let stem = name.split('.').next().unwrap_or(&name);
        if let Some(hash) = hash_of(stem) {
            outputs.entry(hash.to_owned()).or_default().push(name);
        }
    }
    Ok(outputs)
}

/// The archive of `unit`: what each of the places holds of it, the fingerprint last.
fn archive(dir: &Path, unit: &str, outputs: &[String]) -> anyhow::Result<Vec<u8>> {
    let packed = zstd::Encoder::new(Vec::new(), 3)?;
    let mut tar = tar::Builder::new(packed);
    tar.follow_symlinks(false);
    for output in outputs {
        tar.append_path_with_name(dir.join("deps").join(output), format!("deps/{output}"))?;
    }
    for place in &PLACES[1..] {
        let at = dir.join(place).join(unit);
        if at.exists() {
            tar.append_dir_all(format!("{place}/{unit}"), at)?;
        }
    }
    Ok(tar.into_inner()?.finish()?)
}

/// Sends the units of `dir` the project's list lacks, and lists the ones that arrived.
async fn push(
    dir: &Path,
    remote: &Remote,
    project: &str,
    local: &BTreeSet<String>,
) -> anyhow::Result<String> {
    remote.may_write()?;
    // A list this machine does not trust is one it does not add to: see `extend_list`.
    let listed = Listed::fetch_to_extend(remote, project).await;
    let listed = listed.unwrap_or_else(|_| Listed(BTreeMap::new()));
    let (units, outputs) = (units(dir, local)?, Arc::new(outputs(dir)?));
    let missing = units.into_iter().filter(|it| !listed.0.contains_key(it));
    let sent = Mutex::new(BTreeMap::new());
    let send = |unit: String| {
        let (outputs, sent) = (outputs.clone(), &sent);
        async move {
            let (dir, of) = (dir.to_owned(), unit.clone());
            let pack = move || {
                let outputs = hash_of(&of).and_then(|hash| outputs.get(hash));
                archive(&dir, &of, outputs.map_or(&[], |it| it))
            };
            let bytes = tokio::task::spawn_blocking(pack).await??;
            let (hash, len) = (Digest::of(&bytes), bytes.len() as u64);
            remote.send_named_by_hash(&format!("units/{hash}"), bytes).await?;
            crate::lock(sent).insert(unit, hash);
            Ok(Fetched::Whole(len))
        }
    };
    let tally = Tally::of(missing, remote.jobs(), send).await;
    let sent = sent.into_inner().unwrap_or_default();
    if !sent.is_empty() {
        let extend = |lines: &str| {
            let mut listed = Listed::parse(lines);
            listed.0.extend(sent.iter().map(|(unit, hash)| (unit.clone(), *hash)));
            listed.lines()
        };
        remote.extend_list(&Listed::at(project), extend).await?;
    }
    let said = format!("{} units added to `{project}`, {} bytes sent", tally.done, tally.bytes);
    tally.settle(said, remote.traffic())
}

/// Unpacks the archive of `unit` in `dir`: only files and directories that are of the unit,
/// and no more of them than there is `room` for.
fn unpack(dir: &Path, unit: &str, archive: impl std::io::Read, room: &Room) -> anyhow::Result<()> {
    let mut tar = tar::Archive::new(zstd::Decoder::new(archive)?);
    tar.set_preserve_mtime(true);
    for entry in tar.entries()? {
        let mut entry = entry?;
        // Only what is of this unit: an archive is not trusted to say where it goes.
        let path = entry.path()?.into_owned();
        let mut parts = path.components().map(|it| it.as_os_str().to_str());
        let (Some(Some(place)), Some(Some(name))) = (parts.next(), parts.next()) else {
            bail!("{unit} holds {}", path.display());
        };
        let of_unit = match place {
            "deps" => hash_of(name.split('.').next().unwrap_or(name)) == hash_of(unit),
            "build" | ".fingerprint" => name == unit,
            _ => false,
        };
        // Nor a link, which is to wherever it says. A file with holes is a file.
        let kind = entry.header().entry_type();
        if !of_unit || !(kind.is_file() || kind.is_dir() || kind.is_gnu_sparse()) {
            bail!("{unit} holds {}", path.display());
        }
        // An archive is small for what it unpacks to, and says itself how much that is.
        let size = entry.header().size().unwrap_or(u64::MAX).max(entry.size());
        room.take(size.saturating_add(HEADER))?;
        if !entry.unpack_in(dir)? {
            bail!("{unit} holds {}, which is outside of it", path.display());
        }
    }
    Ok(())
}

/// Fetches the units of the project's list that `dir` has no fingerprint of.
async fn pull(
    dir: &Path,
    remote: &Remote,
    project: &str,
    local: &BTreeSet<String>,
    unpacked_max: Option<u64>,
) -> anyhow::Result<String> {
    let room = Arc::new(Room::of(unpacked_max));
    tokio::fs::create_dir_all(dir).await?;
    let listed = Listed::fetch(remote, project).await?;
    let missing =
        listed.0.iter().filter(|(unit, _)| !dir.join(".fingerprint").join(unit).exists()).filter(
            |(unit, _)| unit.rsplit_once('-').is_some_and(|(package, _)| !local.contains(package)),
        );
    let fetch = |(unit, hash): (&String, &Digest)| {
        let (unit, hash, room) = (unit.clone(), *hash, room.clone());
        async move {
            let archive = dir.join(format!("{hash}.unit"));
            let (from, most) = (format!("units/{hash}"), remote.object_max());
            let fetched = remote.fetch_file(&from, &hash, &archive, most, |_| true).await?;
            if let Fetched::Whole(_) = fetched {
                let (dir, from) = (dir.to_owned(), archive.clone());
                let unpacked = tokio::task::spawn_blocking(move || {
                    unpack(&dir, &unit, std::fs::File::open(&from)?, &room)
                });
                let unpacked = unpacked.await;
                tokio::fs::remove_file(&archive).await?;
                unpacked??;
            }
            Ok(fetched)
        }
    };
    let tally = Tally::of(missing, remote.jobs(), fetch).await;
    let said = format!("{} units of `{project}` fetched, {} bytes", tally.done, tally.bytes);
    tally.settle(said, remote.traffic())
}

/// Where the turns of the lists of units are remembered: with the store, when the machine
/// has one, and with the units themselves when it does not.
fn turns_kept_in(dir: &Path) -> PathBuf {
    match std::env::var_os("RUSTD_CACHE") {
        Some(store) => PathBuf::from(store),
        None => dir.join(".rustd"),
    }
}

/// `rustd deps push|pull REMOTE DIR [PROJECT]`, in the directory of `Cargo.lock`.
pub(crate) fn command(mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    const USAGE: &str = "usage: rustd deps push|pull REMOTE DIR [PROJECT]";
    let (Some(verb), Some(remote), Some(dir)) = (args.next(), args.next(), args.next()) else {
        bail!(USAGE);
    };
    let project = project_name(args.next())?;
    let lock = std::fs::read_to_string("Cargo.lock").context("there is no Cargo.lock here")?;
    let local = local_packages(&lock);
    let dir = Path::new(&dir);
    let start = std::time::Instant::now();
    let done = runtime()?.block_on(async {
        let remote = Remote::open(&remote, &turns_kept_in(dir)).await?;
        let done = match verb.as_str() {
            "push" => push(dir, &remote, &project, &local).await,
            "pull" => pull(dir, &remote, &project, &local, unpacked_max()?).await,
            _ => bail!(USAGE),
        };
        eprintln!("remote: {}", remote.traffic());
        done
    })?;
    println!("{done} in {:.1?}", start.elapsed());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::{
        LIST_MAX,
        tests::{changed, held, put, scratch, unsigned, wait},
    };

    const UNIT: &str = "serde-33c9cbd578789acd";

    fn packed(entries: impl FnOnce(&mut tar::Builder<Vec<u8>>)) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        entries(&mut tar);
        zstd::encode_all(&tar.into_inner().unwrap()[..], 3).unwrap()
    }

    /// A remote whose list for `default` has one unit, the archive `entries` are in, and
    /// that takes at most `object_max` bytes of an archive.
    fn remote_with(
        name: &str,
        object_max: u64,
        entries: impl FnOnce(&mut tar::Builder<Vec<u8>>),
    ) -> (Remote, PathBuf, u64) {
        let archive = packed(entries);
        let hash = Digest::of(&archive);
        let (remote, dir) = unsigned(name, object_max);
        put(&dir, &format!("units/{hash}"), &archive);
        put(&dir, "deps/default", format!("{UNIT} {hash}\n").as_bytes());
        (remote, dir, archive.len() as u64)
    }

    fn header(kind: tar::EntryType, size: u64) -> tar::Header {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(kind);
        header.set_size(size);
        header.set_mode(0o644);
        header
    }

    fn rlib() -> String {
        format!("deps/libserde-{}.rlib", &UNIT[6..])
    }

    fn one_rlib(tar: &mut tar::Builder<Vec<u8>>) {
        tar.append_data(&mut header(tar::EntryType::Regular, 4), rlib(), &b"rlib"[..]).unwrap();
    }

    fn pulled(dir: &Path, remote: &Remote, unpacked_max: Option<u64>) -> anyhow::Result<String> {
        wait(pull(dir, remote, "default", &BTreeSet::new(), unpacked_max))
    }

    #[test]
    fn a_unit_is_unpacked_where_it_belongs() {
        let (remote, top, _) = remote_with("unit", 1 << 20, one_rlib);
        let dir = top.join("target");
        pulled(&dir, &remote, Some(1 << 20)).unwrap();
        assert_eq!(std::fs::read(dir.join(rlib())).unwrap(), b"rlib");
        assert_eq!(held(&dir), 4, "the archive is not left behind");
        _ = std::fs::remove_dir_all(&top);
    }

    #[test]
    fn a_unit_with_a_link_or_a_file_of_another_is_refused() {
        let outside = scratch("link-outside");
        std::fs::create_dir_all(&outside).unwrap();
        let link = |tar: &mut tar::Builder<Vec<u8>>| {
            let mut header = header(tar::EntryType::Symlink, 0);
            tar.append_link(&mut header, format!("build/{UNIT}"), &outside).unwrap();
            let inside = format!("build/{UNIT}/planted");
            tar.append_data(&mut self::header(tar::EntryType::Regular, 1), inside, &b"x"[..])
                .unwrap();
        };
        let other = |tar: &mut tar::Builder<Vec<u8>>| {
            let name = "deps/libother-0123456789abcdef.rlib";
            tar.append_data(&mut header(tar::EntryType::Regular, 1), name, &b"x"[..]).unwrap();
        };
        let elsewhere = |tar: &mut tar::Builder<Vec<u8>>| {
            let mut header = header(tar::EntryType::Regular, 1);
            let name = format!("build/{UNIT}/../../../planted");
            header.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name.as_bytes());
            header.set_cksum();
            tar.append(&header, &b"x"[..]).unwrap();
        };
        let archives: [(&str, &dyn Fn(&mut tar::Builder<Vec<u8>>)); 3] =
            [("link", &link), ("other", &other), ("elsewhere", &elsewhere)];
        for (name, entries) in archives {
            let (remote, top, _) = remote_with(name, 1 << 20, entries);
            let dir = top.join("out/target/debug");
            assert!(pulled(&dir, &remote, Some(1 << 20)).is_err(), "{name}");
            assert!(!outside.join("planted").exists(), "{name}");
            assert!(!top.join("out/planted").exists() && !dir.join("deps").exists(), "{name}");
            assert_eq!(held(&dir), 0, "{name}");
            _ = std::fs::remove_dir_all(&top);
        }
        _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn an_archive_longer_than_is_taken_is_refused() {
        let (_, top, long) = remote_with("long", 0, one_rlib);
        let trust = || crate::remote::tests::taking_unsigned();
        let dir = top.join("target");
        for short in [0, 1, long - 1] {
            let remote = Remote::in_dir(&top, &top.join("kept"), trust(), short);
            let refused = pulled(&dir, &remote, None).unwrap_err().to_string();
            assert!(refused.contains("1 failed, 1 of them rejected"), "{short}: {refused}");
            assert_eq!(held(&dir), 0, "{short}");
        }
        let remote = Remote::in_dir(&top, &top.join("kept"), trust(), long);
        pulled(&dir, &remote, None).unwrap();
        assert_eq!(held(&dir), 4);
        _ = std::fs::remove_dir_all(&top);
    }

    #[test]
    fn a_list_of_units_longer_than_is_read_is_refused() {
        let (remote, top) = unsigned("long-list", 1 << 20);
        let line = format!("{UNIT} {}\n", "ab".repeat(32));
        let mut list = line.repeat(LIST_MAX as usize / line.len());
        list.push_str(&" ".repeat(LIST_MAX as usize - list.len()));
        put(&top, "deps/default", list.as_bytes());
        assert_eq!(wait(Listed::fetch(&remote, "default")).unwrap().0.len(), 1);
        list.push(' ');
        put(&top, "deps/default", list.as_bytes());
        assert!(wait(Listed::fetch(&remote, "default")).is_err());
        assert!(wait(Listed::fetch_to_extend(&remote, "default")).is_err());
        _ = std::fs::remove_dir_all(&top);
    }

    #[test]
    fn an_archive_that_unpacks_to_more_than_is_taken_is_refused() {
        let file = |at: u32| format!("build/{UNIT}/out/{at}");
        let (remote, top, long) = remote_with("bomb", 1 << 20, |tar| {
            for at in 0..64 {
                let zeros = vec![0; 1 << 20];
                let mut header = header(tar::EntryType::Regular, 1 << 20);
                tar.append_data(&mut header, file(at), &zeros[..]).unwrap();
            }
        });
        assert!(long < 64 << 10, "the archive is small");
        let dir = top.join("target");
        let refused = pulled(&dir, &remote, Some(4 << 20)).unwrap_err().to_string();
        assert!(refused.contains("1 failed"), "{refused}");
        assert!(held(&dir) <= 4 << 20, "{} bytes were written", held(&dir));
        let dir = top.join("roomy");
        pulled(&dir, &remote, Some((64 << 20) + 64 * HEADER)).unwrap();
        assert_eq!(held(&dir), 64 << 20);
        _ = std::fs::remove_dir_all(&top);
    }

    #[test]
    fn no_bytes_of_an_archive_write_outside_of_its_unit_or_past_the_bound() {
        let mut tar = tar::Builder::new(Vec::new());
        one_rlib(&mut tar);
        let mut dir = header(tar::EntryType::Directory, 0);
        tar.append_data(&mut dir, format!("build/{UNIT}"), &b""[..]).unwrap();
        let out = format!("build/{UNIT}/output");
        tar.append_data(&mut header(tar::EntryType::Regular, 9), out, &b"cargo:x=1"[..]).unwrap();
        let seed = tar.into_inner().unwrap();
        let top = scratch("changed-archives");
        let dir = top.join("target/debug");
        let runs: usize = std::env::var("RUSTD_FUZZ_RUNS").map_or(3_000, |it| it.parse().unwrap());
        let (mut unpacked, most) = (0, 1 << 16);
        for bytes in changed(&seed, runs) {
            _ = std::fs::remove_dir_all(&top);
            std::fs::create_dir_all(&dir).unwrap();
            let packed = zstd::encode_all(&bytes[..], 1).unwrap();
            unpacked += unpack(&dir, UNIT, &packed[..], &Room::of(Some(most))).is_ok() as usize;
            // The same bytes with nothing packing them are not an archive either.
            _ = unpack(&dir, UNIT, &bytes[..], &Room::of(Some(most)));
            assert!(held(&dir) <= most, "{} bytes were written", held(&dir));
            let around: Vec<_> = std::fs::read_dir(&top).unwrap().flatten().collect();
            assert!(around.len() == 1 && held(&top) == held(&dir), "{around:?}");
            for place in std::fs::read_dir(&dir).unwrap().flatten() {
                let place = place.file_name().into_string().unwrap();
                assert!(matches!(&*place, "deps" | "build"), "{place}");
            }
        }
        assert!(unpacked > runs / 100, "{unpacked} of {runs} unpacked");
        _ = std::fs::remove_dir_all(&top);
    }

    #[test]
    fn no_bytes_break_the_reading_of_a_list_of_units() {
        let seed =
            format!("{UNIT} {}\nlibc-0123456789abcdef {}\n", "ab".repeat(32), "0f".repeat(32));
        let mut listed = 0;
        for bytes in changed(seed.as_bytes(), 20_000) {
            let Ok(text) = String::from_utf8(bytes) else { continue };
            for (unit, archive) in &Listed::parse(&text).0 {
                // Both become names of files.
                assert!(hash_of(unit).is_some(), "{unit}");
                assert!(!unit.contains(['/', '\\', '\0']) && !unit.starts_with('.'), "{unit}");
                assert_eq!(archive.to_string().len(), 64);
                listed += 1;
            }
        }
        assert!(listed > 1000);
    }
}
