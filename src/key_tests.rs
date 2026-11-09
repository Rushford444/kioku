//! Tests of the canonical snapshot.

use super::*;

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::SystemTime;

use tempfile::TempDir;

/// Creates a repository holding one committed file.
fn git_repository() -> TempDir {
    let directory = TempDir::new().expect("a temporary directory");
    git(directory.path(), &["init", "-q", "-b", "main"]);
    write(directory.path(), "src/main.rs", "fn main() {}\n");
    commit(directory.path());
    directory
}

/// Writes a file, creating its directories.
fn write(root: &Path, path: &str, content: &str) {
    let full = root.join(path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).expect("the directories of the file");
    }
    fs::write(full, content).expect("the file is written");
}

/// Runs `git` in a directory.
fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(Stdio::null())
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?} failed");
}

/// Commits everything the repository holds.
fn commit(cwd: &Path) {
    git(cwd, &["add", "-A"]);
    git(
        cwd,
        &[
            "-c",
            "user.email=kioku@example.com",
            "-c",
            "user.name=kioku",
            "commit",
            "-q",
            "-m",
            "state",
        ],
    );
}

/// Capture options of a plain run.
fn options(cwd: &Path) -> CaptureOptions {
    CaptureOptions {
        cwd: cwd.to_path_buf(),
        argv: vec!["cargo".to_string(), "test".to_string()],
        mode: Mode::Bytes,
        inputs: Vec::new(),
        env: Vec::new(),
    }
}

/// Takes a snapshot of a repository.
#[cfg(not(windows))]
fn snapshot(cwd: &Path) -> Snapshot {
    Snapshot::capture(&options(cwd)).expect("the snapshot is taken")
}

#[cfg(not(windows))]
#[test]
fn two_identical_repositories_have_the_same_key() {
    let first = git_repository();
    let second = git_repository();

    let first_snapshot = snapshot(first.path());
    let second_snapshot = snapshot(second.path());

    assert_eq!(first_snapshot.key, second_snapshot.key);
    assert_eq!(first_snapshot.cwd, second_snapshot.cwd);
    assert_eq!(first_snapshot.inputs, second_snapshot.inputs);
}

#[cfg(not(windows))]
#[test]
fn a_changed_file_changes_the_key() {
    let directory = git_repository();
    let before = snapshot(directory.path());

    write(directory.path(), "src/main.rs", "fn main() { work(); }\n");
    let after = snapshot(directory.path());

    assert_ne!(before.key, after.key);
}

#[cfg(windows)]
#[test]
fn windows_command_identity_refuses_unverified_executable_resolution() {
    let directory = TempDir::new().expect("temporary directory");
    let mut capture = options(directory.path());
    capture.argv = vec!["cmd".to_string()];
    assert!(matches!(
        command_identity(&capture),
        Err(Error::ToolProbe { .. })
    ));
}

#[cfg(unix)]
#[test]
fn command_identity_distinguishes_relative_paths_from_path_lookup() {
    use std::os::unix::fs::PermissionsExt;
    let directory = TempDir::new().expect("temporary directory");
    let local = directory.path().join("sh");
    fs::write(&local, b"distinct local executable").expect("local executable");
    fs::set_permissions(&local, fs::Permissions::from_mode(0o755)).expect("permissions");
    let mut capture = options(directory.path());
    capture.argv = vec!["./sh".to_string()];
    let relative = command_identity(&capture).expect("relative executable");
    let metadata = fs::metadata(&local).expect("metadata");
    assert_eq!(
        relative,
        format!(
            "{}:{}",
            mode_of(&metadata),
            raw_digest(b"distinct local executable")
        )
    );
    capture.argv = vec!["sh".to_string()];
    let from_path = command_identity(&capture).expect("PATH executable");
    assert_ne!(
        from_path, relative,
        "bare names must be resolved through PATH"
    );
}

#[cfg(unix)]
#[test]
fn changed_command_executable_changes_the_key_even_with_same_version() {
    use std::os::unix::fs::PermissionsExt;
    let directory = git_repository();
    let binary_dir = TempDir::new().expect("temporary tool directory");
    let binary = binary_dir.path().join("tool");
    fs::write(&binary, "#!/bin/sh\nexit 0\n").expect("tool");
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).expect("permissions");
    let mut capture = options(directory.path());
    capture.argv = vec![binary.to_string_lossy().into_owned()];
    let first = Snapshot::capture(&capture).expect("snapshot");
    fs::write(&binary, "#!/bin/sh\nexit 1\n").expect("changed tool");
    let second = Snapshot::capture(&capture).expect("snapshot");
    assert_ne!(first.key, second.key);
}

