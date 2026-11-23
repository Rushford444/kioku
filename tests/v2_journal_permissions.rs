#![cfg(unix)]
//! Read-only audit of the private local journal's path and mode discipline.
//! Each test builds its own private temporary bootstrap fixture and asserts that
//! the journal refuses insecure filesystem inputs and never writes outside its
//! declared state root. No product source, service, or network is involved.

use kioku::ci2::{
    domain::*,
    storage::{JournalError, LocalJournal},
};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

fn id(text: &str) -> Id {
    Id::new(text).unwrap()
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode() & 0o777
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

fn request() -> LocalRequest {
    LocalRequest {
        schema: "kioku/ci2/local-request/v1".into(),
        revision: 1,
        request_id: id("request"),
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

/// Every path the journal creates, relative to the state root.
fn created_paths(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            out.push(
                path.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            );
            if path.is_dir() {
                walk(&path, root, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

#[test]
fn bootstrap_creates_only_private_paths_under_the_state_root() {
    let dir = tempfile::tempdir().unwrap();
    let config = bootstrap(dir.path());
    let root = PathBuf::from(&config.state_root);
    let mut journal = LocalJournal::open(config).unwrap();

    assert_eq!(mode(&root), 0o700, "state root must be owner-only");
    assert_eq!(mode(&root.join(".owner.lock")), 0o600);
    assert_eq!(mode(&root.join("admissions.jsonl")), 0o600);

    let run = journal
        .accept(&identity(), &request(), request_digest(&request()))
        .unwrap()
        .run_id;
    journal.start_run(&run, RunState::Running).unwrap();
    private_streams(&journal, &run);

    let results = root.join("results");
    assert_eq!(mode(&results), 0o700);
    assert_eq!(mode(&results.join(run.as_str())), 0o700);
    for channel in ["stdout", "stderr"] {
        assert_eq!(
            mode(&results.join(run.as_str()).join(format!("task.{channel}"))),
            0o600
        );
    }

    let blob = journal.store_blob(b"{}").unwrap();
    assert_eq!(mode(&root.join("blobs")), 0o700);
    assert_eq!(
        mode(
            &root
                .join("blobs")
                .join(format!("blob-{}", blob.digest.as_hex()))
        ),
        0o600
    );

    journal
        .append_observation(&run, &id("task"), 0, native())
        .unwrap();
    journal.start_run(&run, RunState::Collecting).unwrap();
    journal
        .finalize_run(&run, RunState::Incomplete, &[(id("task"), native())], blob)
        .unwrap();

    // The journal creates exactly these entries and nothing outside the root.
    let created = created_paths(&root);
    let allowed = |path: &str| {
        matches!(
            path,
            ".owner.lock" | "admissions.jsonl" | "blobs" | "results"
        ) || path.starts_with("blobs/")
            || path.starts_with("results/")
    };
    for path in &created {
        assert!(allowed(path), "unexpected journal path: {path}");
        assert_eq!(
            mode(&root.join(path)) & 0o077,
            0,
            "group/other bits on {path}"
        );
    }
    assert!(
        !created.iter().any(|path| path.contains("bootstrap.json")),
        "the journal must not fabricate the bootstrap document it references"
    );
    // Only the fixture parent and its single state directory are touched.
    let mut top: Vec<String> = fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    top.sort();
    assert_eq!(top, vec!["state".to_string()]);
    // The declared socket and stop paths are not journal outputs.
    assert!(!dir.path().join("socket").exists());
    assert!(!dir.path().join("stop").exists());
}

#[test]
fn preexisting_public_state_root_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let config = bootstrap(dir.path());
    fs::create_dir(&config.state_root).unwrap();
    fs::set_permissions(&config.state_root, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        LocalJournal::open(config.clone()),
        Err(JournalError::RootPermissions)
    ));
    assert!(matches!(
        LocalJournal::open_read_only(config),
        Err(JournalError::RootPermissions)
    ));
}

#[test]
fn symlinked_or_non_directory_state_root_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real-state");
    fs::create_dir(&real).unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();

    let linked = bootstrap(dir.path());
    std::os::unix::fs::symlink(&real, &linked.state_root).unwrap();
    assert!(
        matches!(
            LocalJournal::open(linked),
            Err(JournalError::RootPermissions)
        ),
        "a symlinked state root must not be followed"
    );

    let mut file_root = bootstrap(dir.path());
    let plain = dir.path().join("plain-file-state");
    fs::write(&plain, b"not a directory").unwrap();
    file_root.state_root = plain.to_str().unwrap().into();
    assert!(matches!(
        LocalJournal::open(file_root),
        Err(JournalError::RootPermissions)
    ));
}

#[test]
fn preexisting_symlinked_or_public_journal_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let config = bootstrap(dir.path());
    let root = PathBuf::from(&config.state_root);
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();

    let outside = dir.path().join("outside.jsonl");
    fs::write(&outside, b"").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("admissions.jsonl")).unwrap();
    assert!(
        matches!(
            LocalJournal::open(config.clone()),
            Err(JournalError::Io(err)) if err.kind() == std::io::ErrorKind::PermissionDenied
        ),
        "a symlinked journal must not be followed"
    );
    fs::remove_file(root.join("admissions.jsonl")).unwrap();

    fs::write(root.join("admissions.jsonl"), b"").unwrap();
    fs::set_permissions(
        root.join("admissions.jsonl"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    for opened in [
        LocalJournal::open(config.clone()),
        LocalJournal::open_read_only(config),
    ] {
        assert!(
            matches!(
                opened,
                Err(JournalError::Io(err)) if err.kind() == std::io::ErrorKind::PermissionDenied
            ),
            "a group/world readable journal is not private"
        );
    }
    assert_eq!(
        fs::read(outside).unwrap(),
        b"",
        "nothing written through the link"
    );
}

#[test]
fn public_result_or_blob_paths_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let config = bootstrap(dir.path());
    let root = PathBuf::from(&config.state_root);
    let mut journal = LocalJournal::open(config).unwrap();
    let run = journal
        .accept(&identity(), &request(), request_digest(&request()))
        .unwrap()
        .run_id;
    journal.start_run(&run, RunState::Running).unwrap();
    private_streams(&journal, &run);

    // World-readable stream file is rejected instead of being trusted.
    let stream = journal
        .location_for_task(&run, &id("task"), "stdout")
        .unwrap();
    fs::set_permissions(&stream, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        journal.append_observation(&run, &id("task"), 0, native()),
        Err(JournalError::Conflict)
    ));
    fs::set_permissions(&stream, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(
        matches!(
            journal.artifact_for_task(&run, &id("task"), "stdout"),
            Err(JournalError::Conflict)
        ),
        "no observation is durable yet"
    );
    journal
        .append_observation(&run, &id("task"), 0, native())
        .unwrap();
    assert_eq!(
        journal
            .artifact_for_task(&run, &id("task"), "stdout")
            .unwrap(),
        stream
    );

    // A public results directory poisons the replay path.
    let results = root.join("results");
    fs::set_permissions(&results, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        journal.append_observation(&run, &id("task"), 1, native()),
        Err(JournalError::RootPermissions | JournalError::Conflict)
    ));
    fs::set_permissions(&results, fs::Permissions::from_mode(0o700)).unwrap();

    // A public blob directory is refused before any blob is written.
    let blobs = root.join("blobs");
    fs::create_dir(&blobs).unwrap();
    fs::set_permissions(&blobs, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        journal.store_blob(b"{}"),
        Err(JournalError::RootPermissions)
    ));
    assert_eq!(created_paths(&blobs), Vec::<String>::new());
}

