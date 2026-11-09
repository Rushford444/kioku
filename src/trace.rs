//! Conservative Linux syscall observation. Trace records are never written to disk.

use crate::policy::{ObservedPath, PathKind};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

const HASH_AARCH64: &str = "0e69fa123b3d9732645ce1ac8c89819d100f83cdc9e52a80565d9ad8e7d8cf28";
const HASH_X86_64: &str = "aebaf212ce84a12208c5fa25f08c892029e6f6949b778da55e071ac145404a1e";
const RAW: &str = "raw=execveat,read,readv,pread64,preadv,preadv2,write,writev,pwrite64,pwritev,pwritev2,getrandom,recvfrom,recvmsg,sendto,sendmsg";
const MAX_TRACE: usize = 16 * 1024 * 1024;

/// Linux architecture whose syscall grammar is used.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Arch {
    /// 64-bit ARM.
    Aarch64,
    /// 64-bit x86.
    X86_64,
}

impl Arch {
    /// Architecture of the running build, if tracing is supported there.
    #[must_use]
    pub const fn current() -> Option<Self> {
        #[cfg(target_arch = "aarch64")]
        {
            Some(Self::Aarch64)
        }
        #[cfg(target_arch = "x86_64")]
        {
            Some(Self::X86_64)
        }
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        {
            None
        }
    }

    /// Name used in diagnostics.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Aarch64 => "aarch64",
            Self::X86_64 => "x86_64",
        }
    }

    /// SHA-256 of the pinned Debian strace 6.1 executable.
    #[must_use]
    pub const fn sha256(self) -> &'static str {
        match self {
            Self::Aarch64 => HASH_AARCH64,
            Self::X86_64 => HASH_X86_64,
        }
    }

    /// Distributions of the pinned tracer.
    #[must_use]
    pub const fn version(self) -> &'static str {
        match self {
            Self::Aarch64 => "strace 6.1 Debian arm64",
            Self::X86_64 => "strace 6.1 Debian amd64",
        }
    }

    /// `e_machine` value of a static ELF image for this architecture.
    #[must_use]
    pub const fn machine(self) -> u16 {
        match self {
            Self::Aarch64 => 183,
            Self::X86_64 => 62,
        }
    }
}

/// Pinned tracer identity and output-affecting arguments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TracerIdentity {
    /// SHA-256 of the tracer executable.
    pub sha256: String,
    /// Grammar schema.
    pub schema: String,
    /// Tracer version.
    pub version: String,
    /// Arguments determining the trace grammar.
    pub flags: Vec<String>,
    /// Architecture selected for the grammar.
    pub arch: Arch,
}
/// Complete observation for capture by policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Observation {
    /// Dependencies observed across all descendants.
    pub dependencies: Vec<ObservedPath>,
    /// Verified tracer identity.
    pub tracer_identity: TracerIdentity,
}
/// Why the observation cannot be reused.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DenyReason {
    /// Unavailable tracer, unsupported syscall or incomplete observation.
    #[error("unsupported or incomplete trace")]
    Unsupported,
    /// A malformed or unsupported record, identified only by line number.
    #[error("unsupported trace record at line {0}")]
    Parse(usize),
    /// Executable is not a supported static ELF image.
    #[error("unsupported executable format")]
    ExecutableFormat,
    /// Network access was observed.
    #[error("network access")]
    Network,
    /// Time access was observed.
    #[error("clock access")]
    Clock,
    /// Randomness was observed.
    #[error("randomness")]
    Random,
    /// Input from stdin was observed.
    #[error("stdin access")]
    Stdin,
    /// A kernel or device filesystem path was observed.
    #[error("system path")]
    SystemPath,
    /// An unmodeled syscall was observed.
    #[error("unknown syscall")]
    Unknown,
    /// A file was opened for writing.
    #[error("file write")]
    Write,
    /// No trace grammar exists for this platform.
    #[error("platform unsupported")]
    PlatformUnsupported,
    /// The pinned tracer is absent or cannot be read.
    #[error("tracer unavailable")]
    TracerUnavailable,
    /// `KIOKU_STRACE` is not an absolute path.
    #[error("tracer path is not absolute")]
    TracerNotAbsolute,
    /// The tracer does not match the pinned executable.
    #[error("tracer untrusted")]
    TracerUntrusted,
}

impl DenyReason {
    /// Stable machine-readable code for JSON and diagnostics.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Unsupported => "unsupported",
            Self::Parse(_) => "parse-error",
            Self::ExecutableFormat => "executable-format",
            Self::Network => "network",
            Self::Clock => "clock",
            Self::Random => "random",
            Self::Stdin => "stdin",
            Self::SystemPath => "system-path",
            Self::Unknown => "unknown-syscall",
            Self::Write => "write",
            Self::PlatformUnsupported => "platform-unsupported",
            Self::TracerUnavailable => "tracer-unavailable",
            Self::TracerNotAbsolute => "tracer-not-absolute",
            Self::TracerUntrusted => "tracer-untrusted",
        }
    }
}
/// Identifier of the trace contract: the policy name every trace event reports
/// and the grammar version of the supported tracer.
pub const CONTRACT: &str = "trace-v2";

/// Trace classification independent of child exit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TraceObservation {
    /// All records modeled.
    Complete(Observation),
    /// Unknown behavior or incomplete records.
    Denied(DenyReason),
}
/// Result of one tracer launch. An unknown status must never be retried automatically.
#[derive(Debug)]
pub struct TraceOutcome {
    /// The child's known exit status, or unknown.
    pub child_status: Option<ExitStatus>,
    /// Observation independently denied on ambiguity.
    pub observation: TraceObservation,
}

