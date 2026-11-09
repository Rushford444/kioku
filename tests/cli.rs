//! Integration tests of the `kioku` command line.
//!
//! The tests use a real git repository in a temporary directory: the cache key
//! depends on the tracked files, the lockfiles and the tools, so a real
//! repository is the only honest fixture.
//!
//! The tests that run a child process need a shell, so they are `cfg(unix)`.
//! The tests of the command line contract itself run everywhere.

mod support;

#[cfg(unix)]
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::Command as Kioku;
use tempfile::TempDir;

/// A temporary git repository.
struct Repo {
    dir: TempDir,
}

impl Repo {
    fn new() -> Self {
        let dir = TempDir::new().expect("a temporary directory");
        git(dir.path(), &["init", "--quiet", "--initial-branch=main"]);
        git(dir.path(), &["config", "user.email", "kioku@example.com"]);
        git(dir.path(), &["config", "user.name", "kioku tests"]);
        git(dir.path(), &["config", "commit.gpgsign", "false"]);
        Self { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn write(&self, name: &str, content: &str) {
        let path = self.dir.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("the parent directory");
        }
        std::fs::write(path, content).expect("the file must be written");
    }

    fn commit(&self) {
        git(self.dir.path(), &["add", "--all"]);
        git(self.dir.path(), &["commit", "--quiet", "--message", "test"]);
    }

    fn kioku(&self) -> Kioku {
        kioku(self.dir.path())
    }

    fn run(&self) -> Run {
        Run::new(self.dir.path())
    }
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
fn tracer_path() -> PathBuf {
    std::env::var_os("KIOKU_STRACE").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("target/tracer/extracted/usr/bin/strace"),
        PathBuf::from,
    )
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("git must be available");
    assert!(status.success(), "git {args:?} failed");
}

fn kioku(dir: &Path) -> Kioku {
    let mut command = Kioku::new(env!("CARGO_BIN_EXE_kioku"));
    command.current_dir(dir);
    command
}

/// A kioku invocation with an empty standard input, like a build step.
///
/// The standard input is part of the decision: a command that reads a pipe or a
/// file is never cached. The tests must not depend on the standard input of the
/// test runner, so they give an empty one.
struct Run {
    command: std::process::Command,
    stdin: Option<String>,
}

impl Run {
    fn new(dir: &Path) -> Self {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_kioku"));
        command
            .current_dir(dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        Self {
            command,
            stdin: None,
        }
    }

    fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        self.command.args(args);
        self
    }

    fn arg<S: AsRef<std::ffi::OsStr>>(mut self, arg: S) -> Self {
        self.command.arg(arg);
        self
    }

    #[cfg(unix)]
    fn env<K, V>(mut self, key: K, value: V) -> Self
    where
        K: AsRef<std::ffi::OsStr>,
        V: AsRef<std::ffi::OsStr>,
    {
        self.command.env(key, value);
        self
    }

    /// Runs kioku from another directory of the repository.
    #[cfg(unix)]
    fn current_dir(mut self, dir: &Path) -> Self {
        self.command.current_dir(dir);
        self
    }

    /// Gives the command a standard input that carries content.
    #[cfg(unix)]
    fn with_stdin(mut self, input: &str) -> Self {
        self.command.stdin(std::process::Stdio::piped());
        self.stdin = Some(input.to_string());
        self
    }

    fn output(mut self) -> std::process::Output {
        let mut child = self.command.spawn().expect("kioku must run");
        if let Some(input) = &self.stdin {
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .expect("the standard input must be piped")
                .write_all(input.as_bytes())
                .expect("the standard input must be written");
        }
        child.wait_with_output().expect("kioku must run")
    }

    fn success(self) -> std::process::Output {
        let output = self.output();
        assert!(
            output.status.success(),
            "kioku failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn code(self, code: i32) -> std::process::Output {
        let output = self.output();
        assert_eq!(
            output.status.code(),
            Some(code),
            "kioku returned {output:?}"
        );
        output
    }
}

fn store_dir(repo: &Repo) -> PathBuf {
    repo.path().join(".git").join("kioku")
}

/// A command that succeeds, using the native shell of the platform.
fn native_success() -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd".to_string(), "/C".to_string(), "exit 0".to_string()]
    } else {
        vec!["sh".to_string(), "-c".to_string(), "exit 0".to_string()]
    }
}

/// The canonical form of a path, when it exists.
#[cfg(unix)]
fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// The canonical form of a path whose parent exists.
///
/// The store does not exist yet when kioku reports it, and a temporary
/// directory can be reached through a symbolic link: kioku canonicalizes the
/// longest existing ancestor, so the expectation must do the same.
#[cfg(unix)]
fn canonical_child(path: &Path) -> PathBuf {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => canonical(parent).join(name),
        _ => path.to_path_buf(),
    }
}

/// The cache key of a run, read from its JSON event.
fn entry_files(store: &Path) -> Vec<PathBuf> {
    let entries = store.join("entries");
    let mut files = Vec::new();
    if let Ok(reader) = std::fs::read_dir(entries) {
        for item in reader.flatten() {
            files.push(item.path());
        }
    }
    files
}

#[cfg(unix)]
#[test]
fn default_bytes_detects_self_read_source_change() {
    let repo = Repo::new();
    repo.write("value.js", "const value = 1;\n");
    repo.commit();
    let cmd = [
        "run",
        "--json",
        "--",
        "sh",
        "-c",
        "grep -q '^const value = 1;' value.js",
    ];
    repo.run().args(cmd).success();
    repo.write("value.js", "const  value = 1;\n");
    let output = repo.run().args(cmd).code(1);
    assert!(String::from_utf8_lossy(&output.stderr).contains("\"mode\":\"bytes\""));
}

#[cfg(unix)]
#[test]
fn explain_after_whitespace_change_does_not_offer_ast_reuse() {
    let repo = Repo::new();
    repo.write("value.js", "const value = 1;\n");
    repo.commit();
    let cmd = "grep -q '^const value = 1;' value.js";
    repo.run().args(["run", "--", "sh", "-c", cmd]).success();
    repo.write("value.js", "const  value = 1;\n");
    let explanation = repo
        .run()
        .args(["explain", "--json", "--", "sh", "-c", cmd])
        .success();
    let json: serde_json::Value = serde_json::from_slice(&explanation.stdout).unwrap();
    assert_eq!(json["hit"], false);
    assert_eq!(json["policy"], "execute-only");
    assert_eq!(json["mode_effective"], "bytes");
    let hypothetical = repo
        .run()
        .args(["explain", "--json", "--", "sh", "-c", cmd])
        .success();
    let hypothetical: serde_json::Value = serde_json::from_slice(&hypothetical.stdout).unwrap();
    assert_eq!(hypothetical["ast_would_skip"], true);
    assert_eq!(hypothetical["hit"], false);
    repo.run().args(["run", "--", "sh", "-c", cmd]).code(1);
}

#[cfg(unix)]
#[test]
fn configured_ast_and_cli_bytes_both_use_bytes_in_v2() {
    let repo = Repo::new();
    repo.write(
        "kioku.toml",
        "[[commands]]\nargv = [\"sh\", \"-c\", \"exit 0\"]\nmode = \"ast\"\n",
    );
    repo.write("value.js", "const value = 1;\n");
    repo.commit();
    let first = repo
        .run()
        .args(["run", "--json", "--", "sh", "-c", "exit 0"])
        .success();
    let a: serde_json::Value = serde_json::from_slice(&first.stderr).unwrap();
    assert_eq!(a["mode_requested"], "ast");
    assert_eq!(a["mode_effective"], "bytes");
    let second = repo
        .run()
        .args([
            "run", "--json", "--mode", "bytes", "--", "sh", "-c", "exit 0",
        ])
        .success();
    let b: serde_json::Value = serde_json::from_slice(&second.stderr).unwrap();
    assert_eq!(b["mode_requested"], "bytes");
    assert_eq!(b["mode_effective"], "bytes");
}
#[cfg(unix)]
#[test]
fn invalid_config_is_visible_and_never_executes_command() {
    let repo = Repo::new();
    repo.write("kioku.toml", "not = [valid");
    repo.commit();
    let output = repo
        .run()
        .args(["run", "--", "sh", "-c", "echo should-not-run"])
        .output();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid kioku.toml"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("should-not-run"));
}

#[cfg(unix)]
#[test]
fn ast_opt_in_reports_its_contract() {
    let repo = Repo::new();
    repo.write("value.js", "const value = 1;\n");
    repo.commit();
    let output = repo
        .run()
        .args(["run", "--json", "--mode", "ast", "--", "sh", "-c", "exit 0"])
        .success();
    let event: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(event["mode_requested"], "ast");
    assert_eq!(event["mode_effective"], "bytes");
    assert_eq!(event["decision"], "executed");
    assert_eq!(event["reason"], "execute-only");
}
#[cfg(unix)]
#[test]
fn default_success_executes_again_instead_of_reusing_v1() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    for _ in 0..2 {
        let out = repo
            .run()
            .args(["run", "--json", "--", "sh", "-c", "exit 0"])
            .success();
        let event: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(event["decision"], "executed");
        assert_eq!(event["reason"], "execute-only");
        assert_eq!(event["saved_ms"], 0);
    }
}
#[cfg(unix)]
#[test]
fn transmits_the_exit_code_of_the_command() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();

    repo.run().args(["run", "--", "sh", "-c", "exit 7"]).code(7);
}

#[cfg(unix)]
#[test]
fn never_caches_a_failure() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();

    repo.run().args(["run", "--", "sh", "-c", "exit 3"]).code(3);
    let second = repo.run().args(["run", "--", "sh", "-c", "exit 3"]).code(3);
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(!stderr.contains("cached (pass"), "stderr was: {stderr}");
}

#[cfg(unix)]
#[test]
fn default_success_then_failure_preserves_actual_exit() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    repo.run()
        .args(["run", "--", "sh", "-c", "grep -q hello main.txt"])
        .success();
    repo.write("main.txt", "bye\n");
    repo.run()
        .args(["run", "--", "sh", "-c", "grep -q hello main.txt"])
        .code(1);
    assert_eq!(entry_files(&store_dir(&repo)).len(), 0);
}
#[cfg(unix)]
#[test]
fn an_untracked_file_changes_the_key() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();

    repo.run()
        .args(["run", "--", "sh", "-c", "exit 0"])
        .success();
    repo.write("untracked.txt", "new\n");
    let second = repo
        .run()
        .args(["run", "--", "sh", "-c", "exit 0"])
        .success();
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(!stderr.contains("cached (pass"), "stderr was: {stderr}");
}

#[cfg(unix)]
#[test]
fn default_execution_observes_ignored_file_change() {
    let repo = Repo::new();
    repo.write(".gitignore", "ignored.txt\n");
    repo.write("ignored.txt", "yes\n");
    repo.commit();
    let args = ["run", "--", "sh", "-c", "grep -q yes ignored.txt"];
    repo.run().args(args).success();
    repo.write("ignored.txt", "no\n");
    repo.run().args(args).code(1);
}
#[cfg(unix)]
#[test]
fn a_modified_lockfile_changes_the_key() {
    let repo = Repo::new();
    repo.write("package-lock.json", "{}\n");
    repo.write("main.txt", "hello\n");
    repo.commit();

    repo.run()
        .args(["run", "--", "sh", "-c", "exit 0"])
        .success();
    repo.write("package-lock.json", "{\"lockfileVersion\": 3}\n");
    let second = repo
        .run()
        .args(["run", "--", "sh", "-c", "exit 0"])
        .success();
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(!stderr.contains("cached (pass"), "stderr was: {stderr}");
}

