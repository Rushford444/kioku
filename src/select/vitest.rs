//! Vitest execution fallback. Selection is deliberately disabled until the import
//! graph, base inventory and native coverage can all be validated together.

use std::path::Path;
use std::process::{Command, ExitCode, ExitStatus};

pub(super) fn run(repo: &Path, json: bool, quiet: bool) -> ExitCode {
    let executable = match repo.canonicalize() {
        Ok(root) => root.join("node_modules").join(".bin").join("vitest"),
        Err(error) => {
            super::print_error(json, "select", &format!("Vitest repository: {error}"));
            return ExitCode::from(3);
        }
    };
    if json {
        eprintln!(
            "{}",
            serde_json::json!({
                "schema": "kioku/v1", "event": "vitest-fallback",
                "status": "incomplete", "reason": "selection-not-available",
                "scope": "full"
            })
        );
    } else if !quiet {
        eprintln!(
            "kioku: Vitest selection unavailable; running the full suite (incomplete, exit 3 on success)"
        );
    }
    match Command::new(executable)
        .arg("run")
        .current_dir(repo)
        .status()
    {
        Ok(status) => ExitCode::from(exit_code_of(status)),
        Err(error) => {
            super::print_error(
                json,
                "select",
                &format!("cannot start local Vitest: {error}"),
            );
            ExitCode::from(3)
        }
    }
}

/// Exit code of an observed end of the fallback run: `3` for a native success
/// (incomplete for selection), `1` for a native failure, `128 + signal` for a
/// child killed by a signal, as `kioku::js_run` already does. A status with
/// neither a code nor a signal stays `3`, never a success.
fn exit_code_of(status: ExitStatus) -> u8 {
    if status.success() {
        return 3;
    }
    match status.code() {
        Some(_) => 1,
        None => match signal_of(status) {
            Some(signal) => u8::try_from(128i32.saturating_add(signal)).unwrap_or(3),
            None => 3,
        },
    }
}

/// Signal that terminated the child, when it was not a normal exit.
#[cfg(unix)]
fn signal_of(status: ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt as _;
    status.signal()
}

/// No signal is observable on this platform: a status without a code stays
/// incomplete (`3`) rather than inventing a signal number.
#[cfg(not(unix))]
fn signal_of(_status: ExitStatus) -> Option<i32> {
    None
}

/// These cases need a real child and a POSIX shell, so they are Unix only.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn status_of(script: &str) -> ExitStatus {
        Command::new("sh")
            .arg("-c")
            .arg(script)
            .status()
            .expect("child")
    }

    #[test]
    fn a_native_success_stays_incomplete() {
        assert_eq!(exit_code_of(status_of("exit 0")), 3);
    }

    #[test]
    fn a_native_failure_stays_red() {
        assert_eq!(exit_code_of(status_of("exit 1")), 1);
        assert_eq!(exit_code_of(status_of("exit 5")), 1);
    }

    #[test]
    fn a_child_killed_by_a_signal_is_128_plus_the_signal() {
        let status = status_of("kill -TERM $$");
        assert_eq!(status.code(), None, "a signalled child has no exit code");
        assert_eq!(exit_code_of(status), 143);
    }
}
