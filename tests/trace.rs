#![cfg(target_os = "linux")]

#[cfg(target_arch = "aarch64")]
mod support;

use kioku::policy::{ObservedPath, PathKind};
use kioku::trace::{Arch, DenyReason, parse, parse_for};
use std::path::Path;

#[test]
fn x86_64_legacy_syscalls_use_the_x86_64_grammar() {
    let trace = "100 execve(\"/work/helper\", [...], 0x3000 /* 4 vars */) = 0\n\
100 open(\"file\", O_RDONLY) = 3\n\
100 stat(\"file\", {st_mode=S_IFREG|0644, st_size=1}) = 0\n\
100 lstat(\"link\", {st_mode=S_IFLNK|0777, st_size=4}) = 0\n\
100 access(\"file\", F_OK) = 0\n\
100 readlink(\"link\", \"file\", 4096) = 4\n\
100 fstat(3, {st_mode=S_IFREG|0644, st_size=1}) = 0\n\
100 close(3) = 0\n\
100 faccessat(AT_FDCWD, \"file\", F_OK) = 0\n\
100 readlinkat(AT_FDCWD, \"link\", \"file\", 4096) = 4\n\
100 exit_group(0) = ?\n\
100 +++ exited with 0 +++\n";
    let deps = parse_for(
        Arch::X86_64,
        trace,
        Path::new("/work"),
        Path::new("/work/helper"),
    )
    .unwrap();
    for (name, kind) in [
        ("/work/helper", PathKind::Executable),
        ("/work/file", PathKind::File),
        ("/work/link", PathKind::Metadata),
    ] {
        assert!(deps.contains(&ObservedPath {
            path: name.into(),
            kind
        }));
    }
    assert_eq!(
        parse_for(
            Arch::Aarch64,
            trace,
            Path::new("/work"),
            Path::new("/work/helper")
        ),
        Err(DenyReason::Unknown)
    );
}

#[test]
fn x86_64_write_open_is_a_write_denial() {
    let trace = "100 execve(\"/work/helper\", [...], 0x3000 /* 4 vars */) = 0\n\
100 open(\"out\", O_WRONLY|O_CREAT|O_TRUNC, 0644) = 3\n\
100 exit_group(0) = ?\n\
100 +++ exited with 0 +++\n";
    assert_eq!(
        parse_for(
            Arch::X86_64,
            trace,
            Path::new("/work"),
            Path::new("/work/helper")
        ),
        Err(DenyReason::Write)
    );
}

#[test]
fn denial_codes_are_stable() {
    for (reason, code) in [
        (DenyReason::Unsupported, "unsupported"),
        (DenyReason::Parse(7), "parse-error"),
        (DenyReason::ExecutableFormat, "executable-format"),
        (DenyReason::Network, "network"),
        (DenyReason::Clock, "clock"),
        (DenyReason::Random, "random"),
        (DenyReason::Stdin, "stdin"),
        (DenyReason::SystemPath, "system-path"),
        (DenyReason::Unknown, "unknown-syscall"),
        (DenyReason::Write, "write"),
        (DenyReason::PlatformUnsupported, "platform-unsupported"),
        (DenyReason::TracerUnavailable, "tracer-unavailable"),
        (DenyReason::TracerNotAbsolute, "tracer-not-absolute"),
        (DenyReason::TracerUntrusted, "tracer-untrusted"),
    ] {
        assert_eq!(reason.code(), code);
    }
}

#[test]
fn architecture_diagnostics_are_stable() {
    assert_eq!(Arch::Aarch64.name(), "aarch64");
    assert_eq!(Arch::X86_64.name(), "x86_64");
    assert_eq!(Arch::Aarch64.version(), "strace 6.1 Debian arm64");
    assert_eq!(Arch::X86_64.version(), "strace 6.1 Debian amd64");
}

#[test]
fn parse_for_rejects_each_noncanonical_input_independently() {
    let trace = "100 execve(\"/work/helper\", [...], 0x3000 /* 4 vars */) = 0\n\
100 exit_group(0) = ?\n\
100 +++ exited with 0 +++\n";
    let cases = [
        (Path::new("work"), Path::new("/work/helper")),
        (Path::new("/work"), Path::new("helper")),
    ];
    for (cwd, executable) in cases {
        assert_eq!(
            parse_for(Arch::Aarch64, trace, cwd, executable),
            Err(DenyReason::Unsupported)
        );
    }
    let oversized = "x".repeat(16 * 1024 * 1024 + 1);
    assert_eq!(
        parse_for(
            Arch::Aarch64,
            &oversized,
            Path::new("/work"),
            Path::new("/work/helper")
        ),
        Err(DenyReason::Unsupported)
    );
}

#[test]
fn detects_reader_and_child_dependencies() {
    let trace = "100 execve(\"/work/helper\", [...], 0x3000 /* 4 vars */) = 0\n\
100 openat(AT_FDCWD, \"parent\", O_RDONLY) = 3\n\
100 read(0x3, 0x123, 0x10) = 0x10\n\
100 close(3) = 0\n\
100 clone(child_stack=NULL, flags=SIGCHLD) = 101\n\
101 openat(AT_FDCWD, \"child\", O_RDONLY) = 3\n\
101 read(0x3, 0x123, 0x10) = 0x10\n\
101 close(3) = 0\n\
101 exit(0) = ?\n\
101 +++ exited with 0 +++\n\
100 wait4(101, NULL, 0, NULL) = 101\n\
100 exit(0) = ?\n\
100 +++ exited with 0 +++\n";
    let deps = parse(trace, Path::new("/work"), Path::new("/work/helper")).unwrap();
    for (name, kind) in [
        ("/work/helper", PathKind::Executable),
        ("/work/parent", PathKind::File),
        ("/work/child", PathKind::File),
    ] {
        assert!(deps.contains(&ObservedPath {
            path: name.into(),
            kind
        }));
    }
}

