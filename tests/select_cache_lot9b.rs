//! Cooperative preparation cache: no selection or result reuse.
use kioku::select::cache::{CacheError, CacheKey, InputSnapshot, LocalCooperativeStore};
use std::collections::BTreeMap;
use std::fs;

fn key() -> CacheKey {
    CacheKey {
        repository: "/cooperative/fixture".into(),
        commit: "a".repeat(40),
        input_snapshot: InputSnapshot {
            full_tree_sha256: "b".repeat(64),
            input_closure_sha256: "c".repeat(64),
        },
        toolchain: BTreeMap::from([("rustc".into(), "fixture-toolchain-v1".into())]),
        preparation_options: BTreeMap::from([("operation".into(), "graph-fixture-v1".into())]),
        store_options: BTreeMap::from([("format".into(), "fixture-v1".into())]),
    }
}

#[test]
fn sealed_preparation_is_copied_and_only_usable_before_tests() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalCooperativeStore::open(dir.path().join("store")).unwrap();
    let entry = store
        .prepare(&key(), |out| {
            fs::write(out.join("graph"), b"complete graph")?;
            Ok(())
        })
        .unwrap();
    let reused = store.prepare(&key(), |_| panic!("prepared twice")).unwrap();
    assert_eq!(entry.identity(), reused.identity());
    let mut copy = reused.private_copy(dir.path().join("private")).unwrap();
    copy.with_before_test_outputs(|out| {
        assert_eq!(fs::read(out.join("graph"))?, b"complete graph");
        Ok(())
    })
    .unwrap();
    copy.run_tests(|out| {
        assert_eq!(fs::read(out.join("graph"))?, b"complete graph");
        Ok(())
    })
    .unwrap();
    assert!(matches!(
        copy.with_before_test_outputs(|_| Ok(())),
        Err(CacheError::AfterTest)
    ));
}

#[test]
fn cooperative_child() {
    let Some(root) = std::env::var_os("KIOKU_LOT9B_CHILD") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let store = LocalCooperativeStore::open(root.join("store")).unwrap();
    let entry = store
        .prepare(&key(), |out| {
            use std::io::Write;
            let mut calls = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(root.join("calls"))?;
            writeln!(calls, "producer")?;
            std::thread::sleep(std::time::Duration::from_millis(150));
            fs::write(out.join("graph"), b"joined preparation")?;
            Ok(())
        })
        .unwrap();
    let private = entry
        .private_copy(root.join(format!("private-{}", std::process::id())))
        .unwrap();
    private
        .with_before_test_outputs(|out| {
            assert_eq!(fs::read(out.join("graph"))?, b"joined preparation");
            Ok(())
        })
        .unwrap();
}

