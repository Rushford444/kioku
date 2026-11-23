//! Linux-only, descriptor-anchored capture of explicitly supplied inputs.
//! This is not a Git inventory or a proof of transitive tool closure.
//!
//! On other targets only the `capture` stub below is reachable, so the helpers
//! and imports that serve the Linux path are dead code there. The allowance is
//! scoped to non-Linux builds: the Linux build is unchanged.
#![cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, unused_imports, clippy::missing_errors_doc)
)]
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

#[cfg(target_os = "linux")]
use std::os::unix::{
    fs::{MetadataExt, OpenOptionsExt},
    io::AsRawFd,
};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CapturedFile {
    pub digest: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Snapshot {
    pub files: BTreeMap<String, CapturedFile>,
    pub directories: BTreeSet<String>,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn excluded(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "qa"
        || lower == ".git"
        || lower.starts_with(".env")
        || [".sql", ".dump", ".sqlite", ".sqlite3", ".db"]
            .iter()
            .any(|suffix| lower.ends_with(suffix))
}

fn components(path: &Path) -> io::Result<Vec<&str>> {
    let text = path.to_str().ok_or_else(|| invalid("non-UTF8 input"))?;
    if text
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(invalid("non-canonical input"));
    }
    let mut parts = Vec::new();
    for component in path.components() {
        let Component::Normal(value) = component else {
            return Err(invalid("non-relative input"));
        };
        let text = value.to_str().ok_or_else(|| invalid("non-UTF8 input"))?;
        if excluded(text) {
            return Err(invalid("excluded input"));
        }
        parts.push(text);
    }
    if parts.is_empty() {
        return Err(invalid("empty input"));
    }
    Ok(parts)
}

#[cfg(target_os = "linux")]
#[derive(Clone, PartialEq, Eq)]
struct Identity {
    dev: u64,
    ino: u64,
    size: u64,
    mode: u32,
    mtime: i64,
    mtime_ns: i64,
    ctime: i64,
    ctime_ns: i64,
}
#[cfg(target_os = "linux")]
fn identity(m: &Metadata) -> Identity {
    Identity {
        dev: m.dev(),
        ino: m.ino(),
        size: m.size(),
        mode: m.mode(),
        mtime: m.mtime(),
        mtime_ns: m.mtime_nsec(),
        ctime: m.ctime(),
        ctime_ns: m.ctime_nsec(),
    }
}

#[cfg(target_os = "linux")]
fn anchored(parent: &File, name: &str, directory: bool) -> io::Result<File> {
    let path = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd())).join(name);
    let mut options = OpenOptions::new();
    options.read(true).custom_flags(
        libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | if directory {
                libc::O_DIRECTORY
            } else {
                libc::O_NONBLOCK
            },
    );
    options.open(path)
}