#[cfg(not(windows))]
#[test]
fn a_changed_command_line_changes_the_key() {
    let directory = git_repository();
    let mut capture = options(directory.path());
    let before = Snapshot::capture(&capture).expect("the snapshot is taken");

    capture.argv.push("--release".to_string());
    let after = Snapshot::capture(&capture).expect("the snapshot is taken");

    assert_ne!(before.key, after.key);
}

#[test]
fn a_missing_environment_variable_is_not_an_empty_one() {
    let directory = git_repository();
    let capture = options(directory.path());
    let mut missing = BTreeMap::new();
    missing.insert("KIOKU_TEST".to_string(), None);
    let mut empty = BTreeMap::new();
    empty.insert("KIOKU_TEST".to_string(), Some(String::new()));

    let missing_key = compute_key(
        &capture,
        ".",
        &missing,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .expect("the key is computed");
    let empty_key = compute_key(
        &capture,
        ".",
        &empty,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .expect("the key is computed");

    assert_ne!(missing_key, empty_key);
}

#[cfg(not(windows))]
#[test]
fn the_lockfiles_are_captured_even_when_the_inputs_are_restricted() {
    let directory = git_repository();
    write(directory.path(), "Cargo.lock", "[[package]]\n");
    commit(directory.path());
    let mut capture = options(directory.path());
    capture.inputs = vec!["src/**".to_string()];

    let captured = Snapshot::capture(&capture).expect("the snapshot is taken");

    assert!(captured.lockfiles.contains_key("Cargo.lock"));
    assert!(captured.inputs.contains_key("src/main.rs"));
    assert!(!captured.inputs.contains_key("Cargo.lock"));
}

#[cfg(not(windows))]
#[test]
fn an_input_glob_restricts_the_captured_files() {
    let directory = git_repository();
    write(directory.path(), "top.rs", "fn top() {}\n");
    let mut capture = options(directory.path());
    capture.inputs = vec!["*.rs".to_string()];

    let captured = Snapshot::capture(&capture).expect("the snapshot is taken");

    assert!(captured.inputs.contains_key("top.rs"));
    assert!(!captured.inputs.contains_key("src/main.rs"));
}

#[cfg(not(windows))]
#[test]
fn an_untracked_file_is_captured_and_an_ignored_file_is_not() {
    let directory = git_repository();
    write(directory.path(), ".gitignore", "ignored.txt\n");
    commit(directory.path());
    write(directory.path(), "ignored.txt", "ignored\n");
    write(directory.path(), "extra.txt", "extra\n");

    let captured = snapshot(directory.path());

    assert!(captured.inputs.contains_key("extra.txt"));
    assert!(!captured.inputs.contains_key("ignored.txt"));
}

#[cfg(not(windows))]
#[test]
fn a_deleted_tracked_file_invalidates_the_key_without_failing() {
    let directory = git_repository();
    let before = snapshot(directory.path());

    fs::remove_file(directory.path().join("src/main.rs")).expect("the file is removed");
    let after = snapshot(directory.path());

    assert_ne!(before.key, after.key);
    assert!(after.inputs.contains_key("src/main.rs"));
}

#[cfg(unix)]
#[test]
fn the_mode_of_a_file_is_part_of_the_key() {
    use std::os::unix::fs::PermissionsExt;

    let directory = git_repository();
    let before = snapshot(directory.path());

    fs::set_permissions(
        directory.path().join("src/main.rs"),
        fs::Permissions::from_mode(0o755),
    )
    .expect("the mode is changed");
    let after = snapshot(directory.path());

    assert_ne!(before.key, after.key);
}

#[cfg(unix)]
#[test]
fn the_target_of_a_symlink_is_part_of_the_key() {
    use std::os::unix::fs::symlink;

    let directory = git_repository();
    write(directory.path(), "target.txt", "one\n");
    symlink("target.txt", directory.path().join("link")).expect("the symlink is created");
    let before = snapshot(directory.path());

    write(directory.path(), "target.txt", "two\n");
    let after = snapshot(directory.path());

    assert_ne!(before.key, after.key);
}

#[cfg(not(windows))]
#[test]
fn the_guard_moves_when_a_file_is_touched_even_though_the_key_does_not() {
    let directory = git_repository();
    let before = snapshot(directory.path());

    std::thread::sleep(std::time::Duration::from_millis(20));
    write(directory.path(), "src/main.rs", "fn main() {}\n");
    let after = snapshot(directory.path());

    assert_eq!(before.key, after.key);
    assert_ne!(before.guard, after.guard);
}

#[cfg(unix)]
#[test]
fn an_unreadable_file_fails_the_capture() {
    use std::os::unix::fs::PermissionsExt;

    let directory = git_repository();
    let file = directory.path().join("src/main.rs");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).expect("the mode is changed");
    if fs::read(&file).is_ok() {
        // The tests run as a user that can read anything: nothing to prove.
        return;
    }

    let error = Snapshot::capture(&options(directory.path())).expect_err("the capture fails");

    assert!(matches!(error, Error::UnreadableInput { .. }));
}

#[cfg(unix)]
#[test]
fn a_path_that_is_not_utf8_fails_the_capture() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let directory = git_repository();
    let name = OsString::from_vec(vec![b'b', 0xff]);
    if fs::write(directory.path().join(&name), "x").is_err() {
        // The filesystem refuses the name: nothing to prove.
        return;
    }

    let error = Snapshot::capture(&options(directory.path())).expect_err("the capture fails");

    assert!(matches!(error, Error::NonUtf8Path { .. }));
}