#[test]
fn task_and_channel_names_cannot_select_an_arbitrary_path() {
    let dir = tempfile::tempdir().unwrap();
    let config = bootstrap(dir.path());
    let root = PathBuf::from(&config.state_root);
    let mut journal = LocalJournal::open(config).unwrap();
    let run = journal
        .accept(&identity(), &request(), request_digest(&request()))
        .unwrap()
        .run_id;
    journal.start_run(&run, RunState::Running).unwrap();

    // Traversal and separators cannot even form an identifier.
    for hostile in ["../escape", "..", "a/b", "/abs", "task/../task"] {
        assert!(Id::new(hostile).is_err(), "{hostile} must be rejected");
    }
    // Unknown channels and unknown tasks are refused without creating paths.
    for channel in ["", "stdout/../../x", "..", "log"] {
        assert!(matches!(
            journal.location_for_task(&run, &id("task"), channel),
            Err(JournalError::Conflict)
        ));
    }
    assert!(matches!(
        journal.location_for_task(&run, &id("other-task"), "stdout"),
        Err(JournalError::Conflict)
    ));
    assert!(matches!(
        journal.artifact_for_task(&run, &id("task"), "stderr"),
        Err(JournalError::Conflict)
    ));
    assert!(
        !root.join("results").exists(),
        "a rejected request must not create the results directory"
    );

    // The accepted location is always directly under results/<run-id>.
    let path = journal
        .location_for_task(&run, &id("task"), "stdout")
        .unwrap();
    assert_eq!(
        path,
        root.join("results").join(run.as_str()).join("task.stdout")
    );
    assert_eq!(path.components().count(), root.components().count() + 3);
}

#[test]
fn read_only_replay_never_creates_or_mutates_paths() {
    let dir = tempfile::tempdir().unwrap();
    let config = bootstrap(dir.path());
    let root = PathBuf::from(&config.state_root);
    let mut journal = LocalJournal::open(config.clone()).unwrap();
    let run = journal
        .accept(&identity(), &request(), request_digest(&request()))
        .unwrap()
        .run_id;
    journal.start_run(&run, RunState::Capturing).unwrap();
    drop(journal);

    let before = created_paths(&root);
    let snapshot = LocalJournal::open_read_only(config.clone()).unwrap();
    assert_eq!(snapshot.load_run(&run).unwrap().state, RunState::Capturing);
    assert_eq!(
        created_paths(&root),
        before,
        "read-only replay created paths"
    );

    assert!(matches!(
        snapshot.artifact_for_task(&run, &id("task"), "stdout"),
        Err(JournalError::Conflict)
    ));

    // A write through the read-only descriptor is refused by the kernel, and the
    // handle poisons itself rather than pretend the append succeeded.
    let mut snapshot = snapshot;
    assert!(matches!(
        snapshot.start_run(&run, RunState::Running),
        Err(JournalError::Io(_))
    ));
    assert!(matches!(
        snapshot.load_run(&run),
        Err(JournalError::Poisoned)
    ));
    assert!(
        !root.join("results").exists(),
        "read-only replay must not create result folders"
    );
    assert_eq!(created_paths(&root), before);

    // A fresh read-only replay sees the unchanged durable state.
    let fresh = LocalJournal::open_read_only(config.clone()).unwrap();
    assert_eq!(fresh.load_run(&run).unwrap().state, RunState::Capturing);
    assert_eq!(fresh.load_run(&run).unwrap().event_cursor, 2);
    drop(fresh);
    assert_eq!(created_paths(&root), before);

    // A writable open resumes the same run under the same root.
    let mut journal = LocalJournal::open(config).unwrap();
    assert_eq!(journal.start_run(&run, RunState::Running).unwrap(), 3);
    assert_eq!(mode(&root), 0o700);
}
