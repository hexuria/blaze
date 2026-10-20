//! rustd as cargo's `RUSTDOC` for `cargo test --doc`: the examples in the documentation of a
//! library are taken out as rustdoc takes them, each is a program of its own that rustc
//! compiles, and the ones that run are linked with rustd's code of the library ahead of
//! rustc's. What they print and how they end is told as rustdoc's test harness tells it.
//!
//! Anything else rustdoc is asked for, and a run of the tests that asks for what is not done
//! here, is rustdoc's: the reason is said and rustdoc runs.

mod extract;
mod program;

use std::{
    ffi::OsString,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
    time::Instant,
};

use anyhow::Context as _;
use ide_db::{RootDatabase, base_db::SourceDatabase};
use syntax::Edition;

use crate::cargo::plan::LinkLine;
use extract::{Example, Marks};
use program::{Frame, Program};

/// The variable that makes `rustd` stand in for rustdoc. Its value is the rustdoc to run for
/// what is left to it.
pub(crate) const ENABLED: &str = "RUSTD_RUSTDOC";

/// The code a test harness ends with when a test failed.
const FAILED: i32 = 101;

/// A run of the examples of one library, as cargo asks rustdoc for it.
pub(crate) struct Asked {
    crate_name: String,
    /// The root file of the library, as cargo names it from the package's directory.
    source: PathBuf,
    edition: String,
    /// Where the examples run.
    run_in: PathBuf,
    /// The arguments rustc compiles every example with.
    rustc_args: Vec<String>,
    target: Option<String>,
    features: Vec<String>,
    wanted: Wanted,
    /// Where the programs are built.
    dir: PathBuf,
}

/// Which examples the arguments after `--` ask for.
#[derive(Default)]
struct Wanted {
    filters: Vec<String>,
    skipped: Vec<String>,
    exact: bool,
}

impl Wanted {
    /// `Err` is the argument that asks for something not done here.
    fn parse(args: &[String]) -> Result<Wanted, String> {
        let mut wanted = Wanted::default();
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--exact" => wanted.exact = true,
                "--skip" => wanted.skipped.extend(args.next().cloned()),
                // The examples run as many at a time as there are processors either way.
                "--test-threads" => _ = args.next(),
                _ if arg.starts_with("--test-threads=") => {}
                _ if arg.starts_with('-') => return Err(arg.clone()),
                _ => wanted.filters.push(arg.clone()),
            }
        }
        Ok(wanted)
    }

    fn has(&self, name: &str) -> bool {
        let is = |it: &String| if self.exact { name == it } else { name.contains(it.as_str()) };
        (self.filters.is_empty() || self.filters.iter().any(is)) && !self.skipped.iter().any(is)
    }
}

/// The flags of rustdoc that take a value and are passed on to rustc for every example.
const TO_RUSTC: [&str; 6] = ["--cfg", "--check-cfg", "-L", "--extern", "-C", "--cap-lints"];

/// The flags of rustdoc that take a value and say nothing of how an example is built. The
/// levels of lints are of the documentation itself: rustdoc compiles no example with them.
const UNUSED: [&str; 12] = [
    "--crate-type",
    "--color",
    "--error-format",
    "--diagnostic-width",
    "--allow",
    "--warn",
    "--deny",
    "--forbid",
    "-A",
    "-W",
    "-D",
    "-F",
];