#[cfg(not(windows))]
#[test]
fn the_working_directory_is_relative_to_the_root_of_the_repository() {
    let directory = git_repository();

    let root = snapshot(directory.path());
    assert_eq!(root.cwd, ".");

    let nested = snapshot(&directory.path().join("src"));
    assert_eq!(nested.cwd, "src");
}

#[test]
fn the_repository_holds_the_root_and_the_git_directory() {
    let directory = git_repository();

    let (root, git_dir) = repository(directory.path()).expect("the repository is found");

    assert!(root.is_absolute());
    assert!(git_dir.is_absolute());
    assert!(git_dir.ends_with(".git"));
    assert!(root.ends_with(directory.path().file_name().expect("a name")));
}

#[test]
fn invalid_glob_is_rejected_by_matcher() {
    assert!(matches!(
        InputMatcher::new(&["[".to_string()]),
        Err(Error::InvalidGlob { .. })
    ));
}

#[test]
fn invalid_glob_is_checked_before_repository_access() {
    let directory = TempDir::new().expect("temporary directory");
    let mut capture = options(&directory.path().join("nonexistent"));
    capture.inputs = vec!["[".to_string()];
    let error = Snapshot::capture(&capture).expect_err("invalid glob");
    assert!(matches!(error, Error::InvalidGlob { .. }), "{error:?}");
}

#[test]
fn an_invalid_glob_fails_the_capture() {
    let directory = git_repository();
    let mut capture = options(directory.path());
    capture.inputs = vec!["[".to_string()];

    let error = Snapshot::capture(&capture).expect_err("the capture fails");

    assert!(
        matches!(error, Error::InvalidGlob { .. }),
        "unexpected error: {error:?}"
    );
}

#[cfg(not(windows))]
#[test]
fn the_ast_mode_ignores_the_whitespace_and_the_ordinary_comments() {
    let directory = git_repository();
    write(directory.path(), "app.ts", "export const a = 1;\n");
    let mut capture = options(directory.path());
    capture.mode = Mode::Ast;
    let before = Snapshot::capture(&capture).expect("the snapshot is taken");

    write(
        directory.path(),
        "app.ts",
        "// an ordinary comment\nexport const a  =  1;\n\n",
    );
    let after = Snapshot::capture(&capture).expect("the snapshot is taken");

    assert_eq!(before.key, after.key);
    assert_eq!(before.inputs, after.inputs);
}

#[cfg(not(windows))]
#[test]
fn the_bytes_mode_sees_the_whitespace() {
    let directory = git_repository();
    write(directory.path(), "app.ts", "export const a = 1;\n");
    let mut capture = options(directory.path());
    capture.mode = Mode::Bytes;
    let before = Snapshot::capture(&capture).expect("the snapshot is taken");

    write(
        directory.path(),
        "app.ts",
        "// an ordinary comment\nexport const a  =  1;\n\n",
    );
    let after = Snapshot::capture(&capture).expect("the snapshot is taken");

    assert_ne!(before.key, after.key);
}

#[cfg(not(windows))]
#[test]
fn the_default_variables_are_captured_even_when_they_are_not_set() {
    let directory = git_repository();

    let captured = snapshot(directory.path());

    for name in DEFAULT_ENV_VARS {
        assert!(captured.env.contains_key(*name));
    }
    // Cargo sets CARGO_PKG_NAME for the binaries it runs.
    if std::env::var_os("CARGO_PKG_NAME").is_some() {
        assert!(captured.env.contains_key("CARGO_PKG_NAME"));
    }
}

