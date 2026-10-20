#!/usr/bin/env python3
"""soakrows.py: the files and toolchains a daemon meets that a test workspace does not have.

    soakrows.py [ROW...] [--work DIR] [--other-fs DIR] [--keep]

Each row copies the `soak/` workspace (a build script, three kinds of proc macro, a program
and its tests), has a daemon of its own load it, does the thing the row is named for, and
then compares `cargo rustd run` (or `test`) with a cold `cargo run` of the same tree. A row
is right when the two agree at every comparison and neither the daemon nor a worker logged
a panic; who built each one, the daemon or cargo with the reason rustd gave, is in the
table. A build cargo did in place of the daemon is right: that is the contract.

Writes SOAKROWS.md next to this file.
"""
import argparse, glob, os, re, shutil, signal, subprocess, sys, tempfile, time

from cargo_rustd import HERE, RUSTD, Side, base_env, difference, how_built, program_output, test_results, wait_warm

GIT = ["git", "-c", "user.name=rustd", "-c", "user.email=rustd@localhost", "-c", "advice.detachedHead=false"]
OTHER_TOOLCHAIN = "1.94.0"
MARK = re.compile(r"pub fn mark\(\) -> u64 \{ \d+ \}")


class Wrong(Exception):
    pass


class Row:
    """One workspace, the daemon that holds it, and what was compared."""

    def __init__(self, work, name, env=None, at=None):
        self.dir = os.path.join(work, name)
        self.ws = os.path.realpath(at or os.path.join(self.dir, "ws"))
        self.home = os.path.join(self.dir, "home")
        os.makedirs(self.dir, exist_ok=True)
        self.env = dict(base_env(self.home), RUSTD_CACHE=os.path.join(self.dir, "store"), **(env or {}))
        self.built, self.notes, self.mark = [], [], 1000
        self.copy(self.ws)

    def copy(self, ws):
        shutil.copytree(os.path.join(HERE, "soak"), ws, ignore=shutil.ignore_patterns("target*", "Cargo.lock"))
        with open(os.path.join(ws, "core/src/lib.rs"), "a") as lib:
            lib.write("\npub fn mark() -> u64 { 1000 }\n")
        main = os.path.join(ws, "app/src/main.rs")
        text = open(main).read().replace("fn main() {", 'fn main() {\n    println!("mark {}", soak_core::mark());', 1)
        open(main, "w").write(text)
        open(os.path.join(ws, ".gitignore"), "w").write("target\ntarget-ref\ntarget-warm\n")
        self.git("init", "-q", cwd=ws), self.git("add", ".", cwd=ws), self.git("commit", "-qm", "first", cwd=ws)

    def git(self, *args, cwd=None):
        return subprocess.run([*GIT, *args], cwd=cwd or self.ws, check=True, capture_output=True, text=True)

    def path(self, *parts, ws=None):
        return os.path.join(ws or self.ws, *parts)

    def next_mark(self):
        self.mark += 1
        return f"pub fn mark() -> u64 {{ {self.mark} }}"

    def edit(self, file=None, write=None):
        """Gives `mark` of the file another value of the same length, written by `write`."""
        file = file or self.path("core/src/lib.rs")
        text = MARK.sub(self.next_mark(), open(file).read())
        (write or in_place)(file, text)

    def warm(self, ws=None, env=None):
        side = Side(ws or self.ws, dict(self.env, **(env or {})), True)
        _, ran = side.cargo("build")
        if ran.returncode:
            raise Wrong(f"the first build failed: {ran.stderr[-300:]}")
        if "daemon: not running" in side.cargo("status")[1].stdout:
            raise Wrong(f"no daemon after the first build: {ran.stderr[-300:]}")
        _, states = wait_warm(side, os.path.realpath(ws or self.ws))
        return states

    def both(self, label, command="run", ws=None, env=None, args=(), want=None):
        """Compares `cargo rustd COMMAND` with a cold `cargo COMMAND` of the tree."""
        ws, env = ws or self.ws, dict(self.env, **(env or {}))
        _, ours = Side(ws, env, True).cargo(command, *args, timeout=1800)
        reference = self.path("target-ref", ws=os.path.realpath(ws))
        shutil.rmtree(reference, ignore_errors=True)
        _, theirs = Side(ws, dict(env, CARGO_TARGET_DIR=reference), False).cargo(command, *args, timeout=1800)
        read = test_results if command == "test" else program_output
        differs = difference(read(ours), read(theirs))
        how = how_built(ours.stderr)
        self.built.append(f"{label}: {how}")
        if differs:
            raise Wrong(f"{label} ({how}): {differs}")
        if want and want not in ours.stdout:
            raise Wrong(f"{label}: both say the same, and neither says `{want}`: {ours.stdout[-200:]}")
        return how, ours

    def status(self, env=None):
        text = Side(self.ws, dict(self.env, **(env or {})), True).cargo("status")[1].stdout
        pid = re.search(r"^daemon: pid (\d+)", text, re.M)
        return (int(pid[1]) if pid else None), [int(p) for p in re.findall(r"worker: pid (\d+)", text)], text

    def panics(self):
        logs = [os.path.join(self.home, "daemon.log"), *glob.glob(os.path.join(self.home, "**", "log"), recursive=True)]
        found = []
        for log in logs:
            try:
                found += [f"{log}: {l}" for l in open(log, errors="replace").read().splitlines() if "panicked" in l]
            except OSError:
                pass
        return found

    def stop(self, env=None):
        Side(self.ws, dict(self.env, **(env or {})), True).cargo("stop")
        pid = self.status(env)[0]
        try:
            os.kill(pid or 0, signal.SIGKILL) if pid else None
        except ProcessLookupError:
            pass