/// Verify the exact supported tracer without executing a child.
///
/// # Errors
/// Returns an error if the tracer cannot be authenticated.
pub fn identity() -> Result<TracerIdentity, DenyReason> {
    use sha2::{Digest, Sha256};
    if !cfg!(target_os = "linux") {
        return Err(DenyReason::PlatformUnsupported);
    }
    let arch = Arch::current().ok_or(DenyReason::PlatformUnsupported)?;
    let path = std::env::var_os("KIOKU_STRACE").ok_or(DenyReason::TracerUnavailable)?;
    let path = Path::new(&path);
    if !path.is_absolute() {
        return Err(DenyReason::TracerNotAbsolute);
    }
    let bytes = std::fs::read(path).map_err(|_| DenyReason::TracerUnavailable)?;
    if format!("{:x}", Sha256::digest(bytes)) != arch.sha256() {
        return Err(DenyReason::TracerUntrusted);
    }
    Ok(TracerIdentity {
        sha256: arch.sha256().into(),
        schema: CONTRACT.into(),
        version: arch.version().into(),
        flags: TRACE_FLAGS.iter().map(|flag| (*flag).to_string()).collect(),
        arch,
    })
}

const TRACE_FLAGS: &[&str] = &["-f", "-e", "trace=all", "-e", RAW, "-s", "0"];