#[cfg(unix)]
#[test]
fn a_malformed_legacy_entry_is_never_a_hit() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    let entries = store_dir(&repo).join("entries");
    std::fs::create_dir_all(&entries).unwrap();
    std::fs::write(entries.join("malformed.json"), "{ not json").unwrap();
    let output = repo
        .run()
        .args(["run", "--json", "--", "sh", "-c", "exit 0"])
        .success();
    let event: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(event["decision"], "executed");
    assert_eq!(event["reason"], "execute-only");
}
#[cfg(unix)]
#[test]
fn two_agents_can_run_at_the_same_time() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();

    let first = repo
        .run()
        .args(["run", "--", "sh", "-c", "exit 0"])
        .success();
    let second = repo
        .run()
        .args(["run", "--", "sh", "-c", "exit 0"])
        .success();
    assert!(first.status.success());
    assert!(second.status.success());
    repo.run().args(["status"]).success();
}

#[cfg(unix)]
#[test]
fn an_external_store_does_not_leak_between_different_repositories() {
    let first = Repo::new();
    first.write("main.txt", "one\n");
    first.commit();
    let second = Repo::new();
    second.write("main.txt", "two\n");
    second.commit();
    let shared = TempDir::new().expect("a temporary directory");

    first
        .run()
        .env("KIOKU_DIR", shared.path())
        .args(["run", "--", "sh", "-c", "exit 0"])
        .success();
    let other = second
        .run()
        .env("KIOKU_DIR", shared.path())
        .args(["run", "--", "sh", "-c", "exit 0"])
        .success();
    let stderr = String::from_utf8_lossy(&other.stderr);
    assert!(!stderr.contains("cached (pass"), "stderr was: {stderr}");
}

#[cfg(unix)]
#[test]
fn cache_disable_values_are_interpreted_individually() {
    let program = support::true_program();
    let program = program.to_str().unwrap();
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    for (value, disabled) in [
        ("1", true),
        ("true", true),
        ("yes", true),
        ("on", true),
        ("TRUE", true),
        ("0", false),
        ("false", false),
        ("", false),
    ] {
        let output = repo
            .run()
            .env("KIOKU_DISABLE", value)
            .args(["run", "--json", "--trust-trace", "--", program])
            .success();
        let event: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(
            event["reason"] == "cache-disabled",
            disabled,
            "value: {value:?}"
        );
    }
}
#[cfg(unix)]
#[test]
fn external_store_does_not_enable_default_reuse_between_repositories() {
    let first = Repo::new();
    first.write("main.txt", "same\n");
    first.commit();
    let second = Repo::new();
    second.write("main.txt", "same\n");
    second.commit();
    let shared = TempDir::new().unwrap();
    for repo in [&first, &second] {
        let output = repo
            .run()
            .env("KIOKU_DIR", shared.path())
            .args(["run", "--json", "--", "sh", "-c", "exit 0"])
            .success();
        let event: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(event["decision"], "executed");
    }
}
#[cfg(unix)]
#[test]
fn json_disabled_cache_reason_is_json_only() {
    let program = support::true_program();
    let program = program.to_str().unwrap();
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    let output = repo
        .run()
        .env("KIOKU_DISABLE", "1")
        .args(["run", "--json", "--trust-trace", "--", program])
        .success();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let events: Vec<serde_json::Value> = stderr
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["decision"], "executed");
    assert_eq!(events[0]["reason"], "cache-disabled");
}
#[cfg(unix)]
#[test]
fn deeply_nested_javascript_executes_in_default_and_ast_modes() {
    let program = support::true_program();
    let program = program.to_str().unwrap();
    let repo = Repo::new();
    repo.write(
        "value.js",
        &format!(
            "const x = {}1{};\n",
            "(".repeat(150_000),
            ")".repeat(150_000)
        ),
    );
    repo.commit();
    for args in [
        vec!["run", "--json", "--", program],
        vec!["run", "--json", "--mode", "ast", "--", program],
    ] {
        let output = repo.run().args(args).success();
        let event: serde_json::Value = serde_json::from_slice(&output.stderr).expect("JSON event");
        assert_eq!(event["decision"], "executed");
        assert_eq!(event["exit_code"], 0);
    }
}

#[cfg(unix)]
#[test]
fn v02_rejects_invalid_verify_rates_and_reports_ast_effective_bytes() {
    let program = support::true_program();
    let program = program.to_str().unwrap();
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    for rate in ["-0.1", "1.1", "NaN"] {
        repo.run()
            .args(["run", "--verify-rate", rate, "--", program])
            .code(2);
    }
    let output = repo
        .run()
        .args([
            "run",
            "--json",
            "--mode",
            "ast",
            "--trust-trace",
            "--require-output",
            "--",
            program,
        ])
        .success();
    let event: serde_json::Value = serde_json::from_slice(&output.stderr).expect("JSON event");
    assert_eq!(event["mode_requested"], "ast");
    assert_eq!(event["mode_effective"], "bytes");
    assert_eq!(event["reason"], "output-required");
    assert_eq!(event["contract"], "caller-asserted-stable-inputs");
}

#[cfg(unix)]
#[test]
fn v02_default_executes_even_after_success() {
    let program = support::true_program();
    let program = program.to_str().unwrap();
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    let first = repo.run().args(["run", "--json", "--", program]).success();
    let second = repo.run().args(["run", "--json", "--", program]).success();
    for output in [first, second] {
        let event: serde_json::Value = serde_json::from_slice(&output.stderr).expect("JSON event");
        assert_eq!(event["decision"], "executed");
        assert_eq!(event["policy"], "execute-only");
        assert_eq!(event["cacheable"], false);
    }
}

#[cfg(unix)]
#[test]
fn the_cache_can_be_disabled() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();

    repo.run()
        .args(["run", "--", "sh", "-c", "exit 0"])
        .success();
    let second = repo
        .run()
        .env("KIOKU_DISABLE", "1")
        .args(["run", "--", "sh", "-c", "exit 0"])
        .success();
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(!stderr.contains("cached (pass"), "stderr was: {stderr}");
}

#[cfg(unix)]
#[test]
fn the_child_keeps_stdout_and_kioku_writes_its_event_on_stderr() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();

    let output = repo
        .run()
        .args(["run", "--json", "--", "sh", "-c", "echo from-the-child"])
        .output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("from-the-child"), "stdout was: {stdout}");
    assert!(
        stderr.contains("\"schema\":\"kioku/v1\""),
        "stderr was: {stderr}"
    );
    assert!(
        stderr.contains("\"mode\":\"bytes\""),
        "stderr was: {stderr}"
    );
}

#[cfg(unix)]
#[test]
fn the_child_inherits_stdin() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();

    let output = repo
        .run()
        .args(["run", "--", "sh", "-c", "cat"])
        .with_stdin("from stdin\n")
        .output();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("from stdin"), "stdout was: {stdout}");
}

#[cfg(unix)]
#[test]
fn a_store_inside_the_work_tree_disables_the_cache() {
    // A store in the work tree could hide a tracked file from the key, so the
    // cache is off: the command always runs and nothing is memorized.
    for name in ["store", ".store"] {
        let repo = Repo::new();
        repo.write("main.txt", "hello\n");
        repo.commit();
        let store = repo.path().join(name);

        repo.run()
            .env("KIOKU_DIR", &store)
            .args(["run", "--", "sh", "-c", "exit 0"])
            .success();
        let second = repo
            .run()
            .env("KIOKU_DIR", &store)
            .args(["run", "--", "sh", "-c", "exit 0"])
            .success();
        let stderr = String::from_utf8_lossy(&second.stderr);
        assert!(!stderr.contains("cached (pass"), "stderr was: {stderr}");
        assert!(!store.exists(), "default execution must not create a store");
    }
}

/// The names kioku would leave in a store directory.
#[cfg(unix)]
fn store_entries(store: &Path) -> Vec<String> {
    let Ok(reader) = std::fs::read_dir(store) else {
        return Vec::new();
    };
    reader
        .map(|item| {
            item.expect("a store entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

/// A store in the work tree turns the cache off, whatever path names it.
///
/// The files of the store are not part of the key, so a tracked file that
/// happens to live there would be left out of it too, and a change of it would
/// not change the key. The refusal must not depend on how the path is
/// written: an absolute path, a relative path, a path with a detour, a
/// trailing separator and a symbolic link all name the same directory.
#[cfg(unix)]
#[test]
fn a_store_in_the_work_tree_turns_the_cache_off_for_every_path_alias() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    let inside = repo.path().join("cache");
    std::fs::create_dir_all(&inside).expect("the store directory");
    let link = repo.path().join("link-to-cache");
    std::os::unix::fs::symlink(&inside, &link).expect("the symbolic link");

    for alias in [
        inside.clone(),
        PathBuf::from("cache"),
        PathBuf::from("./cache"),
        PathBuf::from("cache/"),
        link.clone(),
    ] {
        let output = repo
            .run()
            .env("KIOKU_DIR", &alias)
            .args(["run", "--json", "--trust-trace", "--", "sh", "-c", "exit 0"])
            .output();
        assert!(output.status.success(), "kioku failed: {output:?}");
        let event: serde_json::Value =
            serde_json::from_slice(&output.stderr).expect("the JSON event");
        assert_eq!(
            event["reason"], "store-in-work-tree",
            "the store {alias:?} must turn the cache off: {event}"
        );
        assert_eq!(event["denial_reason"], "store-in-work-tree", "{event}");
        assert_eq!(event["decision"], "executed", "{event}");
        assert!(
            store_entries(&inside).is_empty(),
            "kioku must leave the work tree alone: {alias:?}"
        );
    }
}

/// A store outside the work tree names a store the cache can use.
#[cfg(unix)]
#[test]
fn a_store_outside_the_work_tree_is_not_refused() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    let outside = TempDir::new().expect("a temporary store");
    let store = outside.path().join("store");

    let output = repo
        .run()
        .env("KIOKU_DIR", &store)
        .args(["run", "--json", "--trust-trace", "--", "sh", "-c", "exit 0"])
        .output();
    assert!(output.status.success(), "kioku failed: {output:?}");
    let event: serde_json::Value = serde_json::from_slice(&output.stderr).expect("the JSON event");
    assert_ne!(
        event["reason"], "store-in-work-tree",
        "a store outside the work tree is not refused: {event}"
    );
    assert!(store.exists(), "the store outside the work tree is used");
}

/// The default store lives in the git directory: the cache is on there.
#[cfg(unix)]
#[test]
fn the_default_store_is_not_refused() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();

    let output = repo
        .run()
        .args(["run", "--json", "--trust-trace", "--", "sh", "-c", "exit 0"])
        .output();
    assert!(output.status.success(), "kioku failed: {output:?}");
    let event: serde_json::Value = serde_json::from_slice(&output.stderr).expect("the JSON event");
    assert_ne!(
        event["reason"], "store-in-work-tree",
        "the default store is not refused: {event}"
    );
}

/// The explanation says the same thing as the run.
#[cfg(unix)]
#[test]
fn explain_refuses_a_store_in_the_work_tree() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    let store = repo.path().join("cache");

    let output = repo
        .run()
        .env("KIOKU_DIR", &store)
        .args([
            "explain",
            "--json",
            "--trust-trace",
            "--",
            "sh",
            "-c",
            "exit 0",
        ])
        .output();
    assert!(output.status.success(), "kioku failed: {output:?}");
    let event: serde_json::Value = serde_json::from_slice(&output.stdout).expect("the JSON event");
    assert_eq!(event["reason"], "store-in-work-tree", "{event}");
    assert_eq!(event["denial_reason"], "store-in-work-tree", "{event}");
    assert_eq!(event["hit"], false, "{event}");
    assert!(
        store_entries(&store).is_empty(),
        "kioku must leave the work tree alone"
    );
}

/// The refusal is a decision about the store: no environment value reaches
/// the output, and nothing is written in the work tree.
#[cfg(unix)]
#[test]
fn a_refused_store_keeps_the_environment_out_of_the_output() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    let store = repo.path().join("cache");
    let sentinel = "kioku-store-refusal-sentinel-4f21a";

    let output = repo
        .run()
        .env("KIOKU_DIR", &store)
        .env("KIOKU_SENTINEL_SECRET", sentinel)
        .args(["run", "--json", "--trust-trace", "--", "sh", "-c", "exit 0"])
        .output();
    assert!(output.status.success(), "kioku failed: {output:?}");
    let event: serde_json::Value = serde_json::from_slice(&output.stderr).expect("the JSON event");
    assert_eq!(event["reason"], "store-in-work-tree", "{event}");
    let text = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(!text.contains(sentinel), "the environment leaked: {text}");
    assert!(
        store_entries(&store).is_empty(),
        "kioku must leave the work tree alone"
    );
}

