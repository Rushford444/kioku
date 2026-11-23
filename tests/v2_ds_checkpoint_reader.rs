#![cfg(target_os = "linux")]
//! Adversarial review of the native closure checkpoint reader.
//!
//! Every case starts from a genuine receipt produced by the trusted monitor in
//! this process tree, then damages a copy of the bytes. The reader must reject
//! the forgery and must not rewrite the file it inspected.
use kioku::ci2::native::{self, SupervisorError};
use kioku::ci2::storage::ProcessEvidence;
use kioku::ci2::*;
use sha2::{Digest as _, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};

fn digest() -> Digest {
    Digest::new(&("b3:".to_owned() + &"a".repeat(64))).unwrap()
}
fn id(value: &str) -> Id {
    Id::new(value).unwrap()
}
fn reference() -> BlobRef {
    BlobRef {
        digest: digest(),
        size_bytes: 0,
        media_type: "application/octet-stream".into(),
    }
}
fn task() -> TaskSpec {
    TaskSpec {
        id: id("fixture-parent"),
        adapter: id("python-eval-none"),
        command: CommandSpec {
            executable: ToolIdentity {
                name: "python3".into(),
                version: "3.11".into(),
                binary_digest: digest(),
                installation_digest: digest(),
            },
            argv: vec!["-c".into(), "pass".into()],
            cwd: String::new(),
            timeout_ms: 15_000,
            recipe_revision: id("v1"),
        },
        inputs: vec![],
        outputs: vec![],
        dependencies: vec![],
        env: vec![],
        resources: ResourceRecipe {
            heavy_slots: 1,
            capture_slots: 1,
            max_state_bytes: 65536,
            max_stderr_bytes: 65536,
        },
        required: true,
        result_contract: ResultContract {
            id: id("nontest"),
            kind: ResultKind::NonTest,
            adapter_id: id("python-eval"),
            reporter_version: id("explicit-exit-native-v1"),
            inventory_recipe_ref: None,
            execution_recipe_ref: reference(),
            identity_rule_digest: digest(),
            normalization_policy_ref: reference(),
            native_skips: vec![],
            expected_empty: false,
            complete_required: true,
        },
        cancellation: CancellationPolicy {
            cancellable: true,
            term_grace_ms: 200,
            max_drain_ms: 800,
        },
        admission: AdmissionPolicy {
            result_cache: false,
            single_flight: false,
        },
    }
}
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}
fn fingerprint(bytes: &[u8]) -> (u64, String) {
    (
        u64::try_from(bytes.len()).unwrap(),
        format!("{:x}", Sha256::digest(bytes)),
    )
}
/// Produce a real closure checkpoint and return its work directory and evidence.
fn genuine_receipt(base: &Path) -> (PathBuf, ProcessEvidence, Vec<u8>) {
    let source = base.join("source");
    fs::create_dir(&source).unwrap();
    let supervisor = base.join("supervisor.py");
    fs::copy(root().join("src/ci2/native/supervisor.py"), &supervisor).unwrap();
    let stop = base.join("cancel");
    let work = fs::canonicalize(&source).unwrap().join("run");
    let observation = native::supervise_with_stops_and_possession(
        &task(),
        Path::new("/usr/bin/python3"),
        &source,
        &work,
        &supervisor,
        &stop,
        &[],
        Some(&mut |_| Ok(())),
    )
    .expect("genuine supervision");
    let evidence = observation.process_evidence.expect("possession evidence");
    assert!(
        observation.absence_proof.is_some(),
        "genuine monitor closure is expected"
    );
    let bytes = fs::read(work.join("possession-closure.json")).unwrap();
    (work, evidence, bytes)
}

