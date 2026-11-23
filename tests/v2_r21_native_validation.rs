use kioku::ci2::reports::{self, TaskVerdict};
use kioku::ci2::*;

fn native() -> NativeObservation {
    NativeObservation {
        exit_code: Some(0),
        signal: None,
        stdout_digest: Digest::new(&format!("sha256:{}", "a".repeat(64))).unwrap(),
        stderr_digest: Digest::new(&format!("sha256:{}", "a".repeat(64))).unwrap(),
        stdout_bytes: 0,
        stderr_bytes: 0,
        wall_ns: 1,
        cpu_user_ns: 0,
        cpu_system_ns: 0,
        cpu_complete: true,
        descendants_drained: true,
        started_ms: 1,
        ended_ms: 2,
        timed_out: false,
    }
}

fn decode(value: &NativeObservation) -> Result<NativeObservation, ContractError> {
    decode_bounded(&serde_json::to_vec(value).unwrap())
}

fn report(value: &NativeObservation) -> reports::TaskReport {
    let blob = BlobRef {
        digest: value.stdout_digest.clone(),
        size_bytes: 0,
        media_type: "application/json".into(),
    };
    let contract = ResultContract {
        id: Id::new("result").unwrap(),
        kind: ResultKind::OpaqueDiagnostic,
        adapter_id: Id::new("adapter").unwrap(),
        reporter_version: Id::new("libtest-pretty-v2").unwrap(),
        inventory_recipe_ref: Some(blob.clone()),
        execution_recipe_ref: blob.clone(),
        identity_rule_digest: value.stdout_digest.clone(),
        normalization_policy_ref: blob,
        native_skips: vec![],
        expected_empty: false,
        complete_required: false,
    };
    contract.validate().unwrap();
    reports::evaluate(
        &contract,
        value,
        b"",
        b"",
        b"",
        &value.stdout_digest,
        2,
        true,
        false,
    )
}

#[test]
fn exit_and_signal_cannot_both_describe_the_leader() {
    for code in [0, 1, 255] {
        let contradictory = NativeObservation {
            exit_code: Some(code),
            signal: Some(15),
            ..native()
        };
        assert!(contradictory.validate().is_err(), "exit {code} plus signal");
        assert!(decode(&contradictory).is_err());
    }
}

#[test]
fn exclusive_native_outcomes_remain_observations_not_test_success() {
    for (exit_code, signal, verdict) in [
        (Some(0), None, TaskVerdict::Incomplete),
        (Some(1), None, TaskVerdict::Failed),
        (None, Some(15), TaskVerdict::Incomplete),
    ] {
        let observation = NativeObservation {
            exit_code,
            signal,
            ..native()
        };
        observation.validate().unwrap();
        let decoded = decode(&observation).unwrap();
        assert_eq!(decoded, observation);
        assert_eq!(report(&decoded).verdict, verdict);
    }
}

#[test]
fn missing_leader_status_remains_admissible_but_incomplete() {
    let observation = NativeObservation {
        exit_code: None,
        cpu_complete: false,
        descendants_drained: false,
        ..native()
    };
    observation.validate().unwrap();
    let decoded = decode(&observation).unwrap();
    assert_eq!(report(&decoded).verdict, TaskVerdict::Incomplete);
}