#[test]
fn rejects_unknown_incomplete_and_forged_exit() {
    let prefix = "100 execve(\"/work/helper\", [...], 0x3 /* 4 vars */) = 0\n";
    for suffix in [
        "100 totally_unknown() = 0\n100 +++ exited with 0 +++\n",
        "100 getrandom(0x1, 0x8, 0) = 8\n100 +++ exited with 0 +++\n",
        "100 clone(child_stack=NULL, flags=SIGCHLD) = 101\n100 +++ exited with 0 +++\n",
        "100 +++ exited with 0 +++\n",
    ] {
        assert!(
            parse(
                &(prefix.to_owned() + suffix),
                Path::new("/work"),
                Path::new("/work/helper")
            )
            .is_err()
        );
    }
}

#[test]
fn parser_rejects_nonabsolute_executable_without_rejecting_valid_control() {
    let valid = "100 execve(\"/work/helper\", [...], 0x3 /* 4 vars */) = 0\n100 exit(0) = ?\n100 +++ exited with 0 +++\n";
    assert!(parse(valid, Path::new("/work"), Path::new("/work/helper")).is_ok());
    let relative = valid.replace("/work/helper", "helper");
    assert!(parse(&relative, Path::new("/work"), Path::new("helper")).is_err());
}

#[test]
fn parser_requires_initial_exec_and_root_completion() {
    let cwd = Path::new("/work");
    let exe = Path::new("/work/helper");
    let prefix = "100 execve(\"/work/helper\", [...], 0x3 /* 4 vars */) = 0\n";
    let finished = "100 exit(0) = ?\n100 +++ exited with 0 +++\n";
    assert!(parse(&(prefix.to_owned() + finished), cwd, exe).is_ok());
    assert!(parse(finished, cwd, exe).is_err());
    assert!(parse(prefix, cwd, exe).is_err());
}

#[test]
#[cfg(target_arch = "aarch64")]
#[ignore = "requires pinned tracer"]
fn run_rejects_noncanonical_inputs_before_spawning() {
    assert!(std::env::var_os("KIOKU_STRACE").is_some());
    let cwd = std::env::current_dir().unwrap();
    let executable = support::true_program();
    let control = kioku::trace::run(&[executable.to_string_lossy().into_owned()], &cwd);
    assert!(control.child_status.is_some_and(|status| status.success()));
    let relative_cwd = Path::new(".");
    let noncanonical_executable = format!(
        "{}/../{}/{}",
        executable.parent().unwrap().display(),
        executable
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy(),
        executable.file_name().unwrap().to_string_lossy()
    );
    let noncanonical_cwd = cwd.join("..").join(cwd.file_name().unwrap());
    for (program, dir) in [
        (executable.to_string_lossy().into_owned(), relative_cwd),
        (noncanonical_executable, cwd.as_path()),
        (
            executable.to_string_lossy().into_owned(),
            noncanonical_cwd.as_path(),
        ),
    ] {
        let result = kioku::trace::run(&[program], dir);
        assert!(
            result.child_status.is_none(),
            "invalid inputs must not run a child"
        );
    }
    let relative_cwd = kioku::trace::run_resolved(
        &[executable.to_string_lossy().into_owned()],
        Path::new("."),
        &executable,
    );
    assert!(
        relative_cwd.child_status.is_none(),
        "a relative working directory must be rejected before spawning"
    );
}

#[test]
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[ignore = "requires pinned tracer"]
fn dynamic_executable_is_denied_after_successful_execution() {
    assert!(std::env::var_os("KIOKU_STRACE").is_some());
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("exit.S");
    let interpreter = dir.path().join("interpreter");
    let executable = dir.path().join("dynamic");
    std::fs::write(
        &source,
        ".global _start\n.text\n_start:\n mov x0, 0\n mov x8, 93\n svc 0\n",
    )
    .unwrap();
    assert!(
        std::process::Command::new(support::executable("cc"))
            .args(["-nostdlib", "-static"])
            .arg(&source)
            .arg("-o")
            .arg(&interpreter)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        std::process::Command::new(support::executable("cc"))
            .args(["-nostdlib", "-pie"])
            .arg(format!("-Wl,--dynamic-linker={}", interpreter.display()))
            .arg(&source)
            .arg("-o")
            .arg(&executable)
            .status()
            .unwrap()
            .success()
    );
    let cwd = std::env::current_dir().unwrap();
    let outcome = kioku::trace::run(&[executable.to_string_lossy().into_owned()], &cwd);
    assert!(outcome.child_status.is_some_and(|status| status.success()));
    assert!(
        matches!(
            outcome.observation,
            kioku::trace::TraceObservation::Denied(kioku::trace::DenyReason::ExecutableFormat)
        ),
        "{:?}",
        outcome.observation
    );
}

