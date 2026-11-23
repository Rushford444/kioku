//! Mechanical boundary checks for the `RelPath` primitive.
//!
//! These tests use only the public constructors and serde round-trips of
//! `kioku::ci2` value types. They start no daemon, no runner and no native
//! session, and they claim nothing about traced reuse or execution.

use kioku::ci2::*;

/// A portable relative path is exact text, never a sanitised one: control
/// characters are not a spelling this contract can carry to a filesystem, a
/// report line or a JSON field.
#[test]
fn relative_path_refuses_control_characters() {
    for rejected in [
        "work/\u{1}name",
        "line\nbreak",
        "tab\there",
        "carriage\rreturn",
        "work/\u{7f}",
        "work/\u{9b}name",
    ] {
        assert!(
            RelPath::new(rejected).is_err(),
            "relative path accepted a control character: {rejected:?}"
        );
    }
    for accepted in [
        "dir/sub dir",
        "dir/..hidden",
        "dir/a.b-c_d",
        "profils/profil-observé.json",
    ] {
        let path = RelPath::new(accepted)
            .unwrap_or_else(|error| panic!("relative path refused {accepted:?}: {error}"));
        assert_eq!(
            path.as_str(),
            accepted,
            "relative path spelling must stay byte exact: {accepted:?}"
        );
        assert!(path.validate().is_ok(), "round trip refused {accepted:?}");
    }
}
