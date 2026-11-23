#![cfg(unix)]

#[path = "support/coverage.rs"]
mod coverage;

use kioku::ci2::{
    config::publish_local_default,
    domain::{BlobRef, Digest, Id, RegisteredRoot, ServiceBootstrap, ServiceMode, Validate},
};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
    process::Command,
};

fn probe(linked: bool, test_name: &str) {
    if std::env::var_os("KIOKU_R19_LOCAL_DEFAULT_CHILD").is_none() {
        let home = tempfile::tempdir().unwrap();
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture"])
            .env_clear()
            .envs(coverage::profile_env())
            .env("KIOKU_CI2", home.path())
            .env("KIOKU_R19_LOCAL_DEFAULT_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated public API probe failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let home = PathBuf::from(std::env::var_os("KIOKU_CI2").unwrap());
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
        state_root: home.join("state").to_str().unwrap().into(),
        socket_path: home.join("socket").to_str().unwrap().into(),
        stop_file: home.join("stop").to_str().unwrap().into(),
        registered_roots: vec![RegisteredRoot {
            id: Id::new("root").unwrap(),
            project_id: Id::new("project").unwrap(),
            canonical_path: home.to_str().unwrap().into(),
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
    let directory = home.join(".ci2");
    fs::create_dir(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    let path = directory.join("bootstrap.json");
    if linked {
        let target = home.join("outside-bootstrap.json");
        fs::write(&target, &bytes).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o400)).unwrap();
        symlink(&target, &path).unwrap();
        let result = publish_local_default(&config);
        assert_eq!(
            (
                result.is_err(),
                fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                fs::read(&target).unwrap() == bytes,
                fs::symlink_metadata(&path)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
            ),
            (true, 0o400, true, true),
            "linked local default must be rejected without changing its target"
        );
    } else {
        assert_eq!(publish_local_default(&config).unwrap(), path);
        assert_eq!(publish_local_default(&config).unwrap(), path);
        assert!(fs::symlink_metadata(&path).unwrap().is_file());
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn local_default_rejects_same_bytes_symlink_without_chmod() {
    probe(
        true,
        "local_default_rejects_same_bytes_symlink_without_chmod",
    );
}

#[test]
fn local_default_creates_and_reuses_regular_same_bytes_file() {
    probe(
        false,
        "local_default_creates_and_reuses_regular_same_bytes_file",
    );
}
