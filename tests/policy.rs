#[cfg(unix)]
mod support;

use kioku::policy::*;
use std::ffi::OsString;
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use tempfile::tempdir;

#[cfg(not(unix))]
#[test]
fn unsupported_platform_rejects_environment_and_dependency_capture() {
    let environment = [(OsString::from("A"), OsString::from("1"))];
    assert!(matches!(
        env_digest(&[7; 32], &environment),
        Err(PolicyError::Unsupported)
    ));
    let path = std::env::current_dir().unwrap().join("input");
    assert!(matches!(
        Manifest::capture(&[ObservedPath {
            path,
            kind: PathKind::File
        }]),
        Err(PolicyError::Unsupported)
    ));
}

#[cfg(unix)]
#[test]
fn environment_digest_preserves_order_and_rejects_duplicates() {
    let key = [7; 32];
    let one = vec![
        (OsString::from("A"), OsString::from("1")),
        (OsString::from("B"), OsString::from("2")),
    ];
    let mut reversed = one.clone();
    reversed.reverse();
    assert_eq!(
        env_digest(&key, &one).unwrap(),
        env_digest(&key, &one).unwrap()
    );
    assert_ne!(
        env_digest(&key, &one).unwrap(),
        env_digest(&key, &reversed).unwrap()
    );
    let mut changed = one.clone();
    changed[0].1 = "3".into();
    assert_ne!(
        env_digest(&key, &one).unwrap(),
        env_digest(&key, &changed).unwrap()
    );
    let mut duplicate = one.clone();
    duplicate.push(("A".into(), "different".into()));
    assert!(matches!(
        env_digest(&key, &duplicate),
        Err(PolicyError::DuplicateEnvironment)
    ));
}
#[cfg(unix)]
#[test]
fn file_and_absence_are_revalidated() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("input");
    fs::write(&file, "one").unwrap();
    let paths = vec![
        ObservedPath {
            path: file.clone(),
            kind: PathKind::File,
        },
        ObservedPath {
            path: dir.path().join("absent"),
            kind: PathKind::Absent,
        },
    ];
    let manifest = Manifest::capture(&paths).unwrap();
    assert!(manifest.revalidate().unwrap());
    fs::write(&file, "two").unwrap();
    assert!(!manifest.revalidate().unwrap());
}
#[cfg(unix)]
#[test]
fn discovery_learning_eligible_requires_identical_evidence() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("f");
    fs::write(&file, "x").unwrap();
    let paths = vec![ObservedPath {
        path: file,
        kind: PathKind::File,
    }];
    let manifest = Manifest::capture(&paths).unwrap();
    let identity = Identity {
        repository: "repo".into(),
        cwd: "cwd".into(),
        argv_digest: [1; 32],
        environment_digest: [2; 32],
        tracer_digest: [3; 32],
        version: 2,
    };
    let first = observe(&identity, None, None, &manifest, true, true).unwrap();
    assert!(!decide(&identity, Some(&first), &manifest, true, false));
    let second = observe(
        &identity,
        Some(&first),
        Some(&manifest),
        &manifest,
        true,
        true,
    )
    .unwrap();
    assert!(decide(&identity, Some(&second), &manifest, true, false));
    assert!(!decide(&identity, Some(&second), &manifest, false, false));
    assert!(!decide(&identity, Some(&second), &manifest, true, true));
    let mut other = identity.clone();
    other.repository = "other".into();
    assert!(!decide(&other, Some(&second), &manifest, true, false));
}

