//! Opt-in, local cooperative preparation sharing; never a test-result cache.
//!
//! The caller supplies a complete, stable input snapshot and joins **all** writers
//! before returning from preparation. Joining is a caller obligation, not an
//! enforced one: if the producer leaves a writer running, the inventory can still
//! look complete and the sealed tree can change afterwards. That is a violated
//! caller contract, and no storage-level check detects it.
//!
//! SHA-256 detects accidental corruption only. There are no signatures, no
//! authentication and no I1-T trust: a rewritten store or manifest is not
//! distinguished from a legitimate one, and hostile or dishonest callers are
//! outside the contract. No descendant-quiescence proof is provided.
//!
//! A crashed/failed producer permanently freezes its key; use an independent
//! conservative preparation/full run, never adopt its temporary tree. Supported
//! output trees are Unix-oriented; non-Unix behaviours are unverified.
//! See `docs/dev/lots/NUIT-1611-LOT9B.md` for the custody contract.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const SCHEMA: &str = "kioku/cooperative-preparation/v1";

/// Explicit snapshot supplied by the caller, not inferred from a Git commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputSnapshot {
    /// SHA-256 of the full tree: tracked, dirty, staged, untracked and relevant
    /// ignored inputs, with names, bytes, modes and absence/deletion identity.
    pub full_tree_sha256: String,
    /// SHA-256 of the complete remaining input closure (including external
    /// files, build environment, configuration and preparation implementation).
    pub input_closure_sha256: String,
}

/// Exact identity of one cooperative preparation. No fields are defaulted.
/// Values must not contain secrets: digest sensitive environment/options first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheKey {
    /// Stable canonical repository identity; this is not a portable store.
    pub repository: String,
    /// Full Git commit identifier (SHA-1 or SHA-256).
    pub commit: String,
    /// Required full snapshot, including dirty inputs and their closure.
    pub input_snapshot: InputSnapshot,
    /// Every relevant tool and version/configuration identity.
    pub toolchain: BTreeMap<String, String>,
    /// Complete preparation flags, feature set and implementation identity.
    pub preparation_options: BTreeMap<String, String>,
    /// Complete output/store format and policy options.
    pub store_options: BTreeMap<String, String>,
}