def in_place(file, text):
    """As an editor that truncates the file and writes it again."""
    with open(file, "w") as out:
        out.write(text)


def by_rename(file, text):
    """As an editor that writes another file and renames it over this one."""
    other = file + ".swp~"
    with open(other, "w") as out:
        out.write(text)
        out.flush()
        os.fsync(out.fileno())
    os.replace(other, file)


def same_stamp(file, text):
    """Written with the size and the time of modification it had."""
    was = os.stat(file)
    in_place(file, text)
    os.utime(file, ns=(was.st_atime_ns, was.st_mtime_ns))


def symlinked_file(row):
    lib, shared = row.path("core/src/lib.rs"), os.path.join(row.dir, "shared")
    os.makedirs(shared)
    shutil.move(lib, os.path.join(shared, "one.rs"))
    os.symlink(os.path.join(shared, "one.rs"), lib)
    row.warm()
    row.both("as loaded")
    row.edit(os.path.join(shared, "one.rs"))
    row.both("what it points at edited", want=f"mark {row.mark}")
    text = MARK.sub(row.next_mark(), open(os.path.join(shared, "one.rs")).read())
    in_place(os.path.join(shared, "two.rs"), text)
    os.remove(lib)
    os.symlink(os.path.join(shared, "two.rs"), lib)
    row.both("pointed at another file", want=f"mark {row.mark}")


def symlinked_directory(row):
    src, shared = row.path("core/src"), os.path.join(row.dir, "shared-src")
    shutil.move(src, shared)
    os.symlink(shared, src)
    row.warm()
    row.both("as loaded")
    row.edit(os.path.join(shared, "lib.rs"))
    row.both("a file under it edited", want=f"mark {row.mark}")
    row.edit(row.path("core/src/lib.rs"), by_rename)
    row.both("edited through the link", want=f"mark {row.mark}")


def symlinked_root(row):
    link = os.path.join(row.dir, "link")
    os.symlink(row.ws, link)
    args = ("--manifest-path", os.path.join(link, "Cargo.toml"))
    env = {"PWD": link}
    row.both("cold through the link", args=args, ws=link, env=env)
    wait_warm(Side(link, row.env, True), row.ws)
    row.notes.append("status after a build through the link:\n" + row.status()[2])
    row.edit()
    row.both("edited, through the link", args=args, ws=link, env=env, want=f"mark {row.mark}")
    row.edit(os.path.join(link, "core/src/lib.rs"))
    row.both("edited through the link, by the real path", want=f"mark {row.mark}")
    row.edit()
    row.both("again through the link", args=args, ws=link, env=env, want=f"mark {row.mark}")


def save_by_rename(row):
    row.warm()
    for round in range(4):
        row.edit(write=by_rename)
        row.both(f"renamed over, {round}", want=f"mark {row.mark}")


def save_by_truncate(row):
    row.warm()
    for round in range(4):
        row.edit(write=in_place)
        row.both(f"truncated and written, {round}", want=f"mark {row.mark}")


def twice_in_a_tick(row):
    row.warm()
    for round in range(3):
        row.edit()
        Side(row.ws, row.env, True).cargo("build")
        row.edit()
        row.both(f"written again at once, {round}", want=f"mark {row.mark}")
    # The same size and the same time: nothing but the bytes says it changed. What cargo
    # does with a target directory it built in before is said too.
    warm = dict(row.env, CARGO_TARGET_DIR=row.path("target-warm"))
    Side(row.ws, warm, False).cargo("run")
    row.edit(write=same_stamp)
    _, cargos = Side(row.ws, warm, False).cargo("run")
    stale = f"mark {row.mark}" not in cargos.stdout
    row.notes.append(f"same size and time: warm cargo is {'stale' if stale else 'right'}")
    try:
        row.both("same size and time", want=f"mark {row.mark}")
    except Wrong as wrong:
        if not stale:
            raise
        row.built.append(f"same size and time: stale, as warm cargo is ({wrong})")


