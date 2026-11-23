//! Cooperative native execution. This is not an isolation boundary.
//!
//! The caller supplies an approved task and executable binding. Output is hashed
//! while drained, never retained here. CPU and process-tree completeness remain
//! unknown until an OS-backed supervisor provides that evidence.
use crate::ci2::domain::{
    Digest, EnvKind, Lease, NativeObservation, QueueItem, RuntimePort, StopMode, StopToken,
    TaskSpec, Validate,
};
use sha2::{Digest as _, Sha256};
use std::fs;
use std::io::{self, Read};
#[cfg(unix)]
use std::os::fd::OwnedFd;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::{
    fs::PermissionsExt,
    process::{CommandExt, ExitStatusExt},
};

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("runtime configuration rejected")]
    Configuration,
    #[error("execution stopped; state requires reconciliation")]
    Interrupted,
    #[error("output drainage incomplete; state requires reconciliation")]
    DrainIncomplete,
    #[error("native execution failed: {0}")]
    Io(#[from] io::Error),
    #[error("digest construction failed")]
    Digest,
}

/// One approved executable binding for one task; no implicit PATH lookup.
pub struct LocalProcessRunner {
    pub work_root: PathBuf,
    pub private_home: PathBuf,
    pub executable: PathBuf,
    pub task: TaskSpec,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
}

struct Capture {
    hash: Sha256,
    bytes: u64,
    limit: u64,
    done: bool,
}

impl Capture {
    fn new(limit: u64) -> Self {
        let mut hash = Sha256::new();
        hash.update(b"kioku/ci2/native-stream/v1\0");
        Self {
            hash,
            bytes: 0,
            limit,
            done: false,
        }
    }