#[cfg(unix)]
#[test]
fn changed_identity_and_new_dependencies_restart_learning() {
    let dir = tempdir().unwrap();
    let one = dir.path().join("one");
    let two = dir.path().join("two");
    fs::write(&one, "a").unwrap();
    fs::write(&two, "b").unwrap();
    let first = Manifest::capture(&[ObservedPath {
        path: one.clone(),
        kind: PathKind::File,
    }])
    .unwrap();
    let both = Manifest::capture(&[
        ObservedPath {
            path: one,
            kind: PathKind::File,
        },
        ObservedPath {
            path: two,
            kind: PathKind::File,
        },
    ])
    .unwrap();
    let id = Identity {
        repository: "repo".into(),
        cwd: "cwd".into(),
        argv_digest: [1; 32],
        environment_digest: [2; 32],
        tracer_digest: [3; 32],
        version: 2,
    };
    let discovered = observe(&id, None, None, &first, true, true).unwrap();
    let expanded = observe(&id, Some(&discovered), Some(&first), &both, true, true).unwrap();
    assert!(!decide(&id, Some(&expanded), &both, true, false));
    let learned = observe(&id, Some(&expanded), Some(&both), &both, true, true).unwrap();
    assert!(decide(&id, Some(&learned), &both, true, false));
    let mut changed = id.clone();
    changed.environment_digest[0] ^= 1;
    assert!(!decide(&changed, Some(&learned), &both, true, false));
}

#[cfg(unix)]
#[test]
fn symlink_target_and_retarget_change_manifest_even_with_equal_bytes() {
    use std::os::unix::fs::symlink;
    let dir = tempdir().unwrap();
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    fs::write(&a, b"same").unwrap();
    fs::write(&b, b"same").unwrap();
    let link = dir.path().join("link");
    symlink("a", &link).unwrap();
    let paths = [ObservedPath {
        path: link.clone(),
        kind: PathKind::File,
    }];
    let first = Manifest::capture(&paths).unwrap();
    assert!(first.revalidate().unwrap());
    fs::remove_file(&link).unwrap();
    symlink("b", &link).unwrap();
    assert!(!first.revalidate().unwrap());
    let second = Manifest::capture(&paths).unwrap();
    fs::write(&b, b"changed").unwrap();
    assert!(!second.revalidate().unwrap());
}

#[cfg(unix)]
#[test]
fn unresolved_terminal_root_is_denied() {
    use std::os::unix::fs::symlink;
    assert!(
        Manifest::capture(&[ObservedPath {
            path: "/".into(),
            kind: PathKind::Directory
        }])
        .is_err()
    );
    let dir = tempdir().unwrap();
    let link = dir.path().join("root");
    symlink("/", &link).unwrap();
    assert!(
        Manifest::capture(&[ObservedPath {
            path: link,
            kind: PathKind::Directory
        }])
        .is_err()
    );
}

#[cfg(unix)]
#[test]
fn symlinks_into_pseudo_filesystems_are_denied() {
    use std::os::unix::fs::symlink;
    let dir = tempdir().unwrap();
    for (index, (target, suffix, kind)) in [
        ("/proc", "sys/kernel/ostype", PathKind::File),
        ("/sys", "kernel", PathKind::Directory),
        ("/dev", "null", PathKind::Metadata),
        ("/proc/sys/kernel/ostype", "", PathKind::File),
    ]
    .into_iter()
    .enumerate()
    {
        let link = dir.path().join(format!("link-{index}"));
        symlink(target, &link).unwrap();
        assert!(
            Manifest::capture(&[ObservedPath {
                path: link.join(suffix),
                kind
            }])
            .is_err()
        );
    }
}

#[cfg(unix)]
#[test]
fn symlink_parent_components_and_missing_paths_fail_closed() {
    use std::os::unix::fs::symlink;
    let dir = tempdir().unwrap();
    let sub = dir.path().join("sub");
    fs::create_dir(&sub).unwrap();
    fs::write(dir.path().join("actual"), b"ok").unwrap();
    let link = sub.join("link");
    symlink("../actual", &link).unwrap();
    assert!(
        Manifest::capture(&[ObservedPath {
            path: link,
            kind: PathKind::File
        }])
        .is_ok()
    );
    let escape = dir.path().join("escape");
    symlink("../../../../../../actual", &escape).unwrap();
    assert!(
        Manifest::capture(&[ObservedPath {
            path: escape,
            kind: PathKind::File
        }])
        .is_err()
    );
    let missing = dir.path().join("missing");
    assert!(
        Manifest::capture(&[ObservedPath {
            path: missing.clone(),
            kind: PathKind::File
        }])
        .is_err()
    );
    assert!(
        Manifest::capture(&[ObservedPath {
            path: missing.join("child"),
            kind: PathKind::Absent
        }])
        .is_err()
    );
    let file = dir.path().join("actual");
    assert!(
        Manifest::capture(&[ObservedPath {
            path: file.join("child"),
            kind: PathKind::Absent
        }])
        .is_err()
    );
}

