#![cfg(unix)]
//! Focused contract tests for `prepare_one` artifact-directory preparation.
//! Each test builds its own private temporary fixture and asserts path and
//! permission regularity. No product source, service, network or secret path
//! is read or written.

use kioku::ci2::domain::artifact::prepare_one;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
};

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

/// The intermediate directory the helper creates must be private too: the run
/// directory is 0700 but its freshly created parent must not stay world-traversable.
#[test]
fn created_intermediate_directory_is_private() {
    let fixture = tempfile::tempdir().unwrap();
    let parent = fixture.path().join("fresh");
    let run = parent.join("run");

    prepare_one(&run).unwrap();

    assert!(fs::symlink_metadata(&run).unwrap().is_dir());
    assert_eq!(mode(&run), 0o700, "run directory mode");
    assert_eq!(mode(&parent), 0o700, "created parent mode");
}

/// A legitimate absent output directory is still created as a private leaf.
#[test]
fn absent_artifact_directory_is_created_private() {
    let fixture = tempfile::tempdir().unwrap();
    let run = fixture.path().join("direct");

    prepare_one(&run).unwrap();

    let metadata = fs::symlink_metadata(&run).unwrap();
    assert!(metadata.is_dir());
    assert_eq!(mode(&run), 0o700);
}

/// Symlinked parents and pre-existing directories fail closed without touching
/// the link target or the existing path.
#[test]
fn symlinked_parent_and_existing_path_fail_closed() {
    let fixture = tempfile::tempdir().unwrap();
    let target = fixture.path().join("target");
    fs::create_dir(&target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
    let link = fixture.path().join("link");
    symlink(&target, &link).unwrap();

    assert!(prepare_one(&link.join("run")).is_err());
    assert!(!target.join("run").exists());

    let existing = fixture.path().join("existing");
    fs::create_dir(&existing).unwrap();
    assert!(prepare_one(&existing).is_err());
    assert!(fs::symlink_metadata(&existing).unwrap().is_dir());
}