#[cfg(not(windows))]
#[test]
fn a_declared_variable_that_is_not_set_is_captured_as_absent() {
    let directory = git_repository();
    let mut capture = options(directory.path());
    capture.env = vec!["KIOKU_VARIABLE_THAT_IS_NOT_SET".to_string()];

    let captured = Snapshot::capture(&capture).expect("the snapshot is taken");

    assert_eq!(
        captured.env.get("KIOKU_VARIABLE_THAT_IS_NOT_SET"),
        Some(&None)
    );
}

#[cfg(not(windows))]
#[test]
fn the_exclude_file_of_the_repository_is_honoured() {
    let directory = git_repository();
    fs::write(
        directory.path().join(".git").join("info").join("exclude"),
        "excluded.txt\n",
    )
    .expect("the exclude file is written");
    write(directory.path(), "excluded.txt", "excluded\n");
    write(directory.path(), "kept.txt", "kept\n");

    let captured = snapshot(directory.path());

    assert!(!captured.inputs.contains_key("excluded.txt"));
    assert!(captured.inputs.contains_key("kept.txt"));
}

#[test]
fn a_linked_worktree_has_its_own_git_directory() {
    let directory = git_repository();
    let parent = TempDir::new().expect("a temporary directory");
    let linked = parent.path().join("linked");
    git(
        directory.path(),
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "linked",
            linked.to_str().expect("a path"),
        ],
    );

    let (root, git_dir) = repository(&linked).expect("the repository is found");

    assert!(root.ends_with("linked"));
    assert!(git_dir.to_string_lossy().contains("worktrees"));
}

#[test]
fn a_store_in_the_work_tree_refuses_to_cache() {
    let directory = git_repository();
    let root = directory.path();
    let git_dir = root.join(".git");

    // A store in the work tree would hide the files that live there, tracked
    // or not: the cache is off.
    assert!(store_inside_work_tree(&root.join("cache"), root, &git_dir));
    assert!(store_inside_work_tree(
        &root.join("nested").join("cache"),
        root,
        &git_dir
    ));
    // The default store lives in the git directory, which git never lists.
    assert!(!store_inside_work_tree(
        &git_dir.join("kioku"),
        root,
        &git_dir
    ));
    // A store outside the work tree cannot hide any file of the repository.
    assert!(!store_inside_work_tree(
        Path::new("/tmp/kioku-store"),
        root,
        &git_dir
    ));
}

#[cfg(not(windows))]
#[test]
fn the_ast_mode_sees_a_token_change() {
    let directory = git_repository();
    write(directory.path(), "app.ts", "export const a = 1;\n");
    let mut capture = options(directory.path());
    capture.mode = Mode::Ast;
    let before = Snapshot::capture(&capture).expect("the snapshot is taken");

    write(directory.path(), "app.ts", "export const a = 2;\n");
    let after = Snapshot::capture(&capture).expect("the snapshot is taken");

    assert_ne!(before.key, after.key);
}

#[cfg(not(windows))]
#[test]
fn the_ast_mode_sees_a_meaningful_comment() {
    let directory = git_repository();
    write(directory.path(), "app.ts", "export const a = 1;\n");
    let mut capture = options(directory.path());
    capture.mode = Mode::Ast;
    let before = Snapshot::capture(&capture).expect("the snapshot is taken");

    write(
        directory.path(),
        "app.ts",
        "// @ts-expect-error\nexport const a = 1;\n",
    );
    let after = Snapshot::capture(&capture).expect("the snapshot is taken");

    assert_ne!(before.key, after.key);
}

#[cfg(not(windows))]
#[test]
fn the_guard_sees_a_content_change_that_keeps_the_size_and_the_timestamp() {
    let directory = git_repository();
    let file = directory.path().join("src/main.rs");
    let before = snapshot(directory.path());
    let timestamp = fs::metadata(&file)
        .and_then(|meta| meta.modified())
        .expect("a timestamp");

    // The same number of bytes, and the timestamp of the first version is
    // put back: only the content changed, the metadata did not.
    write(directory.path(), "src/main.rs", "fn done() {}\n");
    restore_timestamp(&file, timestamp);
    let after = snapshot(directory.path());

    assert_eq!(
        before.inputs["src/main.rs"].mode,
        after.inputs["src/main.rs"].mode
    );
    assert_ne!(before.guard, after.guard);
    assert_ne!(before.key, after.key);
}