#[test]
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[ignore = "requires pinned tracer"]
fn a_trace_larger_than_the_pipe_is_not_truncated_after_child_exit() {
    assert!(std::env::var_os("KIOKU_STRACE").is_some());
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("loop.S");
    let executable = dir.path().join("loop");
    std::fs::write(&source, include_str!("false_green_zoo/loop_aarch64.S")).unwrap();
    std::fs::write(dir.path().join("input.txt"), b"x\n").unwrap();
    assert!(
        std::process::Command::new(support::executable("cc"))
            .args(["-nostdlib", "-static"])
            .arg(&source)
            .arg("-o")
            .arg(&executable)
            .status()
            .unwrap()
            .success()
    );
    let outcome = kioku::trace::run(&[executable.to_string_lossy().into_owned()], dir.path());
    assert!(outcome.child_status.is_some_and(|status| status.success()));
    let kioku::trace::TraceObservation::Complete(observation) = outcome.observation else {
        panic!("large trace was truncated: {:?}", outcome.observation);
    };
    assert!(observation.dependencies.contains(&ObservedPath {
        path: dir.path().join("input.txt"),
        kind: PathKind::File,
    }));
}

#[test]
#[ignore = "requires pinned tracer and static x86_64 helper"]
fn x86_64_static_helper_is_traced() {
    if Arch::current() != Some(Arch::X86_64) {
        return;
    }
    let helper =
        std::fs::canonicalize(std::env::var("KIOKU_TRACE_TEST_HELPER").expect("helper path"))
            .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input.txt");
    std::fs::write(&input, b"x\n").unwrap();
    let outcome = kioku::trace::run(
        &[
            helper.to_string_lossy().into_owned(),
            input.to_string_lossy().into_owned(),
        ],
        dir.path(),
    );
    assert!(outcome.child_status.is_some_and(|status| status.success()));
    let kioku::trace::TraceObservation::Complete(observation) = outcome.observation else {
        panic!("x86_64 trace denied: {:?}", outcome.observation);
    };
    assert!(observation.dependencies.contains(&ObservedPath {
        path: helper,
        kind: PathKind::Executable,
    }));
    assert!(observation.dependencies.contains(&ObservedPath {
        path: input,
        kind: PathKind::File,
    }));
}

#[test]
#[ignore = "requires pinned tracer and static aarch64 test helper"]
fn real_static_parent_and_fork_child_are_observed() {
    let binary = std::env::var("KIOKU_TRACE_TEST_HELPER").unwrap();
    let input = std::env::current_dir()
        .unwrap()
        .join("tests/false_green_zoo/read_aarch64.S");
    let child_input = std::env::current_dir().unwrap().join("Cargo.toml");
    let cwd = std::env::current_dir().unwrap();
    for (index, args) in [
        vec![binary.clone(), input.to_string_lossy().into_owned()],
        vec![
            binary.clone(),
            input.to_string_lossy().into_owned(),
            child_input.to_string_lossy().into_owned(),
        ],
    ]
    .into_iter()
    .enumerate()
    {
        let outcome = kioku::trace::run(&args, &cwd);
        assert!(
            outcome.child_status.is_some_and(|status| status.success()),
            "unknown or failed status"
        );
        let kioku::trace::TraceObservation::Complete(observation) = outcome.observation else {
            panic!("trace denied case {index}: {:?}", outcome.observation);
        };
        assert!(observation.dependencies.contains(&ObservedPath {
            path: input.clone(),
            kind: PathKind::File
        }));
        if index == 1 {
            assert!(observation.dependencies.contains(&ObservedPath {
                path: child_input.clone(),
                kind: PathKind::File
            }));
        }
        assert!(observation.dependencies.contains(&ObservedPath {
            path: binary.clone().into(),
            kind: PathKind::Executable
        }));
    }
}

#[test]
fn reasons_distinguish_denied_nondeterministic_operations() {
    use kioku::trace::DenyReason;
    let prefix = "100 execve(\"/work/helper\", [...], 0x3 /* 4 vars */) = 0\n";
    for (record, reason) in [
        ("socket(2, 1, 0) = 3", DenyReason::Network),
        ("clock_gettime(1, 0x1) = 0", DenyReason::Clock),
        ("getrandom(0x1, 0x8, 0) = 8", DenyReason::Random),
        ("read(0x0, 0x1, 0x8) = 0x8", DenyReason::Stdin),
        (
            "openat(AT_FDCWD, \"/proc/version\", O_RDONLY) = 3",
            DenyReason::SystemPath,
        ),
        ("totally_unknown() = 0", DenyReason::Unknown),
    ] {
        let actual = parse(
            &(prefix.to_owned() + "100 " + record + "\n"),
            Path::new("/work"),
            Path::new("/work/helper"),
        );
        assert_eq!(actual, Err(reason));
    }
}

#[test]
#[ignore = "requires pinned tracer and static aarch64 exec helper"]
fn real_child_exec_captures_executable_and_input() {
    let parent = std::env::var("KIOKU_TRACE_EXEC_HELPER").unwrap();
    let child = std::env::var("KIOKU_TRACE_TEST_HELPER").unwrap();
    let cwd = std::env::current_dir().unwrap();
    let input = cwd.join("Cargo.toml");
    let args = vec![
        parent.clone(),
        child.clone(),
        input.to_string_lossy().into_owned(),
    ];
    let outcome = kioku::trace::run(&args, &cwd);
    assert!(
        outcome.child_status.is_some_and(|status| status.success()),
        "unknown or failed status"
    );
    let kioku::trace::TraceObservation::Complete(observation) = outcome.observation else {
        panic!("trace denied: {:?}", outcome.observation);
    };
    for (path, kind) in [
        (parent.into(), PathKind::Executable),
        (child.into(), PathKind::Executable),
        (input, PathKind::File),
    ] {
        assert!(
            observation
                .dependencies
                .contains(&ObservedPath { path, kind })
        );
    }
}