def checkout_of_thousands(row, count=3000):
    def branch(name, add):
        row.git("checkout", "-q", "-b", name, "master")
        os.makedirs(row.path("app/src/many"), exist_ok=True)
        for at in range(count):
            in_place(row.path(f"app/src/many/m{at}.rs"), f"pub fn f() -> u64 {{ {at + add} }}\n")
        mods = "".join(f"pub mod m{at};\n" for at in range(count))
        all_of = ", ".join(f"m{at}::f" for at in range(count))
        in_place(row.path("app/src/many/mod.rs"), f"{mods}\npub fn total() -> u64 {{\n    let all: [fn() -> u64; {count}] = [{all_of}];\n    all.iter().map(|f| f()).sum()\n}}\n")
        main = open(row.path("app/src/main.rs")).read()
        in_place(row.path("app/src/main.rs"), "mod many;\n" + main.replace("fn main() {", 'fn main() {\n    println!("many {}", many::total());'))
        row.git("add", "."), row.git("commit", "-qm", name)
        return sum(range(count)) + add * count

    row.git("branch", "-M", "master")
    one, two = branch("one", 0), branch("two", 1)
    row.git("checkout", "-q", "master")
    row.warm()
    row.both("before")
    for name, total in (("one", one), ("two", two), ("one", one)):
        row.git("checkout", "-q", name)
        row.both(f"checkout of {count} files, {name}", want=f"many {total}")
    row.git("checkout", "-q", "master")
    row.both("checkout that removes them")


def case_only_rename(row):
    in_place(row.path("core/src/helper.rs"), "pub fn help() -> u64 { 5 }\n")
    lib = open(row.path("core/src/lib.rs")).read()
    in_place(row.path("core/src/lib.rs"), lib.replace("pub fn mark() -> u64 { 1000 }", "mod helper;\npub fn mark() -> u64 { 1000 + helper::help() }"))
    row.warm()
    row.both("as loaded", want="mark 1005")
    os.rename(row.path("core/src/helper.rs"), row.path("core/src/Helper.rs"))
    lib = open(row.path("core/src/lib.rs")).read()
    in_place(row.path("core/src/lib.rs"), lib.replace("mod helper;", '#[path = "Helper.rs"]\nmod helper;'))
    in_place(row.path("core/src/Helper.rs"), "pub fn help() -> u64 { 6 }\n")
    row.both("renamed in case only", want="mark 1006")
    os.rename(row.path("core/src/Helper.rs"), row.path("core/src/helper.rs"))
    in_place(row.path("core/src/lib.rs"), lib)
    in_place(row.path("core/src/helper.rs"), "pub fn help() -> u64 { 7 }\n")
    row.both("renamed back", want="mark 1007")


def worktree_recreated(row):
    row.git("branch", "-M", "master")
    for name in ("one", "two", "three"):
        row.git("checkout", "-q", "-b", name, "master")
        row.edit()
        row.git("commit", "-qam", name)
    row.git("checkout", "-q", "master")
    tree = os.path.join(row.dir, "tree")
    row.warm()
    row.git("worktree", "add", "-q", tree, "one")
    row.warm(ws=tree)
    row.both("the worktree", ws=tree, want="mark 1001")
    row.git("worktree", "remove", "--force", tree)
    row.git("worktree", "add", "-q", tree, "two")
    row.both("removed and added again at another commit", ws=tree, want="mark 1002")
    wait_warm(Side(tree, row.env, True), tree)
    shutil.rmtree(tree)
    row.git("worktree", "prune")
    time.sleep(2.5)
    row.notes.append("status with the worktree deleted:\n" + row.status()[2])
    row.git("worktree", "add", "-q", tree, "three")
    row.both("deleted and added again", ws=tree, want="mark 1003")
    wait_warm(Side(tree, row.env, True), tree)
    row.edit(os.path.join(tree, "core/src/lib.rs"))
    how, _ = row.both("edited after that", ws=tree, want=f"mark {row.mark}")
    row.both("the first checkout still", want="mark 1000")


