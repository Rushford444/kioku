//! Execute commands conservatively; traced reuse requires explicit consent.
//!
//! Trace hits apply only to exit status, not output or generated artifacts.
//! The caller must keep inputs stable and accept the documented trace assumptions.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Instant;

use crate::config;
use crate::fingerprint::Mode;
use crate::key;
use crate::store::{self, Counters, Store};

/// Errors raised while running a command.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The cache key could not be computed.
    #[error(transparent)]
    Key(#[from] crate::Error),
    /// The cache store failed.
    #[error(transparent)]
    Store(#[from] store::Error),
    /// The configuration could not be read or parsed.
    #[error(transparent)]
    Config(#[from] config::Error),
    /// The child process could not be started.
    #[error("cannot execute {program}: {source}")]
    Launch {
        /// Program that could not be started.
        program: String,
        /// Underlying operating system error.
        source: std::io::Error,
    },
    /// No command was given.
    #[error("no command given")]
    EmptyCommand,
    /// The tracer may have started the child but its status is unknown.
    #[error("tracer returned an unknown child status; command not retried")]
    TraceStatusUnknown,
    /// Verification rate must be finite and in the inclusive unit interval.
    #[error("verify rate must be between 0 and 1")]
    InvalidVerifyRate,
    /// Non-UTF-8 repository or working directory cannot identify a trace safely.
    #[error("trace identity does not support non-UTF-8 paths")]
    UnsupportedIdentityPath,
}

/// What kioku decided about a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The cached pass was reused; the command did not run.
    Cached,
    /// The command ran.
    Executed,
    /// The command ran although a cached pass existed, because caching was disabled.
    Bypassed,
}

/// A command to run, explain or verify.
#[derive(Debug, Clone)]
pub struct Request {
    /// Exact command line, `argv[0]` being the program.
    pub argv: Vec<String>,
    /// Working directory of the run.
    pub cwd: PathBuf,
    /// Explicit fingerprint mode; when absent the mode is detected from the command.
    pub mode: Option<Mode>,
}

/// Result of a run.
#[derive(Debug, Clone)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent result facts in the public API"
)]
pub struct Outcome {
    /// What kioku decided.
    pub decision: Decision,
    /// Cache key of the run.
    pub key: String,
    /// Exact command line.
    pub argv: Vec<String>,
    /// Working directory, relative to the repository root.
    pub cwd: String,
    /// Fingerprint mode.
    pub mode: String,
    /// Exit code of the child process.
    pub exit_code: i32,
    /// Duration of the run, in milliseconds. A cached run reports the cost of
    /// the hit itself.
    pub duration_ms: u64,
    /// Measured time saved by reusing a cached pass, or `None` when the run
    /// that learned it did not record its duration.
    pub saved_ms: Option<u64>,
    /// Whether the result was stored.
    pub stored: bool,
    /// Number of learned passes this run quarantined because it exposed a
    /// false green: zero or one.
    pub invalidated: usize,
    /// Whether kioku would have skipped the command.
    pub would_skip: bool,
    /// Whether the verification exposed a false green.
    pub false_green: bool,
    /// Non fatal problems, reported to the user.
    pub warnings: Vec<String>,
    /// Machine-readable decision reason.
    pub reason: String,
    /// Whether trace policy found a reusable result.
    pub cacheable: bool,
    /// Actual observation state.
    pub trace_state: String,
    /// Identity of the authenticated tracer, if used.
    pub tracer_sha256: Option<String>,
    /// Flags of the authenticated tracer, if used.
    pub tracer_flags: Vec<String>,
    /// Tracer grammar identifier.
    pub tracer_schema: Option<String>,
    /// Stable reason for a denied observation.
    pub denial_reason: Option<String>,
    /// Digest of the private observed manifest, if available.
    pub manifest_digest: Option<String>,
}

/// Why the command is a hit or a miss.
#[derive(Debug, Clone)]
pub struct Explanation {
    /// Cache key of the command.
    pub key: String,
    /// Whether the command is a hit.
    pub hit: bool,
    /// Human readable reason.
    pub reason: String,
    /// Effective mode, if a snapshot was captured.
    pub mode: String,
    /// Whether a previously recorded AST entry would be a hit.
    pub ast_would_skip: Option<bool>,
    /// Number of changed or unreadable trace dependencies, without paths.
    pub changed_dependencies: Option<Vec<crate::policy::DependencyChange>>,
    /// Whether the environment digest changed.
    pub environment_changed: Option<bool>,
    /// Whether the tracer identity changed.
    pub tracer_changed: Option<bool>,
    /// Machine-readable denial reason, when the command cannot be traced.
    pub denial_reason: Option<String>,
    /// Number of paths recorded in the stored manifest, when one exists.
    pub dependencies: usize,
    /// Whether traced reuse was authorized for this explanation.
    pub traced: bool,
}

/// Statistics of the store.
#[derive(Debug, Clone)]
pub struct Status {
    /// Root directory of the store.
    pub store_root: PathBuf,
    /// Number of valid entries.
    pub entries: usize,
    /// Number of files that are not readable entries.
    pub invalid: usize,
    /// Size of the entry files, in bytes.
    pub bytes: u64,
    /// Cumulative counters.
    pub counters: Counters,
    /// Sum of the durations of the cached passes.
    pub cached_ms: u64,
    /// Number of authenticated trace states.
    pub trace_states: usize,
    /// Number of persistent quarantines.
    pub quarantines: usize,
    /// Why the cache is off for this store, when it is off.
    ///
    /// The counters are empty then: the store is not read, because reading it
    /// would create it.
    pub denial_reason: Option<String>,
}