/// Gives a file the timestamp it had.
#[cfg(not(windows))]
fn restore_timestamp(path: &Path, timestamp: std::time::SystemTime) {
    let file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("the file is opened");
    file.set_times(std::fs::FileTimes::new().set_modified(timestamp))
        .expect("the timestamp is set");
}

#[cfg(not(windows))]
#[test]
fn the_two_modes_have_two_keys() {
    let directory = git_repository();
    write(directory.path(), "app.ts", "export const a = 1;\n");
    let mut ast = options(directory.path());
    ast.mode = Mode::Ast;
    let mut bytes = options(directory.path());
    bytes.mode = Mode::Bytes;

    let ast_snapshot = Snapshot::capture(&ast).expect("the snapshot is taken");
    let bytes_snapshot = Snapshot::capture(&bytes).expect("the snapshot is taken");

    assert_ne!(ast_snapshot.key, bytes_snapshot.key);
}

#[test]
fn a_file_that_changed_between_two_reads_is_detected() {
    let epoch = SystemTime::UNIX_EPOCH;
    let later = epoch + std::time::Duration::from_secs(1);

    assert!(moved((10, Some(epoch)), (11, Some(epoch))));
    assert!(moved((10, Some(epoch)), (10, Some(later))));
    assert!(!moved((10, Some(epoch)), (10, Some(epoch))));
}

#[cfg(not(windows))]
#[test]
fn the_states_of_a_file_have_different_digests() {
    let directory = git_repository();
    write(directory.path(), "gone.txt", "gone\n");
    commit(directory.path());
    fs::remove_file(directory.path().join("gone.txt")).expect("the file is removed");
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink("target.txt", directory.path().join("link")).expect("the symlink is created");
    }

    let captured = snapshot(directory.path());

    let missing = captured.inputs["gone.txt"].digest.clone();
    #[cfg(unix)]
    {
        let link = captured.inputs["link"].digest.clone();
        assert_ne!(missing, link);
    }
    assert_ne!(missing, captured.inputs["src/main.rs"].digest);
}

#[cfg(not(windows))]
#[test]
fn the_mode_of_a_captured_file_is_its_kind_and_its_permissions() {
    let directory = git_repository();
    write(directory.path(), "gone.txt", "gone\n");
    commit(directory.path());
    fs::remove_file(directory.path().join("gone.txt")).expect("the file is removed");

    let captured = snapshot(directory.path());

    let file = directory.path().join("src/main.rs");
    let permissions = fs::metadata(&file).expect("the metadata").permissions();
    #[cfg(unix)]
    let permissions = {
        use std::os::unix::fs::PermissionsExt;
        permissions.mode() & 0o777
    };
    #[cfg(not(unix))]
    let permissions = if permissions.readonly() { 0o444 } else { 0o644 };
    assert_eq!(captured.inputs["src/main.rs"].mode, permissions);
    // The kind of the file is in the high bits: file, directory, symlink,
    // missing.
    assert_eq!(captured.inputs["src/main.rs"].mode >> 12, 0);
    assert_eq!(captured.inputs["gone.txt"].mode >> 12, 3);
}

#[cfg(unix)]
#[test]
fn a_dangling_symlink_is_captured() {
    use std::os::unix::fs::symlink;

    let directory = git_repository();
    symlink("nowhere.txt", directory.path().join("link")).expect("the symlink is created");

    let captured = snapshot(directory.path());

    assert!(captured.inputs.contains_key("link"));
    assert_eq!(captured.inputs["link"].mode >> 12, 2);
}

#[cfg(unix)]
#[test]
fn a_symlink_to_a_directory_fails_the_capture() {
    use std::os::unix::fs::symlink;
    let directory = git_repository();
    symlink("src", directory.path().join("link")).expect("the symlink is created");
    assert!(Snapshot::capture(&options(directory.path())).is_err());
}

#[cfg(unix)]
#[test]
fn a_symlink_loop_fails_the_capture() {
    use std::os::unix::fs::symlink;

    let directory = git_repository();
    symlink("loop-b", directory.path().join("loop-a")).expect("the symlink is created");
    symlink("loop-a", directory.path().join("loop-b")).expect("the symlink is created");

    let error = Snapshot::capture(&options(directory.path())).expect_err("the capture fails");

    assert!(matches!(error, Error::UnreadableInput { .. }));
}

