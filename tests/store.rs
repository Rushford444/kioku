//! Contract tests of the cache store.
//!
//! The store holds the cached passes of a repository. Two of its decisions
//! matter to a caller: which passes a failed command invalidates, and what a
//! removal reports. These tests pin both, through the public API only.

use std::collections::BTreeMap;

use kioku::store::{CommandIdentity, ENTRY_SCHEMA, Entry, ManifestEntry, Store};

/// The manifest of one input file.
fn manifest(digest: &str) -> BTreeMap<String, ManifestEntry> {
    let mut inputs = BTreeMap::new();
    inputs.insert(
        "src/main.rs".to_string(),
        ManifestEntry {
            digest: digest.to_string(),
            mode: 0o644,
        },
    );
    inputs
}

/// A cached pass of `cargo test`, in the repository `/repo`.
fn entry(key: &str) -> Entry {
    Entry {
        schema: ENTRY_SCHEMA.to_string(),
        key: key.to_string(),
        version: "0.1.0".to_string(),
        argv: vec!["cargo".to_string(), "test".to_string()],
        cwd: String::new(),
        mode: "bytes".to_string(),
        diagnostic_ast_key: None,
        duration_ms: 1_500,
        created_unix_ms: 1_000,
        portable: true,
        repo: "/repo".to_string(),
        inputs: manifest("abc"),
        env: BTreeMap::new(),
        lockfiles: BTreeMap::new(),
        tools: BTreeMap::new(),
    }
}

#[test]
fn an_invalidation_removes_only_the_passes_of_the_same_command() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let store = Store::open(dir.path());

    // The passes of the command that failed, in this repository.
    let mut restricted = entry("ffff");
    restricted.portable = false;
    // The passes that belong to something else.
    let mut other_command = entry("bbbb");
    other_command.argv = vec!["cargo".to_string(), "build".to_string()];
    let mut other_directory = entry("cccc");
    other_directory.cwd = "sub".to_string();
    let mut other_repository = entry("dddd");
    other_repository.repo = "/other".to_string();
    other_repository.portable = false;
    let mut other_inputs = entry("eeee");
    other_inputs.repo = "/other".to_string();
    other_inputs.inputs = manifest("different");

    for stored in [
        entry("aaaa"),
        restricted,
        other_command,
        other_directory,
        other_repository,
        other_inputs,
    ] {
        store
            .write_entry(&stored)
            .expect("the entry must be written");
    }

    let inputs = manifest("abc");
    let identity = CommandIdentity {
        argv: &["cargo".to_string(), "test".to_string()],
        cwd: "",
        repo: "/repo",
        inputs: &inputs,
    };

    assert_eq!(store.invalidate(&identity).expect("invalidation"), 2);
    assert!(store.read_entry("aaaa").expect("read").is_none());
    assert!(store.read_entry("ffff").expect("read").is_none());
    for survivor in ["bbbb", "cccc", "dddd", "eeee"] {
        assert!(
            store.read_entry(survivor).expect("read").is_some(),
            "{survivor} must survive"
        );
    }
}

#[test]
fn a_removal_reports_whether_it_removed_an_entry() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let store = Store::open(dir.path());
    store
        .write_entry(&entry("aaaa"))
        .expect("the entry must be written");

    assert!(
        store
            .remove_entry("aaaa")
            .expect("the removal must succeed")
    );
    assert!(store.read_entry("aaaa").expect("read").is_none());
    assert!(
        !store
            .remove_entry("aaaa")
            .expect("the removal of a missing entry must succeed"),
        "a missing entry is not a removal"
    );
}

#[test]
fn a_removal_refuses_a_key_that_is_not_a_file_name() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let store = Store::open(dir.path());

    assert!(store.remove_entry("../escape").is_err());
    assert!(store.remove_entry("").is_err());
}
