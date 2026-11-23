//! Focused report-boundary tests: no healthy verdict without a complete native
//! closure, and no forged workflow directive across the diagnostic seam.
use kioku::ci2::reports::{self, FailureClass, NativeEvaluation, TaskVerdict};
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

const LIST: &[u8] = b"good: test\n";
const OK: &[u8] = b"running 1 test\ntest good ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n";

fn base() -> NativeObservation {
    NativeObservation {
        exit_code: Some(0),
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

fn evaluate(
    kind: ResultKind,
    native: &NativeObservation,
    results: &[u8],
    stderr: &[u8],
    complete: bool,
    cancelled: bool,
) -> reports::TaskReport {
    reports::evaluate_with_context(
        &contract(kind),
        native,
        LIST,
        results,
        stderr,
        &NativeEvaluation {
            project: "fixture-project",
            profile: &digest(),
            now_ms: 1,
            complete,
            cancelled,
        },
    )
}

#[test]
fn pass_requires_a_complete_drained_zero_exit_closure() {
    // Every observation below is accepted by `Validate`, i.e. reachable from
    // stored and supervised native records.
    let partial = NativeObservation {
        cpu_complete: false,
        descendants_drained: false,
        ..base()
    };
    let signalled = NativeObservation {
        signal: Some(15),
        exit_code: None,
        ..base()
    };
    let timed_out = NativeObservation {
        timed_out: true,
        ..base()
    };
    let failed_exit = NativeObservation {
        exit_code: Some(101),
        ..base()
    };
    for native in [&partial, &signalled, &timed_out, &failed_exit] {
        assert!(native.validate().is_ok());
    }
    for (native, complete, cancelled) in [
        (&partial, true, false),
        (&base(), false, false),
        (&base(), true, true),
        (&signalled, true, false),
        (&timed_out, true, false),
    ] {
        for kind in [ResultKind::NonTest, ResultKind::Test] {
            let report = evaluate(kind, native, OK, b"", complete, cancelled);
            assert_ne!(report.verdict, TaskVerdict::Passed);
            assert_ne!(reports::classify(&report), FailureClass::NativeProcess);
        }
    }
    assert_eq!(
        evaluate(ResultKind::Test, &base(), OK, b"", true, false).verdict,
        TaskVerdict::Passed
    );
    assert_eq!(
        evaluate(ResultKind::NonTest, &base(), OK, b"", true, false).verdict,
        TaskVerdict::Passed
    );
    assert_eq!(
        reports::classify(&evaluate(
            ResultKind::Test,
            &failed_exit,
            OK,
            b"",
            true,
            false
        )),
        FailureClass::NativeProcess
    );
}

#[test]
fn diagnostics_never_forge_a_line_start_directive() {
    // Each stream is escaped on its own; concatenation must not rebuild a
    // rendered `::` marker at the start of the joined diagnostics.
    for (label, results, stderr, expected) in [
        (
            "colon-seam",
            &b"x\n:"[..],
            &b":error::forged"[..],
            "x\n:\n:error::forged",
        ),
        (
            "colon-seam-mid",
            &b"x\n::"[..],
            &b"error::forged"[..],
            "x\n[escaped-colon]:\nerror::forged",
        ),
        (
            "plain-seam",
            &b"x\n"[..],
            &b"::error::forged"[..],
            "x\n[escaped-colon]:error::forged",
        ),
    ] {
        let report = evaluate(ResultKind::NonTest, &base(), results, stderr, true, false);
        assert_eq!(report.verdict, TaskVerdict::Passed, "{label}");
        assert_eq!(report.diagnostics, expected, "{label}");
        assert!(
            report
                .diagnostics
                .split_inclusive('\n')
                .all(|line| !line.starts_with("::")),
            "{label}: forged directive"
        );
    }
    // NUL, DEL and non-UTF bytes are escaped; a mid-line marker is untouched.
    assert_eq!(
        reports::render_escaped(b"a\x00b\xffc\x7f\n::error::x"),
        "a\\x00b\\xffc\\x7f\n[escaped-colon]:error::x"
    );
    assert_eq!(reports::render_escaped(b"mid::error::x"), "mid::error::x");
}