#[derive(Clone)]
enum Pending {
    Exec(PathBuf),
    Open(PathBuf, bool, &'static str),
    Wait(u32),
}

#[derive(Default, Clone)]
struct Task {
    files: BTreeMap<i32, (PathBuf, bool)>,
    cwd: PathBuf,
    exited: bool,
    exit_called: bool,
    pending: Option<Pending>,
}

/// Parse a complete raw-syscall trace under the pinned grammar of the host.
///
/// # Errors
/// Rejects incomplete, unknown, ambiguous or unsupported records.
pub fn parse(trace: &str, cwd: &Path, executable: &Path) -> Result<Vec<ObservedPath>, DenyReason> {
    parse_for(
        Arch::current().unwrap_or(Arch::Aarch64),
        trace,
        cwd,
        executable,
    )
}

/// Parse a complete raw-syscall trace under an explicit architecture grammar.
///
/// # Errors
/// Rejects incomplete, unknown, ambiguous or unsupported records.
// The parser keeps per-task transitions together so each accepted record updates one state machine.
#[allow(clippy::too_many_lines)]
pub fn parse_for(
    arch: Arch,
    trace: &str,
    cwd: &Path,
    executable: &Path,
) -> Result<Vec<ObservedPath>, DenyReason> {
    if !cwd.is_absolute() || !executable.is_absolute() || trace.len() > MAX_TRACE {
        return Err(DenyReason::Unsupported);
    }
    let line_number = std::cell::Cell::new(0usize);
    let bad = || DenyReason::Parse(line_number.get());
    let mut tasks: BTreeMap<u32, Task> = BTreeMap::new();
    let mut root = None;
    let mut seen_exec = false;
    let mut deps = vec![ObservedPath {
        path: executable.to_path_buf(),
        kind: PathKind::Executable,
    }];
    let lines = pair_clones(trace)?;
    for (index, line) in lines.iter().enumerate() {
        let Some(line) = line.as_deref() else {
            continue;
        };
        line_number.set(index + 1);
        if line.len() > 8192 {
            return Err(bad());
        }
        let (pid, call) = line.split_once(' ').ok_or_else(bad)?;
        let pid: u32 = pid.parse().map_err(|_| bad())?;
        let call = call.trim_start();
        let normalized = call
            .rsplit_once(" = ")
            .map(|(left, right)| format!("{} = {right}", left.trim_end()));
        let call = normalized.as_deref().unwrap_or(call);
        if root.is_none() {
            root = Some(pid);
            tasks.insert(
                pid,
                Task {
                    cwd: cwd.to_path_buf(),
                    ..Task::default()
                },
            );
        }
        if !tasks.contains_key(&pid) || tasks.get(&pid).ok_or_else(bad)?.exited {
            return Err(bad());
        }
        if let Some(pending) = &tasks.get(&pid).ok_or_else(bad)?.pending {
            let expected = match pending {
                Pending::Exec(_) => "<... execve resumed>".to_string(),
                Pending::Open(_, _, name) => format!("<... {name} resumed>"),
                Pending::Wait(_) => "<... wait4 resumed>".to_string(),
            };
            if !call.starts_with(&expected) {
                return Err(bad());
            }
        }
        if let Some(args) = call.strip_prefix("execve(") {
            let path = quoted_path(args, &tasks.get(&pid).ok_or_else(bad)?.cwd)?;
            let (_, rest) = args.split_once("\", ").ok_or_else(bad)?;
            let unfinished_env =
                rest.starts_with("[...], 0x") && rest.ends_with(" vars */ <unfinished ...>");
            if rest == "[...], NULL <unfinished ...>" || unfinished_env {
                if !seen_exec || !tasks.get(&pid).ok_or_else(bad)?.files.is_empty() {
                    return Err(bad());
                }
                tasks.get_mut(&pid).ok_or_else(bad)?.pending = Some(Pending::Exec(path));
                continue;
            }
            let redacted_env = rest.starts_with("[...], 0x") && rest.ends_with(" vars */) = 0");
            if !redacted_env && rest != "[...], NULL) = 0" {
                return Err(bad());
            }
            if seen_exec {
                if !tasks.get(&pid).ok_or_else(bad)?.files.is_empty() {
                    return Err(bad());
                }
                deps.push(ObservedPath {
                    path,
                    kind: PathKind::Executable,
                });
            } else {
                if Some(pid) != root || path != executable {
                    return Err(bad());
                }
                seen_exec = true;
            }
        } else if call == "<... execve resumed>) = 0" {
            let Some(Pending::Exec(path)) = tasks.get_mut(&pid).ok_or_else(bad)?.pending.take()
            else {
                return Err(bad());
            };
            deps.push(ObservedPath {
                path,
                kind: PathKind::Executable,
            });
        } else if let Some(args) = call.strip_prefix("chdir(") {
            let path = quoted_path(args, &tasks.get(&pid).ok_or_else(bad)?.cwd)?;
            if !args.ends_with("\") = 0") {
                return Err(bad());
            }
            tasks.get_mut(&pid).ok_or_else(bad)?.cwd.clone_from(&path);
            deps.push(ObservedPath {
                path,
                kind: PathKind::Metadata,
            });
        } else if let Some((dirfd, args)) = open_args(arch, call) {
            let pending =
                parse_open_record(tasks.get_mut(&pid).ok_or_else(bad)?, dirfd, args, &mut deps)?;
            if let Some(pending) = pending {
                tasks.get_mut(&pid).ok_or_else(bad)?.pending = Some(pending);
            }
        } else if let Some((name, result)) = open_resume(call) {
            let Some(Pending::Open(path, directory, pending_name)) =
                tasks.get_mut(&pid).ok_or_else(bad)?.pending.take()
            else {
                return Err(bad());
            };
            if name != pending_name {
                return Err(bad());
            }
            register_open(
                tasks.get_mut(&pid).ok_or_else(bad)?,
                path,
                directory,
                result,
                &mut deps,
            )?;
        } else if let Some((dirfd, args)) = stat_args(arch, call) {
            parse_stat_record(tasks.get(&pid).ok_or_else(bad)?, dirfd, args, &mut deps)?;
        } else if let Some((dirfd, args)) = access_args(arch, call) {
            parse_access_record(tasks.get(&pid).ok_or_else(bad)?, dirfd, args, &mut deps)?;
        } else if let Some((dirfd, args)) = readlink_args(arch, call) {
            parse_readlink_record(tasks.get(&pid).ok_or_else(bad)?, dirfd, args, &mut deps)?;
        } else if let Some(rest) = call.strip_prefix("fstat(") {
            parse_fstat_record(tasks.get(&pid).ok_or_else(bad)?, rest)?;
        } else if let Some(rest) = call.strip_prefix("fcntl(") {
            parse_fcntl_record(tasks.get(&pid).ok_or_else(bad)?, rest)?;
        } else if let Some(rest) = call.strip_prefix("lseek(") {
            parse_lseek_record(tasks.get(&pid).ok_or_else(bad)?, rest)?;
        } else if let Some(rest) = call.strip_prefix("write(0x") {
            parse_write_record(rest)?;
        } else if let Some(rest) = call.strip_prefix("getdents64(") {
            let (fd, _) = rest.split_once(',').ok_or_else(bad)?;
            let fd: i32 = fd.parse().map_err(|_| bad())?;
            let (path, directory) = tasks
                .get(&pid)
                .ok_or_else(bad)?
                .files
                .get(&fd)
                .ok_or_else(bad)?;
            if !directory
                || call
                    .rsplit_once(") = ")
                    .and_then(|(_, value)| value.parse::<u64>().ok())
                    .is_none()
            {
                return Err(bad());
            }
            deps.push(ObservedPath {
                path: path.clone(),
                kind: PathKind::Directory,
            });
        } else if let Some(rest) = call.strip_prefix("mmap(NULL, ") {
            let (args, result) = rest.split_once(") = ").ok_or_else(bad)?;
            let fields: Vec<_> = args.split(", ").collect();
            if fields.len() != 5
                || fields[0].parse::<u64>().is_err()
                || fields[1] != "PROT_READ"
                || fields[2] != "MAP_PRIVATE"
                || fields[4].parse::<u64>().is_err()
                || !result.starts_with("0x")
            {
                return Err(DenyReason::Unsupported);
            }
            let fd: i32 = fields[3].parse().map_err(|_| bad())?;
            if tasks
                .get(&pid)
                .ok_or_else(bad)?
                .files
                .get(&fd)
                .is_none_or(|(_, directory)| *directory)
            {
                return Err(bad());
            }
        } else if let Some(rest) = call.strip_prefix("read(0x") {
            let (fd, tail) = rest.split_once(',').ok_or_else(bad)?;
            let fd = i32::from_str_radix(fd, 16).map_err(|_| bad())?;
            if fd == 0 {
                return Err(DenyReason::Stdin);
            }
            if !tasks.get(&pid).ok_or_else(bad)?.files.contains_key(&fd) || !tail.contains(") = ") {
                return Err(bad());
            }
            let result = tail.rsplit_once(") = ").ok_or_else(bad)?.1;
            if result != "0" && !result.starts_with("0x") {
                return Err(bad());
            }
        } else if let Some(rest) = call.strip_prefix("close(") {
            let fd: i32 = rest
                .strip_suffix(") = 0")
                .ok_or_else(bad)?
                .parse()
                .map_err(|_| bad())?;
            if tasks
                .get_mut(&pid)
                .ok_or_else(bad)?
                .files
                .remove(&fd)
                .is_none()
            {
                return Err(bad());
            }
        } else if let Some(rest) = call.strip_prefix("clone(child_stack=NULL, flags=SIGCHLD) = ") {
            let child: u32 = rest.parse().map_err(|_| bad())?;
            let state = tasks.get(&pid).ok_or_else(bad)?.clone();
            if tasks.insert(child, state).is_some() {
                return Err(bad());
            }
        } else if let Some(rest) = call.strip_prefix("wait4(") {
            let child: u32 = rest
                .split(',')
                .next()
                .ok_or_else(bad)?
                .parse()
                .map_err(|_| bad())?;
            if !tasks.contains_key(&child) {
                return Err(bad());
            }
            if call.ends_with("<unfinished ...>") {
                if tasks
                    .get_mut(&pid)
                    .ok_or_else(bad)?
                    .pending
                    .replace(Pending::Wait(child))
                    .is_some()
                {
                    return Err(bad());
                }
            } else if !call.ends_with(&format!(") = {child}")) {
                return Err(bad());
            }
        } else if call.starts_with("<... wait4 resumed>") {
            let Some(Pending::Wait(child)) = tasks.get_mut(&pid).ok_or_else(bad)?.pending.take()
            else {
                return Err(bad());
            };
            if !tasks.contains_key(&child) || !call.ends_with(&format!(") = {child}")) {
                return Err(bad());
            }
        } else if call.starts_with("--- SIGCHLD ") {
            let child = call
                .split("si_pid=")
                .nth(1)
                .and_then(|x| x.split(',').next())
                .and_then(|x| x.parse::<u32>().ok())
                .ok_or_else(bad)?;
            if !tasks.contains_key(&child)
                || !call.contains("si_code=CLD_EXITED")
                || !call.contains("si_status=0")
            {
                return Err(bad());
            }
        } else if call == "exit(0) = ?" || call == "exit_group(0) = ?" {
            let task = tasks.get_mut(&pid).ok_or_else(bad)?;
            if task.exit_called || task.pending.is_some() {
                return Err(bad());
            }
            task.exit_called = true;
        } else if call == "+++ exited with 0 +++" {
            let task = tasks.get_mut(&pid).ok_or_else(bad)?;
            if !task.exit_called {
                return Err(bad());
            }
            task.exited = true;
        } else {
            let name = call.split('(').next().ok_or_else(bad)?;
            let reason = match name {
                "socket" | "socketpair" | "connect" | "bind" | "listen" | "accept" | "accept4"
                | "sendto" | "sendmsg" | "recvfrom" | "recvmsg" => DenyReason::Network,
                "clock_gettime" | "clock_getres" | "gettimeofday" | "time" | "nanosleep"
                | "clock_nanosleep" => DenyReason::Clock,
                "getrandom" => DenyReason::Random,
                _ => DenyReason::Unknown,
            };
            return Err(reason);
        }
    }
    if !seen_exec || tasks.is_empty() || tasks.values().any(|t| !t.exited || t.pending.is_some()) {
        return Err(bad());
    }
    let mut seen = BTreeSet::new();
    deps.retain(|p| seen.insert((p.path.clone(), p.kind)));
    Ok(deps)
}

fn is_system_path(path: &Path) -> bool {
    ["/proc", "/sys", "/dev"]
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

fn open_args(arch: Arch, call: &str) -> Option<(Option<&str>, &str)> {
    if let Some(rest) = call.strip_prefix("openat(") {
        let (dirfd, args) = rest.split_once(", ")?;
        return Some((Some(dirfd), args));
    }
    if arch == Arch::X86_64 {
        return call.strip_prefix("open(").map(|args| (None, args));
    }
    None
}

fn open_resume(call: &str) -> Option<(&'static str, &str)> {
    if let Some(result) = call.strip_prefix("<... openat resumed>) = ") {
        return Some(("openat", result));
    }
    call.strip_prefix("<... open resumed>) = ")
        .map(|result| ("open", result))
}

fn stat_args(arch: Arch, call: &str) -> Option<(Option<&str>, &str)> {
    for name in ["newfstatat(", "fstatat("] {
        if let Some(rest) = call.strip_prefix(name) {
            let (dirfd, args) = rest.split_once(", ")?;
            return Some((Some(dirfd), args));
        }
    }
    if arch == Arch::X86_64 {
        return call
            .strip_prefix("stat(")
            .or_else(|| call.strip_prefix("lstat("))
            .map(|args| (None, args));
    }
    None
}

fn access_args(arch: Arch, call: &str) -> Option<(Option<&str>, &str)> {
    if let Some(rest) = call.strip_prefix("faccessat(") {
        let (dirfd, args) = rest.split_once(", ")?;
        return Some((Some(dirfd), args));
    }
    if arch == Arch::X86_64 {
        return call.strip_prefix("access(").map(|args| (None, args));
    }
    None
}

fn readlink_args(arch: Arch, call: &str) -> Option<(Option<&str>, &str)> {
    if let Some(rest) = call.strip_prefix("readlinkat(") {
        let (dirfd, args) = rest.split_once(", ")?;
        return Some((Some(dirfd), args));
    }
    if arch == Arch::X86_64 {
        return call.strip_prefix("readlink(").map(|args| (None, args));
    }
    None
}

fn path_base<'a>(task: &'a Task, dirfd: Option<&str>) -> Result<&'a Path, DenyReason> {
    match dirfd {
        None | Some("AT_FDCWD") => Ok(&task.cwd),
        Some(fd) => {
            let fd: i32 = fd.parse().map_err(|_| DenyReason::Unsupported)?;
            let (path, directory) = task.files.get(&fd).ok_or(DenyReason::Unsupported)?;
            if !directory {
                return Err(DenyReason::Unsupported);
            }
            Ok(path)
        }
    }
}

