//! Conservative trace policy. The caller must enforce a stable input window and
//! serialize decisions with the runtime store and quarantine lock.
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};
use thiserror::Error;

/// An access observed by the complete trace.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObservedPath {
    /// Lexical path observed by the tracer.
    pub path: PathBuf,
    /// Kind of observation.
    pub kind: PathKind,
}
/// The operation whose outcome must be revalidated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PathKind {
    /// File contents were read.
    File,
    /// Executable contents were observed.
    Executable,
    /// Path did not exist.
    Absent,
    /// Metadata was queried.
    Metadata,
    /// Directory entries were read.
    Directory,
}
/// A failed or unsupported observation cannot authorize reuse.
#[derive(Debug, Error)]
pub enum PolicyError {
    /// Observation cannot be represented safely.
    #[error("unsupported or ambiguous dependency")]
    Unsupported,
    /// A resolved dependency entered a pseudo-filesystem.
    #[error("dependency resolves into a pseudo-filesystem")]
    SystemPath,
    /// Dependency access failed.
    #[error("dependency could not be read: {0}")]
    Io(#[from] std::io::Error),
    /// Environment has duplicate names.
    #[error("duplicate environment variable")]
    DuplicateEnvironment,
    /// Evidence is incomplete or unstable.
    #[error("incomplete or unstable observation")]
    Incomplete,
}

#[cfg(unix)]
#[allow(
    clippy::unnecessary_wraps,
    reason = "matching fallible non-Unix implementation"
)]
fn os_bytes(s: &std::ffi::OsStr) -> Result<Vec<u8>, PolicyError> {
    use std::os::unix::ffi::OsStrExt;
    Ok(s.as_bytes().to_vec())
}
#[cfg(not(unix))]
fn os_bytes(_: &std::ffi::OsStr) -> Result<Vec<u8>, PolicyError> {
    Err(PolicyError::Unsupported)
}
#[cfg(unix)]
#[allow(
    clippy::unnecessary_wraps,
    reason = "matching fallible non-Unix implementation"
)]
fn bytes_path(s: &[u8]) -> Result<PathBuf, PolicyError> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(OsString::from_vec(s.to_vec())))
}
#[cfg(not(unix))]
fn bytes_path(_: &[u8]) -> Result<PathBuf, PolicyError> {
    Err(PolicyError::Unsupported)
}

/// Digest the complete child environment without storing its values.
///
/// # Errors
/// Returns an error for duplicate or unsupported environment entries.
pub fn env_digest(key: &[u8; 32], vars: &[(OsString, OsString)]) -> Result<[u8; 32], PolicyError> {
    let pairs = vars
        .iter()
        .map(|(k, v)| Ok((os_bytes(k)?, os_bytes(v)?)))
        .collect::<Result<Vec<_>, PolicyError>>()?;
    let mut names: Vec<_> = pairs.iter().map(|(name, _)| name).collect();
    names.sort_unstable();
    if names.windows(2).any(|w| w[0] == w[1]) {
        return Err(PolicyError::DuplicateEnvironment);
    }
    let mut h = blake3::Hasher::new_keyed(key);
    h.update(b"kioku/v2/environment\0");
    for (k, v) in pairs {
        for item in [k, v] {
            h.update(&(item.len() as u64).to_le_bytes());
            h.update(&item);
        }
    }
    Ok(*h.finalize().as_bytes())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    path: Vec<u8>,
    kind: PathKind,
    digest: [u8; 32],
}
/// Private local manifest. Paths are lossless OS bytes and must never enter reports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    version: u32,
    records: Vec<Record>,
}

