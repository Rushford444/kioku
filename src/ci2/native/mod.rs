//! Versioned subprocess supervisor. Child output is data, never control text.

use super::domain::{ContractError, EnvKind, NativeObservation, TaskSpec, Validate};
use super::storage::ProcessEvidence;
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::time::{Duration, Instant};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const MONITOR_SCHEMA: &str = "kioku/ci2/native-supervisor-request/v1";
const RESULT_SCHEMA: &str = "kioku/ci2/native-supervision/v1";

const POSSESSION_HEADER_BYTES: u64 = 4096;
const POSSESSION_RESULT_BYTES: u64 = 1_048_576;
const POSSESSION_STARTUP: Duration = Duration::from_secs(2);

/// The scope actually closed by the native wait4/subreaper monitor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AbsenceScope {
    OriginalGroupAndAdoptedDescendants,
}

/// Archival data, not authority to mint a new native absence proof.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeAbsenceReceipt {
    pub monitor_pid: u32,
    pub source: String,
    pub leader_reaped: bool,
    pub subreaper_echild: bool,
    pub original_group_empty: bool,
    pub evidence: ProcessEvidence,
    pub reaped_pids: Vec<u32>,
    pub scope: AbsenceScope,
}

/// Only the native reader can mint this from a validated monitor checkpoint.
#[derive(Clone, Debug)]
pub struct NativeAbsenceProof {
    receipt: NativeAbsenceReceipt,
}
impl NativeAbsenceProof {
    #[must_use]
    pub const fn evidence(&self) -> &ProcessEvidence {
        &self.receipt.evidence
    }

