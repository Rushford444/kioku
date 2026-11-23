//! Canonical protected-reference syntax at the local wire boundary.
//!
//! These checks stay inside the pure domain layer: they build values from the
//! public wire type, decode them from JSON, and call `Validate`. They start no
//! daemon, resolve no reference, and make no claim about traced reuse.

use kioku::ci2::*;

fn id(value: &str) -> Id {
    Id::new(value).unwrap_or_else(|error| panic!("fixture ID {value} rejected: {error}"))
}

fn root(protected_refs: Vec<String>) -> RegisteredRoot {
    RegisteredRoot {
        id: id("kioku-local"),
        project_id: id("kioku"),
        canonical_path: "/srv/kioku".into(),
        allowed_principals: vec![id("local-operator")],
        protected_refs,
    }
}

/// A protected reference is compared as text against caller-supplied names, so
/// a malformed identifier is refused here instead of being carried forward as
/// if it named a real full reference. Every case below is accepted by the
/// former `starts_with("refs/")` shape alone.
#[test]
fn protected_refs_reject_malformed_full_refs() {
    let malformed = [
        "refs/",
        "refs//heads/main",
        "refs/heads//main",
        "refs/heads/main/",
        "refs/heads/../main",
        "refs/heads/..",
        "refs/heads/main.",
        "refs/heads/main..",
        "refs/heads/main.lock",
        "refs/heads/.hidden",
        "refs/heads/./main",
        "refs/heads/ma in",
        "refs/heads/ma\tin",
        "refs/heads/ma\u{7f}in",
        "refs/heads/ma@{in",
        "refs/heads/ma~in",
        "refs/heads/ma^in",
        "refs/heads/ma:in",
        "refs/heads/ma?in",
        "refs/heads/ma*in",
        "refs/heads/ma[in",
        "refs/heads/ma\\in",
        "refs/heads/ma\u{0}in",
    ];
    for reference in malformed {
        let wire = serde_json::json!({
            "id": "kioku-local",
            "project_id": "kioku",
            "canonical_path": "/srv/kioku",
            "allowed_principals": ["local-operator"],
            "protected_refs": [reference],
        });
        let decoded: RegisteredRoot =
            serde_json::from_value(wire).expect("fixture root decodes from the wire");
        assert!(
            decoded.validate().is_err(),
            "root accepted malformed protected reference {reference:?}"
        );
    }
}

/// Ordinary branch names and the lowercase full object identifiers already
/// handled by this boundary keep validating unchanged.
#[test]
fn protected_refs_accept_canonical_names_and_hashes() {
    let accepted = [
        "refs/heads/main".to_string(),
        "refs/tags/v0.3.1".to_string(),
        "refs/heads/release/0.3".to_string(),
        "a".repeat(40),
        "0123456789abcdef".repeat(4),
    ];
    assert!(
        root(accepted.to_vec()).validate().is_ok(),
        "canonical protected references rejected: {accepted:?}"
    );
    let rejected = [
        String::new(),
        "heads/main".to_string(),
        "/refs/heads/main".to_string(),
        "a".repeat(39),
        "a".repeat(41),
    ];
    for reference in rejected {
        assert!(
            root(vec![reference.clone()]).validate().is_err(),
            "root accepted out-of-contract protected reference {reference:?}"
        );
    }
}
