//! Contract tests of the `kioku/v1` JSON events.
//!
//! Each test parses one event on the channel the documentation promises and
//! checks the schema it declares. A caller that reads `policy` must see the
//! policy actually applied, not an identifier left behind by an older store
//! layout.

mod support;

use std::process::{Command, Output};

use tempfile::TempDir;

/// Schema identifier of every event.
const SCHEMA: &str = "kioku/v1";

/// A temporary git repository, the only honest fixture for a cache command.
struct Repo {
    dir: TempDir,
}

impl Repo {
    fn new() -> Self {
        let dir = TempDir::new().expect("a temporary directory");
        for args in [
            &["init", "--quiet", "--initial-branch=main"][..],
            &["config", "user.email", "kioku@example.com"],
            &["config", "user.name", "kioku tests"],
            &["config", "commit.gpgsign", "false"],
        ] {
            let status = Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .expect("git must be available");
            assert!(status.success(), "git {args:?} failed");
        }
        Self { dir }
    }

    fn output(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_kioku"))
            .current_dir(self.dir.path())
            .stdin(std::process::Stdio::null())
            .arg("--json")
            .args(args)
            .output()
            .expect("kioku must run")
    }

    fn commit(&self) {
        std::fs::write(self.dir.path().join("main.txt"), "hello\n").expect("the fixture file");
        for args in [
            &["add", "--all"][..],
            &["commit", "--quiet", "--message", "test"],
        ] {
            let status = Command::new("git")
                .args(args)
                .current_dir(self.dir.path())
                .status()
                .expect("git must be available");
            assert!(status.success(), "git {args:?} failed");
        }
    }
}

/// Parses one event from the channel it is documented to use.
fn event(output: &Output, to_stderr: bool) -> serde_json::Value {
    assert!(
        output.status.success(),
        "kioku failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let raw = if to_stderr {
        &output.stderr
    } else {
        &output.stdout
    };
    let text = String::from_utf8_lossy(raw);
    serde_json::from_str(text.trim()).unwrap_or_else(|error| {
        panic!("event is not JSON: {error}\n{text}");
    })
}

#[test]
fn quarantine_reset_event_matches_the_contract() {
    let repo = Repo::new();
    repo.commit();
    let program = support::true_program();
    let program = program.to_str().expect("a UTF-8 program path");
    let value = event(&repo.output(&["quarantine", "reset", "--", program]), false);
    assert_eq!(value["schema"], SCHEMA);
    assert_eq!(value["event"], "quarantine-reset");
    assert_eq!(value["output_replayed"], false);
    assert_eq!(value["result"], "exit-status");
    let keys: Vec<&str> = value
        .as_object()
        .expect("an event object")
        .keys()
        .map(String::as_str)
        .collect();
    for expected in [
        "contract",
        "event",
        "mode_effective",
        "mode_requested",
        "output_replayed",
        "policy",
        "result",
        "schema",
    ] {
        assert!(
            keys.contains(&expected),
            "the reset event must carry `{expected}`: {value}"
        );
    }
    assert_eq!(
        keys.len(),
        8,
        "unexpected field in the reset event: {value}"
    );
}

#[test]
fn quarantine_reset_announces_the_trace_policy_it_applies() {
    let repo = Repo::new();
    repo.commit();
    let program = support::true_program();
    let program = program.to_str().expect("a UTF-8 program path");
    let run = event(&repo.output(&["run", "--trust-trace", "--", program]), true);
    let reset = event(&repo.output(&["quarantine", "reset", "--", program]), false);
    assert_eq!(run["policy"], "trace-v2");
    assert_eq!(
        reset["policy"], run["policy"],
        "a reset applies the same trace policy as a traced run: {reset}"
    );
    assert_eq!(reset["contract"], run["contract"]);
}

#[test]
fn no_command_name_narrows_the_fingerprint_mode() {
    // The removed `profiles` table claimed that formatters and linters force a
    // byte fingerprint while other commands could use AST. Every command is
    // byte-fingerprinted, so the promise holds for both groups alike.
    let repo = Repo::new();
    let commands: &[&[&str]] = &[
        &["prettier", "--check"],
        &["eslint", "src"],
        &["cargo", "clippy"],
        &["npx", "--", "prettier"],
        &["cargo", "test"],
        &["tsc", "--noEmit"],
    ];
    for argv in commands {
        let mut args = vec!["explain"];
        args.extend_from_slice(argv);
        let value = event(&repo.output(&args), false);
        assert_eq!(
            value["mode_requested"], "bytes",
            "command {argv:?} must not need a special profile: {value}"
        );
        assert_eq!(
            value["mode_effective"], "bytes",
            "command {argv:?} must be byte-fingerprinted: {value}"
        );
    }
}

#[test]
fn explain_keeps_its_documented_null_keys() {
    // `closest` and `diff` stay part of `kioku/v1` even though the trace policy
    // has no legacy entry to compare: a caller that reads them must still find
    // the keys, with a null value, and never a missing field.
    let repo = Repo::new();
    repo.commit();
    let program = support::true_program();
    let program = program.to_str().expect("a UTF-8 program path");
    for args in [
        vec!["explain", "--", program],
        vec!["explain", "--trust-trace", "--", program],
    ] {
        let value = event(&repo.output(&args), false);
        for key in ["closest", "diff"] {
            let field = value
                .get(key)
                .unwrap_or_else(|| panic!("`{key}` must stay in the event: {value}"));
            assert!(
                field.is_null(),
                "`{key}` must be null: the trace policy keeps no legacy entry: {value}"
            );
        }
    }
}

#[test]
fn explain_human_output_reports_no_closest_entry() {
    let repo = Repo::new();
    repo.commit();
    let program = support::true_program();
    let program = program.to_str().expect("a UTF-8 program path");
    let output = Command::new(env!("CARGO_BIN_EXE_kioku"))
        .current_dir(repo.dir.path())
        .stdin(std::process::Stdio::null())
        .args(["explain", "--", program])
        .output()
        .expect("kioku must run");
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        !text.contains("closest entry"),
        "explain must not announce an entry it never resolves: {text}"
    );
}
