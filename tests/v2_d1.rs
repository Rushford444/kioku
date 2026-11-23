use std::io::{Read, Seek};

use kioku::ci2::domain::{Digest, Id, NativeObservation};
use kioku::ci2::reports::d1::{DecisionEvent, DecisionInput};

fn input() -> DecisionInput {
    let id = |s| Id::new(s).unwrap();
    let digest = || Digest::blake3("test", b"fixture").unwrap();
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
        history_cutoff_ms: Some(9),
    }
}
fn native(exit_code: Option<i32>) -> NativeObservation {
    let digest = Digest::blake3("test", b"fixture").unwrap();
    NativeObservation {
        exit_code,
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
        started_ms: 11,
        ended_ms: 12,
        timed_out: false,
    }
}
#[test]
fn decision_is_log_only_without_training_or_inference() {
    let event = serde_json::to_value(DecisionEvent::new(input()).unwrap()).unwrap();
    assert_eq!(event["mode"], "log_only");
    assert_eq!(event["gate_result"], "off");
    assert_eq!(event["training_allowed"], false);
    assert!(event.get("model_bundle_digest").is_none());
}
#[test]
fn outcome_never_invents_gold_or_action() {
    let event = DecisionEvent::new(input()).unwrap();
    for (code, expected) in [
        (Some(0), "exit_zero"),
        (Some(2), "exit_nonzero"),
        (None, "incomplete"),
    ] {
        let outcome = serde_json::to_value(event.observe(&native(code)).unwrap()).unwrap();
        assert_eq!(outcome["status"], expected);
        assert_eq!(outcome["label_status"], "unresolved");
        assert!(outcome["counterfactual"].is_null());
        assert!(outcome["derived_action"].is_null());
    }
}
#[test]
fn rejects_future_history_and_invalid_timing() {
    let mut i = input();
    i.history_cutoff_ms = Some(11);
    assert!(DecisionEvent::new(i).is_err());
    let event = DecisionEvent::new(input()).unwrap();
    let mut n = native(Some(0));
    n.started_ms = 9;
    assert!(event.observe(&n).is_err());
    n.started_ms = 11;
    n.ended_ms = 10;
    assert!(event.observe(&n).is_err());
}
#[test]
fn timeout_and_undrained_do_not_look_successful() {
    let event = DecisionEvent::new(input()).unwrap();
    let mut n = native(Some(0));
    n.timed_out = true;
    assert_eq!(
        serde_json::to_value(event.observe(&n).unwrap()).unwrap()["status"],
        "timed_out"
    );
    n.timed_out = false;
    n.descendants_drained = false;
    assert_eq!(
        serde_json::to_value(event.observe(&n).unwrap()).unwrap()["status"],
        "incomplete"
    );
}
#[test]
fn actual_child_stream_is_discarded_before_persistence() {
    let secret = "synthetic-secret-do-not-persist";
    let child = std::process::Command::new("printf")
        .arg("%s")
        .arg(secret)
        .output()
        .unwrap();
    assert!(child.status.success());
    assert!(kioku::ci2::reports::d1::public_marker_present(
        &child.stdout
    ));
    let event = DecisionEvent::new(input()).unwrap();
    let mut n = native(child.status.code());
    n.stdout_bytes = child.stdout.len() as u64;
    let file = tempfile::tempfile().unwrap();
    serde_json::to_writer(&file, &event).unwrap();
    serde_json::to_writer(&file, &event.observe(&n).unwrap()).unwrap();
    let mut file = file;
    file.rewind().unwrap();
    let mut saved = String::new();
    file.read_to_string(&mut saved).unwrap();
    assert!(!saved.contains(secret));
}
