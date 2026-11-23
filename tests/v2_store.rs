#![cfg(unix)]
use kioku::ci2::{
    domain::*,
    storage::{JournalError, LocalJournal},
};
use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::Write;

fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}
fn blob() -> BlobRef {
    BlobRef {
        digest: Digest::blake3("fixture", b"content").unwrap(),
        size_bytes: 7,
        media_type: "application/json".into(),
    }
}
fn setup(root: &std::path::Path) -> (ServiceBootstrap, ClientIdentity, LocalRequest) {
    let reference = blob();
    let config = ServiceBootstrap {
        schema: "kioku/ci2/bootstrap/v1".into(),
        revision: 1,
        service_id: id("service"),
        mode: ServiceMode::LocalCooperative,
        resource_domain_id: id("domain"),
        state_root: root.join("state").to_str().unwrap().into(),
        socket_path: root.join("socket").to_str().unwrap().into(),
        stop_file: root.join("stop").to_str().unwrap().into(),
        registered_roots: vec![RegisteredRoot {
            id: id("root"),
            project_id: id("project"),
            canonical_path: root.to_str().unwrap().into(),
            allowed_principals: vec![id("alice")],
            protected_refs: vec![],
        }],
        approved_manifest_refs: vec![reference.clone()],
        profiles_ref: reference.clone(),
        policy_ref: reference.clone(),
        auth_config_ref: None,
        capture_slots: 1,
        heavy_slots: 1,
        max_state_bytes: 1_000_000,
        max_queue_wait_ms: 1,
        drain_grace_ms: 1,
    };
    let identity = ClientIdentity {
        principal_id: id("alice"),
        project_ids: vec![id("project")],
        roles: vec![Role::Requester],
        trust_scope: id("local"),
        lane_id: None,
    };
    let request = LocalRequest {
        schema: "kioku/ci2/local-request/v1".into(),
        revision: 1,
        request_id: id("request"),
        project_id: id("project"),
        origin_claim: OriginClaim::LocalRegisteredCheckout {
            root_id: id("root"),
            relative_path: RelPath::new("work").unwrap(),
        },
        manifest_digest: reference.digest,
        profile_id: id("profile"),
        requested_task_ids: vec![id("task")],
        mode: RunMode::Normal,
        wait_class: WaitClass::Agent,
    };
    (config, identity, request)
}
fn digest(request: &LocalRequest) -> Digest {
    digest_request_document(&serde_json::to_value(request).unwrap()).unwrap()
}

#[test]
fn admission_survives_reopen_and_conflicts_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let (config, identity, request) = setup(dir.path());
    let mut journal = LocalJournal::open(config.clone()).unwrap();
    assert!(matches!(
        LocalJournal::open(config.clone()),
        Err(JournalError::Locked)
    ));
    let first = journal
        .accept(&identity, &request, digest(&request))
        .unwrap();
    assert_eq!(
        first,
        journal
            .accept(&identity, &request, digest(&request))
            .unwrap()
    );
    let mut changed = request.clone();
    changed.profile_id = id("other");
    assert!(matches!(
        journal.accept(&identity, &changed, digest(&changed)),
        Err(JournalError::Conflict)
    ));
    drop(journal);
    let mut reopened = LocalJournal::open(config).unwrap();
    assert_eq!(
        first,
        reopened
            .accept(&identity, &request, digest(&request))
            .unwrap()
    );
}

#[test]
fn phases_and_cancellation_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let (config, identity, request) = setup(dir.path());
    let mut journal = LocalJournal::open(config.clone()).unwrap();
    let accepted = journal
        .accept(&identity, &request, digest(&request))
        .unwrap();
    let initial = journal.load_run(&accepted.run_id).unwrap();
    assert_eq!(initial.state, RunState::Queued);
    assert_eq!(initial.event_cursor, 1);
    assert_eq!(
        journal
            .start_run(&accepted.run_id, RunState::Capturing)
            .unwrap(),
        2
    );
    assert!(matches!(
        journal.cancel(&accepted.run_id, &id("mallory")),
        Err(JournalError::Unauthorized)
    ));
    assert_eq!(
        journal
            .cancel(&accepted.run_id, &identity.principal_id)
            .unwrap(),
        RunState::Cancelled
    );
    assert_eq!(
        journal
            .cancel(&accepted.run_id, &identity.principal_id)
            .unwrap(),
        RunState::Cancelled
    );
    assert!(
        journal
            .start_run(&accepted.run_id, RunState::Running)
            .is_err()
    );
    drop(journal);
    let mut journal = LocalJournal::open(config).unwrap();
    let recovered = journal.load_run(&accepted.run_id).unwrap();
    assert_eq!(recovered.state, RunState::Cancelled);
    assert_eq!(recovered.event_cursor, 3);
    assert_eq!(journal.all_runs().unwrap(), vec![recovered]);
    assert_eq!(
        journal
            .accept(&identity, &request, digest(&request))
            .unwrap(),
        accepted
    );
}