/// No command creates a store in the work tree.
#[cfg(unix)]
#[test]
fn no_command_creates_a_store_in_the_work_tree() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    let store = repo.path().join("cache");
    let program = support::true_program();

    let commands: Vec<Vec<&str>> = vec![
        vec!["run", "--trust-trace", "--"],
        vec!["run", "--"],
        vec!["verify", "--trust-trace", "--"],
        vec!["explain", "--trust-trace", "--"],
        vec!["status"],
        vec!["clean", "--all"],
    ];
    for args in commands {
        let mut command = repo.run().env("KIOKU_DIR", &store).args(&args);
        if args.last() == Some(&"--") {
            command = command.arg(&program);
        }
        let output = command.output();
        assert!(output.status.success(), "kioku {args:?} failed: {output:?}");
        assert!(
            store_entries(&store).is_empty(),
            "kioku {args:?} must leave the work tree alone"
        );
    }
    let output = repo
        .run()
        .env("KIOKU_DIR", &store)
        .args(["quarantine", "reset", "--"])
        .arg(&program)
        .output();
    assert!(output.status.success(), "kioku failed: {output:?}");
    assert!(!store.exists(), "kioku must leave the work tree alone");
}

/// The status of a store in the work tree reads the empty store, and does not
/// create it.
#[cfg(unix)]
#[test]
fn the_status_of_a_store_in_the_work_tree_creates_nothing() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    let store = repo.path().join("cache");

    let output = repo
        .run()
        .env("KIOKU_DIR", &store)
        .args(["status", "--json"])
        .output();
    assert!(output.status.success(), "kioku failed: {output:?}");
    let event: serde_json::Value = serde_json::from_slice(&output.stdout).expect("the JSON event");
    assert_eq!(event["counters"]["hits"], 0, "{event}");
    assert_eq!(event["counters"]["executions"], 0, "{event}");
    assert_eq!(
        event["store"],
        canonical_child(&store).to_string_lossy().as_ref(),
        "{event}"
    );
    assert_eq!(
        event["reason"], "store-in-work-tree",
        "the status must say why it is empty: {event}"
    );
    assert!(!store.exists(), "kioku must leave the work tree alone");

    let output = repo.run().env("KIOKU_DIR", &store).arg("status").output();
    assert!(output.status.success());
    let human = String::from_utf8_lossy(&output.stdout);
    assert!(
        human.contains("the store is inside the work tree"),
        "the human status must say why it is empty: {human}"
    );
}

/// The clean of a store in the work tree says the cache is off, and leaves
/// the store alone.
#[cfg(unix)]
#[test]
fn the_clean_of_a_store_in_the_work_tree_says_the_cache_is_off() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    let store = repo.path().join("cache");
    std::fs::create_dir_all(&store).expect("the store directory");
    let stale = store.join("counters.json");
    std::fs::write(&stale, "{\"schema\":\"kioku/counters/v1\"}").expect("the stale document");

    let output = repo
        .run()
        .env("KIOKU_DIR", &store)
        .args(["clean", "--all", "--json"])
        .output();
    assert!(output.status.success(), "kioku failed: {output:?}");
    let event: serde_json::Value = serde_json::from_slice(&output.stdout).expect("the JSON event");
    assert_eq!(
        event["reason"], "store-in-work-tree",
        "the clean must say why it removed nothing: {event}"
    );
    assert_eq!(event["removed"], 0, "{event}");
    assert!(
        stale.exists(),
        "the store of a refused clean is left to its owner"
    );

    let output = repo
        .run()
        .env("KIOKU_DIR", &store)
        .args(["clean", "--all"])
        .output();
    assert!(output.status.success());
    let human = String::from_utf8_lossy(&output.stdout);
    assert!(
        human.contains("the store is inside the work tree"),
        "the human clean must say why it removed nothing: {human}"
    );
}

#[cfg(unix)]
#[test]
fn the_key_does_not_depend_on_the_absolute_path() {
    let first = Repo::new();
    first.write("main.txt", "same\n");
    first.commit();
    let second = Repo::new();
    second.write("main.txt", "same\n");
    second.commit();

    let key_of = |repo: &Repo| {
        let output = repo
            .run()
            .args(["explain", "--json", "--", "sh", "-c", "exit 0"])
            .output();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let value: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
        value["key"].as_str().expect("a key").to_string()
    };
    assert_eq!(key_of(&first), key_of(&second));
}

#[test]
fn a_usage_error_is_reported_as_json() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();

    let output = repo
        .kioku()
        .args(["run", "--json"])
        .output()
        .expect("kioku must run");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    let value: serde_json::Value = serde_json::from_str(stderr.trim()).expect("valid JSON");
    assert_eq!(value["schema"], "kioku/v1");
    assert_eq!(value["error"]["kind"], "usage");
}

#[cfg(unix)]
#[test]
fn a_source_change_during_the_command_prevents_memorizing() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();

    // The command rewrites the file it depends on and still succeeds.
    repo.run()
        .args(["run", "--", "sh", "-c", "printf 'changed\\n' > main.txt"])
        .success();
    let store = store_dir(&repo);
    assert!(
        entry_files(&store).is_empty(),
        "a source that moved during the command must not be memorized"
    );
}

#[cfg(unix)]
#[test]
fn a_same_size_change_with_a_restored_timestamp_is_not_memorized() {
    let repo = Repo::new();
    repo.write("main.rs", "fn a() { let b = 1; }\n");
    repo.commit();
    let reference_dir = TempDir::new().expect("a temporary directory");
    let reference = reference_dir.path().join("reference");

    // The command replaces a space by a tab: the tree is the same, the size is
    // the same, and the timestamp is put back. Only a digest of the raw bytes
    // can see that the source moved.
    repo.run()
        .env("KIOKU_TEST_REFERENCE", &reference)
        .args([
            "run",
            "--",
            "sh",
            "-c",
            "cp -p main.rs \"$KIOKU_TEST_REFERENCE\" && printf 'fn a() {\tlet b = 1; }\n' > main.rs && touch -r \"$KIOKU_TEST_REFERENCE\" main.rs",
        ])
        .success();
    let store = store_dir(&repo);
    assert!(
        entry_files(&store).is_empty(),
        "a same size change with a restored timestamp must not be memorized"
    );
}

#[cfg(unix)]
#[test]
fn a_tampered_legacy_manifest_is_never_a_hit() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    let entries = store_dir(&repo).join("entries");
    std::fs::create_dir_all(&entries).unwrap();
    std::fs::write(
        entries.join("legacy.json"),
        br#"{"schema":"kioku/entry/v1","inputs":{"main.txt":{"digest":"tampered"}}}"#,
    )
    .unwrap();
    let output = repo
        .run()
        .args(["run", "--json", "--", "sh", "-c", "exit 0"])
        .success();
    let event: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(event["decision"], "executed");
}
#[cfg(unix)]
#[test]
fn a_relative_store_is_resolved_against_the_working_directory() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    let nested = repo.path().join("nested");
    std::fs::create_dir_all(&nested).expect("the nested directory");

    // A relative KIOKU_DIR is resolved against the working directory, the way
    // a shell resolves it: the store of a run from `nested` is `nested/store`.
    // The store is in the work tree, so the cache is off and the path of the
    // store is reported.
    let output = repo
        .run()
        .current_dir(&nested)
        .env("KIOKU_DIR", "store")
        .args(["status", "--json"])
        .output();
    let stderr = String::from_utf8_lossy(&output.stdout);
    let expected = canonical(&nested).join("store");
    assert!(
        stderr.contains(expected.to_string_lossy().as_ref()),
        "the store must be resolved against the working directory: {stderr}"
    );
    let wrong = canonical(repo.path()).join("store");
    assert!(
        !stderr.contains(wrong.to_string_lossy().as_ref()),
        "the store must not be resolved against the root of the repository: {stderr}"
    );
}

#[cfg(unix)]
#[test]
fn a_relative_store_is_resolved_against_the_root_of_a_worktree() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    let worktree = repo.path().join("worktree");
    git(
        repo.path(),
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            worktree.to_str().expect("a path"),
        ],
    );

    // The git directory of a worktree lives in the main repository, not at the
    // root of the worktree: the store must follow the root of the worktree.
    let output = repo
        .run()
        .current_dir(&worktree)
        .env("KIOKU_DIR", "store")
        .args(["status", "--json"])
        .output();
    let stderr = String::from_utf8_lossy(&output.stdout);
    let expected = canonical(&worktree).join("store");
    assert!(
        stderr.contains(expected.to_string_lossy().as_ref()),
        "the store must follow the root of the worktree: {stderr}"
    );
    let wrong = canonical(repo.path()).join("store");
    assert!(
        !stderr.contains(wrong.to_string_lossy().as_ref()),
        "the store must not be resolved against the main repository: {stderr}"
    );
}

#[cfg(unix)]
#[test]
fn a_store_that_shadows_a_source_directory_does_not_hide_a_change() {
    let repo = Repo::new();
    repo.write("cache/value", "pass\n");
    repo.commit();
    let nested = repo.path().join("nested");
    std::fs::create_dir_all(&nested).expect("the nested directory");

    // KIOKU_DIR is relative to the working directory: the store of a run from
    // `nested` is `nested/cache`, never the tracked `cache` directory of the
    // repository. The tracked file stays in the key, so rewriting it is a miss.
    let command = [
        "run",
        "--mode",
        "bytes",
        "--",
        "sh",
        "-c",
        "test \"$(cat ../cache/value)\" = pass",
    ];
    repo.run()
        .current_dir(&nested)
        .env("KIOKU_DIR", "cache")
        .args(command)
        .success();
    repo.write("cache/value", "fail\n");
    let second = repo
        .run()
        .current_dir(&nested)
        .env("KIOKU_DIR", "cache")
        .args(command)
        .code(1);
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(!stderr.contains("cached (pass"), "stderr was: {stderr}");
}

#[cfg(unix)]
#[test]
fn explain_after_permissions_change_does_not_claim_default_hit() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    repo.run()
        .args(["run", "--", "sh", "-c", "exit 0"])
        .success();

    let path = repo.path().join("main.txt");
    let mut permissions = std::fs::metadata(&path)
        .expect("the metadata")
        .permissions();
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o755);
    }
    std::fs::set_permissions(&path, permissions).expect("the permissions must be set");

    let output = repo
        .run()
        .args(["explain", "--json", "--", "sh", "-c", "exit 0"])
        .output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(value["hit"], false);
    assert_eq!(value["policy"], "execute-only");
    assert!(
        value["diff"].is_null(),
        "v2 does not invent a legacy diff: {stdout}"
    );
}

#[cfg(not(windows))]
#[test]
fn a_document_that_does_not_describe_its_key_is_not_an_entry() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    let entries = store_dir(&repo).join("entries");
    std::fs::create_dir_all(&entries).unwrap();
    std::fs::write(
        entries.join("wrong-schema.json"),
        br#"{"schema":"kioku/entry/v0"}"#,
    )
    .unwrap();
    std::fs::write(
        entries.join("other.json"),
        br#"{"schema":"kioku/entry/v1","key":"not-the-filename"}"#,
    )
    .unwrap();
    let output = repo.run().args(["status", "--json"]).output();
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["entries"], 0);
    assert_eq!(value["invalid_entries"], 2);
}
#[cfg(unix)]
#[test]
fn a_store_that_overlaps_a_source_directory_never_hides_a_change() {
    // KIOKU_DIR points at a directory that holds source files: the store and
    // the source overlap. The source must stay in the key, or the result of a
    // command would be served from the cache after the file changed.
    for tracked in [true, false] {
        let repo = Repo::new();
        repo.write("cache/value", "pass\n");
        if tracked {
            repo.commit();
        }
        let command = [
            "run",
            "--mode",
            "bytes",
            "--",
            "sh",
            "-c",
            "test \"$(cat cache/value)\" = pass",
        ];
        let first = repo
            .run()
            .env("KIOKU_DIR", "cache")
            .args([
                "run",
                "--json",
                "--mode",
                "bytes",
                "--",
                "sh",
                "-c",
                "test \"$(cat cache/value)\" = pass",
            ])
            .success();
        let first_stderr = String::from_utf8_lossy(&first.stderr);
        assert!(
            first_stderr.contains("\"stored\":false"),
            "nothing must be memorized: {first_stderr}"
        );
        repo.write("cache/value", "fail\n");
        let second = repo.run().env("KIOKU_DIR", "cache").args(command).code(1);
        let stderr = String::from_utf8_lossy(&second.stderr);
        assert!(
            !stderr.contains("cached (pass"),
            "tracked={tracked} stderr was: {stderr}"
        );
        assert!(
            stderr.contains("executed"),
            "tracked={tracked} stderr was: {stderr}"
        );
    }
}

