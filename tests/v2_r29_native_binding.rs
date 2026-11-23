//! Local binding proofs only; not full UL05/08/09 coverage.
mod ci2 {
    pub use kioku::ci2::*;
}
#[path = "../src/ci2/reports/native_binding.rs"]
mod native_binding;
use kioku::ci2::{reports, snapshot::Snapshot, *};
use native_binding::{EvaluationOptions, NativeBinding, evaluate_bound};
use std::collections::{BTreeMap, BTreeSet};

fn id(value: &str) -> Id {
    Id::new(value).unwrap()
}
fn digest(value: &str) -> Digest {
    Digest::sha256("r29-fixture", value.as_bytes()).unwrap()
}
fn reference(value: &str) -> BlobRef {
    BlobRef {
        digest: digest(value),
        size_bytes: 1,
        media_type: "application/json".into(),
    }
}
fn recipe(command: &CommandSpec) -> BlobRef {
    let bytes = serde_json::to_vec(command).unwrap();
    BlobRef {
        digest: Digest::sha256("kioku/ci2/libtest-recipe/v1", &bytes).unwrap(),
        size_bytes: bytes.len() as u64,
        media_type: "application/json".into(),
    }
}
fn bind_recipe(task: &mut TaskSpec) {
    task.result_contract.execution_recipe_ref = recipe(&task.command);
    task.result_contract.normalization_policy_ref = recipe(&task.command);
}
fn snapshot() -> Snapshot {
    Snapshot {
        files: BTreeMap::new(),
        directories: BTreeSet::new(),
    }
}
fn tasks() -> (TaskSpec, TaskSpec) {
    let mut inventory = TaskSpec {
        id: id("inventory"),
        adapter: id("libtest"),
        command: CommandSpec {
            executable: ToolIdentity {
                name: "fixture".into(),
                version: "1".into(),
                binary_digest: digest("binary"),
                installation_digest: digest("install"),
            },
            argv: ["--list", "--format", "terse"].map(str::to_owned).to_vec(),
            cwd: String::new(),
            timeout_ms: 1000,
            recipe_revision: id("v1"),
        },
        inputs: vec![InputSelector::File {
            path: RelPath::new("fixture.rs").unwrap(),
        }],
        outputs: vec![OutputSpec {
            id: id("inventory-stream"),
            kind: OutputKind::TestInventory,
            path: Some(RelPath::new("inventory.txt").unwrap()),
            required: true,
            max_bytes: 65536,
        }],
        dependencies: vec![],
        env: vec![],
        resources: ResourceRecipe {
            heavy_slots: 1,
            capture_slots: 1,
            max_state_bytes: 65536,
            max_stderr_bytes: 65536,
        },
        required: true,
        result_contract: ResultContract {
            id: id("native"),
            kind: ResultKind::NonTest,
            adapter_id: id("libtest"),
            reporter_version: id(reports::REPORTER_VERSION),
            inventory_recipe_ref: None,
            execution_recipe_ref: reference("pending"),
            identity_rule_digest: native_binding::identity_ref().unwrap(),
            normalization_policy_ref: reference("normalization"),
            native_skips: vec![],
            expected_empty: false,
            complete_required: true,
        },
        cancellation: CancellationPolicy {
            cancellable: true,
            term_grace_ms: 10,
            max_drain_ms: 100,
        },
        admission: AdmissionPolicy {
            result_cache: false,
            single_flight: false,
        },
    };
    bind_recipe(&mut inventory);
    let mut execution = inventory.clone();
    execution.id = id("execution");
    execution.command.argv = ["--format", "pretty", "--test-threads", "1"]
        .map(str::to_owned)
        .to_vec();
    execution.inputs.push(InputSelector::Producer {
        task_id: inventory.id.clone(),
        output_id: id("inventory-stream"),
    });
    execution.dependencies.push(TaskDependency {
        task_id: inventory.id.clone(),
        consumed_output_ids: vec![id("inventory-stream")],
    });
    execution.outputs = vec![OutputSpec {
        id: id("results-stream"),
        kind: OutputKind::TestResults,
        path: Some(RelPath::new("results.txt").unwrap()),
        required: true,
        max_bytes: 65536,
    }];
    execution.result_contract.kind = ResultKind::Test;
    execution.result_contract.inventory_recipe_ref = Some(recipe(&inventory.command));
    bind_recipe(&mut execution);
    (inventory, execution)
}
fn binding<'a>(
    inventory: &'a TaskSpec,
    execution: &'a TaskSpec,
    snapshot: &'a Snapshot,
) -> NativeBinding<'a> {
    NativeBinding {
        inventory,
        execution,
        inventory_snapshot: snapshot,
        execution_snapshot: snapshot,
        inventory_native_healthy: true,
    }
}
#[test]
fn real_pair_accepts_but_missing_or_inverted_producer_rejects() {
    let (inventory, execution) = tasks();
    let snapshot = snapshot();
    assert!(
        binding(&inventory, &execution, &snapshot)
            .validate()
            .is_ok()
    );
    let mut missing = execution.clone();
    missing.inputs.pop();
    assert!(binding(&inventory, &missing, &snapshot).validate().is_err());
    assert!(
        binding(&execution, &inventory, &snapshot)
            .validate()
            .is_err()
    );
    let mut missing_dependency = execution;
    missing_dependency.dependencies.clear();
    assert!(
        binding(&inventory, &missing_dependency, &snapshot)
            .validate()
            .is_err()
    );
}

