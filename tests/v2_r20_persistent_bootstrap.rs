#![cfg(unix)]

use kioku::ci2::{
    config::{persist_bootstrap, read_bootstrap},
    domain::{BlobRef, Digest, Id, RegisteredRoot, ServiceBootstrap, ServiceMode, Validate},
};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
};

#[test]
fn durable_bootstrap_rejects_same_bytes_symlink_and_reuses_regular_file() {
    let home = tempfile::tempdir().unwrap();
    fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let reference = BlobRef {
        digest: Digest::blake3("fixture", b"content").unwrap(),
        size_bytes: 7,
        media_type: "application/json".into(),
    };
    let config = ServiceBootstrap {
        schema: "kioku/ci2/bootstrap/v1".into(),
        revision: 1,
        service_id: Id::new("service").unwrap(),
        mode: ServiceMode::LocalCooperative,
        resource_domain_id: Id::new("domain").unwrap(),
        state_root: home.path().join("state").to_str().unwrap().into(),
        socket_path: home.path().join("socket").to_str().unwrap().into(),
        stop_file: home.path().join("stop").to_str().unwrap().into(),
        registered_roots: vec![RegisteredRoot {
            id: Id::new("root").unwrap(),
            project_id: Id::new("project").unwrap(),
            canonical_path: home.path().to_str().unwrap().into(),
            allowed_principals: vec![Id::new("alice").unwrap()],
            protected_refs: vec![],
        }],
        approved_manifest_refs: vec![reference.clone()],
        profiles_ref: reference.clone(),
        policy_ref: reference,
        auth_config_ref: None,
        capture_slots: 1,
        heavy_slots: 1,
        max_state_bytes: 1_000_000,
        max_queue_wait_ms: 1,
        drain_grace_ms: 1,
    };
    config.validate().unwrap();
    let bytes = serde_json::to_vec(&config).unwrap();
    let path = home.path().join("state/bootstrap.json");
    assert_eq!(persist_bootstrap(&config).unwrap(), path);
    assert_eq!(persist_bootstrap(&config).unwrap(), path);
    assert!(fs::symlink_metadata(&path).unwrap().is_file());
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(read_bootstrap(&path).is_ok());

    fs::remove_file(&path).unwrap();
    let target = home.path().join("outside-bootstrap.json");
    fs::write(&target, &bytes).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o400)).unwrap();
    symlink(&target, &path).unwrap();
    assert!(read_bootstrap(&path).is_err());
    let persisted = persist_bootstrap(&config);
    assert_eq!(fs::read(&target).unwrap(), bytes);
    assert_eq!(
        fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o400
    );
    assert!(
        fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        persisted.is_err(),
        "durable bootstrap must not accept a symlink alias"
    );
}