impl Asked {
    /// Reads rustdoc's arguments. `Err` is why this run is rustdoc's.
    fn read(all: &[OsString]) -> Result<Asked, String> {
        let words: Vec<String> = all
            .iter()
            .map(|it| it.to_str().map(str::to_owned))
            .collect::<Option<_>>()
            .ok_or("an argument is not text")?;
        let cwd = std::env::current_dir().map_err(|it| it.to_string())?;
        let (mut test, mut source, mut crate_name) = (false, None, None);
        let (mut edition, mut run_in, mut target) = ("2015".to_owned(), cwd.clone(), None);
        let (mut rustc_args, mut test_args, mut features) = (Vec::new(), Vec::new(), Vec::new());
        let mut words = words.into_iter();
        while let Some(word) = words.next() {
            // A flag and its value are one word with `=` between them, or two.
            let (flag, joined) = match word.split_once('=') {
                Some((flag, value)) if flag.starts_with("--") => {
                    (flag.to_owned(), Some(value.to_owned()))
                }
                _ => (word.clone(), None),
            };
            let mut value =
                || joined.clone().or_else(|| words.next()).ok_or(format!("{flag} has no value"));
            match flag.as_str() {
                "--test" => test = true,
                "--edition" => edition = value()?,
                "--crate-name" => crate_name = Some(value()?),
                "--test-run-directory" => run_in = PathBuf::from(value()?),
                "--test-args" => {
                    let value = value()?;
                    test_args.extend(
                        shlex::split(&value).ok_or("`--test-args` is not quoted as a shell's")?,
                    );
                }
                "--target" => {
                    let triple = value()?;
                    rustc_args.extend(["--target".to_owned(), triple.clone()]);
                    target = Some(triple);
                }
                _ if TO_RUSTC.contains(&flag.as_str()) => {
                    let value = value()?;
                    if let Some(feature) = feature_of(&flag, &value) {
                        features.push(feature);
                    }
                    rustc_args.extend([flag.clone(), value]);
                }
                _ if UNUSED.contains(&flag.as_str()) => _ = value()?,
                _ if flag.starts_with('-') => return Err(format!("rustdoc is run with `{flag}`")),
                _ if source.is_none() => source = Some(PathBuf::from(word)),
                _ => return Err(format!("rustdoc is given `{word}` too")),
            }
        }
        if !test {
            return Err("rustdoc is not asked to run tests".to_owned());
        }
        let wanted =
            Wanted::parse(&test_args).map_err(|arg| format!("the tests are run with `{arg}`"))?;
        Ok(Asked {
            crate_name: crate_name.ok_or("rustdoc is not told the name of the crate")?,
            source: source.ok_or("rustdoc is given no source file")?,
            edition,
            run_in,
            rustc_args,
            target,
            features,
            wanted,
            dir: std::env::temp_dir().join(format!("rustd-doctest-{}", std::process::id())),
        })
    }

    /// The arguments of a build by rustd of the library the examples are of. `Err` is why
    /// rustd builds none.
    fn build_args(&self) -> anyhow::Result<Result<Vec<String>, String>> {
        std::fs::create_dir_all(&self.dir)?;
        let cwd = std::env::current_dir()?;
        // What a program that uses the library is linked from: every example is linked from
        // that and its own object.
        let probe = format!("extern crate r#{};\nfn main() {{}}\n", self.crate_name);
        let built = self.compile(&probe, &self.dir.join("probe"), &self.edition, &[], true)?;
        if !built.status.success() {
            return Ok(Err("a program that uses the library is not built by rustc".to_owned()));
        }
        let Some(line) = self.link_line(&self.dir.join("probe"))? else {
            return Ok(Err("the link line is not one of an ELF linker driven by `cc`".to_owned()));
        };
        let mut args = vec![
            cwd.display().to_string(),
            "--crate".to_owned(),
            cwd.join(&self.source).display().to_string(),
            "--out".to_owned(),
            self.object().display().to_string(),
            "--dev-deps".to_owned(),
            "--no-default-features".to_owned(),
        ];
        if !self.features.is_empty() {
            args.extend(["--features".to_owned(), self.features.join(",")]);
        }
        if let Some(target) = &self.target {
            args.extend(["--target".to_owned(), target.clone()]);
        }
        for file in line.rustc_files() {
            args.extend(["--symbols".to_owned(), file.display().to_string()]);
        }
        Ok(Ok(args))
    }

    /// The file rustc built of the library, as cargo names it with `--extern`.
    fn library(&self) -> Option<&str> {
        let externs = self.rustc_args.windows(2).filter(|it| it[0] == "--extern");
        externs.filter_map(|it| it[1].split_once('=')).find_map(|(name, file)| {
            (name.rsplit(':').next() == Some(self.crate_name.as_str())).then_some(file)
        })
    }

    /// The object with rustd's code of the library.
    fn object(&self) -> PathBuf {
        self.dir.join("rustd.o")
    }

