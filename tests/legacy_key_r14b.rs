//! Legacy snapshot portability only; these tests do not exercise traced reuse.

#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::Command;

use kioku::{CaptureOptions, Mode, Snapshot};
use tempfile::TempDir;

fn capture_repository(parent: &Path, name: &str) -> Snapshot {
    let root = parent.join(name);
    fs::create_dir(&root).expect("create fixture repository");
    let status = Command::new("git")
        .args(["init", "-q"])
        .arg(&root)
        .status()
        .expect("initialize fixture repository");
    assert!(status.success());
    fs::write(root.join("check"), b"synthetic command bytes").expect("write command fixture");
    let options = CaptureOptions {
        cwd: root,
        argv: vec!["./check".to_string()],
        mode: Mode::Bytes,
        inputs: Vec::new(),
        env: Vec::new(),
    };
    Snapshot::capture(&options).expect("capture repository without changing its path")
}

fn assert_portable_root(name: &str) {
    let fixture = TempDir::new().expect("create fixture directory");
    let ordinary = capture_repository(fixture.path(), "ordinary");
    let whitespace = capture_repository(fixture.path(), name);
    assert_eq!(ordinary.cwd, ".");
    assert_eq!(whitespace.cwd, ordinary.cwd);
    assert_eq!(whitespace.argv, ordinary.argv);
    assert_eq!(whitespace.inputs, ordinary.inputs);
    assert_eq!(whitespace.key, ordinary.key);
}

#[test]
fn trailing_space_in_repository_root_preserves_portable_key() {
    assert_portable_root("repository ");
}

#[test]
fn trailing_newline_in_repository_root_preserves_portable_key() {
    assert_portable_root("repository\n");
}
