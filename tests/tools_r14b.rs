//! A proxy must query its own manager, not another manager earlier in PATH.

#![cfg(unix)]

#[path = "support/coverage.rs"]
mod coverage;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::Command;

use kioku::tools::detect;
use tempfile::TempDir;

fn manager(directory: &Path, selected: &Path) {
    fs::create_dir(directory).expect("manager directory");
    let executable = directory.join("rustup");
    fs::write(
        &executable,
        "#!/bin/sh\nIFS= read -r selected < \"${0%/*}/selection\"\n\
         if [ \"$1\" = which ]; then\n  printf '%s\\n' \"$selected\"\n\
         elif [ \"$1\" = --version ]; then\n  IFS= read -r version < \"$selected\"\n\
           printf '%s\\n' \"$version\"\nelse\n  exit 1\nfi\n",
    )
    .expect("controlled manager");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).expect("executable");
    fs::write(
        directory.join("selection"),
        format!("{}\n", selected.display()),
    )
    .expect("manager selection");
}

#[test]
fn proxy_version_cache_tracks_its_own_manager_selection() {
    const CHILD: &str = "KIOKU_R14B_TOOLS_CHILD";
    if let Some(directory) = std::env::var_os(CHILD) {
        let directory = Path::new(&directory);
        let lockfiles = BTreeMap::from([("Cargo.lock".to_owned(), String::new())]);
        let first = detect(directory, &lockfiles).expect("first controlled probe");
        assert_eq!(first.len(), 2);
        assert!(first.iter().all(|tool| tool.version == "fixture 1"));

        // Selection changes between probes, never while either probe is running.
        let selected = directory.join("version-2");
        fs::write(
            directory.join("actual/selection"),
            format!("{}\n", selected.display()),
        )
        .expect("change the actual manager selection");
        let second = detect(directory, &lockfiles).expect("second controlled probe");
        assert_eq!(second.len(), 2);
        for tool in second {
            assert_eq!(
                tool.version, "fixture 2",
                "must not reuse the old selection"
            );
            assert_eq!(
                tool.selected.as_ref().map(|(path, _)| path),
                Some(&selected)
            );
        }
        return;
    }

    let fixture = TempDir::new().expect("isolated tools");
    let directory = fixture.path();
    for (name, version) in [
        ("version-1", "fixture 1\n"),
        ("version-2", "fixture 2\n"),
        ("unrelated-version", "unrelated fixture\n"),
    ] {
        fs::write(directory.join(name), version).expect("public version fixture");
    }
    let actual = directory.join("actual");
    let unrelated = directory.join("unrelated");
    manager(&actual, &directory.join("version-1"));
    manager(&unrelated, &directory.join("unrelated-version"));
    for tool in ["cargo", "rustc"] {
        symlink(actual.join("rustup"), actual.join(tool)).expect("controlled tool proxy");
    }
    let output = Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "proxy_version_cache_tracks_its_own_manager_selection",
        ])
        .env_clear()
        .envs(coverage::profile_env())
        .env(
            "PATH",
            std::env::join_paths([unrelated, actual]).expect("fixture PATH"),
        )
        .env(CHILD, directory)
        .output()
        .expect("isolated test child");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