#[cfg(unix)]
#[test]
fn explain_has_no_closest_entry_after_default_runs() {
    let repo = Repo::new();
    repo.write("one.txt", "a\n");
    repo.write("two.txt", "a\n");
    repo.commit();

    let first = repo
        .run()
        .args(["run", "--json", "--", "sh", "-c", "exit 0"])
        .success();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&first.stderr).unwrap()["decision"],
        "executed"
    );

    repo.write("one.txt", "b\n");
    repo.write("two.txt", "b\n");
    let second = repo
        .run()
        .args(["run", "--json", "--", "sh", "-c", "exit 0"])
        .success();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&second.stderr).unwrap()["decision"],
        "executed"
    );

    // Default execution creates no reusable entry to compare.
    repo.write("one.txt", "c\n");
    let output = repo
        .run()
        .args(["explain", "--json", "--", "sh", "-c", "exit 0"])
        .output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(value["hit"], false);
    assert!(value["closest"].is_null());
}

#[cfg(windows)]
#[test]
fn windows_runs_twice_without_storing() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    for _ in 0..2 {
        let output = repo
            .run()
            .args(["run", "--", "cmd", "/C", "echo ran>>counter.txt"])
            .success();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("execute-only"), "{stderr}");
        assert!(!stderr.contains("cached (pass"), "{stderr}");
    }
    let content = std::fs::read_to_string(repo.path().join("counter.txt")).expect("counter");
    assert_eq!(content.lines().count(), 2);
    assert_eq!(entry_files(&store_dir(&repo)).len(), 0);
}

#[test]
fn native_command_executes_each_time_without_trace_consent() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    for _ in 0..2 {
        let output = repo
            .run()
            .args(["run", "--json", "--"])
            .args(native_success())
            .success();
        let event: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(event["decision"], "executed");
        assert_eq!(event["reason"], "execute-only");
    }
}
#[cfg(unix)]
#[test]
fn a_command_that_reads_the_standard_input_is_never_cached() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();

    // The standard input carries content the key cannot capture: the command is
    // always run and its result is never memorized.
    for _ in 0..2 {
        let output = repo
            .run()
            .args(["run", "--", "sh", "-c", "cat > /dev/null"])
            .with_stdin("some input\n")
            .output();
        assert!(output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("cached (pass"), "stderr was: {stderr}");
    }
    let store = store_dir(&repo);
    assert!(
        entry_files(&store).is_empty(),
        "a command that reads the standard input must not be memorized"
    );
}

#[test]
fn the_help_lists_the_commands_and_the_examples() {
    let repo = Repo::new();
    let output = repo.kioku().arg("--help").output().expect("kioku must run");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in ["run", "verify", "explain", "status", "clean", "Examples:"] {
        assert!(stdout.contains(expected), "help was: {stdout}");
    }
}

#[test]
fn the_run_help_lists_the_documented_options_only() {
    let repo = Repo::new();
    let output = repo
        .kioku()
        .args(["run", "--help"])
        .output()
        .expect("kioku must run");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut options: Vec<&str> = stdout
        .lines()
        .flat_map(str::split_whitespace)
        .filter(|token| token.starts_with("--"))
        .collect();
    options.sort_unstable();
    options.dedup();
    assert_eq!(
        options,
        [
            "--help",
            "--json",
            "--mode",
            "--quiet",
            "--require-output",
            "--trust-trace",
            "--verify-rate",
        ],
        "run help was: {stdout}"
    );
    assert!(
        stdout.contains(
            "The complete initial environment is part of the trace identity; no option narrows it."
        ),
        "run help was: {stdout}"
    );
}

#[cfg(unix)]
#[test]
fn removed_input_and_environment_options_are_usage_errors() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    let removed: [&[&str]; 3] = [
        &["--inputs", "main.txt"],
        &["--env", "PATH"],
        &["--strict-env"],
    ];
    for option in removed {
        let output = repo
            .kioku()
            .args(["run"])
            .args(option)
            .args(["--", "sh", "-c", "exit 0"])
            .output()
            .expect("kioku must run");
        assert_eq!(
            output.status.code(),
            Some(2),
            "option {option:?} was accepted: {output:?}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        let expected = format!("error: unexpected argument '{}' found", option[0]);
        assert_eq!(
            stderr.lines().next(),
            Some(expected.as_str()),
            "option {option:?} was not reported exactly: {stderr}"
        );
    }
}

#[cfg(unix)]
#[test]
fn an_unknown_run_option_is_a_usage_error() {
    let repo = Repo::new();
    let output = repo
        .kioku()
        .args(["run", "--foo", "--", "sh", "-c", "exit 0"])
        .output()
        .expect("kioku must run");
    assert_eq!(output.status.code(), Some(2), "output was: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr.lines().next(),
        Some("error: unexpected argument '--foo' found"),
        "unknown option was not reported exactly: {stderr}"
    );
}

#[test]
fn git_errors_do_not_repeat_the_git_prefix() {
    let error = kioku::Error::Git {
        command: "rev-parse --show-toplevel".into(),
        stderr: "boom".into(),
    };
    assert_eq!(
        error.to_string(),
        "git rev-parse --show-toplevel failed: boom"
    );
}

#[test]
fn the_version_is_printed() {
    let repo = Repo::new();
    let output = repo
        .kioku()
        .arg("--version")
        .output()
        .expect("kioku must run");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "version was: {stdout}"
    );
}

#[test]
fn status_and_clean_outside_a_repository_succeed_with_a_message() {
    let outside = TempDir::new().expect("a temporary directory");
    for command in ["status", "clean"] {
        let mut args = vec![command];
        if command == "clean" {
            args.push("--all");
        }
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_kioku"))
            .current_dir(outside.path())
            .args(&args)
            .output()
            .expect("kioku must run");
        assert!(output.status.success(), "{command}: {output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("not a Git repository"), "{stdout}");
    }
}

#[test]
fn clean_needs_a_duration_or_all() {
    let repo = Repo::new();
    repo.kioku().args(["clean"]).assert().code(2);
}

#[test]
fn status_reports_the_store_in_json() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();

    let output = repo.run().args(["status", "--json"]).output();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(value["schema"], "kioku/v1");
    assert_eq!(value["entries"], 0);
    assert!(
        value["store"]
            .as_str()
            .is_some_and(|store| store.ends_with("kioku"))
    );
}

#[test]
fn a_missing_command_reports_127() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();

    repo.kioku()
        .args(["run", "--", "kioku-command-that-does-not-exist"])
        .assert()
        .code(127);
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[test]
#[ignore = "requires pinned tracer"]
fn traced_dynamic_binary_reports_executable_format() {
    let repo = Repo::new();
    repo.write("tracked", "x\n");
    repo.commit();
    let output = repo
        .run()
        .env("KIOKU_STRACE", tracer_path())
        .args(["run", "--json", "--trust-trace", "--", "/usr/bin/true"])
        .output();
    assert!(output.status.success());
    let event: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(event["decision"], "executed");
    assert_eq!(event["denial_reason"], "executable-format");
    assert_eq!(event["trace_state"], "denied");
    assert_eq!(event["cacheable"], false);
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[test]
#[ignore = "requires pinned tracer"]
fn traced_relative_symlink_and_path_programs_are_cached() {
    let repo = Repo::new();
    repo.write("input", "x\n");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/false_green_zoo/read_aarch64.S");
    assert!(
        Command::new(support::executable("cc"))
            .args(["-nostdlib", "-static", "-o"])
            .arg(repo.path().join("reader"))
            .arg(source)
            .status()
            .unwrap()
            .success()
    );
    std::os::unix::fs::symlink("reader", repo.path().join("linkreader")).unwrap();
    repo.commit();
    let tracer = tracer_path();
    assert!(tracer.exists(), "pinned tracer required");
    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let invoke = |program: &str, with_path: bool| {
        let mut command = repo.run().env("KIOKU_STRACE", &tracer);
        if with_path {
            let mut path = repo.path().as_os_str().to_owned();
            path.push(":");
            path.push(&original_path);
            command = command.env("PATH", path);
        }
        let output = command
            .args(["run", "--json", "--trust-trace", "--", program, "input"])
            .output();
        assert!(
            output.status.success(),
            "{program}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stderr).unwrap()
    };
    for (program, with_path) in [
        ("./reader", false),
        ("./linkreader", false),
        ("reader", true),
    ] {
        assert_eq!(invoke(program, with_path)["decision"], "executed");
        assert_eq!(invoke(program, with_path)["decision"], "executed");
        assert_eq!(invoke(program, with_path)["decision"], "cached");
    }
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires a pinned tracer and a static helper"]
fn path_program_key_uses_the_resolved_executable() {
    let helper = std::env::var_os("KIOKU_TRACE_TEST_HELPER")
        .expect("KIOKU_TRACE_TEST_HELPER must name a static helper");
    let tracer = std::env::var_os("KIOKU_STRACE").expect("KIOKU_STRACE must name the tracer");
    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let repo = Repo::new();
    repo.write("input", "x\n");
    repo.commit();
    let first = repo.path().join("first");
    let second = repo.path().join("second");
    std::fs::create_dir_all(first.join("bin")).unwrap();
    std::fs::create_dir_all(second.join("bin")).unwrap();
    for dir in [&first, &second] {
        let target = dir.join("bin/reader");
        std::fs::copy(&helper, &target).unwrap();
        let mut mode = std::fs::metadata(&target).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o755);
        std::fs::set_permissions(&target, mode).unwrap();
    }
    let key = |bin: &Path| {
        let mut path = bin.as_os_str().to_owned();
        path.push(":");
        path.push(&original_path);
        let output = repo
            .run()
            .env("KIOKU_STRACE", &tracer)
            .env("PATH", path)
            .args([
                "explain",
                "--json",
                "--trust-trace",
                "--",
                "reader",
                "input",
            ])
            .output();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        event["key"].as_str().unwrap().to_owned()
    };
    assert_ne!(key(&first.join("bin")), key(&second.join("bin")));
}

/// A file that cannot be executed is not a program.
///
/// The search of the path must skip such a file, exactly like a shell does.
/// The resolved program, and therefore the command key, must not depend on a
/// regular file that happens to sit earlier in `PATH`.
#[cfg(unix)]
#[test]
fn a_non_executable_file_earlier_in_path_is_not_the_program() {
    let repo = Repo::new();
    repo.write("tracked", "x\n");
    repo.commit();
    let decoy = repo.path().join("decoy");
    let real = repo.path().join("real");
    std::fs::create_dir_all(&decoy).unwrap();
    std::fs::create_dir_all(&real).unwrap();
    // The decoy is a regular file that cannot be executed.
    std::fs::write(decoy.join("reader"), "not a program\n").unwrap();
    // The program is executable.
    std::fs::write(real.join("reader"), "#!/bin/sh\nexit 0\n").unwrap();
    let mut mode = std::fs::metadata(real.join("reader"))
        .unwrap()
        .permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o755);
    std::fs::set_permissions(real.join("reader"), mode).unwrap();

    let key = |path: &OsStr| {
        let output = repo
            .run()
            .env("PATH", path)
            .env("KIOKU_DISABLE", "0")
            .args(["explain", "--json", "--", "reader"])
            .output();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        event["key"].as_str().expect("a key").to_owned()
    };

    // The decoy comes first, the program comes second: the search must skip
    // the decoy and reach the program.
    let mut decoy_first = decoy.as_os_str().to_owned();
    decoy_first.push(":");
    decoy_first.push(&real);
    decoy_first.push(":");
    decoy_first.push(std::env::var_os("PATH").unwrap_or_default());
    let mut program_only = real.as_os_str().to_owned();
    program_only.push(":");
    program_only.push(std::env::var_os("PATH").unwrap_or_default());

    assert_eq!(
        key(&decoy_first),
        key(&program_only),
        "the program must not be a file that cannot be executed"
    );
}

/// A program given by an explicit path that cannot be executed is not a
/// program.
///
/// The trace policy must refuse it instead of trying to spawn it, so the
/// explanation names the unresolved program.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires the pinned tracer"]
fn an_explicit_program_that_cannot_be_executed_is_not_resolved() {
    let tracer = std::env::var_os("KIOKU_STRACE").expect("KIOKU_STRACE must name the tracer");
    let repo = Repo::new();
    repo.write("tracked", "x\n");
    repo.commit();
    let tool = repo.path().join("tool");
    std::fs::write(&tool, "not a program\n").unwrap();
    let mut mode = std::fs::metadata(&tool).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o644);
    std::fs::set_permissions(&tool, mode).unwrap();

    let output = repo
        .run()
        .env("KIOKU_STRACE", &tracer)
        .env("KIOKU_DISABLE", "0")
        .args(["explain", "--json", "--trust-trace", "--", "./tool"])
        .output();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        event["reason"], "program-unresolved",
        "the explanation must refuse a program that cannot be executed: {event}"
    );
    assert_eq!(
        event["denial_reason"], "program-unresolved",
        "the denial must name the unresolved program: {event}"
    );
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[test]
fn traced_realtime_signal_exit_is_reported_without_child_retry() {
    let repo = Repo::new();
    repo.write("tracked", "x\n");
    repo.commit();
    let marker = repo.path().join("marker");
    let script = format!("echo x >> {}; kill -RTMIN $$", marker.display());
    let output = repo
        .run()
        .env("KIOKU_STRACE", tracer_path())
        .args(["verify", "--json", "--trust-trace", "--"])
        .args([support::executable("bash")])
        .args(["-c", &script])
        .code(162);
    let event: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(event["exit_code"], 162);
    assert_eq!(event["actual_exit"], 162);
    assert_eq!(event["cacheable"], false);
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "x\n");
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[test]
fn traced_signal_exit_is_reported_without_child_retry() {
    let repo = Repo::new();
    repo.write("tracked", "x\n");
    repo.commit();
    let marker = repo.path().join("marker");
    let script = format!("echo x >> {}; kill -TERM $$", marker.display());
    let shell = support::executable("sh");
    let output = repo
        .run()
        .env("KIOKU_STRACE", tracer_path())
        .args(["verify", "--json", "--trust-trace", "--"])
        .args([shell.to_str().unwrap(), "-c", &script])
        .code(143);
    let event: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(event["exit_code"], 143);
    assert_eq!(event["actual_exit"], 143);
    assert_eq!(event["cacheable"], false);
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "x\n");
}
#[cfg(unix)]
#[test]
fn verify_reports_the_actual_exit_and_no_default_hit() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    repo.run()
        .args(["run", "--", "sh", "-c", "exit 0"])
        .success();
    let output = repo
        .run()
        .args(["verify", "--json", "--", "sh", "-c", "exit 0"])
        .success();
    let event: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(event["would_skip"], false);
    assert_eq!(event["actual_exit"], 0);
    assert_eq!(event["decision"], "executed");
}
#[cfg(unix)]
#[test]
fn verify_exposes_a_false_green_and_invalidates_the_pass() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    let marker = TempDir::new().expect("a temporary directory");
    let path = marker.path().join("marker");
    std::fs::write(&path, "here\n").expect("the marker");

    repo.run()
        .args([
            "run",
            "--",
            "sh",
            "-c",
            &format!("test -f {}", path.display()),
        ])
        .success();
    std::fs::remove_file(&path).expect("the marker must be removed");

    repo.run()
        .args([
            "verify",
            "--",
            "sh",
            "-c",
            &format!("test -f {}", path.display()),
        ])
        .code(1);

    let store = store_dir(&repo);
    assert!(
        entry_files(&store).is_empty(),
        "the false green must invalidate the cached pass"
    );
}

