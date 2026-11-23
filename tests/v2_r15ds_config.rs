//! R15-DS: public config bootstrap reader must not accept a malformed declared
//! protected-ref shape. Probe only: built against the allocated exported API
//! `kioku::ci2::config::read_bootstrap`, using a private temp directory.
#![forbid(unsafe_code)]

use kioku::ci2::config::{ConfigError, read_bootstrap};
use std::fs;

fn bootstrap_value(
    root: &std::path::Path,
    protected_refs: &serde_json::Value,
) -> serde_json::Value {
    let blob = serde_json::json!({
        "digest": format!("b3:{}", "a".repeat(64)),
        "size_bytes": 17,
        "media_type": "application/json",
    });
    serde_json::json!({
        "schema": "kioku/ci2/bootstrap/v1",
        "revision": 1,
        "service_id": "i1l-runner",
        "mode": "local_cooperative",
        "resource_domain_id": "cotm",
        "state_root": root.join("state"),
        "socket_path": root.join("state/ci.sock"),
        "stop_file": root.join("stop"),
        "registered_roots": [{
            "id": "kioku-local",
            "project_id": "kioku",
            "canonical_path": root,
            "allowed_principals": ["local-operator"],
            "protected_refs": protected_refs,
        }],
        "approved_manifest_refs": [blob.clone()],
        "profiles_ref": blob.clone(),
        "policy_ref": blob,
        "auth_config_ref": null,
        "capture_slots": 1,
        "heavy_slots": 1,
        "max_state_bytes": 1_048_576,
        "max_queue_wait_ms": 1000,
        "drain_grace_ms": 1000,
    })
}

fn write_and_read(root: &std::path::Path, value: &serde_json::Value) -> Result<(), String> {
    let path = root.join("bootstrap.json");
    fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
    match read_bootstrap(&path) {
        Ok(_) => Ok(()),
        Err(ConfigError::Contract(error)) => Err(format!("contract: {error}")),
        Err(other) => Err(format!("other: {other}")),
    }
}

#[test]
fn declared_protected_ref_shape_rejects_names_git_cannot_address() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    // Control: the declared shape with a real git ref name is admitted.
    assert!(
        write_and_read(
            &root,
            &bootstrap_value(&root, &serde_json::json!(["refs/heads/main"]))
        )
        .is_ok()
    );
    for malformed in [
        "refs/",
        "refs/../heads/main",
        "refs/heads/",
        "refs//heads/main",
        "refs/heads/ma\nin",
        "refs/heads/ma in",
        "refs/heads/..",
        "refs/heads/main.lock",
    ] {
        let value = bootstrap_value(&root, &serde_json::json!([malformed]));
        let outcome = write_and_read(&root, &value);
        assert!(
            outcome.is_err(),
            "read_bootstrap accepted malformed protected ref {malformed:?}: {outcome:?}"
        );
    }
}
