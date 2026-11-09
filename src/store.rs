//! On-disk cache store for kioku.
//!
//! The store lives in `<git-dir>/kioku/` by default and can be redirected with the
//! `KIOKU_DIR` environment variable. Layout:
//!
//! ```text
//! <store>/
//!   entries/<key>.json   one JSON document per cache key
//!   counters.json        cumulative counters
//!   shadow.jsonl         shadow log written by `kioku verify`
//!   lock                 advisory lock file, never removed by `kioku clean`
//! ```
//!
//! Safety rules implemented here:
//!
//! - a document that does not parse, or whose content does not match the current
//!   snapshot byte for byte, is never a hit: the store fails closed;
//! - an entry is written to a temporary file and renamed, so a reader never observes
//!   a partial document and two writers of the same key cannot corrupt each other;
//! - the lock file has its own inode and is never deleted, so `clean` cannot pull the
//!   lock out from under a running process.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};

/// Schema identifier of an entry document.
pub const ENTRY_SCHEMA: &str = "kioku/entry/v1";
/// Schema identifier of the counters document.
pub const COUNTERS_SCHEMA: &str = "kioku/counters/v1";
/// Schema identifier of a shadow record.
pub const SHADOW_SCHEMA: &str = "kioku/shadow/v1";

/// Environment variable that moves the cache store.
pub const STORE_DIR_ENV: &str = "KIOKU_DIR";

const ENTRIES_DIR: &str = "entries";
const COUNTERS_FILE: &str = "counters.json";
const SHADOW_FILE: &str = "shadow.jsonl";
const LOCK_FILE: &str = "lock";

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Errors raised by the store.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A filesystem operation failed.
    #[error("i/o error on {path}: {source}")]
    Io {
        /// Path the operation was working on.
        path: PathBuf,
        /// Underlying operating system error.
        source: std::io::Error,
    },
    /// A cache document could not be parsed.
    #[error("cache document {path} is not valid JSON: {source}")]
    Json {
        /// Path of the offending document.
        path: PathBuf,
        /// Underlying parser error.
        source: serde_json::Error,
    },
    /// A cache key cannot be used as a file name.
    #[error("unsafe cache key {key:?}")]
    UnsafeKey {
        /// The rejected key.
        key: String,
    },
    /// The document does not describe the key it is stored under.
    #[error("cache document {path} does not describe the key {key:?}")]
    Inconsistent {
        /// Path of the offending document.
        path: PathBuf,
        /// The key the document claims.
        key: String,
    },
}

/// One input file recorded in a cache entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// Fingerprint of the file contents.
    pub digest: String,
    /// Unix permission bits of the file.
    pub mode: u32,
}

/// A cached successful run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// Schema identifier of this document.
    pub schema: String,
    /// Cache key of the run.
    pub key: String,
    /// Version of kioku that produced the entry.
    pub version: String,
    /// Exact command line of the run.
    pub argv: Vec<String>,
    /// Working directory of the run, relative to the repository root.
    pub cwd: String,
    /// Fingerprint mode used by the run (`ast` or `bytes`).
    pub mode: String,
    /// AST key captured with the successful byte-mode run, for diagnostics only.
    #[serde(default)]
    pub diagnostic_ast_key: Option<String>,
    /// Wall clock duration of the original run, in milliseconds.
    pub duration_ms: u64,
    /// Creation time, in milliseconds since the Unix epoch.
    pub created_unix_ms: u64,
    /// `true` when the entry covers the whole repository and may be reused by an
    /// identical repository; `false` when the input set was restricted.
    pub portable: bool,
    /// Absolute repository root. Metadata only: never part of the cache key.
    pub repo: String,
    /// Fingerprint of every input file.
    pub inputs: BTreeMap<String, ManifestEntry>,
    /// Environment variables that took part in the key. A variable that is not
    /// set is `None`, which is not the same as a variable set to an empty value.
    pub env: BTreeMap<String, Option<String>>,
    /// Fingerprint of every detected lockfile.
    pub lockfiles: BTreeMap<String, String>,
    /// Versions of the detected tools.
    pub tools: BTreeMap<String, String>,
}

/// The state of the current run, used to decide whether a stored entry is a hit.
#[derive(Debug, Clone)]
pub struct Lookup<'a> {
    /// Cache key of the current run.
    pub key: &'a str,
    /// Exact command line of the current run.
    pub argv: &'a [String],
    /// Working directory of the current run, relative to the repository root.
    pub cwd: &'a str,
    /// Fingerprint mode of the current run.
    pub mode: &'a str,
    /// Fingerprint of every input file of the current run.
    pub inputs: &'a BTreeMap<String, ManifestEntry>,
    /// Environment variables of the current run.
    pub env: &'a BTreeMap<String, Option<String>>,
    /// Lockfile fingerprints of the current run.
    pub lockfiles: &'a BTreeMap<String, String>,
    /// Tool versions of the current run.
    pub tools: &'a BTreeMap<String, String>,
    /// Absolute repository root of the current run.
    pub repo: &'a str,
}