fn parse_open_record(
    task: &mut Task,
    dirfd: Option<&str>,
    args: &str,
    deps: &mut Vec<ObservedPath>,
) -> Result<Option<Pending>, DenyReason> {
    let path = quoted_path(args, path_base(task, dirfd)?)?;
    if is_system_path(&path) {
        return Err(DenyReason::SystemPath);
    }
    let (_, tail) = args.split_once("\", ").ok_or(DenyReason::Unsupported)?;
    if let Some(flags) = tail.strip_suffix(" <unfinished ...>") {
        let directory = open_flags(flags)?;
        let name = if dirfd.is_some() { "openat" } else { "open" };
        return Ok(Some(Pending::Open(path, directory, name)));
    }
    let (flags, result) = tail.split_once(") = ").ok_or(DenyReason::Unsupported)?;
    let directory = open_flags(flags)?;
    register_open(task, path, directory, result, deps)?;
    Ok(None)
}

fn open_flags(flags: &str) -> Result<bool, DenyReason> {
    let mut read_only = false;
    let mut directory = false;
    for part in flags.split('|') {
        match part {
            "O_RDONLY" => read_only = true,
            "O_CLOEXEC" | "O_LARGEFILE" | "O_NOFOLLOW" => {}
            "O_DIRECTORY" => directory = true,
            "O_WRONLY" | "O_RDWR" | "O_CREAT" | "O_TRUNC" | "O_APPEND" => {
                return Err(DenyReason::Write);
            }
            _ => return Err(DenyReason::Unsupported),
        }
    }
    if !read_only {
        return Err(DenyReason::Unsupported);
    }
    Ok(directory)
}

fn register_open(
    task: &mut Task,
    path: PathBuf,
    directory: bool,
    result: &str,
    deps: &mut Vec<ObservedPath>,
) -> Result<(), DenyReason> {
    if result == "-1 ENOENT (No such file or directory)" {
        deps.push(ObservedPath {
            path,
            kind: PathKind::Absent,
        });
        return Ok(());
    }
    let fd: i32 = result.parse().map_err(|_| DenyReason::Unsupported)?;
    if fd < 3 || task.files.insert(fd, (path.clone(), directory)).is_some() {
        return Err(DenyReason::Unsupported);
    }
    deps.push(ObservedPath {
        path,
        kind: if directory {
            PathKind::Metadata
        } else {
            PathKind::File
        },
    });
    Ok(())
}