/// Result of a clean operation.
#[derive(Debug, Clone, Default)]
pub struct OutcomeClean {
    /// Number of removed files.
    pub removed: usize,
    /// Number of kept files.
    pub kept: usize,
    /// Freed bytes.
    pub bytes_freed: u64,
    /// Why the cache is off for this store, when it is off.
    ///
    /// Nothing is removed then, and the store is not read: reading it would
    /// create it.
    pub denial_reason: Option<String>,
}

/// Tells whether the cache is disabled by the environment.
#[must_use]
pub fn cache_disabled() -> bool {
    match std::env::var("KIOKU_DISABLE") {
        Ok(value) => {
            let value = value.trim().to_ascii_lowercase();
            value == "1" || value == "true" || value == "yes" || value == "on"
        }
        Err(_) => false,
    }
}

/// Tells whether the standard input carries content that the key cannot capture.
///
/// A pipe or a redirected file can bring a different content on every run. That
/// content is not part of the key, so a command that reads it cannot be cached:
/// kioku always runs it and never memorizes its result. The standard input is
/// inspected, never consumed, and the child still inherits it.
///
/// An interactive terminal can supply input; only a null device is cacheable.
///
/// When the kind of the standard input cannot be read, the safe side is taken:
/// the content is assumed to be there, so the command is executed and nothing is
/// memorized.
#[cfg(unix)]
fn stdin_carries_content() -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    match std::fs::metadata("/dev/stdin") {
        Ok(meta) => {
            let is_null = std::fs::metadata("/dev/null")
                .is_ok_and(|null| meta.file_type().is_char_device() && meta.rdev() == null.rdev());
            !is_null
        }
        // The kind of the standard input cannot be read: take the safe side.
        Err(_) => true,
    }
}

#[cfg(not(unix))]
fn stdin_carries_content() -> bool {
    // On Windows the kind of the standard input cannot be read without the
    // Win32 API, so anything that is not a terminal is treated as carrying
    // content.
    use std::io::IsTerminal;
    !std::io::stdin().is_terminal()
}

/// Runs a command without reuse. Use [`run_with_options`] for opt-in tracing.
///
/// # Errors
/// Returns an error when the command cannot be started.
pub fn run(request: &Request) -> Result<Outcome, Error> {
    execute_only(request)
}

/// Runs a command without reuse. Use [`run_with_options`] with verification for shadow evaluation.
///
/// # Errors
/// Returns an error when the command cannot be started.
pub fn verify(request: &Request) -> Result<Outcome, Error> {
    execute_only(request)
}

/// Trace reuse options. Tracing is opt-in and never replays output.
#[derive(Debug, Clone, Copy, Default)]
pub struct TraceOptions {
    /// The caller asserts stable inputs and deterministic unobserved state.
    pub trust_trace: bool,
    /// Always execute to preserve output.
    pub require_output: bool,
    /// Fraction of otherwise reusable results to verify.
    pub verify_rate: f64,
}

struct PreparedTrace {
    identity: crate::policy::Identity,
    id: String,
    previous: Option<crate::policy::State>,
    before: Option<crate::policy::Manifest>,
    would_skip: bool,
    quarantined: bool,
    manifest_digest: Option<String>,
    timing: Option<u64>,
}

fn update_hash(hash: &mut blake3::Hasher, value: &[u8]) {
    hash.update(&(value.len() as u64).to_le_bytes());
    hash.update(value);
}

fn command_digest(repo: &Path, cwd: &Path, argv: &[String]) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    update_hash(&mut hash, b"kioku/v2/command");
    // The store is already scoped to one repository. The key keeps the
    // repository-relative working directory so two clones of the same tree
    // produce the same key.
    let relative = cwd.strip_prefix(repo).unwrap_or(cwd);
    update_hash(&mut hash, relative.to_string_lossy().as_bytes());
    for arg in argv {
        update_hash(&mut hash, arg.as_bytes());
    }
    *hash.finalize().as_bytes()
}

/// Explicitly clear the command quarantine and require new discovery and learning.
///
/// # Errors
/// Returns an error if the repository or private store cannot be accessed.
pub fn reset_quarantine(request: &Request) -> Result<(), Error> {
    if request.argv.is_empty() {
        return Err(Error::EmptyCommand);
    }
    let (repo, git_dir) = key::repository(&request.cwd)?;
    let cwd = std::fs::canonicalize(&request.cwd).map_err(|source| Error::Launch {
        program: "current directory".into(),
        source,
    })?;
    if repo.to_str().is_none() || cwd.to_str().is_none() {
        return Err(Error::UnsupportedIdentityPath);
    }
    let root = store::store_dir(&cwd, &git_dir);
    let Some(store) = usable_store(&root, &repo, &git_dir) else {
        return Ok(());
    };
    store.trace_transaction(|tx| tx.reset_quarantine(&command_key(&repo, &cwd, &request.argv)))?;
    Ok(())
}