def worktrees_built_apart(row):
    """Two worktrees whose libraries rustc built from different source: the one's holds code
    of the standard library for a type of its own that the program calls too, and the other's
    does not. The worktree asked second is read as the first."""
    kept = "\npub fn kept(count: usize) -> usize {\n    let mut all: Vec<Reading> = Vec::new();\n    for _ in 0..count {\n        all.push(Reading::default());\n    }\n    all.len()\n}\n"
    main = row.path("app/src/main.rs")
    text = open(main).read().replace("fn main() {", 'fn main() {\n    println!("kept {}", kept(std::env::args().count() + 4));', 1)
    open(main, "w").write(text + kept.replace("pub fn", "fn").replace("Reading", "soak_core::Reading"))
    row.git("commit", "-qam", "the program keeps readings")
    row.git("branch", "-M", "master")
    row.git("checkout", "-q", "-b", "one", "master")
    with open(row.path("core/src/lib.rs"), "a") as lib:
        lib.write(kept)
    row.git("commit", "-qam", "the library keeps them too")
    row.git("checkout", "-q", "master")
    tree = os.path.join(row.dir, "tree")
    row.git("worktree", "add", "-q", tree, "one")
    row.warm(ws=tree)
    row.both("the worktree whose library keeps readings", ws=tree, want="kept 5")
    row.warm()
    def asked_after_the_other(label, ws, file):
        """The first build of one after the other may fail to link, which is said with what
        the linker lacked; the daemon loads it by itself once cargo has built it again."""
        how, _ = row.both(label, ws=ws, want="kept 5")
        if "link failed" in how and "undefined symbol" not in how:
            raise Wrong(f"refused without what the linker lacked: {how}")
        row.warm(ws=ws)
        row.edit(file)
        row.both(f"{label}, edited", ws=ws, want=f"mark {row.mark}")
        row.warm(ws=ws)
        for _ in range(3):
            row.edit(file)
            how, _ = row.both(f"{label}, edited again", ws=ws, want=f"mark {row.mark}")
            if how != "daemon":
                raise Wrong(f"an edit was not built by the daemon: {how}")

    asked_after_the_other("the one whose library does not", row.ws, None)
    asked_after_the_other("the first", tree, os.path.join(tree, "core/src/lib.rs"))


def worktrees_that_run_other_code(row):
    """Two worktrees whose proc macro and build script data differ, in one daemon: cargo
    names what each builds of them alike, so neither may be built where the other was. The
    files of the one asked second are the older ones, which is when cargo would take what
    the first built as fresh."""
    row.git("branch", "-M", "master")
    tree = os.path.join(row.dir, "tree")
    row.git("worktree", "add", "-q", tree, "-b", "one")
    time.sleep(1.1)
    gen, data = row.path("gen/src/lib.rs"), row.path("core/data.txt")
    in_place(gen, open(gen).read().replace("const SALT: u64 = 7;", "const SALT: u64 = 5;"))
    in_place(data, re.sub(r"^alpha \d+", "alpha 2", open(data).read(), flags=re.M))
    row.git("commit", "-qam", "another salt and other data")
    row.warm()
    row.both("the first, with a salt and data of its own", want="Reading/5")
    row.both("the worktree asked second", ws=tree, want="Reading/7")
    row.warm(ws=tree)
    for ws, salt in ((tree, 7), (row.ws, 5), (tree, 7)):
        row.edit(os.path.join(ws, "core/src/lib.rs"))
        how, _ = row.both(f"salt {salt}, edited", ws=ws, want=f"Reading/{salt}")
        row.warm(ws=ws)


def non_utf8_names(row):
    src = row.path("core/src").encode()
    with open(os.path.join(src, b"caf\xe9.rs"), "wb") as file:
        file.write(b"pub fn unused() {}\n")
    with open(os.path.join(src, b"latin.rs"), "wb") as file:
        file.write(b"// caf\xe9 is not UTF-8\npub fn unused() {}\n")
    os.makedirs(os.path.join(src, b"d\xff"))
    with open(os.path.join(src, b"d\xff", b"mod.rs"), "wb") as file:
        file.write(b"pub fn unused() {}\n")
    row.warm()
    row.both("with files cargo does not compile: a name and a text that are not UTF-8")
    row.edit()
    row.both("edited", want=f"mark {row.mark}")
    with open(os.path.join(src, b"latin.rs"), "ab") as file:
        file.write(b"// \xff\n")
    row.edit()
    row.both("the text that is not UTF-8 changed", want=f"mark {row.mark}")
    row.both("tests", command="test")


