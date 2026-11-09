//! Real CLI tests for the execute-only policy; trace eligibility is tested separately.
#![cfg(unix)]

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
mod support;

use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

fn invoke(repo: &TempDir, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kioku"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", repo.path())
        .env("LANG", "C.UTF-8")
        .env("KIOKU_SENTINEL_SECRET", "sk-test-SENTINEL")
        .current_dir(repo.path())
        .stdin(Stdio::null())
        .args(args)
        .output()
        .expect("run kioku")
}

fn assert_redacted(bytes: &[u8]) {
    let sentinel = b"sk-test-SENTINEL";
    assert!(
        !bytes
            .windows(sentinel.len())
            .any(|window| window == sentinel),
        "sentinel leaked"
    );
}

fn assert_tree_redacted(path: &std::path::Path) {
    if !path.exists() {
        return;
    }
    for entry in std::fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            assert_tree_redacted(&entry.path());
        } else if kind.is_file() {
            assert_redacted(&std::fs::read(entry.path()).unwrap());
        }
    }
}

fn event(output: &Output) -> serde_json::Value {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let line = stderr
        .lines()
        .rev()
        .find(|line| line.starts_with('{'))
        .expect("JSON event on stderr");
    serde_json::from_str(line).expect("valid event")
}

#[test]
fn default_executes_twice_and_reflects_ignored_input_change() {
    let repo = TempDir::new().unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(repo.path().join(".gitignore"), "secret-input\n").unwrap();
    std::fs::write(repo.path().join("secret-input"), "yes\n").unwrap();
    let args = [
        "run",
        "--json",
        "--",
        "sh",
        "-c",
        "grep -q yes secret-input",
    ];
    let first = invoke(&repo, &args);
    assert!(first.status.success());
    let second = invoke(&repo, &args);
    assert!(second.status.success());
    assert_eq!(event(&second)["policy"], "execute-only");
    assert_eq!(event(&second)["cacheable"], false);
    std::fs::write(repo.path().join("secret-input"), "no\n").unwrap();
    let changed = invoke(&repo, &args);
    assert_eq!(changed.status.code(), Some(1));
    assert_eq!(event(&changed)["policy"], "execute-only");
    for output in [&first, &second, &changed] {
        assert_redacted(&output.stdout);
        assert_redacted(&output.stderr);
    }
    assert_tree_redacted(&repo.path().join(".git/kioku"));
}

#[test]
fn source_text_change_cannot_reuse_a_default_result() {
    let repo = TempDir::new().unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success()
    );
    let source = repo.path().join("value.js");
    std::fs::write(&source, "const value = 1;\n").unwrap();
    let args = [
        "run",
        "--json",
        "--",
        "sh",
        "-c",
        "grep -q '^const value' value.js",
    ];
    assert!(invoke(&repo, &args).status.success());
    assert!(invoke(&repo, &args).status.success());
    std::fs::write(&source, "const  value = 1;\n").unwrap();
    assert_eq!(invoke(&repo, &args).status.code(), Some(1));
}

#[test]
fn default_observes_created_previously_absent_path() {
    let repo = TempDir::new().unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success()
    );
    let args = ["run", "--json", "--", "sh", "-c", "test ! -e absent.lock"];
    assert!(invoke(&repo, &args).status.success());
    assert!(invoke(&repo, &args).status.success());
    std::fs::write(repo.path().join("absent.lock"), "created").unwrap();
    assert_eq!(invoke(&repo, &args).status.code(), Some(1));
}