    /// Has rustc compile `source` into a program in `dir`.
    fn compile(
        &self,
        source: &str,
        dir: &Path,
        edition: &str,
        env: &[(&str, String)],
        links: bool,
    ) -> anyhow::Result<std::process::Output> {
        std::fs::create_dir_all(dir)?;
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let mut rustc = as_asked(rustc);
        rustc.args(&self.rustc_args).args(["--edition", edition, "--color", "never"]);
        rustc.arg("--crate-type=bin").arg("-o").arg(dir.join("rust_out"));
        if links {
            let line = dir.join("link.line");
            rustc.arg(format!("--print=link-args={}", line.display())).arg("-Csave-temps");
        } else {
            rustc.arg("--emit=metadata");
        }
        rustc.arg("-").envs(env.iter().map(|(name, value)| (name, value)));
        rustc.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut rustc = rustc.spawn().context("running rustc")?;
        rustc.stdin.take().context("rustc takes no input")?.write_all(source.as_bytes())?;
        Ok(rustc.wait_with_output()?)
    }

    /// The line rustc linked the program in `dir` with, when it is one rustd links by.
    fn link_line(&self, dir: &Path) -> anyhow::Result<Option<LinkLine>> {
        let line = dir.join("link.line");
        let line = std::fs::read_to_string(&line)
            .with_context(|| format!("reading {}", line.display()))?;
        Ok(LinkLine::parse(&line))
    }

    /// The path of `file` as rustdoc shows it: from the directory it runs in.
    fn shown(&self, file: &Path) -> String {
        let cwd = std::env::current_dir().unwrap_or_default();
        file.strip_prefix(&cwd).unwrap_or(file).display().to_string()
    }
}

/// The feature a `--cfg feature="name"` turns on.
fn feature_of(flag: &str, value: &str) -> Option<String> {
    let name = value.strip_prefix("feature=").filter(|_| flag == "--cfg")?;
    Some(name.trim_matches('"').to_owned())
}

/// A command that runs where rustd was asked and with its environment as it is: what an
/// example reads of either is what it reads under rustdoc.
#[expect(clippy::disallowed_methods, reason = "`toolchain::command` adds to the environment")]
pub(crate) fn as_asked(program: impl AsRef<std::ffi::OsStr>) -> Command {
    Command::new(program)
}

/// Says why and runs rustdoc as it was asked, ending as it does.
pub(crate) fn leave_to_rustdoc(args: &[OsString], why: Option<&str>) -> ! {
    if let Some(why) = why {
        eprintln!("rustd: the doctests are left to rustdoc: {why}");
    }
    let rustdoc = std::env::var_os(ENABLED).filter(|it| !it.is_empty());
    let rustdoc = rustdoc.unwrap_or_else(|| "rustdoc".into());
    let status = as_asked(rustdoc).args(args).status();
    match status {
        Ok(status) => std::process::exit(crate::cargo::run::exit_code(status)),
        Err(error) => {
            eprintln!("error: rustdoc did not run: {error}");
            std::process::exit(1)
        }
    }
}

/// rustd as rustdoc: runs the examples when that is what is asked and rustd can, and has
/// rustdoc do as it was asked otherwise.
pub(crate) fn command(args: impl Iterator<Item = OsString>) -> anyhow::Result<()> {
    let all: Vec<OsString> = args.collect();
    if !all.iter().any(|it| it == "--test") {
        leave_to_rustdoc(&all, None);
    }
    let asked = match Asked::read(&all) {
        Ok(asked) => asked,
        Err(why) => leave_to_rustdoc(&all, Some(&why)),
    };
    let built = asked.build_args().and_then(|args| match args {
        Ok(args) => crate::run(crate::Args::parse(args.into_iter())?, Some(&asked)),
        Err(why) => Err(anyhow::anyhow!(why)),
    });
    _ = std::fs::remove_dir_all(&asked.dir);
    match built {
        Ok(()) => Ok(()),
        Err(why) => leave_to_rustdoc(&all, Some(&format!("{why:#}"))),
    }
}

