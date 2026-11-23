#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn fixture(exit: u8) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("node_modules/.bin");
    fs::create_dir_all(&bin).unwrap();
    let vitest = bin.join("vitest");
    fs::write(
        &vitest,
        format!("#!/bin/sh\nprintf '%s' \"$*\" > called\nexit {exit}\n"),
    )
    .unwrap();
    fs::set_permissions(vitest, fs::Permissions::from_mode(0o755)).unwrap();
    root
}

#[test]
fn full_fallback_is_incomplete_and_does_not_filter() {
    let root = fixture(0);
    let output = Command::new(env!("CARGO_BIN_EXE_kioku"))
        .args(["--json", "test", "--runner", "vitest", "--repo"])
        .arg(root.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        fs::read_to_string(root.path().join("called")).unwrap(),
        "run"
    );
    let event: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(event["status"], "incomplete");
    assert_eq!(event["scope"], "full");
}

#[test]
fn failure_never_becomes_green_and_selection_flags_are_rejected() {
    let root = fixture(5);
    let run = || Command::new(env!("CARGO_BIN_EXE_kioku"));
    let status = run()
        .args(["test", "--runner", "vitest", "--repo"])
        .arg(root.path())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(1));
    fs::remove_file(root.path().join("called")).unwrap();
    let status = run()
        .args(["test", "--runner", "vitest", "--repo"])
        .arg(root.path())
        .args(["-E", "anything"])
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(2));
    assert!(!root.path().join("called").exists());
}

#[test]
fn interrupted_runner_is_not_a_success() {
    let root = fixture(0);
    fs::write(
        root.path().join("node_modules/.bin/vitest"),
        "#!/bin/sh\nprintf run > called\nkill -TERM $$\n",
    )
    .unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_kioku"))
        .args(["test", "--runner", "vitest", "--repo"])
        .arg(root.path())
        .status()
        .unwrap();
    assert_eq!(
        fs::read_to_string(root.path().join("called")).unwrap(),
        "run"
    );
    assert_eq!(
        status.code(),
        Some(143),
        "a runner killed by SIGTERM is 128 + 15, not the incomplete success 3"
    );
}

#[test]
fn missing_local_runner_is_incomplete() {
    let root = tempfile::tempdir().unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_kioku"))
        .args(["test", "--runner", "vitest", "--repo"])
        .arg(root.path())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(3));
}

#[test]
fn unavailable_base_preparation_does_not_modify_existing_store() {
    let root = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let existing = store.path().join("vitest-base.json");
    fs::write(&existing, b"existing data").unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_kioku"))
        .args(["select", "base", "--runner", "vitest", "--repo"])
        .arg(root.path())
        .arg("--store")
        .arg(store.path())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(3));
    assert_eq!(fs::read(existing).unwrap(), b"existing data");
}
