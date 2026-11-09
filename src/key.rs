//! Canonical snapshot of the inputs selected for cache comparison.
//!
//! The snapshot records the command line, repository-relative working directory,
//! declared environment, lockfiles, probed tools, executable and selected files.
//! It does not capture all possible dependencies, such as network responses,
//! undeclared environment variables, external files or child executables.
//!
//! Two rules drive the whole module:
//!
//! * the key holds no absolute path of the machine, so the same repository
//!   gives the same key whatever the directory it lives in;
//! * a file that cannot be read with certainty is never silently left out.
//!   An unreadable, ambiguous or moving file fails the capture, so the
//!   command runs and nothing is cached.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use blake3::Hasher;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use crate::Error;
use crate::fingerprint::{self, Mode};
use crate::tools::{self, Tool};

/// Version of the layout of the key material. Bumping it invalidates every
/// cached entry.
pub const KEY_SCHEMA_VERSION: u32 = 1;

/// Environment variables captured for every command.
pub const DEFAULT_ENV_VARS: &[&str] = &["PATH", "NODE_ENV", "RUSTFLAGS", "CI"];

/// Prefix of the environment variables captured for every command.
pub const CARGO_ENV_PREFIX: &str = "CARGO_";

/// Environment variable holding the cache directory. The cache directory is
/// left out of the captured files, so that the cache cannot change the key it
/// is stored under.
pub const STORE_DIR_ENV: &str = "KIOKU_DIR";

/// Lockfiles captured for every command, wherever they are in the repository.
/// They are captured even when the inputs are restricted.
pub const LOCKFILES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lockb",
    "poetry.lock",
    "uv.lock",
    "go.sum",
];

/// Kind of a captured file.
const KIND_FILE: u32 = 0;
/// Kind of a captured directory.
const KIND_DIRECTORY: u32 = 1;
/// Kind of a captured symlink.
const KIND_SYMLINK: u32 = 2;
/// Kind of a tracked file that is not in the working tree.
const KIND_MISSING: u32 = 3;

/// What to capture, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureOptions {
    /// Working directory of the command, inside the repository.
    pub cwd: PathBuf,
    /// Exact command line, `argv[0]` being the program.
    pub argv: Vec<String>,
    /// Fingerprint mode.
    pub mode: Mode,
    /// Globs restricting the captured files. An empty list captures every file.
    pub inputs: Vec<String>,
    /// Environment variables captured in addition to the default ones.
    pub env: Vec<String>,
}

/// State of one captured file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputRecord {
    /// BLAKE3 digest of the captured state.
    pub digest: String,
    /// Normalized mode: the kind of the file and its permission bits.
    pub mode: u32,
}

/// Everything a command result depends on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// BLAKE3 key of the snapshot.
    pub key: String,
    /// Version of kioku that took the snapshot.
    pub version: String,
    /// Exact command line, `argv[0]` being the program.
    pub argv: Vec<String>,
    /// Working directory, relative to the root of the repository.
    pub cwd: String,
    /// Fingerprint mode.
    pub mode: Mode,
    /// Captured environment variables. A variable that is not set is `None`,
    /// which is not the same as a variable set to an empty value.
    pub env: BTreeMap<String, Option<String>>,
    /// Lockfiles of the repository, with the digest of their content.
    pub lockfiles: BTreeMap<String, String>,
    /// Versions of the tools the repository depends on.
    pub tools: BTreeMap<String, String>,
    /// Captured files, with the digest of their state and their mode.
    pub inputs: BTreeMap<String, InputRecord>,
    /// Digest of the raw metadata of the captured files.
    ///
    /// The guard spots a file that changed while the command was running. It
    /// holds timestamps, so it is not part of the key: it is only meant to be
    /// compared with the guard of another snapshot of the same machine.
    pub guard: String,
}