impl Entry {
    /// Tells whether this entry is a valid hit for the given state.
    ///
    /// Every component is compared, not only the key: a document that disagrees with
    /// the current state is a miss. An entry produced by another repository is only
    /// accepted when it is portable (whole repository input set) and the input
    /// manifests are strictly identical.
    #[must_use]
    pub fn is_hit(&self, lookup: &Lookup<'_>) -> bool {
        self.schema == ENTRY_SCHEMA
            && self.key == lookup.key
            && self.argv == lookup.argv
            && self.cwd == lookup.cwd
            && self.mode == lookup.mode
            && self.inputs == *lookup.inputs
            && self.env == *lookup.env
            && self.lockfiles == *lookup.lockfiles
            && self.tools == *lookup.tools
            && (self.repo == lookup.repo || self.portable)
    }
}

/// Identifies the cached runs of one command, used to invalidate stale passes.
#[derive(Debug, Clone, Copy)]
pub struct CommandIdentity<'a> {
    /// Exact command line.
    pub argv: &'a [String],
    /// Working directory, relative to the repository root.
    pub cwd: &'a str,
    /// Absolute repository root.
    pub repo: &'a str,
    /// Input manifest of the current run.
    pub inputs: &'a BTreeMap<String, ManifestEntry>,
}

impl CommandIdentity<'_> {
    fn matches(&self, entry: &Entry) -> bool {
        if entry.argv != self.argv || entry.cwd != self.cwd {
            return false;
        }
        entry.repo == self.repo || (entry.portable && &entry.inputs == self.inputs)
    }
}

/// Cumulative counters of the store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Counters {
    /// Schema identifier of this document.
    pub schema: String,
    /// Number of runs served from a cached pass.
    pub hits: u64,
    /// Number of hits whose saving could be measured, because the run that
    /// learned the pass recorded a duration. `None` when the document predates
    /// this field: the count is unknown, not zero.
    #[serde(default)]
    pub measured_hits: Option<u64>,
    /// Number of runs not served from a cached pass and not bypassed: a miss,
    /// a learning run or a verification.
    pub misses: u64,
    /// Number of child processes kioku started and observed.
    pub executions: u64,
    /// Number of runs that never consulted the cache, because caching was
    /// disabled by an option, an environment variable or a carried standard
    /// input.
    pub bypassed: u64,
    /// Milliseconds saved by the hits whose saving was measured. Tracing
    /// overhead is part of the recorded learning duration, so this is an upper
    /// bound of the gain over a direct execution.
    pub saved_ms: u64,
    /// Number of learned passes quarantined because a verification of their
    /// claimed success failed.
    pub invalidated: u64,
    /// Milliseconds recorded by an older version, before per-hit measurement.
    /// Kept for information, never attributed to a measured hit.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub legacy_saved_ms: u64,
}

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde requires a predicate that borrows the field"
)]
const fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl Default for Counters {
    fn default() -> Self {
        Self {
            schema: COUNTERS_SCHEMA.to_string(),
            hits: 0,
            measured_hits: Some(0),
            misses: 0,
            executions: 0,
            bypassed: 0,
            saved_ms: 0,
            invalidated: 0,
            legacy_saved_ms: 0,
        }
    }
}

/// One line of the shadow log written by `kioku verify`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowRecord {
    /// Schema identifier of this record.
    pub schema: String,
    /// Time of the verification, in milliseconds since the Unix epoch.
    pub created_unix_ms: u64,
    /// Cache key of the verified run.
    pub key: String,
    /// Exact command line.
    pub argv: Vec<String>,
    /// Working directory, relative to the repository root.
    pub cwd: String,
    /// Fingerprint mode.
    pub mode: String,
    /// Whether kioku would have skipped the command.
    pub would_skip: bool,
    /// Exit code observed by the verification.
    pub actual_exit: i32,
    /// Whether the verification exposed a false green.
    pub false_green: bool,
}

/// Entries found in the store.
#[derive(Debug, Clone, Default)]
pub struct EntryList {
    /// Entries that parsed and carry the expected schema.
    pub entries: Vec<Entry>,
    /// Files that are not readable entries.
    pub invalid: usize,
    /// Total size of the entry files, in bytes.
    pub bytes: u64,
}

/// Result of a clean operation.
#[derive(Debug, Clone, Default)]
pub struct CleanReport {
    /// Number of removed files.
    pub removed: usize,
    /// Number of kept files.
    pub kept: usize,
    /// Freed bytes.
    pub bytes_freed: u64,
}