#[test]
fn chdir_and_openat_directory_fd_preserve_context_across_fork() {
    let trace = concat!(
        "100 execve(\"/work/helper\", [...], 0x3 /* 4 vars */) = 0\n",
        "100 chdir(\"/work/sub\") = 0\n",
        "100 openat(AT_FDCWD, \"dir\", O_RDONLY|O_DIRECTORY) = 3\n",
        "100 clone(child_stack=NULL, flags=SIGCHLD) = 101\n",
        "101 openat(3, \"input\", O_RDONLY) = 4\n",
        "101 close(4) = 0\n",
        "101 close(3) = 0\n",
        "101 exit(0) = ?\n",
        "101 +++ exited with 0 +++\n",
        "100 close(3) = 0\n",
        "100 exit(0) = ?\n",
        "100 +++ exited with 0 +++\n"
    );
    let actual = parse(trace, Path::new("/work"), Path::new("/work/helper")).unwrap();
    assert!(actual.contains(&ObservedPath {
        path: "/work/sub/dir/input".into(),
        kind: PathKind::File
    }));
}

#[test]
fn metadata_directory_and_private_mapping_are_dependencies() {
    let trace = concat!(
        "100 execve(\"/work/helper\", [...], 0x3 /* 4 vars */) = 0\n",
        "100 newfstatat(AT_FDCWD, \"/work/input\", {st_mode=S_IFREG|0644, st_size=14, ...}, 0) = 0\n",
        "100 openat(AT_FDCWD, \"/work/input\", O_RDONLY) = 3\n",
        "100 mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, 3, 0) = 0x1000\n",
        "100 openat(AT_FDCWD, \"/work/dir\", O_RDONLY|O_DIRECTORY) = 4\n",
        "100 getdents64(4, 0x123, 4096) = 48\n",
        "100 exit(0) = ?\n100 +++ exited with 0 +++\n"
    );
    let deps = parse(trace, Path::new("/work"), Path::new("/work/helper")).unwrap();
    assert!(deps.contains(&ObservedPath {
        path: "/work/input".into(),
        kind: PathKind::Metadata
    }));
    assert!(deps.contains(&ObservedPath {
        path: "/work/dir".into(),
        kind: PathKind::Directory
    }));
}

#[test]
#[ignore = "requires pinned tracer and static operation helper"]
fn real_operations_are_modeled() {
    let binary = std::env::var("KIOKU_TRACE_OPS_HELPER").unwrap();
    let cwd = std::env::current_dir().unwrap();
    for (mode, path, extra) in [
        ("c", cwd.join("tests"), "trace.rs"),
        ("d", cwd.join("tests"), "trace.rs"),
        ("s", cwd.join("Cargo.toml"), ""),
        ("l", cwd.join("tests"), ""),
        ("m", cwd.join("Cargo.toml"), ""),
    ] {
        let args = vec![
            binary.clone(),
            mode.into(),
            path.to_string_lossy().into_owned(),
            extra.into(),
        ];
        let outcome = kioku::trace::run(&args, &cwd);
        assert!(
            outcome.child_status.is_some_and(|s| s.success()),
            "{mode}: unknown or failed exit"
        );
        assert!(
            matches!(
                outcome.observation,
                kioku::trace::TraceObservation::Complete(_)
            ),
            "{mode}: {:?}",
            outcome.observation
        );
    }
}

#[test]
fn unfinished_child_exec_is_completed_only_on_successful_resume() {
    let text = concat!(
        "100 execve(\"/work/helper\", [...], 0x3 /* 4 vars */) = 0\n",
        "100 clone(child_stack=NULL, flags=SIGCHLD) = 101\n",
        "101 execve(\"/work/child\", [...], NULL <unfinished ...>\n",
        "100 wait4(101,  <unfinished ...>\n",
        "101 <... execve resumed>)           = 0\n",
        "101 exit(0) = ?\n101 +++ exited with 0 +++\n",
        "100 <... wait4 resumed>[{WIFEXITED(s) && WEXITSTATUS(s) == 0}], 0, NULL) = 101\n",
        "100 exit(0) = ?\n100 +++ exited with 0 +++\n"
    );
    let deps = parse(text, Path::new("/work"), Path::new("/work/helper")).unwrap();
    assert!(deps.contains(&ObservedPath {
        path: "/work/child".into(),
        kind: PathKind::Executable
    }));
    let with_env = text.replace(
        "[...], NULL <unfinished ...>",
        "[...], 0x1234 /* 1 vars */ <unfinished ...>",
    );
    assert_eq!(
        parse(&with_env, Path::new("/work"), Path::new("/work/helper")).unwrap(),
        deps
    );
    assert!(
        parse(
            &text.replace(
                "resumed>)           = 0",
                "resumed>)           = -1 ENOENT (No such file or directory)"
            ),
            Path::new("/work"),
            Path::new("/work/helper")
        )
        .is_err()
    );
}

#[test]
#[ignore = "requires pinned tracer, static helper, and isolated TMPDIR"]
fn real_fifo_is_removed_after_tracing() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let status = std::process::Command::new("git")
        .args(["init", "--quiet", repo.to_string_lossy().as_ref()])
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::write(repo.join("input"), b"x\n").unwrap();
    let helper = std::env::var("KIOKU_TRACE_TEST_HELPER").unwrap();
    let tracer = std::env::var("KIOKU_STRACE").unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_kioku"))
        .current_dir(&repo)
        .env("TMPDIR", tmp.path())
        .env("KIOKU_STRACE", tracer)
        .args(["run", "--trust-trace", "--", &helper, "input"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|entry| {
            let name = entry.ok()?.file_name();
            name.to_string_lossy()
                .starts_with("kioku-trace-")
                .then_some(name)
        })
        .collect();
    assert!(
        leftovers.is_empty(),
        "trace directory left behind: {leftovers:?}"
    );
}

