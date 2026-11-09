//! Tests of the tool detection.

use super::*;

#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use tempfile::TempDir;

/// Writes an executable script and returns its path.
#[cfg(unix)]
fn script(directory: &Path, name: &str, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let path = directory.join(name);
    fs::write(&path, body).expect("the script is written");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("the mode is changed");
    path
}

#[cfg(unix)]
#[test]
fn go_probe_uses_version_subcommand_and_handles_absence() {
    const CHILD: &str = "KIOKU_GO_PROBE_CHILD";
    if let Some(case) = std::env::var_os(CHILD) {
        let tool = probe(Path::new("."), "go").expect("go probe");
        if case == "present" {
            assert_eq!(tool.version, "go version fake");
        } else {
            assert_eq!(tool.version, ABSENT);
            assert!(tool.path.is_none());
        }
        return;
    }
    let directory = TempDir::new().expect("temporary PATH");
    let go = script(
        directory.path(),
        "go",
        "#!/bin/sh\nif [ \"$1\" = version ]; then echo 'go version fake'; else exit 2; fi\n",
    );
    for case in ["present", "absent"] {
        if case == "absent" {
            fs::remove_file(&go).expect("remove fake go");
        }
        let output = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "tools::tests::go_probe_uses_version_subcommand_and_handles_absence",
            ])
            .env("PATH", directory.path())
            .env(CHILD, case)
            .output()
            .expect("child test");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[test]
fn version_cache_is_shared_between_calls() {
    assert!(std::ptr::eq(cache(), cache()));
}

#[test]
fn every_lockfile_names_its_tools() {
    assert_eq!(tools_for("Cargo.lock"), &["cargo", "rustc"]);
    assert_eq!(tools_for("package-lock.json"), &["node", "npm"]);
    assert_eq!(tools_for("pnpm-lock.yaml"), &["node", "pnpm"]);
    assert_eq!(tools_for("yarn.lock"), &["node", "yarn"]);
    assert_eq!(tools_for("bun.lockb"), &["node", "bun"]);
    assert_eq!(tools_for("poetry.lock"), &["python", "poetry"]);
    assert_eq!(tools_for("uv.lock"), &["python", "uv"]);
    assert_eq!(tools_for("go.sum"), &["go"]);
    assert_eq!(tools_for("unknown.lock"), &[] as &[&str]);
}

#[test]
fn the_first_line_that_holds_something_is_the_version() {
    assert_eq!(
        first_line("\n   \n  tool 1.0  \nsecond line\n"),
        Some("tool 1.0".to_string())
    );
    assert_eq!(first_line("   \n\n"), None);
}

#[test]
fn the_entire_first_version_line_is_preserved() {
    let line = format!("{}-changed", "x".repeat(200));
    assert_eq!(first_line(&line), Some(line));
}

#[cfg(unix)]
#[test]
fn the_version_of_a_tool_is_the_first_line_it_prints() {
    let directory = TempDir::new().expect("a temporary directory");
    let tool = script(
        directory.path(),
        "tool",
        "#!/bin/sh\necho 'tool 1.2.3'\necho 'second line'\n",
    );

    let version = run_version("tool", &tool, directory.path(), Duration::from_secs(10))
        .expect("the version is read");

    assert_eq!(version, "tool 1.2.3");
}

#[cfg(unix)]
#[test]
fn the_tool_is_run_under_the_name_it_is_called_with() {
    use std::os::unix::fs::symlink;

    let directory = TempDir::new().expect("a temporary directory");
    let real = script(
        directory.path(),
        "real-tool",
        "#!/bin/sh\necho \"${0##*/} 1.2.3\"\n",
    );
    let proxy = directory.path().join("cargo");
    symlink(&real, &proxy).expect("the proxy is created");

    // The proxy dispatches on the name it is called with: the name must not
    // be lost on the way.
    let version = run_version("cargo", &proxy, directory.path(), Duration::from_secs(10))
        .expect("the version is read");

    assert_eq!(version, "cargo 1.2.3");
}

#[cfg(unix)]
#[test]
fn the_tool_is_run_in_the_directory_of_the_command() {
    let directory = TempDir::new().expect("a temporary directory");
    let tool = script(directory.path(), "tool", "#!/bin/sh\npwd\n");

    let version = run_version("tool", &tool, directory.path(), Duration::from_secs(10))
        .expect("the version is read");

    assert_eq!(
        version,
        fs::canonicalize(directory.path())
            .expect("a path")
            .to_string_lossy()
    );
}

