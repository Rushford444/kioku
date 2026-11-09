//! Shared helpers for integration tests.

use std::path::PathBuf;

/// Resolve a program before a test clears the child environment or changes directory.
#[allow(dead_code)]
pub fn executable(name: &str) -> PathBuf {
    let path = std::env::var_os("PATH").expect("PATH must be set for integration tests");
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                candidate
                    .metadata()
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            }
            #[cfg(not(unix))]
            {
                candidate.is_file()
            }
        })
        .unwrap_or_else(|| panic!("{name} must be available on PATH"))
        .canonicalize()
        .unwrap_or_else(|err| panic!("cannot canonicalize {name}: {err}"))
}

/// Resolve a program that succeeds even when passed arguments.
#[allow(dead_code)]
pub fn true_program() -> PathBuf {
    #[cfg(unix)]
    {
        executable("true")
    }
    #[cfg(windows)]
    {
        static FIXTURE: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
        FIXTURE
            .get_or_init(|| {
                let dir = tempfile::tempdir().expect("temporary fixture directory");
                let source = dir.path().join("true.rs");
                std::fs::write(&source, "fn main() {}\n").expect("write success fixture");
                let status = std::process::Command::new("rustc")
                    .arg(&source)
                    .arg("-o")
                    .arg(dir.path().join("true.exe"))
                    .status()
                    .expect("rustc must be available on PATH");
                assert!(
                    status.success(),
                    "compiling success fixture failed: {status}"
                );
                dir
            })
            .path()
            .join("true.exe")
    }
}

#[cfg(test)]
#[test]
fn true_program_succeeds_with_closed_stdin_and_empty_environment() {
    let status = std::process::Command::new(true_program())
        .args(["synthetic-argument", "--ignored"])
        .stdin(std::process::Stdio::null())
        .env_clear()
        .status()
        .expect("the success program must start");
    assert!(status.success(), "success program returned {status}");
}