impl Snapshot {
    /// Captures the state a command depends on.
    ///
    /// # Errors
    ///
    /// Returns an error when the repository cannot be read with certainty:
    /// when `git` fails, when the working directory is outside the repository,
    /// when a captured file cannot be read, when a captured file changes while
    /// it is read, or when a path or an environment value cannot be
    /// represented without ambiguity.
    pub fn capture(options: &CaptureOptions) -> Result<Self, Error> {
        let matcher = InputMatcher::new(&options.inputs)?;
        let (root, git_dir) = repository(&options.cwd)?;
        let cwd = relative_cwd(&root, &options.cwd)?;
        // The store must not live in the work tree: the key would leave out
        // the files of the store, and a tracked file that happens to be there
        // would be left out of the key too, so a change of it would not change
        // the key. A store in the work tree turns the cache off.
        let store = crate::store::store_dir(&options.cwd, &git_dir);
        if store_inside_work_tree(&store, &root, &git_dir) {
            return Err(Error::StoreInsideWorkTree {
                path: store.to_string_lossy().into_owned(),
            });
        }
        let listing = list_files(&root)?;
        let (lockfiles, lockfile_meta) = capture_lockfiles(&root, &listing)?;
        let (inputs, input_meta) = capture_inputs(&root, &listing, &matcher, options.mode)?;
        let env = capture_env(options)?;
        let detected = tools::detect(&options.cwd, &lockfiles)?;
        let mut versions: BTreeMap<String, String> = detected
            .iter()
            .map(|tool| (tool.name.clone(), tool.version.clone()))
            .collect();
        versions.insert("command-executable".to_string(), command_identity(options)?);
        let key = compute_key(options, &cwd, &env, &lockfiles, &versions, &inputs)?;
        let mut guard_files = input_meta;
        guard_files.extend(lockfile_meta);
        let guard = compute_guard(&guard_files, &detected);
        Ok(Self {
            key,
            version: env!("CARGO_PKG_VERSION").to_string(),
            argv: options.argv.clone(),
            cwd,
            mode: options.mode,
            env,
            lockfiles,
            tools: versions,
            inputs,
            guard,
        })
    }
}

/// Returns the root of the repository and the absolute path of its git
/// directory.
///
/// The git directory is the real one, so a linked worktree is supported: the
/// root is the worktree, and the git directory is the one of the worktree.
///
/// # Errors
///
/// Returns an error when `git` cannot be run, when it fails, or when the
/// directory is not inside a repository.
pub fn repository(cwd: &Path) -> Result<(PathBuf, PathBuf), Error> {
    let root = match git_output(cwd, &["rev-parse", "--show-toplevel"]) {
        Ok(root) => root,
        Err(Error::Git { stderr, .. }) if is_not_a_repository(&stderr) => {
            return Err(Error::NotARepository {
                path: cwd.to_path_buf(),
            });
        }
        Err(error) => return Err(error),
    };
    let git_dir = git_output(cwd, &["rev-parse", "--absolute-git-dir"])?;
    let root = PathBuf::from(String::from_utf8_lossy(&root).trim().to_string());
    let git_dir = PathBuf::from(String::from_utf8_lossy(&git_dir).trim().to_string());
    if !root.is_absolute() {
        return Err(Error::NotARepository {
            path: cwd.to_path_buf(),
        });
    }
    Ok((root, git_dir))
}

/// Tells whether a store lives in the work tree.
///
/// The store of a run may not live in the work tree: the key leaves out the
/// files of the store, and a tracked file that happens to be there would be
/// left out of the key too, so a change of it would not change the key and a
/// success could be served for a failure. The cache is off in that case: no
/// trace is learned, no trace is reused, and the store is not created. The
/// default store lives in the git directory, which git never lists, so it is
/// safe.
pub(crate) fn store_inside_work_tree(store: &Path, root: &Path, git_dir: &Path) -> bool {
    store.starts_with(root) && !store.starts_with(git_dir)
}

/// Working directory relative to the root of the repository.
fn relative_cwd(root: &Path, cwd: &Path) -> Result<String, Error> {
    let cwd = fs::canonicalize(cwd).map_err(|source| Error::Io {
        path: cwd.to_path_buf(),
        source,
    })?;
    let relative = cwd
        .strip_prefix(root)
        .map_err(|_| Error::OutsideRepository {
            cwd: cwd.clone(),
            root: root.to_path_buf(),
        })?;
    let mut parts = Vec::new();
    for component in relative.components() {
        let part = component
            .as_os_str()
            .to_str()
            .ok_or_else(|| Error::NonUtf8Path {
                path: relative.to_string_lossy().into_owned(),
            })?;
        parts.push(part);
    }
    if parts.is_empty() {
        Ok(".".to_string())
    } else {
        Ok(parts.join("/"))
    }
}

/// Files of the repository, as `git` sees them.
struct Listing {
    /// Files tracked by git, including the ones removed from the working tree.
    tracked: Vec<String>,
    /// Files git does not track and does not ignore.
    untracked: Vec<String>,
    /// Every file of the repository, sorted.
    all: Vec<String>,
}