fn prepare_trace(
    store: &Store,
    repo: &Path,
    cwd: &Path,
    argv: &[String],
    tracer_identity: &crate::trace::TracerIdentity,
) -> Result<PreparedTrace, store::Error> {
    store.trace_transaction(|tx| {
        let argv_digest = command_digest(repo, cwd, argv);
        let id = blake3::Hash::from(argv_digest).to_hex().to_string();
        let vars: Vec<_> = std::env::vars_os().collect();
        let environment_digest =
            crate::policy::env_digest(&tx.secret, &vars).map_err(|_| store::Error::Io {
                path: store.root().to_path_buf(),
                source: std::io::Error::other("unsupported environment"),
            })?;
        let mut tracer = blake3::Hasher::new();
        for part in std::iter::once(&tracer_identity.sha256)
            .chain(std::iter::once(&tracer_identity.version))
            .chain(std::iter::once(&tracer_identity.schema))
            .chain(tracer_identity.flags.iter())
        {
            update_hash(&mut tracer, part.as_bytes());
        }
        let identity = crate::policy::Identity {
            repository: repo.to_string_lossy().into_owned(),
            cwd: cwd.to_string_lossy().into_owned(),
            argv_digest,
            environment_digest,
            tracer_digest: *tracer.finalize().as_bytes(),
            version: 2,
        };
        let previous = tx.state(&id)?;
        let before = previous.as_ref().and_then(|state| {
            state
                .manifest()
                .revalidate()
                .ok()
                .filter(|valid| *valid)
                .map(|_| state.manifest().clone())
        });
        let quarantined = tx.quarantined(&id)?;
        let would_skip = before.as_ref().is_some_and(|manifest| {
            crate::policy::decide(&identity, previous.as_ref(), manifest, true, quarantined)
        });
        let manifest_digest = before
            .as_ref()
            .and_then(|manifest| manifest_digest(&tx.secret, manifest));
        // Timing is telemetry: an unreadable record means an unknown saving,
        // never a failure of the run.
        let timing = tx.timing(&id).ok().flatten();
        Ok(PreparedTrace {
            identity,
            id,
            previous,
            before,
            would_skip,
            quarantined,
            manifest_digest,
            timing,
        })
    })
}

fn authorize_hit(store: &Store, prepared: &PreparedTrace) -> bool {
    store
        .trace_transaction(|tx| {
            if tx.quarantined(&prepared.id)? {
                return Ok(false);
            }
            let current = tx.state(&prepared.id)?;
            Ok(current == prepared.previous
                && prepared.before.as_ref().is_some_and(|manifest| {
                    crate::policy::decide(
                        &prepared.identity,
                        current.as_ref(),
                        manifest,
                        true,
                        false,
                    )
                }))
        })
        .unwrap_or(false)
}

fn manifest_digest(secret: &[u8; 32], manifest: &crate::policy::Manifest) -> Option<String> {
    let bytes = serde_json::to_vec(manifest).ok()?;
    Some(blake3::keyed_hash(secret, &bytes).to_hex().to_string())
}

fn sample(rate: f64) -> bool {
    if rate <= 0.0 {
        return false;
    }
    if rate >= 1.0 {
        return true;
    }
    let mut bytes = [0; 4];
    if getrandom::fill(&mut bytes).is_err() {
        return true;
    }
    sample_from_u32(rate, u32::from_le_bytes(bytes))
}

fn sample_from_u32(rate: f64, value: u32) -> bool {
    f64::from(value) / 4_294_967_296.0 < rate
}

/// Run under the explicit traced-reuse contract.
///
/// # Errors
/// Returns an error if the child cannot be launched or its traced status is unknown.
pub fn run_with_options(
    request: &Request,
    options: TraceOptions,
    verify: bool,
) -> Result<Outcome, Error> {
    let before = if cache_disabled() {
        None
    } else {
        ast_snapshot(request)
    };
    let result = run_with_options_inner(request, options, verify);
    if let (Ok(outcome), Some(before)) = (&result, before)
        && outcome.exit_code == 0
        && outcome.decision != Decision::Cached
        && let Some(after) = ast_snapshot(request)
        && before == after
    {
        let _ = ast_diagnostic(request, &after, true);
    }
    result
}