#[test]
fn absence_with_errno_parentheses_is_recorded() {
    let trace = concat!(
        "100 execve(\"/work/helper\", [...], 0x3 /* 4 vars */) = 0\n",
        "100 openat(AT_FDCWD, \"/work/missing\", O_RDONLY) = -1 ENOENT (No such file or directory)\n",
        "100 exit(0)                         = ?\n100 +++ exited with 0 +++\n"
    );
    let deps = parse(trace, Path::new("/work"), Path::new("/work/helper")).unwrap();
    assert!(deps.contains(&ObservedPath {
        path: "/work/missing".into(),
        kind: PathKind::Absent
    }));
}

#[test]
#[ignore = "requires pinned tracer"]
fn tracer_identity_reports_exact_spawn_flags() {
    let identity = kioku::trace::identity().unwrap();
    assert_eq!(
        identity.flags,
        [
            "-f",
            "-e",
            "trace=all",
            "-e",
            "raw=execveat,read,readv,pread64,preadv,preadv2,write,writev,pwrite64,pwritev,pwritev2,getrandom,recvfrom,recvmsg,sendto,sendmsg",
            "-s",
            "0"
        ]
    );
}

#[test]
fn denied_trace_never_exposes_synthetic_buffer_in_event_or_error() {
    let sentinel = "kioku-synthetic-secret-payload-7722";
    let prefix = "100 execve(\"/work/helper\", [...], 0x3 /* 4 vars */) = 0\n";
    for record in [
        format!("100 readv(0x3, 0x1234, 0x1) = 0x4 /* {sentinel} */\n"),
        format!("100 getrandom(0x1234, 0x4, 0) = 0x4 /* {sentinel} */\n"),
    ] {
        let result = parse(
            &(prefix.to_owned() + &record),
            Path::new("/work"),
            Path::new("/work/helper"),
        );
        assert!(result.is_err());
        assert!(!format!("{result:?}").contains(sentinel));
    }
}

#[test]
fn unfinished_open_is_paired_per_task_and_orphans_are_rejected() {
    let trace = concat!(
        "100 execve(\"/work/helper\", [...], 0x3 /* 4 vars */) = 0\n",
        "100 clone(child_stack=NULL, flags=SIGCHLD) = 101\n",
        "101 openat(AT_FDCWD, \"/work/data\", O_RDONLY <unfinished ...>\n",
        "100 wait4(101,  <unfinished ...>\n",
        "101 <... openat resumed>) = 3\n",
        "101 close(3) = 0\n",
        "101 exit(0) = ?\n101 +++ exited with 0 +++\n",
        "100 <... wait4 resumed>[{WIFEXITED(s) && WEXITSTATUS(s) == 0}], 0, NULL) = 101\n",
        "100 exit(0) = ?\n100 +++ exited with 0 +++\n"
    );
    let deps = parse(trace, Path::new("/work"), Path::new("/work/helper")).unwrap();
    assert!(deps.contains(&ObservedPath {
        path: "/work/data".into(),
        kind: PathKind::File
    }));
    assert!(
        parse(
            &trace.replace(
                "101 openat(AT_FDCWD, \"/work/data\", O_RDONLY <unfinished ...>\n",
                ""
            ),
            Path::new("/work"),
            Path::new("/work/helper")
        )
        .is_err()
    );
}

#[test]
fn orphan_exec_resume_and_unfinished_initial_exec_are_rejected() {
    let root = "100 execve(\"/work/helper\", [...], 0x3 /* 4 vars */) = 0\n";
    let orphan = format!(
        "{root}100 <... execve resumed>) = 0\n100 exit(0) = ?\n100 +++ exited with 0 +++\n"
    );
    let initial = "100 execve(\"/work/helper\", [...], NULL <unfinished ...>\n100 <... execve resumed>) = 0\n100 exit(0) = ?\n100 +++ exited with 0 +++\n";
    for trace in [&orphan, &initial.to_string()] {
        assert!(parse(trace, Path::new("/work"), Path::new("/work/helper")).is_err());
    }
}

#[test]
fn pending_wait_rejects_mismatched_resume_duplicate_and_eof() {
    let prefix = concat!(
        "100 execve(\"/work/helper\", [...], 0x3 /* 4 vars */) = 0\n",
        "100 clone(child_stack=NULL, flags=SIGCHLD) = 101\n",
        "100 wait4(101,  <unfinished ...>\n"
    );
    for tail in [
        "100 <... openat resumed>) = 3\n",
        "100 wait4(101,  <unfinished ...>\n",
        "",
        "100 exit(0) = ?\n",
    ] {
        assert!(
            parse(
                &(prefix.to_owned() + tail),
                Path::new("/work"),
                Path::new("/work/helper")
            )
            .is_err()
        );
    }
}