def non_utf8_root(row):
    """The whole workspace under a directory whose name is not UTF-8."""
    under = os.path.join(row.dir.encode(), b"r\xff\xfe")
    os.makedirs(under)
    ws = os.path.join(under, b"ws")
    shutil.copytree(row.ws.encode(), ws, ignore=lambda _, names: [it for it in names if it.startswith(b"target")])
    run = lambda argv, env: subprocess.run(argv, cwd=ws, env=env, capture_output=True, text=True, errors="replace", timeout=1800)
    for label in ("cold", "edited"):
        ours = run(["cargo", "rustd", "run"], row.env)
        theirs = run(["cargo", "run"], dict(row.env, CARGO_TARGET_DIR=os.path.join(ws, b"target-ref")))
        how = how_built(ours.stderr)
        row.built.append(f"{label}: {how}" + ("" if theirs.returncode == 0 else f" (cargo itself exits {theirs.returncode})"))
        if (ours.returncode, ours.stdout) != (theirs.returncode, theirs.stdout):
            raise Wrong(f"{label} ({how}): {ours.returncode} {ours.stdout[-200:]!r} {ours.stderr[-300:]!r} for {theirs.returncode} {theirs.stdout[-200:]!r}")
        lib = os.path.join(ws, b"core/src/lib.rs")
        in_place(lib, MARK.sub(row.next_mark(), open(lib).read()))
        time.sleep(5)


def other_filesystem(row):
    here, there = os.stat(row.ws).st_dev, os.stat(row.env["RUSTD_CACHE"] if os.path.isdir(row.env["RUSTD_CACHE"]) else row.dir).st_dev
    if here == there:
        raise Wrong("the workspace and the store are on one filesystem here; name another with --other-fs")
    row.warm()
    for round in range(3):
        row.edit()
        row.both(f"edited, {round}", want=f"mark {row.mark}")
    row.both("tests", command="test")
    # A pack is open for as long as the worker that writes it lives.
    packs = [it for kind in ("*.pack", "*.open") for it in glob.glob(os.path.join(row.env["RUSTD_CACHE"], kind))]
    stored = sum(os.path.getsize(it) for it in packs)
    row.notes.append(f"{stored} bytes in {len(packs)} packs in the store on the other filesystem")
    if not stored:
        raise Wrong("nothing was stored")


def says_its_rustc(row):
    """Has the program say which rustc built it."""
    build = open(row.path("core/build.rs")).read()
    said = ('    let rustc = std::env::var("RUSTC").unwrap();\n    let version = std::process::Command::new(rustc).arg("--version").output().unwrap();\n'
            '    println!("cargo:rustc-env=SOAK_RUSTC={}", String::from_utf8(version.stdout).unwrap().trim());\n')
    in_place(row.path("core/build.rs"), build.replace("fn main() {", "fn main() {\n" + said, 1))
    lib = open(row.path("core/src/lib.rs")).read()
    in_place(row.path("core/src/lib.rs"), lib + '\npub fn rustc() -> &\'static str { env!("SOAK_RUSTC") }\n')
    main = open(row.path("app/src/main.rs")).read()
    in_place(row.path("app/src/main.rs"), main.replace("fn main() {", 'fn main() {\n    println!("{}", soak_core::rustc());', 1))


def toolchain_switched(row):
    """The toolchain file of the directory changes under a live daemon, as `rustup
    override` or a checkout does."""
    says_its_rustc(row)
    env = {"RUSTUP_TOOLCHAIN": ""}
    file = row.path("rust-toolchain.toml")
    ours = lambda version: in_place(file, f'[toolchain]\nchannel = "{version}"\n')
    ours(row.env["RUSTUP_TOOLCHAIN"])
    row.env.pop("RUSTUP_TOOLCHAIN")
    row.warm()
    row.both("the toolchain rustd is built for", want=f"rustc {TOOLCHAIN}")
    ours(OTHER_TOOLCHAIN)
    how, _ = row.both(f"the file now says {OTHER_TOOLCHAIN}", want=f"rustc {OTHER_TOOLCHAIN}")
    if how == "daemon":
        raise Wrong("the daemon built for a toolchain it does not compile for")
    row.edit()
    row.both(f"edited under {OTHER_TOOLCHAIN}", want=f"mark {row.mark}")
    ours(TOOLCHAIN)
    row.edit()
    row.both("the file says the first again", want=f"rustc {TOOLCHAIN}")
    wait_warm(Side(row.ws, row.env, True), row.ws)
    row.edit()
    row.both("edited after that", want=f"mark {row.mark}")


def toolchains_in_turn(row):
    """Two toolchains named by the environment of the client, turn by turn."""
    says_its_rustc(row)
    row.warm()
    for round in range(3):
        for version in (OTHER_TOOLCHAIN, TOOLCHAIN):
            row.edit()
            how, ran = row.both(f"{version}, {round}", env={"RUSTUP_TOOLCHAIN": version}, want=f"rustc {version}")
            if f"mark {row.mark}" not in ran.stdout:
                raise Wrong(f"{version}, {round}: not the edit")
            if how == "daemon" and version == OTHER_TOOLCHAIN:
                raise Wrong("the daemon built for a toolchain it does not compile for")
        wait_warm(Side(row.ws, row.env, True), row.ws)