#[cfg(unix)]
#[test]
fn symlink_depth_limit_accepts_forty_and_denies_forty_one() {
    use std::os::unix::fs::symlink;
    let dir = tempdir().unwrap();
    // Exclude symlinks in the temporary-directory prefix from the application limit.
    let root = fs::canonicalize(dir.path()).unwrap();
    fs::write(root.join("target"), b"ok").unwrap();
    for index in 1..=41 {
        let target = if index == 1 {
            "target".to_owned()
        } else {
            format!("link{}", index - 1)
        };
        symlink(target, root.join(format!("link{index}"))).unwrap();
    }
    let observed = |depth| {
        [ObservedPath {
            path: root.join(format!("link{depth}")),
            kind: PathKind::File,
        }]
    };
    let result = Manifest::capture(&observed(40));
    assert!(result.is_ok(), "40 links at {}: {result:?}", root.display());
    // Recreate the original fixture with a symlinked TMPDIR prefix. The raw
    // path consumes one extra link; canonicalizing that prefix preserves 40.
    let parent = tempdir().unwrap();
    let parent_root = fs::canonicalize(parent.path()).unwrap();
    let alias = parent_root.join("tmp-alias");
    symlink(&root, &alias).unwrap();
    let aliased_temp = tempfile::tempdir_in(&alias).unwrap();
    let raw_root = aliased_temp.path();
    for index in 1..=40 {
        let target = if index == 1 {
            root.join("target")
        } else {
            format!("link{}", index - 1).into()
        };
        symlink(target, raw_root.join(format!("link{index}"))).unwrap();
    }
    let raw = Manifest::capture(&[ObservedPath {
        path: raw_root.join("link40"),
        kind: PathKind::File,
    }]);
    assert!(
        matches!(raw, Err(PolicyError::Unsupported)),
        "original path {}: {raw:?}",
        raw_root.display()
    );
    let canonical_root = fs::canonicalize(raw_root).unwrap();
    let fixed = Manifest::capture(&[ObservedPath {
        path: canonical_root.join("link40"),
        kind: PathKind::File,
    }]);
    assert!(
        fixed.is_ok(),
        "canonical path {}: {fixed:?}",
        canonical_root.display()
    );
    assert!(matches!(
        Manifest::capture(&observed(41)),
        Err(PolicyError::Unsupported)
    ));
}

#[cfg(unix)]
#[test]
fn wrong_dependency_kind_is_unsupported_not_an_io_error() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("file");
    fs::write(&file, b"ok").unwrap();
    for (path, kind) in [
        (dir.path().to_owned(), PathKind::File),
        (file, PathKind::Directory),
    ] {
        assert!(matches!(
            Manifest::capture(&[ObservedPath { path, kind }]),
            Err(PolicyError::Unsupported)
        ));
    }
}

