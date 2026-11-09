//! Detection of the tools a repository depends on.
//!
//! The same command can give two different results with two versions of the
//! same tool, so the version of the tools the repository depends on is part
//! of the cache key. A tool is only looked up when the repository holds a
//! lockfile of its ecosystem, so an unrelated toolchain cannot change the key.
//!
//! Three things decide which tool really runs, and all three are captured:
//!
//! * the name the tool is called with. A tool manager installs a proxy under
//!   the name of the tool, and the proxy dispatches on that name. The
//!   executable is therefore never canonicalized before it is run.
//! * the directory the probe runs in. A tool manager can pick another
//!   toolchain for a directory.
//! * the tool the proxy really runs. A proxy keeps the same path and the same
//!   timestamp when the selected toolchain changes, so the identity of the
//!   selected tool is part of the cache of the versions.
//!
//! The absolute path of an executable is never part of the key: it is a
//! property of the machine, not of the repository. A probe that cannot
//! establish the version of a tool fails: a key that hides the version of a
//! tool would hide the changes of that tool.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use crate::Error;

/// How long a tool is given to report its version.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the probe looks at the child process.
const PROBE_TICK: Duration = Duration::from_millis(20);

/// Name of the tool manager whose proxies are recognised.
const RUSTUP: &str = "rustup";

/// Version reported for a tool that is not installed.
pub const ABSENT: &str = "absent";

/// A tool whose version can change the result of a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tool {
    /// Name the tool is looked up under in `PATH`.
    pub name: String,
    /// Version the tool reported.
    pub version: String,
    /// Path of the executable, as it is found in `PATH`.
    pub path: Option<PathBuf>,
    /// Modification time of the executable.
    pub modified: Option<SystemTime>,
    /// Path and modification time of the tool the executable really runs, when
    /// the executable is a proxy.
    pub selected: Option<(PathBuf, Option<SystemTime>)>,
}

/// Detects the tools implied by the lockfiles of a repository.
///
/// The lockfiles are the ones captured by [`crate::key`]; only their file name
/// matters here. The probes run in `cwd`, because a tool manager can pick
/// another toolchain for a directory.
///
/// # Errors
///
/// Returns an error when the version of a tool cannot be established: the
/// caller must not build a key that hides it.
pub fn detect(cwd: &Path, lockfiles: &BTreeMap<String, String>) -> Result<Vec<Tool>, Error> {
    let mut names: BTreeSet<&str> = BTreeSet::new();
    for path in lockfiles.keys() {
        let name = path.rsplit('/').next().unwrap_or(path.as_str());
        for tool in tools_for(name) {
            names.insert(tool);
        }
    }
    names.into_iter().map(|name| probe(cwd, name)).collect()
}

/// Tools whose version can change the result of a command holding `lockfile`.
pub(crate) fn tools_for(lockfile: &str) -> &'static [&'static str] {
    match lockfile {
        "Cargo.lock" => &["cargo", "rustc"],
        "package-lock.json" => &["node", "npm"],
        "pnpm-lock.yaml" => &["node", "pnpm"],
        "yarn.lock" => &["node", "yarn"],
        "bun.lockb" => &["node", "bun"],
        "poetry.lock" => &["python", "poetry"],
        "uv.lock" => &["python", "uv"],
        "go.sum" => &["go"],
        _ => &[],
    }
}

/// Looks up a tool and reads the version it reports.
///
/// # Errors
///
/// Returns an error when the tool does not answer or answers nothing usable.
pub(crate) fn probe(cwd: &Path, name: &str) -> Result<Tool, Error> {
    let Some(path) = resolve(name) else {
        return Ok(Tool {
            name: name.to_string(),
            version: ABSENT.to_string(),
            path: None,
            modified: None,
            selected: None,
        });
    };
    let modified = fs::metadata(&path).and_then(|meta| meta.modified()).ok();
    let selected = selected_identity(cwd, name, &path);
    let cache_key = CacheKey {
        cwd: cwd.to_path_buf(),
        path: path.clone(),
        modified,
        selected: selected.clone(),
    };
    let version = {
        let mut cache = cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(version) = cache.get(&cache_key) {
            version.clone()
        } else {
            let version = run_version(name, &path, cwd, PROBE_TIMEOUT)?;
            cache.insert(cache_key, version.clone());
            version
        }
    };
    Ok(Tool {
        name: name.to_string(),
        version,
        path: Some(path),
        modified,
        selected,
    })
}