#[cfg(unix)]
#[test]
fn a_tool_that_does_not_answer_fails_the_probe() {
    let directory = TempDir::new().expect("a temporary directory");
    let tool = script(directory.path(), "slow", "#!/bin/sh\nwhile :; do :; done\n");

    let error = run_version("slow", &tool, directory.path(), Duration::from_millis(200))
        .expect_err("the probe fails");

    assert!(matches!(error, Error::ToolProbe { .. }));
}

#[cfg(unix)]
#[test]
fn a_tool_that_cannot_be_run_fails_the_probe() {
    let directory = TempDir::new().expect("a temporary directory");
    let tool = directory.path().join("broken");
    fs::write(&tool, "not a program").expect("the file is written");

    let error = run_version(
        "broken",
        &tool,
        directory.path(),
        Duration::from_millis(200),
    )
    .expect_err("the probe fails");

    assert!(matches!(error, Error::ToolProbe { .. }));
}

#[cfg(unix)]
#[test]
fn a_tool_that_fails_fails_the_probe() {
    let directory = TempDir::new().expect("a temporary directory");
    let tool = script(
        directory.path(),
        "failing",
        "#!/bin/sh\necho 'no version here'\nexit 1\n",
    );

    let error = run_version("failing", &tool, directory.path(), Duration::from_secs(10))
        .expect_err("the probe fails");

    assert!(matches!(error, Error::ToolProbe { .. }));
}

#[cfg(unix)]
#[test]
fn a_tool_of_the_path_is_found() {
    let directory = TempDir::new().expect("temporary directory");
    let tool = script(directory.path(), "fixture-tool", "#!/bin/sh\necho ok\n");
    assert_eq!(
        find_in_directory(directory.path(), "fixture-tool"),
        Some(tool)
    );
}

#[cfg(unix)]
#[test]
fn a_file_that_cannot_be_run_is_not_a_tool() {
    let directory = TempDir::new().expect("a temporary directory");
    let file = directory.path().join("data");
    fs::write(&file, "data").expect("the file is written");

    assert!(!is_executable(&file));
}

#[cfg(unix)]
#[test]
fn the_executable_keeps_the_name_of_the_tool() {
    use std::os::unix::fs::symlink;

    let directory = TempDir::new().expect("a temporary directory");
    let real = script(directory.path(), "real-tool", "#!/bin/sh\necho ok\n");
    let proxy = directory.path().join("cargo");
    symlink(&real, &proxy).expect("the proxy is created");

    let found = find_in_directory(directory.path(), "cargo").expect("the tool is found");

    // The proxy is the executable of the tool: its name is what the tool
    // manager dispatches on.
    assert!(found.ends_with("cargo"));
    assert_ne!(found, fs::canonicalize(&real).expect("a path"));
}

#[cfg(unix)]
#[test]
fn the_path_of_a_tool_is_absolute() {
    let path = absolute(Path::new("relative/path"));

    assert!(path.is_absolute());
}

#[test]
fn the_cache_of_the_versions_separates_two_directories() {
    let first = CacheKey {
        cwd: PathBuf::from("/one"),
        path: PathBuf::from("/tool"),
        modified: None,
        selected: None,
    };
    let second = CacheKey {
        cwd: PathBuf::from("/two"),
        path: PathBuf::from("/tool"),
        modified: None,
        selected: None,
    };

    assert_ne!(first, second);
}

#[cfg(unix)]
#[test]
fn the_selected_tool_is_read_when_the_executable_is_a_proxy() {
    const CHILD: &str = "KIOKU_RUSTUP_PROBE_CHILD";
    if let Some(directory) = std::env::var_os(CHILD) {
        let directory = PathBuf::from(directory);
        let (selected, _) = selected_identity(&directory, "cargo", &directory.join("rustup"))
            .expect("selected tool");
        assert_eq!(
            selected,
            fs::canonicalize(directory.join("selected")).expect("selected path")
        );
        return;
    }
    let directory = TempDir::new().expect("temporary PATH");
    script(
        directory.path(),
        "rustup",
        "#!/bin/sh\nprintf '%s/selected\\n' \"$PWD\"\n",
    );
    fs::write(directory.path().join("selected"), "fixture").expect("selected fixture");
    let alias_parent = TempDir::new().expect("temporary alias parent");
    let alias = alias_parent.path().join("alias");
    std::os::unix::fs::symlink(directory.path(), &alias).expect("temporary alias");
    let output = Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "tools::tests::the_selected_tool_is_read_when_the_executable_is_a_proxy",
        ])
        .env("PATH", &alias)
        .env(CHILD, &alias)
        .output()
        .expect("child test");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}
