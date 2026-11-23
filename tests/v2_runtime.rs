#![cfg(unix)]
use kioku::ci2::domain::{Lease, QueueItem, RuntimePort, StopToken, TaskSpec};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

// The root integration later registers this module in the production library.
mod ci2 {
    pub use kioku::ci2::domain;
}
#[path = "../src/ci2/runtime/mod.rs"]
mod runtime;
use runtime::{LocalProcessRunner, RuntimeError};
use serde_json::{Value, json};

fn digest() -> String {
    format!("sha256:{}", "0".repeat(64))
}
fn blob() -> Value {
    json!({"digest": digest(), "size_bytes": 0, "media_type": "application/json"})
}
fn runner(dir: &tempfile::TempDir, exit: i32) -> (LocalProcessRunner, QueueItem, Lease, StopToken) {
    let root = dir.path().canonicalize().unwrap();
    let home = root.join("home");
    std::fs::create_dir(&home).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ci2/runtime/command.sh");
    let resources =
        json!({"heavy_slots":0,"capture_slots":0,"max_state_bytes":0,"max_stderr_bytes":1024});
    let task: TaskSpec = serde_json::from_value(json!({
        "id":"task", "adapter":"shell", "command":{
            "executable":{"name":"sh","version":"test","binary_digest":digest(),"installation_digest":digest()},
            "argv":[fixture.to_str().unwrap(),exit.to_string()],"cwd":".","timeout_ms":3000,"recipe_revision":"v1"
        }, "inputs":[], "outputs":[], "dependencies":[], "env":[], "resources":resources,
        "required":true, "result_contract":{
            "id":"result","kind":"non_test","adapter_id":"shell","reporter_version":"v1",
            "inventory_recipe_ref":null,"execution_recipe_ref":blob(),"identity_rule_digest":digest(),
            "normalization_policy_ref":blob(),"native_skips":[],"expected_empty":false,"complete_required":true
        },"cancellation":{"cancellable":true,"term_grace_ms":100,"max_drain_ms":1000},
        "admission":{"result_cache":false,"single_flight":false}
    })).unwrap();
    let item: QueueItem = serde_json::from_value(json!({"item_id":"item","run_id":"run","task_id":"task",
        "snapshot_digest":digest(),"config_digest":digest(),"actor":{"principal_id":"principal","project_ids":[],"roles":[],"trust_scope":"local","lane_id":null},
        "priority":0,"wait_class":"agent","lane_id":"lane","resources":resources,"submitted_ms":0,"expected_epoch":1})).unwrap();
    let lease: Lease = serde_json::from_value(json!({"lease_id":"lease","item_id":"item","owner":"owner","epoch":1,"acquired_ms":0,"process_identity":null})).unwrap();
    let stop: StopToken = serde_json::from_value(
        json!({"state":"none","reason":null,"stop_file":root.join("stop"),"observed_at_ms":0}),
    )
    .unwrap();
    (
        LocalProcessRunner {
            work_root: root,
            private_home: home,
            executable: "/bin/sh".into(),
            task,
            stdout_bytes: 1024,
            stderr_bytes: 1024,
        },
        item,
        lease,
        stop,
    )
}

#[test]
fn native_exit_and_streams_are_observed_without_asserting_cpu_or_tree_completeness() {
    let dir = tempfile::tempdir().unwrap();
    let (mut r, item, lease, stop) = runner(&dir, 0);
    let obs = r.execute(&item, &lease, &stop).unwrap();
    assert_eq!(obs.exit_code, Some(0));
    assert_eq!((obs.stdout_bytes, obs.stderr_bytes), (13, 13));
    assert_eq!(
        obs.stdout_digest,
        kioku::ci2::domain::Digest::sha256("kioku/ci2/native-stream/v1", b"native stdout").unwrap()
    );
    assert!(!obs.cpu_complete && !obs.descendants_drained && !obs.timed_out);
}

#[test]
fn failing_child_is_not_rewritten_to_success() {
    let dir = tempfile::tempdir().unwrap();
    let (mut r, item, lease, stop) = runner(&dir, 7);
    assert_eq!(r.execute(&item, &lease, &stop).unwrap().exit_code, Some(7));
}

#[test]
fn stop_prevents_spawn_and_stream_limit_is_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let (mut r, item, lease, stop) = runner(&dir, 0);
    std::fs::write(&stop.stop_file, b"").unwrap();
    assert!(matches!(
        r.execute(&item, &lease, &stop),
        Err(RuntimeError::Interrupted)
    ));
    std::fs::remove_file(&stop.stop_file).unwrap();
    r.stdout_bytes = 1;
    assert!(matches!(
        r.execute(&item, &lease, &stop),
        Err(RuntimeError::DrainIncomplete)
    ));
}

fn fixture(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/ci2/runtime")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

#[test]
fn inherited_stream_is_bounded_after_direct_exit() {
    let dir = tempfile::tempdir().unwrap();
    let (mut r, item, lease, stop) = runner(&dir, 0);
    r.task.command.argv = vec![fixture("hold-stream.sh")];
    r.task.cancellation.max_drain_ms = 50;
    let started = std::time::Instant::now();
    assert!(matches!(
        r.execute(&item, &lease, &stop),
        Err(RuntimeError::DrainIncomplete)
    ));
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
}

#[test]
fn timeout_is_an_observed_native_result() {
    let dir = tempfile::tempdir().unwrap();
    let (mut r, item, lease, stop) = runner(&dir, 0);
    r.task.command.argv = vec![fixture("wait.sh")];
    r.task.command.timeout_ms = 50;
    let obs = r.execute(&item, &lease, &stop).unwrap();
    assert!(obs.timed_out);
    assert!(obs.signal.is_some());
    assert!(!obs.cpu_complete && !obs.descendants_drained);
}
