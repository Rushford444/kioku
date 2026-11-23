//! Minimal, local decision journal. Inputs are opaque identities and digests, never streams.
use crate::ci2::domain::{ContractError, Digest, Id, NativeObservation, check_range};
use serde::Serialize;

/// Immutable decision-time inputs. IDs must be opaque, not names, paths or secrets.
#[derive(Debug, Clone)]
pub struct DecisionInput {
    pub event_id: Id,
    pub run_id: Id,
    pub project_id: Id,
    pub trust_scope: Id,
    pub outcome_link_id: Id,
    pub decision_at_ms: u64,
    pub snapshot_digest: Digest,
    pub profile_digest: Digest,
    pub policy_digest: Digest,
    pub deterministic_plan_digest: Digest,
    /// Latest historical fact used by the caller, if any. Future facts are rejected.
    pub history_cutoff_ms: Option<u64>,
}

/// No raw stream, environment, model response or training input is accepted here.
#[derive(Debug, Serialize)]
pub struct DecisionEvent {
    schema: &'static str,
    revision: u8,
    event_id: Id,
    run_id: Id,
    project_id: Id,
    trust_scope: Id,
    outcome_link_id: Id,
    family: &'static str,
    decision_at_ms: u64,
    snapshot_digest: Digest,
    profile_digest: Digest,
    policy_digest: Digest,
    deterministic_plan_digest: Digest,
    history_cutoff_ms: Option<u64>,
    mode: &'static str,
    gate_result: &'static str,
    training_allowed: bool,
}

impl DecisionEvent {
    /// Create a log-only event before the outcome is known.
    ///
    /// # Errors
    /// Rejects out-of-range timestamps and history after the decision.
    pub fn new(input: DecisionInput) -> Result<Self, ContractError> {
        check_range(input.decision_at_ms)?;
        if let Some(cutoff) = input.history_cutoff_ms {
            check_range(cutoff)?;
            if cutoff > input.decision_at_ms {
                return Err(ContractError::Configuration("future history".into()));
            }
        }
        Ok(Self {
            schema: "kioku/ci2/decision-event/v1",
            revision: 1,
            event_id: input.event_id,
            run_id: input.run_id,
            project_id: input.project_id,
            trust_scope: input.trust_scope,
            outcome_link_id: input.outcome_link_id,
            family: "execution_risk",
            decision_at_ms: input.decision_at_ms,
            snapshot_digest: input.snapshot_digest,
            profile_digest: input.profile_digest,
            policy_digest: input.policy_digest,
            deterministic_plan_digest: input.deterministic_plan_digest,
            history_cutoff_ms: input.history_cutoff_ms,
            mode: "log_only",
            gate_result: "off",
            training_allowed: false,
        })
    }

    /// Link a native observation without treating its exit as an oracle or test verdict.
    ///
    /// # Errors
    /// Rejects timestamps outside the wire range or observations preceding the decision.
    pub fn observe(&self, native: &NativeObservation) -> Result<Outcome, ContractError> {
        check_range(native.started_ms)?;
        check_range(native.ended_ms)?;
        if native.started_ms < self.decision_at_ms || native.ended_ms < native.started_ms {
            return Err(ContractError::Configuration(
                "invalid observation time".into(),
            ));
        }
        let status = if native.timed_out {
            ObservedStatus::TimedOut
        } else if native.signal.is_some() {
            ObservedStatus::Signalled
        } else if !native.descendants_drained {
            ObservedStatus::Incomplete
        } else {
            match native.exit_code {
                Some(0) => ObservedStatus::ExitZero,
                Some(_) => ObservedStatus::ExitNonzero,
                None => ObservedStatus::Incomplete,
            }
        };
        Ok(Outcome {
            schema: "kioku/ci2/outcome-link/v1",
            id: self.outcome_link_id.clone(),
            event_id: self.event_id.clone(),
            run_id: self.run_id.clone(),
            observed_at_ms: native.ended_ms,
            status,
            label_status: "unresolved",
            counterfactual: None,
            derived_action: None,
        })
    }
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ObservedStatus {
    ExitZero,
    ExitNonzero,
    TimedOut,
    Signalled,
    Incomplete,
}

/// Separate append-only outcome record. The caller owns atomic persistence.
#[derive(Debug, Serialize)]
pub struct Outcome {
    schema: &'static str,
    id: Id,
    event_id: Id,
    run_id: Id,
    observed_at_ms: u64,
    status: ObservedStatus,
    label_status: &'static str,
    counterfactual: Option<()>,
    derived_action: Option<()>,
}

/// Deliberately discard an untrusted public marker or stream before serialization.
/// The boolean indicates presence only; no content, digest, or length is retained.
#[must_use]
pub fn public_marker_present(bytes: &[u8]) -> bool {
    !bytes.is_empty()
}

#[cfg(test)]
mod f3c_tests {
    use super::*;

    fn input() -> DecisionInput {
        let id = |value| Id::new(value).unwrap();
        let digest = || Digest::blake3("f3c", b"synthetic").unwrap();
        DecisionInput {
            event_id: id("event"),
            run_id: id("run"),
            project_id: id("project"),
            trust_scope: id("scope"),
            outcome_link_id: id("outcome"),
            decision_at_ms: 10,
            snapshot_digest: digest(),
            profile_digest: digest(),
            policy_digest: digest(),
            deterministic_plan_digest: digest(),
            history_cutoff_ms: Some(10),
        }
    }

    #[test]
    fn outcome_precedence_does_not_turn_incomplete_observations_into_zero() {
        let event = DecisionEvent::new(input()).unwrap();
        let digest = Digest::blake3("f3c", b"synthetic").unwrap();
        let mut native = NativeObservation {
            exit_code: Some(0),
            signal: None,
            stdout_digest: digest.clone(),
            stderr_digest: digest,
            stdout_bytes: 0,
            stderr_bytes: 0,
            wall_ns: 0,
            cpu_user_ns: 0,
            cpu_system_ns: 0,
            cpu_complete: false,
            descendants_drained: true,
            started_ms: 10,
            ended_ms: 10,
            timed_out: false,
        };
        for (exit, signal, timeout, drained, expected) in [
            (Some(0), None, false, true, "exit_zero"),
            (Some(9), None, false, true, "exit_nonzero"),
            (None, None, false, true, "incomplete"),
            (Some(0), None, false, false, "incomplete"),
            (Some(0), Some(9), false, false, "signalled"),
            (Some(0), Some(9), true, false, "timed_out"),
        ] {
            native.exit_code = exit;
            native.signal = signal;
            native.timed_out = timeout;
            native.descendants_drained = drained;
            let outcome = serde_json::to_value(event.observe(&native).unwrap()).unwrap();
            assert_eq!(outcome["status"], expected);
            assert_eq!(outcome["label_status"], "unresolved");
            assert!(outcome["counterfactual"].is_null());
            assert!(outcome["derived_action"].is_null());
        }
    }

    #[test]
    fn opaque_identifiers_are_rejected_without_normalization() {
        for id in ["", ".event", "event/name", " event", "évent", "event\n"] {
            assert!(Id::new(id).is_err(), "accepted {id:?}");
        }
        assert_eq!(Id::new("event-1").unwrap().as_str(), "event-1");
        let mut future = input();
        future.history_cutoff_ms = Some(11);
        assert!(DecisionEvent::new(future).is_err());
    }
}