fn expect_rejected(work: &Path, evidence: &ProcessEvidence, damage: &[u8], label: &str) {
    let path = work.join("possession-closure.json");
    fs::write(&path, damage).unwrap();
    let before = fingerprint(&fs::read(&path).unwrap());
    let outcome = native::read_absence_proof(work, evidence);
    let after = fingerprint(&fs::read(&path).unwrap());
    assert_eq!(before, after, "{label}: reader rewrote the checkpoint");
    match outcome {
        Err(SupervisorError::Wire(message)) => {
            assert_eq!(message, "invalid native closure checkpoint", "{label}");
        }
        other => panic!("{label}: expected Wire rejection, got {other:?}"),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn forged_checkpoint_bytes_are_rejected_without_rewriting_the_file() {
    let base = tempfile::tempdir().unwrap();
    let (work, evidence, bytes) = genuine_receipt(base.path());
    let text = String::from_utf8(bytes.clone()).unwrap();
    assert!(text.contains("\"no_children\": true"), "sorted JSON shape");
    assert!(text.contains("\"result\": "), "result object present");

    let mut cases = 0u32;
    // Non-object top level: serde must not treat a scalar as an empty record.
    for (label, damage) in [
        ("top-level array", b"[]".to_vec()),
        ("top-level string", b"\"closure\"".to_vec()),
        ("top-level null", b"null".to_vec()),
        ("top-level integer", b"7".to_vec()),
    ] {
        expect_rejected(&work, &evidence, &damage, label);
        cases += 1;
    }
    // Booleans are not integers and integers are not booleans.
    for (label, needle, replacement) in [
        (
            "bool field as int",
            "\"no_children\": true",
            "\"no_children\": 1",
        ),
        (
            "bool field as string",
            "\"original_group_empty\": true",
            "\"original_group_empty\": \"true\"",
        ),
        (
            "bool field as null",
            "\"original_group_empty\": true",
            "\"original_group_empty\": null",
        ),
    ] {
        assert!(text.contains(needle), "{label}: fixture shape");
        let damage = text.replacen(needle, replacement, 1).into_bytes();
        expect_rejected(&work, &evidence, &damage, label);
        cases += 1;
    }
    // Null and wrongly typed nested objects must not deserialize into defaults.
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    for (label, mutate) in [
        ("header null", "header"),
        ("header as array", "header-array"),
        ("result null", "result"),
        ("result as string", "result-string"),
    ] {
        let mut damaged = value.clone();
        match mutate {
            "header" => damaged["header"] = serde_json::Value::Null,
            "header-array" => damaged["header"] = serde_json::json!([]),
            "result" => damaged["result"] = serde_json::Value::Null,
            _ => damaged["result"] = serde_json::json!("result"),
        }
        expect_rejected(
            &work,
            &evidence,
            &serde_json::to_vec(&damaged).unwrap(),
            label,
        );
        cases += 1;
    }
    // Truncation at several depths, including mid-string and mid-object.
    for (label, cut) in [("truncated header", 64), ("truncated mid-string", 260)] {
        assert!(cut < bytes.len());
        expect_rejected(&work, &evidence, &bytes[..cut], label);
        cases += 1;
    }
    // Duplicate authoritative keys: serde's derived struct rejects a repeated
    // field rather than letting the last value silently win.
    let duplicate_bool = text.replacen(
        "\"no_children\": true",
        "\"no_children\": true, \"no_children\": true",
        1,
    );
    assert_ne!(duplicate_bool, text, "duplicate bool injected");
    expect_rejected(
        &work,
        &evidence,
        duplicate_bool.as_bytes(),
        "duplicate no_children",
    );
    cases += 1;
    let schema_value = "\"kioku/ci2/native-closure/v1\"";
    let duplicate_schema = text.replacen(
        schema_value,
        &format!("{schema_value}, \"schema\": {schema_value}"),
        1,
    );
    assert_ne!(duplicate_schema, text, "duplicate schema injected");
    expect_rejected(
        &work,
        &evidence,
        duplicate_schema.as_bytes(),
        "duplicate schema",
    );
    cases += 1;

    // Unknown keys are not silently ignored at any level.
    let unknown = text.replacen("{\"header\"", "{\"extra\": true, \"header\"", 1);
    expect_rejected(
        &work,
        &evidence,
        unknown.as_bytes(),
        "unknown top-level key",
    );
    cases += 1;

    // Broken UTF-8 inside a string value and an unterminated stream.
    let mut invalid_utf8 = bytes.clone();
    let offset = text.find("kioku/ci2/native-closure/v1").unwrap() + 6;
    invalid_utf8[offset] = 0xFF;
    expect_rejected(&work, &evidence, &invalid_utf8, "invalid UTF-8 in string");
    cases += 1;
    let unterminated = text.replacen(
        "\"kioku/ci2/native-closure/v1\"",
        "\"kioku/ci2/native-closure/v1",
        1,
    );
    expect_rejected(
        &work,
        &evidence,
        unterminated.as_bytes(),
        "unterminated string",
    );
    cases += 1;
    let bad_escape = text.replacen(
        "\"kioku/ci2/native-closure/v1\"",
        "\"kioku/ci2/native-closure/v1\\u12\"",
        1,
    );
    expect_rejected(&work, &evidence, bad_escape.as_bytes(), "truncated escape");
    cases += 1;

    // The genuine bytes still parse to a proof: the damage, not the harness, is rejected.
    let path = work.join("possession-closure.json");
    fs::write(&path, &bytes).unwrap();
    let proof = native::read_absence_proof(&work, &evidence)
        .unwrap()
        .expect("genuine closure");
    assert_eq!(proof.evidence(), &evidence);
    assert!(proof.receipt().leader_reaped && proof.receipt().subreaper_echild);
    assert_eq!(
        proof.receipt().scope,
        native::AbsenceScope::OriginalGroupAndAdoptedDescendants
    );
    assert_eq!(fingerprint(&fs::read(&path).unwrap()), fingerprint(&bytes));

    assert_eq!(cases, 19, "adversarial cases executed");
}