#[cfg(target_os = "linux")]
struct Capture {
    // Retain bounded identities, not descriptors. Final checks reopen anchored paths.
    observed: BTreeMap<String, Identity>,
    files: BTreeMap<String, CapturedFile>,
    directories: BTreeSet<String>,
}
#[cfg(target_os = "linux")]
impl Capture {
    const MAX_DEPTH: usize = 64;
    const MAX_OBSERVED: usize = 4096;
    fn retain(&mut self, path: &str, before: Identity) -> io::Result<()> {
        if let Some(previous) = self.observed.get(path) {
            if *previous != before {
                return Err(invalid("input changed between observations"));
            }
        } else {
            if self.observed.len() >= Self::MAX_OBSERVED {
                return Err(invalid("input identity limit exceeded"));
            }
            self.observed.insert(path.to_owned(), before);
        }
        Ok(())
    }
    fn verify(&self, root: &File) -> io::Result<()> {
        for (path, expected) in &self.observed {
            let parts = components(Path::new(path))?;
            let mut parent = root.try_clone()?;
            let mut relative = String::new();
            for (index, name) in parts.iter().enumerate() {
                if !relative.is_empty() {
                    relative.push('/');
                }
                relative.push_str(name);
                let before = self
                    .observed
                    .get(&relative)
                    .ok_or_else(|| invalid("unobserved ancestor"))?;
                let directory =
                    index + 1 < parts.len() || before.mode & libc::S_IFMT == libc::S_IFDIR;
                let next = anchored(&parent, name, directory)?;
                if identity(&next.metadata()?) != *before {
                    return Err(invalid("input path changed after capture"));
                }
                parent = next;
            }
            if identity(&parent.metadata()?) != *expected {
                return Err(invalid("input changed during revalidation"));
            }
        }
        Ok(())
    }
    fn walk(&mut self, parent: &File, name: &str, relative: &str) -> io::Result<()> {
        if excluded(name) {
            return Err(invalid("excluded input"));
        }
        if relative.split('/').count() > Self::MAX_DEPTH {
            return Err(invalid("input depth limit exceeded"));
        }
        let proc_path = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd())).join(name);
        let initial = fs::symlink_metadata(&proc_path)?;
        if initial.file_type().is_symlink() {
            return Err(invalid("symlink input"));
        }
        let directory = initial.is_dir();
        if initial.is_file() && initial.nlink() != 1 {
            return Err(invalid("hardlinked input"));
        }
        if !directory && !initial.is_file() {
            return Err(invalid("unsupported input kind"));
        }
        let mut file = anchored(parent, name, directory)?;
        let before = identity(&file.metadata()?);
        if before != identity(&initial) {
            return Err(invalid("input changed while opening"));
        }
        self.retain(relative, before.clone())?;
        if directory {
            if !self.directories.insert(relative.to_owned()) {
                return Err(invalid("overlapping inputs"));
            }
            let path = format!("/proc/self/fd/{}", file.as_raw_fd());
            let mut names = Vec::new();
            for entry in fs::read_dir(path)? {
                let entry = entry?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| invalid("non-UTF8 input"))?;
                if names.len() >= Self::MAX_OBSERVED {
                    return Err(invalid("directory entry limit exceeded"));
                }
                names.push(name);
            }
            names.sort_unstable();
            for child in names {
                self.walk(&file, &child, &format!("{relative}/{child}"))?;
            }
        } else {
            let mut hasher = blake3::Hasher::new();
            hasher.update(b"kioku/ci2/workspace-file/v1\0");
            let mut size = 0_u64;
            let mut buffer = vec![0_u8; 65536].into_boxed_slice();
            loop {
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                size = size
                    .checked_add(u64::try_from(count).map_err(|_| invalid("file too large"))?)
                    .ok_or_else(|| invalid("file too large"))?;
                hasher.update(&buffer[..count]);
            }
            if before.size != size {
                return Err(invalid("input size changed"));
            }
            let item = CapturedFile {
                digest: format!("b3:{}", hasher.finalize().to_hex()),
                bytes: size,
            };
            if self.files.insert(relative.to_owned(), item).is_some() {
                return Err(invalid("overlapping inputs"));
            }
        }
        if before != identity(&file.metadata()?) {
            return Err(invalid("input changed during capture"));
        }
        self.retain(relative, before)
    }
}

/// Capture caller-supplied inputs; excluded, missing, racing and symlink inputs fail closed.
///
/// # Errors
/// Returns typed I/O errors for unvalidated shapes, symlinks or mutations. This is not a transitive closure proof.
/// The caller must supply and qualify a complete inventory, including tracked and generated files.
#[cfg(target_os = "linux")]
pub fn capture(root: &Path, inputs: &[PathBuf]) -> io::Result<Snapshot> {
    if inputs.is_empty() {
        return Err(invalid("empty inventory"));
    }
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let root_path = root;
    let root = options.open(root_path)?;
    let root_identity = identity(&root.metadata()?);
    let proc_root = format!("/proc/self/fd/{}", root.as_raw_fd());
    if root_identity != identity(&fs::metadata(proc_root)?) {
        return Err(invalid("proc fd identity mismatch"));
    }
    let mut state = Capture {
        observed: BTreeMap::new(),
        files: BTreeMap::new(),
        directories: BTreeSet::new(),
    };
    for path in inputs {
        let parts = components(path)?;
        let mut parent = root.try_clone()?;
        if parts.len() > Capture::MAX_DEPTH {
            return Err(invalid("input depth limit exceeded"));
        }
        let mut relative = String::new();
        for name in &parts[..parts.len() - 1] {
            if !relative.is_empty() {
                relative.push('/');
            }
            relative.push_str(name);
            let next = anchored(&parent, name, true)?;
            state.retain(&relative, identity(&next.metadata()?))?;
            parent = next;
        }
        state.walk(&parent, parts[parts.len() - 1], &parts.join("/"))?;
    }
    if root_identity != identity(&root.metadata()?) {
        return Err(invalid("root changed during capture"));
    }
    state.verify(&root)?;
    // A retained descriptor can stay unchanged while an ancestor path is renamed.
    let current_root = options.open(root_path)?;
    if root_identity != identity(&current_root.metadata()?) {
        return Err(invalid("root path changed during capture"));
    }
    Ok(Snapshot {
        files: state.files,
        directories: state.directories,
    })
}

