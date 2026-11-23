#![cfg(unix)]
use kioku::ci2::{
    domain::*,
    storage::{JournalError, LocalJournal, RunRecord},
};
use std::{fs, io::Write, os::unix::fs::PermissionsExt, path::Path};

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
        max_state_bytes: 128 * 1024 * 1024,
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

fn private_dir(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn observation(digest: Digest) -> NativeObservation {
    let observation = NativeObservation {
        exit_code: Some(0),
        signal: None,
        stdout_digest: digest.clone(),
        stderr_digest: digest,
        stdout_bytes: 0,
        stderr_bytes: 0,
        wall_ns: 3,
        cpu_user_ns: 1,
        cpu_system_ns: 1,
        cpu_complete: false,
        descendants_drained: false,
        started_ms: 1,
        ended_ms: 2,
        timed_out: false,
    };
    observation.validate().unwrap();
    observation
}

// Synthetic durable events are loaded through the public, validating read-only API.
fn fixture(root: &Path, count: usize, observed_tasks: usize) -> (ServiceBootstrap, Vec<RunRecord>) {
    let (config, identity, mut request) = setup(root);
    let state = Path::new(&config.state_root);
    private_dir(state);
    private_dir(&state.join("results"));
    request.requested_task_ids = (0..64).map(|index| id(&format!("task-{index}"))).collect();
    let mut bytes = Vec::new();
    let mut expected = Vec::new();
    let mut cursor = 0;
    for index in 0..count {
        request.request_id = id(&format!("request-{}", count - index));
        let run_id = id(&format!("run-{}", count - index));
        let accepted = LocalAccepted {
            schema: "kioku/ci2/local-accepted/v1".into(),
            revision: 1,
            request_id: request.request_id.clone(),
            run_id: run_id.clone(),
            request_digest: digest(&request),
            resume_argv: vec!["kioku".into()],
            state: RunState::Queued,
        };
        writeln!(
            &mut bytes,
            "{}",
            serde_json::json!({
                "schema": "kioku/ci2/admission-event/v1", "revision": 1,
                "principal_id": identity.principal_id, "request": request, "accepted": accepted,
            })
        )
        .unwrap();
        cursor += 1;
        let mut record = RunRecord {
            principal_id: identity.principal_id.clone(),
            request: request.clone(),
            accepted,
            state: RunState::Queued,
            accepted_cursor: cursor,
            event_cursor: cursor,
            observations: Vec::new(),
            report: None,
            interrupted_from: None,
        };
        if observed_tasks > 0 {
            cursor += 1;
            writeln!(
                &mut bytes,
                "{}",
                serde_json::json!({
                    "schema": "kioku/ci2/state-event/v1", "revision": 1,
                    "run_id": run_id, "cursor": cursor, "state": "running",
                })
            )
            .unwrap();
            record.state = RunState::Running;
            let directory = state.join("results").join(run_id.as_str());
            private_dir(&directory);
            for (sequence, task) in request
                .requested_task_ids
                .iter()
                .enumerate()
                .take(observed_tasks)
            {
                use sha2::Digest as _;
                let digest =
                    Digest::new(&format!("sha256:{:x}", sha2::Sha256::digest(b""))).unwrap();
                for channel in ["stdout", "stderr"] {
                    let path = directory.join(format!("{task}.{channel}"));
                    fs::write(&path, b"").unwrap();
                    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
                }
                let observation = observation(digest);
                let outcome = kioku::ci2::storage::TaskOutcome {
                    task_id: task.clone(),
                    observation,
                };
                cursor += 1;
                writeln!(&mut bytes, "{}", serde_json::json!({
                    "schema": "kioku/ci2/observation-event/v1", "revision": 1,
                    "run_id": run_id, "cursor": cursor, "sequence": sequence, "outcome": outcome,
                })).unwrap();
                record.observations.push(outcome);
            }
            record.event_cursor = cursor;
        }
        expected.push(record);
    }
    let path = state.join("admissions.jsonl");
    fs::write(&path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    (config, expected)
}

#[test]
fn readonly_all_runs_keeps_oldest_complete_owned_records() {
    for count in [0, 1, 256, 257, 1024] {
        let dir = tempfile::tempdir().unwrap();
        let (config, expected) = fixture(dir.path(), count, 0);
        let path = Path::new(&config.state_root).join("admissions.jsonl");
        let before = fs::read(&path).unwrap();
        let journal = LocalJournal::open_read_only(config).unwrap();
        assert_eq!(journal.all_runs().unwrap(), expected[..count.min(256)]);
        assert_eq!(fs::read(path).unwrap(), before);
    }
}

#[test]
fn readonly_all_runs_retains_large_observation_groups_and_rejects_poison() {
    let dir = tempfile::tempdir().unwrap();
    let (config, expected) = fixture(dir.path(), 257, 32);
    let mut journal = LocalJournal::open_read_only(config.clone()).unwrap();
    assert_eq!(journal.all_runs().unwrap(), expected[..256]);
    let run = &expected[0].accepted.run_id;
    assert!(matches!(
        journal.start_run(run, RunState::Collecting),
        Err(JournalError::Io(_))
    ));
    assert!(matches!(journal.all_runs(), Err(JournalError::Poisoned)));
    assert!(matches!(
        journal.load_run(&id("missing")),
        Err(JournalError::Poisoned)
    ));
    let path = Path::new(&config.state_root).join("admissions.jsonl");
    fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(b"{")
        .unwrap();
    assert!(matches!(
        LocalJournal::open_read_only(config),
        Err(JournalError::Corrupt(_))
    ));
}

#[test]
#[ignore = "component timing only; run explicitly on pinned baseline and candidate test binaries"]
fn all_runs_component_measurement() {
    use std::{hint::black_box, time::Instant};
    for count in [512, 1024, 4096] {
        let dir = tempfile::tempdir().unwrap();
        let (config, expected) = fixture(dir.path(), count, 0);
        let journal = LocalJournal::open_read_only(config).unwrap();
        let expected = &expected[..256];
        let encoded = serde_json::to_vec(expected).unwrap();
        let hash = blake3::hash(&encoded);
        for _ in 0..5 {
            assert_eq!(black_box(journal.all_runs().unwrap()), expected);
        }
        for repeat in 0..31 {
            let start = Instant::now();
            let actual = black_box(journal.all_runs().unwrap());
            let elapsed = start.elapsed().as_nanos();
            assert_eq!(actual, expected);
            println!(
                "component=LocalJournal::all_runs N={count} repeat={repeat} ns={elapsed} records={} digest={hash} FullInvocation=false RSS=UNKNOWN IO=UNKNOWN",
                actual.len()
            );
        }
    }
}
