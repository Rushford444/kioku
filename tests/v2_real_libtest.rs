mod support;

use kioku::ci2::reports;
use kioku::ci2::reports::{CaseStatus, NativeEvaluation, TaskVerdict};
use kioku::ci2::*;
use std::process::{Command, Output};

fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}
fn digest() -> Digest {
    Digest::new(&format!("b3:{}", "a".repeat(64))).unwrap()
}
fn blob() -> BlobRef {
    BlobRef {
        digest: digest(),
        size_bytes: 1,
        media_type: "application/json".into(),
    }
}
fn contract() -> ResultContract {
    ResultContract {
        id: id("result"),
        kind: ResultKind::Test,
        adapter_id: id("adapter"),
        reporter_version: id("libtest-pretty-v2"),
        inventory_recipe_ref: Some(blob()),
        execution_recipe_ref: blob(),
        identity_rule_digest: digest(),
        normalization_policy_ref: blob(),
        native_skips: vec![NativeSkipRule {
            case_id: reports::case_id("fixture-project", "adapter", "ignored_case", "").unwrap(),
            profile_digest: digest(),
            reason_code: id("approved"),
            approval_ref: blob(),
            expires_at_ms: 100,
        }],
        expected_empty: false,
        complete_required: true,
    }
}
fn native(code: Option<i32>) -> NativeObservation {
    NativeObservation {
        exit_code: code,
        signal: None,
        stdout_digest: digest(),
        stderr_digest: digest(),
        stdout_bytes: 0,
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
fn execute(command: &mut Command) -> Output {
    support::assert_no_operator_stop_file();
    command.output().expect("execute fixture command")
}
#[test]
fn real_libtest_inventory_and_results_green_red_and_unknown() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("fixture.rs");
    let bin = temp.path().join("fixture");
    let fixture = r#"
#[test] fn first() {}
#[test] fn second() { FAIL_BODY }
#[test] #[ignore = "approved"] fn ignored_case() {}
"#;
    let profile = digest();
    let context = NativeEvaluation {
        project: "fixture-project",
        profile: &profile,
        now_ms: 1,
        complete: true,
        cancelled: false,
    };
    let c = contract();
    for (body, expected) in [
        ("", TaskVerdict::Passed),
        ("panic!(\"red\");", TaskVerdict::Failed),
    ] {
        std::fs::write(&source, fixture.replace("FAIL_BODY", body)).unwrap();
        let compiled = execute(
            Command::new("rustc")
                .arg("--test")
                .arg(&source)
                .arg("-o")
                .arg(&bin),
        );
        assert!(
            compiled.status.success(),
            "{}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let inventory = execute(Command::new(&bin).args(["--list", "--format", "terse"]));
        assert!(inventory.status.success());
        let output =
            execute(Command::new(&bin).args(["--format", "pretty", "--test-threads", "1"]));
        println!(
            "fixture {expected:?} inventory:\n{}results:\n{}stderr:\n{}",
            String::from_utf8_lossy(&inventory.stdout),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let report = reports::evaluate_with_context(
            &c,
            &native(output.status.code()),
            &inventory.stdout,
            &output.stdout,
            &output.stderr,
            &context,
        );
        assert_eq!(report.verdict, expected);
        let cases = report.cases.unwrap();
        assert_eq!(cases.len(), 3);
        assert!(cases.iter().all(|case| case.case_id.is_some()));
        assert_eq!(
            cases
                .iter()
                .filter(|case| matches!(case.status, CaseStatus::Ignored { .. }))
                .count(),
            1
        );
        if expected == TaskVerdict::Passed {
            assert_eq!(
                cases
                    .iter()
                    .filter(|case| case.status == CaseStatus::Passed)
                    .count(),
                2
            );
            let mut invalid_contract = c.clone();
            let mut duplicate_skip = invalid_contract.native_skips[0].clone();
            duplicate_skip.profile_digest = Digest::new(&format!("b3:{}", "b".repeat(64))).unwrap();
            invalid_contract.native_skips.push(duplicate_skip);
            assert!(invalid_contract.validate().is_err());
            let invalid = reports::evaluate_with_context(
                &invalid_contract,
                &native(output.status.code()),
                &inventory.stdout,
                &output.stdout,
                &output.stderr,
                &context,
            );
            assert_eq!(invalid.verdict, TaskVerdict::Incomplete);
            let unknown = reports::evaluate_with_context(
                &c,
                &native(None),
                &inventory.stdout,
                &output.stdout,
                &output.stderr,
                &context,
            );
            assert_eq!(unknown.verdict, TaskVerdict::Incomplete);
        } else {
            assert!(cases.iter().any(|case| case.status == CaseStatus::Failed));
        }
    }
}