fn parse_stat_record(
    task: &Task,
    dirfd: Option<&str>,
    args: &str,
    deps: &mut Vec<ObservedPath>,
) -> Result<(), DenyReason> {
    let path = quoted_path(args, path_base(task, dirfd)?)?;
    if is_system_path(&path) {
        return Err(DenyReason::SystemPath);
    }
    let (_, tail) = args.split_once("\", ").ok_or(DenyReason::Unsupported)?;
    if tail.starts_with("{st_mode=") && tail.ends_with("= 0") {
        deps.push(ObservedPath {
            path,
            kind: PathKind::Metadata,
        });
        Ok(())
    } else if tail.starts_with("0x0") && tail.ends_with("= -1 ENOENT (No such file or directory)") {
        deps.push(ObservedPath {
            path,
            kind: PathKind::Absent,
        });
        Ok(())
    } else {
        Err(DenyReason::Unsupported)
    }
}

fn parse_access_record(
    task: &Task,
    dirfd: Option<&str>,
    args: &str,
    deps: &mut Vec<ObservedPath>,
) -> Result<(), DenyReason> {
    let path = quoted_path(args, path_base(task, dirfd)?)?;
    if is_system_path(&path) {
        return Err(DenyReason::SystemPath);
    }
    let (_, tail) = args.split_once("\", ").ok_or(DenyReason::Unsupported)?;
    if tail.ends_with(") = 0") {
        deps.push(ObservedPath {
            path,
            kind: PathKind::Metadata,
        });
        Ok(())
    } else if tail.ends_with("= -1 ENOENT (No such file or directory)") {
        deps.push(ObservedPath {
            path,
            kind: PathKind::Absent,
        });
        Ok(())
    } else {
        Err(DenyReason::Unsupported)
    }
}

fn parse_readlink_record(
    task: &Task,
    dirfd: Option<&str>,
    args: &str,
    deps: &mut Vec<ObservedPath>,
) -> Result<(), DenyReason> {
    let path = quoted_path(args, path_base(task, dirfd)?)?;
    if is_system_path(&path) {
        return Err(DenyReason::SystemPath);
    }
    let (_, tail) = args.split_once("\", ").ok_or(DenyReason::Unsupported)?;
    let result = tail.rsplit_once(") = ").ok_or(DenyReason::Unsupported)?.1;
    if result != "0" && result.parse::<u64>().is_err() {
        return Err(DenyReason::Unsupported);
    }
    deps.push(ObservedPath {
        path,
        kind: PathKind::Metadata,
    });
    Ok(())
}

fn parse_fstat_record(task: &Task, rest: &str) -> Result<(), DenyReason> {
    let (fd, tail) = rest.split_once(", ").ok_or(DenyReason::Unsupported)?;
    let fd: i32 = fd.parse().map_err(|_| DenyReason::Unsupported)?;
    if !task.files.contains_key(&fd) || !tail.starts_with("{st_mode=") || !tail.ends_with("= 0") {
        return Err(DenyReason::Unsupported);
    }
    Ok(())
}

fn parse_fcntl_record(task: &Task, rest: &str) -> Result<(), DenyReason> {
    let (fd, tail) = rest.split_once(", ").ok_or(DenyReason::Unsupported)?;
    let fd: i32 = fd.parse().map_err(|_| DenyReason::Unsupported)?;
    let (command, _) = tail.split_once(") = ").ok_or(DenyReason::Unsupported)?;
    if !task.files.contains_key(&fd) || !matches!(command, "F_SETFD, FD_CLOEXEC" | "F_GETFD") {
        return Err(DenyReason::Unsupported);
    }
    Ok(())
}

fn parse_lseek_record(task: &Task, rest: &str) -> Result<(), DenyReason> {
    let (fd, tail) = rest.split_once(", ").ok_or(DenyReason::Unsupported)?;
    let fd: i32 = fd.parse().map_err(|_| DenyReason::Unsupported)?;
    if !task.files.contains_key(&fd) || !tail.contains(") = ") {
        return Err(DenyReason::Unsupported);
    }
    Ok(())
}

fn parse_write_record(rest: &str) -> Result<(), DenyReason> {
    let (fd, tail) = rest.split_once(", ").ok_or(DenyReason::Unsupported)?;
    let fd = i32::from_str_radix(fd, 16).map_err(|_| DenyReason::Unsupported)?;
    if !matches!(fd, 1 | 2) {
        return Err(DenyReason::Write);
    }
    let result = tail.rsplit_once(") = ").ok_or(DenyReason::Unsupported)?.1;
    if result == "0" || result.starts_with("0x") {
        Ok(())
    } else {
        Err(DenyReason::Unsupported)
    }
}

#[cfg(target_os = "linux")]
fn tracee_status(text: &str, root: u32, wrapper: ExitStatus) -> Option<ExitStatus> {
    use std::os::unix::process::ExitStatusExt;
    // Linux aarch64 standard signals; unsupported spellings remain unknown.
    const SIGNALS: [&str; 31] = [
        "SIGHUP",
        "SIGINT",
        "SIGQUIT",
        "SIGILL",
        "SIGTRAP",
        "SIGABRT",
        "SIGBUS",
        "SIGFPE",
        "SIGKILL",
        "SIGUSR1",
        "SIGSEGV",
        "SIGUSR2",
        "SIGPIPE",
        "SIGALRM",
        "SIGTERM",
        "SIGSTKFLT",
        "SIGCHLD",
        "SIGCONT",
        "SIGSTOP",
        "SIGTSTP",
        "SIGTTIN",
        "SIGTTOU",
        "SIGURG",
        "SIGXCPU",
        "SIGXFSZ",
        "SIGVTALRM",
        "SIGPROF",
        "SIGWINCH",
        "SIGIO",
        "SIGPWR",
        "SIGSYS",
    ];
    let markers: Vec<_> = text
        .lines()
        .filter_map(|line| {
            let (pid, rest) = line.split_once(' ')?;
            if pid.parse::<u32>().ok()? != root {
                return None;
            }
            let rest = rest.trim_start();
            if let Some(code) = rest.strip_prefix("+++ exited with ") {
                return Some((
                    false,
                    code.strip_suffix(" +++")
                        .and_then(|s| s.parse::<u8>().ok())
                        .map_or(-1, i32::from),
                ));
            }
            let name = rest.strip_prefix("+++ killed by ")?;
            let name = name.strip_suffix(" +++").unwrap_or("");
            let name = name.strip_suffix(" (core dumped)").unwrap_or(name);
            let signal = SIGNALS
                .iter()
                .position(|candidate| *candidate == name)
                .and_then(|n| i32::try_from(n + 1).ok())
                .or_else(|| {
                    let suffix = name.strip_prefix("SIGRT_")?;
                    let offset = suffix.parse::<i32>().ok()?;
                    ((0..=32).contains(&offset) && suffix == offset.to_string())
                        .then(|| 32 + offset)
                })
                .unwrap_or(0);
            Some((true, signal))
        })
        .collect();
    match markers.as_slice() {
        [(false, code)] if wrapper.code() == Some(*code) => Some(ExitStatus::from_raw(*code << 8)),
        [(true, sig)]
            if *sig > 0
                && (wrapper.signal() == Some(*sig) || wrapper.code() == Some(128 + *sig)) =>
        {
            Some(ExitStatus::from_raw(*sig))
        }
        _ => None,
    }
}

