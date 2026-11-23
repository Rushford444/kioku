#![cfg(unix)]
//! C17: capacity refusals preserve the durable prefix and readable history.

use kioku::ci2::{
    domain::*,
    storage::{JournalError, LocalJournal},
};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

fn id(text: &str) -> Id {
    Id::new(text).unwrap()
}

fn reference() -> BlobRef {
    BlobRef {
        digest: Digest::blake3("fixture", b"content").unwrap(),
        size_bytes: 7,
        media_type: "application/json".into(),
    }
}

/// Builds a minimal I1-L bootstrap rooted inside `dir`.
fn bootstrap(dir: &Path) -> ServiceBootstrap {
    let reference = reference();
    ServiceBootstrap {
        schema: "kioku/ci2/bootstrap/v1".into(),
        revision: 1,
        service_id: id("service"),
        mode: ServiceMode::LocalCooperative,
        resource_domain_id: id("domain"),
        state_root: dir.join("state").to_str().unwrap().into(),
        socket_path: dir.join("socket").to_str().unwrap().into(),
        stop_file: dir.join("stop").to_str().unwrap().into(),
        registered_roots: vec![RegisteredRoot {
            id: id("root"),
            project_id: id("project"),
            canonical_path: dir.to_str().unwrap().into(),
            allowed_principals: vec![id("alice")],
            protected_refs: vec![],
        }],
        approved_manifest_refs: vec![reference.clone()],
        profiles_ref: reference.clone(),
        policy_ref: reference,
        auth_config_ref: None,
        capture_slots: 1,
        heavy_slots: 1,
        max_state_bytes: 1_000_000,
        max_queue_wait_ms: 1,
        drain_grace_ms: 1,
    }
}

fn identity() -> ClientIdentity {
    ClientIdentity {
        principal_id: id("alice"),
        project_ids: vec![id("project")],
        roles: vec![Role::Requester],
        trust_scope: id("local"),
        lane_id: None,
    }
}

fn request(name: &str) -> LocalRequest {
    LocalRequest {
        schema: "kioku/ci2/local-request/v1".into(),
        revision: 1,
        request_id: id(name),
        project_id: id("project"),
        origin_claim: OriginClaim::LocalRegisteredCheckout {
            root_id: id("root"),
            relative_path: RelPath::new("work").unwrap(),
        },
        manifest_digest: reference().digest,
        profile_id: id("profile"),
        requested_task_ids: vec![id("task")],
        mode: RunMode::Normal,
        wait_class: WaitClass::Agent,
    }
}

fn request_digest(request: &LocalRequest) -> Digest {
    digest_request_document(&serde_json::to_value(request).unwrap()).unwrap()
}

/// Empty stdout/stderr pair with the matching SHA-256 digest.
fn native() -> NativeObservation {
    use sha2::Digest as _;
    let digest = Digest::new(&format!("sha256:{:x}", sha2::Sha256::digest(b""))).unwrap();
    NativeObservation {
        exit_code: Some(0),
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
        timed_out: false,
    }
}