def another_build(source, bin):
    """A rustd that is another build: the same program with bytes after its end."""
    for name in ("rustd", "cargo-rustd"):
        other = os.path.join(bin, name + ".new")
        shutil.copy(os.path.join(source, name), other)
        with open(other, "ab") as file:
            file.write(b"\0" * 16)
        os.replace(other, os.path.join(bin, name))


def binary_replaced(row):
    source, bin, old = os.path.dirname(RUSTD), os.path.join(row.dir, "bin"), os.path.join(row.dir, "bin-old")
    for dir in (bin, old):
        os.makedirs(dir)
        for name in ("rustd", "cargo-rustd"):
            shutil.copy2(os.path.join(source, name), os.path.join(dir, name))
    path = row.env["PATH"]
    row.env["PATH"] = bin + os.pathsep + path
    row.warm()
    row.edit()
    row.both("before", want=f"mark {row.mark}")
    first = row.status()[0]

    another_build(source, bin)
    row.edit()
    row.both("the binary replaced under the daemon", want=f"mark {row.mark}")
    second = row.status()[0]
    if second in (None, first) or os.path.exists(f"/proc/{first}/exe") and "Z" not in open(f"/proc/{first}/stat").read().split(")")[-1].split()[0]:
        raise Wrong(f"the daemon of the build that is gone is still there: {first} then {second}")
    row.notes.append(f"daemon {first} was replaced by {second}")
    wait_warm(Side(row.ws, row.env, True), row.ws)
    row.edit()
    how, _ = row.both("the build after, by the new daemon", want=f"mark {row.mark}")

    # A client of the build before meets the daemon of the build after.
    older = {"PATH": old + os.pathsep + path}
    row.edit()
    row.both("an older client and a newer daemon", env=older, want=f"mark {row.mark}")
    third = row.status(older)[0]
    row.notes.append(f"the older client replaced daemon {second} by {third}")
    row.edit()
    row.both("and the newer client again", want=f"mark {row.mark}")
    wait_warm(Side(row.ws, row.env, True), row.ws)

    # The file the daemon runs from is deleted, and it is asked for a workspace it has no
    # worker for, by a client that is the same build from another place.
    twin = os.path.join(row.dir, "bin-twin")
    shutil.copytree(bin, twin, copy_function=shutil.copy2)
    daemon = row.status()[0]
    os.remove(os.path.join(bin, "rustd"))
    second_ws = os.path.join(row.dir, "ws2")
    row.copy(second_ws)
    apart = {"PATH": twin + os.pathsep + path}
    row.both("the binary deleted, another workspace", ws=second_ws, env=apart, want="mark 1000")
    # A daemon cannot start a worker from a file that is gone: it ends, and the next build
    # starts the one that is there.
    time.sleep(3)
    row.both("the build after that", ws=second_ws, env=apart, want="mark 1000")
    after = row.status(apart)[0]
    if after in (None, daemon):
        raise Wrong(f"the daemon whose binary was deleted is still the one asked: {daemon} then {after}")
    wait_warm(Side(second_ws, dict(row.env, **apart), True), second_ws)
    lib = os.path.join(second_ws, "core/src/lib.rs")
    row.edit(lib)
    how, _ = row.both("edited there", ws=second_ws, env=apart, want=f"mark {row.mark}")
    if how != "daemon":
        raise Wrong(f"the daemon that replaced the one whose binary was deleted did not build the edit: {how}")
    row.notes.append(f"daemon {daemon} whose binary was deleted was replaced by {after}")
    row.stop(apart)


def memory_ceiling(row):
    """With a ceiling below what one worker holds, the worker asked longest ago is stopped,
    and its workspace builds right after."""
    second = os.path.join(row.dir, "ws2")
    row.copy(second)
    row.warm()
    row.edit()
    how, _ = row.both("the first workspace, loaded", want=f"mark {row.mark}")
    _, first_workers, text = row.status()
    row.notes.append("one workspace loaded:\n" + text)
    row.warm(ws=second)
    time.sleep(3)
    _, workers, text = row.status()
    row.notes.append("a second loaded, three seconds later:\n" + text)
    if any(worker in workers for worker in first_workers):
        raise Wrong(f"the worker asked longest ago is still there: {first_workers} of {workers}")
    if not workers:
        raise Wrong("the worker asked last was stopped too")
    row.edit()
    row.both("the first workspace, its worker stopped", want=f"mark {row.mark}")
    wait_warm(Side(row.ws, row.env, True), row.ws)
    row.edit()
    how, _ = row.both("the first workspace, loaded again", want=f"mark {row.mark}")
    if how != "daemon":
        raise Wrong(f"it was not loaded again: {how}")
    row.both("its tests", command="test")
    time.sleep(3)
    row.notes.append("after the first was asked again:\n" + row.status()[2])
    log = open(os.path.join(row.home, "daemon.log"), errors="replace").read()
    row.notes.append("\n".join(l for l in log.splitlines() if "RUSTD_MEMORY_MAX" in l))