/// What the crate asks of every example with `#![doc(test(..))]`.
#[derive(Default)]
struct OfTheCrate {
    attrs: Vec<String>,
    no_crate_inject: bool,
}

impl OfTheCrate {
    fn read(db: &RootDatabase, krate: hir::Crate) -> OfTheCrate {
        use syntax::{AstNode, SyntaxKind, ast};
        let root = krate.root_file(db);
        let file = ast::SourceFile::parse(db.file_text(root).text(db), Edition::CURRENT).tree();
        let mut of_the_crate = OfTheCrate::default();
        let trees = file.syntax().descendants().filter(|it| it.kind() == SyntaxKind::TOKEN_TREE);
        // `test(..)` directly inside `doc(..)` of an attribute of the file itself.
        for tree in trees {
            let before = |node: &syntax::SyntaxNode| {
                let mut prev = node.prev_sibling_or_token();
                while prev.as_ref().is_some_and(|it| it.kind().is_trivia()) {
                    prev = prev.and_then(|it| it.prev_sibling_or_token());
                }
                prev.map(|it| it.to_string())
            };
            let outer = tree.parent().filter(|it| it.kind() == SyntaxKind::TOKEN_TREE);
            let in_doc = outer.as_ref().is_some_and(|it| before(it).as_deref() == Some("doc"));
            if before(&tree).as_deref() != Some("test") || !in_doc {
                continue;
            }
            for inner in tree.children().filter(|it| it.kind() == SyntaxKind::TOKEN_TREE) {
                if before(&inner).as_deref() == Some("attr") {
                    let text = inner.text().to_string();
                    of_the_crate.attrs.extend(split_attrs(&text[1..text.len() - 1]));
                }
            }
            let words = tree.children_with_tokens().filter_map(|it| it.into_token());
            of_the_crate.no_crate_inject |=
                words.into_iter().any(|it| it.text() == "no_crate_inject");
        }
        of_the_crate
    }
}

/// The attributes in a list separated by commas, which their own arguments have too.
fn split_attrs(list: &str) -> Vec<String> {
    let (mut attrs, mut depth, mut start) = (Vec::new(), 0, 0);
    for (at, byte) in list.bytes().enumerate() {
        match byte {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b',' if depth == 0 => {
                attrs.push(list[start..at].trim().to_owned());
                start = at + 1;
            }
            _ => {}
        }
    }
    attrs.push(list[start..].trim().to_owned());
    attrs.retain(|it| !it.is_empty());
    attrs
}

/// How an example ended.
enum Ended {
    Passed,
    Ignored,
    /// With what the harness shows of it.
    Failed(String),
}

/// One example ready to build: its name as the harness shows it and its program.
struct Test {
    name: String,
    marks: Marks,
    program: Program,
    /// The path and line rustc is to show for the first line of the example.
    shown_path: String,
    line: usize,
    /// Whether rustdoc builds the example in one program with the others, as it does from
    /// edition 2024: the test harness of that program names it.
    in_one: bool,
}

impl Test {
    fn of(asked: &Asked, example: Example, frame: &Frame<'_>) -> Test {
        let shown_path = asked.shown(&example.file);
        let item =
            if example.item.is_empty() { String::new() } else { format!("{} ", example.item) };
        let name = format!("{shown_path} - {item}(line {})", example.line);
        let id: String = format!("{shown_path}_{}_0", example.line)
            .chars()
            .map(|it| if it.is_ascii_alphanumeric() { it } else { '_' })
            .collect();
        let edition = example.marks.edition.as_deref().unwrap_or(&asked.edition);
        let edition = edition.parse().unwrap_or(Edition::CURRENT);
        let program = Program::of(&example.code, frame, &id, edition);
        let in_one = edition.at_least_2024();
        Test { name, marks: example.marks, program, shown_path, line: example.line, in_one }
    }

    /// The name with what the harness adds for how the example is tested.
    fn shown_name(&self) -> String {
        let how = match &self.marks {
            it if it.compile_fail => " - compile fail",
            it if it.no_run => " - compile",
            it if it.should_panic && self.in_one => " - should panic",
            _ => "",
        };
        format!("{}{how}", self.name)
    }