#[test]
fn separate_processes_share_one_preparation() {
    let dir = tempfile::tempdir().unwrap();
    let executable = std::env::current_exe().unwrap();
    let mut children: Vec<_> = (0..4)
        .map(|_| {
            std::process::Command::new(&executable)
                .args(["--exact", "cooperative_child"])
                .env("KIOKU_LOT9B_CHILD", dir.path())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in &mut children {
        assert!(child.wait().unwrap().success());
    }
    assert_eq!(
        fs::read_to_string(dir.path().join("calls"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn corruption_freezes_generation_and_revokes_existing_copies() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    let store = LocalCooperativeStore::open(&root).unwrap();
    let entry = store
        .prepare(&key(), |out| {
            fs::write(out.join("graph"), b"good")?;
            Ok(())
        })
        .unwrap();
    let copy = entry.private_copy(dir.path().join("copy")).unwrap();
    let manifest = root.join(entry.identity()).join("ready/manifest.json");
    fs::write(manifest, b"corrupt").unwrap();
    assert!(copy.with_before_test_outputs(|_| Ok(())).is_err());
    assert!(matches!(
        store.prepare(&key(), |_| Ok(())),
        Err(CacheError::Frozen(_))
    ));
}

#[test]
fn failed_producer_freezes_without_retry_and_keys_are_separate() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalCooperativeStore::open(dir.path().join("store")).unwrap();
    assert!(
        store
            .prepare(&key(), |_| Err(std::io::Error::other("failed").into()))
            .is_err()
    );
    assert!(matches!(
        store.prepare(&key(), |_| panic!("must not retry")),
        Err(CacheError::Frozen(_))
    ));
    let mut other = key();
    other.input_snapshot.full_tree_sha256 = "d".repeat(64);
    let entry = store
        .prepare(&other, |out| {
            fs::write(out.join("graph"), b"other")?;
            Ok(())
        })
        .unwrap();
    assert!(entry.private_copy(dir.path().join("other")).is_ok());
}

#[test]
fn private_copies_are_independent_and_test_start_is_final_on_error() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalCooperativeStore::open(dir.path().join("store")).unwrap();
    let entry = store
        .prepare(&key(), |out| {
            fs::write(out.join("graph"), b"graph")?;
            Ok(())
        })
        .unwrap();
    let mut first = entry.private_copy(dir.path().join("first")).unwrap();
    let second = entry.private_copy(dir.path().join("second")).unwrap();
    assert!(
        first
            .run_tests::<()>(|_| Err(std::io::Error::other("test failed").into()))
            .is_err()
    );
    assert!(matches!(
        first.run_tests(|_| Ok(())),
        Err(CacheError::AfterTest)
    ));
    second
        .with_before_test_outputs(|out| {
            assert_eq!(fs::read(out.join("graph"))?, b"graph");
            Ok(())
        })
        .unwrap();
}

#[test]
fn producer_joins_writer_before_publication() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalCooperativeStore::open(dir.path().join("store")).unwrap();
    let entry = store
        .prepare(&key(), |out| {
            let path = out.join("graph");
            std::thread::spawn(move || fs::write(path, b"complete"))
                .join()
                .map_err(|_| std::io::Error::other("writer panicked"))??;
            Ok(())
        })
        .unwrap();
    let copy = entry.private_copy(dir.path().join("copy")).unwrap();
    copy.with_before_test_outputs(|out| {
        assert_eq!(fs::read(out.join("graph"))?, b"complete");
        Ok(())
    })
    .unwrap();
}

#[test]
fn missing_generation_is_frozen_instead_of_reprepared() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    let store = LocalCooperativeStore::open(&root).unwrap();
    let entry = store
        .prepare(&key(), |out| {
            fs::write(out.join("graph"), b"graph")?;
            Ok(())
        })
        .unwrap();
    fs::remove_dir_all(root.join(entry.identity()).join("ready")).unwrap();
    assert!(matches!(
        store.prepare(&key(), |_| panic!("must not retry")),
        Err(CacheError::Frozen(_))
    ));
}

#[test]
fn interrupted_initialization_without_marker_freezes_generation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    let store = LocalCooperativeStore::open(&root).unwrap();
    let entry = store
        .prepare(&key(), |out| {
            fs::write(out.join("graph"), b"graph")?;
            Ok(())
        })
        .unwrap();
    let generation = root.join(entry.identity());
    fs::remove_dir_all(generation.join("ready")).unwrap();
    fs::remove_file(generation.join("STARTED")).unwrap();
    assert!(matches!(
        store.prepare(&key(), |_| panic!("must not retry")),
        Err(CacheError::Frozen(_))
    ));
}

#[test]
fn missing_private_output_freezes_generation() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalCooperativeStore::open(dir.path().join("store")).unwrap();
    let entry = store
        .prepare(&key(), |out| {
            fs::write(out.join("graph"), b"graph")?;
            Ok(())
        })
        .unwrap();
    let copy = entry.private_copy(dir.path().join("copy")).unwrap();
    fs::remove_file(dir.path().join("copy/outputs/graph")).unwrap();
    assert!(matches!(
        copy.with_before_test_outputs(|_| Ok(())),
        Err(CacheError::Frozen(_))
    ));
    assert!(matches!(
        store.prepare(&key(), |_| panic!("must not retry")),
        Err(CacheError::Frozen(_))
    ));
}

fn other_key() -> CacheKey {
    let mut other = key();
    other.repository = "/cooperative/other".into();
    other.input_snapshot.full_tree_sha256 = "e".repeat(64);
    other.input_snapshot.input_closure_sha256 = "f".repeat(64);
    other
}

fn key_json(key: &CacheKey) -> String {
    serde_json::to_string(key).unwrap()
}

fn manifest_for(key: &CacheKey, extra_body: &str) -> String {
    format!(
        r#"{{"body":{{"schema":"kioku/cooperative-preparation/v1","key":{},"phase":"before-test","outputs":{{"graph":{{"sha256":"{}","bytes":5,"executable":0}}}}{extra_body}}},"sha256":"{}"}}"#,
        key_json(key),
        "0".repeat(64),
        "1".repeat(64)
    )
}

