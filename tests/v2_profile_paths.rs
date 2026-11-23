//! Pure profile-identity and portable-path contract checks.
//!
//! These tests exercise only existing `kioku::ci2` domain value types. They do
//! not start a daemon, a runner, or any native session, and they make no claim
//! about native execution or traced reuse.

use kioku::ci2::*;

fn id(value: &str) -> Id {
    Id::new(value).unwrap_or_else(|error| panic!("fixture ID {value} rejected: {error}"))
}

fn digest() -> Digest {
    Digest::new(&format!("b3:{}", "a".repeat(64))).expect("fixture digest")
}

fn blob() -> BlobRef {
    BlobRef {
        digest: digest(),
        size_bytes: 17,
        media_type: "application/json".into(),
    }
}

/// The published profile identifier is an `Id`, so it must survive the wire
/// unchanged and must never be spelled as a path.
#[test]
fn profile_identifier_is_a_wire_stable_id() {
    let profile = id("profile-local-observed");
    assert_eq!(profile.as_str(), "profile-local-observed");
    assert!(profile.validate().is_ok());
    let encoded = serde_json::to_string(&profile).expect("profile identifier encodes");
    assert_eq!(encoded, "\"profile-local-observed\"");
    let decoded: Id = serde_json::from_str(&encoded).expect("profile identifier decodes");
    assert_eq!(decoded, profile);
    for rejected in [
        "profiles/local-observed",
        "profiles\\local-observed",
        "profile local",
        "",
        "Profile-Local-Observed/",
        "profile-observé",
    ] {
        assert!(
            Id::new(rejected).is_err(),
            "profile identifier accepted {rejected:?}"
        );
    }
}

/// A profile reference travels as a `RelPath`, so platform-specific path
/// shapes must be refused before the value can reach the filesystem.
#[test]
fn profile_paths_reject_os_specific_shapes() {
    let portable = RelPath::new("profiles/local-observed.json").expect("portable profile path");
    assert_eq!(portable.as_str(), "profiles/local-observed.json");
    assert!(portable.validate().is_ok());
    for rejected in [
        "/profiles/local-observed.json",
        "profiles\\local-observed.json",
        "profiles/./local-observed.json",
        "profiles/../local-observed.json",
        "profiles//local-observed.json",
        "profiles/local-observed.json/",
        "~/profiles/local-observed.json",
        "C:/profiles/local-observed.json",
        "profiles/local-observed.json\u{0}",
    ] {
        assert!(
            RelPath::new(rejected).is_err(),
            "profile path accepted {rejected:?}"
        );
    }
}

/// Capability profiles keep one closed wire spelling on every platform.
#[test]
fn capability_profile_wire_names_are_closed() {
    for (profile, wire) in [
        (CapabilityProfile::LabCooperative, "\"lab_cooperative\""),
        (CapabilityProfile::RunnerIsolated, "\"runner_isolated\""),
        (CapabilityProfile::RunnerEnforced, "\"runner_enforced\""),
    ] {
        assert_eq!(
            serde_json::to_string(&profile).expect("capability profile encodes"),
            wire
        );
        let decoded: CapabilityProfile =
            serde_json::from_str(wire).expect("capability profile decodes");
        assert_eq!(decoded, profile);
    }
    for rejected in ["\"lab-cooperative\"", "\"LabCooperative\"", "\"runner\""] {
        assert!(
            serde_json::from_str::<CapabilityProfile>(rejected).is_err(),
            "capability profile accepted {rejected}"
        );
    }
}

fn bootstrap(capture_slots: u16, heavy_slots: u8) -> ServiceBootstrap {
    ServiceBootstrap {
        schema: "kioku/ci2/bootstrap/v1".into(),
        revision: 1,
        service_id: id("i1l-runner"),
        mode: ServiceMode::LocalCooperative,
        resource_domain_id: id("cotm"),
        state_root: "/var/lib/kioku-state".into(),
        socket_path: "/var/lib/kioku-state/ci.sock".into(),
        stop_file: "/var/lib/kioku-state/STOP".into(),
        registered_roots: vec![RegisteredRoot {
            id: id("kioku-local"),
            project_id: id("kioku"),
            canonical_path: "/srv/kioku".into(),
            allowed_principals: vec![id("local-operator")],
            protected_refs: vec!["refs/heads/main".into()],
        }],
        approved_manifest_refs: vec![blob()],
        profiles_ref: blob(),
        policy_ref: blob(),
        auth_config_ref: None,
        capture_slots,
        heavy_slots,
        max_state_bytes: 12 * 1024 * 1024,
        max_queue_wait_ms: 60_000,
        drain_grace_ms: 15_000,
    }
}

/// The accepted profile shape is exactly one capture slot and one heavy slot;
/// any other shape stays unsupported instead of being normalized.
#[test]
fn bootstrap_profile_shape_is_one_and_one() {
    assert!(bootstrap(1, 1).validate().is_ok());
    for (capture, heavy) in [(0, 1), (2, 1), (1, 0), (1, 2)] {
        let result = bootstrap(capture, heavy).validate();
        assert!(
            result.is_err(),
            "bootstrap accepted capture_slots={capture} heavy_slots={heavy}"
        );
    }
}

/// A profile document is decoded as closed TOML: unknown and duplicate
/// top-level keys are refused rather than merged.
#[test]
fn profile_document_toml_is_closed() {
    let document = "profile_id = \"profile-local-observed\"\ncapture_slots = 1\nheavy_slots = 1\n";
    let parsed: toml::Value = toml::from_str(document).expect("profile document parses");
    let table = parsed.as_table().expect("profile document is a table");
    assert_eq!(table.len(), 3);
    assert!(
        toml::from_str::<toml::Value>("profile_id = \"a\"\nprofile_id = \"b\"\n").is_err(),
        "duplicate profile key accepted"
    );
    assert!(
        toml::from_str::<toml::Value>("profile_id = \"a\"\nprofile_id = \"a\"\n").is_err(),
        "duplicate profile key with equal values accepted"
    );
}