#[test]
fn terminal_and_reversed_phases_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (config, identity, request) = setup(dir.path());
    let mut journal = LocalJournal::open(config).unwrap();
    let accepted = journal
        .accept(&identity, &request, digest(&request))
        .unwrap();
    assert!(
        journal
            .start_run(&accepted.run_id, RunState::Passed)
            .is_err()
    );
    assert_eq!(
        journal
            .start_run(&accepted.run_id, RunState::Running)
            .unwrap(),
        2
    );
    assert!(
        journal
            .cancel(&accepted.run_id, &identity.principal_id)
            .is_err()
    );
    assert!(
        journal
            .start_run(&accepted.run_id, RunState::Capturing)
            .is_err()
    );
    assert_eq!(
        journal
            .start_run(&accepted.run_id, RunState::Collecting)
            .unwrap(),
        3
    );
}

#[test]
fn truncated_journal_and_unauthorized_identity_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (config, mut identity, request) = setup(dir.path());
    let mut journal = LocalJournal::open(config.clone()).unwrap();
    identity.principal_id = id("mallory");
    assert!(matches!(
        journal.accept(&identity, &request, digest(&request)),
        Err(JournalError::Unauthorized)
    ));
    drop(journal);
    let mut file = OpenOptions::new()
        .append(true)
        .open(dir.path().join("state/admissions.jsonl"))
        .unwrap();
    file.write_all(b"{\"broken\":").unwrap();
    file.sync_all().unwrap();
    assert!(matches!(
        LocalJournal::open(config),
        Err(JournalError::Corrupt(_))
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn orphan_binding_is_durable_and_never_released_without_proof() {
    use kioku::ci2::storage::ProcessEvidence;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    struct Owned(Child);
    impl Drop for Owned {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut command = Command::new("/usr/bin/sleep");
    command
        .arg("60")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = Owned(command.spawn().unwrap());
    let pid = child.0.id();
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let fields: Vec<_> = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_ascii_whitespace()
        .collect();
    let evidence = ProcessEvidence {
        process: ProcessIdentity {
            pid,
            pgid: pid,
            started_ms: 1,
        },
        start_ticks: fields[19].parse().unwrap(),
        boot_id: std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .unwrap()
            .trim()
            .into(),
    };
    let dir = tempfile::tempdir().unwrap();
    let (config, identity, request) = setup(dir.path());
    let mut journal = LocalJournal::open(config.clone()).unwrap();
    let run = journal
        .accept(&identity, &request, digest(&request))
        .unwrap()
        .run_id;
    journal.start_run(&run, RunState::Running).unwrap();
    let mut wrong = evidence.clone();
    wrong.start_ticks += 1;
    assert!(journal.bind_process(&run, &id("task"), wrong).is_err());
    assert!(
        journal
            .bind_process(&run, &id("foreign"), evidence.clone())
            .is_err()
    );
    journal
        .bind_process(&run, &id("task"), evidence.clone())
        .unwrap();
    assert!(
        journal
            .mark_orphaned(&run, &id("foreign"), &evidence)
            .is_err()
    );
    journal.mark_orphaned(&run, &id("task"), &evidence).unwrap();
    assert!(journal.start_run(&run, RunState::Collecting).is_err());
    assert!(
        journal
            .reconcile_orphan_stopped(&run, &id("task"), &evidence)
            .is_err()
    );
    drop(journal);
    drop(child);
    let mut reopened = LocalJournal::open(config.clone()).unwrap();
    assert!(reopened.orphan_record(&run).unwrap().unwrap().orphaned);
    assert!(reopened.start_run(&run, RunState::Collecting).is_err());
    assert!(matches!(
        reopened.reconcile_orphan_stopped(&run, &id("task"), &evidence),
        Err(JournalError::Unsupported { .. })
    ));
    drop(reopened);
    let path = dir.path().join("state/admissions.jsonl");
    let data = std::fs::read_to_string(&path).unwrap();
    assert!(data.contains("\"orphaned\":true"));
    std::fs::write(
        &path,
        data.replace("\"orphaned\":true", "\"orphaned\":false"),
    )
    .unwrap();
    assert!(matches!(
        LocalJournal::open(config),
        Err(JournalError::Corrupt(_))
    ));
}

fn native(exit_code: i32, timed_out: bool) -> NativeObservation {
    use sha2::Digest as _;
    let digest = Digest::new(&format!("sha256:{:x}", sha2::Sha256::digest(b""))).unwrap();
    NativeObservation {
        exit_code: Some(exit_code),
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
        timed_out,
    }
}
fn empty_streams(journal: &LocalJournal, run: &Id) {
    use std::os::unix::fs::PermissionsExt;
    for channel in ["stdout", "stderr"] {
        let path = journal
            .location_for_task(run, &id("task"), channel)
            .unwrap();
        std::fs::write(&path, b"").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}
#[test]
fn evidence_and_terminal_report_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let (config, identity, request) = setup(dir.path());
    let mut journal = LocalJournal::open(config.clone()).unwrap();
    let run = journal
        .accept(&identity, &request, digest(&request))
        .unwrap()
        .run_id;
    journal.start_run(&run, RunState::Running).unwrap();
    empty_streams(&journal, &run);
    let obs = native(0, false);
    assert!(
        journal
            .append_observation(&run, &id("task"), 1, obs.clone())
            .is_err()
    );
    journal
        .append_observation(&run, &id("task"), 0, obs.clone())
        .unwrap();
    assert!(
        journal
            .append_observation(&run, &id("task"), 1, obs.clone())
            .is_err()
    );
    journal.start_run(&run, RunState::Collecting).unwrap();
    let report = journal.store_blob(b"{}").unwrap();
    assert!(
        journal
            .finalize_run(&run, RunState::Passed, &[], report.clone())
            .is_err()
    );
    let evidence = vec![(id("task"), obs)];
    let cursor = journal
        .finalize_run(&run, RunState::Passed, &evidence, report.clone())
        .unwrap();
    assert_eq!(
        cursor,
        journal
            .finalize_run(&run, RunState::Passed, &evidence, report.clone())
            .unwrap()
    );
    assert!(
        journal
            .finalize_run(&run, RunState::Failed, &evidence, report.clone())
            .is_err()
    );
    drop(journal);
    let journal = LocalJournal::open(config).unwrap();
    assert_eq!(journal.load_run(&run).unwrap().state, RunState::Passed);
    assert_eq!(journal.observations(&run).unwrap(), evidence);
    assert_eq!(journal.report_ref(&run).unwrap(), Some(report));
}
#[test]
fn failure_and_timeout_never_pass() {
    for (code, timeout, terminal) in [(7, false, RunState::Failed), (0, true, RunState::Failed)] {
        let dir = tempfile::tempdir().unwrap();
        let (config, identity, request) = setup(dir.path());
        let mut journal = LocalJournal::open(config).unwrap();
        let run = journal
            .accept(&identity, &request, digest(&request))
            .unwrap()
            .run_id;
        journal.start_run(&run, RunState::Running).unwrap();
        empty_streams(&journal, &run);
        let mut obs = native(code, timeout);
        if timeout {
            obs.exit_code = None;
            obs.signal = Some(15);
        }
        journal
            .append_observation(&run, &id("task"), 0, obs.clone())
            .unwrap();
        journal.start_run(&run, RunState::Collecting).unwrap();
        let report = journal.store_blob(b"{}").unwrap();
        let evidence = vec![(id("task"), obs)];
        assert!(
            journal
                .finalize_run(&run, RunState::Passed, &evidence, report.clone())
                .is_err()
        );
        journal
            .finalize_run(&run, terminal, &evidence, report)
            .unwrap();
    }
}

#[test]
fn timeout_without_drain_cannot_finalize_failed() {
    let dir = tempfile::tempdir().unwrap();
    let (config, identity, request) = setup(dir.path());
    let mut journal = LocalJournal::open(config).unwrap();
    let run = journal
        .accept(&identity, &request, digest(&request))
        .unwrap()
        .run_id;
    journal.start_run(&run, RunState::Running).unwrap();
    empty_streams(&journal, &run);
    let mut obs = native(0, true);
    obs.exit_code = None;
    obs.signal = Some(15);
    obs.cpu_complete = false;
    obs.descendants_drained = false;
    journal
        .append_observation(&run, &id("task"), 0, obs.clone())
        .unwrap();
    journal.start_run(&run, RunState::Collecting).unwrap();
    let report = journal.store_blob(b"{}").unwrap();
    let evidence = [(id("task"), obs)];
    assert!(
        journal
            .finalize_run(&run, RunState::Failed, &evidence, report.clone())
            .is_err()
    );
    journal
        .finalize_run(&run, RunState::Incomplete, &evidence, report)
        .unwrap();
}

#[test]
fn readonly_snapshot_replays_complete_incomplete_and_rejects_writes() {
    let dir = tempfile::tempdir().unwrap();
    let (config, identity, request) = setup(dir.path());
    let mut journal = LocalJournal::open(config.clone()).unwrap();
    let run = journal
        .accept(&identity, &request, digest(&request))
        .unwrap()
        .run_id;
    journal.start_run(&run, RunState::Running).unwrap();
    empty_streams(&journal, &run);
    let observation = native(0, false);
    journal
        .append_observation(&run, &id("task"), 0, observation.clone())
        .unwrap();
    journal.start_run(&run, RunState::Collecting).unwrap();
    let report = journal.store_blob(b"{}").unwrap();
    journal
        .finalize_run(
            &run,
            RunState::Incomplete,
            &[(id("task"), observation)],
            report,
        )
        .unwrap();
    let mut snapshot = LocalJournal::open_read_only(config.clone()).unwrap();
    assert_eq!(snapshot.load_run(&run).unwrap().state, RunState::Incomplete);
    assert!(snapshot.start_run(&run, RunState::Running).is_err());
    drop(journal);
    let no_service = LocalJournal::open_read_only(config).unwrap();
    assert_eq!(
        no_service.load_run(&run).unwrap().state,
        RunState::Incomplete
    );
}

#[test]
fn one_successful_task_cannot_pass_a_two_task_selection() {
    let dir = tempfile::tempdir().unwrap();
    let (config, identity, mut request) = setup(dir.path());
    request.requested_task_ids.push(id("other-task"));
    let mut journal = LocalJournal::open(config).unwrap();
    let run = journal
        .accept(&identity, &request, digest(&request))
        .unwrap()
        .run_id;
    journal.start_run(&run, RunState::Running).unwrap();
    empty_streams(&journal, &run);
    journal
        .append_observation(&run, &id("task"), 0, native(0, false))
        .unwrap();
    journal.start_run(&run, RunState::Collecting).unwrap();
    let blob = journal.store_blob(b"{}").unwrap();
    let incomplete_pass = journal.finalize_run(
        &run,
        RunState::Passed,
        &[(id("task"), native(0, false))],
        blob,
    );
    assert!(matches!(incomplete_pass, Err(JournalError::Conflict)));
    assert_eq!(journal.load_run(&run).unwrap().state, RunState::Collecting);
}

#[test]
fn pre_execution_refusal_is_terminal_zero_observation_and_replays() {
    let dir = tempfile::tempdir().unwrap();
    let (config, identity, request) = setup(dir.path());
    let mut journal = LocalJournal::open(config.clone()).unwrap();
    let run = journal
        .accept(&identity, &request, digest(&request))
        .unwrap()
        .run_id;
    let queued = journal.load_run(&run).unwrap();
    assert_eq!(queued.state, RunState::Queued);
    let report_bytes = b"{\"result\":\"incomplete\"}";
    let report = journal.store_blob(report_bytes).unwrap();
    assert_eq!(
        journal.refuse_before_start(&run, report.clone()).unwrap(),
        RunState::Incomplete
    );
    let refused = journal.load_run(&run).unwrap();
    assert_eq!(refused.state, RunState::Incomplete);
    assert_eq!(refused.observations.len(), 0);
    assert_eq!(refused.report, Some(report.clone()));
    assert!(refused.event_cursor > queued.event_cursor);
    assert!(
        journal.start_run(&run, RunState::Running).is_err(),
        "a refused run can never start"
    );
    assert!(
        journal.start_run(&run, RunState::Collecting).is_err(),
        "a refused run can never collect"
    );
    assert!(
        journal.refuse_before_start(&run, report).is_err(),
        "a refusal is written once"
    );
    assert!(journal.cancel(&run, &identity.principal_id).is_err());
    let cursor = refused.event_cursor;
    drop(journal);
    let mut reopened = LocalJournal::open(config).unwrap();
    let replay = reopened.load_run(&run).unwrap();
    assert_eq!(replay.state, RunState::Incomplete);
    assert_eq!(replay.event_cursor, cursor);
    assert_eq!(replay.observations.len(), 0);
    assert_eq!(
        reopened
            .accept(&identity, &request, digest(&request))
            .unwrap()
            .run_id,
        run,
        "the same request is not respawned"
    );
}

#[test]
fn pre_execution_refusal_rejects_active_or_unknown_runs_and_bad_reports() {
    let dir = tempfile::tempdir().unwrap();
    let (config, identity, request) = setup(dir.path());
    let mut journal = LocalJournal::open(config).unwrap();
    let run = journal
        .accept(&identity, &request, digest(&request))
        .unwrap()
        .run_id;
    let bogus = BlobRef {
        digest: Digest::blake3("missing", b"never stored").unwrap(),
        size_bytes: 12,
        media_type: "application/json".into(),
    };
    assert!(
        journal.refuse_before_start(&run, bogus).is_err(),
        "unverified report blob is rejected"
    );
    assert_eq!(journal.load_run(&run).unwrap().state, RunState::Queued);
    journal.start_run(&run, RunState::Capturing).unwrap();
    let report = journal.store_blob(b"{}").unwrap();
    assert!(
        journal.refuse_before_start(&run, report).is_err(),
        "only a queued run may be refused"
    );
}

#[test]
fn corrupt_refusal_event_is_rejected_on_replay() {
    use std::io::Write as _;
    let dir = tempfile::tempdir().unwrap();
    let (config, identity, request) = setup(dir.path());
    let mut journal = LocalJournal::open(config.clone()).unwrap();
    let run = journal
        .accept(&identity, &request, digest(&request))
        .unwrap()
        .run_id;
    let report = journal.store_blob(b"{}").unwrap();
    journal.refuse_before_start(&run, report.clone()).unwrap();
    drop(journal);
    let journal_path = dir.path().join("state/admissions.jsonl");
    let mut file = OpenOptions::new().append(true).open(&journal_path).unwrap();
    // A replayed state event after a terminal refusal would reopen a phase.
    let cursor = 99;
    let event = serde_json::json!({
        "schema": "kioku/ci2/state-event/v1",
        "revision": 1,
        "run_id": run.as_str(),
        "state": "running",
        "cursor": cursor,
    });
    file.write_all(format!("{event}\n").as_bytes()).unwrap();
    file.sync_all().unwrap();
    assert!(
        LocalJournal::open(config).is_err(),
        "invalid replay after refusal must fail closed"
    );
}

#[test]
fn interruption_terminal_replays_and_blocks_new_native_start() {
    for terminal in [RunState::Failed, RunState::Incomplete] {
        let dir = tempfile::tempdir().unwrap();
        let (config, identity, request) = setup(dir.path());
        let mut journal = LocalJournal::open(config.clone()).unwrap();
        let run = journal
            .accept(&identity, &request, digest(&request))
            .unwrap()
            .run_id;
        journal.start_run(&run, RunState::Running).unwrap();
        let report = journal.store_blob(b"{}").unwrap();
        let cursor = journal
            .interrupt_run_with_result(&run, report.clone(), terminal)
            .unwrap();
        let before = journal.load_run(&run).unwrap();
        assert_eq!(before.state, terminal);
        assert_eq!(before.interrupted_from, Some(RunState::Running));
        assert!(journal.has_unresolved_execution().unwrap());
        drop(journal);
        let bytes = std::fs::read_to_string(dir.path().join("state/admissions.jsonl")).unwrap();
        assert!(bytes.contains("\"terminal\":"));
        let mut reopened = LocalJournal::open(config).unwrap();
        let after = reopened.load_run(&run).unwrap();
        assert_eq!(after.state, terminal);
        assert_eq!(after.interrupted_from, Some(RunState::Running));
        assert_eq!(after.report, Some(report));
        assert_eq!(after.event_cursor, cursor);
        assert!(reopened.has_unresolved_execution().unwrap());
        let mut another = request.clone();
        another.request_id = id("another");
        let next = reopened
            .accept(&identity, &another, digest(&another))
            .unwrap()
            .run_id;
        assert!(reopened.start_run(&next, RunState::Running).is_err());
    }
}

#[test]
fn legacy_interruption_omits_terminal_and_replays_incomplete() {
    let dir = tempfile::tempdir().unwrap();
    let (config, identity, request) = setup(dir.path());
    let mut journal = LocalJournal::open(config.clone()).unwrap();
    let run = journal
        .accept(&identity, &request, digest(&request))
        .unwrap()
        .run_id;
    journal.start_run(&run, RunState::Collecting).unwrap_err();
    journal.start_run(&run, RunState::Running).unwrap();
    let report = journal.store_blob(b"{}").unwrap();
    journal.interrupt_run(&run, report).unwrap();
    drop(journal);
    let bytes = std::fs::read_to_string(dir.path().join("state/admissions.jsonl")).unwrap();
    assert!(!bytes.contains("\"terminal\":"));
    let reopened = LocalJournal::open(config).unwrap();
    assert_eq!(reopened.load_run(&run).unwrap().state, RunState::Incomplete);
    assert!(reopened.has_unresolved_execution().unwrap());
}

#[test]
fn interruption_rejects_invalid_terminal_and_missing_report_without_append() {
    let dir = tempfile::tempdir().unwrap();
    let (config, identity, request) = setup(dir.path());
    let mut journal = LocalJournal::open(config).unwrap();
    let run = journal
        .accept(&identity, &request, digest(&request))
        .unwrap()
        .run_id;
    journal.start_run(&run, RunState::Running).unwrap();
    let report = journal.store_blob(b"{}").unwrap();
    let path = dir.path().join("state/admissions.jsonl");
    let before = std::fs::read(&path).unwrap();
    for terminal in [RunState::Passed, RunState::Cancelled, RunState::Running] {
        assert!(
            journal
                .interrupt_run_with_result(&run, report.clone(), terminal)
                .is_err()
        );
    }
    assert!(
        journal
            .interrupt_run_with_result(&run, blob(), RunState::Failed)
            .is_err()
    );
    assert_eq!(std::fs::read(path).unwrap(), before);
    assert_eq!(journal.load_run(&run).unwrap().state, RunState::Running);
}

#[test]
fn corrupt_interruption_terminal_or_report_is_rejected_on_replay() {
    for corrupt_report in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (config, identity, request) = setup(dir.path());
        let mut journal = LocalJournal::open(config.clone()).unwrap();
        let run = journal
            .accept(&identity, &request, digest(&request))
            .unwrap()
            .run_id;
        journal.start_run(&run, RunState::Running).unwrap();
        let report = journal.store_blob(b"{}").unwrap();
        journal
            .interrupt_run_with_result(&run, report, RunState::Failed)
            .unwrap();
        drop(journal);
        let path = dir.path().join("state/admissions.jsonl");
        let bytes = std::fs::read_to_string(&path).unwrap();
        let mut events: Vec<serde_json::Value> = bytes
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let event = events.last_mut().unwrap();
        if corrupt_report {
            event["report"].as_object_mut().unwrap().remove("digest");
        } else {
            event["terminal"] = serde_json::json!("passed");
        }
        let changed = events.iter().fold(String::new(), |mut changed, event| {
            let _ = writeln!(changed, "{event}");
            changed
        });
        std::fs::write(path, changed).unwrap();
        assert!(LocalJournal::open(config).is_err());
    }
}
