//! Legacy `Snapshot` payload privacy and identity.
//!
//! A declared environment value exists only inside a private child process with
//! an explicit environment. The parent records booleans and keys, never a value.

#![cfg(not(windows))]

#[path = "support/coverage.rs"]
mod coverage;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use kioku::{CaptureOptions, Mode, Snapshot};
use tempfile::TempDir;

const CHILD: &str = "KIOKU_R15DS_ENV_CHILD";
const SENTINEL_NAME: &str = "KIOKU_R15DS_SYNTHETIC";
const SENTINEL_VALUE: &str = "r15ds-synthetic-value-4f1c";
const OTHER_VALUE: &str = "r15ds-synthetic-value-9b27";
const FIXTURE: &str = "KIOKU_R15DS_FIXTURE";

const CASE_PAYLOAD: &str = "legacy_snapshot_payload_carries_no_environment_value";
const CASE_IDENTITY: &str = "legacy_snapshot_key_still_depends_on_the_declared_value";

fn fixture() -> TempDir {
    let directory = TempDir::new().expect("fixture directory");
    let status = Command::new("git")
        .args(["init", "-q"])
        .arg(directory.path())
        .status()
        .expect("initialize fixture repository");
    assert!(status.success(), "git init failed");
    std::fs::write(directory.path().join("check"), b"synthetic command bytes")
        .expect("write command fixture");
    directory
}

fn capture(directory: &Path) -> Snapshot {
    let options = CaptureOptions {
        cwd: directory.to_path_buf(),
        argv: vec!["./check".to_string()],
        mode: Mode::Bytes,
        inputs: Vec::new(),
        env: vec![SENTINEL_NAME.to_string()],
    };
    Snapshot::capture(&options).expect("capture fixture repository")
}

/// Runs `case` in a child that alone holds `value`, and returns its standard
/// output.
fn run_child(case: &str, directory: &Path, value: &str) -> String {
    let probe = Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", case, "--nocapture"])
        .env_clear()
        .envs(coverage::profile_env())
        .env("PATH", "/usr/bin:/bin")
        .env(CHILD, "1")
        .env(FIXTURE, directory)
        .env(SENTINEL_NAME, value)
        .stdin(Stdio::null())
        .output()
        .expect("child test");
    assert!(
        probe.status.success(),
        "child case {case} failed with {:?}",
        probe.status.code()
    );
    String::from_utf8_lossy(&probe.stdout).into_owned()
}

fn child_fixture() -> Option<PathBuf> {
    std::env::var_os(CHILD).map(|_| PathBuf::from(std::env::var_os(FIXTURE).expect("fixture")))
}

fn field<'a>(output: &'a str, name: &str) -> Option<&'a str> {
    output
        .lines()
        .find_map(|line| line.strip_prefix(&format!("PROOF {name}=")))
}

#[test]
fn legacy_snapshot_payload_carries_no_environment_value() {
    if let Some(directory) = child_fixture() {
        let snapshot = capture(&directory);
        let payload = serde_json::to_string(&snapshot).expect("serialize snapshot");
        println!(
            "PROOF declared={}",
            u8::from(snapshot.env.contains_key(SENTINEL_NAME))
        );
        println!(
            "PROOF leaked={}",
            u8::from(payload.contains(SENTINEL_VALUE))
        );
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"kioku/v1/environment-value\0");
        hasher.update(SENTINEL_VALUE.as_bytes());
        let expected = format!("blake3:{}", hasher.finalize().to_hex());
        println!(
            "PROOF digest_matches={}",
            u8::from(snapshot.env.get(SENTINEL_NAME) == Some(&Some(expected)))
        );
        println!("PROOF payload_bytes={}", payload.len());
        return;
    }
    let directory = fixture();
    let output = run_child(CASE_PAYLOAD, directory.path(), SENTINEL_VALUE);
    assert_eq!(field(&output, "declared"), Some("1"), "{output}");
    assert_eq!(
        field(&output, "leaked"),
        Some("0"),
        "a declared environment value reached the serialized snapshot payload"
    );
    assert_eq!(
        field(&output, "digest_matches"),
        Some("1"),
        "the snapshot must record the digest of the captured value"
    );
}

#[test]
fn legacy_snapshot_key_still_depends_on_the_declared_value() {
    if let Some(directory) = child_fixture() {
        println!("PROOF key={}", capture(&directory).key);
        return;
    }
    let first = fixture();
    let second = fixture();
    let key = |directory: &Path, value: &str| {
        field(&run_child(CASE_IDENTITY, directory, value), "key")
            .expect("key of the child snapshot")
            .to_string()
    };
    let first_key = key(first.path(), SENTINEL_VALUE);
    let other_root_same_value = key(second.path(), SENTINEL_VALUE);
    let other_value = key(first.path(), OTHER_VALUE);
    assert_eq!(
        first_key, other_root_same_value,
        "the same declared environment must keep the key portable across roots"
    );
    assert_ne!(
        first_key, other_value,
        "a changed declared environment value must change the key"
    );
}