#[test]
fn unfinished_clone_binds_child_only_from_matching_parent_resume() {
    let prefix = "100 execve(\"/work/helper\", [...], 0x3 /* 4 vars */) = 0\n";
    let start = "100 clone(child_stack=NULL, flags=SIGCHLD <unfinished ...>\n";
    let child = "101 execve(\"/work/child\", [...], NULL <unfinished ...>\n";
    let resume = "100 <... clone resumed>) = 101\n";
    let tail = concat!(
        "101 <... execve resumed>) = 0\n",
        "101 exit(0) = ?\n101 +++ exited with 0 +++\n",
        "100 exit(0) = ?\n100 +++ exited with 0 +++\n"
    );
    let good = format!("{prefix}{start}{child}{resume}{tail}");
    let deps = parse(&good, Path::new("/work"), Path::new("/work/helper")).unwrap();
    assert!(deps.contains(&ObservedPath {
        path: "/work/child".into(),
        kind: PathKind::Executable
    }));
    for bad in [
        good.replace(start, ""),
        good.replace(resume, ""),
        good.replace(resume, "102 <... clone resumed>) = 101\n"),
        good.replace(resume, "100 <... clone resumed>) = 100\n"),
        good.replace(resume, &format!("{resume}{resume}")),
        good.replace(resume, "100 close(3) = 0\n100 <... clone resumed>) = 101\n"),
    ] {
        assert!(parse(&bad, Path::new("/work"), Path::new("/work/helper")).is_err());
    }
}

const LIMIT: usize = 16 * 1024 * 1024;
const PREFIX: &str = "100 execve(\"/work/helper\", [...], 0x3 /* 4 vars */) = 0\n100 openat(AT_FDCWD, \"/work/input\", O_RDONLY) = 3\n";
const SUFFIX: &str = "100 close(3) = 0\n100 exit(0) = ?\n100 +++ exited with 0 +++\n";

fn exact_trace(size: usize) -> String {
    let mut s = PREFIX.to_owned();
    let line = format!("100 read(0x3, 0x0, 0x0){} = 0\n", " ".repeat(8000));
    while s.len() + line.len() + SUFFIX.len() <= size {
        s.push_str(&line);
    }
    let padding = size - s.len() - SUFFIX.len();
    if padding > 0 {
        let insertion = s.find(" = ").unwrap();
        s.insert_str(insertion, &" ".repeat(padding));
    }
    s.push_str(SUFFIX);
    assert_eq!(s.len(), size);
    s
}

#[test]
fn parse_absolute_argument_contract() {
    let trace = format!("{PREFIX}{SUFFIX}");
    assert!(parse(&trace, Path::new("/work"), Path::new("/work/helper")).is_ok());
    assert!(parse(&trace, Path::new("work"), Path::new("/work/helper")).is_err());
    assert!(parse(&trace, Path::new("/work"), Path::new("work/helper")).is_err());
    assert!(parse(&trace, Path::new("work"), Path::new("work/helper")).is_err());
}

#[test]
fn parser_accepts_exact_byte_limit_and_rejects_next_byte() {
    let trace = exact_trace(LIMIT);
    assert!(parse(&trace, Path::new("/work"), Path::new("/work/helper")).is_ok());
    let too_large = format!("{trace}\n");
    assert!(parse(&too_large, Path::new("/work"), Path::new("/work/helper")).is_err());
}

#[test]
fn parser_accepts_exact_line_limit_and_rejects_next_byte() {
    let first = PREFIX.lines().next().unwrap();
    let tail = PREFIX.strip_prefix(first).unwrap();
    for n in [8192, 8193] {
        let (left, right) = first.rsplit_once(" = ").unwrap();
        let trace = format!(
            "{left}{} = {right}{tail}{SUFFIX}",
            " ".repeat(n - first.len())
        );
        assert_eq!(
            parse(&trace, Path::new("/work"), Path::new("/work/helper")).is_ok(),
            n == 8192
        );
    }
}

#[test]
fn rejects_records_after_exit_and_unknown_tasks() {
    let trace = format!("{PREFIX}{SUFFIX}");
    assert!(
        parse(
            &(trace.clone() + "100 execve(\"/work/other\", [...], NULL) = 0\n"),
            Path::new("/work"),
            Path::new("/work/helper")
        )
        .is_err()
    );
    assert!(
        parse(
            &(trace + "101 execve(\"/work/other\", [...], NULL) = 0\n"),
            Path::new("/work"),
            Path::new("/work/helper")
        )
        .is_err()
    );
}

#[test]
fn unfinished_exec_requires_initial_exec_and_no_open_descriptors() {
    let first = concat!(
        "100 execve(\"/work/helper\", [...], NULL <unfinished ...>\n",
        "100 <... execve resumed>) = 0\n",
        "100 exit(0) = ?\n100 +++ exited with 0 +++\n"
    );
    let open = concat!(
        "100 execve(\"/work/helper\", [...], 0x3 /* 4 vars */) = 0\n",
        "100 openat(AT_FDCWD, \"/work/input\", O_RDONLY) = 3\n",
        "100 execve(\"/work/child\", [...], NULL <unfinished ...>\n",
        "100 <... execve resumed>) = 0\n",
        "100 close(3) = 0\n100 exit(0) = ?\n100 +++ exited with 0 +++\n"
    );
    for trace in [first, open] {
        assert!(parse(trace, Path::new("/work"), Path::new("/work/helper")).is_err());
    }
}

#[test]
fn exec_redacted_env_requires_complete_shape() {
    for env in ["[...], 0x3 /* broken suffix", "garbage vars */) = 0"] {
        let trace = format!(
            "100 execve(\"/work/helper\", {env}\n100 exit(0) = ?\n100 +++ exited with 0 +++\n"
        );
        assert!(parse(&trace, Path::new("/work"), Path::new("/work/helper")).is_err());
    }
}

#[test]
fn initial_exec_must_match_requested_executable() {
    let matching = format!("{PREFIX}{SUFFIX}");
    let mismatched = matching.replacen("/work/helper", "/work/other", 1);
    assert!(parse(&matching, Path::new("/work"), Path::new("/work/helper")).is_ok());
    assert!(parse(&mismatched, Path::new("/work"), Path::new("/work/helper")).is_err());
}