#[test]
fn recipes_bind_exact_command_json_digest_size_and_media_type() {
    let (inventory, execution) = tasks();
    let snapshot = snapshot();
    let mut changed = execution.clone();
    changed.result_contract.execution_recipe_ref.digest = digest("wrong");
    assert!(binding(&inventory, &changed, &snapshot).validate().is_err());
    changed = execution.clone();
    changed
        .result_contract
        .inventory_recipe_ref
        .as_mut()
        .unwrap()
        .size_bytes += 1;
    assert!(binding(&inventory, &changed, &snapshot).validate().is_err());
    changed = execution.clone();
    changed.result_contract.execution_recipe_ref.media_type = "text/plain".into();
    assert!(binding(&inventory, &changed, &snapshot).validate().is_err());
    let mut wrong_inventory = inventory;
    wrong_inventory.result_contract.execution_recipe_ref.digest = digest("wrong");
    assert!(
        binding(&wrong_inventory, &execution, &snapshot)
            .validate()
            .is_err()
    );
}

#[test]
fn commands_tool_cwd_environment_and_resources_cannot_drift() {
    let (inventory, execution) = tasks();
    let snapshot = snapshot();
    for argv in [
        vec!["--format", "json"],
        vec!["--format", "pretty", "--test-threads", "2"],
        vec!["--format", "pretty", "--test-threads", "1", "--ignored"],
    ] {
        let mut changed = execution.clone();
        changed.command.argv = argv.into_iter().map(str::to_owned).collect();
        bind_recipe(&mut changed);
        assert!(binding(&inventory, &changed, &snapshot).validate().is_err());
    }
    let mut wrong_inventory = inventory.clone();
    wrong_inventory.command.argv.push("--ignored".into());
    bind_recipe(&mut wrong_inventory);
    let mut paired_execution = execution.clone();
    paired_execution.result_contract.inventory_recipe_ref = Some(recipe(&wrong_inventory.command));
    assert!(
        binding(&wrong_inventory, &paired_execution, &snapshot)
            .validate()
            .is_err()
    );
    let mut changed = execution.clone();
    changed.command.executable.binary_digest = digest("another tool");
    bind_recipe(&mut changed);
    assert!(binding(&inventory, &changed, &snapshot).validate().is_err());
    changed = execution.clone();
    changed.command.cwd = "other".into();
    bind_recipe(&mut changed);
    assert!(binding(&inventory, &changed, &snapshot).validate().is_err());
    changed = execution.clone();
    changed.env.push(EnvSpec {
        name: "RUST_TEST_THREADS".into(),
        kind: EnvKind::PublicValue,
        value: Some("2".into()),
        capability_ref: None,
    });
    assert!(binding(&inventory, &changed, &snapshot).validate().is_err());
    changed = execution;
    changed.resources.max_state_bytes += 1;
    assert!(binding(&inventory, &changed, &snapshot).validate().is_err());
}