/// Lists the files of the repository.
fn list_files(root: &Path) -> Result<Listing, Error> {
    let tracked = git_paths(root, &["ls-files", "-z", "--cached", "--full-name"])?;
    let untracked = git_paths(
        root,
        &[
            "ls-files",
            "-z",
            "--others",
            "--exclude-standard",
            "--full-name",
        ],
    )?;
    let mut all = tracked.clone();
    all.extend(untracked.iter().cloned());
    all.sort();
    all.dedup();
    Ok(Listing {
        tracked,
        untracked,
        all,
    })
}

/// Runs `git` and splits its output on null bytes.
fn git_paths(root: &Path, args: &[&str]) -> Result<Vec<String>, Error> {
    let output = git_output(root, args)?;
    let mut paths = Vec::new();
    for raw in output.split(|byte| *byte == 0) {
        if raw.is_empty() {
            continue;
        }
        let path = std::str::from_utf8(raw).map_err(|_| Error::NonUtf8Path {
            path: String::from_utf8_lossy(raw).into_owned(),
        })?;
        paths.push(path.to_string());
    }
    Ok(paths)
}

/// Whether `git` reported that the directory is outside any work tree.
fn is_not_a_repository(stderr: &str) -> bool {
    stderr.contains("not a git repository")
        || stderr.contains("not a git work tree")
        || stderr.contains("outside repository")
}

/// Runs `git` in a directory and returns its standard output.
fn git_output(root: &Path, args: &[&str]) -> Result<Vec<u8>, Error> {
    let command = args.join(" ");
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                Error::GitUnavailable { source }
            } else {
                Error::Git {
                    command: command.clone(),
                    stderr: source.to_string(),
                }
            }
        })?;
    if !output.status.success() {
        return Err(Error::Git {
            command,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(output.stdout)
}

/// Globs restricting the captured files.
struct InputMatcher {
    set: Option<GlobSet>,
}

impl InputMatcher {
    /// Builds the matcher. No pattern means every file is captured.
    fn new(patterns: &[String]) -> Result<Self, Error> {
        if patterns.is_empty() {
            return Ok(Self { set: None });
        }
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            let glob = GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .map_err(|source| Error::InvalidGlob {
                    pattern: pattern.clone(),
                    message: source.to_string(),
                })?;
            builder.add(glob);
        }
        let set = builder.build().map_err(|source| Error::InvalidGlob {
            pattern: patterns.join(", "),
            message: source.to_string(),
        })?;
        Ok(Self { set: Some(set) })
    }

    /// Tells whether a repository path is captured.
    fn matches(&self, path: &str) -> bool {
        match &self.set {
            None => true,
            Some(set) => set.is_match(path),
        }
    }
}

/// Captures the files the command is allowed to read.
fn capture_inputs(
    root: &Path,
    listing: &Listing,
    matcher: &InputMatcher,
    mode: Mode,
) -> Result<(BTreeMap<String, InputRecord>, Vec<MetaRecord>), Error> {
    let mut inputs = BTreeMap::new();
    let mut meta = Vec::new();
    let files = listing
        .tracked
        .iter()
        .map(|path| (path, true))
        .chain(listing.untracked.iter().map(|path| (path, false)));
    for (path, tracked) in files {
        if !matcher.matches(path) {
            continue;
        }
        let (record, meta_record) = capture_path(root, path, tracked, mode)?;
        inputs.insert(path.clone(), record);
        meta.push(meta_record);
    }
    Ok((inputs, meta))
}

/// Captures the lockfiles of the repository, wherever they are.
fn capture_lockfiles(
    root: &Path,
    listing: &Listing,
) -> Result<(BTreeMap<String, String>, Vec<MetaRecord>), Error> {
    let mut lockfiles = BTreeMap::new();
    let mut meta = Vec::new();
    for path in &listing.all {
        let name = path.rsplit('/').next().unwrap_or(path.as_str());
        if !LOCKFILES.contains(&name) {
            continue;
        }
        // A lockfile is not source code: its digest stays in the bytes
        // domain, whatever the mode of the command.
        let (record, meta_record) = capture_path(root, path, true, Mode::Bytes)?;
        lockfiles.insert(path.clone(), record.digest);
        meta.push(meta_record);
    }
    Ok((lockfiles, meta))
}

/// Raw metadata of a captured file.
struct MetaRecord {
    /// Path of the file, relative to the root of the repository.
    path: String,
    /// Size of the file.
    len: u64,
    /// Modification time of the file.
    modified: Option<SystemTime>,
    /// Normalized mode of the file.
    mode: u32,
    /// Digest of the raw content of the file.
    ///
    /// The size and the modification time can stay the same while the content
    /// changes, so the guard holds the content itself and not only its
    /// metadata.
    content: String,
}

/// Captures the state of one file.
fn capture_path(
    root: &Path,
    path: &str,
    tracked: bool,
    mode: Mode,
) -> Result<(InputRecord, MetaRecord), Error> {
    let full = root.join(path);
    let meta = match fs::symlink_metadata(&full) {
        Ok(meta) => meta,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            if !tracked {
                // The file was there a moment ago: the tree moved under us.
                return Err(Error::RacyInput {
                    path: path.to_string(),
                });
            }
            // A tracked file removed from the working tree is a state of its
            // own: the cache must miss, but the command must still run.
            let mode = missing_mode();
            return Ok((
                InputRecord {
                    digest: digest_of(&[b"kioku/v1/missing"]),
                    mode,
                },
                MetaRecord {
                    path: path.to_string(),
                    len: 0,
                    modified: None,
                    mode,
                    content: raw_state(&[b"missing"]),
                },
            ));
        }
        Err(source) => {
            return Err(Error::UnreadableInput {
                path: path.to_string(),
                source,
            });
        }
    };
    if meta.is_symlink() {
        return capture_symlink(root, path, &full, &meta, mode);
    }
    if meta.is_dir() {
        return Err(Error::UnreadableInput {
            path: path.to_string(),
            source: std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "directory input cannot be captured",
            ),
        });
    }
    let before = (meta.len(), meta.modified().ok());
    let bytes = read_file(&full, path)?;
    let after = fs::metadata(&full).map_err(|source| Error::UnreadableInput {
        path: path.to_string(),
        source,
    })?;
    if moved(before, (after.len(), after.modified().ok())) {
        return Err(Error::RacyInput {
            path: path.to_string(),
        });
    }
    // The path only tells the grammar to the fingerprint; it is never hashed,
    // so the digest does not depend on where the repository lives.
    let digest = fingerprint::fingerprint(Path::new(path), &bytes, mode);
    let file_mode = mode_of(&meta);
    Ok((
        InputRecord {
            digest,
            mode: file_mode,
        },
        MetaRecord {
            path: path.to_string(),
            len: before.0,
            modified: before.1,
            mode: file_mode,
            content: raw_state(&[b"file", b"\0", raw_digest(&bytes).as_bytes()]),
        },
    ))
}