#[allow(
    clippy::too_many_lines,
    reason = "one trace execution and its publication are kept together"
)]
fn run_with_options_inner(
    request: &Request,
    options: TraceOptions,
    verify: bool,
) -> Result<Outcome, Error> {
    if let Ok((root, _)) = key::repository(&request.cwd) {
        let _configured_mode = config::mode(&root, &request.argv)?;
    }
    if !options.verify_rate.is_finite() || !(0.0..=1.0).contains(&options.verify_rate) {
        return Err(Error::InvalidVerifyRate);
    }
    let bypass = if options.require_output {
        Some("output-required")
    } else if !options.trust_trace {
        Some("execute-only")
    } else if cache_disabled() {
        Some("cache-disabled")
    } else if stdin_carries_content() {
        Some("stdin-unsupported")
    } else {
        None
    };
    if let Some(reason) = bypass {
        let mut out = execute_only_counted(request, |counters| {
            counters.bypassed = counters.bypassed.saturating_add(1);
        })?;
        out.reason = reason.into();
        if reason == "stdin-unsupported" {
            out.denial_reason = Some("stdin".into());
        }
        return Ok(out);
    }
    let count_miss = |counters: &mut store::Counters| {
        counters.misses = counters.misses.saturating_add(1);
    };
    let Ok((repo, git_dir)) = key::repository(&request.cwd) else {
        let mut out = execute_only_counted(request, count_miss)?;
        out.reason = "repository-unavailable".into();
        out.denial_reason = Some("repository-unavailable".into());
        return Ok(out);
    };
    let Ok(cwd) = std::fs::canonicalize(&request.cwd) else {
        let mut out = execute_only_counted(request, count_miss)?;
        out.reason = "cwd-unavailable".into();
        out.denial_reason = Some("cwd-unavailable".into());
        return Ok(out);
    };
    if repo.to_str().is_none() || cwd.to_str().is_none() {
        let mut out = execute_only_counted(request, count_miss)?;
        out.reason = "trace-unsupported-path".into();
        out.denial_reason = Some("trace-unsupported-path".into());
        return Ok(out);
    }
    // A store in the work tree turns the cache off. The refusal comes before
    // the tracer: it holds whatever the tracer can observe, and the store is
    // the reason, not the tracing.
    let root = store::store_dir(&cwd, &git_dir);
    let Some(store) = usable_store(&root, &repo, &git_dir) else {
        let mut out = execute_only_counted(request, |_| {})?;
        out.reason = "store-in-work-tree".into();
        out.denial_reason = Some("store-in-work-tree".into());
        fill_key(&mut out, request);
        return Ok(out);
    };
    let tracer_identity = match crate::trace::identity() {
        Ok(identity) => identity,
        Err(reason) => {
            let mut out = execute_only_counted(request, count_miss)?;
            out.reason = "trace-unavailable".into();
            out.denial_reason = Some(reason.code().into());
            fill_key(&mut out, request);
            return Ok(out);
        }
    };
    let Some((program, traced_argv)) = resolve_program(&request.argv, &cwd) else {
        let mut out = execute_only_counted(request, count_miss)?;
        out.reason = "program-unresolved".into();
        out.denial_reason = Some("program-unresolved".into());
        fill_key(&mut out, request);
        return Ok(out);
    };
    let start = Instant::now();
    let prepared = prepare_trace(&store, &repo, &cwd, &traced_argv, &tracer_identity);
    let Ok(prepared) = prepared else {
        let mut out = execute_only_counted(request, count_miss)?;
        out.reason = "trace-store-unavailable".into();
        return Ok(out);
    };
    let may_hit = prepared.would_skip
        && !verify
        && !sample(options.verify_rate)
        && authorize_hit(&store, &prepared);
    if may_hit {
        let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        let saved_ms = prepared
            .timing
            .map(|recorded| recorded.saturating_sub(duration_ms));
        let _ = store.update_counters(|counters| {
            counters.hits = counters.hits.saturating_add(1);
            if let Some(saved) = saved_ms {
                counters.measured_hits = Some(
                    counters
                        .measured_hits
                        .map_or(0, |count| count.saturating_add(1)),
                );
                counters.saved_ms = counters.saved_ms.saturating_add(saved);
            }
        });
        return Ok(Outcome {
            decision: Decision::Cached,
            key: prepared.id,
            argv: request.argv.clone(),
            cwd: cwd.to_string_lossy().into_owned(),
            mode: "bytes".into(),
            exit_code: 0,
            duration_ms,
            saved_ms,
            stored: false,
            invalidated: 0,
            would_skip: true,
            false_green: false,
            warnings: Vec::new(),
            reason: "trace-hit".into(),
            cacheable: true,
            trace_state: "complete".into(),
            tracer_sha256: Some(tracer_identity.sha256.clone()),
            tracer_flags: tracer_identity.flags.clone(),
            tracer_schema: Some(tracer_identity.schema.clone()),
            denial_reason: None,
            manifest_digest: prepared.manifest_digest,
        });
    }
    let traced = crate::trace::run_resolved(&traced_argv, &cwd, &program);
    let status = traced.child_status.ok_or(Error::TraceStatusUnknown)?;
    let exit_code = exit_code_of(&status);
    let false_green = prepared.would_skip && exit_code != 0;
    let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    let mut quarantined_now = false;
    let mut reason = if prepared.quarantined {
        "quarantined"
    } else {
        "trace-denied"
    };
    let mut stored = false;
    let mut cacheable = false;
    let mut observed_digest = None;
    let mut trace_state = if matches!(
        &traced.observation,
        crate::trace::TraceObservation::Complete(_)
    ) {
        "complete"
    } else {
        "denied"
    };
    let mut denial_reason = match &traced.observation {
        crate::trace::TraceObservation::Denied(reason) => Some(reason.code().to_owned()),
        crate::trace::TraceObservation::Complete(_) => None,
    };
    if false_green {
        match store.trace_transaction(|tx| tx.quarantine(&prepared.id)) {
            Ok(created) => quarantined_now = created,
            Err(error) => {
                let _ = store.update_counters(|counters| {
                    counters.misses = counters.misses.saturating_add(1);
                    counters.executions = counters.executions.saturating_add(1);
                });
                return Err(Error::Store(error));
            }
        }
        reason = "false-green-quarantined";
    } else if !prepared.quarantined
        && exit_code == 0
        && let crate::trace::TraceObservation::Complete(observation) = traced.observation
        && observation.tracer_identity == tracer_identity
    {
        let captured = crate::policy::Manifest::capture(&observation.dependencies);
        if let Err(error) = &captured {
            denial_reason = Some(
                match error {
                    crate::policy::PolicyError::SystemPath => "system-path",
                    crate::policy::PolicyError::Unsupported => "policy-unsupported",
                    crate::policy::PolicyError::Io(_) => "policy-unreadable",
                    crate::policy::PolicyError::Incomplete => "policy-incomplete",
                    crate::policy::PolicyError::DuplicateEnvironment => {
                        "policy-duplicate-environment"
                    }
                }
                .into(),
            );
        }
        if let Ok(after) = captured {
            trace_state = "complete";
            observed_digest = store
                .trace_transaction(|tx| Ok(manifest_digest(&tx.secret, &after)))
                .ok()
                .flatten();
            if let Ok(state) = crate::policy::observe(
                &prepared.identity,
                prepared.previous.as_ref(),
                prepared.before.as_ref(),
                &after,
                true,
                true,
            ) {
                stored = store
                    .trace_transaction(|tx| {
                        if tx.quarantined(&prepared.id)?
                            || tx.state(&prepared.id)? != prepared.previous
                        {
                            return Ok(false);
                        }
                        // A duration describes the state it was measured on: a
                        // new state replaces it, an equivalent one keeps the
                        // first record, so a verification never replaces the
                        // duration of the learning run. Timing is telemetry;
                        // losing it must not undo the pass.
                        let unchanged = prepared.previous.as_ref() == Some(&state);
                        tx.set_state(&prepared.id, &state)?;
                        if !unchanged || tx.timing(&prepared.id).ok().flatten().is_none() {
                            let _ = tx.set_timing(&prepared.id, duration_ms);
                        }
                        Ok(true)
                    })
                    .unwrap_or(false);
                cacheable = stored && state.is_eligible();
                reason = if stored {
                    "trace-learning"
                } else {
                    "trace-store-unavailable"
                };
            }
        }
    }
    // One update per run: the miss, the execution and, when it happened, the
    // quarantine of the pass that failed its verification.
    let _ = store.update_counters(|counters| {
        counters.misses = counters.misses.saturating_add(1);
        counters.executions = counters.executions.saturating_add(1);
        if quarantined_now {
            counters.invalidated = counters.invalidated.saturating_add(1);
        }
    });
    Ok(Outcome {
        decision: Decision::Executed,
        key: prepared.id,
        argv: request.argv.clone(),
        cwd: cwd.to_string_lossy().into_owned(),
        mode: "bytes".into(),
        exit_code,
        duration_ms,
        saved_ms: Some(0),
        stored,
        invalidated: usize::from(quarantined_now),
        would_skip: prepared.would_skip,
        false_green,
        warnings: Vec::new(),
        reason: reason.into(),
        cacheable,
        trace_state: trace_state.into(),
        tracer_sha256: Some(tracer_identity.sha256),
        tracer_flags: tracer_identity.flags,
        tracer_schema: Some(tracer_identity.schema),
        denial_reason,
        manifest_digest: observed_digest,
    })
}