#[test]
fn unknown_or_different_input_declarations_and_snapshot_reject() {
    let (inventory, execution) = tasks();
    let snapshot = snapshot();
    for input in [
        InputSelector::Producer {
            task_id: id("unknown"),
            output_id: id("inventory-stream"),
        },
        InputSelector::Producer {
            task_id: inventory.id.clone(),
            output_id: id("unknown"),
        },
        InputSelector::Absent {
            path: RelPath::new("fixture.rs").unwrap(),
        },
        InputSelector::Glob {
            root: RelPath::new("src").unwrap(),
            pattern: "*.rs".into(),
        },
        InputSelector::Tree {
            root: RelPath::new("src").unwrap(),
        },
    ] {
        let mut changed = execution.clone();
        changed.inputs[0] = input;
        assert!(binding(&inventory, &changed, &snapshot).validate().is_err());
    }
    let mut pair = binding(&inventory, &execution, &snapshot);
    let mut different = snapshot.clone();
    different.files.insert(
        "fixture.rs".into(),
        snapshot::CapturedFile {
            digest: digest("different bytes").to_string(),
            bytes: 2,
        },
    );
    pair.execution_snapshot = &different;
    assert!(pair.validate().is_err());
}

#[test]
fn extra_reports_dependencies_producers_and_unhealthy_inventory_reject() {
    let (inventory, execution) = tasks();
    let snapshot = snapshot();
    let mut pair = binding(&inventory, &execution, &snapshot);
    pair.inventory_native_healthy = false;
    assert!(pair.validate().is_err());
    let mut changed = execution.clone();
    let mut extra = changed.outputs[0].clone();
    extra.id = id("extra");
    changed.outputs.push(extra);
    assert!(binding(&inventory, &changed, &snapshot).validate().is_err());
    changed = execution.clone();
    changed.dependencies[0]
        .consumed_output_ids
        .push(id("extra"));
    assert!(binding(&inventory, &changed, &snapshot).validate().is_err());
    changed = execution.clone();
    changed.outputs[0].kind = OutputKind::TestInventory;
    assert!(binding(&inventory, &changed, &snapshot).validate().is_err());
    changed = execution;
    changed.outputs[0].required = false;
    assert!(binding(&inventory, &changed, &snapshot).validate().is_err());
}

const INVENTORY: &[u8] = b"case_a: test\n1 test, 0 benchmarks\n";
const RESULTS: &[u8] = b"running 1 test\ntest case_a ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n";
fn native() -> NativeObservation {
    NativeObservation {
        exit_code: Some(0),
        signal: None,
        stdout_digest: digest("stdout"),
        stderr_digest: digest("stderr"),
        stdout_bytes: RESULTS.len() as u64,
        stderr_bytes: 0,
        wall_ns: 1,
        cpu_user_ns: 1,
        cpu_system_ns: 0,
        cpu_complete: true,
        descendants_drained: true,
        started_ms: 1,
        ended_ms: 2,
        timed_out: false,
    }
}
#[test]
fn evaluation_requires_verified_streams_and_external_health_then_normalizes_cases() {
    let (inventory, execution) = tasks();
    let snapshot = snapshot();
    let pair = binding(&inventory, &execution, &snapshot);
    let profile = digest("separately-established-profile");
    let mut options = EvaluationOptions {
        context: reports::NativeEvaluation {
            project: "project",
            profile: &profile,
            now_ms: 3,
            complete: true,
            cancelled: false,
        },
        execution_native_healthy: true,
        streams_verified: false,
    };
    assert!(evaluate_bound(&pair, &native(), INVENTORY, RESULTS, b"", &options).is_err());
    options.streams_verified = true;
    let report = evaluate_bound(&pair, &native(), INVENTORY, RESULTS, b"", &options).unwrap();
    assert_eq!(report.verdict, reports::TaskVerdict::Passed);
    assert_eq!(
        report.cases.unwrap()[0].case_id,
        Some(reports::case_id("project", "libtest", "case_a", "").unwrap())
    );
    options.execution_native_healthy = false;
    assert_eq!(
        evaluate_bound(&pair, &native(), INVENTORY, RESULTS, b"", &options)
            .unwrap()
            .verdict,
        reports::TaskVerdict::Incomplete
    );
    options.execution_native_healthy = true;
    let mut failed = native();
    failed.exit_code = Some(1);
    assert_eq!(
        evaluate_bound(&pair, &failed, INVENTORY, RESULTS, b"", &options)
            .unwrap()
            .verdict,
        reports::TaskVerdict::Failed
    );
    assert_ne!(
        evaluate_bound(&pair, &native(), b"", RESULTS, b"", &options)
            .unwrap()
            .verdict,
        reports::TaskVerdict::Passed
    );
}