/// Captures a symlink: its target, and the state of the file it points at.
///
/// The target of a symlink can change on its own, so the state of the file it
/// points at is captured as well. A target that cannot be read fails the
/// capture rather than being left out.
fn capture_symlink(
    root: &Path,
    path: &str,
    full: &Path,
    meta: &fs::Metadata,
    mode: Mode,
) -> Result<(InputRecord, MetaRecord), Error> {
    let target = fs::read_link(full).map_err(|source| Error::UnreadableInput {
        path: path.to_string(),
        source,
    })?;
    let target_text = target.to_str().ok_or_else(|| Error::NonUtf8Path {
        path: target.to_string_lossy().into_owned(),
    })?;
    let referent = full.parent().unwrap_or(root).join(&target);
    let state = match fs::canonicalize(&referent) {
        Ok(resolved) => {
            let resolved_meta =
                fs::metadata(&resolved).map_err(|source| Error::UnreadableInput {
                    path: path.to_string(),
                    source,
                })?;
            if resolved_meta.is_file() {
                let bytes = read_file(&resolved, path)?;
                let digest = fingerprint::fingerprint(&resolved, &bytes, mode);
                let raw = raw_digest(&bytes);
                format!("file\0{digest}\0{raw}\0{}", mode_of(&resolved_meta))
            } else {
                return Err(Error::UnsupportedSymlink {
                    path: path.to_string(),
                });
            }
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => "dangling".to_string(),
        Err(source) => {
            return Err(Error::UnreadableInput {
                path: path.to_string(),
                source,
            });
        }
    };
    let digest = digest_of(&[
        b"kioku/v1/symlink",
        b"\0",
        target_text.as_bytes(),
        b"\0",
        state.as_bytes(),
    ]);
    let mode = mode_of(meta);
    Ok((
        InputRecord { digest, mode },
        MetaRecord {
            path: path.to_string(),
            len: meta.len(),
            modified: meta.modified().ok(),
            mode,
            content: raw_state(&[
                b"symlink",
                b"\0",
                target_text.as_bytes(),
                b"\0",
                state.as_bytes(),
            ]),
        },
    ))
}

/// Tells whether a file changed between two reads.
///
/// The size and the modification time are compared: a file that changes
/// while it is read cannot be captured with certainty.
fn moved(before: (u64, Option<SystemTime>), after: (u64, Option<SystemTime>)) -> bool {
    after.0 != before.0 || after.1 != before.1
}

/// Content of a file.
fn read_file(full: &Path, path: &str) -> Result<Vec<u8>, Error> {
    fs::read(full).map_err(|source| Error::UnreadableInput {
        path: path.to_string(),
        source,
    })
}

/// Digest of the raw content of a file.
fn raw_digest(bytes: &[u8]) -> String {
    let mut hasher = Hasher::new();
    hasher.update(b"kioku/v1/raw\0");
    hasher.update(bytes);
    hasher.finalize().to_hex().to_string()
}

/// Digest of the raw state of a file, whatever its kind.
fn raw_state(parts: &[&[u8]]) -> String {
    let mut hasher = Hasher::new();
    hasher.update(b"kioku/v1/raw-state\0");
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().to_hex().to_string()
}

/// Digest of the parts of a state.
fn digest_of(parts: &[&[u8]]) -> String {
    let mut hasher = Hasher::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().to_hex().to_string()
}

/// Normalized mode of a file: its kind and its permission bits.
fn mode_of(meta: &fs::Metadata) -> u32 {
    let kind = if meta.is_symlink() {
        KIND_SYMLINK
    } else if meta.is_dir() {
        KIND_DIRECTORY
    } else {
        KIND_FILE
    };
    #[cfg(unix)]
    let permissions = {
        use std::os::unix::fs::MetadataExt;
        meta.mode() & 0o777
    };
    #[cfg(not(unix))]
    let permissions = if meta.permissions().readonly() {
        0o444
    } else {
        0o644
    };
    (kind << 12) | permissions
}

/// Mode of a tracked file that is not in the working tree.
fn missing_mode() -> u32 {
    KIND_MISSING << 12
}

/// Reads the invoked executable without running it.
fn command_identity(options: &CaptureOptions) -> Result<String, Error> {
    if cfg!(windows) {
        return Err(Error::ToolProbe {
            tool: "command-executable".to_string(),
            reason: "Windows executable resolution is not verified".to_string(),
        });
    }
    let Some(program) = options.argv.first() else {
        return Ok("absent".to_string());
    };
    let program = Path::new(program);
    let candidate = if program.components().count() > 1 || program.is_absolute() {
        Some(options.cwd.join(program))
    } else {
        std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|directory| options.cwd.join(directory).join(program))
                .find(|candidate| tools::is_executable(candidate))
        })
    };
    let Some(path) = candidate else {
        return Ok("absent".to_string());
    };
    let meta = fs::metadata(&path).map_err(|source| Error::Io {
        path: path.clone(),
        source,
    })?;
    let bytes = fs::read(&path).map_err(|source| Error::Io {
        path: path.clone(),
        source,
    })?;
    let after = fs::metadata(&path).map_err(|source| Error::Io {
        path: path.clone(),
        source,
    })?;
    if moved(
        (meta.len(), meta.modified().ok()),
        (after.len(), after.modified().ok()),
    ) {
        return Err(Error::RacyInput {
            path: path.to_string_lossy().into_owned(),
        });
    }
    Ok(format!("{}:{}", mode_of(&meta), raw_digest(&bytes)))
}

