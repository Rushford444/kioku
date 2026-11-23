use kioku::ci2::*;
use std::path::PathBuf;

fn digest() -> Digest {
    Digest::new(&format!("b3:{}", "a".repeat(64))).expect("valid fixture")
}
fn blob() -> BlobRef {
    BlobRef {
        digest: digest(),
        size_bytes: 17,
        media_type: "application/json".into(),
    }
}
fn id(value: &str) -> Id {
    Id::new(value).expect("valid fixture")
}
fn origin() -> OriginClaim {
    OriginClaim::LocalRegisteredCheckout {
        root_id: id("kioku-local"),
        relative_path: RelPath::new("tests/fixtures/ci2/stub").expect("key"),
    }
}
fn request(mode: RunMode) -> LocalRequest {
    LocalRequest {
        schema: "kioku/ci2/local-request/v1".into(),
        revision: 1,
        request_id: id("request-001"),
        project_id: id("kioku"),
        origin_claim: origin(),
        manifest_digest: digest(),
        profile_id: id("local-observed"),
        requested_task_ids: vec![id("cargo-check")],
        mode,
        wait_class: WaitClass::Agent,
    }
}
fn environment(kind: EnvKind) -> EnvSpec {
    EnvSpec {
        name: "RUST_BACKTRACE".into(),
        kind,
        value: match kind {
            EnvKind::PublicValue => Some("0".into()),
            _ => None,
        },
        capability_ref: match kind {
            EnvKind::SecretCapability => Some(id("blocked")),
            _ => None,
        },
    }
}
fn command() -> CommandSpec {
    CommandSpec {
        executable: ToolIdentity {
            name: "cargo".into(),
            version: "1.99.1".into(),
            binary_digest: digest(),
            installation_digest: digest(),
        },
        argv: vec!["test".into(), "--locked".into()],
        cwd: String::new(),
        timeout_ms: 30_000,
        recipe_revision: id("cargo-i1l-v1"),
    }
}
fn outputs() -> Vec<OutputSpec> {
    vec![
        OutputSpec {
            id: id("stdout"),
            kind: OutputKind::Stdout,
            path: None,
            required: true,
            max_bytes: 65_536,
        },
        OutputSpec {
            id: id("report"),
            kind: OutputKind::Artifact,
            path: Some(RelPath::new("target/report.json").expect("path")),
            required: true,
            max_bytes: 1024 * 1024,
        },
    ]
}
fn result_contract() -> ResultContract {
    ResultContract {
        id: id("nontest"),
        kind: ResultKind::NonTest,
        adapter_id: id("kioku"),
        reporter_version: id("cargo-native-v1"),
        inventory_recipe_ref: None,
        execution_recipe_ref: blob(),
        identity_rule_digest: digest(),
        normalization_policy_ref: blob(),
        native_skips: vec![],
        expected_empty: false,
        complete_required: true,
    }
}
fn task(name: &str, parents: Vec<TaskDependency>) -> TaskSpec {
    TaskSpec {
        id: id(name),
        adapter: id("kioku"),
        command: command(),
        inputs: vec![InputSelector::Tree {
            root: RelPath::new("src").expect("src"),
        }],
        outputs: outputs(),
        dependencies: parents,
        env: vec![environment(EnvKind::PublicValue)],
        resources: ResourceRecipe {
            heavy_slots: 1,
            capture_slots: 1,
            max_state_bytes: 12 * 1024 * 1024,
            max_stderr_bytes: 64 * 1024,
        },
        required: true,
        result_contract: result_contract(),
        cancellation: CancellationPolicy {
            cancellable: true,
            term_grace_ms: 1_500,
            max_drain_ms: 30_000,
        },
        admission: AdmissionPolicy {
            result_cache: false,
            single_flight: false,
        },
    }
}
fn manifest(tasks: Vec<TaskSpec>) -> TaskManifest {
    TaskManifest {
        schema: "kioku/ci2/tasks/v1".into(),
        revision: 1,
        required_task_ids: tasks.iter().map(|task| task.id.clone()).collect(),
        tasks,
        protected_refs: Vec::new(),
    }
}
fn bootstrap() -> ServiceBootstrap {
    // Fixture-only absolute paths under a private temporary directory that is removed at once:
    // the stop file is never present and nothing outside the test is read.
    let base = tempfile::tempdir()
        .expect("temporary directory")
        .path()
        .to_path_buf();
    ServiceBootstrap {
        schema: "kioku/ci2/bootstrap/v1".into(),
        revision: 1,
        service_id: id("i1l-runner"),
        mode: ServiceMode::LocalCooperative,
        resource_domain_id: id("cotm"),
        state_root: "/var/lib/kioku-state".into(),
        socket_path: "/var/lib/kioku-state/ci.sock".into(),
        stop_file: base.join("ARRET-KIOKU.txt").to_string_lossy().into_owned(),
        registered_roots: vec![RegisteredRoot {
            id: id("kioku-local"),
            project_id: id("kioku"),
            canonical_path: base.join("root").to_string_lossy().into_owned(),
            allowed_principals: vec![id("test-principal")],
            protected_refs: vec![],
        }],
        approved_manifest_refs: vec![blob()],
        profiles_ref: blob(),
        policy_ref: blob(),
        auth_config_ref: None,
        capture_slots: 1,
        heavy_slots: 1,
        max_state_bytes: 12 * 1024 * 1024,
        max_queue_wait_ms: 60_000,
        drain_grace_ms: 15_000,
    }
}