/// The cache store.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Opens a store rooted at the given directory.
    #[must_use]
    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Root directory of the store.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory holding the entry documents.
    #[must_use]
    pub fn entries_dir(&self) -> PathBuf {
        self.root.join(ENTRIES_DIR)
    }

    fn entry_path(&self, key: &str) -> Result<PathBuf, Error> {
        if key.is_empty()
            || key.starts_with('.')
            || key.contains('/')
            || key.contains('\\')
            || key.contains("..")
        {
            return Err(Error::UnsafeKey {
                key: key.to_string(),
            });
        }
        Ok(self.entries_dir().join(format!("{key}.json")))
    }

    /// Reads the entry stored under `key`.
    ///
    /// A missing entry is `Ok(None)`; a malformed document is an error, and the
    /// caller must treat it as a miss.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read or parsed.
    pub fn read_entry(&self, key: &str) -> Result<Option<Entry>, Error> {
        self.with_lock(|| self.read_entry_unlocked(key))
    }

    fn read_entry_unlocked(&self, key: &str) -> Result<Option<Entry>, Error> {
        let path = self.entry_path(key)?;
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(Error::Io { path, source }),
        };
        let entry = serde_json::from_str::<Entry>(&raw).map_err(|source| Error::Json {
            path: path.clone(),
            source,
        })?;
        if !is_consistent(&entry, &path) {
            return Err(Error::Inconsistent {
                path,
                key: entry.key,
            });
        }
        Ok(Some(entry))
    }

    /// Writes an entry atomically.
    ///
    /// # Errors
    ///
    /// Returns an error when the document cannot be written or renamed.
    pub fn write_entry(&self, entry: &Entry) -> Result<(), Error> {
        self.with_lock(|| self.write_entry_unlocked(entry))
    }

    fn write_entry_unlocked(&self, entry: &Entry) -> Result<(), Error> {
        let path = self.entry_path(&entry.key)?;
        let dir = self.entries_dir();
        fs::create_dir_all(&dir).map_err(|source| Error::Io {
            path: dir.clone(),
            source,
        })?;
        let body = serde_json::to_vec_pretty(entry).map_err(|source| Error::Json {
            path: path.clone(),
            source,
        })?;
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".{}.{}.{seq}.tmp", entry.key, std::process::id()));
        {
            let mut file = File::create(&tmp).map_err(|source| Error::Io {
                path: tmp.clone(),
                source,
            })?;
            file.write_all(&body).map_err(|source| Error::Io {
                path: tmp.clone(),
                source,
            })?;
            file.sync_all().map_err(|source| Error::Io {
                path: tmp.clone(),
                source,
            })?;
        }
        fs::rename(&tmp, &path).map_err(|source| Error::Io { path, source })?;
        Ok(())
    }

    /// Removes the entry stored under `key`.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be removed.
    pub fn remove_entry(&self, key: &str) -> Result<bool, Error> {
        self.with_lock(|| self.remove_entry_unlocked(key))
    }

    fn remove_entry_unlocked(&self, key: &str) -> Result<bool, Error> {
        let path = self.entry_path(key)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(Error::Io { path, source }),
        }
    }

    /// Lists the entries of the store.
    ///
    /// # Errors
    ///
    /// Returns an error when the entries directory cannot be read.
    pub fn list_entries(&self) -> Result<EntryList, Error> {
        self.with_lock(|| self.list_entries_unlocked())
    }

    fn list_entries_unlocked(&self) -> Result<EntryList, Error> {
        let dir = self.entries_dir();
        let mut list = EntryList::default();
        let reader = match fs::read_dir(&dir) {
            Ok(reader) => reader,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(list),
            Err(source) => return Err(Error::Io { path: dir, source }),
        };
        for item in reader {
            let item = item.map_err(|source| Error::Io {
                path: dir.clone(),
                source,
            })?;
            let path = item.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let size = item.metadata().map_or(0, |meta| meta.len());
            match fs::read_to_string(&path) {
                Ok(raw) => match serde_json::from_str::<Entry>(&raw) {
                    // A document that does not describe its own key is not an
                    // entry: it is counted as invalid, never served.
                    Ok(entry) if is_consistent(&entry, &path) => {
                        list.bytes += size;
                        list.entries.push(entry);
                    }
                    _ => list.invalid += 1,
                },
                Err(_) => list.invalid += 1,
            }
        }
        list.entries.sort_by(|left, right| left.key.cmp(&right.key));
        Ok(list)
    }

    /// Removes every cached pass of one command.
    ///
    /// Used after a failure: a command that just failed must not keep a cached pass
    /// that a later run could reuse.
    ///
    /// # Errors
    ///
    /// Returns an error when the entries cannot be listed or removed.
    pub fn invalidate(&self, identity: &CommandIdentity<'_>) -> Result<usize, Error> {
        self.with_lock(|| self.invalidate_unlocked(identity))
    }

    fn invalidate_unlocked(&self, identity: &CommandIdentity<'_>) -> Result<usize, Error> {
        let list = self.list_entries_unlocked()?;
        let mut removed = 0;
        for entry in &list.entries {
            if identity.matches(entry) && self.remove_entry_unlocked(&entry.key)? {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Reads the counters of the store.
    ///
    /// A missing or malformed document yields default counters: counters are
    /// telemetry, never a reason to fail a command.
    ///
    /// # Errors
    ///
    /// Returns an error only when the lock cannot be taken.
    pub fn read_counters(&self) -> Result<Counters, Error> {
        self.with_lock(|| {
            let path = self.root.join(COUNTERS_FILE);
            let raw = match fs::read_to_string(&path) {
                Ok(raw) => raw,
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(Counters::default());
                }
                Err(source) => return Err(Error::Io { path, source }),
            };
            Ok(serde_json::from_str::<Counters>(&raw).unwrap_or_default())
        })
    }

    /// Updates the counters of the store under the store lock.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock cannot be taken or the document cannot be
    /// written.
    pub fn update_counters(&self, update: impl FnOnce(&mut Counters)) -> Result<Counters, Error> {
        self.with_lock(|| self.update_counters_unlocked(update))
    }

    fn update_counters_unlocked(
        &self,
        update: impl FnOnce(&mut Counters),
    ) -> Result<Counters, Error> {
        let path = self.root.join(COUNTERS_FILE);
        let mut counters = match fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str::<Counters>(&raw).unwrap_or_default(),
            Err(_) => Counters::default(),
        };
        // A document written before per-hit measurement kept a saved total
        // without a measured count. Move that total to the legacy bucket once,
        // so a later hit cannot claim it as its own measured saving.
        if counters.measured_hits.is_none() {
            counters.legacy_saved_ms = counters.legacy_saved_ms.saturating_add(counters.saved_ms);
            counters.saved_ms = 0;
            counters.measured_hits = Some(0);
        }
        update(&mut counters);
        counters.schema = COUNTERS_SCHEMA.to_string();
        let body = serde_json::to_vec_pretty(&counters).map_err(|source| Error::Json {
            path: path.clone(),
            source,
        })?;
        let tmp = self
            .root
            .join(format!(".{COUNTERS_FILE}.{}.tmp", std::process::id()));
        {
            let mut file = File::create(&tmp).map_err(|source| Error::Io {
                path: tmp.clone(),
                source,
            })?;
            file.write_all(&body).map_err(|source| Error::Io {
                path: tmp.clone(),
                source,
            })?;
        }
        fs::rename(&tmp, &path).map_err(|source| Error::Io { path, source })?;
        Ok(counters)
    }

    /// Appends one record to the shadow log.
    ///
    /// # Errors
    ///
    /// Returns an error when the log cannot be written.
    pub fn append_shadow(&self, record: &ShadowRecord) -> Result<(), Error> {
        self.with_lock(|| self.append_shadow_unlocked(record))
    }

    fn append_shadow_unlocked(&self, record: &ShadowRecord) -> Result<(), Error> {
        fs::create_dir_all(&self.root).map_err(|source| Error::Io {
            path: self.root.clone(),
            source,
        })?;
        let path = self.root.join(SHADOW_FILE);
        let mut line = serde_json::to_string(record).map_err(|source| Error::Json {
            path: path.clone(),
            source,
        })?;
        line.push('\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|source| Error::Io {
                path: path.clone(),
                source,
            })?;
        file.write_all(line.as_bytes())
            .map_err(|source| Error::Io { path, source })?;
        Ok(())
    }

    /// Removes old entries.
    ///
    /// Without a duration every entry is removed and the counters are reset. The
    /// lock file is never removed: it keeps its inode for the whole life of the
    /// store so that a running process never loses its lock.
    ///
    /// # Errors
    ///
    /// Returns an error when the entries cannot be listed or removed.
    pub fn clean(&self, older_than: Option<Duration>) -> Result<CleanReport, Error> {
        let mut report = self.with_lock(|| self.clean_unlocked(older_than))?;
        if self.root.join("trace-v2").exists() {
            self.trace_transaction(|tx| tx.clean_states(older_than, &mut report))?;
        }
        Ok(report)
    }

    fn clean_unlocked(&self, older_than: Option<Duration>) -> Result<CleanReport, Error> {
        let mut report = CleanReport::default();
        let dir = self.entries_dir();
        let reader = match fs::read_dir(&dir) {
            Ok(reader) => Some(reader),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => return Err(Error::Io { path: dir, source }),
        };
        let now = now_unix_ms();
        for item in reader.into_iter().flatten() {
            let item = item.map_err(|source| Error::Io {
                path: dir.clone(),
                source,
            })?;
            let path = item.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let size = item.metadata().map_or(0, |meta| meta.len());
            let expired = match older_than {
                None => true,
                Some(age) => {
                    let modified = item
                        .metadata()
                        .and_then(|meta| meta.modified())
                        .ok()
                        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                        .and_then(|since| u64::try_from(since.as_millis()).ok())
                        .unwrap_or(now);
                    let age_ms = u64::try_from(age.as_millis()).unwrap_or(u64::MAX);
                    now.saturating_sub(modified) >= age_ms
                }
            };
            if expired {
                match fs::remove_file(&path) {
                    Ok(()) => {
                        report.removed += 1;
                        report.bytes_freed += size;
                    }
                    Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
                    Err(source) => return Err(Error::Io { path, source }),
                }
            } else {
                report.kept += 1;
            }
        }
        if older_than.is_none() {
            self.update_counters_unlocked(|counters| *counters = Counters::default())?;
        }
        Ok(report)
    }

    /// Resets the counters of the store.
    ///
    /// # Errors
    ///
    /// Returns an error when the document cannot be written.
    pub fn reset_counters(&self) -> Result<(), Error> {
        self.update_counters(|counters| *counters = Counters::default())?;
        Ok(())
    }

    fn with_lock<T>(&self, action: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
        fs::create_dir_all(&self.root).map_err(|source| Error::Io {
            path: self.root.clone(),
            source,
        })?;
        let path = self.root.join(LOCK_FILE);
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| Error::Io {
                path: path.clone(),
                source,
            })?;
        file.lock_exclusive().map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        let result = action();
        let _ = file.unlock();
        result
    }
}

