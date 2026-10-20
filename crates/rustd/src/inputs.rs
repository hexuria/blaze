//! What a checkout was loaded from besides the source: its manifests and lock file, cargo's
//! configuration, the flags for rustc in the environment, and what its build scripts read.
//! Before each build the daemon looks whether they are still what it loaded, runs the build
//! scripts again where cargo would, and says which checkouts are to be loaded again.

use std::{
    hash::{Hash as _, Hasher as _},
    path::{Path, PathBuf},
};

use ide_db::base_db::Env;
use project_model::{CargoConfig, ProjectWorkspace, ProjectWorkspaceKind, TargetKind};
use rustc_hash::{FxHashMap, FxHasher};

/// The variables cargo takes flags for rustc from.
const FLAGS: &[&str] = &["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_RUSTFLAGS"];

/// What each of the daemon's checkouts that is loaded in full was loaded from.
#[derive(Default)]
pub(crate) struct Inputs {
    checkouts: Vec<Checkout>,
}

/// What the workspace at `root` was loaded from.
struct Checkout {
    root: PathBuf,
    /// The manifests and the lock file, the build scripts cargo finds by their name, and
    /// the files cargo's configuration is read from: those that are not there too.
    files: Vec<Watched>,
    /// The variables of `FLAGS` as cargo was run with them.
    flags: Vec<(&'static str, Option<String>)>,
    scripts: Option<Scripts>,
}

/// A file or a directory, and what it was when the daemon read it.
struct Watched {
    path: PathBuf,
    /// Whether the directory is a package, of which cargo leaves out what a build wrote and
    /// the packages inside.
    package: bool,
    stamp: u64,
}

impl Watched {
    fn new(path: PathBuf, package: bool) -> Watched {
        let stamp = stamp(&path, package);
        Watched { path, package, stamp }
    }

    fn changed(&self) -> bool {
        stamp(&self.path, self.package) != self.stamp
    }
}

/// The build scripts of one workspace and what they said.
struct Scripts {
    workspace: ProjectWorkspace,
    config: CargoConfig,
    /// What cargo runs a script again for a change of.
    files: Vec<Watched>,
    /// The variables it runs one again for a change of, as they were when they last ran.
    variables: Vec<(String, Option<String>)>,
}

/// The variable `name` as the commands of `config` are run with it.
fn run_with(config: &CargoConfig, name: &str) -> Option<String> {
    match config.extra_env.get(name) {
        Some(value) => value.clone(),
        None => std::env::var(name).ok(),
    }
}

impl Inputs {
    /// Has the workspace at `root`, loaded with `config`, be one whose inputs are watched.
    pub(crate) fn watch(
        &mut self,
        root: &Path,
        workspace: &ProjectWorkspace,
        config: &CargoConfig,
    ) {
        let mut files = Vec::new();
        for dir in root.ancestors() {
            for name in ["config.toml", "config"] {
                files.push(Watched::new(dir.join(".cargo").join(name), false));
            }
        }
        for name in ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "rust-toolchain"] {
            files.push(Watched::new(root.join(name), false));
        }
        if let ProjectWorkspaceKind::Cargo { cargo, .. } = &workspace.kind {
            for package in cargo.packages().filter(|&it| cargo[it].is_local) {
                let manifest: &Path = cargo[package].manifest.as_ref();
                files.push(Watched::new(manifest.to_owned(), false));
                // A script that appears there is run without the manifest naming it.
                files.push(Watched::new(manifest.with_file_name("build.rs"), false));
            }
        }
        let flags = FLAGS.iter().map(|&it| (it, run_with(config, it))).collect();
        let scripts = Scripts::of(workspace, config);
        self.checkouts.retain(|it| it.root != root);
        self.checkouts.push(Checkout { root: root.to_owned(), files, flags, scripts });
    }

    /// Whether the checkout at `root` is no longer what it was loaded from. It is asked for
    /// a process with the variables `env`, or with `None` for whoever it was loaded for.
    /// An error is why its build scripts can't say.
    pub(crate) fn stale(&mut self, root: &Path, env: Option<&Env>) -> anyhow::Result<bool> {
        let Some(checkout) = self.checkouts.iter_mut().find(|it| it.root == root) else {
            return Ok(false);
        };
        if checkout.files.iter().any(Watched::changed) {
            return Ok(true);
        }
        if let Some(env) = env
            && checkout.flags.iter().any(|(name, was)| env.get(name) != *was)
        {
            return Ok(true);
        }
        match &mut checkout.scripts {
            Some(scripts) => scripts.changed(env),
            None => Ok(false),
        }
    }

    /// The variables cargo and the build scripts of the checkout at `root` read, as a
    /// process with `env` has them.
    pub(crate) fn variables(&self, root: &Path, env: &Env) -> FxHashMap<String, Option<String>> {
        let scripts = self.checkouts.iter().find(|it| it.root == root);
        let scripts = scripts.and_then(|it| it.scripts.as_ref());
        let read = scripts.into_iter().flat_map(|it| it.variables.iter().map(|it| &*it.0));
        FLAGS.iter().copied().chain(read).map(|it| (it.to_owned(), env.get(it))).collect()
    }
}

