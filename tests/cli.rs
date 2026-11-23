//! Integration tests of the `kioku` command line.
//!
//! The tests use a real git repository in a temporary directory: the cache key
//! depends on the tracked files, the lockfiles and the tools, so a real
//! repository is the only honest fixture.
//!
//! The tests that run a child process need a shell, so they are `cfg(unix)`.
//! The tests of the command line contract itself run everywhere.

mod support;

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
fn a_declared_environment_variable_changes_the_key() {
    let repo = Repo::new();
    repo.write("main.txt", "hello\n");
    repo.commit();

    repo.run()
        .env("KIOKU_TEST_VALUE", "one")
        .args([
            "run",
            "--env",
            "KIOKU_TEST_VALUE",
            "--",
            "sh",
            "-c",
            "exit 0",
        ])
        .success();
    let second = repo
        .run()
        .env("KIOKU_TEST_VALUE", "two")
        .args([
            "run",
            "--env",
            "KIOKU_TEST_VALUE",
            "--",
            "sh",
            "-c",
            "exit 0",
        ])
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
fn a_store_with_restricted_inputs_is_not_shared_between_repositories() {
    let first = Repo::new();
    first.write("main.txt", "same\n");
    first.commit();
    let second = Repo::new();
    second.write("main.txt", "same\n");
    second.commit();
    let shared = TempDir::new().expect("a temporary directory");

    // The inputs are restricted: the entry is tied to its repository, so an
    // identical repository must not reuse it.
    first
        .run()
        .env("KIOKU_DIR", shared.path())
        .args(["run", "--inputs", "main.txt", "--", "sh", "-c", "exit 0"])
        .success();
    let other = second
        .run()
        .env("KIOKU_DIR", shared.path())
        .args(["run", "--inputs", "main.txt", "--", "sh", "-c", "exit 0"])
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
        "--inputs",
        "cache/**",
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
            .args([
                verb,
                "--json",
                "--trust-trace",
                "--strict-env",
                "--verify-rate",
                rate,
                "--",
            ])
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

#[test]
fn removed_daemon_commands_are_rejected() {
    for command in ["daemon", "ci"] {
        let output = Command::new(env!("CARGO_BIN_EXE_kioku"))
            .arg(command)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{command}");
        assert_eq!(output.stdout, [] as [u8; 0]);
    }
}

#[test]
fn operator_stop_file_is_only_checked_when_the_variable_names_one() {
    let dir = TempDir::new().unwrap();
    let present = dir.path().join("ARRET-KIOKU.txt");
    let absent = dir.path().join("never-created.txt");
    std::fs::write(&present, b"stop").unwrap();
    assert_eq!(support::existing_operator_stop_file(None), None);
    assert_eq!(
        support::existing_operator_stop_file(Some(absent.as_os_str())),
        None
    );
    assert_eq!(
        support::existing_operator_stop_file(Some(present.as_os_str())),
        Some(present)
    );
}

#[cfg(test)]
#[test]
fn true_program_succeeds_with_closed_stdin_and_empty_environment() {
    let status = std::process::Command::new(support::true_program())
        .args(["synthetic-argument", "--ignored"])
        .stdin(std::process::Stdio::null())
        .env_clear()
        .status()
        .expect("the success program must start");
    assert!(status.success(), "success program returned {status}");
}