/// Path of the cache store.
///
/// The default is `<git dir>/kioku`, next to the repository and not in the work
/// tree. `KIOKU_DIR` moves it: a relative path is resolved against the working
/// directory, the way a shell resolves it, so a run from a subdirectory uses
/// the store of that subdirectory and never the store of the repository root.
///
/// This is the single resolution of the store path: the runtime opens the store
/// here, and every path that reads or writes the store resolves it the same
/// way. A store inside the work tree turns the cache off: the files of the
/// store are not part of the key, so a tracked file that happens to live there
/// would be left out of it too, and a change of it would not change the key.
#[must_use]
pub fn store_dir(cwd: &Path, git_dir: &Path) -> PathBuf {
    match std::env::var_os(STORE_DIR_ENV) {
        Some(value) if !value.is_empty() => {
            let path = PathBuf::from(value);
            let path = if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            };
            normalize(path)
        }
        _ => normalize(git_dir.join("kioku")),
    }
}

/// Normalizes a path: the longest existing ancestor is canonicalized and the
/// missing part is appended, so the result does not depend on whether the
/// directory exists yet.
fn normalize(path: PathBuf) -> PathBuf {
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut current = path.as_path();
    loop {
        if let Ok(canonical) = current.canonicalize() {
            let mut result = canonical;
            for part in missing.iter().rev() {
                result.push(part);
            }
            return result;
        }
        match (current.parent(), current.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name.to_os_string());
                current = parent;
            }
            _ => return path,
        }
    }
}