/// Resolve a command to the absolute path the tracer will report.
///
/// A program containing `/` keeps its lexical form, so symlinks and `..`
/// remain visible to the policy. A bare name is resolved through `PATH` and
/// `argv[0]` is replaced by that absolute path, because the trace would
/// otherwise contain a name that cannot be resolved from the child's cwd.
fn resolve_program(argv: &[String], cwd: &Path) -> Option<(PathBuf, Vec<String>)> {
    let program = argv.first()?;
    if program.is_empty() {
        return None;
    }
    if program.contains('/') {
        let executable = crate::trace::resolve_relative(cwd, program)?;
        let canonical = std::fs::canonicalize(&executable).ok()?;
        if !canonical.is_file() || !is_executable(&canonical) {
            return None;
        }
        Some((executable, argv.to_vec()))
    } else {
        let executable = find_in_path(program)?;
        let mut resolved = argv.to_vec();
        executable.to_str()?.clone_into(&mut resolved[0]);
        Some((executable, resolved))
    }
}

fn find_in_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file() && is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Compute the stable command key without executing or observing anything.
///
/// # Errors
/// Returns an error if the private store cannot be opened.
#[must_use]
pub fn command_key(repo: &Path, cwd: &Path, argv: &[String]) -> String {
    let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let resolved =
        resolve_program(argv, &cwd).map_or_else(|| argv.to_vec(), |(_program, argv)| argv);
    blake3::Hash::from(command_digest(repo, &cwd, &resolved))
        .to_hex()
        .to_string()
}

fn fill_key(out: &mut Outcome, request: &Request) {
    if !out.key.is_empty() {
        return;
    }
    let Ok((repo, _git_dir)) = key::repository(&request.cwd) else {
        return;
    };
    let Ok(cwd) = std::fs::canonicalize(&request.cwd) else {
        return;
    };
    if repo.to_str().is_none() || cwd.to_str().is_none() {
        return;
    }
    out.key = command_key(&repo, &cwd, &request.argv);
}

/// The store of a run, when the cache may use it.
///
/// A store in the work tree turns the cache off: the files of the store are
/// not part of the key, so a tracked file that happens to live there would be
/// left out of it too, and a change of it would not change the key. The
/// default store lives in the git directory, which git never lists, so it is
/// safe.
///
/// `None` means the cache is off. Kioku then creates nothing in that store,
/// learns nothing there and reuses nothing from it. The store is not even
/// read: reading it would create it in the work tree.
fn usable_store(root: &Path, repo: &Path, git_dir: &Path) -> Option<Store> {
    if key::store_inside_work_tree(root, repo, git_dir) {
        return None;
    }
    Some(Store::open(root.to_path_buf()))
}

/// Adds to the counters of the store of `cwd`.
///
/// Counters are telemetry: every failure is ignored and never fails a command.
fn record_counters(cwd: &Path, update: impl FnOnce(&mut store::Counters)) {
    let Ok((repo, git_dir)) = key::repository(cwd) else {
        return;
    };
    let root = store::store_dir(cwd, &git_dir);
    let Some(store) = usable_store(&root, &repo, &git_dir) else {
        return;
    };
    let _ = store.update_counters(update);
}

fn execute_only(request: &Request) -> Result<Outcome, Error> {
    execute_only_counted(request, |_| {})
}

/// Executes `request` and records the execution, plus the `extra` counters of
/// the caller, in a single store update.
fn execute_only_counted(
    request: &Request,
    extra: impl FnOnce(&mut store::Counters),
) -> Result<Outcome, Error> {
    let started = Instant::now();
    let status = execute(&request.argv, &request.cwd)?;
    // Measured before the telemetry write: the reported duration is the cost of
    // the child, not the cost of the counters update that follows it.
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    record_counters(&request.cwd, |counters| {
        counters.executions = counters.executions.saturating_add(1);
        extra(counters);
    });
    Ok(Outcome {
        decision: Decision::Executed,
        key: String::new(),
        argv: request.argv.clone(),
        cwd: request.cwd.to_string_lossy().into_owned(),
        mode: "bytes".to_string(),
        exit_code: exit_code_of(&status),
        duration_ms,
        saved_ms: Some(0),
        stored: false,
        invalidated: 0,
        would_skip: false,
        false_green: false,
        warnings: Vec::new(),
        reason: "execute-only".into(),
        cacheable: false,
        trace_state: "unavailable".into(),
        tracer_sha256: None,
        tracer_flags: Vec::new(),
        tracer_schema: None,
        denial_reason: None,
        manifest_digest: None,
    })
}

