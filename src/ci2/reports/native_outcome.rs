//! Pure terminal-state evaluation for already parsed native test output.
//!
//! This module does not read streams, parse reports, or persist state. Callers retain
//! ownership of durable terminal states and of non-test task evaluation.

use super::{FailureClass, TaskReport, TaskVerdict, classify};
use crate::ci2::domain::{NativeObservation, RunState};

/// Select a test task state after reading and evaluating its selected output.
///
/// A pass requires an explicit passed report, a zero native exit without signal or
/// timeout, complete CPU accounting, drained descendants, and complete selected
/// output. Missing reports or unknown closure never imply success. Known case
/// failures retain their failure classification, even with incomplete output.
///
/// `run` is the enclosing run state, not a previous durable task state.
/// `known_error` prevents success when that run is incomplete; an unrelated task's
/// failure does not invalidate this task. `task_output_error` covers any incomplete
/// or unreadable selected inventory/execution output. An intentional cancellation
/// requires complete CPU accounting and drained descendants. A timeout with that
/// closure is a failure, otherwise it is incomplete.
///
/// Callers must preserve already durable task states rather than recomputing them.
/// The report and native observation must belong to the same execution.
#[must_use]
pub fn reported_test_state(
    native: &NativeObservation,
    report: Option<&TaskReport>,
    run: RunState,
    known_error: bool,
    task_output_error: bool,
) -> RunState {
    if report.is_some_and(|report| classify(report) == FailureClass::TestCase) {
        return RunState::Failed;
    }
    if run == RunState::Cancelled {
        return if native.cpu_complete && native.descendants_drained {
            RunState::Cancelled
        } else {
            RunState::Incomplete
        };
    }
    if !native.timed_out
        && (native.signal.is_some() || native.exit_code.is_some_and(|code| code != 0))
    {
        return RunState::Failed;
    }
    if report.is_some_and(|report| {
        report.verdict == TaskVerdict::Failed || classify(report) == FailureClass::NativeProcess
    }) {
        return RunState::Failed;
    }
    if !native.cpu_complete || !native.descendants_drained {
        return RunState::Incomplete;
    }
    if native.timed_out {
        return RunState::Failed;
    }
    if native.exit_code != Some(0)
        || native.signal.is_some()
        || task_output_error
        || (run == RunState::Incomplete && known_error)
    {
        return RunState::Incomplete;
    }
    match report.map(|report| report.verdict) {
        Some(TaskVerdict::Passed) => RunState::Passed,
        Some(TaskVerdict::Cancelled) => RunState::Cancelled,
        Some(TaskVerdict::Failed) => RunState::Failed,
        Some(TaskVerdict::Incomplete) | None => RunState::Incomplete,
    }
}