#[cfg(unix)]
fn metadata_bytes(meta: &fs::Metadata) -> Vec<u8> {
    use std::os::unix::fs::MetadataExt;
    let mut out = Vec::new();
    for x in [
        u64::from(meta.mode()),
        u64::from(meta.uid()),
        u64::from(meta.gid()),
        meta.len(),
        meta.mtime().cast_unsigned(),
        meta.mtime_nsec().cast_unsigned(),
        meta.ctime().cast_unsigned(),
        meta.ctime_nsec().cast_unsigned(),
        meta.ino(),
        meta.dev(),
        meta.nlink(),
    ] {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}
#[cfg(not(unix))]
fn metadata_bytes(_: &fs::Metadata) -> Vec<u8> {
    Vec::new()
}

#[allow(
    clippy::too_many_lines,
    reason = "path traversal and observation validation share state"
)]
fn record(path: &Path, kind: PathKind) -> Result<Record, PolicyError> {
    use std::collections::VecDeque;
    use std::path::Component;
    if !path.is_absolute() {
        return Err(PolicyError::Unsupported);
    }
    let mut remaining: VecDeque<_> = path
        .components()
        .filter_map(|part| match part {
            Component::Normal(value) => Some(value.to_os_string()),
            Component::ParentDir | Component::Prefix(_) => Some(OsString::from("..")),
            Component::RootDir | Component::CurDir => None,
        })
        .collect();
    let mut current = PathBuf::from("/");
    let mut h = blake3::Hasher::new();
    h.update(b"kioku/v2/dependency\0");
    let mut links = 0_u8;
    let mut finalized = false;
    while let Some(part) = remaining.pop_front() {
        if part == ".." {
            if !current.pop() {
                return Err(PolicyError::Unsupported);
            }
            continue;
        }
        current.push(part);
        if ["/proc", "/sys", "/dev"]
            .iter()
            .any(|root| current.starts_with(root))
        {
            return Err(PolicyError::SystemPath);
        }
        let meta = match fs::symlink_metadata(&current) {
            Ok(meta) => meta,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && remaining.is_empty()
                    && kind == PathKind::Absent =>
            {
                h.update(b"absent");
                return Ok(Record {
                    path: os_bytes(path.as_os_str())?,
                    kind,
                    digest: *h.finalize().as_bytes(),
                });
            }
            Err(error) => return Err(error.into()),
        };
        if meta.file_type().is_symlink() {
            links = links.checked_add(1).ok_or(PolicyError::Unsupported)?;
            if links > 40 {
                return Err(PolicyError::Unsupported);
            }
            let target = fs::read_link(&current)?;
            h.update(b"symlink");
            h.update(&metadata_bytes(&meta));
            let target_bytes = os_bytes(target.as_os_str())?;
            h.update(&(target_bytes.len() as u64).to_le_bytes());
            h.update(&target_bytes);
            if metadata_bytes(&meta) != metadata_bytes(&fs::symlink_metadata(&current)?) {
                return Err(PolicyError::Incomplete);
            }
            current.pop();
            if target.is_absolute() {
                current = PathBuf::from("/");
            }
            let parts: Vec<_> = target
                .components()
                .filter_map(|part| match part {
                    Component::Normal(value) => Some(value.to_os_string()),
                    Component::ParentDir | Component::Prefix(_) => Some(OsString::from("..")),
                    Component::RootDir | Component::CurDir => None,
                })
                .collect();
            for value in parts.into_iter().rev() {
                remaining.push_front(value);
            }
        } else if remaining.is_empty() {
            finalized = true;
            if kind == PathKind::Absent {
                return Err(PolicyError::Unsupported);
            }
            h.update(&metadata_bytes(&meta));
            match kind {
                PathKind::File | PathKind::Executable if meta.is_file() => {
                    h.update(&fs::read(&current)?);
                }
                PathKind::Metadata => {}
                PathKind::Directory if meta.is_dir() => {
                    let mut entries = fs::read_dir(&current)?
                        .map(|e| {
                            e.and_then(|e| {
                                os_bytes(&e.file_name())
                                    .map_err(|_| std::io::Error::other("unsupported filename"))
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    entries.sort_unstable();
                    for entry in entries {
                        h.update(&(entry.len() as u64).to_le_bytes());
                        h.update(&entry);
                    }
                }
                _ => return Err(PolicyError::Unsupported),
            }
            if metadata_bytes(&meta) != metadata_bytes(&fs::symlink_metadata(&current)?) {
                return Err(PolicyError::Incomplete);
            }
        } else if !meta.is_dir() {
            return Err(PolicyError::Unsupported);
        } else {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                h.update(b"parent");
                for value in [
                    u64::from(meta.mode()),
                    u64::from(meta.uid()),
                    u64::from(meta.gid()),
                ] {
                    h.update(&value.to_le_bytes());
                }
            }
        }
    }
    if !finalized {
        return Err(PolicyError::Unsupported);
    }
    Ok(Record {
        path: os_bytes(path.as_os_str())?,
        kind,
        digest: *h.finalize().as_bytes(),
    })
}

impl Manifest {
    /// Capture observed paths; caller ensures paths and inputs stay stable through execution.
    ///
    /// # Errors
    /// Returns an error if an observation is unsupported or cannot be read.
    pub fn capture(paths: &[ObservedPath]) -> Result<Self, PolicyError> {
        let mut sorted = paths.to_vec();
        sorted.sort();
        sorted.dedup();
        let records = sorted
            .iter()
            .map(|p| record(&p.path, p.kind))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            version: 2,
            records,
        })
    }
    /// Number of recorded paths.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the manifest records no paths.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Identify changed dependencies with keyed IDs; paths are never returned.
    #[must_use]
    pub fn changes(&self, key: &[u8; 32]) -> Vec<DependencyChange> {
        self.records
            .iter()
            .filter(|r| {
                bytes_path(&r.path)
                    .and_then(|path| record(&path, r.kind))
                    .map_or(true, |current| current != **r)
            })
            .map(|r| DependencyChange {
                id: blake3::keyed_hash(key, &r.path).to_hex().to_string(),
                kind: r.kind,
            })
            .collect()
    }
    /// Count changed or unreadable dependencies without disclosing their paths.
    #[must_use]
    pub fn changed_count(&self) -> usize {
        self.records
            .iter()
            .filter(|r| {
                bytes_path(&r.path)
                    .and_then(|path| record(&path, r.kind))
                    .map_or(true, |current| current != **r)
            })
            .count()
    }
    /// Revalidate all dependencies. Errors deny reuse.
    ///
    /// # Errors
    /// Returns an error if any dependency cannot be read.
    pub fn revalidate(&self) -> Result<bool, PolicyError> {
        if self.version != 2 {
            return Ok(false);
        }
        for r in &self.records {
            if record(&bytes_path(&r.path)?, r.kind)? != *r {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
/// Redacted changed dependency.
#[derive(Clone, Debug, Serialize)]
pub struct DependencyChange {
    /// Store-keyed identifier of the lexical path.
    pub id: String,
    /// Type of observed dependency.
    pub kind: PathKind,
}
/// Complete runtime identity. Values are digests, not plaintext environment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    /// Repository identity.
    pub repository: String,
    /// Working directory identity.
    pub cwd: String,
    /// Keyed command digest.
    pub argv_digest: [u8; 32],
    /// Keyed complete environment digest.
    pub environment_digest: [u8; 32],
    /// Digest of tracer identity and settings.
    pub tracer_digest: [u8; 32],
    /// Policy schema version.
    pub version: u32,
}
/// Persisted learning state; runtime owns storage and locking.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    identity: Identity,
    manifest: Manifest,
    eligible: bool,
}
impl State {
    /// The previously observed dependency set, for pre-execution validation.
    #[must_use]
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    /// Whether stable pre- and post-execution observations established eligibility.
    #[must_use]
    pub fn is_eligible(&self) -> bool {
        self.eligible
    }
    /// Whether this state belongs to the supplied command, environment and tracer.
    #[must_use]
    pub fn matches_identity(&self, identity: &Identity) -> bool {
        self.identity == *identity
    }
    /// Whether the keyed environment digest differs.
    #[must_use]
    pub fn environment_changed(&self, identity: &Identity) -> bool {
        self.identity.environment_digest != identity.environment_digest
    }
    /// Whether tracer identity differs.
    #[must_use]
    pub fn tracer_changed(&self, identity: &Identity) -> bool {
        self.identity.tracer_digest != identity.tracer_digest
    }
}
/// Check whether a learned state is reusable; runtime must hold its lock and check quarantine.
#[must_use]
pub fn decide(
    identity: &Identity,
    state: Option<&State>,
    current: &Manifest,
    trust_trace: bool,
    quarantined: bool,
) -> bool {
    trust_trace
        && !quarantined
        && identity.version == 2
        && state.is_some_and(|s| {
            s.eligible
                && s.identity == *identity
                && s.manifest == *current
                && current.revalidate().unwrap_or(false)
        })
}
/// Transition only after a successful, complete trace. New dependencies restart discovery.
///
/// # Errors
/// Returns an error for incomplete or unstable evidence.
pub fn observe(
    identity: &Identity,
    state: Option<&State>,
    before: Option<&Manifest>,
    after: &Manifest,
    success: bool,
    complete: bool,
) -> Result<State, PolicyError> {
    if identity.version != 2 || !success || !complete || !after.revalidate()? {
        return Err(PolicyError::Incomplete);
    }
    let eligible = state
        .is_some_and(|s| s.identity == *identity && s.manifest == *after && before == Some(after));
    Ok(State {
        identity: identity.clone(),
        manifest: after.clone(),
        eligible,
    })
}