/// Explains whether a command is a hit or a miss.
///
/// # Errors
///
/// Returns an error when the cache cannot be read.
pub fn explain(request: &Request) -> Result<Explanation, Error> {
    explain_with_options(
        request,
        TraceOptions {
            trust_trace: false,
            require_output: false,
            verify_rate: 0.0,
        },
    )
}

#[cfg(unix)]
fn ast_snapshot(request: &Request) -> Option<(String, String)> {
    use std::os::unix::ffi::OsStringExt;
    let (repo, git_dir) = key::repository(&request.cwd).ok()?;
    let output = Command::new("git")
        .current_dir(&repo)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut files: Vec<_> = output
        .stdout
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .collect();
    files.sort_unstable();
    files.dedup();
    let store_path = store::store_dir(&request.cwd, &git_dir);
    if key::store_inside_work_tree(&store_path, &repo, &git_dir) {
        return None;
    }
    let store = Store::open(store_path);
    store
        .trace_transaction(|tx| {
            let mut semantic = blake3::Hasher::new_keyed(&tx.secret);
            let mut raw = blake3::Hasher::new_keyed(&tx.secret);
            let vars: Vec<_> = std::env::vars_os().collect();
            let env =
                crate::policy::env_digest(&tx.secret, &vars).map_err(|_| store::Error::Io {
                    path: repo.clone(),
                    source: std::io::Error::other("unsupported environment"),
                })?;
            semantic.update(&env);
            raw.update(&env);
            for name in files {
                let path = repo.join(std::ffi::OsString::from_vec(name.to_vec()));
                let metadata =
                    std::fs::symlink_metadata(&path).map_err(|source| store::Error::Io {
                        path: path.clone(),
                        source,
                    })?;
                if !metadata.is_file() {
                    return Err(store::Error::Io {
                        path,
                        source: std::io::Error::other("unsupported diagnostic file"),
                    });
                }
                let data = std::fs::read(&path).map_err(|source| store::Error::Io {
                    path: path.clone(),
                    source,
                })?;
                update_hash(&mut semantic, name);
                update_hash(&mut raw, name);
                update_hash(
                    &mut semantic,
                    crate::fingerprint::fingerprint(&path, &data, crate::Mode::Ast).as_bytes(),
                );
                update_hash(&mut raw, &data);
            }
            Ok((
                semantic.finalize().to_hex().to_string(),
                raw.finalize().to_hex().to_string(),
            ))
        })
        .ok()
}
#[cfg(not(unix))]
fn ast_snapshot(_: &Request) -> Option<(String, String)> {
    None
}

fn ast_diagnostic(request: &Request, snapshot: &(String, String), write: bool) -> Option<bool> {
    let (repo, git_dir) = key::repository(&request.cwd).ok()?;
    let cwd = std::fs::canonicalize(&request.cwd).ok()?;
    let store = Store::open(store::store_dir(&cwd, &git_dir));
    store
        .trace_transaction(|tx| {
            let id = command_key(&repo, &cwd, &request.argv);
            tx.ast_diagnostic(&id, &snapshot.0, write)
        })
        .ok()
}