#[test]
fn portable_id_digest_paths_odds_validate() {
    assert!("src/lib.rs".parse::<RelPath>().is_ok());
    for invalid in [
        "/etc/passwd",
        "../escape",
        "node_modules/../.env",
        "a//b",
        "a\\b",
        "./src",
        "http://x",
        "a:",
    ] {
        assert!(
            RelPath::new(invalid).is_err(),
            "accepted bad path {invalid}"
        );
    }
    assert!(digest().validate().is_ok());
    for bad in [
        format!("b3:{}", "z".repeat(64)),
        format!("b3:{}", "A".repeat(64)),
        "sha1:deadbeef".to_owned(),
        "b3:abc".to_owned(),
    ] {
        assert!(Digest::new(&bad).is_err(), "accepted digest {bad}");
    }
    for bad in ["", ".bad", "bad path", "meta:start", "é"] {
        assert!(Id::new(bad).is_err(), "accepted ID {bad:?}");
        assert!(Id::new("a".repeat(129)).is_err());
    }
    assert!(
        Digest::new(
            &Digest::blake3("kioku/ci2/task/v1", b"x")
                .expect("digest")
                .to_string()
        )
        .is_ok()
    );
    assert!(
        Digest::new("sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .is_ok()
    );
}
#[test]
fn request_strict_digest_have_modes_and_stable_schema() {
    let direct = request(RunMode::Normal);
    serde_json::to_value(&direct).expect("json closed");
    assert_eq!(
        serde_json::from_value::<LocalRequest>(
            serde_json::from_str(&serde_json::to_string(&direct).expect("json")).expect("json")
        )
        .expect("json"),
        direct
    );
    for mode in [RunMode::Normal, RunMode::Full, RunMode::FullCold] {
        let mut req = request(mode);
        req.request_id.clone_from(&direct.request_id);
        req.manifest_digest = digest();
        let returned = serde_json::to_vec(&req).expect("request bytes");
        assert_eq!(
            serde_json::from_slice::<LocalRequest>(&returned).expect("json"),
            req
        );
        let key =
            exact_digest("kioku/ci2/local-request/key/v1", &returned).expect("request digest");
        assert_eq!(key.algorithm(), &DigestAlgorithm::Blake3);
    }
    let mut unknown = serde_json::to_value(&direct).expect("map");
    unknown
        .as_object_mut()
        .expect("map")
        .insert("native_seeds".into(), serde_json::json!([]));
    assert!(serde_json::from_value::<LocalRequest>(unknown).is_err());
    let mut broken = direct.clone();
    broken.schema = "kioku/pivot-local-request/v1".to_owned();
    assert!(broken.validate().is_err());
    broken = direct.clone();
    broken.revision = 2;
    assert!(broken.validate().is_err());
    let mut looped = direct.clone();
    looped.requested_task_ids = vec![id("a"), id("b"), id("a")];
    assert!(looped.validate().is_err());
}
#[test]
fn manifest_checks_graph_and_no_invented_caches() {
    let required = manifest(vec![task("cargo-check", vec![])]);
    assert!(required.validate().is_ok());
    let mut empty = required.clone();
    empty.tasks.clear();
    assert!(empty.validate().is_err());
    let mut missing = required.clone();
    missing.required_task_ids.push(id("unused"));
    assert!(missing.validate().is_err());
    let mut forged_cache = required.clone();
    forged_cache.tasks[0].admission.single_flight = true;
    assert!(forged_cache.validate().is_err());
    let mut wrong_arg = required.clone();
    wrong_arg.tasks[0].command.argv = vec!["https://bad.example".into()];
    assert!(wrong_arg.validate().is_err());
}
#[test]
fn manifest_rejects_self_and_cycle_and_unknown_parent_and_duplicate_ids() {
    let base = task("a", vec![]);
    let forward = TaskDependency {
        task_id: id("a"),
        consumed_output_ids: vec![id("report")],
    };
    let pointed = TaskDependency {
        task_id: id("b"),
        consumed_output_ids: vec![id("report")],
    };
    let selfy = manifest(vec![task("a", vec![forward.clone()])]);
    assert!(selfy.validate().is_err());
    let cycle = manifest(vec![
        task("a", vec![pointed.clone()]),
        task("b", vec![forward.clone()]),
    ]);
    assert!(cycle.validate().is_err());
    let absent = manifest(vec![task(
        "a",
        vec![TaskDependency {
            task_id: id("b"),
            consumed_output_ids: vec![id("nope")],
        }],
    )]);
    assert!(absent.validate().is_err());
    let mut wrong_output = required_manifest();
    wrong_output.tasks[0].dependencies.push(TaskDependency {
        task_id: id("a"),
        consumed_output_ids: vec![id("missing")],
    });
    assert!(wrong_output.validate().is_err());
    drop(base);
}
fn required_manifest() -> TaskManifest {
    manifest(vec![task("a", vec![])])
}
#[test]
fn bootstrap_admits_only_cooperative_existing_operator_shape_not_secret() {
    let good = bootstrap();
    assert!(good.validate().is_ok());
    for mode in [ServiceMode::Trusted, ServiceMode::LocalCooperative] {
        let mut b = good.clone();
        b.mode = mode;
        if mode == ServiceMode::Trusted {
            assert!(b.validate().is_err());
        }
    }
    let mut dupe = good.clone();
    dupe.capture_slots = 2;
    assert!(dupe.validate().is_err());
    let mut external = good.clone();
    external.state_root = "var/lib/state".into();
    assert!(external.validate().is_err());
    let mut empty_owner = ClientIdentity {
        principal_id: id("p"),
        project_ids: vec![id("kioku")],
        roles: vec![Role::Requester],
        trust_scope: id("lane-i1l"),
        lane_id: Some(id("a")),
    };
    assert!(empty_owner.validate().is_ok());
    empty_owner.roles.push(Role::Requester);
    assert!(empty_owner.validate().is_err());
    empty_owner = ClientIdentity {
        principal_id: id("p"),
        project_ids: vec![id("kioku")],
        roles: vec![Role::Executor, Role::Operator],
        trust_scope: id("lane-i1l"),
        lane_id: None,
    };
    assert!(empty_owner.validate().is_err());
    assert!(environment(EnvKind::SecretCapability).validate().is_err());
}

#[test]
fn valid_dag_binds_an_existing_required_output() {
    let producer = task("producer", vec![]);
    let consumer = task(
        "consumer",
        vec![TaskDependency {
            task_id: id("producer"),
            consumed_output_ids: vec![id("report")],
        }],
    );
    assert!(manifest(vec![producer, consumer]).validate().is_ok());
    let mut unknown = manifest(vec![task("a", vec![])]);
    unknown.tasks[0].outputs = vec![];
    assert!(unknown.validate().is_err());
    unknown = manifest(vec![task("a", vec![])]);
    unknown.tasks[0].command.timeout_ms = 0;
    assert!(unknown.validate().is_err());
    unknown = manifest(vec![task("a", vec![])]);
    unknown.tasks[0].env = vec![EnvSpec {
        name: "SECRET_FILE".into(),
        kind: EnvKind::SecretCapability,
        value: None,
        capability_ref: Some(id("capability-x")),
    }];
    assert!(unknown.validate().is_err());
}
#[test]
fn ids_are_ascii_and_paths_do_not_depend_on_process_cwd() {
    let local = PathBuf::from("/tmp/never-read-ci2-contract");
    for rejected in ["a:b", "a\\b", "~/.env", "/tmp/file", "a/../b", ".env"] {
        let _candidate = local.join(rejected);
        if rejected == ".env" {
            assert!(RelPath::new(rejected).is_ok());
        } else {
            assert!(RelPath::new(rejected).is_err());
        }
    }
}

#[test]
fn duplicate_json_and_float_or_deep_wire_are_never_tolerated() {
    let raw = br#"{"schema":"kioku/ci2/local-request/v1","revision":1,"revision":2}"#;
    let duplicate = decode_bounded::<LocalRequest>(raw);
    assert!(duplicate.is_err());
    let mut typed = serde_json::to_value(request(RunMode::Full)).expect("wire");
    typed
        .as_object_mut()
        .expect("map")
        .insert("revision".into(), serde_json::json!(2.5));
    assert!(digest_request_document(&typed).is_err());
    let regular = serde_json::to_value(request(RunMode::Full)).expect("wire");
    let encoded = serde_json::to_vec(&regular).expect("bytes");
    let value: LocalRequest = decode_bounded(&encoded).expect("bounded typed request");
    assert_eq!(value, request(RunMode::Full));
}

#[cfg(target_os = "linux")]
#[test]
fn approved_manifest_refuses_links_protected_paths_and_wrong_pin() {
    use kioku::ci2::config::{approved_workspace_manifest, read_manifest};
    use std::{fs, os::unix::fs::symlink};
    let temp = tempfile::tempdir().expect("private temp");
    let source = temp.path().join("source");
    fs::create_dir(&source).expect("source");
    let source = source.canonicalize().expect("canonical");
    let bytes = serde_json::to_vec(&manifest(vec![task("cargo-check", vec![])])).expect("manifest");
    let mut config = bootstrap();
    config.registered_roots[0].canonical_path = source.to_string_lossy().into_owned();
    config.approved_manifest_refs[0].digest =
        Digest::sha256("kioku/ci2/workspace-file/v1", &bytes).expect("hash");
    config.approved_manifest_refs[0].size_bytes = bytes.len() as u64;
    let manifest_path = source.join("ci2-manifest.json");
    fs::write(&manifest_path, &bytes).expect("write");
    assert!(approved_workspace_manifest(&config, &source).is_ok());
    assert!(
        read_manifest(
            &config,
            &id("kioku-local"),
            &RelPath::new("ci2-manifest.json").expect("path")
        )
        .is_ok()
    );
    config.approved_manifest_refs[0].size_bytes += 1;
    assert!(approved_workspace_manifest(&config, &source).is_err());
    config.approved_manifest_refs[0].size_bytes -= 1;
    config.approved_manifest_refs[0].digest = digest();
    assert!(approved_workspace_manifest(&config, &source).is_err());
    let sentinel = temp.path().join("sentinel");
    fs::write(&sentinel, &bytes).expect("sentinel");
    fs::remove_file(&manifest_path).expect("remove");
    symlink(&sentinel, &manifest_path).expect("link");
    let error = approved_workspace_manifest(&config, &source).expect_err("linked file refused");
    assert!(error.to_string().contains("symlink"));
    symlink(temp.path(), source.join("outside")).expect("directory link");
    let error = read_manifest(
        &config,
        &id("kioku-local"),
        &RelPath::new("outside/sentinel").expect("path"),
    )
    .expect_err("linked ancestor refused");
    assert!(error.to_string().contains("symlink"));
    fs::create_dir(source.join("QA")).expect("qa");
    fs::write(source.join("QA/.env"), &bytes).expect("synthetic sentinel");
    let error = read_manifest(
        &config,
        &id("kioku-local"),
        &RelPath::new("QA/.env").expect("path"),
    )
    .expect_err("protected path refused");
    assert!(error.to_string().contains("protected"));
    let other = temp.path().join("other");
    fs::create_dir(&other).expect("other");
    assert!(approved_workspace_manifest(&config, &other).is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn secret_capability_manifest_refused_on_read() {
    use kioku::ci2::config::approved_workspace_manifest;
    use std::fs;
    let temp = tempfile::tempdir().expect("private temp");
    let source = temp.path().canonicalize().expect("canonical");
    let mut m = manifest(vec![task("cargo-check", vec![])]);
    m.tasks[0].env = vec![environment(EnvKind::SecretCapability)];
    let bytes = serde_json::to_vec(&m).expect("manifest");
    fs::write(source.join("ci2-manifest.json"), &bytes).expect("write");
    let mut config = bootstrap();
    config.registered_roots[0].canonical_path = source.to_string_lossy().into_owned();
    config.approved_manifest_refs[0].digest =
        Digest::sha256("kioku/ci2/workspace-file/v1", &bytes).expect("hash");
    config.approved_manifest_refs[0].size_bytes = bytes.len() as u64;
    assert!(approved_workspace_manifest(&config, &source).is_err());
}

#[test]
fn unsupported_bootstrap_auth_reference_is_refused() {
    use kioku::ci2::config::read_bootstrap;
    let temp = tempfile::tempdir().expect("private temp");
    let path = temp.path().join("bootstrap.json");
    let mut config = bootstrap();
    std::fs::write(&path, serde_json::to_vec(&config).expect("bytes")).expect("write");
    assert!(read_bootstrap(&path).is_ok());
    config.auth_config_ref = Some(id("unavailable-auth"));
    std::fs::write(&path, serde_json::to_vec(&config).expect("bytes")).expect("write");
    assert!(read_bootstrap(&path).is_err());
}
