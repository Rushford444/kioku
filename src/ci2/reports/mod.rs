//! Conservative adapters for native command output. The typed `TaskReport::diagnostics`
//! retains escaped, not redacted, native results/stderr under the established JSON contract.
pub mod d1;
// Kept for report binding tests.
#[allow(dead_code)]
pub(crate) mod native_binding;
pub mod native_outcome;
use crate::ci2::domain::{
    ContractError, Digest, Id, NativeObservation, ResultContract, ResultKind, Validate,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const REPORTER_VERSION: &str = "libtest-pretty-v2";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskVerdict {
    Passed,
    Failed,
    Incomplete,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseResult {
    pub raw: String,
    pub case_id: Option<Id>,
    pub status: CaseStatus,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CaseStatus {
    Passed,
    Failed,
    Ignored { reason: Option<String> },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskReport {
    pub verdict: TaskVerdict,
    pub exit_code: Option<i32>,
    pub cases: Option<Vec<CaseResult>>,
    pub diagnostics: String,
}

/// Failure classification does not infer success from an unclassified report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    None,
    TestCase,
    NativeProcess,
    Incomplete,
    Cancelled,
}

/// Classify using only known case failures and the native process outcome.
#[must_use]
pub fn classify(report: &TaskReport) -> FailureClass {
    if report
        .cases
        .as_ref()
        .is_some_and(|cases| cases.iter().any(|case| case.status == CaseStatus::Failed))
    {
        FailureClass::TestCase
    } else if report.exit_code.is_some_and(|code| code != 0) {
        FailureClass::NativeProcess
    } else {
        match report.verdict {
            TaskVerdict::Passed => FailureClass::None,
            TaskVerdict::Cancelled => FailureClass::Cancelled,
            TaskVerdict::Failed | TaskVerdict::Incomplete => FailureClass::Incomplete,
        }
    }
}

/// Escape untrusted bytes for a terminal. The original byte stream is not modified.
#[must_use]
pub fn render_escaped(bytes: &[u8]) -> String {
    let mut out = String::new();
    for byte in bytes {
        match *byte {
            b' '..=b'~' => out.push(char::from(*byte)),
            b'\n' => out.push('\n'),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            _ => {
                use std::fmt::Write;
                out.push(char::from(92));
                let _ = write!(out, "x{byte:02x}");
            }
        }
    }
    out.split_inclusive('\n')
        .map(|line| {
            line.strip_prefix("::")
                .map_or_else(|| line.to_owned(), |rest| format!("[escaped-colon]:{rest}"))
        })
        .collect()
}

/// Derive an opaque identity from four length-prefixed UTF-8 components.
///
/// # Errors
/// Returns an error if the digest domain or generated identifier is invalid.
pub fn case_id(
    project: &str,
    adapter: &str,
    raw_name: &str,
    parameter: &str,
) -> Result<Id, ContractError> {
    let mut data = Vec::new();
    for part in [project, adapter, raw_name, parameter] {
        data.extend_from_slice(&(part.len() as u64).to_be_bytes());
        data.extend_from_slice(part.as_bytes());
    }
    let digest = Digest::blake3("kioku/ci2/case-id/v1", &data)?;
    Id::new(format!("case_{}", digest.as_hex()))
}

/// Identity context must describe the actual run, not an inferred project.
pub struct NativeEvaluation<'a> {
    pub project: &'a str,
    pub profile: &'a Digest,
    pub now_ms: u64,
    pub complete: bool,
    pub cancelled: bool,
}

fn valid_case(s: &str) -> bool {
    !s.is_empty()
        && s.is_ascii()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b':' | b'-' | b'.'))
}

/// Caller must pass complete, separately captured inventory and execution streams from
/// the same snapshot and profile. `complete` also covers truncation and collection errors.
#[must_use]
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn evaluate(
    contract: &ResultContract,
    native: &NativeObservation,
    inventory: &[u8],
    results: &[u8],
    stderr: &[u8],
    profile: &Digest,
    now_ms: u64,
    complete: bool,
    cancelled: bool,
) -> TaskReport {
    evaluate_inner(
        contract, native, inventory, results, stderr, profile, now_ms, complete, cancelled, None,
    )
}