#[cfg(unix)]
#[test]
fn a_file_in_an_unreadable_directory_fails_the_capture() {
    use std::os::unix::fs::PermissionsExt;

    let directory = git_repository();
    write(directory.path(), "sub/file.txt", "x\n");
    commit(directory.path());
    fs::set_permissions(
        directory.path().join("sub"),
        fs::Permissions::from_mode(0o000),
    )
    .expect("the mode is changed");
    if fs::read_dir(directory.path().join("sub")).is_ok() {
        // The tests run as a user that can read anything: nothing to prove.
        return;
    }

    let error = Snapshot::capture(&options(directory.path())).expect_err("the capture fails");

    assert!(matches!(error, Error::UnreadableInput { .. }));
}

#[test]
fn a_dirty_submodule_disables_capture() {
    let parent = TempDir::new().expect("a temporary directory");
    let inner = parent.path().join("inner");
    let outer = parent.path().join("outer");
    fs::create_dir_all(&inner).expect("the directory is created");
    fs::create_dir_all(&outer).expect("the directory is created");
    git(&inner, &["init", "-q", "-b", "main"]);
    write(&inner, "inner.txt", "inner\n");
    commit(&inner);
    git(&outer, &["init", "-q", "-b", "main"]);
    git(
        &outer,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            inner.to_str().expect("a path"),
            "sub",
        ],
    );
    commit(&outer);

    write(&outer, "sub/inner.txt", "dirty\n");
    assert!(Snapshot::capture(&options(&outer)).is_err());
}

#[cfg(not(windows))]
#[test]
fn the_two_modes_have_two_keys_even_without_any_file() {
    let directory = TempDir::new().expect("a temporary directory");
    git(directory.path(), &["init", "-q", "-b", "main"]);
    let mut ast = options(directory.path());
    ast.mode = Mode::Ast;
    let mut bytes = options(directory.path());
    bytes.mode = Mode::Bytes;

    let ast_snapshot = Snapshot::capture(&ast).expect("the snapshot is taken");
    let bytes_snapshot = Snapshot::capture(&bytes).expect("the snapshot is taken");

    assert!(ast_snapshot.inputs.is_empty());
    assert_eq!(ast_snapshot.inputs, bytes_snapshot.inputs);
    assert_ne!(ast_snapshot.key, bytes_snapshot.key);
}

#[cfg(not(windows))]
#[test]
fn the_tools_of_the_lockfiles_are_captured() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    const CHILD: &str = "KIOKU_LOCKFILE_TOOLS_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let directory = TempDir::new().expect("temporary PATH");
        let git = crate::tools::resolve("git").expect("git required for repository fixtures");
        symlink(git, directory.path().join("git")).expect("git link");
        for name in ["cargo", "rustc", "node", "npm", "go"] {
            let path = directory.path().join(name);
            fs::write(&path, "#!/bin/sh\necho fixture-version\n").expect("fixture tool");
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("executable");
        }
        let output = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "key::tests::the_tools_of_the_lockfiles_are_captured",
            ])
            .env("PATH", directory.path())
            .env(CHILD, "1")
            .output()
            .expect("child test");
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let directory = git_repository();
    write(directory.path(), "Cargo.lock", "[[package]]\n");
    write(directory.path(), "package-lock.json", "{}\n");
    write(directory.path(), "go.sum", "");
    commit(directory.path());

    let captured = snapshot(directory.path());

    assert!(captured.tools.contains_key("cargo"));
    assert!(captured.tools.contains_key("rustc"));
    assert!(captured.tools.contains_key("node"));
    assert!(captured.tools.contains_key("npm"));
    assert!(captured.tools.contains_key("go"));
}

proptest::proptest! {
    /// Two different command lines cannot be written the same way, so they
    /// cannot have the same key.
    #[test]
    fn two_command_lines_have_two_keys(
        first in proptest::collection::vec(proptest::string::string_regex("[a-z]{0,8}").expect("a valid pattern"), 0..4),
        second in proptest::collection::vec(proptest::string::string_regex("[a-z]{0,8}").expect("a valid pattern"), 0..4),
    ) {
        proptest::prelude::prop_assume!(first != second);
        let mut capture = options(Path::new("/tmp"));
        capture.argv = first.clone();
        let first_key = compute_key(
            &capture,
            ".",
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .expect("the key is computed");
        capture.argv = second.clone();
        let second_key = compute_key(
            &capture,
            ".",
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .expect("the key is computed");
        proptest::prelude::prop_assert_ne!(first_key, second_key);
    }
}
