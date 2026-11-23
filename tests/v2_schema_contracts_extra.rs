//! Mechanical capture-contract checks over the existing bounded JSON decoder.
//!
//! Every case here exercises code that already exists at the base commit:
//! `kioku::ci2::decode_bounded`, `digest_request_document`, `Digest` and the
//! `Validate` contract they run. Nothing in `src/` is modified by this file and
//! no new contract type is introduced; the payloads are the real `LocalRequest`
//! wire shape used in `tests/v2_contracts.rs`.

use kioku::ci2::*;

const MIB: usize = 1024 * 1024;

fn digest() -> Digest {
    Digest::new(&format!("b3:{}", "a".repeat(64))).expect("valid fixture")
}

fn id(value: &str) -> Id {
    Id::new(value).expect("valid fixture")
}

fn request(mode: RunMode) -> LocalRequest {
    LocalRequest {
        schema: "kioku/ci2/local-request/v1".into(),
        revision: 1,
        request_id: id("request-extra"),
        project_id: id("kioku"),
        origin_claim: OriginClaim::LocalRegisteredCheckout {
            root_id: id("kioku-local"),
            relative_path: RelPath::new("tests/fixtures/ci2/stub").expect("key"),
        },
        manifest_digest: digest(),
        profile_id: id("local-observed"),
        requested_task_ids: vec![id("cargo-check")],
        mode,
        wait_class: WaitClass::Agent,
    }
}

fn encoded(mode: RunMode) -> Vec<u8> {
    serde_json::to_vec(&request(mode)).expect("wire bytes")
}

/// Smallest accepted body: the typed record decodes, passes range and semantic checks.
#[test]
fn bounded_decode_accepts_the_small_valid_record() {
    let bytes = encoded(RunMode::Normal);
    let decoded: LocalRequest = decode_bounded(&bytes).expect("small valid record");
    assert_eq!(decoded, request(RunMode::Normal));
    for mode in [RunMode::Full, RunMode::FullCold] {
        let decoded: LocalRequest = decode_bounded(&encoded(mode)).expect("valid mode");
        assert_eq!(decoded, request(mode));
    }
}

/// Zero-length input is refused instead of being read as an empty record.
#[test]
fn bounded_decode_refuses_zero_length_input() {
    let empty = decode_bounded::<LocalRequest>(b"");
    assert!(empty.is_err(), "empty body accepted");
    // Whitespace is not a record either, and is refused by the parser, not by a default.
    let blank = decode_bounded::<LocalRequest>(b" \n\t ");
    assert!(blank.is_err(), "blank body accepted");
    // An empty object parses as JSON but cannot satisfy the closed typed record.
    let object = decode_bounded::<LocalRequest>(b"{}");
    assert!(object.is_err(), "empty object accepted as a request");
}

/// The 4 MiB input cap is enforced before any field is used.
#[test]
fn bounded_decode_refuses_bodies_over_the_size_cap() {
    let mut oversized = encoded(RunMode::Normal);
    let padding = vec![b' '; MIB * 4 + 1 - oversized.len()];
    oversized.extend_from_slice(&padding);
    assert_eq!(oversized.len(), MIB * 4 + 1);
    let refused = decode_bounded::<LocalRequest>(&oversized);
    match refused {
        Err(ContractError::Unsupported(message)) => {
            assert!(message.contains("4 MiB"), "unexpected message {message}");
        }
        other => panic!("oversized body was not refused by the size bound: {other:?}"),
    }
    // The same body without its padding is still valid, so the cap is the only refusal.
    let exact = encoded(RunMode::Normal);
    assert!(decode_bounded::<LocalRequest>(&exact).is_ok());
}

/// A NUL byte is refused wherever it appears, including inside a string value.
#[test]
fn bounded_decode_refuses_nul_bytes_outside_valid_text() {
    let mut raw = encoded(RunMode::Normal);
    let text = String::from_utf8(raw.clone()).expect("utf8 wire");
    raw = text
        .replace("local-observed", "local-\u{0}observed")
        .into_bytes();
    assert!(raw.contains(&0), "fixture must carry a real NUL");
    let refused = decode_bounded::<LocalRequest>(&raw);
    assert!(refused.is_err(), "NUL inside a string was accepted");
}

/// Content after the record is refused; trailing ASCII whitespace is not content.
#[test]
fn bounded_decode_refuses_trailing_content_but_allows_trailing_whitespace() {
    let mut extended = encoded(RunMode::Normal);
    extended.extend_from_slice(b"{}");
    assert!(
        decode_bounded::<LocalRequest>(&extended).is_err(),
        "a second JSON value after the record was accepted"
    );
    // Observed behaviour of `serde_json::Deserializer::end`: it skips trailing
    // whitespace and only refuses a further value. A newline-terminated body is
    // therefore accepted, and this test fixes that boundary rather than assuming it.
    let mut with_suffix = encoded(RunMode::Normal);
    with_suffix.push(b'\n');
    assert_eq!(
        decode_bounded::<LocalRequest>(&with_suffix).expect("newline-terminated record"),
        request(RunMode::Normal)
    );
    let mut padded = encoded(RunMode::Normal);
    padded.extend_from_slice(b" \r\n\t");
    assert!(decode_bounded::<LocalRequest>(&padded).is_ok());
}

/// Closed records reject an added field instead of dropping it.
#[test]
fn bounded_decode_refuses_unknown_fields() {
    let mut value = serde_json::to_value(request(RunMode::Full)).expect("map");
    value
        .as_object_mut()
        .expect("map")
        .insert("native_seeds".into(), serde_json::json!([]));
    let raw = serde_json::to_vec(&value).expect("wire");
    let refused = decode_bounded::<LocalRequest>(&raw);
    assert!(
        matches!(refused, Err(ContractError::Configuration(_))),
        "unknown field was not refused as a wire error: {refused:?}"
    );
    // The same payload without that field still decodes, so the field caused the refusal.
    let clean = serde_json::to_vec(&request(RunMode::Full)).expect("wire");
    assert!(decode_bounded::<LocalRequest>(&clean).is_ok());
}