#[cfg(unix)]
#[test]
fn public_state_reports_learning_identity_and_dependency_changes() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("input");
    fs::write(&file, b"before").unwrap();
    let manifest = Manifest::capture(&[ObservedPath {
        path: file.clone(),
        kind: PathKind::File,
    }])
    .unwrap();
    let id = Identity {
        repository: "repo".into(),
        cwd: "cwd".into(),
        argv_digest: [1; 32],
        environment_digest: [2; 32],
        tracer_digest: [3; 32],
        version: 2,
    };
    let first = observe(&id, None, None, &manifest, true, true).unwrap();
    assert!(!first.is_eligible());
    assert!(first.matches_identity(&id));
    assert!(!first.environment_changed(&id));
    assert!(!first.tracer_changed(&id));
    assert_eq!(first.manifest().changed_count(), 0);
    assert!(first.manifest().changes(&[9; 32]).is_empty());
    let learned = observe(&id, Some(&first), Some(&manifest), &manifest, true, true).unwrap();
    assert!(learned.is_eligible());
    let mut env = id.clone();
    env.environment_digest[0] ^= 1;
    assert!(!learned.matches_identity(&env));
    assert!(learned.environment_changed(&env));
    assert!(!learned.tracer_changed(&env));
    assert!(!decide(&env, Some(&learned), &manifest, true, false));
    let mut tracer = id.clone();
    tracer.tracer_digest[0] ^= 1;
    assert!(!learned.matches_identity(&tracer));
    assert!(!learned.environment_changed(&tracer));
    assert!(learned.tracer_changed(&tracer));
    assert!(!decide(&tracer, Some(&learned), &manifest, true, false));
    let mut argv = id.clone();
    argv.argv_digest[0] ^= 1;
    assert!(!learned.matches_identity(&argv));
    assert!(!decide(&argv, Some(&learned), &manifest, true, false));
    let mut cwd = id.clone();
    cwd.cwd = "other".into();
    assert!(!learned.matches_identity(&cwd));
    assert!(!decide(&cwd, Some(&learned), &manifest, true, false));
    fs::write(&file, b"changed").unwrap();
    assert_eq!(learned.manifest().changed_count(), 1);
    let changes = learned.manifest().changes(&[9; 32]);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].kind, PathKind::File);
    assert_eq!(changes[0].id.len(), 64);
}

#[cfg(unix)]
#[test]
fn invalid_verify_rate_is_rejected_by_library_before_execution() {
    use kioku::runner::{Request, TraceOptions, run_with_options};
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("marker");
    let request = Request {
        argv: vec![
            support::executable("sh").to_string_lossy().into_owned(),
            "-c".into(),
            format!("touch {}", marker.display()),
        ],
        cwd: dir.path().into(),
        mode: None,
    };
    for rate in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.1, 1.1] {
        let options = TraceOptions {
            trust_trace: true,
            require_output: false,
            verify_rate: rate,
        };
        assert!(matches!(
            run_with_options(&request, options, false),
            Err(kioku::runner::Error::InvalidVerifyRate)
        ));
        assert!(!marker.exists());
    }
}

#[cfg(unix)]
#[test]
fn observe_rejects_each_invalid_input_and_never_promotes_mismatches() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("input");
    fs::write(&file, b"before").unwrap();
    let observed = [ObservedPath {
        path: file.clone(),
        kind: PathKind::File,
    }];
    let old = Manifest::capture(&observed).unwrap();
    let id = Identity {
        repository: "repo".into(),
        cwd: "cwd".into(),
        argv_digest: [1; 32],
        environment_digest: [2; 32],
        tracer_digest: [3; 32],
        version: 2,
    };
    let mut bad_version = id.clone();
    bad_version.version = 1;
    assert!(matches!(
        observe(&bad_version, None, None, &old, true, true),
        Err(PolicyError::Incomplete)
    ));
    assert!(matches!(
        observe(&id, None, None, &old, false, true),
        Err(PolicyError::Incomplete)
    ));
    assert!(matches!(
        observe(&id, None, None, &old, true, false),
        Err(PolicyError::Incomplete)
    ));
    let first = observe(&id, None, None, &old, true, true).unwrap();
    assert!(
        !observe(&id, Some(&first), None, &old, true, true)
            .unwrap()
            .is_eligible()
    );
    let mut different = id.clone();
    different.argv_digest[0] ^= 1;
    assert!(
        !observe(&different, Some(&first), Some(&old), &old, true, true)
            .unwrap()
            .is_eligible()
    );
    fs::write(&file, b"after").unwrap();
    assert!(matches!(
        observe(&id, None, None, &old, true, true),
        Err(PolicyError::Incomplete)
    ));
    let new = Manifest::capture(&observed).unwrap();
    assert!(
        !observe(&id, Some(&first), Some(&new), &new, true, true)
            .unwrap()
            .is_eligible()
    );
    assert!(
        !observe(&id, Some(&first), Some(&old), &new, true, true)
            .unwrap()
            .is_eligible()
    );
}