/// All errors deny consumption. There is deliberately no automatic retry/reset.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    /// Storage or producer I/O failed; no artifact is accepted.
    #[error("cooperative preparation i/o failed: {0}")]
    Io(#[from] io::Error),
    /// Caller identity is incomplete or malformed.
    #[error("incomplete cooperative key: {0}")]
    InvalidKey(&'static str),
    /// Permanent durable revocation; run an independent conservative preparation.
    #[error("cooperative generation frozen: {0}; independent preparation required")]
    Frozen(&'static str),
    /// Unsupported artifact tree or manifest.
    #[error("invalid cooperative preparation: {0}")]
    Invalid(&'static str),
    /// A private copy cannot be reused after testing has begun.
    #[error("private preparation is after-test and cannot be reused")]
    AfterTest,
}

type Result<T> = std::result::Result<T, CacheError>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Output {
    sha256: String,
    bytes: u64,
    executable: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Body {
    schema: String,
    key: CacheKey,
    phase: String,
    outputs: BTreeMap<String, Output>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    body: Body,
    sha256: String,
}

/// An explicitly opted-in store for mutually trusted local cooperating callers.
#[derive(Debug, Clone)]
pub struct LocalCooperativeStore {
    root: PathBuf,
}

/// A sealed generation. No shared output path is exposed.
#[derive(Debug)]
pub struct PreparedEntry {
    directory: PathBuf,
    identity: String,
    key: CacheKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    BeforeTest,
    AfterTest,
}

/// An exclusive private copy, not a target directory or cached test outcome.
#[derive(Debug)]
pub struct PrivatePreparation {
    entry: PreparedEntry,
    directory: PathBuf,
    body: Body,
    phase: Phase,
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn encode(value: &impl Serialize) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|_| CacheError::Invalid("cannot encode manifest"))
}

fn hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

impl CacheKey {
    fn identity(&self) -> Result<String> {
        if self.repository.is_empty() || !(hex(&self.commit, 40) || hex(&self.commit, 64)) {
            return Err(CacheError::InvalidKey("repository/commit"));
        }
        if !hex(&self.input_snapshot.full_tree_sha256, 64)
            || !hex(&self.input_snapshot.input_closure_sha256, 64)
        {
            return Err(CacheError::InvalidKey(
                "explicit complete snapshot required",
            ));
        }
        for options in [
            &self.toolchain,
            &self.preparation_options,
            &self.store_options,
        ] {
            if options.is_empty() || options.iter().any(|(k, v)| k.is_empty() || v.is_empty()) {
                return Err(CacheError::InvalidKey(
                    "toolchain/preparation/store options",
                ));
            }
        }
        Ok(sha256(&encode(&(SCHEMA, self))?))
    }
}

fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn durable_new(path: &Path, contents: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    if let Some(parent) = path.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

fn ordinary_directory(path: &Path) -> Result<()> {
    if !fs::symlink_metadata(path)?.file_type().is_dir() {
        return Err(CacheError::Invalid("ordinary directory required"));
    }
    Ok(())
}

fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn lock(directory: &Path) -> Result<File> {
    ordinary_directory(directory)?;
    let path = directory.join("lock");
    if exists(&path)? && !fs::symlink_metadata(&path)?.file_type().is_file() {
        return Err(CacheError::Invalid("ordinary lock required"));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    FileExt::lock_exclusive(&file)?;
    Ok(file)
}

fn check_frozen(directory: &Path) -> Result<()> {
    if exists(&directory.join("FROZEN"))? {
        return Err(CacheError::Frozen("durable revocation"));
    }
    Ok(())
}

fn freeze(directory: &Path, reason: &'static str) -> Result<()> {
    let path = directory.join("FROZEN");
    if !exists(&path)? {
        durable_new(&path, reason.as_bytes())?;
    }
    // Even a previous interrupted creation must be synced before returning.
    File::open(path)?.sync_all()?;
    sync_dir(directory)?;
    Err(CacheError::Frozen(reason))
}

fn executable(metadata: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        0
    }
}

fn digest_file(path: &Path) -> Result<Output> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(CacheError::Invalid("only ordinary output files supported"));
    }
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    let mut bytes = 0;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        bytes += u64::try_from(count).map_err(|_| CacheError::Invalid("output size"))?;
    }
    if bytes != metadata.len() {
        return Err(CacheError::Invalid("output changed during read"));
    }
    Ok(Output {
        sha256: format!("{:x}", hasher.finalize()),
        bytes,
        executable: executable(&metadata),
    })
}

fn inventory(root: &Path) -> Result<BTreeMap<String, Output>> {
    fn visit(root: &Path, current: &Path, files: &mut BTreeMap<String, Output>) -> Result<()> {
        ordinary_directory(current)?;
        let mut count = 0;
        for child in fs::read_dir(current)? {
            let path = child?.path();
            count += 1;
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_dir() {
                visit(root, &path, files)?;
            } else {
                let relative = path
                    .strip_prefix(root)
                    .map_err(|_| CacheError::Invalid("output path"))?;
                let name = relative
                    .to_str()
                    .ok_or(CacheError::Invalid("UTF-8 output names required"))?;
                if relative
                    .components()
                    .any(|p| !matches!(p, Component::Normal(_)))
                    || name.contains('\\')
                {
                    return Err(CacheError::Invalid("non-relative output path"));
                }
                files.insert(name.to_owned(), digest_file(&path)?);
            }
        }
        if count == 0 {
            return Err(CacheError::Invalid("empty output directories unsupported"));
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files)?;
    Ok(files)
}

fn readonly_tree(root: &Path) {
    // Best effort only. This is not an access-control or hostile-writer boundary.
    if let Ok(children) = fs::read_dir(root) {
        for child in children.flatten() {
            if let Ok(metadata) = fs::symlink_metadata(child.path()) {
                if metadata.file_type().is_dir() {
                    readonly_tree(&child.path());
                } else if metadata.file_type().is_file() {
                    let mut permissions = metadata.permissions();
                    permissions.set_readonly(true);
                    let _ = fs::set_permissions(child.path(), permissions);
                }
            }
        }
    }
}

fn sync_tree(root: &Path) -> Result<()> {
    for child in fs::read_dir(root)? {
        let path = child?.path();
        if fs::symlink_metadata(&path)?.file_type().is_dir() {
            sync_tree(&path)?;
        } else {
            File::open(path)?.sync_all()?;
        }
    }
    sync_dir(root)
}

fn read_ready(directory: &Path, key: &CacheKey) -> Result<Body> {
    check_frozen(directory)?;
    if !exists(&directory.join("STARTED"))? {
        freeze(directory, "missing generation marker")?;
    }
    if exists(&directory.join("PREPARING"))? {
        freeze(directory, "interrupted producer; no recovery")?;
    }
    let validate = || -> Result<Body> {
        let ready = directory.join("ready");
        ordinary_directory(&ready)?;
        let manifest_path = ready.join("manifest.json");
        if !fs::symlink_metadata(&manifest_path)?.file_type().is_file() {
            return Err(CacheError::Invalid("ordinary manifest required"));
        }
        let manifest: Manifest = serde_json::from_slice(&fs::read(manifest_path)?)
            .map_err(|_| CacheError::Invalid("malformed manifest"))?;
        if manifest.body.schema != SCHEMA
            || manifest.body.phase != "before-test"
            || &manifest.body.key != key
            || manifest.sha256 != sha256(&encode(&manifest.body)?)
            || manifest.body.outputs != inventory(&ready.join("outputs"))?
        {
            return Err(CacheError::Invalid("manifest/output mismatch"));
        }
        Ok(manifest.body)
    };
    if let Ok(body) = validate() {
        Ok(body)
    } else {
        freeze(directory, "invalid sealed preparation")?;
        unreachable!()
    }
}

impl LocalCooperativeStore {
    /// Opt in to an exclusively cooperative local store. No discovery or network.
    ///
    /// # Errors
    /// Returns an error if directory creation, validation or syncing fails.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        fs::create_dir_all(root)?;
        ordinary_directory(root)?;
        sync_dir(root)?;
        if let Some(parent) = root.parent() {
            sync_dir(parent)?;
        }
        Ok(Self {
            root: fs::canonicalize(root)?,
        })
    }

    /// Prepare once across processes with this **exact** complete key.
    /// The closure must finish and join every writer before returning success;
    /// no tests may run in this tree. The process lock spans the whole closure,
    /// manifest seal, fsync, rename and publication. Failure/crash is never retried.
    /// The store and its lock files must not be removed or replaced while in use.
    /// Deleting a whole key directory loses its history and cannot be detected.
    /// The producer must not reenter the store: nesting on this key deadlocks, and
    /// nesting on other keys can deadlock through a cycle across keys (A→B, B→A).
    ///
    /// # Errors
    /// Incomplete identity, I/O, invalid outputs or permanent freeze deny reuse.
    pub fn prepare(
        &self,
        key: &CacheKey,
        producer: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<PreparedEntry> {
        let identity = key.identity()?;
        let directory = self.root.join(&identity);
        // Serialize creation until the generation has a durable identity marker.
        // Existing generations acquire their own lock only after releasing this one.
        let root_lock = lock(&self.root)?;
        let created = match fs::create_dir(&directory) {
            Ok(()) => {
                sync_dir(&self.root)?;
                true
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                ordinary_directory(&directory)?;
                false
            }
            Err(e) => return Err(e.into()),
        };
        let new_lock = if created {
            let guard = lock(&directory)?;
            durable_new(
                &directory.join("STARTED"),
                b"never recreate this generation",
            )?;
            Some(guard)
        } else {
            None
        };
        drop(root_lock);
        let _lock = match new_lock {
            Some(guard) => guard,
            None => lock(&directory)?,
        };
        check_frozen(&directory)?;
        if !created && !exists(&directory.join("STARTED"))? {
            freeze(&directory, "interrupted initialization; no recovery")?;
        }
        if exists(&directory.join("PREPARING"))? {
            freeze(&directory, "interrupted producer; no recovery")?;
        }
        if exists(&directory.join("ready"))? {
            read_ready(&directory, key)?;
        } else {
            if !created {
                freeze(&directory, "generation missing; no recovery")?;
            }
            durable_new(&directory.join("PREPARING"), b"no automatic recovery")?;
            let produce = || -> Result<()> {
                let temp = directory.join("temporary");
                fs::create_dir(&temp)?;
                let outputs = temp.join("outputs");
                fs::create_dir(&outputs)?;
                producer(&outputs)?;
                readonly_tree(&outputs);
                let body = Body {
                    schema: SCHEMA.into(),
                    key: key.clone(),
                    phase: "before-test".into(),
                    outputs: inventory(&outputs)?,
                };
                let manifest = Manifest {
                    sha256: sha256(&encode(&body)?),
                    body,
                };
                durable_new(&temp.join("manifest.json"), &encode(&manifest)?)?;
                sync_tree(&temp)?;
                fs::rename(&temp, directory.join("ready"))?;
                sync_dir(&directory)?;
                fs::remove_file(directory.join("PREPARING"))?;
                sync_dir(&directory)?;
                Ok(())
            };
            if produce().is_err() {
                freeze(&directory, "producer failed; no recovery")?;
            }
            read_ready(&directory, key)?;
        }
        Ok(PreparedEntry {
            directory,
            identity,
            key: key.clone(),
        })
    }
}

impl PreparedEntry {
    /// Digest of the complete supplied key (not a proof of input completeness).
    #[must_use]
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// Durably revoke this generation and all existing private copies on next use.
    /// Waits for any cooperating use already holding the lock to finish. It never
    /// interrupts a running callback, and calling it from a callback reenters the
    /// cache, which can deadlock like any other nested call.
    ///
    /// # Errors
    /// Always returns `Frozen` after durable revocation, or an I/O error.
    pub fn poison(&self) -> Result<()> {
        let _lock = lock(&self.directory)?;
        freeze(&self.directory, "explicit poison")
    }

    /// Copy only sealed, verified files to a new exclusive directory. Never point
    /// this at a live target or use a target shared by another caller. Calling it
    /// from a store callback reenters the lock and can deadlock.
    ///
    /// # Errors
    /// Existing destination, corruption, revocation or I/O deny consumption.
    pub fn private_copy(&self, destination: impl AsRef<Path>) -> Result<PrivatePreparation> {
        let _lock = lock(&self.directory)?;
        let body = read_ready(&self.directory, &self.key)?;
        let directory = destination.as_ref();
        fs::create_dir(directory)?;
        let directory = fs::canonicalize(directory)?;
        let outputs = directory.join("outputs");
        fs::create_dir(&outputs)?;
        for name in body.outputs.keys() {
            let path = outputs.join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(self.directory.join("ready/outputs").join(name), path)?;
        }
        readonly_tree(&outputs);
        if !matches!(inventory(&outputs), Ok(actual) if actual == body.outputs) {
            freeze(&self.directory, "private copy mismatch")?;
        }
        sync_tree(&outputs)?;
        durable_new(
            &directory.join("before-test"),
            b"private copy; no test result",
        )?;
        Ok(PrivatePreparation {
            entry: Self {
                directory: self.directory.clone(),
                identity: self.identity.clone(),
                key: self.key.clone(),
            },
            directory,
            body,
            phase: Phase::BeforeTest,
        })
    }
}

impl PrivatePreparation {
    fn verify(&self) -> Result<()> {
        if self.phase != Phase::BeforeTest || exists(&self.directory.join("after-test"))? {
            return Err(CacheError::AfterTest);
        }
        let body = read_ready(&self.entry.directory, &self.entry.key)?;
        if body != self.body
            || !matches!(inventory(&self.directory.join("outputs")), Ok(outputs) if outputs == self.body.outputs)
        {
            freeze(&self.entry.directory, "private output changed")?;
        }
        Ok(())
    }

    /// Verify every output and durable poison state immediately before a use.
    /// The lock spans the action.
    ///
    /// Do not retain the exposed path, write artifacts, start tests or spawn any
    /// writer in this callback. Do not reenter the cache from it: calling
    /// `prepare`, `private_copy`, `with_before_test_outputs`, `run_tests` or
    /// `poison` on this key or on **any other key** can deadlock, including a
    /// cycle across distinct keys (A waits for B while B waits for A). Key order
    /// is not imposed and such cycles are not detected.
    ///
    /// # Errors
    /// After-test state, corruption, revocation and action errors are propagated.
    pub fn with_before_test_outputs<T>(
        &self,
        action: impl FnOnce(&Path) -> Result<T>,
    ) -> Result<T> {
        let _lock = lock(&self.entry.directory)?;
        self.verify()?;
        action(&self.directory.join("outputs"))
    }

    /// Verify then irreversibly mark this copy after-test **before** the callback
    /// runs (including failed/panicking tests). No verdict is cached or skipped.
    /// Tests needing writes must copy inputs to their own independent target.
    ///
    /// Do not retain the exposed path or leave writers behind, and do not reenter
    /// the cache from this callback: any nested `prepare`, `private_copy`,
    /// `with_before_test_outputs`, `run_tests` or `poison`, on this key or on
    /// another, can deadlock through a cross-key cycle (A→B, B→A). Key ordering
    /// and cycle detection are not provided.
    ///
    /// # Errors
    /// After-test state, corruption, revocation, transition I/O or action failure.
    pub fn run_tests<T>(&mut self, action: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
        let _lock = lock(&self.entry.directory)?;
        self.verify()?;
        self.phase = Phase::AfterTest;
        durable_new(
            &self.directory.join("after-test"),
            b"never reusable after test start",
        )?;
        action(&self.directory.join("outputs"))
    }
}