// Bind only the exact fork form: the resume supplies the child PID, never a guess.
fn pair_clones(trace: &str) -> Result<Vec<Option<String>>, DenyReason> {
    if trace.len() > MAX_TRACE || trace.lines().any(|line| line.len() > 8192) {
        return Err(DenyReason::Unsupported);
    }
    let mut lines: Vec<Option<String>> = trace.lines().map(|line| Some(line.to_owned())).collect();
    let mut pending = BTreeMap::new();
    for index in 0..lines.len() {
        let line = lines[index]
            .as_deref()
            .ok_or(DenyReason::Parse(index + 1))?;
        let (pid, call) = line.split_once(' ').ok_or(DenyReason::Parse(index + 1))?;
        let pid: u32 = pid.parse().map_err(|_| DenyReason::Parse(index + 1))?;
        let call = call.trim_start();
        if call == "clone(child_stack=NULL, flags=SIGCHLD <unfinished ...>" {
            if pending.insert(pid, index).is_some() {
                return Err(DenyReason::Parse(index + 1));
            }
        } else if let Some(child) = call.strip_prefix("<... clone resumed>)") {
            let child = child
                .trim_start()
                .strip_prefix("= ")
                .ok_or(DenyReason::Parse(index + 1))?;
            let child: u32 = child.parse().map_err(|_| DenyReason::Parse(index + 1))?;
            let start = pending.remove(&pid).ok_or(DenyReason::Parse(index + 1))?;
            lines[start] = Some(format!(
                "{pid} clone(child_stack=NULL, flags=SIGCHLD) = {child}"
            ));
            lines[index] = None;
        } else if pending.contains_key(&pid) {
            return Err(DenyReason::Parse(index + 1));
        }
    }
    if !pending.is_empty() {
        return Err(DenyReason::Parse(lines.len()));
    }
    Ok(lines)
}

fn quoted_path(input: &str, cwd: &Path) -> Result<PathBuf, DenyReason> {
    let input = input.strip_prefix('"').ok_or(DenyReason::Unsupported)?;
    let (name, _) = input.split_once('"').ok_or(DenyReason::Unsupported)?;
    resolve_relative(cwd, name).ok_or(DenyReason::Unsupported)
}

/// Resolve a traced path against a base without touching the filesystem.
///
/// The tracer prints the path exactly as the child passed it. `.`, `..` and
/// repeated separators are normalized lexically; symlinks are left for the
/// policy layer, which records them explicitly.
#[must_use]
pub fn resolve_relative(base: &Path, raw: &str) -> Option<PathBuf> {
    if !base.is_absolute() || raw.is_empty() || raw.contains(['\\', '\n', '\0']) {
        return None;
    }
    let mut out = if raw.starts_with('/') {
        PathBuf::from("/")
    } else {
        base.to_path_buf()
    };
    for part in raw.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if !out.pop() {
                    return None;
                }
            }
            _ => out.push(part),
        }
    }
    Some(out)
}

/// Execute the command exactly once under the authenticated tracer.
///
/// The command's executable and working directory must be absolute and canonical.
/// A missing child status is ambiguous and must not trigger a second execution.
#[must_use]
pub fn run(argv: &[String], cwd: &Path) -> TraceOutcome {
    let denied = || TraceOutcome {
        child_status: None,
        observation: TraceObservation::Denied(DenyReason::Unsupported),
    };
    let Some(program) = argv.first() else {
        return denied();
    };
    let executable = Path::new(program);
    if !executable.is_absolute()
        || !cwd.is_absolute()
        || std::fs::canonicalize(executable).ok().as_deref() != Some(executable)
        || std::fs::canonicalize(cwd).ok().as_deref() != Some(cwd)
    {
        return denied();
    }
    run_resolved(argv, cwd, executable)
}

/// Execute the command exactly once under the authenticated tracer.
///
/// `executable` is the absolute path the tracer will report in its first
/// `execve` record. It may be a symlink or contain `.` components; the parser
/// normalizes it. `argv[0]` must match how the child is invoked. A missing
/// child status is ambiguous and must not trigger a second execution.
#[must_use]
pub fn run_resolved(argv: &[String], cwd: &Path, executable: &Path) -> TraceOutcome {
    let denied = |reason| TraceOutcome {
        child_status: None,
        observation: TraceObservation::Denied(reason),
    };
    let tracer_identity = match identity() {
        Ok(identity) => identity,
        Err(reason) => return denied(reason),
    };
    if !executable.is_absolute() || !cwd.is_absolute() || !executable.exists() {
        return denied(DenyReason::Unsupported);
    }
    #[cfg(target_os = "linux")]
    {
        collect(argv, cwd, executable, tracer_identity)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (argv, cwd, executable, tracer_identity);
        denied(DenyReason::PlatformUnsupported)
    }
}