/// Captures the environment the command depends on.
fn capture_env(options: &CaptureOptions) -> Result<BTreeMap<String, Option<String>>, Error> {
    let mut names: BTreeSet<String> = DEFAULT_ENV_VARS
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    for (key, _) in std::env::vars_os() {
        let name = key.to_str().ok_or_else(|| Error::NonUtf8Env {
            name: key.to_string_lossy().into_owned(),
        })?;
        if name.starts_with(CARGO_ENV_PREFIX) {
            names.insert(name.to_string());
        }
    }
    for name in &options.env {
        names.insert(name.clone());
    }
    let mut env = BTreeMap::new();
    for name in names {
        let value = match std::env::var_os(&name) {
            None => None,
            Some(raw) => Some(
                raw.into_string()
                    .map_err(|_| Error::NonUtf8Env { name: name.clone() })?,
            ),
        };
        env.insert(name, value);
    }
    Ok(env)
}

/// Byte code of the fingerprint mode, part of the key material.
#[allow(unreachable_patterns, clippy::match_wildcard_for_single_variants)]
fn mode_code(mode: Mode) -> Result<u8, Error> {
    match mode {
        Mode::Ast => Ok(0),
        Mode::Bytes => Ok(1),
        _ => Err(Error::UnsupportedMode),
    }
}