#[cfg(unix)]
#[test]
fn verification_journal_reports_quarantine_for_both_reasons() {
    use kioku::runner::{Request, log_verification, run};
    let dir = tempfile::tempdir().unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .arg(dir.path())
            .status()
            .unwrap()
            .success()
    );
    let request = Request {
        argv: vec![support::true_program().to_string_lossy().into_owned()],
        cwd: dir.path().into(),
        mode: None,
    };
    let mut outcome = run(&request).unwrap();
    for (false_green, reason) in [
        (true, "false-green-quarantined"),
        (false, "quarantined"),
        (false, "trace-learning"),
    ] {
        outcome.false_green = false_green;
        outcome.reason = reason.into();
        log_verification(&request, &outcome).unwrap();
    }
    let data =
        std::fs::read_to_string(dir.path().join(".git/kioku/trace-v2/shadow.jsonl")).unwrap();
    let records: Vec<serde_json::Value> = data
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records.len(), 3);
    assert_eq!(records[0]["quarantined"], true);
    assert_eq!(records[1]["quarantined"], true);
    assert_eq!(records[2]["quarantined"], false);
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[test]
#[ignore = "requires pinned tracer"]
fn explain_accepts_a_symlinked_absolute_executable() {
    use kioku::runner::{Request, TraceOptions, explain_with_options};
    let dir = tempfile::tempdir().unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .arg(dir.path())
            .status()
            .unwrap()
            .success()
    );
    let executable = dir.path().join("link");
    std::os::unix::fs::symlink(support::true_program(), &executable).unwrap();
    let request = Request {
        argv: vec![executable.to_str().unwrap().into()],
        cwd: dir.path().into(),
        mode: None,
    };
    let options = TraceOptions {
        trust_trace: true,
        ..TraceOptions::default()
    };
    let explanation = explain_with_options(&request, options).unwrap();
    assert!(!explanation.hit);
    assert_eq!(explanation.reason, "no-state");
    assert_eq!(explanation.key.len(), 64);
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[test]
#[ignore = "requires pinned tracer"]
fn explain_and_reset_reject_non_utf8_working_directory() {
    use kioku::runner::{Error, Request, TraceOptions, explain_with_options, reset_quarantine};
    use std::os::unix::ffi::OsStringExt;
    let dir = tempfile::tempdir().unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .arg(dir.path())
            .status()
            .unwrap()
            .success()
    );
    let cwd = dir.path().join(OsString::from_vec(vec![0xff]));
    fs::create_dir(&cwd).unwrap();
    let executable = support::true_program();
    let request = Request {
        argv: vec![executable.to_str().unwrap().into()],
        cwd,
        mode: None,
    };
    let explanation = explain_with_options(
        &request,
        TraceOptions {
            trust_trace: true,
            ..TraceOptions::default()
        },
    )
    .unwrap();
    assert!(!explanation.hit);
    assert_eq!(explanation.reason, "trace-unsupported-path");
    assert!(matches!(
        reset_quarantine(&request),
        Err(Error::UnsupportedIdentityPath)
    ));
}

#[cfg(unix)]
#[test]
fn metadata_records_are_invalidated_by_a_timestamp_change_only() {
    use std::time::{Duration, SystemTime};
    let dir = tempdir().unwrap();
    let file = dir.path().join("input");
    fs::write(&file, "one").unwrap();
    let manifest = Manifest::capture(&[ObservedPath {
        path: file.clone(),
        kind: PathKind::Metadata,
    }])
    .unwrap();
    assert!(manifest.revalidate().unwrap());
    let later = SystemTime::now() + Duration::from_secs(5);
    fs::OpenOptions::new()
        .write(true)
        .open(&file)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(later))
        .unwrap();
    assert!(!manifest.revalidate().unwrap());
}