def no_ceiling(row):
    """The same two workspaces with no ceiling: both workers stay."""
    second = os.path.join(row.dir, "ws2")
    row.copy(second)
    row.warm()
    row.warm(ws=second)
    time.sleep(3)
    _, workers, text = row.status()
    row.notes.append(text)
    if len(workers) != 2:
        raise Wrong(f"{len(workers)} workers for two workspaces")
    row.edit()
    how, _ = row.both("the first workspace", want=f"mark {row.mark}")
    if how != "daemon":
        raise Wrong(f"not the daemon's: {how}")


def proc_macro_tests(row):
    """The tests of every package, the proc macro's own among them, which link the
    toolchain's `libstd`."""
    with open(row.path("gen/src/lib.rs"), "a") as lib:
        lib.write("\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn salt_is_seven() {\n        assert_eq!(super::SALT, 7);\n    }\n}\n")
    row.both("cold", command="test", args=("--workspace",))
    row.warm()
    row.edit()
    row.both("warm", command="test", args=("--workspace",))
    row.both("the proc macro alone", command="test", args=("-p", "soak-gen"))


def another_under_test(row):
    """A function that is one thing in the program and another in its tests, where it reads
    a thread local a test has set: built and tested in turn, with the profile of the package
    changed between."""
    main = row.path("app/src/main.rs")
    text = open(main).read().replace("fn main() {", 'fn main() {\n    println!("now {} {}", now(), mark());', 1)
    text += """
#[cfg(not(test))]
fn now() -> String {
    "one".to_owned()
}

#[cfg(test)]
thread_local! {
    static SET: std::cell::RefCell<Option<String>> = None.into();
}

#[cfg(test)]
fn now() -> String {
    SET.with_borrow(|set| set.as_ref().cloned().unwrap_or_else(|| "two".to_owned()))
}

pub fn mark() -> u64 { 0 }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_is_what_was_set() {
        assert_eq!(now(), "two");
        SET.with_borrow_mut(|set| *set = Some("three".to_owned()));
        assert_eq!(now(), "three");
        assert!(mark() < 100);
    }
}
"""
    open(main, "w").write(text)
    row.both("cold", want="now one")
    row.both("cold tests", command="test")
    row.warm()
    for turn in range(3):
        row.edit(main)
        row.both("edited", want="now one")
        how, _ = row.both("edited, tests", command="test")
        if how != "daemon":
            raise Wrong(f"the tests were not built by the daemon: {how}")
        if turn == 0:
            with open(row.path("Cargo.toml"), "a") as manifest:
                manifest.write("\n[profile.dev.package.soak-app]\ndebug = false\n")
            row.both("another profile, tests", command="test")
            row.warm()


def kept_to_an_object(row):
    """Tests built without incremental compilation, where rustc keeps to one object the
    functions nothing outside it calls: a test sets a thread local that a function of another
    module reads, and then a test changes into one that fails."""
    lib = row.path("core/src/lib.rs")
    with open(lib, "a") as text:
        text.write("""
pub mod clock {
    #[cfg(test)]
    thread_local! {
        pub(crate) static SET: std::cell::RefCell<Option<u64>> = None.into();
    }

    #[cfg(not(test))]
    fn now() -> u64 {
        7
    }

    #[cfg(test)]
    fn now() -> u64 {
        SET.with_borrow(|set| set.unwrap_or(7))
    }

    pub fn after(then: u64) -> bool {
        now() > then
    }
}

#[cfg(test)]
mod clock_tests {
    use super::clock::{SET, after};

    struct Set;

    impl Set {
        fn new(now: u64) -> Set {
            SET.with_borrow_mut(|set| *set = Some(now));
            Set
        }
    }

    impl Drop for Set {
        fn drop(&mut self) {
            SET.with_borrow_mut(|set| *set = None);
        }
    }

    #[test]
    fn after_what_was_set() {
        let _set = Set::new(100);
        assert!(after(50));
        assert!(super::mark() > 0);
    }

    #[test]
    fn sums() {
        assert_eq!(1 + 1, 2);
    }
}
""")
    args = ("-p", "soak-core")
    row.both("cold tests", command="test", args=args)
    row.warm()
    row.edit()
    row.both("edited, tests", command="test", args=args)
    row.warm()
    in_place(lib, open(lib).read().replace("assert_eq!(1 + 1, 2);", "assert_eq!(1 + 1, 3);"))
    _, ours = row.both("a test that now fails", command="test", args=args)
    if "sums ... FAILED" not in ours.stdout:
        raise Wrong(f"the test that fails was not seen to: {ours.stdout[-300:]}")
    row.edit()
    row.both("edited again, tests", command="test", args=args)