#[test]
fn unfinished_directory_open_preserves_metadata_and_dirfd() {
    let trace = concat!(
        "100 execve(\"/work/helper\", [...], 0x3 /* 4 vars */) = 0\n",
        "100 openat(AT_FDCWD, \"/work/dir\", O_RDONLY|O_DIRECTORY <unfinished ...>\n",
        "100 <... openat resumed>) = 3\n",
        "100 openat(3, \"input\", O_RDONLY) = 4\n",
        "100 close(4) = 0\n100 close(3) = 0\n100 exit(0) = ?\n100 +++ exited with 0 +++\n"
    );
    let deps = parse(trace, Path::new("/work"), Path::new("/work/helper")).unwrap();
    assert!(deps.contains(&ObservedPath {
        path: "/work/dir".into(),
        kind: PathKind::Metadata
    }));
    assert!(deps.contains(&ObservedPath {
        path: "/work/dir/input".into(),
        kind: PathKind::File
    }));
}

#[test]
fn metadata_directory_and_mapping_grammar_rejects_each_invalid_field() {
    let prefix = concat!(
        "100 execve(\"/work/helper\", [...], 0x3 /* 4 vars */) = 0\n",
        "100 openat(AT_FDCWD, \"/work/input\", O_RDONLY) = 3\n",
        "100 openat(AT_FDCWD, \"/work/dir\", O_RDONLY|O_DIRECTORY) = 4\n"
    );
    let suffix = "100 close(3) = 0\n100 close(4) = 0\n100 exit(0) = ?\n100 +++ exited with 0 +++\n";
    let check = |call: &str| {
        parse(
            &format!("{prefix}{call}\n{suffix}"),
            Path::new("/work"),
            Path::new("/work/helper"),
        )
    };
    let metadata = "100 newfstatat(AT_FDCWD, \"/work/input\", {st_mode=S_IFREG}, 0) = 0";
    let absence = "100 newfstatat(AT_FDCWD, \"/work/missing\", 0x0, 0) = -1 ENOENT (No such file or directory)";
    assert!(check(metadata).unwrap().contains(&ObservedPath {
        path: "/work/input".into(),
        kind: PathKind::Metadata
    }));
    assert!(check(absence).unwrap().contains(&ObservedPath {
        path: "/work/missing".into(),
        kind: PathKind::Absent
    }));
    let directory = "100 getdents64(4, 0x0, 4096) = 0";
    let mapping = "100 mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, 3, 0) = 0x1000";
    assert!(check(directory).is_ok());
    assert!(check(mapping).is_ok());
    for invalid in [
        metadata.replace("{st_mode=", "{bad="),
        metadata.replace("}, 0) = 0", "}, 0) = -1 EACCES (Permission denied)"),
        absence.replace(
            "ENOENT (No such file or directory)",
            "EACCES (Permission denied)",
        ),
        directory.replace("getdents64(4,", "getdents64(3,"),
        directory.replace(") = 0", ") = -1 EIO (Input/output error)"),
        mapping.replace("4096", "invalid"),
        mapping.replace("PROT_READ", "PROT_WRITE"),
        mapping.replace("MAP_PRIVATE", "MAP_SHARED"),
        mapping.replace(", 3, 0)", ", 3, invalid)"),
        mapping.replace("= 0x1000", "= -1 ENOMEM (Cannot allocate memory)"),
    ] {
        assert!(check(&invalid).is_err());
    }
}

#[test]
fn lifecycle_wait_signal_and_exit_reject_inconsistent_records() {
    let prefix = concat!(
        "100 execve(\"/work/helper\", [...], 0x3 /* 4 vars */) = 0\n",
        "100 clone(child_stack=NULL, flags=SIGCHLD) = 101\n",
        "101 exit(0) = ?\n101 +++ exited with 0 +++\n"
    );
    let signal = "100 --- SIGCHLD {si_code=CLD_EXITED, si_pid=101, si_status=0} ---\n";
    let waiting = "100 wait4(101,  <unfinished ...>\n100 <... wait4 resumed>) = 101\n";
    let suffix = "100 exit(0) = ?\n100 +++ exited with 0 +++\n";
    let check = |body: &str| {
        parse(
            &format!("{prefix}{body}{suffix}"),
            Path::new("/work"),
            Path::new("/work/helper"),
        )
    };
    assert!(check(&format!("{signal}{waiting}")).is_ok());
    for invalid in [
        signal.replace("CLD_EXITED", "CLD_KILLED"),
        signal.replace("si_status=0", "si_status=1"),
        signal.replace("si_pid=101", "si_pid=102"),
        waiting.replace("resumed>) = 101", "resumed>) = 102"),
        "100 exit(0) = ?\n".to_owned(),
    ] {
        assert!(check(&invalid).is_err());
    }
}

#[test]
fn malformed_clone_records_report_the_correct_line() {
    use kioku::trace::DenyReason;
    let first = "100 execve(\"/work/helper\", [...], 0x3 /* 4 vars */) = 0\n";
    let pending = "100 clone(child_stack=NULL, flags=SIGCHLD <unfinished ...>\n";
    let cases = [
        ("missing-space".to_owned(), 1),
        ("not-a-pid exit(0) = ?\n".to_owned(), 1),
        (format!("{first}{pending}{pending}"), 3),
        (
            format!("{first}{pending}100 <... clone resumed>) missing\n"),
            3,
        ),
        (
            format!("{first}{pending}100 <... clone resumed>) = invalid\n"),
            3,
        ),
        (format!("{first}100 <... clone resumed>) = 101\n"), 2),
        (format!("{first}{pending}100 exit(0) = ?\n"), 3),
    ];
    for (trace, line) in cases {
        assert_eq!(
            parse(&trace, Path::new("/work"), Path::new("/work/helper")),
            Err(DenyReason::Parse(line))
        );
    }
}