    fn drain(&mut self, input: &mut impl Read) -> io::Result<()> {
        let mut buf = [0_u8; 8192];
        // Bound work per iteration so an active writer cannot starve timeout checks.
        for _ in 0..16 {
            match input.read(&mut buf) {
                Ok(0) => {
                    self.done = true;
                    break;
                }
                Ok(n) => {
                    self.bytes = self
                        .bytes
                        .checked_add(n as u64)
                        .ok_or_else(|| io::Error::other("stream too large"))?;
                    if self.bytes > self.limit {
                        return Err(io::Error::other("stream limit exceeded"));
                    }
                    self.hash.update(&buf[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn digest(self) -> Result<Digest, RuntimeError> {
        Digest::new(&format!("sha256:{:x}", self.hash.finalize())).map_err(|_| RuntimeError::Digest)
    }
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
fn elapsed_ns(start: Instant) -> u64 {
    start.elapsed().as_nanos().try_into().unwrap_or(u64::MAX)
}
fn canonical_directory(path: &Path) -> Result<PathBuf, RuntimeError> {
    if !path.is_absolute() || !path.is_dir() || fs::canonicalize(path)? != path {
        return Err(RuntimeError::Configuration);
    }
    Ok(path.to_path_buf())
}
fn stop_requested(stop: &StopToken) -> Result<bool, RuntimeError> {
    if !Path::new(&stop.stop_file).is_absolute() {
        return Err(RuntimeError::Configuration);
    }
    match fs::symlink_metadata(&stop.stop_file) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(stop.state != StopMode::None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(unix)]
fn signal_group(pid: u32, signal: &str) -> io::Result<()> {
    // Negative group id is generated from the child returned by spawn, not user text.
    let status = Command::new("/bin/kill")
        .env_clear()
        .arg(signal)
        .arg("--")
        .arg(format!("-{pid}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("group signal failed"))
    }
}

#[cfg(unix)]
impl RuntimePort for LocalProcessRunner {
    type Error = RuntimeError;
    #[allow(clippy::too_many_lines)]
    fn execute(
        &mut self,
        item: &QueueItem,
        lease: &Lease,
        stop: &StopToken,
    ) -> Result<NativeObservation, Self::Error> {
        if item.task_id != self.task.id
            || lease.item_id != item.item_id
            || lease.epoch != item.expected_epoch
        {
            return Err(RuntimeError::Configuration);
        }
        let root = canonical_directory(&self.work_root)?;
        let home = canonical_directory(&self.private_home)?;
        if fs::metadata(&home)?.permissions().mode() & 0o077 != 0 {
            return Err(RuntimeError::Configuration);
        }
        let cwd = Path::new(&self.task.command.cwd);
        if cwd.is_absolute()
            || cwd
                .components()
                .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return Err(RuntimeError::Configuration);
        }
        let cwd = fs::canonicalize(root.join(cwd))?;
        if !cwd.starts_with(&root) || !cwd.is_dir() {
            return Err(RuntimeError::Configuration);
        }
        if !self.executable.is_absolute() || !self.executable.is_file() {
            return Err(RuntimeError::Configuration);
        }
        if self.task.command.timeout_ms == 0 || self.task.cancellation.max_drain_ms == 0 {
            return Err(RuntimeError::Configuration);
        }
        // Socket pairs permit nonblocking reads without unsafe code or reader threads.
        let (mut stdout_read, stdout_write) = UnixStream::pair()?;
        let (mut stderr_read, stderr_write) = UnixStream::pair()?;
        stdout_read.set_nonblocking(true)?;
        stderr_read.set_nonblocking(true)?;
        let mut cmd = Command::new(&self.executable);
        cmd.args(&self.task.command.argv)
            .current_dir(cwd)
            .env_clear()
            .env("HOME", &home)
            .env("TMPDIR", &home)
            .stdin(Stdio::null())
            .stdout(Stdio::from(OwnedFd::from(stdout_write)))
            .stderr(Stdio::from(OwnedFd::from(stderr_write)))
            .process_group(0);
        for env in &self.task.env {
            if env.validate().is_err() || matches!(env.name.as_str(), "HOME" | "TMPDIR" | "PATH") {
                return Err(RuntimeError::Configuration);
            }
            match (&env.kind, &env.value, &env.capability_ref) {
                (EnvKind::PublicValue, Some(value), None) => {
                    cmd.env(&env.name, value);
                }
                (EnvKind::Absent, None, None) => (),
                _ => return Err(RuntimeError::Configuration),
            }
        }
        if stop_requested(stop)? {
            return Err(RuntimeError::Interrupted);
        }
        let started_ms = epoch_ms();
        let clock = Instant::now();
        let mut child = cmd.spawn()?;
        drop(cmd);
        let mut out = Capture::new(self.stdout_bytes);
        let mut err = Capture::new(self.stderr_bytes);
        let timeout = Duration::from_millis(self.task.command.timeout_ms);
        let grace = Duration::from_millis(self.task.cancellation.term_grace_ms);
        let drain = Duration::from_millis(self.task.cancellation.max_drain_ms);
        let mut termination: Option<(Instant, bool)> = None;
        let mut killed = false;
        let mut status = None;
        let mut exited_at: Option<Instant> = None;
        loop {
            if out.drain(&mut stdout_read).is_err() || err.drain(&mut stderr_read).is_err() {
                if status.is_none() {
                    let _ = signal_group(child.id(), "-KILL");
                    let _ = child.kill();
                    let _ = child.try_wait();
                }
                return Err(RuntimeError::DrainIncomplete);
            }
            if status.is_none() {
                status = child.try_wait()?;
                if status.is_some() {
                    exited_at = Some(Instant::now());
                }
            }
            if status.is_some() && out.done && err.done {
                break;
            }
            if termination.is_none() && status.is_none() {
                let interrupted = stop_requested(stop).unwrap_or(true);
                if interrupted || clock.elapsed() >= timeout {
                    if signal_group(child.id(), "-TERM").is_err() {
                        let _ = child.kill();
                    }
                    termination = Some((Instant::now(), interrupted));
                }
            }
            if let Some((when, _)) = termination {
                if !killed && when.elapsed() >= grace && status.is_none() {
                    if signal_group(child.id(), "-KILL").is_err() {
                        let _ = child.kill();
                    }
                    killed = true;
                }
                if when.elapsed() >= grace.saturating_add(drain) {
                    return Err(RuntimeError::DrainIncomplete);
                }
            } else if exited_at.is_some_and(|when| when.elapsed() >= drain) {
                // The direct child has been reaped; do not signal a possibly reused group ID.
                return Err(RuntimeError::DrainIncomplete);
            }
            thread::sleep(Duration::from_millis(10));
        }
        let status = status.ok_or(RuntimeError::DrainIncomplete)?;
        let timed_out = termination.is_some_and(|(_, interrupted)| !interrupted);
        let stdout_bytes = out.bytes;
        let stderr_bytes = err.bytes;
        let stdout_digest = out.digest()?;
        let stderr_digest = err.digest()?;
        Ok(NativeObservation {
            exit_code: status.code(),
            signal: status.signal(),
            stdout_digest,
            stderr_digest,
            stdout_bytes,
            stderr_bytes,
            wall_ns: elapsed_ns(clock),
            cpu_user_ns: 0,
            cpu_system_ns: 0,
            cpu_complete: false,
            descendants_drained: false,
            started_ms,
            ended_ms: epoch_ms(),
            timed_out,
        })
    }
}