/// Explain a candidate under the selected trace policy without executing the child.
///
/// # Errors
/// Returns an error if configuration or repository state cannot be read.
#[allow(clippy::too_many_lines)]
pub fn explain_with_options(
    request: &Request,
    options: TraceOptions,
) -> Result<Explanation, Error> {
    let mut explanation = Explanation {
        key: String::new(),
        hit: false,
        reason: "execute-only".into(),
        mode: match request.mode {
            Some(Mode::Ast) => "ast".into(),
            _ => "bytes".into(),
        },
        ast_would_skip: None,
        changed_dependencies: None,
        environment_changed: None,
        tracer_changed: None,
        denial_reason: None,
        dependencies: 0,
        traced: false,
    };
    let (repo, git_dir) = key::repository(&request.cwd)?;
    config::mode(&repo, &request.argv)?;
    explanation.key = command_key(&repo, &request.cwd, &request.argv);
    if let Some(snapshot) = ast_snapshot(request) {
        explanation.ast_would_skip = ast_diagnostic(request, &snapshot, false);
    }
    if options.require_output {
        explanation.reason = "output-required".into();
        return Ok(explanation);
    }
    if cache_disabled() {
        explanation.reason = "cache-disabled".into();
        return Ok(explanation);
    }
    if stdin_carries_content() {
        explanation.reason = "stdin-unsupported".into();
        return Ok(explanation);
    }
    // A store in the work tree turns the cache off, so no trace can be
    // reused. The explanation says so, as the run does.
    let root = store::store_dir(&request.cwd, &git_dir);
    let Some(store) = usable_store(&root, &repo, &git_dir) else {
        explanation.reason = "store-in-work-tree".into();
        explanation.denial_reason = Some("store-in-work-tree".into());
        return Ok(explanation);
    };
    let tracer = match crate::trace::identity() {
        Ok(tracer) => tracer,
        Err(reason) => {
            explanation.reason = "trace-unavailable".into();
            explanation.denial_reason = Some(reason.code().into());
            return Ok(explanation);
        }
    };
    let cwd = std::fs::canonicalize(&request.cwd).map_err(|source| Error::Launch {
        program: "current directory".into(),
        source,
    })?;
    if repo.to_str().is_none() || cwd.to_str().is_none() {
        explanation.reason = "trace-unsupported-path".into();
        explanation.denial_reason = Some("trace-unsupported-path".into());
        return Ok(explanation);
    }
    let Some((_program, traced_argv)) = resolve_program(&request.argv, &cwd) else {
        explanation.reason = "program-unresolved".into();
        explanation.denial_reason = Some("program-unresolved".into());
        return Ok(explanation);
    };
    let Ok(prepared) = prepare_trace(&store, &repo, &cwd, &traced_argv, &tracer) else {
        explanation.reason = "trace-store-unavailable".into();
        return Ok(explanation);
    };
    explanation.key.clone_from(&prepared.id);
    explanation.dependencies = prepared
        .previous
        .as_ref()
        .map_or(0, |state| state.manifest().len());
    explanation.changed_dependencies = store
        .trace_transaction(|tx| {
            Ok(prepared
                .previous
                .as_ref()
                .map(|state| state.manifest().changes(&tx.secret)))
        })
        .ok()
        .flatten();
    explanation.environment_changed = prepared
        .previous
        .as_ref()
        .map(|state| state.environment_changed(&prepared.identity));
    explanation.tracer_changed = prepared
        .previous
        .as_ref()
        .map(|state| state.tracer_changed(&prepared.identity));
    explanation.traced = options.trust_trace;
    if !options.trust_trace {
        explanation.reason = "execute-only".into();
        return Ok(explanation);
    }
    explanation.hit = authorize_hit(&store, &prepared);
    explanation.reason = if prepared.quarantined {
        "quarantined"
    } else if prepared.previous.is_none() {
        "no-state"
    } else if prepared.before.is_none() {
        "dependency-changed"
    } else if !prepared
        .previous
        .as_ref()
        .is_some_and(|state| state.matches_identity(&prepared.identity))
    {
        "identity-changed"
    } else if !prepared.would_skip {
        "learning"
    } else if explanation.hit {
        "trace-hit"
    } else {
        "state-changed"
    }
    .into();
    Ok(explanation)
}

/// Persist a sanitized verification result without command arguments or environment.
///
/// # Errors
/// Returns an error if the repository or private journal cannot be accessed.
pub fn log_verification(request: &Request, outcome: &Outcome) -> Result<(), Error> {
    let (repo_root, git_dir) = key::repository(&request.cwd)?;
    let root = store::store_dir(&request.cwd, &git_dir);
    let Some(store) = usable_store(&root, &repo_root, &git_dir) else {
        return Ok(());
    };
    store.trace_transaction(|tx| {
        tx.log_verification(
            outcome.would_skip,
            outcome.exit_code,
            outcome.false_green || outcome.reason == "quarantined",
        )
    })?;
    Ok(())
}

/// Returns the statistics of the store.
///
/// # Errors
///
/// Returns an error when the repository or the store cannot be read.
pub fn status(cwd: &Path) -> Result<Status, Error> {
    let (repo_root, git_dir) = key::repository(cwd)?;
    let root = store::store_dir(cwd, &git_dir);
    // A store in the work tree is off. The status says so and reads nothing:
    // the store is not opened, because opening it would create it.
    let Some(store) = usable_store(&root, &repo_root, &git_dir) else {
        return Ok(Status {
            store_root: root,
            entries: 0,
            invalid: 0,
            bytes: 0,
            counters: Counters::default(),
            cached_ms: 0,
            trace_states: 0,
            quarantines: 0,
            denial_reason: Some("store-in-work-tree".into()),
        });
    };
    let list = store.list_entries()?;
    let mut counters = store.read_counters()?;
    // A measured count above the hit count cannot be true. The document does
    // not say how many hits were measured, so the measurement is unknown:
    // kioku does not invent a smaller number.
    if counters
        .measured_hits
        .is_some_and(|measured| measured > counters.hits)
    {
        counters.measured_hits = None;
    }
    let cached_ms = list.entries.iter().fold(0_u64, |total, entry| {
        total.saturating_add(entry.duration_ms)
    });
    let (trace_states, quarantines) = if store.root().join("trace-v2").exists() {
        store.trace_transaction(super::store::TraceTransaction::counts)?
    } else {
        (0, 0)
    };
    Ok(Status {
        trace_states,
        quarantines,
        store_root: store.root().to_path_buf(),
        entries: list.entries.len(),
        invalid: list.invalid,
        bytes: list.bytes,
        counters,
        cached_ms,
        denial_reason: None,
    })
}

/// Removes old entries from the store.
///
/// # Errors
///
/// Returns an error when the repository or the store cannot be read.
pub fn clean(cwd: &Path, older_than: Option<std::time::Duration>) -> Result<OutcomeClean, Error> {
    let (repo_root, git_dir) = key::repository(cwd)?;
    let root = store::store_dir(cwd, &git_dir);
    // A store in the work tree is off. Nothing is removed there, and the
    // store is not opened: opening it would create it.
    let Some(store) = usable_store(&root, &repo_root, &git_dir) else {
        return Ok(OutcomeClean {
            denial_reason: Some("store-in-work-tree".into()),
            ..OutcomeClean::default()
        });
    };
    let report = store.clean(older_than)?;
    Ok(OutcomeClean {
        removed: report.removed,
        kept: report.kept,
        bytes_freed: report.bytes_freed,
        denial_reason: None,
    })
}