#[cfg(target_os = "linux")]
// Collection keeps FIFO lifecycle, draining, and status verification in one auditable operation.
#[allow(clippy::too_many_lines)]
fn collect(
    argv: &[String],
    cwd: &Path,
    executable: &Path,
    tracer_identity: TracerIdentity,
) -> TraceOutcome {
    use std::fmt::Write;
    use std::io::Read;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    use std::time::Duration;
    struct Directory(PathBuf);
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.0.join("stream"));
            let _ = std::fs::remove_dir(&self.0);
        }
    }
    let denied = || TraceOutcome {
        child_status: None,
        observation: TraceObservation::Denied(DenyReason::Unsupported),
    };
    let mut nonce = [0u8; 16];
    if getrandom::fill(&mut nonce).is_err() {
        return denied();
    }
    let name = nonce.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    });
    let dir = std::env::temp_dir().join(format!("kioku-trace-{name}"));
    if std::fs::DirBuilder::new().mode(0o700).create(&dir).is_err() {
        return denied();
    }
    let guard = Directory(dir);
    let fifo = guard.0.join("stream");
    let made = std::process::Command::new("/usr/bin/mkfifo")
        .env_clear()
        .args(["-m", "600"])
        .arg(&fifo)
        .status();
    if !made.is_ok_and(|s| s.success()) {
        return denied();
    }
    // Linux O_NONBLOCK: opening the FIFO must not wait for the tracer writer.
    let Ok(mut pipe) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(0x800)
        .open(&fifo)
    else {
        return denied();
    };
    let Some(tracer) = std::env::var_os("KIOKU_STRACE") else {
        return denied();
    };
    let Ok(mut child) = std::process::Command::new(tracer)
        .args(TRACE_FLAGS)
        .arg("-o")
        .arg(&fifo)
        .arg("--")
        .args(argv)
        .current_dir(cwd)
        .spawn()
    else {
        return denied();
    };
    let mut trace = Vec::new();
    let mut overflow = false;
    let mut buf = [0u8; 8192];
    let mut status = None;
    loop {
        match pipe.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => {
                if trace.len().saturating_add(n) <= MAX_TRACE && !overflow {
                    trace.extend_from_slice(&buf[..n]);
                } else {
                    overflow = true;
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => {
                overflow = true;
            }
        }
        if status.is_none() {
            if let Ok(s) = child.try_wait() {
                status = s;
            } else {
                overflow = true;
                status = child.wait().ok();
                break;
            }
        }
        if status.is_some() {
            // The child is gone, but the tracer may still be flushing the last
            // records. Read until EOF; a writer that stays open makes the
            // trace incomplete, never silently truncated.
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if trace.len().saturating_add(n) <= MAX_TRACE && !overflow {
                            trace.extend_from_slice(&buf[..n]);
                        } else {
                            overflow = true;
                        }
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && std::time::Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => {
                        overflow = true;
                        break;
                    }
                }
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let Some(status) = status else {
        return denied();
    };
    let executable_is_static = static_elf(executable, tracer_identity.arch);
    if !executable_is_static {
        return TraceOutcome {
            child_status: Some(status),
            observation: TraceObservation::Denied(DenyReason::ExecutableFormat),
        };
    }
    let Ok(text) = std::str::from_utf8(&trace) else {
        return denied();
    };
    let Some(pid) = text
        .lines()
        .next()
        .and_then(|line| line.split_once(' '))
        .and_then(|(p, _)| p.parse::<u32>().ok())
    else {
        return denied();
    };
    let tracee_status = tracee_status(text, pid, status);
    let observation = if overflow || tracee_status.is_none_or(|status| !status.success()) {
        TraceObservation::Denied(DenyReason::Unsupported)
    } else {
        match parse_for(tracer_identity.arch, text, cwd, executable) {
            Ok(dependencies)
                if dependencies
                    .iter()
                    .filter(|p| p.kind == PathKind::Executable)
                    .all(|p| static_elf(&p.path, tracer_identity.arch)) =>
            {
                TraceObservation::Complete(Observation {
                    dependencies,
                    tracer_identity,
                })
            }
            Ok(_) => TraceObservation::Denied(DenyReason::ExecutableFormat),
            Err(reason) => TraceObservation::Denied(reason),
        }
    };
    let cleaned = std::fs::remove_file(&fifo).is_ok() && std::fs::remove_dir(&guard.0).is_ok();
    TraceOutcome {
        child_status: tracee_status,
        observation: if cleaned {
            observation
        } else {
            TraceObservation::Denied(DenyReason::Unsupported)
        },
    }
}

#[cfg(target_os = "linux")]
fn static_elf(path: &Path, arch: Arch) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut hdr = [0u8; 64];
    if file.read_exact(&mut hdr).is_err()
        || &hdr[..6] != b"\x7fELF\x02\x01"
        || u16::from_le_bytes([hdr[18], hdr[19]]) != arch.machine()
    {
        return false;
    }
    let Ok(offset_bytes) = <[u8; 8]>::try_from(&hdr[32..40]) else {
        return false;
    };
    let offset = u64::from_le_bytes(offset_bytes);
    let size = u16::from_le_bytes([hdr[54], hdr[55]]);
    let count = u16::from_le_bytes([hdr[56], hdr[57]]);
    if size != 56 || count == 0 || count > 128 || file.seek(SeekFrom::Start(offset)).is_err() {
        return false;
    }
    for _ in 0..count {
        let mut ph = [0u8; 56];
        if file.read_exact(&mut ph).is_err()
            || u32::from_le_bytes([ph[0], ph[1], ph[2], ph[3]]) == 3
        {
            return false;
        }
    }
    true
}