#[cfg(unix)]
#[test]
fn status_and_clean_answer_in_json() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    repo.run()
        .args(["run", "--", "sh", "-c", "exit 0"])
        .success();

    let status = repo.run().args(["status", "--json"]).output();
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        stdout.contains("\"schema\":\"kioku/v1\""),
        "stdout was: {stdout}"
    );
    assert!(
        stdout.contains("\"entries\":0"),
        "default execution must not create a reusable entry: {stdout}"
    );

    let clean = repo
        .run()
        .args(["clean", "--older-than", "7d", "--json"])
        .output();
    assert!(clean.status.success());
    let stdout = String::from_utf8_lossy(&clean.stdout);
    assert!(
        stdout.contains("\"schema\":\"kioku/v1\""),
        "stdout was: {stdout}"
    );
}

/// Parses `kioku status --json`.
fn status_document(repo: &Repo) -> serde_json::Value {
    let output = repo.run().args(["status", "--json"]).output();
    assert!(
        output.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("status must print JSON")
}

#[test]
fn counters_start_at_zero_in_an_empty_store() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    let counters = status_document(&repo)["counters"].clone();
    assert_eq!(counters["schema"], "kioku/counters/v1");
    for field in [
        "hits",
        "measured_hits",
        "misses",
        "executions",
        "bypassed",
        "saved_ms",
        "invalidated",
    ] {
        assert_eq!(counters[field], 0, "{field} must start at zero: {counters}");
    }
    assert!(
        counters.get("legacy_saved_ms").is_none(),
        "a fresh store has no legacy saving: {counters}"
    );
}

#[test]
fn default_runs_are_counted_as_executions_and_bypasses() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    for _ in 0..2 {
        repo.run()
            .arg("run")
            .arg("--")
            .args(native_success())
            .success();
    }
    let counters = status_document(&repo)["counters"].clone();
    assert_eq!(counters["executions"], 2, "{counters}");
    assert_eq!(counters["bypassed"], 2, "{counters}");
    assert_eq!(counters["hits"], 0, "{counters}");
    assert_eq!(counters["misses"], 0, "{counters}");
}

#[test]
fn a_corrupt_counters_document_does_not_block_a_run() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    let store = store_dir(&repo);
    std::fs::create_dir_all(&store).expect("the store directory");
    std::fs::write(store.join("counters.json"), "not a counters document")
        .expect("the corrupt counters");

    let counters = status_document(&repo)["counters"].clone();
    assert_eq!(
        counters["executions"], 0,
        "a corrupt document must read as zero: {counters}"
    );

    repo.run()
        .arg("run")
        .arg("--")
        .args(native_success())
        .success();
    let counters = status_document(&repo)["counters"].clone();
    assert_eq!(
        counters["executions"], 1,
        "a corrupt document must not block new counts: {counters}"
    );
}

#[test]
fn an_old_counters_document_reports_unknown_savings() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    let store = store_dir(&repo);
    std::fs::create_dir_all(&store).expect("the store directory");
    std::fs::write(
        store.join("counters.json"),
        r#"{"schema":"kioku/counters/v1","hits":3,"misses":0,"executions":0,"bypassed":0,"saved_ms":0,"invalidated":0}"#,
    )
    .expect("an old counters document");

    let counters = status_document(&repo)["counters"].clone();
    assert_eq!(counters["hits"], 3, "{counters}");
    assert!(
        counters["measured_hits"].is_null(),
        "an old document has no measured count, and absence is explicit: {counters}"
    );

    let output = repo.run().arg("status").output();
    assert!(output.status.success());
    let human = String::from_utf8_lossy(&output.stdout);
    assert!(
        human.contains("3 hits (measurement unknown)"),
        "the measured count must be named as unknown: {human}"
    );
    assert!(
        human.contains("unknown (3 hits counted before per-hit measurement)"),
        "a missing measurement is unknown, never zero: {human}"
    );
    assert!(
        !human.contains("0 measured"),
        "an old document must not claim a measured zero: {human}"
    );
}

#[test]
fn an_older_saved_total_is_never_attributed_to_measured_hits() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    let store = store_dir(&repo);
    std::fs::create_dir_all(&store).expect("the store directory");
    std::fs::write(
        store.join("counters.json"),
        r#"{"schema":"kioku/counters/v1","hits":3,"misses":0,"executions":0,"bypassed":0,"saved_ms":500,"invalidated":0}"#,
    )
    .expect("an old counters document");

    let counters = status_document(&repo)["counters"].clone();
    assert_eq!(counters["hits"], 3, "{counters}");
    assert!(
        counters["measured_hits"].is_null(),
        "an old document has no measured count: {counters}"
    );
    assert_eq!(
        counters["saved_ms"], 500,
        "the total is still readable: {counters}"
    );

    let output = repo.run().arg("status").output();
    assert!(output.status.success());
    let human = String::from_utf8_lossy(&output.stdout);
    assert!(
        human.contains("500 ms recorded by an older version (not attributed to hits)"),
        "an old total must stay unattributed: {human}"
    );
    assert!(
        !human.contains("over 3 measured"),
        "an old total is not a measured saving: {human}"
    );

    // Any later run rewrites the counters document. The old total must move to
    // the legacy bucket once, not be erased and not become a measured saving.
    for _ in 0..2 {
        repo.run()
            .arg("run")
            .arg("--")
            .args(native_success())
            .success();
    }
    let counters = status_document(&repo)["counters"].clone();
    assert_eq!(counters["hits"], 3, "{counters}");
    assert_eq!(counters["measured_hits"], 0, "{counters}");
    assert_eq!(counters["saved_ms"], 0, "{counters}");
    assert_eq!(
        counters["legacy_saved_ms"], 500,
        "the old total survives, once: {counters}"
    );

    let output = repo.run().arg("status").output();
    assert!(output.status.success());
    let human = String::from_utf8_lossy(&output.stdout);
    assert!(
        human.contains("500 ms recorded by an older version"),
        "the migrated total must still be shown: {human}"
    );
    assert!(
        !human.contains("over 0 measured"),
        "a migrated total is never attributed to zero measured hits: {human}"
    );
}

#[test]
fn an_incoherent_old_total_with_zero_hits_is_kept_as_legacy() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    let store = store_dir(&repo);
    std::fs::create_dir_all(&store).expect("the store directory");
    std::fs::write(
        store.join("counters.json"),
        r#"{"schema":"kioku/counters/v1","hits":0,"misses":0,"executions":0,"bypassed":0,"saved_ms":500,"invalidated":0}"#,
    )
    .expect("an old counters document");

    let output = repo.run().arg("status").output();
    assert!(output.status.success());
    let human = String::from_utf8_lossy(&output.stdout);
    assert!(
        human.contains("500 ms recorded by an older version"),
        "a total without hits is still a recorded value: {human}"
    );
    assert!(
        !human.contains("no hit yet"),
        "a store with a recorded total is not empty: {human}"
    );

    repo.run()
        .arg("run")
        .arg("--")
        .args(native_success())
        .success();
    let counters = status_document(&repo)["counters"].clone();
    assert_eq!(counters["hits"], 0, "{counters}");
    assert_eq!(counters["measured_hits"], 0, "{counters}");
    assert_eq!(counters["legacy_saved_ms"], 500, "{counters}");
}

#[test]
fn status_without_any_hit_claims_no_saving() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();

    let output = repo.run().arg("status").output();
    assert!(output.status.success());
    let human = String::from_utf8_lossy(&output.stdout);
    assert!(
        human.contains("no hit yet"),
        "an empty store must say so: {human}"
    );
    assert!(
        !human.contains("(measured"),
        "no hit means nothing was measured: {human}"
    );
}

