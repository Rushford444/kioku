//! Schema guards for a libtest inventory producer and a results consumer.
//!
//! Validation checks declarations, not captured-byte provenance. The coordinator
//! still has to bind one producer and its exact output ID/kind, matching recipes,
//! snapshot/profile and complete durable streams. Named report files must match
//! the captured stdout bytes; an output kind alone does not establish that fact.

use kioku::ci2::reports::REPORTER_VERSION;
use kioku::ci2::*;
use serde_json::json;

fn id(value: &str) -> Id {
    Id::new(value).expect("fixture ID")
}

fn recipe(bytes: &[u8]) -> BlobRef {
    BlobRef {
        digest: Digest::blake3("kioku/ci2/recipe/v1", bytes).expect("recipe digest"),
        size_bytes: bytes.len() as u64,
        media_type: "application/json".into(),
    }
}

fn pair() -> TaskManifest {
    let inventory_recipe = recipe(b"libtest --list --format terse");
    let execution_recipe = recipe(b"libtest --format pretty --test-threads 1");
    let normalization = recipe(b"libtest case names");
    let digest = normalization.digest.clone();
    let task = |name, kind, output_kind, path, execution, inventory| {
        json!({
            "id": name, "adapter": "libtest",
            "command": {
                "executable": {"name": "fixture-reporter", "version": "1",
                    "binary_digest": digest, "installation_digest": digest},
                "argv": [name], "cwd": "", "timeout_ms": 10000,
                "recipe_revision": "libtest-report-v1"
            },
            "inputs": [], "dependencies": [], "env": [], "required": true,
            "outputs": [
                {"id": "stdout", "kind": "stdout", "path": null,
                    "required": true, "max_bytes": 4096},
                {"id": "report", "kind": output_kind, "path": path,
                    "required": true, "max_bytes": 4096}
            ],
            "resources": {"heavy_slots": 1, "capture_slots": 1,
                "max_state_bytes": 16384, "max_stderr_bytes": 4096},
            "result_contract": {
                "id": name, "kind": kind, "adapter_id": "libtest",
                "reporter_version": REPORTER_VERSION,
                "inventory_recipe_ref": inventory, "execution_recipe_ref": execution,
                "identity_rule_digest": digest, "normalization_policy_ref": normalization,
                "native_skips": [], "expected_empty": false, "complete_required": true
            },
            "cancellation": {"cancellable": true, "term_grace_ms": 100,
                "max_drain_ms": 1000},
            "admission": {"result_cache": false, "single_flight": false}
        })
    };
    let value = json!({
        "schema": "kioku/ci2/tasks/v1", "revision": 1,
        "required_task_ids": ["inventory", "results"], "protected_refs": [],
        "tasks": [
            task("inventory", "non_test", "test_inventory", "inventory.txt",
                &inventory_recipe, None::<&BlobRef>),
            task("results", "test", "test_results", "results.txt",
                &execution_recipe, Some(&inventory_recipe))
        ]
    });
    let mut manifest: TaskManifest = serde_json::from_value(value).expect("typed fixture");
    manifest.tasks[1].dependencies.push(TaskDependency {
        task_id: id("inventory"),
        consumed_output_ids: vec![id("report")],
    });
    manifest.tasks[1].inputs.push(InputSelector::Producer {
        task_id: id("inventory"),
        output_id: id("report"),
    });
    manifest
}

#[test]
fn libtest_pair_with_report_files_and_separate_streams_round_trips() {
    let manifest = pair();
    manifest
        .validate()
        .expect("valid multi-output producer pair");
    let bytes = serde_json::to_vec(&manifest).expect("manifest bytes");
    let decoded: TaskManifest = decode_bounded(&bytes).expect("validated wire record");
    assert_eq!(decoded, manifest);
    let producer = &decoded.tasks[0];
    let consumer = &decoded.tasks[1];
    assert_eq!(producer.result_contract.kind, ResultKind::NonTest);
    assert_eq!(consumer.result_contract.kind, ResultKind::Test);
    assert_eq!(
        consumer.result_contract.inventory_recipe_ref.as_ref(),
        Some(&producer.result_contract.execution_recipe_ref)
    );
    assert_ne!(
        consumer.result_contract.execution_recipe_ref,
        producer.result_contract.execution_recipe_ref
    );
}

#[test]
fn test_contract_requires_inventory_and_matching_task_adapter() {
    let manifest = pair();
    let valid = &manifest.tasks[1];
    valid.validate().expect("valid test task");
    let mut missing_inventory = valid.clone();
    missing_inventory.result_contract.inventory_recipe_ref = None;
    assert!(matches!(
        missing_inventory.result_contract.validate(),
        Err(ContractError::Configuration(_))
    ));
    assert!(missing_inventory.validate().is_err());

    let mut wrong_adapter = valid.clone();
    wrong_adapter.result_contract.adapter_id = id("different-adapter");
    wrong_adapter
        .result_contract
        .validate()
        .expect("valid isolated contract");
    assert!(matches!(
        wrong_adapter.validate(),
        Err(ContractError::Configuration(_))
    ));
}

#[test]
fn named_reports_cannot_reinterpret_a_pathless_native_stream() {
    let manifest = pair();
    manifest.validate().expect("valid report paths");
    for task in &manifest.tasks {
        let stream = &task.outputs[0];
        stream.validate().expect("pathless native stdout");
        let report = &task.outputs[1];
        report.validate().expect("named report");

        let mut pathless_report = report.clone();
        pathless_report.path = None;
        assert!(matches!(
            pathless_report.validate(),
            Err(ContractError::Configuration(_))
        ));
        let mut named_stream = stream.clone();
        named_stream.path.clone_from(&report.path);
        assert!(matches!(
            named_stream.validate(),
            Err(ContractError::Configuration(_))
        ));
    }
}

#[test]
fn producer_binding_requires_the_exact_declared_dependency_output() {
    let valid = pair();
    valid.validate().expect("bound producer output");

    let mut unknown_output = valid.clone();
    unknown_output.tasks[1].dependencies[0].consumed_output_ids = vec![id("absent")];
    unknown_output.tasks[1].inputs = vec![InputSelector::Producer {
        task_id: id("inventory"),
        output_id: id("absent"),
    }];
    assert!(matches!(
        unknown_output.validate(),
        Err(ContractError::Configuration(_))
    ));

    let mut undeclared_input = valid.clone();
    undeclared_input.tasks[1].inputs = vec![InputSelector::Producer {
        task_id: id("inventory"),
        output_id: id("stdout"),
    }];
    assert!(matches!(
        undeclared_input.validate(),
        Err(ContractError::Configuration(_))
    ));

    let mut duplicate_output = valid;
    let report = duplicate_output.tasks[0].outputs[1].clone();
    duplicate_output.tasks[0].outputs.push(report);
    assert!(matches!(
        duplicate_output.validate(),
        Err(ContractError::Configuration(_))
    ));
}
