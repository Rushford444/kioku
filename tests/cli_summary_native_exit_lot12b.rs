//! Real CLI regression: a filtered fallback runs nextest and writes the full journal.
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;

use kioku::select::journal::{self, Journal, Outcome};

fn copy_fixture(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_fixture(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[test]
fn filtered_fallback_runs_real_nextest_and_preserves_journal_in_native_mode() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/select/l4exec/minicrate");
    copy_fixture(&fixture, &repo);
    let mut previous: Option<Journal> = None;
    for native in [false, true] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_kioku"));
        cmd.current_dir(&repo)
            .args(["--json", "test", "--repo", ".", "--store"])
            .arg(dir.path().join("empty-store"))
            .args(["-E", "test(a_unit)"])
            .env("CARGO_NET_OFFLINE", "true")
            .env_remove("NEXTEST_PROFILE")
            .env_remove("KIOKU_SELECT_STORE");
        for (name, _) in std::env::vars_os() {
            let key = name.to_string_lossy();
            if key.starts_with("NEXTEST")
                || key.starts_with("CARGO_PKG_")
                || key.starts_with("CARGO_BIN_")
                || matches!(
                    key.as_ref(),
                    "CARGO_MANIFEST_DIR"
                        | "CARGO_CRATE_NAME"
                        | "CARGO_PRIMARY_PACKAGE"
                        | "CARGO_TARGET_TMPDIR"
                        | "CARGO_TARGET_DIR"
                )
            {
                cmd.env_remove(name);
            }
        }
        if native {
            cmd.arg("--native-exit-code");
        }
        let out = cmd.output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(3),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let summary: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(summary["schema"], "kioku/select/v1");
        assert_eq!(summary["exit_code"], 3);
        let journal: Journal =
            journal::read(&repo.join("target/kioku-select/journal.json")).unwrap();
        assert_eq!(journal.schema, "kioku/select/v1");
        assert_eq!(journal.user_filter.as_deref(), Some("test(a_unit)"));
        assert!(journal.outcomes.iter().any(|outcome| {
            outcome.test.name.contains("a_unit") && outcome.outcome == Outcome::Passed
        }));
        assert!(journal.outcomes.iter().any(|outcome| {
            outcome.test.name.contains("b_unit") && outcome.outcome == Outcome::FilteredOut
        }));
        let inventory = BTreeSet::new();
        let new = journal
            .entries
            .iter()
            .map(|entry| entry.test.clone())
            .collect();
        journal::check_complete(&journal, &inventory, &new).unwrap();
        if let Some(ref old) = previous {
            assert_eq!(&journal, old);
        }
        previous = Some(journal);
    }
}

#[cfg(unix)]
#[test]
fn native_exit_returns_raw_nextest_error_without_hiding_selection_failure() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let fake_bin = dir.path().join("bin");
    fs::create_dir(&fake_bin).unwrap();
    let cargo = fake_bin.join("cargo");
    let path = format!("{}:{}", fake_bin.display(), std::env::var("PATH").unwrap());
    for (native_code, normal_code) in [(43, 3), (101, 1)] {
        fs::write(&cargo, format!("#!/bin/sh\nexit {native_code}\n")).unwrap();
        fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();
        for native in [false, true] {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_kioku"));
            cmd.current_dir(dir.path())
                .env("PATH", &path)
                .args(["--json", "test", "--repo", ".", "--store"])
                .arg(dir.path().join("store"));
            if native {
                cmd.arg("--native-exit-code");
            }
            let out = cmd.output().unwrap();
            let expected = if native { native_code } else { normal_code };
            assert_eq!(
                out.status.code(),
                Some(expected),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let summary: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(summary["exit_code"], expected);
        }
    }
}