#[test]
fn an_incoherent_measured_count_is_reported_as_unknown() {
    // A measured count above the hit count cannot be true. The document does
    // not say how many hits were measured, so the measurement is unknown:
    // kioku does not invent a smaller number.
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    let store = store_dir(&repo);
    std::fs::create_dir_all(&store).expect("the store directory");
    std::fs::write(
        store.join("counters.json"),
        r#"{"schema":"kioku/counters/v1","hits":2,"measured_hits":5,"misses":0,"executions":0,"bypassed":0,"saved_ms":100,"invalidated":0}"#,
    )
    .expect("the counters document");

    let counters = status_document(&repo)["counters"].clone();
    assert_eq!(counters["hits"], 2, "{counters}");
    assert!(
        counters["measured_hits"].is_null(),
        "an incoherent measured count is unknown: {counters}"
    );

    let output = repo.run().arg("status").output();
    assert!(output.status.success());
    let human = String::from_utf8_lossy(&output.stdout);
    assert!(
        human.contains("2 hits (measurement unknown)"),
        "the human count must not invent a measurement: {human}"
    );
}

#[test]
fn a_legacy_counters_document_without_hits_names_no_measurement() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    let store = store_dir(&repo);
    std::fs::create_dir_all(&store).expect("the store directory");
    std::fs::write(
        store.join("counters.json"),
        r#"{"schema":"kioku/counters/v1","hits":0,"misses":0,"executions":0,"bypassed":0,"saved_ms":0,"invalidated":0}"#,
    )
    .expect("an old counters document");

    let output = repo.run().arg("status").output();
    assert!(output.status.success());
    let human = String::from_utf8_lossy(&output.stdout);
    assert!(
        !human.contains("measurement unknown"),
        "no hit means no measurement: {human}"
    );
    assert!(
        human.contains("0 hits (none)"),
        "a document without hits must say so: {human}"
    );
}

#[test]
fn clean_all_resets_the_counters() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    repo.run()
        .arg("run")
        .arg("--")
        .args(native_success())
        .success();
    assert_eq!(status_document(&repo)["counters"]["executions"], 1);

    repo.run().args(["clean", "--all"]).success();
    let counters = status_document(&repo)["counters"].clone();
    for field in [
        "hits",
        "measured_hits",
        "misses",
        "executions",
        "bypassed",
        "saved_ms",
        "invalidated",
    ] {
        assert_eq!(counters[field], 0, "clean must reset {field}: {counters}");
    }
    assert!(
        counters.get("legacy_saved_ms").is_none(),
        "clean must reset the legacy saving: {counters}"
    );
}

/// Builds the static reader helper inside the repository.
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
fn static_reader(repo: &Repo) -> PathBuf {
    let helper = repo.path().join("reader");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/false_green_zoo/read_aarch64.S");
    assert!(
        Command::new(support::executable("cc"))
            .args(["-nostdlib", "-static", "-o"])
            .arg(&helper)
            .arg(source)
            .status()
            .expect("the C compiler must run")
            .success()
    );
    helper
}

