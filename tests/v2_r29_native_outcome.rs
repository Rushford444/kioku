use kioku::ci2::domain::{Digest, NativeObservation, RunState};
use kioku::ci2::reports::native_outcome::reported_test_state;
use kioku::ci2::reports::{TaskReport, TaskVerdict};

fn native() -> NativeObservation {
    let digest = Digest::blake3("kioku/ci2/native-outcome-test/v1", b"public fixture")
        .expect("fixture digest");
    NativeObservation {
        exit_code: Some(0),
        signal: None,
        stdout_digest: digest.clone(),
        stderr_digest: digest,
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

fn report(verdict: TaskVerdict) -> TaskReport {
    TaskReport {
        verdict,
        exit_code: Some(0),
        cases: None,
        diagnostics: String::new(),
    }
}

#[test]
fn test_success_requires_a_passed_report_and_complete_native_observation() {
    let mut observation = native();
    let passed = report(TaskVerdict::Passed);
    assert_eq!(
        reported_test_state(&observation, None, RunState::Passed, false, false),
        RunState::Incomplete
    );
    assert_eq!(
        reported_test_state(&observation, Some(&passed), RunState::Passed, false, false),
        RunState::Passed
    );
    observation.cpu_complete = false;
    assert_eq!(
        reported_test_state(&observation, Some(&passed), RunState::Passed, false, false),
        RunState::Incomplete
    );
}

#[test]
fn unknown_or_partial_native_closure_never_passes() {
    let passed = report(TaskVerdict::Passed);
    for observation in [
        NativeObservation {
            exit_code: None,
            ..native()
        },
        NativeObservation {
            descendants_drained: false,
            ..native()
        },
        NativeObservation {
            cpu_complete: false,
            ..native()
        },
        NativeObservation {
            timed_out: true,
            cpu_complete: false,
            ..native()
        },
    ] {
        assert_eq!(
            reported_test_state(&observation, Some(&passed), RunState::Passed, false, false),
            RunState::Incomplete
        );
    }
}

#[test]
fn selected_output_errors_and_run_collection_errors_prevent_pass() {
    let observation = native();
    let passed = report(TaskVerdict::Passed);
    for (run, known_error, output_error, expected) in [
        (RunState::Passed, false, true, RunState::Incomplete),
        (RunState::Incomplete, true, false, RunState::Incomplete),
        (RunState::Failed, true, false, RunState::Passed),
        (RunState::Incomplete, false, false, RunState::Passed),
    ] {
        assert_eq!(
            reported_test_state(&observation, Some(&passed), run, known_error, output_error),
            expected
        );
    }
}

#[test]
fn native_failures_and_complete_timeouts_are_not_success() {
    let passed = report(TaskVerdict::Passed);
    for observation in [
        NativeObservation {
            exit_code: Some(101),
            ..native()
        },
        NativeObservation {
            exit_code: None,
            signal: Some(15),
            ..native()
        },
        NativeObservation {
            timed_out: true,
            ..native()
        },
    ] {
        assert_eq!(
            reported_test_state(&observation, Some(&passed), RunState::Failed, false, false),
            RunState::Failed
        );
        assert_eq!(
            reported_test_state(&observation, None, RunState::Failed, false, false),
            RunState::Failed
        );
    }
}

#[test]
fn cancellation_requires_drain_and_cpu_closure() {
    let passed = report(TaskVerdict::Passed);
    let killed = NativeObservation {
        exit_code: None,
        signal: Some(15),
        ..native()
    };
    for observation in [native(), killed.clone()] {
        assert_eq!(
            reported_test_state(
                &observation,
                Some(&passed),
                RunState::Cancelled,
                false,
                false
            ),
            RunState::Cancelled
        );
    }
    for observation in [
        NativeObservation {
            cpu_complete: false,
            ..killed.clone()
        },
        NativeObservation {
            descendants_drained: false,
            ..killed
        },
    ] {
        assert_eq!(
            reported_test_state(
                &observation,
                Some(&passed),
                RunState::Cancelled,
                false,
                false
            ),
            RunState::Incomplete
        );
    }
}

#[test]
fn report_verdicts_are_not_upgraded_to_success() {
    let observation = native();
    for (verdict, expected) in [
        (TaskVerdict::Failed, RunState::Failed),
        (TaskVerdict::Cancelled, RunState::Cancelled),
        (TaskVerdict::Incomplete, RunState::Incomplete),
    ] {
        let report = report(verdict);
        for _ in 0..2 {
            assert_eq!(
                reported_test_state(&observation, Some(&report), RunState::Passed, false, false),
                expected
            );
            assert_eq!(report.verdict, verdict);
        }
    }
}

#[test]
fn existing_classifier_detects_failure_even_in_a_contradictory_report() {
    use kioku::ci2::reports::{CaseResult, CaseStatus};

    let failed_case = TaskReport {
        cases: Some(vec![CaseResult {
            raw: "fixture_case".into(),
            case_id: None,
            status: CaseStatus::Failed,
        }]),
        ..report(TaskVerdict::Passed)
    };
    let failed_native = TaskReport {
        exit_code: Some(1),
        ..report(TaskVerdict::Passed)
    };
    for report in [&failed_case, &failed_native] {
        assert_eq!(
            reported_test_state(&native(), Some(report), RunState::Passed, false, false),
            RunState::Failed
        );
    }
    assert_eq!(
        reported_test_state(
            &native(),
            Some(&failed_case),
            RunState::Cancelled,
            false,
            true
        ),
        RunState::Failed
    );
}

#[test]
fn parsed_paired_output_still_requires_complete_native_accounting() {
    use kioku::ci2::domain::{BlobRef, Id, ResultContract, ResultKind};
    use kioku::ci2::reports::{NativeEvaluation, evaluate_with_context};

    let observation = native();
    let digest = observation.stdout_digest.clone();
    let blob = BlobRef {
        digest: digest.clone(),
        size_bytes: 1,
        media_type: "application/json".into(),
    };
    let contract = ResultContract {
        id: Id::new("result").expect("fixture id"),
        kind: ResultKind::Test,
        adapter_id: Id::new("adapter").expect("fixture adapter"),
        reporter_version: Id::new("libtest-pretty-v2").expect("reporter version"),
        inventory_recipe_ref: Some(blob.clone()),
        execution_recipe_ref: blob.clone(),
        identity_rule_digest: digest.clone(),
        normalization_policy_ref: blob,
        native_skips: vec![],
        expected_empty: false,
        complete_required: true,
    };
    let context = NativeEvaluation {
        project: "fixture-project",
        profile: &digest,
        now_ms: 2,
        complete: true,
        cancelled: false,
    };
    let report = evaluate_with_context(
        &contract,
        &observation,
        b"fixture_case: test\n",
        b"running 1 test\ntest fixture_case ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
        b"",
        &context,
    );
    assert_eq!(report.verdict, TaskVerdict::Passed);
    assert_eq!(
        reported_test_state(&observation, Some(&report), RunState::Passed, false, false),
        RunState::Passed
    );
    let partial = NativeObservation {
        cpu_complete: false,
        ..observation
    };
    assert_eq!(
        reported_test_state(&partial, Some(&report), RunState::Passed, false, false),
        RunState::Incomplete
    );
}