impl Scripts {
    fn of(workspace: &ProjectWorkspace, config: &CargoConfig) -> Option<Scripts> {
        let ProjectWorkspaceKind::Cargo { cargo, build_scripts, .. } = &workspace.kind else {
            return None;
        };
        let (mut files, mut variables) = (Vec::new(), Vec::new());
        for package in cargo.packages() {
            let Some(out_dir) = build_scripts.out_dir(package) else { continue };
            let root: &Path = cargo[package].manifest.parent().as_ref();
            let said = Said::read(out_dir.as_ref());
            variables.extend(said.variables.into_iter().map(|it| {
                let value = run_with(config, &it);
                (it, value)
            }));
            if !cargo[package].is_local {
                continue;
            }
            let script = cargo[package].targets.iter().map(|&it| &cargo[it]);
            let script = script.filter(|it| it.kind == TargetKind::BuildScript);
            files.extend(script.map(|it| Watched::new(PathBuf::from(it.root.as_str()), false)));
            match said.watches {
                // A script that names nothing is run again when any file of its package changes.
                false => files.push(Watched::new(root.to_owned(), true)),
                true => files
                    .extend(said.files.into_iter().map(|it| Watched::new(root.join(it), false))),
            }
        }
        variables.sort();
        variables.dedup();
        if files.is_empty() && variables.is_empty() {
            return None;
        }
        Some(Scripts { workspace: workspace.clone(), config: config.clone(), files, variables })
    }

    /// Runs the scripts again where cargo would for a process with `env`, or with `None`
    /// for one with the variables they last ran with: whether what they say is no longer
    /// what was loaded.
    fn changed(&mut self, env: Option<&Env>) -> anyhow::Result<bool> {
        let files = self.files.iter().any(Watched::changed);
        let variables =
            env.is_some_and(|env| self.variables.iter().any(|(name, was)| env.get(name) != *was));
        if !files && !variables {
            return Ok(false);
        }
        let ProjectWorkspaceKind::Cargo { build_scripts: loaded, .. } = &self.workspace.kind else {
            return Ok(false);
        };
        let mut config = self.config.clone();
        for (name, was) in &self.variables {
            let value = env.map_or_else(|| was.clone(), |env| env.get(name));
            config.extra_env.insert(name.clone(), value);
        }
        let said = self.workspace.run_build_scripts(&config, &|_| {})?;
        if let Some(error) = said.error() {
            anyhow::bail!("a build script failed: {}", error.trim().replace('\n', " "));
        }
        if said != *loaded {
            return Ok(true);
        }
        for file in &mut self.files {
            file.stamp = stamp(&file.path, file.package);
        }
        for (name, was) in &mut self.variables {
            *was = config.extra_env.get(name).cloned().flatten();
        }
        self.config = config;
        Ok(false)
    }
}

/// What a build script asked cargo to watch.
#[derive(Default)]
struct Said {
    /// Whether it named anything: files or variables.
    watches: bool,
    files: Vec<String>,
    variables: Vec<String>,
}

impl Said {
    /// Reads what the script that wrote to `out_dir` printed: cargo keeps it next to it.
    fn read(out_dir: &Path) -> Said {
        let mut said = Said::default();
        let output = out_dir.parent().map(|it| it.join("output"));
        let Some(Ok(output)) = output.map(std::fs::read_to_string) else { return said };
        for line in output.lines() {
            let Some(line) = line.strip_prefix("cargo:") else { continue };
            let line = line.strip_prefix(':').unwrap_or(line);
            if let Some(file) = line.strip_prefix("rerun-if-changed=") {
                said.watches = true;
                said.files.push(file.to_owned());
            } else if let Some(variable) = line.strip_prefix("rerun-if-env-changed=") {
                said.watches = true;
                said.variables.push(variable.to_owned());
            }
        }
        said
    }
}

/// A number that changes when the file at `path`, or a file below the directory there, is
/// written, added or removed.
fn stamp(path: &Path, package: bool) -> u64 {
    let mut hasher = FxHasher::default();
    stamp_into(path, package, &mut hasher);
    hasher.finish()
}

fn stamp_into(path: &Path, package: bool, hasher: &mut FxHasher) {
    let Ok(metadata) = std::fs::metadata(path) else { return };
    if !metadata.is_dir() {
        (metadata.len(), metadata.modified().ok()).hash(hasher);
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else { return };
    let mut entries: Vec<PathBuf> = entries.filter_map(|it| Some(it.ok()?.path())).collect();
    entries.sort();
    for entry in entries {
        let built = entry.join("CACHEDIR.TAG").exists() || entry.ends_with(".git");
        if package && (built || entry.join("Cargo.toml").exists()) {
            continue;
        }
        entry.hash(hasher);
        stamp_into(&entry, package, hasher);
    }
}