/// Evaluate with an explicit project identity, allowing approved native skips.
#[must_use]
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn evaluate_with_context(
    contract: &ResultContract,
    native: &NativeObservation,
    inventory: &[u8],
    results: &[u8],
    stderr: &[u8],
    context: &NativeEvaluation<'_>,
) -> TaskReport {
    evaluate_inner(
        contract,
        native,
        inventory,
        results,
        stderr,
        context.profile,
        context.now_ms,
        context.complete,
        context.cancelled,
        Some(context.project),
    )
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn evaluate_inner(
    contract: &ResultContract,
    native: &NativeObservation,
    inventory: &[u8],
    results: &[u8],
    stderr: &[u8],
    profile: &Digest,
    now_ms: u64,
    complete: bool,
    cancelled: bool,
    project: Option<&str>,
) -> TaskReport {
    let mut verdict = if native.exit_code.is_some_and(|code| code != 0) {
        TaskVerdict::Failed
    } else if cancelled {
        TaskVerdict::Cancelled
    } else {
        TaskVerdict::Incomplete
    };
    let mut diagnostics = render_escaped(results);
    // Each stream is escaped independently, so a rendered `::` is only neutralised
    // at a line start *within* its own stream. Without a separator, results ending
    // in a bare colon and stderr starting with a colon would join into a forged
    // line-start directive.
    if !diagnostics.is_empty() && !diagnostics.ends_with('\n') {
        diagnostics.push('\n');
    }
    diagnostics.push_str(&render_escaped(stderr));
    let mut cases = None;
    let healthy = contract.validate().is_ok()
        && complete
        && !cancelled
        && !native.timed_out
        && native.signal.is_none()
        && native.exit_code == Some(0)
        && native.descendants_drained;
    if contract.kind == ResultKind::NonTest {
        if healthy {
            verdict = TaskVerdict::Passed;
        }
    } else if contract.kind == ResultKind::Test {
        let mut bad = !complete
            || contract.inventory_recipe_ref.is_none()
            || contract.reporter_version.as_str() != REPORTER_VERSION;
        let mut names = BTreeSet::new();
        let mut seen = BTreeSet::new();
        let inv = std::str::from_utf8(inventory);
        let res = std::str::from_utf8(results);
        let mut inventory_summary = false;
        let mut result_summary = false;
        let mut running_count = None;
        if let Ok(inv) = inv {
            for line in inv.lines() {
                if let Some(name) = line.strip_suffix(": test") {
                    if inventory_summary || !valid_case(name) || !names.insert(name.to_owned()) {
                        bad = true;
                    }
                } else if line.ends_with(" tests, 0 benchmarks")
                    || line.ends_with(" test, 0 benchmarks")
                {
                    if inventory_summary
                        || line
                            != format!(
                                "{} {}, 0 benchmarks",
                                names.len(),
                                if names.len() == 1 { "test" } else { "tests" }
                            )
                    {
                        bad = true;
                    }
                    inventory_summary = true;
                } else if !line.is_empty() {
                    bad = true;
                }
            }
        } else {
            bad = true;
        }
        let mut list = Vec::new();
        if let Ok(res) = res {
            for line in res.lines() {
                if line.is_empty() {
                    continue;
                }
                if let Some(count) = line
                    .strip_prefix("running ")
                    .and_then(|s| s.strip_suffix(" tests").or_else(|| s.strip_suffix(" test")))
                    .and_then(|s| s.parse::<usize>().ok())
                {
                    if running_count.replace(count).is_some() || !list.is_empty() || result_summary
                    {
                        bad = true;
                    }
                    continue;
                }
                if line.starts_with("test result: ") {
                    if result_summary {
                        bad = true;
                    }
                    result_summary = true;
                    let passed = list
                        .iter()
                        .filter(|c: &&CaseResult| c.status == CaseStatus::Passed)
                        .count();
                    let failed = list
                        .iter()
                        .filter(|c: &&CaseResult| c.status == CaseStatus::Failed)
                        .count();
                    let ignored = list
                        .iter()
                        .filter(|c: &&CaseResult| matches!(c.status, CaseStatus::Ignored { .. }))
                        .count();
                    let outcome = if failed == 0 { "ok" } else { "FAILED" };
                    let expected = format!(
                        "test result: {outcome}. {passed} passed; {failed} failed; {ignored} ignored; 0 measured; 0 filtered out"
                    );
                    let valid_summary = line == expected
                        || line
                            .strip_prefix(&format!("{expected}; finished in "))
                            .and_then(|duration| duration.strip_suffix('s'))
                            .is_some_and(|duration| {
                                let mut parts = duration.split('.');
                                parts.next().is_some_and(|s| {
                                    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
                                }) && parts.next().is_some_and(|s| {
                                    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
                                }) && parts.next().is_none()
                            });
                    if !valid_summary {
                        bad = true;
                    }
                    if line.starts_with("test result: FAILED") {
                        verdict = TaskVerdict::Failed;
                    }
                    continue;
                }
                if let Some(rest) = line.strip_prefix("test ") {
                    if result_summary {
                        bad = true;
                    }
                    if let Some((name, status)) = rest.rsplit_once(" ... ") {
                        let status = if status == "ok" {
                            Some(CaseStatus::Passed)
                        } else if status == "FAILED" {
                            Some(CaseStatus::Failed)
                        } else if status == "ignored" {
                            Some(CaseStatus::Ignored { reason: None })
                        } else {
                            status
                                .strip_prefix("ignored, ")
                                .map(|r| CaseStatus::Ignored {
                                    reason: Some(r.to_owned()),
                                })
                        };
                        if !valid_case(name) || status.is_none() {
                            bad = true;
                            continue;
                        }
                        if !seen.insert(name.to_owned()) {
                            bad = true;
                        }
                        if let Some(status) = status {
                            if status == CaseStatus::Failed {
                                verdict = TaskVerdict::Failed;
                            }
                            let normalized = project.and_then(|project| {
                                case_id(project, contract.adapter_id.as_str(), name, "").ok()
                            });
                            if let CaseStatus::Ignored { reason } = &status {
                                let matching = contract
                                    .native_skips
                                    .iter()
                                    .filter(|r| {
                                        normalized.as_ref() == Some(&r.case_id)
                                            && &r.profile_digest == profile
                                            && r.expires_at_ms > now_ms
                                            && reason.as_deref() == Some(r.reason_code.as_str())
                                    })
                                    .count();
                                if matching != 1 {
                                    bad = true;
                                }
                            }
                            list.push(CaseResult {
                                raw: name.to_owned(),
                                case_id: normalized,
                                status,
                            });
                        }
                    } else {
                        bad = true;
                    }
                } else {
                    bad = true;
                }
            }
        } else {
            bad = true;
        }
        if !result_summary
            || running_count != Some(names.len())
            || names != seen
            || (names.is_empty() && !contract.expected_empty)
        {
            bad = true;
        }
        if !bad && healthy && verdict != TaskVerdict::Failed {
            verdict = TaskVerdict::Passed;
        }
        cases = Some(list);
    }
    TaskReport {
        verdict,
        exit_code: native.exit_code,
        cases,
        diagnostics,
    }
}