/// Tells whether a document is the entry of the key it is stored under.
///
/// The schema must be the expected one and the file name must be the key: a
/// document that does not match is never served and never counted as valid.
fn is_consistent(entry: &Entry, path: &Path) -> bool {
    entry.schema == ENTRY_SCHEMA
        && path.file_stem().and_then(|stem| stem.to_str()) == Some(entry.key.as_str())
}

/// Current time in milliseconds since the Unix epoch.
#[must_use]
pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| u64::try_from(since.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{CommandIdentity, ENTRY_SCHEMA, Entry, Lookup, ManifestEntry, Store};
    use std::collections::BTreeMap;

    fn manifest() -> BTreeMap<String, ManifestEntry> {
        let mut inputs = BTreeMap::new();
        inputs.insert(
            "src/main.rs".to_string(),
            ManifestEntry {
                digest: "abc".to_string(),
                mode: 0o644,
            },
        );
        inputs
    }

    fn entry() -> Entry {
        Entry {
            schema: ENTRY_SCHEMA.to_string(),
            key: "0123".to_string(),
            version: "0.1.0".to_string(),
            argv: vec!["cargo".to_string(), "test".to_string()],
            cwd: String::new(),
            mode: "ast".to_string(),
            diagnostic_ast_key: None,
            duration_ms: 1_500,
            created_unix_ms: 1_000,
            portable: true,
            repo: "/repo".to_string(),
            inputs: manifest(),
            env: BTreeMap::new(),
            lockfiles: BTreeMap::new(),
            tools: BTreeMap::new(),
        }
    }

    struct Fixture {
        argv: Vec<String>,
        inputs: BTreeMap<String, ManifestEntry>,
        env: BTreeMap<String, Option<String>>,
        lockfiles: BTreeMap<String, String>,
        tools: BTreeMap<String, String>,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                argv: vec!["cargo".to_string(), "test".to_string()],
                inputs: manifest(),
                env: BTreeMap::new(),
                lockfiles: BTreeMap::new(),
                tools: BTreeMap::new(),
            }
        }

        fn lookup<'a>(&'a self, key: &'a str, repo: &'a str) -> Lookup<'a> {
            Lookup {
                key,
                argv: &self.argv,
                cwd: "",
                mode: "ast",
                inputs: &self.inputs,
                env: &self.env,
                lockfiles: &self.lockfiles,
                tools: &self.tools,
                repo,
            }
        }
    }

    #[test]
    fn store_operations_fail_when_lock_cannot_be_opened() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("lock")).expect("block lock file");
        let store = Store::open(dir.path());
        assert!(store.write_entry(&entry()).is_err());
        let fixture = Fixture::new();
        let identity = CommandIdentity {
            argv: &fixture.argv,
            cwd: "",
            repo: "/repo",
            inputs: &fixture.inputs,
        };
        assert!(store.invalidate(&identity).is_err());
        assert!(store.clean(None).is_err());
        let shadow = super::ShadowRecord {
            schema: super::SHADOW_SCHEMA.into(),
            created_unix_ms: 0,
            key: "0123".into(),
            argv: fixture.argv,
            cwd: String::new(),
            mode: "bytes".into(),
            would_skip: false,
            actual_exit: 0,
            false_green: false,
        };
        assert!(store.append_shadow(&shadow).is_err());
    }

    #[test]
    fn a_matching_entry_is_a_hit() {
        let fixture = Fixture::new();
        assert!(entry().is_hit(&fixture.lookup("0123", "/repo")));
    }

    #[test]
    fn a_changed_input_is_a_miss() {
        let mut fixture = Fixture::new();
        fixture.inputs.insert(
            "src/main.rs".to_string(),
            ManifestEntry {
                digest: "different".to_string(),
                mode: 0o644,
            },
        );
        assert!(!entry().is_hit(&fixture.lookup("0123", "/repo")));
    }

    #[test]
    fn another_repository_is_a_miss_unless_the_entry_is_portable() {
        let fixture = Fixture::new();
        let mut restricted = entry();
        restricted.portable = false;
        assert!(!restricted.is_hit(&fixture.lookup("0123", "/other")));
        assert!(entry().is_hit(&fixture.lookup("0123", "/other")));
    }

    #[test]
    fn a_document_with_another_schema_is_never_a_hit() {
        let fixture = Fixture::new();
        let mut broken = entry();
        broken.schema = "kioku/entry/v0".to_string();
        assert!(!broken.is_hit(&fixture.lookup("0123", "/repo")));
    }

    #[test]
    fn an_entry_survives_a_write_and_a_read() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = Store::open(dir.path());
        store
            .write_entry(&entry())
            .expect("the entry must be written");
        let read = store
            .read_entry("0123")
            .expect("the entry must be readable")
            .expect("the entry must exist");
        assert_eq!(read.key, "0123");
        assert_eq!(read.inputs.len(), 1);
    }

    #[test]
    fn a_malformed_document_is_not_a_hit() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = Store::open(dir.path());
        store
            .write_entry(&entry())
            .expect("the entry must be written");
        let path = store.entries_dir().join("0123.json");
        std::fs::write(&path, "{ broken").expect("the document must be broken");
        assert!(store.read_entry("0123").is_err());
    }

    #[test]
    fn a_failure_invalidates_the_passes_of_the_command() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = Store::open(dir.path());
        store
            .write_entry(&entry())
            .expect("the entry must be written");
        let inputs = manifest();
        let identity = CommandIdentity {
            argv: &["cargo".to_string(), "test".to_string()],
            cwd: "",
            repo: "/repo",
            inputs: &inputs,
        };
        assert_eq!(store.invalidate(&identity).expect("invalidation"), 1);
        assert!(store.read_entry("0123").expect("read").is_none());
    }

    #[test]
    fn clean_keeps_the_lock_file() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = Store::open(dir.path());
        store
            .write_entry(&entry())
            .expect("the entry must be written");
        store
            .update_counters(|counters| counters.hits += 1)
            .expect("counters");
        let report = store.clean(None).expect("clean");
        assert_eq!(report.removed, 1);
        assert!(store.root().join("lock").exists(), "the lock must stay");
        assert_eq!(store.read_counters().expect("counters").hits, 0);
    }
}