/// Computes the key of a snapshot.
///
/// The material is written with explicit lengths, so two different states can
/// never be written the same way.
fn compute_key(
    options: &CaptureOptions,
    cwd: &str,
    env: &BTreeMap<String, Option<String>>,
    lockfiles: &BTreeMap<String, String>,
    tools: &BTreeMap<String, String>,
    inputs: &BTreeMap<String, InputRecord>,
) -> Result<String, Error> {
    let mut material = Vec::new();
    material.extend_from_slice(b"kioku-key-v1");
    material.extend_from_slice(&KEY_SCHEMA_VERSION.to_be_bytes());
    push_bytes(&mut material, env!("CARGO_PKG_VERSION").as_bytes());
    material.push(mode_code(options.mode)?);
    material.extend_from_slice(&(options.argv.len() as u64).to_be_bytes());
    for argument in &options.argv {
        push_bytes(&mut material, argument.as_bytes());
    }
    push_bytes(&mut material, cwd.as_bytes());
    material.extend_from_slice(&(env.len() as u64).to_be_bytes());
    for (name, value) in env {
        push_bytes(&mut material, name.as_bytes());
        match value {
            None => material.push(0),
            Some(value) => {
                material.push(1);
                push_bytes(&mut material, value.as_bytes());
            }
        }
    }
    material.extend_from_slice(&(lockfiles.len() as u64).to_be_bytes());
    for (path, digest) in lockfiles {
        push_bytes(&mut material, path.as_bytes());
        push_bytes(&mut material, digest.as_bytes());
    }
    material.extend_from_slice(&(tools.len() as u64).to_be_bytes());
    for (name, version) in tools {
        push_bytes(&mut material, name.as_bytes());
        push_bytes(&mut material, version.as_bytes());
    }
    material.extend_from_slice(&(inputs.len() as u64).to_be_bytes());
    for (path, record) in inputs {
        push_bytes(&mut material, path.as_bytes());
        push_bytes(&mut material, record.digest.as_bytes());
        material.extend_from_slice(&record.mode.to_be_bytes());
    }
    Ok(blake3::hash(&material).to_hex().to_string())
}

/// Computes the guard of a snapshot: the digest of the raw metadata.
///
/// The guard holds timestamps, so it is not part of the key. It is meant to be
/// compared with the guard of another snapshot taken on the same machine.
fn compute_guard(files: &[MetaRecord], tools: &[Tool]) -> String {
    let mut material = Vec::new();
    material.extend_from_slice(b"kioku-guard-v1");
    material.extend_from_slice(&(files.len() as u64).to_be_bytes());
    for file in files {
        push_bytes(&mut material, file.path.as_bytes());
        material.extend_from_slice(&file.len.to_be_bytes());
        push_time(&mut material, file.modified);
        material.extend_from_slice(&file.mode.to_be_bytes());
        push_bytes(&mut material, file.content.as_bytes());
    }
    material.extend_from_slice(&(tools.len() as u64).to_be_bytes());
    for tool in tools {
        push_bytes(&mut material, tool.name.as_bytes());
        let path = tool
            .path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        push_bytes(&mut material, path.as_bytes());
        push_time(&mut material, tool.modified);
    }
    blake3::hash(&material).to_hex().to_string()
}

/// Writes a length and its bytes.
fn push_bytes(material: &mut Vec<u8>, bytes: &[u8]) {
    material.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    material.extend_from_slice(bytes);
}

/// Writes a timestamp.
fn push_time(material: &mut Vec<u8>, time: Option<SystemTime>) {
    match time {
        None => material.push(0),
        Some(time) => {
            material.push(1);
            let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or_default();
            material.extend_from_slice(&since_epoch.as_secs().to_be_bytes());
            material.extend_from_slice(&since_epoch.subsec_nanos().to_be_bytes());
        }
    }
}

#[cfg(test)]
#[path = "key_tests.rs"]
mod tests;
