use kioku::store::{COUNTERS_SCHEMA, Counters, ENTRY_SCHEMA, Entry, Store};

fn entry() -> Entry {
    serde_json::from_value(serde_json::json!({
        "schema": ENTRY_SCHEMA,
        "key": "public-fixture",
        "version": "0.1.0",
        "argv": ["public-check"],
        "cwd": "",
        "mode": "bytes",
        "duration_ms": 12,
        "created_unix_ms": 1000,
        "portable": false,
        "repo": "/public-fixture",
        "inputs": {},
        "env": {},
        "lockfiles": {},
        "tools": {}
    }))
    .unwrap()
}

#[test]
fn entry_documents_reject_ambiguity_but_preserve_valid_legacy_entries() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path());
    let entry = entry();
    store.write_entry(&entry).unwrap();
    let path = store.entries_dir().join("public-fixture.json");
    let valid = std::fs::read_to_string(&path).unwrap();
    let read = store.read_entry(&entry.key).unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(read).unwrap(),
        serde_json::to_value(&entry).unwrap()
    );

    for extra in [
        r#""schema":"kioku/entry/v0","#,
        r#""unsupported_observation":true,"#,
    ] {
        let raw = valid.replacen('{', &format!("{{{extra}"), 1);
        std::fs::write(&path, raw).unwrap();
        assert!(
            store.read_entry(&entry.key).is_err(),
            "ambiguous entry accepted: {extra}"
        );
        let list = store.list_entries().unwrap();
        assert!(list.entries.is_empty());
        assert_eq!(list.invalid, 1);
    }
    std::fs::write(&path, valid).unwrap();
    assert!(store.read_entry(&entry.key).unwrap().is_some());
}

#[test]
fn malformed_counters_never_reset_existing_history() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path());
    let initial = Counters {
        schema: COUNTERS_SCHEMA.to_owned(),
        hits: 11,
        misses: 12,
        executions: 13,
        bypassed: 14,
        saved_ms: 15,
        invalidated: 16,
    };
    assert_eq!(
        serde_json::to_value(store.read_counters().unwrap()).unwrap(),
        serde_json::to_value(Counters::default()).unwrap()
    );
    store
        .update_counters(|counts| *counts = initial.clone())
        .unwrap();
    let path = store.root().join("counters.json");
    let valid = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        serde_json::to_value(store.read_counters().unwrap()).unwrap(),
        serde_json::to_value(&initial).unwrap()
    );

    for malformed in [
        valid[..valid.len() - 1].to_owned(),
        valid.replace(COUNTERS_SCHEMA, "kioku/counters/v0"),
    ] {
        std::fs::write(&path, &malformed).unwrap();
        let read = store.read_counters();
        let mut called = false;
        let update = store.update_counters(|counts| {
            called = true;
            counts.hits += 1;
        });
        assert!(
            read.is_err() && update.is_err(),
            "invalid counters were accepted or overwritten"
        );
        assert!(
            !called,
            "invalid history must be rejected before the update"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), malformed);
    }
    std::fs::write(&path, valid).unwrap();
    let updated = store.update_counters(|counts| counts.hits += 1).unwrap();
    let expected = Counters {
        hits: initial.hits + 1,
        ..initial
    };
    assert_eq!(
        serde_json::to_value(updated).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
    assert_eq!(
        serde_json::to_value(store.read_counters().unwrap()).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
}

#[test]
fn conflicting_digests_for_one_input_path_are_not_readable_entries() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path());
    let mut entry = entry();
    entry.inputs.insert(
        "src/main.rs".to_owned(),
        kioku::store::ManifestEntry {
            digest: "a".repeat(64),
            mode: 0o644,
        },
    );
    store.write_entry(&entry).unwrap();
    let path = store.entries_dir().join("public-fixture.json");
    let valid = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        store.read_entry(&entry.key).unwrap().unwrap().inputs,
        entry.inputs
    );

    // The last digest matches the valid entry; a map must not discard the
    // conflicting first record, including an escaped spelling of the path.
    for (first_path, first_digest) in [
        (r"src/main.rs", "b".repeat(64)),
        (r"src/ma\u0069n.rs", "b".repeat(64)),
        (r"src/main.rs", "a".repeat(64)),
    ] {
        let duplicate =
            format!(r#""inputs": {{"{first_path}": {{"digest":"{first_digest}","mode":420}},"#);
        let ambiguous = valid.replacen(r#""inputs": {"#, &duplicate, 1);
        std::fs::write(&path, &ambiguous).unwrap();
        assert!(
            store.read_entry(&entry.key).is_err(),
            "conflicting path digests were silently collapsed"
        );
        let list = store.list_entries().unwrap();
        assert!(list.entries.is_empty());
        assert_eq!(list.invalid, 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), ambiguous);
    }
    std::fs::write(&path, valid).unwrap();
    assert_eq!(
        store.read_entry(&entry.key).unwrap().unwrap().inputs,
        entry.inputs
    );
}