    fn is_ignored(&self, target: &str) -> bool {
        self.marks.ignore || self.marks.ignored_on.iter().any(|it| target.contains(it.as_str()))
    }

    /// Builds the example in `dir` and runs it.
    fn run(&self, asked: &Asked, dir: &Path, ours: &Path, target: &str) -> anyhow::Result<Ended> {
        if self.is_ignored(target) {
            return Ok(Ended::Ignored);
        }
        let marks = &self.marks;
        let edition = marks.edition.as_deref().unwrap_or(&asked.edition);
        let line = self.line as i64 - self.program.lines_before as i64;
        let env = [
            ("UNSTABLE_RUSTDOC_TEST_PATH", self.shown_path.clone()),
            ("UNSTABLE_RUSTDOC_TEST_LINE", line.to_string()),
        ];
        let runs = !marks.no_run && !marks.compile_fail;
        let built = asked.compile(&self.program.source, dir, edition, &env, runs)?;
        let errors = String::from_utf8_lossy(&built.stderr);
        match (built.status.success(), marks.compile_fail) {
            (true, true) => {
                return Ok(Ended::Failed(
                    "Test compiled successfully, but it's marked `compile_fail`.".to_owned(),
                ));
            }
            (false, false) => {
                return Ok(Ended::Failed(format!("{errors}Couldn't compile the test.")));
            }
            (false, true) => {
                let missing: Vec<&String> =
                    marks.error_codes.iter().filter(|it| !errors.contains(it.as_str())).collect();
                return Ok(match missing.is_empty() {
                    true => Ended::Passed,
                    false => Ended::Failed(format!(
                        "{errors}Some expected error codes were not found: {missing:?}"
                    )),
                });
            }
            (true, false) if !runs => return Ok(Ended::Passed),
            (true, false) => {}
        }
        let binary = dir.join("rust_out");
        let line = asked.link_line(dir)?.context("rustc's link line is of another linker")?;
        // An example that does not name the library is not linked with it: rustd's code of
        // the library has no place in it either.
        if asked.library().is_none_or(|it| line.has(it)) {
            let command = line.command(&[ours], &binary, &dir.join("link.args"))?;
            let linked = crate::shell(&command).output().context("running the linker")?;
            anyhow::ensure!(
                linked.status.success(),
                "the link with rustd's code failed: {}",
                String::from_utf8_lossy(&linked.stderr)
            );
        }
        let ran = match as_asked(&binary).current_dir(&asked.run_in).output() {
            Ok(ran) => ran,
            Err(error) => return Ok(Ended::Failed(format!("Couldn't run the test: {error}"))),
        };
        Ok(match (ran.status.success(), marks.should_panic) {
            (true, false) | (false, true) => Ended::Passed,
            (true, true) => Ended::Failed(
                "Test executable succeeded, but it's marked `should_panic`.".to_owned(),
            ),
            (false, false) => Ended::Failed(failure(&ran)),
        })
    }
}

/// What the harness shows of an example that ran and failed.
fn failure(ran: &std::process::Output) -> String {
    let (stdout, stderr) =
        (String::from_utf8_lossy(&ran.stdout), String::from_utf8_lossy(&ran.stderr));
    let mut shown = format!("Test executable failed ({}).", ran.status);
    if !stdout.is_empty() || !stderr.is_empty() {
        shown.push('\n');
        if !stdout.is_empty() {
            shown.push_str(&format!("\nstdout:\n{stdout}"));
        }
        if !stderr.is_empty() {
            shown.push_str(&format!("\nstderr:\n{stderr}"));
        }
    }
    shown
}

/// Has `krate` be read as rustdoc reads a crate whose examples it tests, with `cfg(doc)` and
/// `cfg(doctest)`: documentation that is only there for them has examples too. The code of
/// the crate is compiled before, as rustc compiles it.
pub(crate) fn document(db: &mut RootDatabase, krate: hir::Crate) {
    use ide_db::base_db::salsa::Setter as _;
    let krate: ide_db::base_db::Crate = krate.into();
    let mut cfg = krate.cfg_options(db).clone();
    cfg.insert_atom(intern::Symbol::intern("doc"));
    cfg.insert_atom(intern::Symbol::intern("doctest"));
    krate.set_cfg_options(db).to(cfg);
}