#[cfg(all(test, target_os = "linux"))]
mod status_tests {
    use super::tracee_status;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    #[test]
    fn realtime_signal_requires_known_spelling_and_matching_wrapper() {
        for (name, signal) in [("SIGRT_0", 32), ("SIGRT_2", 34), ("SIGRT_32", 64)] {
            let line = format!("10 +++ killed by {name} +++\n");
            for wrapper in [
                ExitStatus::from_raw(signal),
                ExitStatus::from_raw((128 + signal) << 8),
            ] {
                assert_eq!(
                    tracee_status(&line, 10, wrapper).and_then(|s| s.signal()),
                    Some(signal)
                );
            }
        }
        let trace = "10 +++ killed by SIGRT_2 +++\n";
        assert!(tracee_status(trace, 10, ExitStatus::from_raw(35)).is_none());
        assert!(tracee_status(trace, 10, ExitStatus::from_raw(163 << 8)).is_none());
        for name in [
            "SIGRT_",
            "SIGRT_-1",
            "SIGRT_33",
            "SIGRT_999",
            "SIGRT_2147483647",
            "SIGRT_-2147483648",
            "SIGRT_2147483648",
            "SIGRT_02",
            "SIGRTMIN",
            "SIGUNKNOWN",
        ] {
            let line = format!("10 +++ killed by {name} +++\n");
            assert!(tracee_status(&line, 10, ExitStatus::from_raw(34)).is_none());
        }
    }

    #[test]
    fn status_rejects_malformed_exit_mismatched_wrapper_and_unknown_signal() {
        assert!(
            tracee_status(
                "10 +++ exited with nope +++\n",
                10,
                ExitStatus::from_raw(1 << 8)
            )
            .is_none()
        );
        assert!(
            tracee_status(
                "10 +++ exited with 7 +++\n",
                10,
                ExitStatus::from_raw(8 << 8)
            )
            .is_none()
        );
        assert!(
            tracee_status(
                "10 +++ killed by SIGUNKNOWN +++\n",
                10,
                ExitStatus::from_raw(128 << 8)
            )
            .is_none()
        );
    }

    #[test]
    fn elf_gate_accepts_static_and_rejects_invalid_headers_and_interpreter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("candidate");
        let mut elf = vec![0u8; 64 + 56];
        elf[..6].copy_from_slice(b"\x7fELF\x02\x01");
        elf[18..20].copy_from_slice(&183u16.to_le_bytes());
        elf[32..40].copy_from_slice(&64u64.to_le_bytes());
        elf[54..56].copy_from_slice(&56u16.to_le_bytes());
        elf[56..58].copy_from_slice(&1u16.to_le_bytes());
        std::fs::write(&path, &elf).unwrap();
        let arch = super::Arch::Aarch64;
        assert!(super::static_elf(&path, arch));
        assert!(!super::static_elf(&path, super::Arch::X86_64));
        for (index, value) in [(0, 0), (4, 1), (5, 2), (18, 0), (54, 0), (56, 0), (56, 129)] {
            let mut bad = elf.clone();
            bad[index] = value;
            std::fs::write(&path, bad).unwrap();
            assert!(
                !super::static_elf(&path, arch),
                "invalid byte {index}={value}"
            );
        }
        let mut limit = elf[..64].to_vec();
        limit[56..58].copy_from_slice(&128u16.to_le_bytes());
        limit.resize(64 + 128 * 56, 0);
        std::fs::write(&path, &limit).unwrap();
        assert!(super::static_elf(&path, arch));
        limit[56..58].copy_from_slice(&129u16.to_le_bytes());
        std::fs::write(&path, &limit).unwrap();
        assert!(!super::static_elf(&path, arch));
        let mut bad_offset = elf.clone();
        bad_offset[32..40].copy_from_slice(&1000u64.to_le_bytes());
        std::fs::write(&path, bad_offset).unwrap();
        assert!(!super::static_elf(&path, arch));
        std::fs::write(&path, &elf[..63]).unwrap();
        assert!(!super::static_elf(&path, arch));
        std::fs::write(&path, &elf[..64]).unwrap();
        assert!(!super::static_elf(&path, arch));
        elf[64..68].copy_from_slice(&3u32.to_le_bytes());
        std::fs::write(&path, elf).unwrap();
        assert!(!super::static_elf(&path, arch));
        assert!(!super::static_elf(&dir.path().join("missing"), arch));
        let mut x86 = vec![0u8; 64 + 56];
        x86[..6].copy_from_slice(b"\x7fELF\x02\x01");
        x86[18..20].copy_from_slice(&62u16.to_le_bytes());
        x86[32..40].copy_from_slice(&64u64.to_le_bytes());
        x86[54..56].copy_from_slice(&56u16.to_le_bytes());
        x86[56..58].copy_from_slice(&1u16.to_le_bytes());
        std::fs::write(&path, x86).unwrap();
        assert!(super::static_elf(&path, super::Arch::X86_64));
        assert!(!super::static_elf(&path, super::Arch::Aarch64));
    }

    #[test]
    fn reconstructs_only_unique_agreeing_root_status() {
        let trace = "10 +++ killed by SIGTERM +++\n";
        assert_eq!(
            tracee_status(trace, 10, ExitStatus::from_raw(15)).and_then(|s| s.signal()),
            Some(15)
        );
        assert_eq!(
            tracee_status(trace, 10, ExitStatus::from_raw(143 << 8)).and_then(|s| s.signal()),
            Some(15)
        );
        for (name, sig) in [("SIGINT", 2), ("SIGKILL", 9), ("SIGTERM", 15)] {
            let line = format!("10 +++ killed by {name} +++\n");
            assert_eq!(
                tracee_status(&line, 10, ExitStatus::from_raw(sig)).and_then(|s| s.signal()),
                Some(sig)
            );
        }
        assert!(tracee_status("", 10, ExitStatus::from_raw(15)).is_none());
        assert!(
            tracee_status(
                "11 +++ killed by SIGTERM +++\n",
                10,
                ExitStatus::from_raw(15)
            )
            .is_none()
        );
        assert!(tracee_status(trace, 10, ExitStatus::from_raw(2)).is_none());
        assert!(
            tracee_status(
                "10 +++ killed by SIGUNKNOWN +++\n",
                10,
                ExitStatus::from_raw(15)
            )
            .is_none()
        );
        assert!(tracee_status(&(trace.to_owned() + trace), 10, ExitStatus::from_raw(15)).is_none());
        assert!(
            tracee_status(
                &(trace.to_owned() + "10 +++ exited with 999 +++\n"),
                10,
                ExitStatus::from_raw(15)
            )
            .is_none()
        );
        assert_eq!(
            tracee_status(
                "10 +++ exited with 7 +++\n",
                10,
                ExitStatus::from_raw(7 << 8)
            )
            .and_then(|s| s.code()),
            Some(7)
        );
    }
}