    #[must_use]
    pub const fn receipt(&self) -> &NativeAbsenceReceipt {
        &self.receipt
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClosureCheckpoint {
    schema: String,
    header: PossessionHeader,
    result: Record,
    no_children: bool,
    original_group_empty: bool,
}

/// Read the trusted monitor's durable control checkpoint, including after loss
/// of the controller. `work` must be the same private per-invocation directory
/// chosen at launch, and `expected` the durably bound early process evidence.
/// This is not a reader for arbitrary task outputs; hostile same-UID writers
/// and modification of the operator-controlled work directory are unsupported.
/// Missing or incomplete native closure never yields a proof. No lease is minted.
///
/// # Errors
/// Insecure paths, malformed checkpoints or evidence mismatch fail closed.
pub fn read_absence_proof(
    work: &Path,
    expected: &ProcessEvidence,
) -> Result<Option<NativeAbsenceProof>, SupervisorError> {
    let invalid = || SupervisorError::Wire("invalid native closure checkpoint".into());
    let directory = fs::symlink_metadata(work)?;
    let owner = fs::metadata("/proc/self")?.uid();
    if !work.is_absolute()
        || work.canonicalize()? != work
        || !directory.is_dir()
        || directory.mode() & 0o777 != 0o700
        || directory.uid() != owner
    {
        return Err(invalid());
    }
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(work.join("possession-closure.json"))
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.mode() & 0o777 != 0o600 || metadata.uid() != owner {
        return Err(invalid());
    }
    let limit = POSSESSION_RESULT_BYTES + POSSESSION_HEADER_BYTES + 1024;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(invalid());
    }
    let checkpoint: ClosureCheckpoint = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    let evidence = checkpoint.header.evidence;
    let result = checkpoint.result;
    if checkpoint.schema != "kioku/ci2/native-closure/v1"
        || checkpoint.header.schema != "kioku/ci2/native-possession/v1"
        || checkpoint.header.source != "owned-direct-child-original-process-group"
        || checkpoint.header.monitor_pid <= 1
        || checkpoint.header.monitor_pid == evidence.process.pid
        || &evidence != expected
        || evidence.process.pid <= 1
        || evidence.process.pid != evidence.process.pgid
        || evidence.start_ticks == 0
        || evidence.process.started_ms == 0
        || evidence.boot_id.len() != 36
        || evidence.boot_id != fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim()
        || result.schema != RESULT_SCHEMA
        || result.started_ms != evidence.process.started_ms
        || result.ended_ms < result.started_ms
        || result.streams.len() != 2
        || result.exit_code.is_some() == result.signal.is_some()
        || result
            .exit_code
            .is_some_and(|code| code < 0 || result.returncode != code)
        || result
            .signal
            .is_some_and(|signal| signal <= 0 || result.returncode.checked_neg() != Some(signal))
        || result.cpu_source
            != "wait4 cumulative direct and subreaper-adopted children; each PID reaped once"
        || result
            .reaped_direct_and_adopted
            .contains(&checkpoint.header.monitor_pid)
        || !result
            .reaped_direct_and_adopted
            .windows(2)
            .all(|pair| pair[0] < pair[1])
    {
        return Err(invalid());
    }
    if !checkpoint.no_children
        || !checkpoint.original_group_empty
        || !result.cpu_complete
        || !result.descendants_drained
        || result.error.is_some()
        || !result
            .reaped_direct_and_adopted
            .contains(&evidence.process.pid)
    {
        return Ok(None);
    }
    Ok(Some(NativeAbsenceProof {
        receipt: NativeAbsenceReceipt {
            monitor_pid: checkpoint.header.monitor_pid,
            source: checkpoint.header.source,
            leader_reaped: result
                .reaped_direct_and_adopted
                .contains(&evidence.process.pid),
            subreaper_echild: checkpoint.no_children,
            original_group_empty: checkpoint.original_group_empty,
            evidence,
            reaped_pids: result.reaped_direct_and_adopted,
            scope: AbsenceScope::OriginalGroupAndAdoptedDescendants,
        },
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PossessionHeader {
    schema: String,
    monitor_pid: u32,
    source: String,
    evidence: ProcessEvidence,
}

fn possession_header(bytes: &[u8], monitor_pid: u32) -> Result<ProcessEvidence, SupervisorError> {
    let invalid = || SupervisorError::Wire("invalid native possession header".into());
    if bytes.len() as u64 > POSSESSION_HEADER_BYTES || !bytes.ends_with(b"\n") {
        return Err(invalid());
    }
    let header: PossessionHeader = serde_json::from_slice(bytes).map_err(|_| invalid())?;
    let evidence = header.evidence;
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    if header.schema != "kioku/ci2/native-possession/v1"
        || header.source != "owned-direct-child-original-process-group"
        || header.monitor_pid != monitor_pid
        || evidence.process.pid <= 1
        || evidence.process.pid == monitor_pid
        || evidence.process.pid != evidence.process.pgid
        || evidence.start_ticks == 0
        || evidence.process.started_ms == 0
        || evidence.boot_id.len() != 36
        || evidence.boot_id != boot.trim()
    {
        return Err(invalid());
    }
    let stat = fs::read_to_string(format!("/proc/{}/stat", evidence.process.pid))?;
    let (_, fields) = stat.rsplit_once(") ").ok_or_else(invalid)?;
    let fields: Vec<_> = fields.split_ascii_whitespace().collect();
    // The monitor has not reaped yet; a fast exited child may already be a zombie.
    if fields.len() <= 19
        || fields[1].parse::<u32>().ok() != Some(monitor_pid)
        || fields[2].parse::<u32>().ok() != Some(evidence.process.pgid)
        || fields[19].parse::<u64>().ok() != Some(evidence.start_ticks)
    {
        return Err(invalid());
    }
    Ok(evidence)
}

fn wait_monitor(
    child: &mut std::process::Child,
    budget: Duration,
) -> Result<std::process::ExitStatus, SupervisorError> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if started.elapsed() >= budget {
            // Only our monitor handle, never a discovered PID or an arbitrary PGID.
            child.kill()?;
            child.wait()?;
            return Err(SupervisorError::Wire(
                "possession monitor deadline; drainage unknown".into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

// Monitor stderr is a separate, bounded diagnostic channel. Only this closed
// vocabulary is rendered; exception messages and arbitrary stderr are not data.
#[derive(Debug, Deserialize)]
enum MonitorErrorKind {
    OSError,
    ProcessLookupError,
    FileNotFoundError,
    PermissionError,
    ValueError,
    RuntimeError,
    Other,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MonitorDiagnostic {
    schema: String,
    kind: MonitorErrorKind,
    errno: Option<u16>,
}

fn render_monitor_diagnostic(bytes: &[u8]) -> String {
    if bytes.len() <= 1024
        && let Ok(diagnostic) = serde_json::from_slice::<MonitorDiagnostic>(bytes)
        && diagnostic.schema == "kioku/ci2/native-diagnostic/v1"
        && diagnostic.errno.is_none_or(|number| number <= 4095)
    {
        return format!(
            "monitor diagnostic: {:?} errno={:?}",
            diagnostic.kind, diagnostic.errno
        );
    }
    "monitor diagnostic unavailable".into()
}

fn monitor_diagnostic(receiver: &std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>) -> String {
    receiver
        .recv_timeout(Duration::from_secs(1))
        .ok()
        .and_then(Result::ok)
        .map_or_else(
            || "monitor diagnostic unavailable".into(),
            |bytes| render_monitor_diagnostic(&bytes),
        )
}

#[allow(clippy::too_many_lines)]
fn run_with_possession(
    command: &mut Command,
    request: &Request,
    abort_file: &Path,
    callback: &mut dyn FnMut(&ProcessEvidence) -> Result<(), SupervisorError>,
) -> Result<(Vec<u8>, ProcessEvidence), SupervisorError> {
    let mut child = command
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| SupervisorError::Wire("monitor stderr unavailable".into()))?;
    let (diagnostic_sender, diagnostic_receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = (&mut stderr).take(1025).read_to_end(&mut bytes);
        // Drain excess without storing it, so a full stderr pipe cannot stall
        // the monitor or be mistaken for a missing possession header.
        let _ = std::io::copy(&mut stderr, &mut std::io::sink());
        let _ = diagnostic_sender.send(result.map(|_| bytes));
    });
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| SupervisorError::Wire("monitor stdout unavailable".into()))?;
    let (sender, receiver) = std::sync::mpsc::channel();
    // A bounded reader preserves buffered bytes after the first newline. Neither
    // stream can allocate without limit or turn tool stdout into control data.
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut header = Vec::new();
        let first = (&mut reader)
            .take(POSSESSION_HEADER_BYTES + 1)
            .read_until(b'\n', &mut header)
            .map(|_| header);
        if sender.send(first).is_err() {
            return;
        }
        let mut terminal = Vec::new();
        let result = reader
            .take(POSSESSION_RESULT_BYTES + 1)
            .read_to_end(&mut terminal)
            .map(|_| terminal);
        let _ = sender.send(result);
    });
    let header = receiver
        .recv_timeout(POSSESSION_STARTUP)
        .map_err(|_| SupervisorError::Wire("native possession header missing or late".into()))
        .and_then(|result| result.map_err(SupervisorError::Io))
        .and_then(|bytes| possession_header(&bytes, child.id()));
    let evidence = header.and_then(|evidence| {
        callback(&evidence)?;
        child
            .stdin
            .as_mut()
            .ok_or_else(|| SupervisorError::Wire("monitor stdin unavailable".into()))?
            .write_all(b"continue\n")?;
        Ok(evidence)
    });
    // EOF is also an abort when no acknowledgement was delivered.
    drop(child.stdin.take());
    let cleanup = Duration::from_millis(
        request
            .max_drain_ms
            .saturating_mul(2)
            .saturating_add(request.grace_ms)
            .saturating_add(2000),
    );
    let evidence = match evidence {
        Ok(evidence) => evidence,
        Err(error) => {
            // No global operator stop and no second command execution. Even if
            // writing fails, stdin EOF asks the trusted monitor to drain its group.
            let _ = fs::write(abort_file, b"native possession failed\n");
            let _ = wait_monitor(&mut child, cleanup + POSSESSION_STARTUP);
            return Err(match error {
                SupervisorError::Wire(message) => SupervisorError::Wire(format!(
                    "{message}; {}",
                    monitor_diagnostic(&diagnostic_receiver)
                )),
                other => other,
            });
        }
    };
    let status = wait_monitor(
        &mut child,
        Duration::from_millis(request.timeout_ms).saturating_add(cleanup),
    )?;
    if !status.success() {
        use std::os::unix::process::ExitStatusExt;
        let reason = status.code().map_or_else(
            || {
                status.signal().map_or_else(
                    || "unknown exit status".to_owned(),
                    |signal| format!("signal {signal}"),
                )
            },
            |code| format!("exit code {code}"),
        );
        return Err(SupervisorError::Wire(format!(
            "possession monitor failed: {reason}; {}",
            monitor_diagnostic(&diagnostic_receiver)
        )));
    }
    let output = receiver
        .recv_timeout(Duration::from_secs(1))
        .map_err(|_| SupervisorError::Wire("possession terminal frame missing".into()))??;
    if output.len() as u64 > POSSESSION_RESULT_BYTES {
        return Err(SupervisorError::Wire(
            "possession terminal frame too large".into(),
        ));
    }
    Ok((output, evidence))
}

#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error("configuration or execution: {0}")]
    Configuration(String),
    #[error("supervisor I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("supervisor unavailable or malformed: {0}")]
    Wire(String),
    #[error("native contract: {0}")]
    Contract(#[from] ContractError),
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct Request {
    schema: &'static str,
    argv: Vec<String>,
    private_home: String,
    cwd: String,
    environment: std::collections::BTreeMap<String, String>,
    artifacts: std::collections::BTreeMap<String, String>,
    stop_file: String,
    stop_files: Vec<String>,
    grace_ms: u64,
    timeout_ms: u64,
    max_drain_ms: u64,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    possession: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamRecord {
    pub bytes: u64,
    pub digest: String,
    pub path: PathBuf,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
struct Record {
    schema: String,
    returncode: i32,
    exit_code: Option<i32>,
    signal: Option<i32>,
    wall_ns: u64,
    cpu_user_ns: u64,
    cpu_system_ns: u64,
    max_rss_bytes: u64,
    cpu_complete: bool,
    descendants_drained: bool,
    started_ms: u64,
    ended_ms: u64,
    timed_out: bool,
    interrupted: bool,
    stopped_files: Vec<String>,
    cpu_source: String,
    streams: Vec<StreamRecord>,
    #[serde(default)]
    error: Option<SupervisorFailure>,
    // Wire v1 distinguishes absent, JSON null and measured duration.
    #[serde(default)]
    #[allow(clippy::option_option)]
    term_to_kill_ns: Option<Option<u64>>,
    #[serde(default)]
    reaped_direct_and_adopted: Vec<u32>,
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupervisorFailure {
    pub code: String,
    pub reason: i32,
    pub message: String,
}
impl std::fmt::Display for SupervisorFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}
#[derive(Debug, PartialEq, Eq)]
pub struct LimitedState {
    pub state_keeps_running: bool,
    pub reason: String,
}
#[derive(Clone, Debug)]
pub struct Observation {
    /// None unless the trusted monitor proved full scoped closure.
    pub absence_proof: Option<NativeAbsenceProof>,
    /// Present only after the opt-in trusted monitor handshake and callback.
    pub process_evidence: Option<ProcessEvidence>,
    pub native: NativeObservation,
    pub streams: (StreamRecord, StreamRecord),
    pub max_rss_bytes: Option<u64>,
    pub interrupted: bool,
    pub stopped_files: Vec<PathBuf>,
    pub supervision_error: Option<SupervisorFailure>,
    pub reaped_pids: Vec<u32>,
    pub term_to_kill_ns: Option<u64>,
}
fn private_dir(path: &Path) -> Result<(), SupervisorError> {
    if !path.is_absolute() {
        return Err(SupervisorError::Configuration(
            "supervision directory is not absolute".into(),
        ));
    }
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        _ => Err(SupervisorError::Configuration(
            "parent supervised work directory must exist".into(),
        )),
    }
}
/// Executes a truthful subprocess group and returns read-only stream references.
///
/// # Errors
/// Malformed executable or operator paths, invalid launcher, child drain failures, or missing result records fail closed.
/// Stops on the sole configured file, preserving the first native fixture interface.
///
/// # Errors
/// Same checked process, filesystem and monitor conditions as `supervise_with_stops`.
pub fn supervise(
    task: &TaskSpec,
    executable: &Path,
    source: &Path,
    work: &Path,
    supervisor: &Path,
    stop_file: &Path,
) -> Result<Observation, SupervisorError> {
    supervise_with_stops(task, executable, source, work, supervisor, stop_file, &[])
}

/// Executes a subprocess group with global and one-run-specific stop paths.
///
/// # Errors
/// Invalid approved paths, monitor failures or inconsistent resource/stream evidence fail closed.
#[allow(clippy::too_many_lines)]
pub fn supervise_with_stops(
    task: &TaskSpec,
    executable: &Path,
    source: &Path,
    work: &Path,
    supervisor: &Path,
    stop_file: &Path,
    additional_stop_files: &[PathBuf],
) -> Result<Observation, SupervisorError> {
    supervise_with_stops_and_possession(
        task,
        executable,
        source,
        work,
        supervisor,
        stop_file,
        additional_stop_files,
        None,
    )
}

/// Opt-in native possession, recorded while the trusted monitor retains its child.
/// The callback must durably associate the evidence with its own run and return promptly.
/// `None` preserves the legacy protocol and supplies no possession evidence.
/// This proves only the original owned group, not a cgroup or escaped descendants.
///
/// # Errors
/// Header or callback failures abort only this invocation, never retry execution.
#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::type_complexity
)]
pub fn supervise_with_stops_and_possession(
    task: &TaskSpec,
    executable: &Path,
    source: &Path,
    work: &Path,
    supervisor: &Path,
    stop_file: &Path,
    additional_stop_files: &[PathBuf],
    possession: Option<&mut dyn FnMut(&ProcessEvidence) -> Result<(), SupervisorError>>,
) -> Result<Observation, SupervisorError> {
    if !task.command.argv.is_empty() && cfg!(not(target_os = "linux")) {
        return Err(SupervisorError::Configuration(
            "only Linux has a qualified supervisor in I1-L".into(),
        ));
    }
    let executable = fs::canonicalize(executable)?;
    if !executable.is_file() || !source.is_absolute() || !work.is_absolute() {
        return Err(SupervisorError::Configuration(
            "all execution paths must be absolute within approved roots".into(),
        ));
    }
    match fs::symlink_metadata(work) {
        Ok(meta) if meta.is_dir() => private_dir(work)?,
        Ok(_) => {
            return Err(SupervisorError::Configuration(
                "supervisor work is not a regular directory".into(),
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::DirBuilder::new().mode(0o700).create(work)?;
        }
        Err(e) => return Err(e.into()),
    }
    let work = fs::canonicalize(work)?;
    private_dir(source)?;
    private_dir(
        supervisor
            .parent()
            .ok_or_else(|| SupervisorError::Configuration("no supervisor parent".into()))?,
    )?;
    if !supervisor.is_file() {
        return Err(SupervisorError::Configuration("supervisor missing".into()));
    }
    let root = source.canonicalize()?;
    let destination = if task.command.cwd.is_empty() {
        root.clone()
    } else {
        fs::canonicalize(root.join(&task.command.cwd))?
    };
    if !destination.starts_with(&root) {
        return Err(SupervisorError::Configuration(
            "command CWD escapes registered root".into(),
        ));
    }
    if work == root || !work.starts_with(&root) {
        return Err(SupervisorError::Configuration(
            "supervisor work must be a child of the registered source root".into(),
        ));
    }
    let request_path = work.join("request.json");
    let request = Request {
        schema: MONITOR_SCHEMA,
        argv: std::iter::once(executable.to_string_lossy().into_owned())
            .chain(task.command.argv.iter().cloned())
            .collect(),
        private_home: work.to_string_lossy().into_owned(),
        cwd: destination.to_string_lossy().into_owned(),
        environment: std::collections::BTreeMap::from([
            ("CI".into(), "1".into()),
            ("COREPACK_ENABLE_NETWORK".into(), "0".into()),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
            ("HOME".into(), work.to_string_lossy().into_owned()),
            ("LANG".into(), "C".into()),
            ("LC_ALL".into(), "C".into()),
            ("NEXT_TELEMETRY_DISABLED".into(), "1".into()),
            (
                "PATH".into(),
                "/home/node/.local/bin:/usr/local/bin:/usr/bin:/bin".into(),
            ),
            ("TMPDIR".into(), work.to_string_lossy().into_owned()),
        ]),
        artifacts: std::collections::BTreeMap::new(),
        stop_file: stop_file.to_string_lossy().into_owned(),
        stop_files: additional_stop_files
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        grace_ms: task.cancellation.term_grace_ms,
        timeout_ms: task.command.timeout_ms,
        max_drain_ms: task.cancellation.max_drain_ms,
        possession: possession.is_some(),
    };
    let mut request = request;
    let abort_file = work.join("possession.abort");
    if possession.is_some() {
        request
            .stop_files
            .push(abort_file.to_string_lossy().into_owned());
    }
    for env in &task.env {
        env.validate()?;
        if matches!(env.name.as_str(), "HOME" | "TMPDIR" | "PATH") {
            return Err(SupervisorError::Configuration(
                "reserved environment name".into(),
            ));
        }
        match (&env.kind, &env.value, &env.capability_ref) {
            (EnvKind::Absent, None, None) => {}
            (EnvKind::PublicValue, Some(value), None) => {
                request.environment.insert(env.name.clone(), value.clone());
            }
            _ => return Err(SupervisorError::Configuration("invalid environment".into())),
        }
    }
    let mut artifacts = std::collections::BTreeMap::new();
    for stream in ["stdout", "stderr"] {
        let path = work.join(stream);
        artifacts.insert(stream.into(), path.to_string_lossy().into_owned());
    }
    request.artifacts = artifacts;
    let bytes =
        serde_json::to_vec(&request).map_err(|error| SupervisorError::Wire(error.to_string()))?;
    fs::write(&request_path, bytes)?;
    let mut command = Command::new("/usr/bin/python3");
    command
        .arg("-B")
        .arg(supervisor)
        .arg(&request_path)
        .current_dir(&work)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .stdout(Stdio::piped());
    let (stdout, process_evidence) = if let Some(callback) = possession {
        let (output, evidence) =
            run_with_possession(&mut command, &request, &abort_file, callback)?;
        (output, Some(evidence))
    } else {
        let running = command
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .output()?;
        if !running.status.success() {
            return Err(SupervisorError::Wire(render_monitor_diagnostic(
                &running.stderr,
            )));
        }
        (running.stdout, None)
    };
    let output: Record = serde_json::from_slice(&stdout)
        .map_err(|error| SupervisorError::Wire(error.to_string()))?;
    if let Some(evidence) = &process_evidence
        && (output.started_ms != evidence.process.started_ms
            || !output
                .reaped_direct_and_adopted
                .contains(&evidence.process.pid))
    {
        return Err(SupervisorError::Wire(
            "terminal record does not match possessed leader".into(),
        ));
    }
    if output.schema != RESULT_SCHEMA || output.streams.len() != 2 {
        return Err(SupervisorError::Wire(
            "supervisor result missing schema/streams".into(),
        ));
    }
    if output.exit_code.is_some() == output.signal.is_some()
        || output
            .exit_code
            .is_some_and(|code| code < 0 || output.returncode != code)
        || output
            .signal
            .is_some_and(|signal| signal <= 0 || output.returncode.checked_neg() != Some(signal))
    {
        return Err(SupervisorError::Wire(
            "supervisor return code disagrees with native child status".into(),
        ));
    }
    if output.interrupted == output.stopped_files.is_empty() {
        return Err(SupervisorError::Wire(
            "stop provenance disagrees with native interruption".into(),
        ));
    }
    if output.cpu_source.is_empty() || output.started_ms > output.ended_ms {
        return Err(SupervisorError::Wire(
            "supervisor clock/source inconsistent".into(),
        ));
    }
    if output.cpu_complete && !output.descendants_drained {
        return Err(SupervisorError::Wire(
            "resource completeness cannot exceed process drainage".into(),
        ));
    }
    if output.error.is_none() && (!output.cpu_complete || !output.descendants_drained) {
        return Err(SupervisorError::Wire(
            "incomplete process/CPU observation lacks supervisor.error".into(),
        ));
    }
    if let Some(error) = &output.error
        && (error.code != "supervisor.error"
            || error.message.is_empty()
            || error.reason < 2
            || error.reason > 3
            || output.cpu_complete)
    {
        return Err(SupervisorError::Wire(
            "supervisor error disagrees with incomplete evidence".into(),
        ));
    }
    for stream in &output.streams {
        let file = fs::read(&stream.path).map_err(|error| {
            SupervisorError::Wire(format!(
                "cannot read native stream {}: {error}",
                stream.path.display()
            ))
        })?;
        if file.len()
            != usize::try_from(stream.bytes).map_err(|_| {
                SupervisorError::Wire("stream length exceeds platform bounds".into())
            })?
        {
            return Err(SupervisorError::Wire("stream byte count mismatch".into()));
        }
        // Uses standard SHA-256 on exact bytes, not blake3 disguised as SHA.
        let actual = sha2::Sha256::digest(&file);
        if stream.digest != format!("sha256:{actual:x}") {
            return Err(SupervisorError::Wire("stream digest mismatch".into()));
        }
        if !stream.path.starts_with(&work) || !fs::symlink_metadata(&stream.path)?.is_file() {
            return Err(SupervisorError::Wire(
                "output file is not under per-run work root".into(),
            ));
        }
    }
    let mut streams = output.streams.into_iter();
    let stdout = streams
        .next()
        .ok_or_else(|| SupervisorError::Wire("missing stdout".into()))?;
    let stderr = streams
        .next()
        .ok_or_else(|| SupervisorError::Wire("missing stderr".into()))?;
    let native = NativeObservation {
        exit_code: output.exit_code,
        signal: output.signal,
        stdout_digest: super::domain::Digest::new(&stdout.digest)?,
        stderr_digest: super::domain::Digest::new(&stderr.digest)?,
        stdout_bytes: stdout.bytes,
        stderr_bytes: stderr.bytes,
        wall_ns: output.wall_ns,
        cpu_user_ns: output.cpu_user_ns,
        cpu_system_ns: output.cpu_system_ns,
        cpu_complete: output.cpu_complete,
        descendants_drained: output.descendants_drained && output.error.is_none(),
        started_ms: output.started_ms,
        ended_ms: output.ended_ms,
        timed_out: output.timed_out,
    };
    native.validate()?;
    let absence_proof = match &process_evidence {
        Some(evidence) => read_absence_proof(&work, evidence)?,
        None => None,
    };
    Ok(Observation {
        absence_proof,
        process_evidence,
        native,
        streams: (stdout, stderr),
        max_rss_bytes: Some(output.max_rss_bytes),
        interrupted: output.interrupted,
        stopped_files: output
            .stopped_files
            .into_iter()
            .map(PathBuf::from)
            .collect(),
        supervision_error: output.error,
        reaped_pids: output.reaped_direct_and_adopted,
        term_to_kill_ns: output.term_to_kill_ns.flatten(),
    })
}