// Trace state is separate from legacy entries; clean never removes quarantine.
pub(crate) struct TraceTransaction {
    root: PathBuf,
    pub(crate) secret: [u8; 32],
}

impl Store {
    pub(crate) fn trace_transaction<T>(
        &self,
        action: impl FnOnce(&TraceTransaction) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let root = self.root.join("trace-v2");
        fs::create_dir_all(&root).map_err(|source| Error::Io {
            path: root.clone(),
            source,
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).map_err(|source| {
                Error::Io {
                    path: root.clone(),
                    source,
                }
            })?;
        }
        let lock_path = root.join("lock");
        let lock = private_options()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| Error::Io {
                path: lock_path.clone(),
                source,
            })?;
        lock.lock_exclusive().map_err(|source| Error::Io {
            path: lock_path,
            source,
        })?;
        let key_path = root.join("key");
        let secret = match fs::read(&key_path) {
            Ok(bytes) => bytes.try_into().map_err(|_| Error::Io {
                path: key_path.clone(),
                source: std::io::Error::other("invalid trace key"),
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut key = [0_u8; 32];
                getrandom::fill(&mut key).map_err(|_| Error::Io {
                    path: key_path.clone(),
                    source: std::io::Error::other("cannot generate trace key"),
                })?;
                let mut file = private_options()
                    .create_new(true)
                    .write(true)
                    .open(&key_path)
                    .map_err(|source| Error::Io {
                        path: key_path.clone(),
                        source,
                    })?;
                file.write_all(&key)
                    .and_then(|()| file.sync_all())
                    .map_err(|source| Error::Io {
                        path: key_path.clone(),
                        source,
                    })?;
                key
            }
            Err(source) => {
                return Err(Error::Io {
                    path: key_path,
                    source,
                });
            }
        };
        let result = action(&TraceTransaction { root, secret });
        let _ = lock.unlock();
        result
    }
}

