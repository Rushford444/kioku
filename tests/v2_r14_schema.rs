//! Regression: tagged wire enums are closed records, like every struct around them.
//!
//! `decode_bounded` enforces duplicate-key, control-character, size and integer
//! bounds before typed decoding, but the typed decode itself decides whether an
//! unknown field is refused or silently dropped. `LocalRequest` and its nested
//! structs are `deny_unknown_fields`; the two internally tagged enums were not,
//! so a variant could carry an unrecognised field that the decoder dropped.
#![forbid(unsafe_code)]

use kioku::ci2::*;
use serde_json::{Value, json};

fn digest() -> Digest {
    Digest::new(&format!("sha256:{}", "a".repeat(64))).expect("valid fixture")
}

fn id(value: &str) -> Id {
    Id::new(value).expect("valid fixture")
}

/// The real `LocalRequest` wire shape, as used by `tests/v2_contracts.rs`.
fn request_value() -> Value {
    let request = LocalRequest {
        schema: "kioku/ci2/local-request/v1".into(),
        revision: 1,
        request_id: id("request-r14"),
        project_id: id("kioku"),
        origin_claim: OriginClaim::LocalRegisteredCheckout {
            root_id: id("kioku-local"),
            relative_path: RelPath::new("tests/fixtures/ci2/stub").expect("relative path"),
        },
        manifest_digest: digest(),
        profile_id: id("local-observed"),
        requested_task_ids: vec![id("cargo-check")],
        mode: RunMode::Normal,
        wait_class: WaitClass::Agent,
    };
    serde_json::to_value(request).expect("wire map")
}

fn manifest_value() -> Value {
    let blob = json!({"digest": digest(), "size_bytes": 0, "media_type": "application/json"});
    json!({
        "schema": "kioku/ci2/tasks/v1",
        "revision": 1,
        "protected_refs": [],
        "required_task_ids": ["one"],
        "tasks": [{
            "id": "one",
            "adapter": "fixture-adapter",
            "command": {
                "executable": {"name": "fixture", "version": "1",
                    "binary_digest": digest(), "installation_digest": digest()},
                "argv": ["one"], "cwd": "", "timeout_ms": 10000, "recipe_revision": "v1"},
            "inputs": [{"kind": "file", "path": "source.txt"}],
            "outputs": [{"id": "stdout", "kind": "stdout", "path": null,
                "required": true, "max_bytes": 4096}],
            "dependencies": [], "env": [], "required": true,
            "resources": {"heavy_slots": 1, "capture_slots": 1,
                "max_state_bytes": 4096, "max_stderr_bytes": 4096},
            "result_contract": {"id": "fixture-result", "kind": "non_test",
                "adapter_id": "fixture-adapter", "reporter_version": "raw-native-exit-v1",
                "inventory_recipe_ref": null, "execution_recipe_ref": blob,
                "identity_rule_digest": digest(), "normalization_policy_ref": blob,
                "native_skips": [], "expected_empty": false, "complete_required": true},
            "cancellation": {"cancellable": true, "term_grace_ms": 100, "max_drain_ms": 500},
            "admission": {"result_cache": false, "single_flight": false}
        }]
    })
}

/// A variant of a tagged enum must refuse a field it does not declare.
///
/// The claim this fixture carries is exactly the kind an operator could try to
/// add: `authority` is a real field of `RunOrigin` and means "verified forge"
/// there. Silently dropping it from `OriginClaim` accepts a wire document whose
/// author asserted something the decoder never read.
#[test]
fn origin_claim_variant_refuses_an_undeclared_field() {
    let mut value = request_value();
    value["origin_claim"]
        .as_object_mut()
        .expect("tagged variant map")
        .insert("authority".into(), json!("verified_forge"));
    let refused = decode_bounded::<LocalRequest>(&serde_json::to_vec(&value).expect("wire"));
    assert!(
        refused.is_err(),
        "an undeclared origin_claim field was silently dropped: {refused:?}"
    );

    // The unmodified document is the same record and must still decode.
    let clean = serde_json::to_vec(&request_value()).expect("wire");
    let decoded: LocalRequest = decode_bounded(&clean).expect("valid request");
    assert_eq!(decoded.profile_id, id("local-observed"));
}

/// The same rule holds for the manifest's tagged `InputSelector`.
#[test]
fn input_selector_variant_refuses_an_undeclared_field() {
    let mut value = manifest_value();
    value["tasks"][0]["inputs"][0]
        .as_object_mut()
        .expect("tagged variant map")
        .insert("root".into(), json!("elsewhere"));
    let refused = decode_bounded::<TaskManifest>(&serde_json::to_vec(&value).expect("wire"));
    assert!(
        refused.is_err(),
        "an undeclared input selector field was silently dropped: {refused:?}"
    );

    // A declared selector with the same kind and path stays valid.
    let clean = serde_json::to_vec(&manifest_value()).expect("wire");
    let decoded: TaskManifest = decode_bounded(&clean).expect("valid manifest");
    assert_eq!(decoded.tasks.len(), 1);
    assert_eq!(decoded.tasks[0].inputs.len(), 1);
}
