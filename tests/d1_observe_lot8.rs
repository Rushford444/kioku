use kioku::select::d1_observe::{Observer, ObserverError, SCHEMA, Stub, observe};
use kioku::select::{D1Proposal, TestId};
use std::collections::BTreeSet;
use std::fs;
fn test(name: &str) -> TestId {
    TestId {
        binary_id: "suite".into(),
        name: name.into(),
    }
}
struct Failed;
impl Observer for Failed {
    fn propose(&self) -> Result<D1Proposal, ObserverError> {
        Err(ObserverError)
    }
}
#[test]
fn add_and_skip_are_audited_without_removing_tests() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("audit.jsonl");
    let selected = BTreeSet::from([test("existing")]);
    let stub = Stub {
        proposal: D1Proposal {
            add: vec![test("new"), test("new"), test("existing")],
            skip: vec![test("existing"), test("new")],
        },
    };
    let result = observe(&selected, Some(&stub), "fixed-time", &journal).unwrap();
    assert_eq!(result, BTreeSet::from([test("existing"), test("new")]));
    let content = fs::read_to_string(&journal).unwrap();
    let events: Vec<serde_json::Value> = content
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0]["schema"], SCHEMA);
    assert_eq!(events[0]["timestamp"], "fixed-time");
    assert_eq!(events[0]["action"], "would-add");
    assert_eq!(events[0]["test"]["name"], "new");
    assert_eq!(events[1]["action"], "skip-refused");
    assert_eq!(events[1]["test"]["name"], "existing");
    assert_eq!(events[2]["test"]["name"], "new");
    observe(&selected, Some(&stub), "fixed-time", &journal).unwrap();
    assert_eq!(
        fs::read_to_string(journal).unwrap(),
        format!("{content}{content}")
    );
}
#[test]
fn unavailable_observer_and_journal_failure_leave_baseline_to_caller() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("audit.jsonl");
    let selected = BTreeSet::from([test("existing")]);
    assert_eq!(observe(&selected, None, "t", &journal).unwrap(), selected);
    assert_eq!(
        observe(&selected, Some(&Failed), "t", &journal).unwrap(),
        selected
    );
    assert!(!journal.exists());
    let stub = Stub {
        proposal: D1Proposal {
            add: vec![test("new")],
            skip: vec![],
        },
    };
    assert!(observe(&selected, Some(&stub), "t", dir.path()).is_err());
    assert_eq!(selected, BTreeSet::from([test("existing")]));
}