#[cfg(not(target_os = "linux"))]
pub fn capture(_root: &Path, _inputs: &[PathBuf]) -> io::Result<Snapshot> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "descriptor anchored capture requires Linux",
    ))
}

#[cfg(all(test, target_os = "linux"))]
mod bounded_tests {
    use super::*;
    #[test]
    fn complete_hundred_file_inventory_is_stable() {
        let root = tempfile::tempdir().unwrap();
        let tree = root.path().join("src");
        fs::create_dir(&tree).unwrap();
        let mut paths = Vec::new();
        for index in 0..100 {
            let name = format!("src/file-{index}.rs");
            fs::write(
                root.path().join(&name),
                format!("const VALUE: u64 = {index};"),
            )
            .unwrap();
            paths.push(PathBuf::from(name));
        }
        let first = capture(root.path(), &paths).unwrap();
        assert_eq!(first.files.len(), 100);
        assert_eq!(first, capture(root.path(), &paths).unwrap());
        assert_eq!(
            first.files,
            capture(root.path(), &[PathBuf::from("src")]).unwrap().files
        );
    }
    #[test]
    fn ancestor_replacement_is_not_a_matching_capture() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("dir")).unwrap();
        fs::write(root.path().join("dir/input"), b"one").unwrap();
        let handle = File::open(root.path()).unwrap();
        let mut captured = Capture {
            observed: BTreeMap::new(),
            files: BTreeMap::new(),
            directories: BTreeSet::new(),
        };
        captured.walk(&handle, "dir", "dir").unwrap();
        captured.verify(&handle).unwrap();
        fs::rename(root.path().join("dir"), root.path().join("old")).unwrap();
        fs::create_dir(root.path().join("dir")).unwrap();
        fs::write(root.path().join("dir/input"), b"one").unwrap();
        assert!(captured.verify(&handle).is_err());
    }
    #[test]
    fn identity_and_depth_limits_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("input"), b"one").unwrap();
        let identity = identity(&fs::metadata(root.path().join("input")).unwrap());
        let mut captured = Capture {
            observed: BTreeMap::new(),
            files: BTreeMap::new(),
            directories: BTreeSet::new(),
        };
        for index in 0..Capture::MAX_OBSERVED {
            captured
                .retain(&format!("file-{index}"), identity.clone())
                .unwrap();
        }
        assert!(captured.retain("extra", identity).is_err());
        let deep = vec!["d"; Capture::MAX_DEPTH + 1].join("/");
        assert!(capture(root.path(), &[PathBuf::from(deep)]).is_err());
    }
    #[test]
    fn final_revalidation_refuses_replacement_and_symlink() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("input"), b"one").unwrap();
        let handle = File::open(root.path()).unwrap();
        let mut captured = Capture {
            observed: BTreeMap::new(),
            files: BTreeMap::new(),
            directories: BTreeSet::new(),
        };
        captured.walk(&handle, "input", "input").unwrap();
        captured.verify(&handle).unwrap();
        fs::rename(root.path().join("input"), root.path().join("old")).unwrap();
        fs::write(root.path().join("input"), b"one").unwrap();
        assert!(captured.verify(&handle).is_err());
        std::os::unix::fs::symlink(root.path().join("old"), root.path().join("link")).unwrap();
        assert!(capture(root.path(), &[PathBuf::from("link")]).is_err());
        fs::hard_link(root.path().join("input"), root.path().join("hardlink")).unwrap();
        assert!(capture(root.path(), &[PathBuf::from("hardlink")]).is_err());
    }
}