/// Runs a traced `run` or `verify` and returns its status and JSON event.
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
fn traced_event(
    repo: &Repo,
    verb: &str,
    program: &Path,
    input: &Path,
) -> (std::process::ExitStatus, serde_json::Value) {
    let output = repo
        .run()
        .env("KIOKU_STRACE", tracer_path())
        .args([verb, "--json", "--trust-trace", "--"])
        .arg(program)
        .arg(input)
        .output();
    let event = serde_json::from_slice(&output.stderr).unwrap_or_else(|_| {
        panic!(
            "missing JSON from {verb}: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output.status, event)
}

/// Runs a traced `run` or `verify` against a chosen store and returns its
/// status and JSON event.
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
fn traced_event_in(
    repo: &Repo,
    store: &Path,
    verb: &str,
    program: &Path,
    input: &Path,
) -> (std::process::ExitStatus, serde_json::Value) {
    let output = repo
        .run()
        .env("KIOKU_STRACE", tracer_path())
        .env("KIOKU_DIR", store)
        .args([verb, "--json", "--trust-trace", "--"])
        .arg(program)
        .arg(input)
        .output();
    let event = serde_json::from_slice(&output.stderr).unwrap_or_else(|_| {
        panic!(
            "missing JSON from {verb}: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output.status, event)
}

/// Paths of the recorded learning durations of the trace states.
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
fn timing_paths(repo: &Repo) -> Vec<PathBuf> {
    let trace_dir = repo.path().join(".git/kioku/trace-v2");
    std::fs::read_dir(&trace_dir)
        .expect("the trace directory")
        .map(|item| item.expect("a trace file").path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("timing-"))
        })
        .collect()
}

/// Removes the observed record of one input from the single trace state, then
/// re-authenticates it, so a later run believes the input is unchanged.
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
fn poison_input_record(repo: &Repo, input: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let trace_dir = repo.path().join(".git/kioku/trace-v2");
    let state_path = std::fs::read_dir(&trace_dir)
        .expect("the trace directory")
        .map(|item| item.expect("a trace file").path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("state-"))
        })
        .expect("a learned trace state");
    let mut state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state_path).expect("the state"))
            .expect("state JSON");
    let records = state["manifest"]["records"]
        .as_array_mut()
        .expect("manifest records");
    let bytes = input.as_os_str().as_bytes();
    let before = records.len();
    records.retain(|record| {
        record["path"].as_array().is_none_or(|path| {
            path.iter()
                .filter_map(serde_json::Value::as_u64)
                .map(|b| u8::try_from(b).expect("a byte"))
                .collect::<Vec<_>>()
                != bytes
        })
    });
    assert_eq!(records.len() + 1, before, "the input must be observed");
    state
        .as_object_mut()
        .expect("the state object")
        .remove("authentication");
    let decoded: kioku::policy::State =
        serde_json::from_value(state.clone()).expect("the policy state");
    let encoded = serde_json::to_vec(&decoded).expect("the encoded state");
    let key: [u8; 32] = std::fs::read(trace_dir.join("key"))
        .expect("the store key")
        .try_into()
        .expect("a 32-byte key");
    state["authentication"] = blake3::keyed_hash(&key, &encoded)
        .to_hex()
        .to_string()
        .into();
    std::fs::write(
        &state_path,
        serde_json::to_vec(&state).expect("the state JSON"),
    )
    .expect("the poisoned state");
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[test]
#[ignore = "requires pinned tracer"]
fn traced_counters_separate_hits_misses_and_executions() {
    let repo = Repo::new();
    repo.write("input", "x\n");
    let reader = static_reader(&repo);
    repo.commit();
    let input = repo.path().join("input");

    for _ in 0..2 {
        let (status, event) = traced_event(&repo, "run", &reader, &input);
        assert!(status.success());
        assert_eq!(event["decision"], "executed", "{event}");
    }
    let counters = status_document(&repo)["counters"].clone();
    assert_eq!(counters["misses"], 2, "{counters}");
    assert_eq!(counters["executions"], 2, "{counters}");
    assert_eq!(counters["hits"], 0, "{counters}");

    let (status, hit) = traced_event(&repo, "run", &reader, &input);
    assert!(status.success());
    assert_eq!(hit["decision"], "cached", "{hit}");
    let saved = hit["saved_ms"]
        .as_u64()
        .unwrap_or_else(|| panic!("a hit must measure its saving: {hit}"));
    assert!(saved > 0, "the learning run was slower than the hit: {hit}");
    let counters = status_document(&repo)["counters"].clone();
    assert_eq!(counters["hits"], 1, "{counters}");
    assert_eq!(
        counters["measured_hits"], 1,
        "the hit had a recorded duration: {counters}"
    );
    assert_eq!(counters["misses"], 2, "{counters}");
    assert_eq!(counters["executions"], 2, "{counters}");
    assert_eq!(counters["saved_ms"].as_u64(), Some(saved));

    std::fs::write(&input, "changed\n").expect("the input must change");
    let (status, miss) = traced_event(&repo, "run", &reader, &input);
    assert!(status.success());
    assert_eq!(miss["decision"], "executed", "{miss}");
    let counters = status_document(&repo)["counters"].clone();
    assert_eq!(counters["misses"], 3, "{counters}");
    assert_eq!(counters["executions"], 3, "{counters}");
    assert_eq!(counters["hits"], 1, "{counters}");
}

/// A store in the work tree turns the cache off, traced runs included.
///
/// The files of the store are not part of the key, so a tracked file that
/// happens to live there would be left out of it too. The cache must not
/// reuse a pass learned in that store, and must not learn one either.
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[test]
#[ignore = "requires pinned tracer"]
fn a_traced_store_in_the_work_tree_never_reuses() {
    let repo = Repo::new();
    repo.write("input", "x\n");
    let reader = static_reader(&repo);
    repo.commit();
    let input = repo.path().join("input");
    let store = repo.path().join("cache");

    for _ in 0..4 {
        let (status, event) = traced_event_in(&repo, &store, "run", &reader, &input);
        assert!(status.success());
        assert_eq!(event["decision"], "executed", "{event}");
        assert_eq!(event["reason"], "store-in-work-tree", "{event}");
        assert_eq!(event["would_skip"], false, "{event}");
    }
    assert!(
        !store.exists(),
        "kioku must not create a store in the work tree"
    );

    // The status reads the empty store and creates nothing either.
    let output = repo
        .run()
        .env("KIOKU_DIR", &store)
        .args(["status", "--json"])
        .output();
    assert!(output.status.success());
    let event: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("status must print JSON");
    assert_eq!(event["counters"]["hits"], 0, "{event}");
    assert_eq!(event["counters"]["executions"], 0, "{event}");
    assert!(
        !store.exists(),
        "kioku must not create a store in the work tree"
    );
}

/// The control: a store outside the work tree still reuses a traced pass.
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[test]
#[ignore = "requires pinned tracer"]
fn a_traced_store_outside_the_work_tree_still_reuses() {
    let repo = Repo::new();
    repo.write("input", "x\n");
    let reader = static_reader(&repo);
    repo.commit();
    let input = repo.path().join("input");
    let outside = TempDir::new().expect("a temporary store");
    let store = outside.path().join("store");

    for _ in 0..2 {
        let (status, event) = traced_event_in(&repo, &store, "run", &reader, &input);
        assert!(status.success());
        assert_eq!(event["decision"], "executed", "{event}");
    }
    let (status, hit) = traced_event_in(&repo, &store, "run", &reader, &input);
    assert!(status.success());
    assert_eq!(hit["decision"], "cached", "{hit}");
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[test]
#[ignore = "requires pinned tracer"]
fn a_hit_without_a_recorded_duration_reports_an_unknown_saving() {
    let repo = Repo::new();
    repo.write("input", "x\n");
    let reader = static_reader(&repo);
    repo.commit();
    let input = repo.path().join("input");
    for _ in 0..2 {
        assert!(traced_event(&repo, "run", &reader, &input).0.success());
    }
    let (status, hit) = traced_event(&repo, "run", &reader, &input);
    assert!(status.success());
    assert_eq!(hit["decision"], "cached", "{hit}");
    let recorded_saving = hit["saved_ms"].as_u64().expect("a measured saving");

    let timings = timing_paths(&repo);
    assert!(
        !timings.is_empty(),
        "a learned pass must record the duration of its original run"
    );
    for timing in &timings {
        std::fs::remove_file(timing).expect("the timing file");
    }

    let (status, hit) = traced_event(&repo, "run", &reader, &input);
    assert!(status.success());
    assert_eq!(hit["decision"], "cached", "{hit}");
    assert!(
        hit["saved_ms"].is_null(),
        "a missing duration is unknown, never zero: {hit}"
    );
    let counters = status_document(&repo)["counters"].clone();
    assert_eq!(counters["hits"], 2, "{counters}");
    assert_eq!(
        counters["measured_hits"], 1,
        "only the first hit was measurable: {counters}"
    );
    assert_eq!(counters["saved_ms"].as_u64(), Some(recorded_saving));

    let output = repo.run().arg("status").output();
    assert!(output.status.success());
    let human = String::from_utf8_lossy(&output.stdout);
    assert!(
        human.contains("2 hits (1 measured)"),
        "status must separate measured and unknown hits: {human}"
    );
    assert!(
        human.contains("; 1 hit without a recorded duration"),
        "status must name the unknown hit: {human}"
    );
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[test]
#[ignore = "requires pinned tracer"]
fn a_passing_verify_keeps_the_learned_duration() {
    let repo = Repo::new();
    repo.write("input", "x\n");
    let reader = static_reader(&repo);
    repo.commit();
    let input = repo.path().join("input");
    for _ in 0..2 {
        assert!(traced_event(&repo, "run", &reader, &input).0.success());
    }
    let timings = timing_paths(&repo);
    assert_eq!(timings.len(), 1, "one learned pass has one duration");
    let before = std::fs::read(&timings[0]).expect("the learned duration");

    let (status, event) = traced_event(&repo, "verify", &reader, &input);
    assert!(status.success());
    assert_eq!(event["exit_code"], 0, "{event}");
    assert_eq!(event["would_skip"], true, "{event}");
    let after = std::fs::read(&timings[0]).expect("the learned duration");
    assert_eq!(
        after, before,
        "a passing verify must not rewrite the learned duration"
    );
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[test]
#[ignore = "requires pinned tracer"]
fn a_new_state_replaces_the_stale_learned_duration() {
    let repo = Repo::new();
    repo.write("input", "x\n");
    let reader = static_reader(&repo);
    repo.commit();
    let input = repo.path().join("input");
    for _ in 0..2 {
        assert!(traced_event(&repo, "run", &reader, &input).0.success());
    }
    let timings = timing_paths(&repo);
    assert_eq!(timings.len(), 1, "one learned pass has one duration");
    // A sentinel stands for a duration no run of this test can produce: the
    // store must replace it as soon as the run learns a different state.
    std::fs::write(&timings[0], "424242").expect("the sentinel duration");

    std::fs::write(&input, "y\n").expect("the input must change");
    for _ in 0..2 {
        let (status, event) = traced_event(&repo, "run", &reader, &input);
        assert!(status.success());
        assert_eq!(event["decision"], "executed", "{event}");
    }
    let recorded = std::fs::read_to_string(&timings[0]).expect("the new duration");
    assert_ne!(
        recorded.trim(),
        "424242",
        "a new state must not keep the duration of the state it replaced"
    );

    let (status, hit) = traced_event(&repo, "run", &reader, &input);
    assert!(status.success());
    assert_eq!(hit["decision"], "cached", "{hit}");
    let saved = hit["saved_ms"]
        .as_u64()
        .unwrap_or_else(|| panic!("a hit must measure its saving: {hit}"));
    assert!(
        saved < 424_242,
        "a hit must not report the duration of a replaced state: {hit}"
    );
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[test]
#[ignore = "requires pinned tracer"]
fn human_labels_distinguish_measured_and_unknown_hits() {
    let repo = Repo::new();
    repo.write("input", "x\n");
    let reader = static_reader(&repo);
    repo.commit();
    let input = repo.path().join("input");
    for _ in 0..2 {
        assert!(traced_event(&repo, "run", &reader, &input).0.success());
    }

    let output = repo
        .run()
        .env("KIOKU_STRACE", tracer_path())
        .args(["run", "--trust-trace", "--"])
        .arg(&reader)
        .arg(&input)
        .output();
    assert!(output.status.success());
    let human = String::from_utf8_lossy(&output.stderr);
    assert!(
        human.contains("cached (pass, ") && !human.contains("unavailable"),
        "a measured hit must name its saving: {human}"
    );

    let output = repo.run().arg("status").output();
    assert!(output.status.success());
    let human = String::from_utf8_lossy(&output.stdout);
    assert!(
        human.contains("1 measured hit (includes tracing overhead)"),
        "the all-measured label must stay singular and honest: {human}"
    );
    assert!(
        !human.contains("without a recorded duration"),
        "no hit is unknown yet: {human}"
    );

    for timing in timing_paths(&repo) {
        std::fs::remove_file(timing).expect("the timing file");
    }
    let output = repo
        .run()
        .env("KIOKU_STRACE", tracer_path())
        .args(["run", "--trust-trace", "--"])
        .arg(&reader)
        .arg(&input)
        .output();
    assert!(output.status.success());
    let human = String::from_utf8_lossy(&output.stderr);
    assert!(
        human.contains("cached (pass, saved time unavailable)"),
        "a missing duration must be labelled unknown: {human}"
    );

    let unknown = repo
        .run()
        .env("KIOKU_STRACE", tracer_path())
        .args(["run", "--trust-trace", "--"])
        .arg(&reader)
        .arg(&input)
        .output();
    assert!(unknown.status.success());
    let output = repo.run().arg("status").output();
    assert!(output.status.success());
    let human = String::from_utf8_lossy(&output.stdout);
    assert!(
        human.contains("3 hits (1 measured)"),
        "status must count two unknown hits: {human}"
    );
    assert!(
        human.contains("2 hits without a recorded duration"),
        "two unknown hits take the plural: {human}"
    );
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[test]
#[ignore = "requires pinned tracer"]
fn a_false_green_counts_as_an_invalidation() {
    let repo = Repo::new();
    repo.write("input", "x\n");
    let reader = static_reader(&repo);
    repo.commit();
    let input = repo.path().join("input");
    for _ in 0..2 {
        assert!(traced_event(&repo, "run", &reader, &input).0.success());
    }
    let (status, hit) = traced_event(&repo, "run", &reader, &input);
    assert!(status.success());
    assert_eq!(hit["decision"], "cached", "{hit}");

    poison_input_record(&repo, &input);
    std::fs::remove_file(&input).expect("the unobserved input must disappear");

    let (status, event) = traced_event(&repo, "verify", &reader, &input);
    assert_eq!(status.code(), Some(1));
    assert_eq!(event["false_green"], true, "{event}");
    assert_eq!(event["invalidated"], 1, "{event}");
    let counters = status_document(&repo)["counters"].clone();
    assert_eq!(counters["invalidated"], 1, "{counters}");
    assert_eq!(counters["executions"], 3, "{counters}");
    assert_eq!(counters["misses"], 3, "{counters}");
    assert_eq!(counters["hits"], 1, "{counters}");
}

#[test]
fn clean_rejects_a_broken_duration() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();

    repo.run()
        .args(["clean", "--older-than", "seven days"])
        .code(2);
}

#[cfg(unix)]
#[test]
fn explain_after_change_reports_execute_only_policy() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();
    repo.run()
        .args(["run", "--", "sh", "-c", "exit 0"])
        .success();
    repo.write("main.txt", "changed\n");

    let output = repo
        .run()
        .args(["explain", "--json", "--", "sh", "-c", "exit 0"])
        .output();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("\"hit\":false"), "stdout was: {stdout}");
    let event: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(event["policy"], "execute-only");
    assert_eq!(event["traced"], false);
    assert_eq!(
        event["key"].as_str().map(str::len),
        Some(64),
        "explain must always show the command key: {event}"
    );
}

#[cfg(unix)]
#[test]
fn json_does_not_echo_command_arguments() {
    let program = support::true_program();
    let program = program.to_str().unwrap();
    let repo = Repo::new();
    let secret = "synthetic-sensitive-argv";
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_kioku"))
        .current_dir(repo.path())
        .args(["run", "--json", "--", program, secret])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
}

#[test]
fn explain_reports_requested_trace_contract_and_effective_bytes() {
    let program = support::true_program();
    let program = program.to_str().unwrap();
    let repo = Repo::new();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_kioku"))
        .current_dir(repo.path())
        .args([
            "explain",
            "--json",
            "--trust-trace",
            "--mode",
            "ast",
            "--",
            program,
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(event["policy"], "trace-v2");
    assert_eq!(event["contract"], "caller-asserted-stable-inputs");
    assert_eq!(event["mode_requested"], "ast");
    assert_eq!(event["mode_effective"], "bytes");
    assert_eq!(event["output_replayed"], false);
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end quarantine lifecycle fixture"
)]
#[ignore = "requires pinned tracer"]
fn traced_sampling_false_green_quarantine_clean_and_reset() {
    sampling_false_green_lifecycle(None);
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[test]
#[ignore = "requires pinned tracer"]
fn traced_sampling_signal_quarantines_without_retry() {
    sampling_false_green_lifecycle(Some(15));
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[test]
#[ignore = "requires pinned tracer"]
fn traced_sampling_realtime_signal_quarantines_without_retry() {
    sampling_false_green_lifecycle(Some(34));
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end quarantine lifecycle fixture"
)]
fn sampling_false_green_lifecycle(signal: Option<i32>) {
    use std::os::unix::ffi::OsStrExt;
    use std::process::Stdio;
    let fixture = tempfile::tempdir().unwrap();
    let repo = fixture.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .arg(&repo)
            .status()
            .unwrap()
            .success()
    );
    let helper = fixture.path().join("reader");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/false_green_zoo/read_aarch64.S");
    let modified_source = fixture.path().join("signal-reader.S");
    let source = if let Some(signal) = signal {
        let code = std::fs::read_to_string(&source).unwrap().replace(
            "failure:\n    mov x0, 1\ndone:",
            &format!("failure:\n    mov x8, 172\n    svc 0\n    mov x1, {signal}\n    mov x8, 129\n    svc 0\n    mov x0, 1\ndone:"),
        );
        std::fs::write(&modified_source, code).unwrap();
        &modified_source
    } else {
        &source
    };
    assert!(
        Command::new(support::executable("cc"))
            .args(["-nostdlib", "-static", "-o"])
            .arg(&helper)
            .arg(source)
            .status()
            .unwrap()
            .success()
    );
    let tracer = tracer_path();
    assert!(tracer.exists(), "pinned tracer required for acceptance");
    let input = repo.join("input");
    std::fs::write(&input, b"input").unwrap();
    let invoke = |verb: &str, rate: &str, changed_env: bool| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_kioku"));
        cmd.current_dir(&repo)
            .stdin(Stdio::null())
            .env("KIOKU_STRACE", &tracer)
            .env(
                "TEST_QUARANTINE_ENV",
                if changed_env { "changed" } else { "initial" },
            )
            .args([verb, "--json", "--trust-trace", "--verify-rate", rate, "--"])
            .arg(&helper)
            .arg(&input);
        let out = cmd.output().unwrap();
        let value: serde_json::Value = serde_json::from_slice(&out.stderr)
            .unwrap_or_else(|_| panic!("missing JSON: {}", String::from_utf8_lossy(&out.stderr)));
        (out.status, value)
    };
    for _ in 0..2 {
        let (status, event) = invoke("run", "0", false);
        assert!(status.success());
        assert_eq!(event["decision"], "executed");
    }
    let (status, hit) = invoke("run", "0", false);
    assert!(status.success());
    assert_eq!(hit["decision"], "cached");
    let (status, sampled) = invoke("run", "1", false);
    assert!(status.success());
    assert_eq!(sampled["decision"], "executed");
    assert_eq!(sampled["would_skip"], true);
    let explain = |trust: bool| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_kioku"));
        cmd.current_dir(&repo)
            .stdin(Stdio::null())
            .env("KIOKU_STRACE", &tracer)
            .env("TEST_QUARANTINE_ENV", "initial")
            .args(["explain", "--json"]);
        if trust {
            cmd.arg("--trust-trace");
        }
        let out = cmd.arg("--").arg(&helper).arg(&input).output().unwrap();
        assert!(out.status.success());
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()
    };
    assert_eq!(explain(true)["reason"], "trace-hit");
    assert_eq!(explain(true)["hit"], true);
    assert_eq!(explain(false)["hit"], false);
    for (extra, env_disable, reason) in [
        (true, false, "output-required"),
        (false, true, "cache-disabled"),
    ] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_kioku"));
        cmd.current_dir(&repo)
            .stdin(Stdio::null())
            .env("KIOKU_STRACE", &tracer)
            .env("TEST_QUARANTINE_ENV", "initial")
            .args(["explain", "--json", "--trust-trace"]);
        if extra {
            cmd.arg("--require-output");
        }
        if env_disable {
            cmd.env("KIOKU_DISABLE", "1");
        }
        let output = cmd.arg("--").arg(&helper).arg(&input).output().unwrap();
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["hit"], false);
        assert_eq!(value["reason"], reason);
    }
    std::fs::write(&input, b"changed").unwrap();
    let miss = explain(true);
    assert_eq!(miss["hit"], false);
    assert_eq!(miss["reason"], "dependency-changed");
    assert_ne!(miss["changed_dependencies"].as_array().unwrap().len(), 0);
    assert!(!miss.to_string().contains(input.to_str().unwrap()));
    std::fs::write(&input, b"input").unwrap();
    for _ in 0..2 {
        let (status, _) = invoke("run", "0", false);
        assert!(status.success());
    }
    assert_eq!(invoke("run", "0", false).1["decision"], "cached");
    let (_, verified) = invoke("verify", "0", false);
    assert_eq!(verified["would_skip"], true);
    let trace_dir = repo.join(".git/kioku/trace-v2");
    let journal = std::fs::read_to_string(trace_dir.join("shadow.jsonl")).unwrap();
    assert!(journal.contains("\"would_skip\":true"));
    assert!(!journal.contains("sk-test-SENTINEL"));
    let status_output = Command::new(env!("CARGO_BIN_EXE_kioku"))
        .current_dir(&repo)
        .args(["status", "--json"])
        .output()
        .unwrap();
    let status: serde_json::Value = serde_json::from_slice(&status_output.stdout).unwrap();
    assert_eq!(status["trace_states"], 1);
    assert_eq!(status["quarantines"], 0);
    let state_path = std::fs::read_dir(&trace_dir)
        .unwrap()
        .map(|v| v.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("state-")
        })
        .unwrap();
    let mut state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
    let records = state["manifest"]["records"].as_array_mut().unwrap();
    let bytes = input.as_os_str().as_bytes();
    let before = records.len();
    records.retain(|record| {
        record["path"].as_array().is_none_or(|path| {
            path.iter()
                .filter_map(serde_json::Value::as_u64)
                .map(|b| u8::try_from(b).unwrap())
                .collect::<Vec<_>>()
                != bytes
        })
    });
    assert_eq!(
        records.len() + 1,
        before,
        "poison must remove real observed input"
    );
    state.as_object_mut().unwrap().remove("authentication");
    let decoded: kioku::policy::State = serde_json::from_value(state.clone()).unwrap();
    let encoded = serde_json::to_vec(&decoded).unwrap();
    let key: [u8; 32] = std::fs::read(trace_dir.join("key"))
        .unwrap()
        .try_into()
        .unwrap();
    state["authentication"] = blake3::keyed_hash(&key, &encoded)
        .to_hex()
        .to_string()
        .into();
    std::fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
    std::fs::remove_file(&input).unwrap();
    let human = Command::new(env!("CARGO_BIN_EXE_kioku"))
        .current_dir(&repo)
        .stdin(Stdio::null())
        .env("KIOKU_STRACE", &tracer)
        .env("TEST_QUARANTINE_ENV", "initial")
        .args([
            "verify",
            "--trust-trace",
            "--",
            helper.to_str().unwrap(),
            input.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!human.status.success());
    let feedback = String::from_utf8_lossy(&human.stderr);
    assert!(feedback.contains("false green"));
    assert!(!feedback.contains("it passes"));
    let reset = Command::new(env!("CARGO_BIN_EXE_kioku"))
        .current_dir(&repo)
        .args(["quarantine", "reset", "--"])
        .arg(&helper)
        .arg(&input)
        .output()
        .unwrap();
    assert!(reset.status.success());
    std::fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
    let (status, false_green) = invoke("run", "1", false);
    assert!(!status.success());
    assert_eq!(false_green["false_green"], true);
    assert_eq!(false_green["reason"], "false-green-quarantined");
    if let Some(signal) = signal {
        assert_eq!(status.code(), Some(128 + signal));
        assert_eq!(false_green["exit_code"], 128 + signal);
    }
    std::fs::write(&input, b"input").unwrap();
    let (_, after_restart) = invoke("run", "0", true);
    assert_eq!(after_restart["decision"], "executed");
    assert_eq!(after_restart["reason"], "quarantined");
    let clean = Command::new(env!("CARGO_BIN_EXE_kioku"))
        .current_dir(&repo)
        .args(["clean", "--all"])
        .output()
        .unwrap();
    assert!(clean.status.success());
    assert!(
        !state_path.exists(),
        "clean must remove reusable trace state"
    );
    let (_, after_clean) = invoke("run", "0", false);
    assert_eq!(after_clean["reason"], "quarantined");
    let reset = Command::new(env!("CARGO_BIN_EXE_kioku"))
        .current_dir(&repo)
        .args(["quarantine", "reset", "--"])
        .arg(&helper)
        .arg(&input)
        .output()
        .unwrap();
    assert!(reset.status.success());
    let (_, after_reset) = invoke("run", "0", false);
    assert_eq!(after_reset["decision"], "executed");
    assert_ne!(after_reset["reason"], "quarantined");
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[test]
#[ignore = "requires pinned tracer"]
fn trace_refuses_non_utf8_repository_identity() {
    use std::os::unix::ffi::OsStringExt;
    use std::process::Stdio;
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp
        .path()
        .join(std::ffi::OsString::from_vec(b"repo-\xff".to_vec()));
    std::fs::create_dir(&repo).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .arg(&repo)
            .status()
            .unwrap()
            .success()
    );
    let tracer = tracer_path();
    let output = Command::new(env!("CARGO_BIN_EXE_kioku"))
        .current_dir(repo)
        .stdin(Stdio::null())
        .env("KIOKU_STRACE", tracer)
        .args(["run", "--json", "--trust-trace", "--"])
        .arg(support::true_program())
        .output()
        .unwrap();
    assert!(output.status.success());
    let event: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(event["reason"], "trace-unsupported-path");
    assert_eq!(event["decision"], "executed");
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[test]
#[ignore = "requires pinned tracer"]
fn piped_stdin_never_reuses_an_eligible_trace() {
    let fixture = tempfile::tempdir().unwrap();
    let repo = fixture.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .arg(&repo)
            .status()
            .unwrap()
            .success()
    );
    let helper = fixture.path().join("reader");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/false_green_zoo/read_aarch64.S");
    assert!(
        Command::new(support::executable("cc"))
            .args(["-nostdlib", "-static", "-o"])
            .arg(&helper)
            .arg(source)
            .status()
            .unwrap()
            .success()
    );
    let input = repo.join("input");
    std::fs::write(&input, b"input").unwrap();
    let tracer = tracer_path();
    let invoke = |mode: u8| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_kioku"));
        cmd.current_dir(&repo)
            .env("KIOKU_STRACE", &tracer)
            .args(["run", "--json", "--trust-trace", "--"])
            .arg(&helper)
            .arg(&input)
            .stdin(match mode {
                1 => std::process::Stdio::piped(),
                2 => std::process::Stdio::from(std::fs::File::open("/dev/zero").unwrap()),
                _ => std::process::Stdio::null(),
            })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        if mode == 1 {
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"input via pipe")
                .unwrap();
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        serde_json::from_slice::<serde_json::Value>(&output.stderr).unwrap()
    };
    assert_eq!(invoke(0)["reason"], "trace-learning");
    assert_eq!(invoke(0)["reason"], "trace-learning");
    assert_eq!(invoke(0)["decision"], "cached");
    let piped = invoke(1);
    assert_eq!(piped["decision"], "executed");
    assert_eq!(piped["reason"], "stdin-unsupported");
    assert_eq!(piped["denial_reason"], "stdin");
    assert_eq!(piped["cacheable"], false);
    let zero = invoke(2);
    assert_eq!(zero["decision"], "executed");
    assert_eq!(zero["reason"], "stdin-unsupported");
    assert_eq!(zero["denial_reason"], "stdin");
    assert_eq!(zero["cacheable"], false);
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[test]
#[ignore = "requires pinned tracer"]
fn learned_commands_keep_distinct_identities_and_manifest_evidence() {
    let fixture = tempfile::tempdir().unwrap();
    let repo = fixture.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .arg(&repo)
            .status()
            .unwrap()
            .success()
    );
    let helper = fixture.path().join("reader");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/false_green_zoo/read_aarch64.S");
    assert!(
        Command::new(support::executable("cc"))
            .args(["-nostdlib", "-static", "-o"])
            .arg(&helper)
            .arg(source)
            .status()
            .unwrap()
            .success()
    );
    let input = repo.join("input");
    std::fs::write(&input, b"input").unwrap();
    std::fs::hard_link(&input, repo.join("alias")).unwrap();
    let tracer = tracer_path();
    let invoke = |arg: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_kioku"))
            .current_dir(&repo)
            .stdin(std::process::Stdio::null())
            .env("KIOKU_STRACE", &tracer)
            .args(["run", "--json", "--trust-trace", "--"])
            .arg(&helper)
            .arg(if arg == "a" {
                input.clone()
            } else {
                repo.join("alias")
            })
            .output()
            .unwrap();
        assert!(output.status.success());
        serde_json::from_slice::<serde_json::Value>(&output.stderr).unwrap()
    };
    let first = invoke("a");
    let second = invoke("a");
    let hit = invoke("a");
    assert_eq!(first["stored"], true);
    assert_eq!(first["cacheable"], false);
    assert_eq!(second["stored"], true);
    assert_eq!(second["cacheable"], true);
    assert_eq!(hit["cacheable"], true);
    assert_eq!(hit["decision"], "cached");
    for event in [&first, &second, &hit] {
        assert!(event["manifest_digest"].as_str().is_some_and(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }));
    }
    assert_eq!(first["manifest_digest"], second["manifest_digest"]);
    assert_eq!(second["manifest_digest"], hit["manifest_digest"]);
    let other = invoke("b");
    assert_eq!(other["decision"], "executed");
    assert_eq!(other["reason"], "trace-learning");
    assert_ne!(other["key"], hit["key"]);
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[test]
#[ignore = "requires pinned tracer"]
fn concurrent_clean_cannot_be_overwritten_by_late_publication() {
    let fixture = tempfile::tempdir().unwrap();
    let repo = fixture.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let helper = fixture.path().join("reader");
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    assert!(
        Command::new(support::executable("cc"))
            .args(["-nostdlib", "-static", "-o"])
            .arg(&helper)
            .arg(fixtures.join("publication_optional_open_aarch64.S"))
            .status()
            .unwrap()
            .success()
    );
    let output = Command::new(support::executable("python3"))
        .arg(fixtures.join("publication_lease.py"))
        .arg(&repo)
        .arg(env!("CARGO_BIN_EXE_kioku"))
        .arg(&helper)
        .arg(tracer_path())
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "lease publication test failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn explain_ast_diagnostic_tracks_changes_without_authorizing_reuse() {
    let repo = Repo::new();
    repo.write("value.js", "const value = 1;\n");
    repo.commit();
    let cmd = ["explain", "--json", "--", "sh", "-c", "exit 0"];
    let before = repo.run().args(cmd).success();
    let before: serde_json::Value = serde_json::from_slice(&before.stdout).unwrap();
    assert_eq!(before["ast_would_skip"], false);
    repo.run()
        .args(["run", "--", "sh", "-c", "exit 0"])
        .success();
    let stable = repo.run().args(cmd).success();
    let stable: serde_json::Value = serde_json::from_slice(&stable.stdout).unwrap();
    assert_eq!(stable["ast_would_skip"], true);
    repo.write("value.js", "const value = 2;\n");
    let changed = repo.run().args(cmd).success();
    let changed: serde_json::Value = serde_json::from_slice(&changed.stdout).unwrap();
    assert_eq!(changed["ast_would_skip"], false);
    assert_eq!(changed["hit"], false);
}
