#![cfg(target_os = "linux")]

use kioku::ci2::{
    ProcessIdentity,
    native::{AbsenceScope, read_absence_proof},
    storage::ProcessEvidence,
};
use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, path::Path};

fn checkpoint(work: &Path) -> (ProcessEvidence, Value) {
    let evidence = ProcessEvidence {
        process: ProcessIdentity {
            pid: 42002,
            pgid: 42002,
            started_ms: 1000,
        },
        start_ticks: 100,
        boot_id: fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .unwrap()
            .trim()
            .into(),
    };
    let streams: Vec<_> = ["stdout", "stderr"]
        .into_iter()
        .map(|name| {
            let path = work.join(name);
            fs::write(&path, b"").unwrap();
            json!({
                "bytes": 0,
                "digest": "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                "path": path,
            })
        })
        .collect();
    let value = json!({
        "schema": "kioku/ci2/native-closure/v1",
        "header": {
            "schema": "kioku/ci2/native-possession/v1",
            "monitor_pid": 42001,
            "source": "owned-direct-child-original-process-group",
            "evidence": evidence,
        },
        "result": {
            "schema": "kioku/ci2/native-supervision/v1",
            "returncode": 0, "exit_code": 0, "signal": null,
            "wall_ns": 1000, "cpu_user_ns": 0, "cpu_system_ns": 0,
            "max_rss_bytes": 0, "cpu_complete": true, "descendants_drained": true,
            "started_ms": 1000, "ended_ms": 1001,
            "timed_out": false, "interrupted": false, "stopped_files": [],
            "cpu_source": "wait4 cumulative direct and subreaper-adopted children; each PID reaped once",
            "streams": streams, "reaped_direct_and_adopted": [42002], "error": null,
        },
        "no_children": true, "original_group_empty": true,
    });
    (evidence, value)
}

fn write_checkpoint(work: &Path, value: &Value) {
    let path = work.join("possession-closure.json");
    fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn valid_reader_fixture_keeps_the_exact_absence_scope() {
    let directory = tempfile::tempdir().unwrap();
    let work = directory.path().canonicalize().unwrap();
    fs::set_permissions(&work, fs::Permissions::from_mode(0o700)).unwrap();
    let (evidence, value) = checkpoint(&work);
    write_checkpoint(&work, &value);
    let proof = read_absence_proof(&work, &evidence).unwrap().unwrap();
    assert_eq!(proof.evidence(), &evidence);
    assert_eq!(proof.receipt().reaped_pids, vec![42002]);
    assert_eq!(
        proof.receipt().scope,
        AbsenceScope::OriginalGroupAndAdoptedDescendants
    );
}

#[test]
fn monitor_cannot_appear_in_its_own_reaped_children() {
    let directory = tempfile::tempdir().unwrap();
    let work = directory.path().canonicalize().unwrap();
    fs::set_permissions(&work, fs::Permissions::from_mode(0o700)).unwrap();
    let (evidence, mut value) = checkpoint(&work);
    write_checkpoint(&work, &value);
    assert!(read_absence_proof(&work, &evidence).unwrap().is_some());
    value["result"]["reaped_direct_and_adopted"] = json!([42001, 42002]);
    write_checkpoint(&work, &value);
    assert!(
        !matches!(read_absence_proof(&work, &evidence), Ok(Some(_))),
        "a monitor cannot reap itself; an impossible checkpoint must not mint a proof"
    );
}
