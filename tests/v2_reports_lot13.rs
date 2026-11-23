//! Retained report parser regressions; these do not prove daemon integration.
use kioku::ci2::reports::{self, TaskVerdict, evaluate};
use kioku::ci2::*;

fn id(s: &str) -> Id {
    Id::new(s).expect("id")
}
fn digest() -> Digest {
    Digest::new(&format!("b3:{}", "a".repeat(64))).expect("digest")
}
fn blob() -> BlobRef {
    BlobRef {
        digest: digest(),
        size_bytes: 1,
        media_type: "application/json".into(),
    }
}
fn contract(kind: ResultKind) -> ResultContract {
    ResultContract {
        id: id("result"),
        kind,
        adapter_id: id("adapter"),
        reporter_version: id("libtest-pretty-v2"),
        inventory_recipe_ref: Some(blob()),
        execution_recipe_ref: blob(),
        identity_rule_digest: digest(),
        normalization_policy_ref: blob(),
        native_skips: vec![],
        expected_empty: false,
        complete_required: true,
    }
}
fn native(code: i32) -> NativeObservation {
    NativeObservation {
        exit_code: Some(code),
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
const LIST: &[u8] = b"tests::foo: test\n\n1 test, 0 benchmarks\n";
fn run(c: &ResultContract, n: &NativeObservation, list: &[u8], result: &[u8]) -> TaskVerdict {
    evaluate(c, n, list, result, b"", &digest(), 1, true, false).verdict
}
#[test]
fn real_terse_inventory_and_verbose_results_are_qualified() {
    let list = b"good: test\nignored: test\n";
    let output = b"\nrunning 2 tests\ntest ignored ... ignored, reason\ntest good ... ok\n\ntest result: ok. 1 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s\n\n";
    let mut c = contract(ResultKind::Test);
    c.native_skips.push(NativeSkipRule {
        case_id: reports::case_id("fixture-project", "adapter", "ignored", "").unwrap(),
        profile_digest: digest(),
        reason_code: id("reason"),
        approval_ref: blob(),
        expires_at_ms: 10,
    });
    let profile = digest();
    let context = reports::NativeEvaluation {
        project: "fixture-project",
        profile: &profile,
        now_ms: 1,
        complete: true,
        cancelled: false,
    };
    let qualified = reports::evaluate_with_context(&c, &native(0), list, output, b"", &context);
    assert_eq!(qualified.verdict, TaskVerdict::Passed);
    assert_eq!(qualified.cases.as_ref().unwrap().len(), 2);
    assert!(
        qualified
            .cases
            .as_ref()
            .unwrap()
            .iter()
            .all(|case| case.case_id.is_some())
    );
    assert_eq!(reports::classify(&qualified), reports::FailureClass::None);
    c.native_skips[0].expires_at_ms = 1;
    assert_eq!(
        reports::evaluate_with_context(&c, &native(0), list, output, b"", &context).verdict,
        TaskVerdict::Incomplete
    );
    c.native_skips[0].expires_at_ms = 10;
    assert_eq!(
        reports::evaluate_with_context(&c, &native(0), list, output, b"", &context).verdict,
        TaskVerdict::Passed
    );
    assert_eq!(
        reports::evaluate_with_context(
            &c,
            &native(0),
            b"good: test\ngood: test\nignored: test\n",
            output,
            b"",
            &context
        )
        .verdict,
        TaskVerdict::Incomplete
    );
    assert_eq!(
        reports::evaluate_with_context(&c, &native(0), b"good: test\n", output, b"", &context)
            .verdict,
        TaskVerdict::Incomplete
    );
}

#[test]
fn empty_and_unrecognized_setup_teardown_and_terse_execution_fail_closed() {
    let mut c = contract(ResultKind::Test);
    let empty = b"running 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
    assert_eq!(run(&c, &native(0), b"", empty), TaskVerdict::Incomplete);
    c.expected_empty = true;
    assert_eq!(run(&c, &native(0), b"", empty), TaskVerdict::Passed);
    assert_eq!(run(&c, &native(0), b"", b"setup failed\nrunning 0 tests\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n"), TaskVerdict::Incomplete);
    assert_eq!(run(&c, &native(0), b"", b"running 0 tests\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\nteardown failed\n"), TaskVerdict::Incomplete);
    assert_eq!(run(&c, &native(0), b"good: test\n", b"running 1 test\n.\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n"), TaskVerdict::Incomplete);
}

#[test]
fn malformed_or_duplicate_summaries_are_not_success() {
    let c = contract(ResultKind::Test);
    let duplicate = b"running 1 test\ntest tests::foo ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n";
    assert_eq!(
        run(&c, &native(0), LIST, duplicate),
        TaskVerdict::Incomplete
    );
    let false_counts = b"running 1 test\ntest tests::foo ... ok\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n";
    assert_eq!(
        run(&c, &native(0), LIST, false_counts),
        TaskVerdict::Incomplete
    );
}