#[test]
fn read_from_an_unopened_descriptor_is_denied() {
    let trace = format!("{PREFIX}100 read(0x9, 0x123, 0x10) = 0x10\n100 exit_group(0) = ?\n");
    assert_eq!(
        parse(&trace, Path::new("/work"), Path::new("/work/helper")),
        Err(DenyReason::Parse(3))
    );
}

#[test]
fn completed_second_exec_after_open_is_denied() {
    let trace =
        format!("{PREFIX}100 execve(\"/work/helper\", [...], 0x3 /* 4 vars */) = 0\n{SUFFIX}");
    assert_eq!(
        parse(&trace, Path::new("/work"), Path::new("/work/helper")),
        Err(DenyReason::Parse(3))
    );
}

#[test]
fn accepted_open_metadata_flags_do_not_deny() {
    let trace = format!(
        "{PREFIX}100 openat(AT_FDCWD, \"/work/other\", O_RDONLY|O_CLOEXEC|O_LARGEFILE|O_NOFOLLOW) = 4\n100 close(4) = 0\n{SUFFIX}"
    );
    let deps = parse(&trace, Path::new("/work"), Path::new("/work/helper")).unwrap();
    assert!(deps.contains(&ObservedPath {
        path: "/work/other".into(),
        kind: PathKind::File,
    }));
}

#[test]
fn invalid_metadata_records_are_denied() {
    let cases = [
        "100 faccessat(AT_FDCWD, \"/work/input\", F_OK) = -1 EACCES (Permission denied)",
        "100 readlinkat(AT_FDCWD, \"/work/input\", \"target\", 4096) = -1 EINVAL (Invalid argument)",
        "100 fstat(9, {st_mode=S_IFREG|0644, st_size=1}) = 0",
        "100 fstat(3, {bad=1}) = 0",
        "100 fstat(3, {st_mode=S_IFREG|0644, st_size=1}) = -1 EIO (Input/output error)",
        "100 fcntl(9, F_GETFD) = 0",
        "100 fcntl(3, F_GETFL) = 0",
        "100 lseek(9, 0, SEEK_SET) = 0",
        "100 lseek(3, 0, SEEK_SET)",
    ];
    for call in cases {
        let trace = format!("{PREFIX}{call}\n{SUFFIX}");
        assert_eq!(
            parse(&trace, Path::new("/work"), Path::new("/work/helper")),
            Err(DenyReason::Unsupported),
            "{call}"
        );
    }
}

#[test]
fn write_records_keep_stdout_allowed_and_every_other_shape_denied() {
    let check = |call: &str| {
        let trace = format!("{PREFIX}{call}\n{SUFFIX}");
        parse(&trace, Path::new("/work"), Path::new("/work/helper"))
    };
    assert!(check("100 write(0x1, 0x0, 0x0) = 0").is_ok());
    assert!(check("100 write(0x1, 0x0, 0x0) = 0x1").is_ok());
    assert_eq!(
        check("100 write(0x3, 0x0, 0x0) = 0x1"),
        Err(DenyReason::Write)
    );
    assert_eq!(
        check("100 write(0x1, 0x0, 0x0) = -1 EIO (Input/output error)"),
        Err(DenyReason::Unsupported)
    );
}

#[test]
fn overlong_line_limit_is_unsupported_not_a_parse_error() {
    let line = format!("100 read(0x3, 0x0, 0x0){} = 0\n", " ".repeat(8193));
    let trace = format!("{PREFIX}{line}{SUFFIX}");
    assert_eq!(
        parse(&trace, Path::new("/work"), Path::new("/work/helper")),
        Err(DenyReason::Unsupported)
    );
}

#[test]
fn resolve_relative_normalizes_empty_and_dot_components() {
    let base = Path::new("/work");
    assert_eq!(
        kioku::trace::resolve_relative(base, "/work/./file"),
        Some("/work/file".into())
    );
    assert_eq!(
        kioku::trace::resolve_relative(base, "/work//file"),
        Some("/work/file".into())
    );
    assert_eq!(kioku::trace::resolve_relative(base, ""), None);
}

#[test]
fn quoted_paths_normalize_lexically_and_reject_escape_and_control_bytes() {
    use kioku::trace::DenyReason;
    let check = |path: &str| {
        let trace = format!(
            "{PREFIX}100 openat(AT_FDCWD, \"{path}\", O_RDONLY) = 4\n100 close(4) = 0\n{SUFFIX}"
        );
        parse(&trace, Path::new("/work"), Path::new("/work/helper"))
    };
    for (path, expected) in [
        ("file", "/work/file"),
        ("/work/file", "/work/file"),
        ("/work/./file", "/work/file"),
        ("/work//file", "/work/file"),
        ("/work/../file", "/file"),
        ("./file", "/work/file"),
        ("../file", "/file"),
        ("a//b", "/work/a/b"),
        ("a/../b", "/work/b"),
    ] {
        assert!(
            check(path).unwrap().contains(&ObservedPath {
                path: expected.into(),
                kind: PathKind::File
            }),
            "path: {path:?}"
        );
    }
    for path in ["", r"bad\name", "../../file", "../../../file"] {
        assert_eq!(check(path), Err(DenyReason::Unsupported), "path: {path:?}");
    }
}
