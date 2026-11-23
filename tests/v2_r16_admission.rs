//! Static producer-input identities must be bound by the manifest DAG.
use kioku::ci2::{TaskManifest, Validate, decode_bounded};
use serde_json::{Value, json};

fn manifest_value() -> Value {
    let digest = format!("sha256:{}", "a".repeat(64));
    let blob = json!({"digest": digest, "size_bytes": 0, "media_type": "application/json"});
    let task = |name: &str| {
        json!({
            "id": name, "adapter": "fixture-adapter",
            "command": {
                "executable": {"name": "fixture", "version": "1",
                    "binary_digest": digest, "installation_digest": digest},
                "argv": [name], "cwd": "", "timeout_ms": 10000, "recipe_revision": "v1"},
            "inputs": [],
            "outputs": [{"id": "stdout", "kind": "stdout", "path": null,
                "required": true, "max_bytes": 4096}],
            "dependencies": [], "env": [], "required": true,
            "resources": {"heavy_slots": 1, "capture_slots": 1,
                "max_state_bytes": 4096, "max_stderr_bytes": 4096},
            "result_contract": {"id": "fixture-result", "kind": "non_test",
                "adapter_id": "fixture-adapter", "reporter_version": "raw-native-exit-v1",
                "inventory_recipe_ref": null, "execution_recipe_ref": blob,
                "identity_rule_digest": digest, "normalization_policy_ref": blob,
                "native_skips": [], "expected_empty": false, "complete_required": true},
            "cancellation": {"cancellable": true, "term_grace_ms": 100, "max_drain_ms": 500},
            "admission": {"result_cache": false, "single_flight": false}
        })
    };
    let mut value = json!({
        "schema": "kioku/ci2/tasks/v1", "revision": 1, "protected_refs": [],
        "required_task_ids": ["producer", "consumer"],
        "tasks": [task("producer"), task("consumer")]
    });
    value["tasks"][1]["inputs"] = json!([
        {"kind": "producer", "task_id": "producer", "output_id": "stdout"}
    ]);
    value["tasks"][1]["dependencies"] = json!([
        {"task_id": "producer", "consumed_output_ids": ["stdout"]}
    ]);
    value
}

#[test]
fn producer_selector_requires_a_matching_dependency_output() {
    for (task_id, output_id, remove_dependency) in [
        ("missing-producer", "stdout", false),
        ("consumer", "stdout", false),
        ("producer", "missing-output", false),
        ("producer", "stdout", true),
    ] {
        let mut value = manifest_value();
        value["tasks"][1]["inputs"][0]["task_id"] = json!(task_id);
        value["tasks"][1]["inputs"][0]["output_id"] = json!(output_id);
        if remove_dependency {
            value["tasks"][1]["dependencies"] = json!([]);
        }
        let bytes = serde_json::to_vec(&value).expect("JSON manifest");
        let typed: TaskManifest = serde_json::from_slice(&bytes).expect("typed manifest");
        let validation = typed.validate();
        assert!(
            validation.is_err(),
            "unbound producer selector admitted: {task_id}/{output_id}, removed edge={remove_dependency}: {validation:?}"
        );
        assert!(decode_bounded::<TaskManifest>(&bytes).is_err());
    }
}

#[test]
fn matching_producer_selector_remains_admitted() {
    let bytes = serde_json::to_vec(&manifest_value()).expect("JSON manifest");
    let typed: TaskManifest = serde_json::from_slice(&bytes).expect("typed manifest");
    typed.validate().expect("bound DAG");
    let decoded: TaskManifest = decode_bounded(&bytes).expect("bounded bound DAG");
    assert_eq!(decoded, typed);
}