fn private_streams(journal: &LocalJournal, run: &Id) {
    for channel in ["stdout", "stderr"] {
        let path = journal
            .location_for_task(run, &id("task"), channel)
            .unwrap();
        fs::write(&path, b"").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
}

fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, files: &mut Vec<(PathBuf, Vec<u8>)>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, files);
            } else {
                files.push((
                    path.strip_prefix(root).unwrap().to_owned(),
                    fs::read(path).unwrap(),
                ));
            }
        }
    }
    let mut files = Vec::new();
    walk(root, root, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

struct Fixture {
    _dir: tempfile::TempDir,
    config: ServiceBootstrap,
    historic: Id,
    pending: Id,
    report: BlobRef,
}

impl Fixture {
    fn new(phase: RunState) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut config = bootstrap(dir.path());
        let mut journal = LocalJournal::open(config.clone()).unwrap();
        let payload = request("historic");
        let historic = journal
            .accept(&identity(), &payload, request_digest(&payload))
            .unwrap()
            .run_id;
        journal.start_run(&historic, RunState::Running).unwrap();
        private_streams(&journal, &historic);
        journal
            .append_observation(&historic, &id("task"), 0, native())
            .unwrap();
        journal.start_run(&historic, RunState::Collecting).unwrap();
        let report = journal.store_blob(br#"{"historic":true}"#).unwrap();
        journal
            .finalize_run(
                &historic,
                RunState::Incomplete,
                &[(id("task"), native())],
                report.clone(),
            )
            .unwrap();

        let payload = request("pending");
        let pending = journal
            .accept(&identity(), &payload, request_digest(&payload))
            .unwrap()
            .run_id;
        if phase != RunState::Queued {
            journal.start_run(&pending, RunState::Running).unwrap();
            private_streams(&journal, &pending);
        }
        if phase == RunState::Collecting {
            journal
                .append_observation(&pending, &id("task"), 0, native())
                .unwrap();
            journal.start_run(&pending, RunState::Collecting).unwrap();
        }
        drop(journal);
        config.max_state_bytes =
            fs::metadata(Path::new(&config.state_root).join("admissions.jsonl"))
                .unwrap()
                .len();
        assert!(
            config.max_state_bytes < 8192,
            "fixture must use a small journal budget"
        );
        Self {
            _dir: dir,
            config,
            historic,
            pending,
            report,
        }
    }

    fn assert_history(&self, journal: &LocalJournal) {
        let historic = journal.load_run(&self.historic).unwrap();
        assert_eq!(historic.state, RunState::Incomplete);
        assert_eq!(historic.event_cursor, 5);
        assert_eq!(
            journal.report_ref(&self.historic).unwrap(),
            Some(self.report.clone())
        );
        assert_eq!(
            journal.observations(&self.historic).unwrap(),
            vec![(id("task"), native())]
        );
        for channel in ["stdout", "stderr"] {
            let path = journal
                .artifact_for_task(&self.historic, &id("task"), channel)
                .unwrap();
            assert_eq!(fs::read(path).unwrap(), b"");
        }
        let blob = Path::new(&self.config.state_root)
            .join("blobs")
            .join(format!("blob-{}", self.report.digest.as_hex()));
        assert_eq!(fs::read(blob).unwrap(), br#"{"historic":true}"#);
    }

    fn assert_replay(
        &self,
        before: &[(PathBuf, Vec<u8>)],
        pending: &kioku::ci2::storage::RunRecord,
    ) {
        for journal in [
            LocalJournal::open_read_only(self.config.clone()).unwrap(),
            LocalJournal::open(self.config.clone()).unwrap(),
        ] {
            self.assert_history(&journal);
            assert_eq!(&journal.load_run(&self.pending).unwrap(), pending);
        }
        assert_eq!(snapshot(Path::new(&self.config.state_root)), before);
    }
}

#[test]
fn capacity_refuses_admission_and_transition_without_overwriting_history() {
    let fixture = Fixture::new(RunState::Queued);
    let root = Path::new(&fixture.config.state_root);
    let before = snapshot(root);
    let mut journal = LocalJournal::open(fixture.config.clone()).unwrap();
    let pending = journal.load_run(&fixture.pending).unwrap();
    let payload = request("refused");
    for _ in 0..2 {
        assert!(matches!(
            journal.accept(&identity(), &payload, request_digest(&payload)),
            Err(JournalError::Capacity)
        ));
        assert!(matches!(
            journal.start_run(&fixture.pending, RunState::Running),
            Err(JournalError::Capacity)
        ));
    }
    let oversized_blob = vec![b'x'; usize::try_from(fixture.config.max_state_bytes).unwrap() + 1];
    assert!(matches!(
        journal.store_blob(&oversized_blob),
        Err(JournalError::Capacity)
    ));
    assert_eq!(journal.all_runs().unwrap().len(), 2);
    let existing = request("historic");
    assert_eq!(
        journal
            .accept(&identity(), &existing, request_digest(&existing))
            .unwrap()
            .run_id,
        fixture.historic
    );
    fixture.assert_history(&journal);
    assert_eq!(journal.load_run(&fixture.pending).unwrap(), pending);
    assert_eq!(snapshot(root), before);
    drop(journal);
    fixture.assert_replay(&before, &pending);

    let mut too_small = fixture.config.clone();
    too_small.max_state_bytes -= 1;
    assert!(matches!(
        LocalJournal::open(too_small.clone()),
        Err(JournalError::Capacity)
    ));
    assert!(matches!(
        LocalJournal::open_read_only(too_small),
        Err(JournalError::Capacity)
    ));
    fixture.assert_replay(&before, &pending);
}

#[test]
fn capacity_refuses_observation_without_truncating_the_tail() {
    let fixture = Fixture::new(RunState::Running);
    let root = Path::new(&fixture.config.state_root);
    let before = snapshot(root);
    let mut journal = LocalJournal::open(fixture.config.clone()).unwrap();
    let pending = journal.load_run(&fixture.pending).unwrap();
    for _ in 0..2 {
        assert!(matches!(
            journal.append_observation(&fixture.pending, &id("task"), 0, native()),
            Err(JournalError::Capacity)
        ));
    }
    fixture.assert_history(&journal);
    assert_eq!(journal.load_run(&fixture.pending).unwrap(), pending);
    assert_eq!(snapshot(root), before);
    drop(journal);
    fixture.assert_replay(&before, &pending);
}

#[test]
fn capacity_refuses_finalization_without_publishing_a_report() {
    let fixture = Fixture::new(RunState::Collecting);
    let root = Path::new(&fixture.config.state_root);
    let before = snapshot(root);
    let mut journal = LocalJournal::open(fixture.config.clone()).unwrap();
    let pending = journal.load_run(&fixture.pending).unwrap();
    assert_eq!(journal.report_ref(&fixture.pending).unwrap(), None);
    for _ in 0..2 {
        assert!(matches!(
            journal.finalize_run(
                &fixture.pending,
                RunState::Incomplete,
                &[(id("task"), native())],
                fixture.report.clone()
            ),
            Err(JournalError::Capacity)
        ));
    }
    fixture.assert_history(&journal);
    assert_eq!(journal.load_run(&fixture.pending).unwrap(), pending);
    assert_eq!(snapshot(root), before);
    drop(journal);
    fixture.assert_replay(&before, &pending);
}