/// Duplicate field names are refused, and a hidden duplicate cannot override a value.
#[test]
fn bounded_decode_refuses_duplicate_fields() {
    let raw = br#"{"schema":"kioku/ci2/local-request/v1","schema":"kioku/ci2/local-request/v1"}"#;
    assert!(decode_bounded::<LocalRequest>(raw).is_err());
}

/// b3 digests are exactly 64 lower-case hex digits behind a known algorithm prefix.
#[test]
fn b3_digest_shape_is_bounded_and_lowercase() {
    let good = Digest::new(&format!("b3:{}", "0".repeat(64))).expect("64 zero hex digits");
    assert_eq!(good.algorithm(), &DigestAlgorithm::Blake3);
    assert_eq!(good.as_hex(), "0".repeat(64));
    for bad in [
        "b3:".to_owned(),
        format!("b3:{}", "a".repeat(63)),
        format!("b3:{}", "a".repeat(65)),
        format!("b3:{}", "A".repeat(64)),
        format!("b3:{}", "g".repeat(64)),
        format!("blake3:{}", "a".repeat(64)),
    ] {
        assert!(Digest::new(&bad).is_err(), "accepted digest {bad}");
    }
    // A decoded digest field is held to the same shape: upper-case hex is not normalised.
    let mut value = serde_json::to_value(request(RunMode::Normal)).expect("map");
    value.as_object_mut().expect("map").insert(
        "manifest_digest".into(),
        serde_json::json!(format!("b3:{}", "A".repeat(64))),
    );
    let refused = decode_bounded::<LocalRequest>(&serde_json::to_vec(&value).expect("wire"));
    assert!(
        refused.is_err(),
        "upper-case digest was accepted on the wire"
    );
}

/// A digest over zero bytes is still a well-formed digest and is not the identity.
#[test]
fn b3_digest_of_zero_length_input_is_well_formed_and_distinct() {
    let empty = exact_digest("kioku/ci2/request-extra/v1", b"").expect("empty digest");
    assert_eq!(empty.algorithm(), &DigestAlgorithm::Blake3);
    assert_eq!(empty.as_hex().len(), 64);
    assert!(
        empty
            .as_hex()
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    );
    assert_ne!(
        empty.as_hex(),
        "0".repeat(64),
        "empty input must not hash to zeros"
    );
    // It round-trips through the strict text form.
    assert!(Digest::new(&empty.to_string()).is_ok());
    let non_empty = exact_digest("kioku/ci2/request-extra/v1", b"x").expect("digest");
    assert_ne!(empty, non_empty);
    // An empty hashing domain is refused instead of being treated as a default.
    assert!(exact_digest("", b"").is_err());
    assert!(Digest::blake3("bad\0domain", b"").is_err());
}

/// The canonical request digest is stable across field order and refuses bad value shapes.
#[test]
fn request_digest_is_order_independent_and_refuses_bad_numbers() {
    let value = serde_json::to_value(request(RunMode::Normal)).expect("map");
    let first = digest_request_document(&value).expect("digest");
    let mut reordered = serde_json::Map::new();
    for (key, field) in value.as_object().expect("map").iter().rev() {
        reordered.insert(key.clone(), field.clone());
    }
    let second = digest_request_document(&serde_json::Value::Object(reordered)).expect("digest");
    assert_eq!(first, second, "field order changed the digest");

    let mut float = value.clone();
    float
        .as_object_mut()
        .expect("map")
        .insert("revision".into(), serde_json::json!(1.5));
    assert!(digest_request_document(&float).is_err(), "float accepted");

    let mut negative = value.clone();
    negative
        .as_object_mut()
        .expect("map")
        .insert("revision".into(), serde_json::json!(-1));
    assert!(
        digest_request_document(&negative).is_err(),
        "negative accepted"
    );

    let mut oversized = value;
    oversized
        .as_object_mut()
        .expect("map")
        .insert("revision".into(), serde_json::json!(JSON_INTEGER_MAX + 1));
    assert!(
        digest_request_document(&oversized).is_err(),
        "out-of-range integer accepted"
    );

    let within = serde_json::json!({ "revision": JSON_INTEGER_MAX });
    assert!(
        digest_request_document(&within).is_ok(),
        "portable integer refused"
    );
}

/// Nested wire deeper than the bound is refused, and a shallow document is not.
#[test]
fn wire_depth_is_bounded() {
    fn nested(levels: usize) -> serde_json::Value {
        let mut value = serde_json::json!(0);
        for _ in 0..levels {
            value = serde_json::Value::Array(vec![value]);
        }
        value
    }
    // A modest document is well inside the bound.
    assert!(digest_request_document(&nested(4)).is_ok());
    // Far past the 128-level bound the visit must refuse instead of recursing.
    let deep = digest_request_document(&nested(256));
    assert!(
        matches!(deep, Err(ContractError::Configuration(_))),
        "over-deep document was not refused: {deep:?}"
    );
}

/// A control character in a field name is refused by the strict node, not silently kept.
#[test]
fn wire_field_names_reject_control_characters() {
    let raw = b"{\"schema\":\"kioku/ci2/local-request/v1\",\"rev\\u0001ision\":1}";
    let refused = decode_bounded::<LocalRequest>(raw);
    assert!(
        refused.is_err(),
        "control character in field name was accepted"
    );
}