#[test]
fn default_observes_symlink_retargeting() {
    use std::os::unix::fs::symlink;
    let repo = TempDir::new().unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(repo.path().join("a"), "yes").unwrap();
    std::fs::write(repo.path().join("b"), "no").unwrap();
    let link = repo.path().join("link");
    symlink("a", &link).unwrap();
    let args = ["run", "--json", "--", "sh", "-c", "grep -q yes link"];
    assert!(invoke(&repo, &args).status.success());
    assert!(invoke(&repo, &args).status.success());
    std::fs::remove_file(&link).unwrap();
    symlink("b", &link).unwrap();
    assert_eq!(invoke(&repo, &args).status.code(), Some(1));
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
mod traced_readers {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::Path;

    fn tracer_path() -> std::path::PathBuf {
        std::env::var_os("KIOKU_STRACE").map_or_else(
            || Path::new(env!("CARGO_MANIFEST_DIR")).join("target/tracer/extracted/usr/bin/strace"),
            std::path::PathBuf::from,
        )
    }

    fn command(program: &Path, home: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", home)
            .env("LANG", "C.UTF-8")
            .env("KIOKU_SENTINEL_SECRET", "sk-test-SENTINEL")
            .env("KIOKU_STRACE", tracer_path())
            .stdin(Stdio::null());
        command
    }

    fn reader(home: &Path, kind: &str) -> std::path::PathBuf {
        let binary = home.join("reader");
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join(if kind.starts_with("absent") {
            "tests/false_green_zoo/absent_aarch64.S"
        } else {
            "tests/false_green_zoo/read_aarch64.S"
        });
        let status = command(&support::executable("cc"), home)
            .args(["-nostdlib", "-static", "-o"])
            .arg(&binary)
            .arg(source)
            .status()
            .expect("compile static helper");
        assert!(status.success(), "static helper compilation failed");
        binary
    }

    fn is_record(
        value: &serde_json::Value,
        path: &[u8],
        expected_kind: &str,
        digest: &[u8; 32],
    ) -> bool {
        let Some(object) = value.as_object() else {
            return false;
        };
        let expected_path: Vec<_> = path.iter().map(|b| serde_json::Value::from(*b)).collect();
        let expected_digest: Vec<_> = digest.iter().map(|b| serde_json::Value::from(*b)).collect();
        object.get("path") == Some(&serde_json::Value::Array(expected_path))
            && object
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|kind| kind.eq_ignore_ascii_case(expected_kind))
            && object.get("digest") == Some(&serde_json::Value::Array(expected_digest))
    }

    fn record_present(
        value: &serde_json::Value,
        path: &[u8],
        expected_kind: &str,
        digest: &[u8; 32],
    ) -> bool {
        is_record(value, path, expected_kind, digest)
            || match value {
                serde_json::Value::Object(object) => object
                    .values()
                    .any(|child| record_present(child, path, expected_kind, digest)),
                serde_json::Value::Array(items) => items
                    .iter()
                    .any(|child| record_present(child, path, expected_kind, digest)),
                _ => false,
            }
    }

    fn erase_record(
        value: &mut serde_json::Value,
        path: &[u8],
        expected_kind: &str,
        digest: &[u8; 32],
    ) {
        match value {
            serde_json::Value::Object(object) => {
                for child in object.values_mut() {
                    erase_record(child, path, expected_kind, digest);
                }
            }
            serde_json::Value::Array(items) => {
                items.retain(|item| !is_record(item, path, expected_kind, digest));
                for child in items {
                    erase_record(child, path, expected_kind, digest);
                }
            }
            _ => {}
        }
    }

    fn json_files(path: &Path, result: &mut Vec<serde_json::Value>) {
        if !path.exists() {
            return;
        }
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                json_files(&entry.path(), result);
            } else if kind.is_file()
                && let Ok(value) =
                    serde_json::from_slice::<serde_json::Value>(&fs::read(entry.path()).unwrap())
            {
                result.push(value);
            }
        }
    }

    fn change_record_digest(
        value: &mut serde_json::Value,
        path: &[u8],
        kind: &str,
        digest: &[u8; 32],
    ) {
        if is_record(value, path, kind, digest) {
            value["digest"][0] = serde_json::Value::from(digest[0] ^ 1);
            return;
        }
        match value {
            serde_json::Value::Object(object) => {
                for child in object.values_mut() {
                    change_record_digest(child, path, kind, digest);
                }
            }
            serde_json::Value::Array(items) => {
                for child in items {
                    change_record_digest(child, path, kind, digest);
                }
            }
            _ => {}
        }
    }

    fn find_digest(
        value: &serde_json::Value,
        path: &[u8],
        expected_kind: &str,
    ) -> Option<[u8; 32]> {
        match value {
            serde_json::Value::Object(object) => {
                let expected: Vec<_> = path
                    .iter()
                    .map(|byte| serde_json::Value::from(*byte))
                    .collect();
                if object.get("path") == Some(&serde_json::Value::Array(expected))
                    && object
                        .get("kind")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|kind| kind.eq_ignore_ascii_case(expected_kind))
                {
                    let values = object.get("digest")?.as_array()?;
                    let bytes: Vec<u8> = values
                        .iter()
                        .map(|value| value.as_u64().and_then(|x| u8::try_from(x).ok()))
                        .collect::<Option<_>>()?;
                    return bytes.try_into().ok();
                }
                object
                    .values()
                    .find_map(|child| find_digest(child, path, expected_kind))
            }
            serde_json::Value::Array(items) => items
                .iter()
                .find_map(|child| find_digest(child, path, expected_kind)),
            _ => None,
        }
    }

    fn assert_manifest_record(repo: &Path, path: &Path, kind: &str) {
        use std::os::unix::ffi::OsStrExt;
        let mut values = Vec::new();
        json_files(&repo.join(".git/kioku/trace-v2"), &mut values);
        let manifest = serde_json::Value::Array(values);
        let path_bytes = path.as_os_str().as_bytes();
        let digest = find_digest(&manifest, path_bytes, kind)
            .expect("expected independent trace path/kind absent from manifest");
        assert_ne!(digest, [0; 32]);
        assert!(record_present(&manifest, path_bytes, kind, &digest));
        let mut deleted = manifest.clone();
        erase_record(&mut deleted, path_bytes, kind, &digest);
        assert!(
            !record_present(&deleted, path_bytes, kind, &digest),
            "record-removal sensitivity control failed"
        );
        let mut changed = manifest;
        change_record_digest(&mut changed, path_bytes, kind, &digest);
        assert!(
            !record_present(&changed, path_bytes, kind, &digest),
            "record-digest sensitivity control failed"
        );
    }

    // Keep each end-to-end scenario together so its controls and mutation stay visible.
    #[allow(clippy::too_many_lines)]
    fn case(kind: &str) {
        let outer = TempDir::new().unwrap();
        let repo = outer.path().join("repo");
        fs::create_dir(&repo).unwrap();
        assert!(
            command(&support::executable("git"), outer.path())
                .args(["init", "-q"])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
        let mut binary = reader(outer.path(), kind);
        let child_binary = if kind == "tool_child" {
            let child = binary;
            binary = outer.path().join("exec-parent");
            let source =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/false_green_zoo/exec_aarch64.S");
            assert!(
                command(&support::executable("cc"), outer.path())
                    .args(["-nostdlib", "-static", "-o"])
                    .arg(&binary)
                    .arg(source)
                    .status()
                    .unwrap()
                    .success()
            );
            Some(child)
        } else {
            None
        };
        let parent = repo.join("parent");
        fs::write(&parent, "parent input\n").unwrap();
        let target = match kind {
            "source" | "line_endings" | "full_environment" | "tool_child" | "executable" => {
                repo.join("source.js")
            }
            "absent" => repo.join("absent"),
            "absent_lock" => repo.join("Cargo.lock"),
            "case_sensitive" => repo.join("Input"),
            "metadata" => repo.join("metadata"),
            "deleted" => repo.join("deleted"),
            "ignored" => {
                fs::write(repo.join(".gitignore"), "ignored\n").unwrap();
                repo.join("ignored")
            }
            "external" => outer.path().join("external"),
            "dotenv" => repo.join(".env"),
            "symlink_content" | "symlink_retarget" => repo.join("target"),
            "child" => outer.path().join("child-input"),
            "submodule" => {
                let origin = outer.path().join("origin");
                fs::create_dir(&origin).unwrap();
                for args in [
                    vec!["init", "-q"],
                    vec!["config", "user.name", "Zoo"],
                    vec!["config", "user.email", "zoo@example.invalid"],
                ] {
                    assert!(
                        command(&support::executable("git"), outer.path())
                            .current_dir(&origin)
                            .args(args)
                            .status()
                            .unwrap()
                            .success()
                    );
                }
                fs::write(origin.join("input"), b"before\n").unwrap();
                for args in [vec!["add", "input"], vec!["commit", "-qm", "fixture"]] {
                    assert!(
                        command(&support::executable("git"), outer.path())
                            .current_dir(&origin)
                            .args(args)
                            .status()
                            .unwrap()
                            .success()
                    );
                }
                assert!(
                    command(&support::executable("git"), outer.path())
                        .current_dir(&repo)
                        .args(["-c", "protocol.file.allow=always", "submodule", "add", "-q"])
                        .arg(&origin)
                        .arg("nested")
                        .status()
                        .unwrap()
                        .success()
                );
                repo.join("nested/input")
            }
            _ => panic!("unknown scenario"),
        };
        if !kind.starts_with("absent") {
            fs::write(
                &target,
                if kind == "source" {
                    "const value = 1;\n"
                } else {
                    "before\n"
                },
            )
            .unwrap();
        }
        if kind == "metadata" {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        }
        let link = repo.join("link");
        if kind.starts_with("symlink") {
            symlink("target", &link).unwrap();
        }
        let selected = if kind.starts_with("symlink") {
            &link
        } else {
            &target
        };
        // The independent oracle captures only these syscalls in memory, never execve arguments.
        let tracer = tracer_path();
        let mut oracle = command(&tracer, outer.path());
        oracle
            .current_dir(&repo)
            .args([
                "-f",
                "-e",
                "trace=openat,read,clone,wait4,exit",
                "-e",
                "raw=read",
                "-s",
                "4096",
                "--",
            ])
            .arg(&binary);
        if kind == "child" {
            oracle.arg(&parent);
        }
        if let Some(child) = &child_binary {
            oracle.arg(child);
        }
        let raw = oracle
            .arg(selected)
            .output()
            .expect("independent strace must be available");
        assert!(raw.status.success(), "independent trace failed");
        assert_redacted(&raw.stderr);
        let selected_bytes = selected.to_str().unwrap().as_bytes();
        assert!(
            raw.stderr
                .windows(selected_bytes.len())
                .any(|bytes| bytes == selected_bytes),
            "independent trace missing expected input"
        );
        if kind == "child" {
            let parent_bytes = parent.to_str().unwrap().as_bytes();
            assert!(
                raw.stderr
                    .windows(parent_bytes.len())
                    .any(|bytes| bytes == parent_bytes),
                "independent trace missing parent input"
            );
            assert!(
                raw.stderr
                    .windows(b"clone(".len())
                    .any(|bytes| bytes == b"clone("),
                "independent trace missing child creation"
            );
        }
        let env_changed = std::cell::Cell::new(false);
        let invoke = || {
            let mut cmd = command(Path::new(env!("CARGO_BIN_EXE_kioku")), outer.path());
            cmd.env(
                "ZOO_UNDECLARED",
                if env_changed.get() { "after" } else { "before" },
            );
            cmd.current_dir(&repo)
                .args(["run", "--json", "--trust-trace"]);
            if kind == "source" {
                cmd.args(["--mode", "ast"]);
            }
            cmd.arg("--").arg(&binary);
            if kind == "child" {
                cmd.arg(&parent);
            }
            if let Some(child) = &child_binary {
                cmd.arg(child);
            }
            cmd.arg(selected).output().unwrap()
        };
        for _ in 0..2 {
            let out = invoke();
            assert!(out.status.success(), "learning child failed: {kind}");
            assert_eq!(
                event(&out)["decision"],
                "executed",
                "learning must execute: {kind}"
            );
        }
        let hit = invoke();
        assert!(hit.status.success());
        let hit_event = event(&hit);
        assert_eq!(
            hit_event["decision"], "cached",
            "positive trace control must genuinely hit: {kind}; reason={} denial={} state={}",
            hit_event["reason"], hit_event["denial_reason"], hit_event["trace_state"]
        );
        if kind == "source" {
            assert_eq!(event(&hit)["mode_requested"], "ast");
            assert_eq!(event(&hit)["mode_effective"], "bytes");
        }
        assert_manifest_record(
            &repo,
            selected,
            if kind.starts_with("absent") {
                "absent"
            } else {
                "file"
            },
        );
        if kind == "child" {
            assert_manifest_record(&repo, &parent, "file");
        }
        assert_manifest_record(&repo, &binary, "executable");
        if let Some(child) = &child_binary {
            assert_manifest_record(&repo, child, "executable");
        }
        if kind == "symlink_retarget" {
            fs::write(repo.join("other"), "before\n").unwrap();
            fs::remove_file(&link).unwrap();
            symlink("other", &link).unwrap();
        } else if kind == "executable" {
            let mut bytes = fs::read(&binary).unwrap();
            bytes.push(0);
            fs::write(&binary, bytes).unwrap();
        } else if let Some(child) = &child_binary {
            let mut bytes = fs::read(child).unwrap();
            bytes.push(0);
            fs::write(child, bytes).unwrap();
        } else if kind == "full_environment" {
            env_changed.set(true);
        } else if kind == "deleted" {
            fs::remove_file(&target).unwrap();
        } else if kind == "metadata" {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        } else if kind == "line_endings" {
            fs::write(&target, "before\r\n").unwrap();
        } else if kind == "source" {
            fs::write(&target, "const  value = 1;\n").unwrap();
        } else {
            fs::write(&target, "after\n").unwrap();
        }
        let changed = invoke();
        assert_eq!(
            changed.status.code(),
            Some(i32::from(kind == "deleted" || kind.starts_with("absent"))),
            "wrong child status: {kind}"
        );
        assert_eq!(
            event(&changed)["decision"],
            "executed",
            "changed dependency falsely hit: {kind}"
        );
        if kind == "full_environment" || kind == "source" {
            assert_eq!(event(&hit)["result"], "exit-status");
            assert_eq!(event(&hit)["output_replayed"], false);
        }
        for output in [&hit, &changed] {
            assert_redacted(&output.stdout);
            assert_redacted(&output.stderr);
        }
        assert_tree_redacted(&repo.join(".git/kioku"));
    }

    // Keep each end-to-end scenario together so its controls and mutation stay visible.
    #[allow(clippy::too_many_lines)]
    fn operation_case(op: &str, record_kind: &str) {
        let outer = TempDir::new().unwrap();
        let repo = outer.path().join("repo");
        fs::create_dir(&repo).unwrap();
        assert!(
            command(&support::executable("git"), outer.path())
                .args(["init", "-q"])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
        let binary = outer.path().join("operations");
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/false_green_zoo/operations_aarch64.S");
        assert!(
            command(&support::executable("cc"), outer.path())
                .args(["-nostdlib", "-static", "-o"])
                .arg(&binary)
                .arg(source)
                .status()
                .unwrap()
                .success()
        );
        let dir = repo.join("dir");
        fs::create_dir(&dir).unwrap();
        let input = dir.join("input");
        fs::write(&input, b"before").unwrap();
        let path = if op == "listing" { &dir } else { &input };
        let args = |cmd: &mut Command| {
            cmd.arg(&binary).arg(op);
            if op == "chdir" || op == "dirfd" {
                cmd.arg(&dir).arg("input");
            } else {
                cmd.arg(path);
            }
        };
        let tracer = tracer_path();
        let mut oracle = command(&tracer, outer.path());
        oracle.current_dir(&repo).args([
            "-f",
            "-e",
            "trace=openat,read,chdir,newfstatat,getdents64,mmap,exit",
            "-e",
            "raw=read,getdents64",
            "-s",
            "4096",
            "--",
        ]);
        args(&mut oracle);
        let trace = oracle.output().unwrap();
        assert!(trace.status.success());
        assert_redacted(&trace.stderr);
        let syscall: &[u8] = match op {
            "chdir" => b"chdir(",
            "dirfd" => b"openat(",
            "stat" => b"newfstatat(",
            "listing" => b"getdents64(",
            "mmap" => b"mmap(",
            _ => panic!("unknown operation"),
        };
        assert!(
            trace
                .stderr
                .windows(syscall.len())
                .any(|line| line == syscall),
            "oracle did not observe expected syscall"
        );
        let invoke = || {
            let mut cli = command(Path::new(env!("CARGO_BIN_EXE_kioku")), outer.path());
            cli.current_dir(&repo)
                .args(["run", "--json", "--trust-trace", "--"]);
            args(&mut cli);
            cli.output().unwrap()
        };
        for _ in 0..2 {
            let out = invoke();
            assert!(out.status.success());
            assert_eq!(event(&out)["decision"], "executed");
        }
        let hit = invoke();
        assert!(hit.status.success());
        assert_eq!(
            event(&hit)["decision"],
            "cached",
            "operation baseline must hit"
        );
        assert_manifest_record(&repo, path, record_kind);
        if op == "listing" {
            fs::write(dir.join("added"), b"new").unwrap();
        } else if op == "stat" {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&input, fs::Permissions::from_mode(0o600)).unwrap();
        } else {
            fs::write(&input, b"after").unwrap();
        }
        let changed = invoke();
        assert!(changed.status.success());
        assert_eq!(
            event(&changed)["decision"],
            "executed",
            "changed operation dependency falsely hit"
        );
    }

    #[test]
    #[ignore = "requires pinned tracer"]
    fn stat_metadata_query() {
        operation_case("stat", "metadata");
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn directory_enumeration() {
        operation_case("listing", "directory");
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn relative_open_after_chdir() {
        operation_case("chdir", "file");
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn relative_open_via_dirfd() {
        operation_case("dirfd", "file");
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn private_file_mapping() {
        operation_case("mmap", "file");
    }

    // Keep each end-to-end scenario together so its controls and mutation stay visible.
    #[allow(clippy::too_many_lines)]
    fn deny_case(operation: &str, expected_reason: &str) {
        case("source");
        let outer = TempDir::new().unwrap();
        let repo = outer.path().join("repo");
        fs::create_dir(&repo).unwrap();
        assert!(
            command(&support::executable("git"), outer.path())
                .args(["init", "-q"])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
        let binary = outer.path().join("attack");
        let source =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/false_green_zoo/attack_aarch64.S");
        assert!(
            command(&support::executable("cc"), outer.path())
                .args(["-nostdlib", "-static", "-o"])
                .arg(&binary)
                .arg(source)
                .status()
                .unwrap()
                .success()
        );
        if operation == "async" {
            let tracer = tracer_path();
            let raw = command(&tracer, outer.path())
                .args(["-e", "trace=io_uring_setup", "--"])
                .arg(&binary)
                .arg("async")
                .output()
                .unwrap();
            assert!(raw.status.success());
            let trace = std::str::from_utf8(&raw.stderr).unwrap();
            let line = trace
                .lines()
                .find(|line| line.starts_with("io_uring_setup("))
                .expect("independent io_uring syscall");
            let result = line.rsplit_once(" = ").unwrap().1;
            assert!(
                result.starts_with("-1 EPERM")
                    || result
                        .split_whitespace()
                        .next()
                        .unwrap()
                        .parse::<i32>()
                        .is_ok_and(|fd| fd >= 0),
                "unexpected io_uring availability result"
            );
            assert_redacted(&raw.stderr);
        }
        let mut cmd = command(Path::new(env!("CARGO_BIN_EXE_kioku")), outer.path());
        let mutable_file = repo.join("mutable-input");
        fs::write(&mutable_file, b"fixture").unwrap();
        cmd.current_dir(&repo)
            .args(["run", "--json", "--trust-trace", "--"])
            .arg(binary)
            .arg(operation)
            .arg(&mutable_file);
        let output = if operation == "input" {
            use std::io::Write;
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = cmd.spawn().unwrap();
            child.stdin.take().unwrap().write_all(b"data").unwrap();
            child.wait_with_output().unwrap()
        } else {
            cmd.output().unwrap()
        };
        assert!(
            output.status.success(),
            "denied child must retain actual status"
        );
        assert_eq!(output.stdout, b"ran\n", "child must actually execute");
        if operation == "write" {
            assert_eq!(
                fs::read(&mutable_file).unwrap(),
                b"Xixture",
                "the read input must really be rewritten"
            );
        }
        assert_redacted(&output.stdout);
        assert_tree_redacted(&repo.join(".git/kioku"));
        let e = event(&output);
        assert_eq!(e["decision"], "executed");
        assert!(
            e["reason"] == "trace-denied"
                || (operation == "input" && e["reason"] == "stdin-unsupported")
        );
        if operation != "input" {
            assert_eq!(
                e["denial_reason"], expected_reason,
                "wrong deny reason category"
            );
        }
        assert!(
            !output
                .stderr
                .windows(b"sk-test-SENTINEL".len())
                .any(|s| s == b"sk-test-SENTINEL")
        );
    }

    #[test]
    #[ignore = "requires pinned tracer"]
    fn required_output_always_executes() {
        case("source");
        let outer = TempDir::new().unwrap();
        let repo = outer.path().join("repo");
        fs::create_dir(&repo).unwrap();
        assert!(
            command(&support::executable("git"), outer.path())
                .args(["init", "-q"])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
        let binary = outer.path().join("attack");
        let source =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/false_green_zoo/attack_aarch64.S");
        assert!(
            command(&support::executable("cc"), outer.path())
                .args(["-nostdlib", "-static", "-o"])
                .arg(&binary)
                .arg(source)
                .status()
                .unwrap()
                .success()
        );
        for _ in 0..3 {
            let output = command(Path::new(env!("CARGO_BIN_EXE_kioku")), outer.path())
                .current_dir(&repo)
                .args(["run", "--json", "--trust-trace", "--require-output", "--"])
                .arg(&binary)
                .arg("none")
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout, b"ran\n");
            let event = event(&output);
            assert_eq!(event["decision"], "executed");
            assert_eq!(event["reason"], "output-required");
            assert_eq!(event["result"], "exit-status");
            assert_eq!(event["output_replayed"], false);
        }
    }

    #[test]
    #[ignore = "requires pinned tracer"]
    fn deny_inherited_fd_read() {
        case("source");
        let outer = TempDir::new().unwrap();
        let repo = outer.path().join("repo");
        fs::create_dir(&repo).unwrap();
        assert!(
            command(&support::executable("git"), outer.path())
                .current_dir(&repo)
                .args(["init", "-q"])
                .status()
                .unwrap()
                .success()
        );
        let binary = outer.path().join("attack");
        let source =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/false_green_zoo/attack_aarch64.S");
        assert!(
            command(&support::executable("cc"), outer.path())
                .args(["-nostdlib", "-static", "-o"])
                .arg(&binary)
                .arg(source)
                .status()
                .unwrap()
                .success()
        );
        let input = repo.join("inherited");
        fs::write(&input, b"fixture").unwrap();
        let out = command(&support::executable("sh"), outer.path())
            .current_dir(&repo)
            .arg("-c")
            .arg("exec 3<\"$1\"; shift; exec \"$@\"")
            .arg("zoo")
            .arg(&input)
            .arg(env!("CARGO_BIN_EXE_kioku"))
            .args(["run", "--json", "--trust-trace", "--"])
            .arg(&binary)
            .arg("fd")
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, b"ran\n");
        let event = event(&out);
        assert_eq!(event["decision"], "executed");
        assert_eq!(event["reason"], "trace-denied");
        assert_eq!(event["denial_reason"], "parse-error");
        assert_redacted(&out.stderr);
        assert_tree_redacted(&repo.join(".git/kioku"));
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn deny_io_uring() {
        deny_case("async", "unknown-syscall");
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn deny_network() {
        deny_case("network", "network");
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn deny_clock() {
        deny_case("clock", "clock");
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn deny_random() {
        deny_case("random", "random");
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn deny_stdin() {
        deny_case("input", "stdin");
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn deny_proc() {
        deny_case("proc", "system-path");
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn deny_proc_through_symlink() {
        case("source");
        let outer = TempDir::new().unwrap();
        let repo = outer.path().join("repo");
        fs::create_dir(&repo).unwrap();
        assert!(
            command(&support::executable("git"), outer.path())
                .current_dir(&repo)
                .args(["init", "-q"])
                .status()
                .unwrap()
                .success()
        );
        let binary = reader(outer.path(), "source");
        let target = Path::new("/proc/sys/kernel/ostype");
        assert!(target.is_file(), "Linux proc fixture is required");
        let link = repo.join("link");
        symlink(target, &link).unwrap();
        for _ in 0..3 {
            let out = command(Path::new(env!("CARGO_BIN_EXE_kioku")), outer.path())
                .current_dir(&repo)
                .args(["run", "--json", "--trust-trace", "--"])
                .arg(&binary)
                .arg(&link)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "child should read proc fixture successfully"
            );
            assert_redacted(&out.stdout);
            assert_redacted(&out.stderr);
            let e = event(&out);
            assert_eq!(e["decision"], "executed", "system path must never hit");
            assert_eq!(e["reason"], "trace-denied");
            assert_eq!(e["denial_reason"], "system-path");
            assert_eq!(e["cacheable"], false);
            assert_eq!(e["trace_state"], "complete");
        }
        let store = repo.join(".git/kioku/trace-v2");
        let mut states = Vec::new();
        if store.exists() {
            json_files(&store, &mut states);
        }
        assert!(
            states.is_empty(),
            "denied dependency must not persist eligible state"
        );
        assert_tree_redacted(&repo.join(".git/kioku"));
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn deny_unknown_syscall() {
        deny_case("unknown", "unknown-syscall");
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn deny_read_write_alias() {
        deny_case("write", "write");
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn deny_shared_writable_mapping() {
        deny_case("mapping", "write");
    }

    #[test]
    #[ignore = "requires pinned tracer"]
    fn poisoned_manifest_and_result_never_authorize_hit() {
        case("source");
        for poison in ["manifest", "result"] {
            let outer = TempDir::new().unwrap();
            let repo = outer.path().join("repo");
            fs::create_dir(&repo).unwrap();
            assert!(
                command(&support::executable("git"), outer.path())
                    .args(["init", "-q"])
                    .current_dir(&repo)
                    .status()
                    .unwrap()
                    .success()
            );
            let binary = reader(outer.path(), "source");
            let input = repo.join("input");
            fs::write(&input, b"fixture").unwrap();
            let invoke = || {
                command(Path::new(env!("CARGO_BIN_EXE_kioku")), outer.path())
                    .current_dir(&repo)
                    .args(["run", "--json", "--trust-trace", "--"])
                    .arg(&binary)
                    .arg(&input)
                    .output()
                    .unwrap()
            };
            assert_eq!(event(&invoke())["decision"], "executed");
            assert_eq!(event(&invoke())["decision"], "executed");
            assert_eq!(event(&invoke())["decision"], "cached");
            let store = repo.join(".git/kioku/trace-v2");
            let states: Vec<_> = fs::read_dir(store)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|s| s.to_str())
                        .is_some_and(|name| name.starts_with("state-"))
                        && path
                            .extension()
                            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
                })
                .collect();
            assert_eq!(
                states.len(),
                1,
                "isolated repository must have exactly one state"
            );
            let state = &states[0];
            let mut value: serde_json::Value =
                serde_json::from_slice(&fs::read(state).unwrap()).unwrap();
            if poison == "manifest" {
                value["manifest"]["records"] = serde_json::json!([]);
            } else {
                value["eligible"] = serde_json::json!("forged");
            }
            fs::write(state, serde_json::to_vec(&value).unwrap()).unwrap();
            let out = invoke();
            assert!(out.status.success());
            assert_eq!(
                event(&out)["decision"],
                "executed",
                "corrupt state falsely hit"
            );
        }
    }

    #[test]
    #[ignore = "requires pinned tracer"]
    fn submodule_tracked_content() {
        case("submodule");
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn source_bytes() {
        case("source");
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn ignored_file() {
        case("ignored");
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn external_file() {
        case("external");
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn dotenv_file() {
        case("dotenv");
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn symlink_target_content() {
        case("symlink_content");
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn symlink_retarget_must_never_hit() {
        let outer = TempDir::new().unwrap();
        let repo = outer.path().join("repo");
        fs::create_dir(&repo).unwrap();
        assert!(
            command(&support::executable("git"), outer.path())
                .args(["init", "-q"])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
        let binary = reader(outer.path(), "source");
        fs::write(repo.join("first"), b"one").unwrap();
        fs::write(repo.join("second"), b"two").unwrap();
        let link = repo.join("link");
        symlink("first", &link).unwrap();
        let invoke = || {
            command(Path::new(env!("CARGO_BIN_EXE_kioku")), outer.path())
                .current_dir(&repo)
                .args(["run", "--json", "--trust-trace", "--"])
                .arg(&binary)
                .arg(&link)
                .output()
                .unwrap()
        };
        assert!(invoke().status.success());
        assert!(invoke().status.success());
        let baseline = invoke();
        assert!(baseline.status.success());
        assert_eq!(
            event(&baseline)["decision"],
            "cached",
            "link positive control must actually hit"
        );
        fs::remove_file(&link).unwrap();
        symlink("second", &link).unwrap();
        let changed = invoke();
        assert!(changed.status.success());
        assert_eq!(
            event(&changed)["decision"],
            "executed",
            "retargeted symlink falsely reused"
        );
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn symlink_retarget() {
        case("symlink_retarget");
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn descendant_file() {
        case("child");
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn line_endings() {
        case("line_endings");
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn case_sensitive_path() {
        case("case_sensitive");
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn file_metadata() {
        case("metadata");
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn file_deleted_after_hit() {
        case("deleted");
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn full_environment_change() {
        case("full_environment");
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn changed_child_executable() {
        case("tool_child");
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn changed_executable_image() {
        case("executable");
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn previously_absent_path() {
        case("absent");
    }
    #[test]
    #[ignore = "requires pinned tracer"]
    fn previously_absent_lockfile() {
        case("absent_lock");
    }
}