/// The examples of the library, each as the program rustdoc makes of it, and how many the
/// arguments leave out.
fn tests(db: &RootDatabase, asked: &Asked, krate: hir::Crate) -> (Vec<Test>, usize) {
    let of_the_crate = OfTheCrate::read(db, krate);
    let frame = Frame {
        crate_name: &asked.crate_name,
        attrs: &of_the_crate.attrs,
        no_crate_inject: of_the_crate.no_crate_inject,
    };
    let mut tests: Vec<Test> =
        extract::examples(db, krate).into_iter().map(|it| Test::of(asked, it, &frame)).collect();
    let all = tests.len();
    tests.retain(|it| asked.wanted.has(&it.name));
    tests.sort_by(|a, b| a.name.cmp(&b.name));
    let left_out = all - tests.len();
    (tests, left_out)
}

/// Runs the examples of `krate` for `target`, each linked with the object `ours` of rustd's
/// code of the library, and says how they went as the test harness does. The code to end with.
pub(crate) fn run(
    db: &RootDatabase,
    asked: &Asked,
    krate: hir::Crate,
    ours: &Path,
    target: &str,
) -> anyhow::Result<i32> {
    let start = Instant::now();
    let (tests, left_out) = hir::attach_db(db, || tests(db, asked, krate));
    let out = std::io::stdout();
    writeln!(&out, "\nrunning {} test{}", tests.len(), if tests.len() == 1 { "" } else { "s" })?;
    let next = Mutex::new(0);
    let ended: Mutex<Vec<(usize, anyhow::Result<Ended>)>> = Mutex::new(Vec::new());
    let threads = std::thread::available_parallelism().map_or(1, |it| it.get()).min(tests.len());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let at = {
                        let mut next = next.lock().unwrap_or_else(|it| it.into_inner());
                        *next += 1;
                        *next - 1
                    };
                    let Some(test) = tests.get(at) else { break };
                    let dir = asked.dir.join(at.to_string());
                    let result = test.run(asked, &dir, ours, target);
                    _ = std::fs::remove_dir_all(&dir);
                    let word = match &result {
                        Ok(Ended::Passed) => "ok",
                        Ok(Ended::Ignored) => "ignored",
                        Ok(Ended::Failed(_)) | Err(_) => "FAILED",
                    };
                    _ = writeln!(&out, "test {} ... {word}", test.shown_name());
                    ended.lock().unwrap_or_else(|it| it.into_inner()).push((at, result));
                }
            });
        }
    });
    let mut ended = ended.into_inner().unwrap_or_else(|it| it.into_inner());
    ended.sort_by_key(|(at, _)| *at);
    let (mut passed, mut ignored, mut failed) = (0, 0, Vec::new());
    for (at, result) in ended {
        match result {
            Ok(Ended::Passed) => passed += 1,
            Ok(Ended::Ignored) => ignored += 1,
            Ok(Ended::Failed(shown)) => failed.push((tests[at].shown_name(), shown)),
            Err(error) => failed.push((tests[at].shown_name(), format!("rustd: {error:#}"))),
        }
    }
    if !failed.is_empty() {
        writeln!(&out, "\nfailures:\n")?;
        for (name, shown) in &failed {
            writeln!(&out, "---- {name} stdout ----\n{shown}\n")?;
        }
        writeln!(&out, "\nfailures:")?;
        for (name, _) in &failed {
            writeln!(&out, "    {name}")?;
        }
    }
    writeln!(
        &out,
        "\ntest result: {}. {passed} passed; {} failed; {ignored} ignored; 0 measured; {left_out} filtered out; finished in {:.2}s\n",
        if failed.is_empty() { "ok" } else { "FAILED" },
        failed.len(),
        start.elapsed().as_secs_f64(),
    )?;
    _ = std::fs::remove_dir_all(&asked.dir);
    Ok(if failed.is_empty() { 0 } else { FAILED })
}