TOOLCHAIN = open(os.path.join(HERE, "toolchain")).read().strip()

ROWS = {
    "symlinked file": (symlinked_file, {}),
    "symlinked directory": (symlinked_directory, {}),
    "symlinked workspace root": (symlinked_root, {}),
    "save by rename": (save_by_rename, {}),
    "save by truncate and write": (save_by_truncate, {}),
    "changed twice within the time stamp": (twice_in_a_tick, {}),
    "checkout of thousands of files": (checkout_of_thousands, {}),
    "case-only rename": (case_only_rename, {}),
    "worktree deleted and recreated": (worktree_recreated, {}),
    "worktrees whose libraries were built apart": (worktrees_built_apart, {}),
    "worktrees built by other code": (worktrees_that_run_other_code, {}),
    "names and text that are not UTF-8": (non_utf8_names, {}),
    "workspace under a name that is not UTF-8": (non_utf8_root, {}),
    "store on another filesystem": (other_filesystem, {}),
    "toolchain file switched": (toolchain_switched, {}),
    "two toolchains in turn": (toolchains_in_turn, {}),
    "daemon binary replaced": (binary_replaced, {}),
    "memory ceiling": (memory_ceiling, {"RUSTD_MEMORY_MAX": "1M"}),
    "no memory ceiling": (no_ceiling, {"RUSTD_MEMORY_MAX": "off"}),
    "tests of a proc macro": (proc_macro_tests, {}),
    "a function that is another under test": (another_under_test, {}),
    "functions rustc keeps to an object": (kept_to_an_object, {"CARGO_INCREMENTAL": "0"}),
}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("rows", nargs="*")
    parser.add_argument("--work")
    parser.add_argument("--other-fs", default="/dev/shm")
    parser.add_argument("--keep", action="store_true")
    args = parser.parse_args()
    if not shutil.which("cargo-rustd", path=os.path.dirname(RUSTD)):
        sys.exit("no cargo-rustd beside the rustd of RUSTD_BIN")
    unknown = [wanted for wanted in args.rows if not any(wanted in name for name in ROWS)]
    if unknown:
        sys.exit(f"no row is named like {unknown}; the rows are: {', '.join(ROWS)}")
    work = args.work or tempfile.mkdtemp(prefix="rustd-soakrows-")
    table, failed = [], 0
    for name, (run, env) in ROWS.items():
        if args.rows and not any(wanted in name for wanted in args.rows):
            continue
        slug = re.sub(r"\W+", "-", name)
        at = None
        if run is other_filesystem:
            at = os.path.join(tempfile.mkdtemp(prefix="rustd-soakrows-", dir=args.other_fs), "ws")
        row, began = Row(work, slug, env, at), time.time()
        try:
            run(row)
            found = row.panics()
            if found:
                raise Wrong(f"{len(found)} panics logged: {found[0][:300]}")
            result = "right"
        except Wrong as wrong:
            result, failed = f"WRONG: {wrong}", failed + 1
        except (subprocess.SubprocessError, OSError) as error:
            result, failed = f"WRONG: the row could not go on: {error}", failed + 1
        finally:
            row.stop()
            if at:
                shutil.rmtree(os.path.dirname(at), ignore_errors=True)
        table.append((name, result, row.built, row.notes))
        print(f"{'ok ' if result == 'right' else 'NOT'} {name}  {time.time() - began:.0f}s  {result if result != 'right' else ''}", flush=True)
        for line in row.built:
            print(f"      {line}", flush=True)
    backend = os.environ.get("RUSTD_BACKEND", "cranelift")
    with open(os.path.join(HERE, "SOAKROWS.md"), "w") as out:
        out.write(f"# soakrows.py\n\n{len(table) - failed} of {len(table)} rows right, {backend}.\n\n| row | result | who built each comparison |\n|---|---|---|\n")
        for name, result, built, _ in table:
            out.write(f"| {name} | {result.replace('|', '/')[:400]} | {'; '.join(built).replace('|', '/')} |\n")
        for name, _, _, notes in table:
            if notes:
                out.write(f"\n## {name}\n\n```\n" + "\n".join(notes) + "\n```\n")
    print(f"{len(table) - failed} of {len(table)} rows right")
    if not args.keep and not failed and not args.work:
        shutil.rmtree(work, ignore_errors=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