fn execute(argv: &[String], cwd: &Path) -> Result<ExitStatus, Error> {
    let (program, arguments) = argv.split_first().ok_or(Error::EmptyCommand)?;
    let mut command = Command::new(program);
    command
        .args(arguments)
        .current_dir(cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    command.status().map_err(|source| Error::Launch {
        program: program.clone(),
        source,
    })
}

/// Exit code of the child process, keeping the signal information where the
/// platform allows it.
#[must_use]
pub fn exit_code_of(status: &ExitStatus) -> i32 {
    match status.code() {
        Some(code) => code,
        None => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                status.signal().map_or(1, |signal| 128 + signal)
            }
            #[cfg(not(unix))]
            {
                1
            }
        }
    }
}

#[cfg(test)]
mod sampling_tests {
    use super::sample_from_u32;
    #[cfg(unix)]
    #[test]
    fn quarantine_between_prepare_and_hit_blocks_authorization() {
        use super::{PreparedTrace, authorize_hit};
        use crate::policy::{Identity, Manifest, ObservedPath, PathKind, observe};
        use crate::store::Store;
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input");
        std::fs::write(&input, b"data").unwrap();
        let manifest = Manifest::capture(&[ObservedPath {
            path: input,
            kind: PathKind::File,
        }])
        .unwrap();
        let identity = Identity {
            repository: "repo".into(),
            cwd: "cwd".into(),
            argv_digest: [1; 32],
            environment_digest: [2; 32],
            tracer_digest: [3; 32],
            version: 2,
        };
        let first = observe(&identity, None, None, &manifest, true, true).unwrap();
        let state = observe(
            &identity,
            Some(&first),
            Some(&manifest),
            &manifest,
            true,
            true,
        )
        .unwrap();
        let id = "c".repeat(64);
        let store = Store::open(dir.path().join("store"));
        store
            .trace_transaction(|tx| tx.set_state(&id, &state))
            .unwrap();
        let prepared = PreparedTrace {
            identity,
            id: id.clone(),
            previous: Some(state),
            before: Some(manifest),
            would_skip: true,
            quarantined: false,
            manifest_digest: None,
            timing: None,
        };
        assert!(authorize_hit(&store, &prepared));
        let other = Store::open(dir.path().join("store"));
        std::thread::spawn(move || other.trace_transaction(|tx| tx.quarantine(&id)).unwrap())
            .join()
            .unwrap();
        assert!(!authorize_hit(&store, &prepared));
    }

    #[cfg(unix)]
    #[test]
    fn changed_dependency_after_prepare_blocks_authorization() {
        use super::{PreparedTrace, authorize_hit};
        use crate::policy::{Identity, Manifest, ObservedPath, PathKind, observe};
        use crate::store::Store;
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input");
        std::fs::write(&input, b"data").unwrap();
        let manifest = Manifest::capture(&[ObservedPath {
            path: input.clone(),
            kind: PathKind::File,
        }])
        .unwrap();
        let identity = Identity {
            repository: "repo".into(),
            cwd: "cwd".into(),
            argv_digest: [1; 32],
            environment_digest: [2; 32],
            tracer_digest: [3; 32],
            version: 2,
        };
        let first = observe(&identity, None, None, &manifest, true, true).unwrap();
        let state = observe(
            &identity,
            Some(&first),
            Some(&manifest),
            &manifest,
            true,
            true,
        )
        .unwrap();
        let id = "e".repeat(64);
        let store = Store::open(dir.path().join("store"));
        store
            .trace_transaction(|tx| tx.set_state(&id, &state))
            .unwrap();
        let prepared = PreparedTrace {
            identity,
            id,
            previous: Some(state),
            before: Some(manifest),
            would_skip: true,
            quarantined: false,
            manifest_digest: None,
            timing: None,
        };
        assert!(authorize_hit(&store, &prepared));
        // The recorded dependency changed after the trace was prepared. The
        // observation no longer supports the reuse, so the hit is refused.
        std::fs::write(&input, b"changed").unwrap();
        assert!(!authorize_hit(&store, &prepared));
    }

    #[cfg(unix)]
    #[test]
    fn ineligible_state_never_authorizes_a_hit() {
        use super::{PreparedTrace, authorize_hit};
        use crate::policy::{Identity, Manifest, ObservedPath, PathKind, observe};
        use crate::store::Store;
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input");
        std::fs::write(&input, b"data").unwrap();
        let manifest = Manifest::capture(&[ObservedPath {
            path: input.clone(),
            kind: PathKind::File,
        }])
        .unwrap();
        let identity = Identity {
            repository: "repo".into(),
            cwd: "cwd".into(),
            argv_digest: [1; 32],
            environment_digest: [2; 32],
            tracer_digest: [3; 32],
            version: 2,
        };
        // A state learned without a stable observation is not eligible for
        // reuse: the policy refuses it and the hit must be refused too.
        let ineligible = observe(&identity, None, None, &manifest, true, true).unwrap();
        let id = "f".repeat(64);
        let store = Store::open(dir.path().join("store"));
        store
            .trace_transaction(|tx| tx.set_state(&id, &ineligible))
            .unwrap();
        let prepared = PreparedTrace {
            identity,
            id,
            previous: Some(ineligible),
            before: Some(manifest),
            would_skip: false,
            quarantined: false,
            manifest_digest: None,
            timing: None,
        };
        assert!(!authorize_hit(&store, &prepared));
    }

    #[test]
    fn deterministic_sampling_boundaries() {
        assert!(!sample_from_u32(0.0, 0));
        assert!(sample_from_u32(1.0, u32::MAX));
        assert!(sample_from_u32(0.5, (1 << 31) - 1));
        assert!(!sample_from_u32(0.5, 1 << 31));
    }
}