#[test]
fn mutated_or_unreadable_manifest_freezes_generation() {
    let variants: [&str; 4] = [
        // valid JSON naming another complete key: identity mismatch, not a parse error
        "other-key",
        // valid JSON with an unknown field inside the body
        "unknown-field",
        // truncated JSON
        "truncated",
        // not JSON at all
        "garbage",
    ];
    for variant in variants {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");
        let store = LocalCooperativeStore::open(&root).unwrap();
        let entry = store
            .prepare(&key(), |out| {
                fs::write(out.join("graph"), b"graph")?;
                Ok(())
            })
            .unwrap();
        let path = root.join(entry.identity()).join("ready/manifest.json");
        let contents: String = match variant {
            "other-key" => manifest_for(&other_key(), ""),
            "unknown-field" => manifest_for(&key(), r#","unknown":true"#),
            "truncated" => manifest_for(&key(), "").chars().take(40).collect(),
            _ => "not json".into(),
        };
        fs::write(&path, contents).unwrap();
        assert!(
            entry.private_copy(dir.path().join("copy")).is_err(),
            "{variant}: mutated manifest must deny consumption"
        );
        assert!(
            matches!(
                store.prepare(&key(), |_| panic!("must not reprepare")),
                Err(CacheError::Frozen(_))
            ),
            "{variant}: generation must stay frozen"
        );
    }
}

#[test]
fn manifest_of_another_complete_key_freezes_only_its_generation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    let store = LocalCooperativeStore::open(&root).unwrap();
    let entry = store
        .prepare(&key(), |out| {
            fs::write(out.join("graph"), b"graph")?;
            Ok(())
        })
        .unwrap();
    let path = root.join(entry.identity()).join("ready/manifest.json");
    fs::write(&path, manifest_for(&other_key(), "")).unwrap();
    assert!(matches!(
        entry.private_copy(dir.path().join("copy")),
        Err(CacheError::Frozen(_))
    ));
    assert!(matches!(
        store.prepare(&key(), |_| panic!("must not reprepare")),
        Err(CacheError::Frozen(_))
    ));
    // No store-wide state: the other key still has its own prepared generation.
    let other = store
        .prepare(&other_key(), |out| {
            fs::write(out.join("graph"), b"other")?;
            Ok(())
        })
        .unwrap();
    let other_copy = other.private_copy(dir.path().join("other")).unwrap();
    other_copy
        .with_before_test_outputs(|out| {
            assert_eq!(fs::read(out.join("graph"))?, b"other");
            Ok(())
        })
        .unwrap();
    // Reuse, not reprepare: the producer must not run for an existing generation.
    store
        .prepare(&other_key(), |_| panic!("must not reprepare"))
        .unwrap();
}

#[cfg(unix)]
#[test]
fn symlink_output_is_refused_and_freezes_generation() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalCooperativeStore::open(dir.path().join("store")).unwrap();
    let outside = dir.path().join("outside");
    fs::write(&outside, b"outside").unwrap();
    assert!(
        store
            .prepare(&key(), |out| {
                std::os::unix::fs::symlink(&outside, out.join("link"))?;
                Ok(())
            })
            .is_err()
    );
    assert!(matches!(
        store.prepare(&key(), |_| panic!("must not retry")),
        Err(CacheError::Frozen(_))
    ));
}

#[test]
fn poison_revokes_every_existing_private_copy() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalCooperativeStore::open(dir.path().join("store")).unwrap();
    let entry = store
        .prepare(&key(), |out| {
            fs::write(out.join("graph"), b"graph")?;
            Ok(())
        })
        .unwrap();
    let first = entry.private_copy(dir.path().join("first")).unwrap();
    let mut second = entry.private_copy(dir.path().join("second")).unwrap();
    assert!(matches!(entry.poison(), Err(CacheError::Frozen(_))));
    assert!(matches!(
        first.with_before_test_outputs(|_| Ok(())),
        Err(CacheError::Frozen(_))
    ));
    assert!(matches!(
        second.run_tests(|_| Ok(())),
        Err(CacheError::Frozen(_))
    ));
    assert!(matches!(
        entry.private_copy(dir.path().join("third")),
        Err(CacheError::Frozen(_))
    ));
    assert!(matches!(
        store.prepare(&key(), |_| panic!("must not prepare")),
        Err(CacheError::Frozen(_))
    ));
}

#[test]
fn distinct_keys_are_independent_generations() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalCooperativeStore::open(dir.path().join("store")).unwrap();
    let first = store
        .prepare(&key(), |out| {
            fs::write(out.join("graph"), b"first")?;
            Ok(())
        })
        .unwrap();
    let second = store
        .prepare(&other_key(), |out| {
            fs::write(out.join("graph"), b"second")?;
            Ok(())
        })
        .unwrap();
    assert_ne!(first.identity(), second.identity());
    for (entry, expected, name) in [
        (&first, b"first".as_slice(), "a"),
        (&second, b"second".as_slice(), "b"),
    ] {
        entry
            .private_copy(dir.path().join(name))
            .unwrap()
            .with_before_test_outputs(|out| {
                assert_eq!(fs::read(out.join("graph"))?, expected);
                Ok(())
            })
            .unwrap();
    }
}
