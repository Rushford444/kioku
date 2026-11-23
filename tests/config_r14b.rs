use kioku::config;

#[test]
fn only_a_missing_config_in_an_existing_root_is_optional() {
    let fixture = tempfile::tempdir().unwrap();
    let argv = vec!["cargo".to_owned(), "test".to_owned()];
    assert!(matches!(config::mode(fixture.path(), &argv), Ok(None)));
    assert!(matches!(
        config::mode(&fixture.path().join("missing-root"), &argv),
        Err(config::Error::Io(_))
    ));
}

#[cfg(unix)]
#[test]
fn dangling_config_symlink_is_an_io_error() {
    let fixture = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink("missing-config", fixture.path().join("kioku.toml")).unwrap();
    let argv = vec!["cargo".to_owned(), "test".to_owned()];
    assert!(matches!(
        config::mode(fixture.path(), &argv),
        Err(config::Error::Io(_))
    ));
}