fn private_options() -> OpenOptions {
    #[cfg(unix)]
    let mut options = OpenOptions::new();
    #[cfg(not(unix))]
    let options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

impl TraceTransaction {
    fn path(&self, kind: &str, id: &str) -> Result<PathBuf, Error> {
        if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::UnsafeKey { key: id.to_owned() });
        }
        Ok(self.root.join(format!("{kind}-{id}.json")))
    }

    pub(crate) fn state(&self, id: &str) -> Result<Option<crate::policy::State>, Error> {
        let path = self.path("state", id)?;
        match fs::read(&path) {
            Ok(bytes) => {
                let mut value: serde_json::Value =
                    serde_json::from_slice(&bytes).map_err(|source| Error::Json {
                        path: path.clone(),
                        source,
                    })?;
                let authentication = value
                    .as_object_mut()
                    .and_then(|object| object.remove("authentication"))
                    .and_then(|v| v.as_str().map(str::to_owned));
                let state: crate::policy::State =
                    serde_json::from_value(value).map_err(|source| Error::Json {
                        path: path.clone(),
                        source,
                    })?;
                let encoded = serde_json::to_vec(&state).map_err(|source| Error::Json {
                    path: path.clone(),
                    source,
                })?;
                let expected = blake3::keyed_hash(&self.secret, &encoded)
                    .to_hex()
                    .to_string();
                if authentication.as_deref() != Some(expected.as_str()) {
                    return Err(Error::Io {
                        path,
                        source: std::io::Error::other("trace state authentication failed"),
                    });
                }
                Ok(Some(state))
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(Error::Io { path, source }),
        }
    }

    pub(crate) fn set_state(&self, id: &str, state: &crate::policy::State) -> Result<(), Error> {
        let path = self.path("state", id)?;
        let encoded = serde_json::to_vec(state).map_err(|source| Error::Json {
            path: path.clone(),
            source,
        })?;
        let mut value = serde_json::to_value(state).map_err(|source| Error::Json {
            path: path.clone(),
            source,
        })?;
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "authentication".into(),
                blake3::keyed_hash(&self.secret, &encoded)
                    .to_hex()
                    .to_string()
                    .into(),
            );
        }
        let bytes = serde_json::to_vec(&value).map_err(|source| Error::Json {
            path: path.clone(),
            source,
        })?;
        let tmp = self.root.join(format!(
            ".{}.{}.tmp",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = private_options()
            .create_new(true)
            .write(true)
            .open(&tmp)
            .map_err(|source| Error::Io {
                path: tmp.clone(),
                source,
            })?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|source| Error::Io {
                path: tmp.clone(),
                source,
            })?;
        fs::rename(&tmp, &path).map_err(|source| Error::Io { path, source })
    }

    pub(crate) fn quarantined(&self, id: &str) -> Result<bool, Error> {
        let path = self.path("quarantine", id)?;
        match fs::metadata(&path) {
            Ok(_) => Ok(true),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(Error::Io { path, source }),
        }
    }

    /// Creates the quarantine marker of a pass.
    ///
    /// Returns `false` when the pass was already quarantined: the creation is
    /// atomic, so concurrent verifications of the same false green count one
    /// invalidation, not one each.
    pub(crate) fn quarantine(&self, id: &str) -> Result<bool, Error> {
        let path = self.path("quarantine", id)?;
        match private_options().create_new(true).write(true).open(&path) {
            Ok(file) => file
                .sync_all()
                .map(|()| true)
                .map_err(|source| Error::Io { path, source }),
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(source) => Err(Error::Io { path, source }),
        }
    }

    /// Records the measured duration of the run that learned a pass.
    ///
    /// A duration describes the state it was measured on: publishing a
    /// different state replaces it, publishing an equivalent one keeps the
    /// first record, so a verification never replaces the duration of the
    /// learning run. Timing is telemetry: it never takes part in a reuse
    /// decision, and a missing or unreadable value is reported as an unknown
    /// saving.
    pub(crate) fn set_timing(&self, id: &str, duration_ms: u64) -> Result<(), Error> {
        let path = self.path("timing", id)?;
        let mut file = private_options()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .map_err(|source| Error::Io {
                path: path.clone(),
                source,
            })?;
        file.write_all(duration_ms.to_string().as_bytes())
            .map_err(|source| Error::Io { path, source })
    }

    /// Reads the recorded duration of the run that learned a pass.
    pub(crate) fn timing(&self, id: &str) -> Result<Option<u64>, Error> {
        let path = self.path("timing", id)?;
        match fs::read_to_string(&path) {
            Ok(raw) => Ok(raw.trim().parse().ok()),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(Error::Io { path, source }),
        }
    }

    pub(crate) fn ast_diagnostic(
        &self,
        id: &str,
        digest: &str,
        write: bool,
    ) -> Result<bool, Error> {
        let path = self.path("ast-diagnostic", id)?;
        let value = blake3::keyed_hash(&self.secret, digest.as_bytes())
            .to_hex()
            .to_string();
        if write {
            let mut file = private_options()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&path)
                .map_err(|source| Error::Io {
                    path: path.clone(),
                    source,
                })?;
            file.write_all(value.as_bytes())
                .map_err(|source| Error::Io { path, source })?;
            Ok(true)
        } else {
            match fs::read_to_string(&path) {
                Ok(previous) => Ok(previous == value),
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(source) => Err(Error::Io { path, source }),
            }
        }
    }

    pub(crate) fn log_verification(
        &self,
        would_skip: bool,
        actual_exit: i32,
        quarantined: bool,
    ) -> Result<(), Error> {
        let path = self.root.join("shadow.jsonl");
        let mut file = private_options()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|source| Error::Io {
                path: path.clone(),
                source,
            })?;
        let line = serde_json::json!({"would_skip":would_skip,"actual_exit":actual_exit,"quarantined":quarantined});
        writeln!(file, "{line}").map_err(|source| Error::Io { path, source })
    }

    pub(crate) fn counts(&self) -> Result<(usize, usize), Error> {
        let mut states = 0;
        let mut quarantines = 0;
        for item in fs::read_dir(&self.root).map_err(|source| Error::Io {
            path: self.root.clone(),
            source,
        })? {
            let item = item.map_err(|source| Error::Io {
                path: self.root.clone(),
                source,
            })?;
            let name = item.file_name();
            let name = name.to_string_lossy();
            if let Some(id) = name
                .strip_prefix("state-")
                .and_then(|v| v.strip_suffix(".json"))
            {
                if self.state(id).ok().flatten().is_some() {
                    states += 1;
                }
            } else if name.starts_with("quarantine-") && name.ends_with(".json") {
                quarantines += 1;
            }
        }
        Ok((states, quarantines))
    }

    fn clean_states(
        &self,
        older_than: Option<Duration>,
        report: &mut CleanReport,
    ) -> Result<(), Error> {
        for entry in fs::read_dir(&self.root).map_err(|source| Error::Io {
            path: self.root.clone(),
            source,
        })? {
            let entry = entry.map_err(|source| Error::Io {
                path: self.root.clone(),
                source,
            })?;
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !(name.starts_with("state-")
                || name.starts_with("ast-diagnostic-")
                || name.starts_with("timing-"))
                || path.extension().and_then(|v| v.to_str()) != Some("json")
            {
                continue;
            }
            let metadata = entry.metadata().map_err(|source| Error::Io {
                path: path.clone(),
                source,
            })?;
            let expired = older_than.is_none_or(|age| {
                metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.elapsed().ok())
                    .is_some_and(|elapsed| elapsed >= age)
            });
            if expired {
                fs::remove_file(&path).map_err(|source| Error::Io { path, source })?;
                report.removed += 1;
                report.bytes_freed = report.bytes_freed.saturating_add(metadata.len());
            } else {
                report.kept += 1;
            }
        }
        Ok(())
    }

    pub(crate) fn reset_quarantine(&self, id: &str) -> Result<(), Error> {
        for kind in ["quarantine", "state", "ast-diagnostic", "timing"] {
            let path = self.path(kind, id)?;
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => return Err(Error::Io { path, source }),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod trace_store_tests {
    use super::Store;

    #[test]
    fn quarantine_prevents_a_prepared_hit_at_authorization() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path());
        let id = "b".repeat(64);
        let initially_allowed = store
            .trace_transaction(|tx| Ok(!tx.quarantined(&id)?))
            .unwrap();
        assert!(initially_allowed);
        let other = Store::open(tmp.path());
        let child_id = id.clone();
        std::thread::spawn(move || {
            other
                .trace_transaction(|tx| tx.quarantine(&child_id))
                .unwrap();
        })
        .join()
        .unwrap();
        let finally_allowed = store
            .trace_transaction(|tx| Ok(!tx.quarantined(&id)?))
            .unwrap();
        assert!(!finally_allowed);
    }

    #[test]
    fn quarantine_reports_only_the_first_creation() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path());
        let id = "c".repeat(64);
        assert!(store.trace_transaction(|tx| tx.quarantine(&id)).unwrap());
        assert!(
            !store.trace_transaction(|tx| tx.quarantine(&id)).unwrap(),
            "a second verification must not count another invalidation"
        );
        let (_, quarantines) = store
            .trace_transaction(super::TraceTransaction::counts)
            .unwrap();
        assert_eq!(quarantines, 1);
    }

    #[test]
    fn a_barrier_race_creates_one_quarantine_and_counts_one_invalidation() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path());
        let id = "d".repeat(64);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let store = store.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                let id = id.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let created = store.trace_transaction(|tx| tx.quarantine(&id)).unwrap();
                    if created {
                        store
                            .update_counters(|counters| {
                                counters.invalidated = counters.invalidated.saturating_add(1);
                            })
                            .unwrap();
                    }
                    created
                })
            })
            .collect();
        let created: Vec<bool> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(created.iter().filter(|created| **created).count(), 1);
        assert_eq!(store.read_counters().unwrap().invalidated, 1);
        let (_, quarantines) = store
            .trace_transaction(super::TraceTransaction::counts)
            .unwrap();
        assert_eq!(quarantines, 1);
    }

    #[test]
    fn quarantine_survives_reopen_and_clean_and_reset_is_explicit() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "a".repeat(64);
        let store = Store::open(tmp.path());
        store.trace_transaction(|tx| tx.quarantine(&id)).unwrap();
        store.clean(None).unwrap();
        let reopened = Store::open(tmp.path());
        assert!(
            reopened
                .trace_transaction(|tx| tx.quarantined(&id))
                .unwrap()
        );
        reopened
            .trace_transaction(|tx| tx.reset_quarantine(&id))
            .unwrap();
        assert!(
            !reopened
                .trace_transaction(|tx| tx.quarantined(&id))
                .unwrap()
        );
    }
}