/// What the version of a tool was read from.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CacheKey {
    /// Directory the probe ran in.
    cwd: PathBuf,
    /// Executable the probe ran.
    path: PathBuf,
    /// Modification time of the executable.
    modified: Option<SystemTime>,
    /// Tool the executable really runs, when it is a proxy.
    selected: Option<(PathBuf, Option<SystemTime>)>,
}

/// Versions already read, keyed by what they were read from: a rebuilt tool,
/// another directory or another selected toolchain is read again.
type VersionCache = Mutex<BTreeMap<CacheKey, String>>;

/// Cache of the versions already read.
fn cache() -> &'static VersionCache {
    static CACHE: OnceLock<VersionCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Identity of the tool the executable really runs, when it is a proxy.
///
/// A proxy keeps the same path and the same timestamp while the toolchain it
/// selects changes, so the selected tool is part of the identity of the
/// version.
fn selected_identity(cwd: &Path, name: &str, path: &Path) -> Option<(PathBuf, Option<SystemTime>)> {
    let resolved = fs::canonicalize(path).ok()?;
    if resolved.file_name().and_then(OsStr::to_str) != Some(RUSTUP) {
        return None;
    }
    let output = Command::new(RUSTUP)
        .arg("which")
        .arg(name)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let selected = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if selected.is_empty() {
        return None;
    }
    let selected = PathBuf::from(selected);
    let modified = fs::metadata(&selected)
        .and_then(|meta| meta.modified())
        .ok();
    Some((selected, modified))
}

/// Runs the tool and reads the first line it prints.
///
/// The tool is run from the directory of the command and under the name it is
/// called with, so that a tool manager runs the tool the command would run.
///
/// # Errors
///
/// Returns an error when the tool cannot be run, does not answer before the
/// timeout, fails, or prints nothing usable.
fn run_version(name: &str, path: &Path, cwd: &Path, timeout: Duration) -> Result<String, Error> {
    let mut child = Command::new(path)
        .arg(if name == "go" { "version" } else { "--version" })
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| Error::ToolProbe {
            tool: name.to_string(),
            reason: source.to_string(),
        })?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(source) => {
                return Err(Error::ToolProbe {
                    tool: name.to_string(),
                    reason: source.to_string(),
                });
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::ToolProbe {
                tool: name.to_string(),
                reason: "the tool did not answer before the timeout".to_string(),
            });
        }
        std::thread::sleep(PROBE_TICK);
    }
    let output = child
        .wait_with_output()
        .map_err(|source| Error::ToolProbe {
            tool: name.to_string(),
            reason: source.to_string(),
        })?;
    if !output.status.success() {
        return Err(Error::ToolProbe {
            tool: name.to_string(),
            reason: format!("the tool failed with {}", output.status),
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    first_line(&stdout)
        .or_else(|| first_line(&stderr))
        .ok_or_else(|| Error::ToolProbe {
            tool: name.to_string(),
            reason: "the tool printed no version".to_string(),
        })
}

/// First line that holds something.
pub(crate) fn first_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

/// Finds the executable of a tool in `PATH`.
///
/// The path is not canonicalized: a tool manager installs a proxy under the
/// name of the tool, and the proxy dispatches on that name.
pub(crate) fn resolve(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|directory| find_in_directory(&directory, name))
}

/// Finds the executable of a tool in one directory of the `PATH`.
pub(crate) fn find_in_directory(directory: &Path, name: &str) -> Option<PathBuf> {
    if directory.as_os_str().is_empty() {
        return None;
    }
    let candidate = directory.join(name);
    if is_executable(&candidate) {
        return Some(absolute(&candidate));
    }
    #[cfg(windows)]
    {
        for extension in [".exe", ".cmd", ".bat"] {
            let candidate = directory.join(format!("{name}{extension}"));
            if candidate.is_file() {
                return Some(absolute(&candidate));
            }
        }
    }
    None
}

/// Makes a path absolute without resolving the symbolic links.
pub(crate) fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    match std::env::current_dir() {
        Ok(directory) => directory.join(path),
        Err(_) => path.to_path_buf(),
    }
}

/// Tells whether a path is an executable file.
pub(crate) fn is_executable(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
#[path = "tools_tests.rs"]
mod tests;