#[test]
fn identity_normalization_reporter_and_skips_must_match_the_supported_rule() {
    let (inventory, execution) = tasks();
    let snapshot = snapshot();
    assert_eq!(
        execution.result_contract.identity_rule_digest,
        Digest::sha256(
            "kioku/ci2/libtest-case-identity/v1",
            b"kioku/ci2/libtest-pretty-v2"
        )
        .unwrap()
    );
    let mut changed = execution.clone();
    changed.result_contract.identity_rule_digest = digest("opaque identity");
    assert!(binding(&inventory, &changed, &snapshot).validate().is_err());
    changed = execution.clone();
    changed.result_contract.normalization_policy_ref = recipe(&inventory.command);
    assert!(binding(&inventory, &changed, &snapshot).validate().is_err());
    changed = execution.clone();
    changed.result_contract.reporter_version = id("libtest-pretty-v1");
    assert!(binding(&inventory, &changed, &snapshot).validate().is_err());
    changed = execution;
    changed.result_contract.native_skips.push(NativeSkipRule {
        case_id: id("case_a"),
        profile_digest: digest("profile"),
        reason_code: id("approved"),
        approval_ref: reference("approval"),
        expires_at_ms: 100,
    });
    assert!(binding(&inventory, &changed, &snapshot).validate().is_err());
}

#[test]
fn bound_results_preserve_pass_failure_and_unapproved_ignored_cases() {
    let (inventory, execution) = tasks();
    let snapshot = snapshot();
    let pair = binding(&inventory, &execution, &snapshot);
    let profile = digest("separately-established-profile");
    let options = EvaluationOptions {
        context: reports::NativeEvaluation {
            project: "project",
            profile: &profile,
            now_ms: 3,
            complete: true,
            cancelled: false,
        },
        execution_native_healthy: true,
        streams_verified: true,
    };
    let failed = b"running 1 test\ntest case_a ... FAILED\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n";
    let ignored = b"running 1 test\ntest case_a ... ignored\ntest result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out\n";
    let passed_report =
        evaluate_bound(&pair, &native(), INVENTORY, RESULTS, b"", &options).unwrap();
    assert_eq!(passed_report.verdict, reports::TaskVerdict::Passed);
    assert_eq!(
        passed_report.cases.unwrap()[0].status,
        reports::CaseStatus::Passed
    );
    let mut failed_native = native();
    failed_native.exit_code = Some(101);
    let failed_report =
        evaluate_bound(&pair, &failed_native, INVENTORY, failed, b"", &options).unwrap();
    assert_eq!(failed_report.verdict, reports::TaskVerdict::Failed);
    assert_eq!(
        failed_report.cases.unwrap()[0].status,
        reports::CaseStatus::Failed
    );
    let ignored_report =
        evaluate_bound(&pair, &native(), INVENTORY, ignored, b"", &options).unwrap();
    assert_eq!(ignored_report.verdict, reports::TaskVerdict::Incomplete);
}
