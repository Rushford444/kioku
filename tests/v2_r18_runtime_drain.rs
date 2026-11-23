#![cfg(unix)]

use kioku::ci2::domain::{EnvSpec, Lease, QueueItem, RuntimePort, StopToken, TaskSpec, Validate};
use kioku::ci2::runtime::{LocalProcessRunner, RuntimeError};
use serde_json::json;
use std::os::unix::fs::PermissionsExt;

fn stopped_runner(
    dir: &tempfile::TempDir,
    name: &str,
) -> (LocalProcessRunner, QueueItem, Lease, StopToken) {
    let root = dir.path().canonicalize().unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let home = root.join("home");
    std::fs::create_dir(&home).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    // A non-executable private file makes accidental spawning fail visibly.
    let executable = root.join("approved-tool");
    std::fs::write(&executable, b"fixture").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o600)).unwrap();
    let stop_file = root.join("stop");
    std::fs::write(&stop_file, b"").unwrap();
    std::fs::set_permissions(&stop_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let digest = format!("sha256:{}", "0".repeat(64));
    let blob = json!({"digest":digest,"size_bytes":0,"media_type":"application/json"});
    let resources =
        json!({"heavy_slots":0,"capture_slots":0,"max_state_bytes":0,"max_stderr_bytes":1024});
    let task: TaskSpec = serde_json::from_value(json!({
        "id":"task","adapter":"shell","command":{
            "executable":{"name":"tool","version":"test","binary_digest":digest,"installation_digest":digest},
            "argv":["fixture"],"cwd":".","timeout_ms":3000,"recipe_revision":"v1"
        },"inputs":[],"outputs":[],"dependencies":[],
        "env":[{"name":name,"kind":"public_value","value":"fixture","capability_ref":null}],
        "resources":resources,"required":true,"result_contract":{
            "id":"result","kind":"non_test","adapter_id":"shell","reporter_version":"v1",
            "inventory_recipe_ref":null,"execution_recipe_ref":blob,"identity_rule_digest":digest,
            "normalization_policy_ref":blob,"native_skips":[],"expected_empty":false,"complete_required":true
        },"cancellation":{"cancellable":true,"term_grace_ms":100,"max_drain_ms":1000},
        "admission":{"result_cache":false,"single_flight":false}
    })).unwrap();
    let item: QueueItem = serde_json::from_value(json!({
        "item_id":"item","run_id":"run","task_id":"task","snapshot_digest":digest,"config_digest":digest,
        "actor":{"principal_id":"principal","project_ids":[],"roles":[],"trust_scope":"local","lane_id":null},
        "priority":0,"wait_class":"agent","lane_id":"lane","resources":resources,"submitted_ms":0,"expected_epoch":1
    })).unwrap();
    let lease: Lease = serde_json::from_value(json!({
        "lease_id":"lease","item_id":"item","owner":"owner","epoch":1,"acquired_ms":0,"process_identity":null
    })).unwrap();
    let stop: StopToken = serde_json::from_value(json!({
        "state":"none","reason":null,"stop_file":stop_file,"observed_at_ms":0
    }))
    .unwrap();
    (
        LocalProcessRunner {
            work_root: root,
            private_home: home,
            executable,
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
fn valid_closed_env_name_reaches_stop_without_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let (mut runner, item, lease, stop) = stopped_runner(&dir, "FIXTURE_1");
    assert!(runner.task.env[0].validate().is_ok());
    assert!(matches!(
        runner.execute(&item, &lease, &stop),
        Err(RuntimeError::Interrupted)
    ));
}

#[test]
fn invalid_env_names_are_configuration_errors_before_stop() {
    for name in ["lowercase", "HAS-DASH", "HAS SPACE", "NONASCIIé"] {
        let dir = tempfile::tempdir().unwrap();
        let (mut runner, item, lease, stop) = stopped_runner(&dir, name);
        let env: &EnvSpec = &runner.task.env[0];
        assert!(env.validate().is_err());
        assert!(matches!(
            runner.execute(&item, &lease, &stop),
            Err(RuntimeError::Configuration)
        ));
    }
}
