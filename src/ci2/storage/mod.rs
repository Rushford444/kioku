//! Local admission journal. This is not yet a complete `LocalStoragePort`.
//! The exclusive owner retains the lock for its entire lifetime.
#[cfg(unix)]
mod unix {
    use crate::ci2::domain::{
        BlobRef, ClientIdentity, ContractError, Digest, Id, Lease, LocalAccepted, LocalRequest,
        LocalStoragePort, NativeObservation, OriginClaim, ProcessIdentity, QueueItem, Role,
        RunState, ServiceBootstrap, Validate, digest_request_document,
    };
    use crate::ci2::native::{AbsenceScope, NativeAbsenceProof, NativeAbsenceReceipt};
    use fs4::fs_std::FileExt;
    use serde::{Deserialize, Serialize};
    use std::{
        collections::HashMap,
        fs::{self, File, OpenOptions},
        io::{self, BufRead, BufReader, Read, Write},
        os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
        path::Path,
    };

    const MAX_EVENT: usize = 4 * 1024 * 1024;

    #[derive(Debug, thiserror::Error)]
    pub enum JournalError {
        #[error("I/O error: {0}")]
        Io(#[from] io::Error),
        #[error("invalid contract: {0}")]
        Contract(#[from] ContractError),
        #[error("invalid journal: {0}")]
        Corrupt(&'static str),
        #[error("state root is not a private directory")]
        RootPermissions,
        #[error("resource domain already owned")]
        Locked,
        #[error("request is not authorized")]
        Unauthorized,
        #[error("request identifier was already used for different content")]
        Conflict,
        #[error("durable run or evidence not found")]
        NotFound,
        #[error("state capacity exhausted")]
        Capacity,
        #[error("random identifier unavailable")]
        Random,
        #[error("journal is unavailable after an uncertain write")]
        Poisoned,
        #[error("unsupported storage operation: {kind}")]
        Unsupported { kind: &'static str },
    }

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Admission {
        schema: String,
        revision: u16,
        principal_id: Id,
        request: LocalRequest,
        accepted: LocalAccepted,
    }
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StateEvent {
        schema: String,
        revision: u16,
        run_id: Id,
        state: RunState,
        cursor: u64,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct TaskOutcome {
        pub task_id: Id,
        pub observation: NativeObservation,
    }
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct EvidenceEvent {
        schema: String,
        revision: u16,
        run_id: Id,
        cursor: u64,
        sequence: u64,
        outcome: TaskOutcome,
    }
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct FinalEvent {
        schema: String,
        revision: u16,
        run_id: Id,
        cursor: u64,
        state: RunState,
        report: BlobRef,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct InterruptionEvent {
        schema: String,
        revision: u16,
        run_id: Id,
        cursor: u64,
        interrupted_from: RunState,
        report: BlobRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        terminal: Option<RunState>,
    }

    /// A queued run refused by a pre-execution check; it never entered a native phase.
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RefusalEvent {
        schema: String,
        revision: u16,
        run_id: Id,
        cursor: u64,
        report: BlobRef,
    }

    /// Process group identity supplied by the trusted native launcher.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct ProcessEvidence {
        pub process: ProcessIdentity,
        pub start_ticks: u64,
        pub boot_id: String,
    }

    /// Durable association of a selected task with its native group.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct OrphanRecord {
        pub task_id: Id,
        pub evidence: ProcessEvidence,
        pub orphaned: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub lease: Option<Lease>,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct OrphanEvent {
        schema: String,
        revision: u16,
        run_id: Id,
        cursor: u64,
        record: OrphanRecord,
    }

    /// Input compatibility does not upgrade a bare identity into a native absence proof.
    pub enum ReconciliationEvidence<'a> {
        Unconfirmed(&'a ProcessEvidence),
        Confirmed(&'a NativeAbsenceProof),
    }
    impl<'a> From<&'a ProcessEvidence> for ReconciliationEvidence<'a> {
        fn from(evidence: &'a ProcessEvidence) -> Self {
            Self::Unconfirmed(evidence)
        }
    }
    impl<'a> From<&'a NativeAbsenceProof> for ReconciliationEvidence<'a> {
        fn from(proof: &'a NativeAbsenceProof) -> Self {
            Self::Confirmed(proof)
        }
    }

    /// A durable launch boundary; lack of a matching native binding is unresolved.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct NativeTaskIntent {
        pub schema: String,
        pub revision: u16,
        pub run_id: Id,
        pub task_id: Id,
        pub cursor: u64,
        pub lease: Lease,
    }

    /// Durable closure of one exact task binding; historical interruption is retained.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct ReconciliationRecord {
        pub schema: String,
        pub revision: u16,
        pub kind: String,
        pub run_id: Id,
        pub task_id: Id,
        pub cursor: u64,
        pub binding_cursor: u64,
        pub service_id: Id,
        pub resource_domain_id: Id,
        pub lease: Lease,
        pub proof: NativeAbsenceReceipt,
    }

    /// Durable admission and phase, without an invented origin or report.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct RunRecord {
        pub principal_id: Id,
        pub request: LocalRequest,
        pub accepted: LocalAccepted,
        pub state: RunState,
        pub accepted_cursor: u64,
        pub event_cursor: u64,
        pub observations: Vec<TaskOutcome>,
        pub report: Option<BlobRef>,
        /// Lost supervision is terminal for the run, but does not release native resources.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub interrupted_from: Option<RunState>,
    }
    type Key = (Id, Id, Id);
    type TaskKey = (Id, Id);

    /// A single writer. Open only from an authenticated, operator-controlled bootstrap.
    pub struct LocalJournal {
        _lock: Option<File>,
        journal: Option<File>,
        config: ServiceBootstrap,
        records: HashMap<Key, LocalAccepted>,
        runs: HashMap<Id, RunRecord>,
        orphans: HashMap<TaskKey, OrphanRecord>,
        binding_cursors: HashMap<TaskKey, u64>,
        intents: HashMap<TaskKey, NativeTaskIntent>,
        reconciliations: HashMap<TaskKey, ReconciliationRecord>,
        cursor: u64,
        bytes: u64,
        poisoned: bool,
    }

    fn key(principal: &Id, request: &LocalRequest) -> Key {
        (
            principal.clone(),
            request.project_id.clone(),
            request.request_id.clone(),
        )
    }
    fn digest(request: &LocalRequest) -> Result<Digest, JournalError> {
        let value = serde_json::to_value(request)
            .map_err(|_| JournalError::Corrupt("cannot encode request"))?;
        Ok(digest_request_document(&value)?)
    }
    fn check_record(record: &Admission) -> Result<(), JournalError> {
        if record.schema != "kioku/ci2/admission-event/v1" || record.revision != 1 {
            return Err(JournalError::Corrupt("unknown event version"));
        }
        record.request.validate()?;
        record.accepted.validate()?;
        if record.accepted.request_id != record.request.request_id
            || record.accepted.request_digest != digest(&record.request)?
            || record.accepted.state != RunState::Queued
        {
            return Err(JournalError::Corrupt("event fields disagree"));
        }
        Ok(())
    }
    fn valid_process_evidence(e: &ProcessEvidence) -> bool {
        e.process.pid > 1
            && e.process.pid == e.process.pgid
            && e.start_ticks > 0
            && e.process.started_ms > 0
            && e.boot_id.len() == 36
            && e.boot_id
                .bytes()
                .all(|b| b.is_ascii_hexdigit() || b == b'-')
    }

    #[cfg(target_os = "linux")]
    fn process_matches(e: &ProcessEvidence, retained_leader: bool) -> bool {
        if !valid_process_evidence(e) {
            return false;
        }
        let Ok(boot) = fs::read_to_string("/proc/sys/kernel/random/boot_id") else {
            return false;
        };
        if boot.trim() != e.boot_id {
            return false;
        }
        let Ok(stat) = fs::read_to_string(format!("/proc/{}/stat", e.process.pid)) else {
            return false;
        };
        let Some((_, fields)) = stat.rsplit_once(") ") else {
            return false;
        };
        let fields: Vec<_> = fields.split_ascii_whitespace().collect();
        fields.len() > 19
            && (retained_leader || fields[0] != "Z")
            && fields[0] != "X"
            && fields[2].parse::<u32>().ok() == Some(e.process.pgid)
            && fields[19].parse::<u64>().ok() == Some(e.start_ticks)
    }
    #[cfg(not(target_os = "linux"))]
    fn process_matches(_: &ProcessEvidence, _: bool) -> bool {
        false
    }

    fn valid_lease(lease: &Lease, run_id: &Id, evidence: &ProcessEvidence, service: &Id) -> bool {
        valid_lease_scope(lease, run_id, service)
            && lease
                .process_identity
                .as_ref()
                .is_none_or(|process| process == &evidence.process)
    }

    fn valid_lease_scope(lease: &Lease, run_id: &Id, service: &Id) -> bool {
        lease.lease_id.validate().is_ok() && lease.item_id == *run_id && lease.owner == *service
    }

    // A wall-clock label or another lease does not create a new native incarnation.
    fn incarnation_is_bound(
        orphans: &HashMap<TaskKey, OrphanRecord>,
        evidence: &ProcessEvidence,
    ) -> bool {
        orphans.values().any(|record| {
            record.evidence.boot_id == evidence.boot_id
                && record.evidence.process.pid == evidence.process.pid
                && record.evidence.start_ticks == evidence.start_ticks
        })
    }

    fn all_bindings_closed(
        orphans: &HashMap<TaskKey, OrphanRecord>,
        reconciliations: &HashMap<TaskKey, ReconciliationRecord>,
        intents: &HashMap<TaskKey, NativeTaskIntent>,
        run_id: &Id,
    ) -> bool {
        orphans
            .keys()
            .chain(intents.keys())
            .filter(|(id, _)| id == run_id)
            .all(|key| orphans.contains_key(key) && reconciliations.contains_key(key))
    }
    fn unresolved_orphan(
        orphans: &HashMap<TaskKey, OrphanRecord>,
        reconciliations: &HashMap<TaskKey, ReconciliationRecord>,
        run_id: &Id,
    ) -> bool {
        orphans.iter().any(|(key, record)| {
            key.0 == *run_id && record.orphaned && !reconciliations.contains_key(key)
        })
    }
    fn unmarked_binding(
        orphans: &HashMap<TaskKey, OrphanRecord>,
        reconciliations: &HashMap<TaskKey, ReconciliationRecord>,
        run_id: &Id,
    ) -> bool {
        orphans.iter().any(|(key, record)| {
            key.0 == *run_id && !record.orphaned && !reconciliations.contains_key(key)
        })
    }
    fn valid_reconciliation(
        event: &ReconciliationRecord,
        binding: &OrphanRecord,
        binding_cursor: u64,
        run: &RunRecord,
        config: &ServiceBootstrap,
    ) -> bool {
        event.schema == "kioku/ci2/reconciliation/v1"
            && event.revision == 1
            && event.kind == "task-native-closed"
            && event.binding_cursor == binding_cursor
            && event.task_id == binding.task_id
            && event.run_id == run.accepted.run_id
            && event.service_id == config.service_id
            && event.resource_domain_id == config.resource_domain_id
            && binding.lease.as_ref() == Some(&event.lease)
            && valid_lease(
                &event.lease,
                &event.run_id,
                &binding.evidence,
                &config.service_id,
            )
            && event.proof.evidence == binding.evidence
            && valid_process_evidence(&event.proof.evidence)
            && event.proof.scope == AbsenceScope::OriginalGroupAndAdoptedDescendants
            && event.proof.monitor_pid > 1
            && event.proof.monitor_pid != binding.evidence.process.pid
            && !event.proof.reaped_pids.contains(&event.proof.monitor_pid)
            && event.proof.source == "owned-direct-child-original-process-group"
            && event.proof.leader_reaped
            && event.proof.subreaper_echild
            && event.proof.original_group_empty
            && event
                .proof
                .reaped_pids
                .contains(&binding.evidence.process.pid)
            && event.proof.reaped_pids.iter().all(|pid| *pid > 1)
            && event
                .proof
                .reaped_pids
                .iter()
                .enumerate()
                .all(|(i, pid)| !event.proof.reaped_pids[..i].contains(pid))
            && (matches!(run.state, RunState::Running | RunState::Collecting)
                || (run.interrupted_from.is_some()
                    && matches!(run.state, RunState::Failed | RunState::Incomplete)))
    }

    fn valid_transition(from: RunState, to: RunState) -> bool {
        matches!(
            (from, to),
            (
                RunState::Queued,
                RunState::Capturing | RunState::Running | RunState::Cancelled
            ) | (RunState::Capturing, RunState::Running | RunState::Cancelled)
                | (RunState::Running, RunState::Collecting)
        )
    }
    fn can_observe(run: &RunRecord, sequence: u64, outcome: &TaskOutcome) -> bool {
        matches!(run.state, RunState::Running | RunState::Collecting)
            && run.request.requested_task_ids.contains(&outcome.task_id)
            && sequence == run.observations.len() as u64
            && !run
                .observations
                .iter()
                .any(|o| o.task_id == outcome.task_id)
            && outcome.observation.validate().is_ok()
    }
    fn valid_final(run: &RunRecord, state: RunState) -> bool {
        if run.state != RunState::Collecting {
            return false;
        }
        let all_good = !run.observations.is_empty()
            && run.observations.iter().all(|o| {
                let n = &o.observation;
                n.exit_code == Some(0)
                    && n.signal.is_none()
                    && !n.timed_out
                    && n.cpu_complete
                    && n.descendants_drained
            });
        let failed = run.observations.iter().any(|o| {
            let n = &o.observation;
            (n.timed_out && n.cpu_complete && n.descendants_drained)
                || (!n.timed_out && (n.signal.is_some() || n.exit_code.is_some_and(|v| v != 0)))
        });
        let ordered_ids: Vec<&Id> = run.observations.iter().map(|o| &o.task_id).collect();
        let selected_ids: Vec<&Id> = run.request.requested_task_ids.iter().collect();
        let exact_coverage = ordered_ids == selected_ids
            && selected_ids
                .iter()
                .all(|id| ordered_ids.iter().filter(|seen| *seen == id).count() == 1);
        match state {
            RunState::Passed => all_good && exact_coverage,
            RunState::Failed => failed && exact_coverage,
            // A healthy native child remains Incomplete when its reporter/inventory or broader contract is unqualified.
            // This downgrade cannot manufacture a Passed verdict; storage verifies exact native coverage for green.
            // The coordinator only requests this after a scoped STOP/cancel request.
            // Any already observed command failure retains priority over cancellation.
            RunState::Cancelled => run.observations.iter().all(|o| {
                let native = &o.observation;
                native.cpu_complete
                    && native.descendants_drained
                    && native.exit_code.is_none_or(|code| code == 0)
            }),
            RunState::Incomplete => true,
            _ => false,
        }
    }
    fn blob_path(root: &Path, digest: &Digest) -> std::path::PathBuf {
        root.join("blobs").join(format!("blob-{}", digest.as_hex()))
    }
    fn verify_blob(root: &Path, blob: &BlobRef) -> Result<(), JournalError> {
        use sha2::Digest as _;
        let path = blob_path(root, &blob.digest);
        let meta = fs::symlink_metadata(&path)?;
        if !meta.file_type().is_file() || meta.len() != blob.size_bytes {
            return Err(JournalError::Conflict);
        }
        let bytes = fs::read(path)?;
        let actual = format!("sha256:{:x}", sha2::Sha256::digest(&bytes));
        if blob.digest.to_string() != actual {
            return Err(JournalError::Conflict);
        }
        Ok(())
    }
    fn verify_streams(root: &Path, run_id: &Id, outcome: &TaskOutcome) -> Result<(), JournalError> {
        use sha2::Digest as _;
        let dir = root.join("results").join(run_id.as_str());
        for part in [root.join("results"), dir.clone()] {
            let meta = fs::symlink_metadata(part)?;
            if !meta.file_type().is_dir() || meta.mode() & 0o077 != 0 {
                return Err(JournalError::RootPermissions);
            }
        }
        for (channel, expected, size) in [
            (
                "stdout",
                &outcome.observation.stdout_digest,
                outcome.observation.stdout_bytes,
            ),
            (
                "stderr",
                &outcome.observation.stderr_digest,
                outcome.observation.stderr_bytes,
            ),
        ] {
            let path = dir.join(format!("{}.{}", outcome.task_id, channel));
            let meta = fs::symlink_metadata(&path)?;
            if !meta.file_type().is_file() || meta.mode() & 0o077 != 0 || meta.len() != size {
                return Err(JournalError::Conflict);
            }
            let mut file = File::open(path)?;
            let mut hash = sha2::Sha256::new();
            let mut buffer = [0u8; 8192];
            loop {
                let n = file.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                hash.update(&buffer[..n]);
            }
            file.sync_all()?;
            if expected.to_string() != format!("sha256:{:x}", hash.finalize()) {
                return Err(JournalError::Conflict);
            }
        }
        File::open(dir)?.sync_all()?;
        Ok(())
    }
    fn private_read_file(path: &Path) -> io::Result<File> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_file() || metadata.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "not a private regular journal",
            ));
        }
        OpenOptions::new().read(true).open(path)
    }
    fn private_file(path: &Path) -> io::Result<File> {
        // A private parent directory is required; reject pre-existing symlinks.
        if let Ok(metadata) = fs::symlink_metadata(path)
            && (!metadata.file_type().is_file() || metadata.mode() & 0o077 != 0)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "not a private regular file",
            ));
        }
        OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .mode(0o600)
            .open(path)
    }

    impl LocalJournal {
        /// Opens and validates the entire bounded journal before accepting any request.
        /// # Errors
        /// Insecure paths, a held writer lock, bad limits or corrupt durable events fail closed.
        pub fn open(config: ServiceBootstrap) -> Result<Self, JournalError> {
            Self::open_inner(config, false)
        }

        /// Replays durable events without taking the single-writer lock or allowing mutation.
        ///
        /// # Errors
        /// A partial, corrupted, inaccessible or over-budget event or artifact fails closed.
        pub fn open_read_only(config: ServiceBootstrap) -> Result<Self, JournalError> {
            Self::open_inner(config, true)
        }

        #[allow(clippy::too_many_lines)]
        fn open_inner(config: ServiceBootstrap, read_only: bool) -> Result<Self, JournalError> {
            config.validate()?;
            let root = Path::new(&config.state_root);
            if !root.exists() {
                fs::DirBuilder::new().mode(0o700).create(root)?;
                if let Some(parent) = root.parent() {
                    File::open(parent)?.sync_all()?;
                }
            }
            let meta = fs::symlink_metadata(root)?;
            if !meta.file_type().is_dir() || meta.mode() & 0o077 != 0 {
                return Err(JournalError::RootPermissions);
            }
            let lock = if read_only {
                None
            } else {
                let lock = private_file(&root.join(".owner.lock"))?;
                if !FileExt::try_lock_exclusive(&lock)? {
                    return Err(JournalError::Locked);
                }
                Some(lock)
            };
            let journal = if read_only {
                private_read_file(&root.join("admissions.jsonl"))?
            } else {
                private_file(&root.join("admissions.jsonl"))?
            };
            File::open(root)?.sync_all()?;
            let bytes = journal.metadata()?.len();
            if bytes > config.max_state_bytes {
                return Err(JournalError::Capacity);
            }
            let mut records = HashMap::new();
            let mut runs = HashMap::new();
            let mut orphans: HashMap<TaskKey, OrphanRecord> = HashMap::new();
            let mut binding_cursors = HashMap::new();
            let mut intents: HashMap<TaskKey, NativeTaskIntent> = HashMap::new();
            let mut reconciliations: HashMap<TaskKey, ReconciliationRecord> = HashMap::new();
            let mut cursor = 0_u64;
            let mut reader = BufReader::new(journal.try_clone()?);
            loop {
                let mut line = Vec::new();
                let n = (&mut reader)
                    .take((MAX_EVENT + 1) as u64)
                    .read_until(b'\n', &mut line)?;
                if n == 0 {
                    break;
                }
                if n > MAX_EVENT || line.last() != Some(&b'\n') {
                    return Err(JournalError::Corrupt("oversized or truncated event"));
                }
                if let Ok(record) = serde_json::from_slice::<Admission>(&line) {
                    check_record(&record)?;
                    cursor = cursor
                        .checked_add(1)
                        .ok_or(JournalError::Corrupt("cursor overflow"))?;
                    let run = RunRecord {
                        principal_id: record.principal_id.clone(),
                        request: record.request.clone(),
                        accepted: record.accepted.clone(),
                        state: RunState::Queued,
                        accepted_cursor: cursor,
                        event_cursor: cursor,
                        observations: Vec::new(),
                        report: None,
                        interrupted_from: None,
                    };
                    if records
                        .insert(
                            key(&record.principal_id, &record.request),
                            record.accepted.clone(),
                        )
                        .is_some()
                        || runs.insert(record.accepted.run_id, run).is_some()
                    {
                        return Err(JournalError::Corrupt("duplicate admission"));
                    }
                } else if let Ok(event) = serde_json::from_slice::<NativeTaskIntent>(&line) {
                    cursor = cursor
                        .checked_add(1)
                        .ok_or(JournalError::Corrupt("cursor overflow"))?;
                    let task_key = (event.run_id.clone(), event.task_id.clone());
                    let run: &mut RunRecord = runs
                        .get_mut(&event.run_id)
                        .ok_or(JournalError::Corrupt("unknown run"))?;
                    if event.schema != "kioku/ci2/native-task-intent/v1"
                        || event.revision != 1
                        || event.cursor != cursor
                        || run.state != RunState::Running
                        || !run.request.requested_task_ids.contains(&event.task_id)
                        || intents.contains_key(&task_key)
                        || orphans.contains_key(&task_key)
                        || !valid_lease_scope(&event.lease, &event.run_id, &config.service_id)
                        || !all_bindings_closed(&orphans, &reconciliations, &intents, &event.run_id)
                    {
                        return Err(JournalError::Corrupt("invalid native task intent"));
                    }
                    run.event_cursor = cursor;
                    intents.insert(task_key, event);
                } else if let Ok(event) = serde_json::from_slice::<OrphanEvent>(&line) {
                    cursor = cursor
                        .checked_add(1)
                        .ok_or(JournalError::Corrupt("cursor overflow"))?;
                    let run: &mut RunRecord = runs
                        .get_mut(&event.run_id)
                        .ok_or(JournalError::Corrupt("unknown run"))?;
                    let task_key = (event.run_id.clone(), event.record.task_id.clone());
                    let valid = match orphans.get(&task_key) {
                        None => {
                            !event.record.orphaned
                                && !incarnation_is_bound(&orphans, &event.record.evidence)
                                && intents.keys().chain(orphans.keys()).all(|key| {
                                    key.0 != event.run_id
                                        || key.1 == event.record.task_id
                                        || reconciliations.contains_key(key)
                                })
                        }
                        Some(old) => {
                            !old.orphaned
                                && event.record.orphaned
                                && old.evidence == event.record.evidence
                                && old.lease == event.record.lease
                        }
                    };
                    if event.schema != "kioku/ci2/orphan-event/v1"
                        || event.revision != 1
                        || event.cursor != cursor
                        || !matches!(run.state, RunState::Running | RunState::Collecting)
                        || !run
                            .request
                            .requested_task_ids
                            .contains(&event.record.task_id)
                        || !valid
                        || !valid_process_evidence(&event.record.evidence)
                        || event.record.lease.as_ref().is_some_and(|lease| {
                            intents
                                .get(&task_key)
                                .is_none_or(|intent| intent.lease != *lease)
                                || !valid_lease(
                                    lease,
                                    &event.run_id,
                                    &event.record.evidence,
                                    &config.service_id,
                                )
                        })
                    {
                        return Err(JournalError::Corrupt("invalid orphan event"));
                    }
                    if !event.record.orphaned {
                        binding_cursors.insert(task_key.clone(), cursor);
                    }
                    run.event_cursor = cursor;
                    orphans.insert(task_key, event.record);
                } else if let Ok(event) = serde_json::from_slice::<ReconciliationRecord>(&line) {
                    cursor = cursor
                        .checked_add(1)
                        .ok_or(JournalError::Corrupt("cursor overflow"))?;
                    let task_key = (event.run_id.clone(), event.task_id.clone());
                    let run = runs
                        .get_mut(&event.run_id)
                        .ok_or(JournalError::Corrupt("unknown run"))?;
                    let binding = orphans
                        .get(&task_key)
                        .ok_or(JournalError::Corrupt("missing native binding"))?;
                    let binding_cursor = *binding_cursors
                        .get(&task_key)
                        .ok_or(JournalError::Corrupt("missing binding cursor"))?;
                    if event.cursor != cursor
                        || reconciliations.contains_key(&task_key)
                        || !valid_reconciliation(&event, binding, binding_cursor, run, &config)
                    {
                        return Err(JournalError::Corrupt("invalid native reconciliation"));
                    }
                    run.event_cursor = cursor;
                    reconciliations.insert(task_key, event);
                } else if let Ok(event) = serde_json::from_slice::<EvidenceEvent>(&line) {
                    cursor = cursor
                        .checked_add(1)
                        .ok_or(JournalError::Corrupt("cursor overflow"))?;
                    let run = runs
                        .get_mut(&event.run_id)
                        .ok_or(JournalError::Corrupt("unknown run"))?;
                    if event.schema != "kioku/ci2/observation-event/v1"
                        || event.revision != 1
                        || event.cursor != cursor
                        || !can_observe(run, event.sequence, &event.outcome)
                        || unresolved_orphan(&orphans, &reconciliations, &event.run_id)
                    {
                        return Err(JournalError::Corrupt("invalid observation"));
                    }
                    verify_streams(root, &event.run_id, &event.outcome)?;
                    run.observations.push(event.outcome);
                    run.event_cursor = cursor;
                } else if let Ok(event) = serde_json::from_slice::<RefusalEvent>(&line) {
                    cursor = cursor
                        .checked_add(1)
                        .ok_or(JournalError::Corrupt("cursor overflow"))?;
                    let run = runs
                        .get_mut(&event.run_id)
                        .ok_or(JournalError::Corrupt("unknown run"))?;
                    if event.schema != "kioku/ci2/refusal-event/v1"
                        || event.revision != 1
                        || event.cursor != cursor
                        || run.state != RunState::Queued
                        || run.report.is_some()
                        || event.report.validate().is_err()
                        || unresolved_orphan(&orphans, &reconciliations, &event.run_id)
                    {
                        return Err(JournalError::Corrupt("invalid pre-execution refusal"));
                    }
                    verify_blob(root, &event.report)?;
                    run.state = RunState::Incomplete;
                    run.report = Some(event.report);
                    run.event_cursor = cursor;
                } else if let Ok(event) = serde_json::from_slice::<InterruptionEvent>(&line) {
                    cursor = cursor
                        .checked_add(1)
                        .ok_or(JournalError::Corrupt("cursor overflow"))?;
                    let run = runs
                        .get_mut(&event.run_id)
                        .ok_or(JournalError::Corrupt("unknown run"))?;
                    if event.schema != "kioku/ci2/interruption-event/v1"
                        || event.revision != 1
                        || event.cursor != cursor
                        || run.state != event.interrupted_from
                        || !matches!(run.state, RunState::Running | RunState::Collecting)
                        || event.report.validate().is_err()
                        || !matches!(
                            event.terminal,
                            None | Some(RunState::Incomplete | RunState::Failed)
                        )
                        || unmarked_binding(&orphans, &reconciliations, &event.run_id)
                    {
                        return Err(JournalError::Corrupt("invalid interruption"));
                    }
                    verify_blob(root, &event.report)?;
                    run.interrupted_from = Some(event.interrupted_from);
                    run.state = event.terminal.unwrap_or(RunState::Incomplete);
                    run.report = Some(event.report);
                    run.event_cursor = cursor;
                } else if let Ok(event) = serde_json::from_slice::<FinalEvent>(&line) {
                    cursor = cursor
                        .checked_add(1)
                        .ok_or(JournalError::Corrupt("cursor overflow"))?;
                    let run = runs
                        .get_mut(&event.run_id)
                        .ok_or(JournalError::Corrupt("unknown run"))?;
                    if event.schema != "kioku/ci2/final-event/v1"
                        || event.revision != 1
                        || event.cursor != cursor
                        || event.report.validate().is_err()
                        || !valid_final(run, event.state)
                        || (intents.keys().any(|(id, _)| *id == event.run_id)
                            && !all_bindings_closed(
                                &orphans,
                                &reconciliations,
                                &intents,
                                &event.run_id,
                            ))
                        || unresolved_orphan(&orphans, &reconciliations, &event.run_id)
                    {
                        return Err(JournalError::Corrupt("invalid finalization"));
                    }
                    verify_blob(root, &event.report)?;
                    run.state = event.state;
                    run.report = Some(event.report);
                    run.event_cursor = cursor;
                } else {
                    let event: StateEvent = serde_json::from_slice(&line)
                        .map_err(|_| JournalError::Corrupt("invalid event"))?;
                    cursor = cursor
                        .checked_add(1)
                        .ok_or(JournalError::Corrupt("cursor overflow"))?;
                    let run: &mut RunRecord = runs
                        .get_mut(&event.run_id)
                        .ok_or(JournalError::Corrupt("unknown run"))?;
                    if event.schema != "kioku/ci2/state-event/v1"
                        || event.revision != 1
                        || event.cursor != cursor
                        || !valid_transition(run.state, event.state)
                        || unresolved_orphan(&orphans, &reconciliations, &event.run_id)
                    {
                        return Err(JournalError::Corrupt("invalid state transition"));
                    }
                    run.state = event.state;
                    run.event_cursor = cursor;
                }
            }
            Ok(Self {
                _lock: lock,
                // Keep the read-only descriptor: kernel write refusal and self-poisoning are part of v1.
                journal: Some(journal),
                config,
                records,
                runs,
                orphans,
                binding_cursors,
                intents,
                reconciliations,
                cursor,
                bytes,
                poisoned: false,
            })
        }

        /// Loads the persisted admission and current durable phase by run id.
        /// # Errors
        /// An unknown run or a poisoned writer has no readable result.
        pub fn load_run(&self, run_id: &Id) -> Result<RunRecord, JournalError> {
            self.borrow_run(run_id).cloned()
        }

        /// Returns the current durable phase without cloning the admission or evidence.
        /// # Errors
        /// An unknown run or a poisoned writer has no readable state.
        pub fn run_state(&self, run_id: &Id) -> Result<RunState, JournalError> {
            Ok(self.borrow_run(run_id)?.state)
        }

        fn borrow_run(&self, run_id: &Id) -> Result<&RunRecord, JournalError> {
            if self.poisoned {
                return Err(JournalError::Poisoned);
            }
            self.runs.get(run_id).ok_or(JournalError::NotFound)
        }

        /// Returns the earliest durable admission for a request id across all scopes.
        /// # Errors
        /// A poisoned journal cannot establish request identity.
        #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
        #[cfg(test)]
        pub(crate) fn load_request_for_id(
            &self,
            request_id: &Id,
        ) -> Result<Option<RunRecord>, JournalError> {
            if self.poisoned {
                return Err(JournalError::Poisoned);
            }
            Ok(self
                .runs
                .values()
                .filter(|run| run.request.request_id == *request_id)
                .min_by_key(|run| run.accepted_cursor)
                .cloned())
        }

        /// Returns at most 256 runs, sorted in admission order.
        /// # Errors
        /// A poisoned writer cannot enumerate its runs.
        pub fn all_runs(&self) -> Result<Vec<RunRecord>, JournalError> {
            if self.poisoned {
                return Err(JournalError::Poisoned);
            }
            // Admission cursors are unique, checked increments on both write and replay.
            // Keep the oldest display records borrowed until the selection is complete.
            let mut earliest = std::collections::BTreeMap::new();
            for run in self.runs.values() {
                if earliest.len() == 256 {
                    if earliest
                        .last_key_value()
                        .is_some_and(|(cursor, _)| run.accepted_cursor >= *cursor)
                    {
                        continue;
                    }
                    earliest.pop_last();
                }
                earliest.insert(run.accepted_cursor, run);
            }
            Ok(earliest.into_values().cloned().collect())
        }

        /// Internal recovery and idempotence must not ignore runs beyond the display limit.
        #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
        #[cfg(test)]
        pub(crate) fn durable_runs(&self) -> Result<Vec<RunRecord>, JournalError> {
            if self.poisoned {
                return Err(JournalError::Poisoned);
            }
            let mut runs: Vec<_> = self.runs.values().cloned().collect();
            runs.sort_by_key(|run| run.accepted_cursor);
            Ok(runs)
        }

        /// Clones only interrupted runs, preserving durable admission order.
        #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
        #[cfg(test)]
        pub(crate) fn interrupted_runs(&self) -> Result<Vec<RunRecord>, JournalError> {
            if self.poisoned {
                return Err(JournalError::Poisoned);
            }
            let mut runs: Vec<_> = self
                .runs
                .values()
                .filter(|run| run.interrupted_from.is_some())
                .cloned()
                .collect();
            runs.sort_by_key(|run| run.accepted_cursor);
            Ok(runs)
        }

        /// Whether native ownership remains unresolved, including legacy runs with no binding.
        /// # Errors
        /// A poisoned journal cannot establish resource availability.
        pub fn has_unresolved_execution(&self) -> Result<bool, JournalError> {
            if self.poisoned {
                return Err(JournalError::Poisoned);
            }
            for (run_id, run) in &self.runs {
                if (run.interrupted_from.is_some()
                    || (!matches!(
                        run.state,
                        RunState::Queued
                            | RunState::Capturing
                            | RunState::Running
                            | RunState::Collecting
                    ) && self.intents.keys().any(|(id, _)| id == run_id))
                    || self
                        .orphans
                        .iter()
                        .any(|(key, record)| key.0 == *run_id && record.orphaned))
                    && self.run_has_unresolved_execution(run_id)?
                {
                    return Ok(true);
                }
            }
            Ok(false)
        }

        /// Check one run's native ownership, including a launch intent with no binding.
        /// This does not infer closure from an empty binding list or from PID disappearance.
        /// # Errors
        /// Unknown runs and poisoned journals cannot establish resource availability.
        pub fn run_has_unresolved_execution(&self, run_id: &Id) -> Result<bool, JournalError> {
            let run = self.borrow_run(run_id)?;
            let has_native = self
                .orphans
                .keys()
                .chain(self.intents.keys())
                .any(|(id, _)| id == run_id);
            if has_native {
                Ok(!all_bindings_closed(
                    &self.orphans,
                    &self.reconciliations,
                    &self.intents,
                    run_id,
                ))
            } else {
                Ok(run.interrupted_from.is_some()
                    || matches!(run.state, RunState::Running | RunState::Collecting))
            }
        }

        /// Whether every native launch intent and task binding has a durable closure.
        /// No binding is unknown, not proof of absence.
        /// # Errors
        /// Unknown runs and poisoned journals cannot establish closure.
        pub fn native_bindings_closed(&self, run_id: &Id) -> Result<bool, JournalError> {
            self.borrow_run(run_id)?;
            Ok(self
                .orphans
                .keys()
                .chain(self.intents.keys())
                .any(|(id, _)| id == run_id)
                && all_bindings_closed(&self.orphans, &self.reconciliations, &self.intents, run_id))
        }

        /// Persist the selected task's lease before spawning its monitor or child.
        /// This launch boundary never authorizes a duplicate execution on retry.
        /// # Errors
        /// Foreign leases, repeated tasks, unresolved earlier launches and uncertain writes fail closed.
        pub fn begin_native_task(
            &mut self,
            run_id: &Id,
            task_id: &Id,
            lease: &Lease,
        ) -> Result<u64, JournalError> {
            let run = self.load_run(run_id)?;
            let task_key = (run_id.clone(), task_id.clone());
            if run.state != RunState::Running
                || !run.request.requested_task_ids.contains(task_id)
                || !valid_lease_scope(lease, run_id, &self.config.service_id)
                || self.intents.contains_key(&task_key)
                || self.orphans.contains_key(&task_key)
                || !all_bindings_closed(&self.orphans, &self.reconciliations, &self.intents, run_id)
            {
                return Err(JournalError::Conflict);
            }
            let cursor = self.cursor.checked_add(1).ok_or(JournalError::Capacity)?;
            let event = NativeTaskIntent {
                schema: "kioku/ci2/native-task-intent/v1".into(),
                revision: 1,
                run_id: run_id.clone(),
                task_id: task_id.clone(),
                cursor,
                lease: lease.clone(),
            };
            self.append_event(&event, cursor)?;
            self.intents.insert(task_key, event);
            self.runs
                .get_mut(run_id)
                .ok_or(JournalError::Poisoned)?
                .event_cursor = cursor;
            Ok(cursor)
        }

        /// Read launch boundaries in durable order, including tasks with no native header.
        /// # Errors
        /// Unknown runs and poisoned journals are rejected.
        pub fn native_task_intents(
            &self,
            run_id: &Id,
        ) -> Result<Vec<NativeTaskIntent>, JournalError> {
            self.borrow_run(run_id)?;
            let mut intents: Vec<_> = self
                .intents
                .values()
                .filter(|intent| intent.run_id == *run_id)
                .cloned()
                .collect();
            intents.sort_by_key(|intent| intent.cursor);
            Ok(intents)
        }

        /// Record lost supervision without inventing observations or releasing a process association.
        /// # Errors
        /// Only active phases with an orphaned (or absent legacy) binding can become Incomplete.
        pub fn interrupt_run(&mut self, run_id: &Id, report: BlobRef) -> Result<u64, JournalError> {
            self.interrupt_run_inner(run_id, report, None)
        }

        /// Record an interruption while preserving an independently established native failure.
        /// The caller must establish failure separately from uncertain cleanup; this cannot record success.
        /// # Errors
        /// Only Failed or Incomplete and an active, unresolved phase are accepted.
        pub fn interrupt_run_with_result(
            &mut self,
            run_id: &Id,
            report: BlobRef,
            terminal: RunState,
        ) -> Result<u64, JournalError> {
            if !matches!(terminal, RunState::Failed | RunState::Incomplete) {
                return Err(JournalError::Conflict);
            }
            self.interrupt_run_inner(run_id, report, Some(terminal))
        }

        fn interrupt_run_inner(
            &mut self,
            run_id: &Id,
            report: BlobRef,
            terminal: Option<RunState>,
        ) -> Result<u64, JournalError> {
            let run = self.load_run(run_id)?;
            if !matches!(run.state, RunState::Running | RunState::Collecting)
                || unmarked_binding(&self.orphans, &self.reconciliations, run_id)
            {
                return Err(JournalError::Conflict);
            }
            report.validate()?;
            verify_blob(Path::new(&self.config.state_root), &report)?;
            let cursor = self.cursor.checked_add(1).ok_or(JournalError::Capacity)?;
            let event = InterruptionEvent {
                schema: "kioku/ci2/interruption-event/v1".into(),
                revision: 1,
                run_id: run_id.clone(),
                cursor,
                interrupted_from: run.state,
                report,
                terminal,
            };
            self.append_event(&event, cursor)?;
            let run = self.runs.get_mut(run_id).ok_or(JournalError::Poisoned)?;
            run.interrupted_from = Some(event.interrupted_from);
            run.state = event.terminal.unwrap_or(RunState::Incomplete);
            run.report = Some(event.report);
            run.event_cursor = cursor;
            Ok(cursor)
        }

        fn advance(&mut self, run_id: &Id, phase: RunState) -> Result<u64, JournalError> {
            if self.poisoned {
                return Err(JournalError::Poisoned);
            }
            let run = self.runs.get(run_id).ok_or(JournalError::Conflict)?;
            if !valid_transition(run.state, phase)
                || unresolved_orphan(&self.orphans, &self.reconciliations, run_id)
                || (matches!(phase, RunState::Capturing | RunState::Running)
                    && self.has_unresolved_execution()?)
            {
                return Err(JournalError::Conflict);
            }
            let cursor = self.cursor.checked_add(1).ok_or(JournalError::Capacity)?;
            let event = StateEvent {
                schema: "kioku/ci2/state-event/v1".into(),
                revision: 1,
                run_id: run_id.clone(),
                state: phase,
                cursor,
            };
            let mut line = serde_json::to_vec(&event)
                .map_err(|_| JournalError::Corrupt("cannot encode state"))?;
            line.push(b'\n');
            let added = u64::try_from(line.len()).map_err(|_| JournalError::Capacity)?;
            if line.len() > MAX_EVENT
                || self
                    .bytes
                    .checked_add(added)
                    .is_none_or(|n| n > self.config.max_state_bytes)
            {
                return Err(JournalError::Capacity);
            }
            self.poisoned = true;
            let Some(journal) = self.journal.as_mut() else {
                self.poisoned = false;
                return Err(JournalError::Locked);
            };
            journal.write_all(&line)?;
            journal.sync_all()?;
            self.bytes += added;
            self.cursor = cursor;
            let run = self.runs.get_mut(run_id).ok_or(JournalError::Poisoned)?;
            run.state = phase;
            run.event_cursor = cursor;
            self.poisoned = false;
            Ok(cursor)
        }

        /// Bind a trusted launcher's selected task to a currently live Linux group leader.
        /// This records the caller's ownership assertion, not proof of how it was spawned.
        /// # Errors
        /// Invalid task, identity mismatch, unavailable procfs or journal failure fail closed.
        pub fn bind_process(
            &mut self,
            run_id: &Id,
            task_id: &Id,
            evidence: ProcessEvidence,
        ) -> Result<u64, JournalError> {
            if self.poisoned {
                return Err(JournalError::Poisoned);
            }
            let run = self.runs.get(run_id).ok_or(JournalError::Conflict)?;
            if run.state != RunState::Running
                || !run.request.requested_task_ids.contains(task_id)
                || self.orphans.keys().any(|(id, _)| id == run_id)
                || incarnation_is_bound(&self.orphans, &evidence)
                || !process_matches(&evidence, false)
            {
                return Err(JournalError::Conflict);
            }
            self.write_orphan(
                run_id,
                OrphanRecord {
                    task_id: task_id.clone(),
                    evidence,
                    orphaned: false,
                    lease: None,
                },
            )
        }

        /// Bind the exact scheduler lease from the trusted native possession callback.
        /// An unreaped leader may be a zombie while the monitor holds it before ACK.
        /// # Errors
        /// Foreign leases, unavailable native identity and uncertain durable writes fail closed.
        pub fn bind_process_with_lease(
            &mut self,
            run_id: &Id,
            task_id: &Id,
            evidence: ProcessEvidence,
            lease: Lease,
        ) -> Result<u64, JournalError> {
            if self.poisoned {
                return Err(JournalError::Poisoned);
            }
            let run = self.runs.get(run_id).ok_or(JournalError::Conflict)?;
            if run.state != RunState::Running
                || !run.request.requested_task_ids.contains(task_id)
                || self
                    .orphans
                    .contains_key(&(run_id.clone(), task_id.clone()))
                || self.intents.keys().chain(self.orphans.keys()).any(|key| {
                    key.0 == *run_id && key.1 != *task_id && !self.reconciliations.contains_key(key)
                })
                || self
                    .intents
                    .get(&(run_id.clone(), task_id.clone()))
                    .is_none_or(|intent| intent.lease != lease)
                || !valid_lease(&lease, run_id, &evidence, &self.config.service_id)
                || incarnation_is_bound(&self.orphans, &evidence)
                || !process_matches(&evidence, true)
            {
                return Err(JournalError::Conflict);
            }
            self.write_orphan(
                run_id,
                OrphanRecord {
                    task_id: task_id.clone(),
                    evidence,
                    orphaned: false,
                    lease: Some(lease),
                },
            )
        }

        /// Record lost supervision for the exact previously bound process; no expiry releases it.
        /// # Errors
        /// Missing, foreign or already orphaned bindings and journal failures are rejected.
        pub fn mark_orphaned(
            &mut self,
            run_id: &Id,
            task_id: &Id,
            evidence: &ProcessEvidence,
        ) -> Result<u64, JournalError> {
            if self.poisoned {
                return Err(JournalError::Poisoned);
            }
            let record = self
                .orphans
                .get(&(run_id.clone(), task_id.clone()))
                .ok_or(JournalError::Conflict)?;
            if self
                .runs
                .get(run_id)
                .is_none_or(|run| !matches!(run.state, RunState::Running | RunState::Collecting))
                || record.orphaned
                || record.task_id != *task_id
                || record.evidence != *evidence
            {
                return Err(JournalError::Conflict);
            }
            let mut next = record.clone();
            next.orphaned = true;
            self.write_orphan(run_id, next)
        }

        fn write_orphan(&mut self, run_id: &Id, record: OrphanRecord) -> Result<u64, JournalError> {
            let cursor = self.cursor.checked_add(1).ok_or(JournalError::Capacity)?;
            let event = OrphanEvent {
                schema: "kioku/ci2/orphan-event/v1".into(),
                revision: 1,
                run_id: run_id.clone(),
                cursor,
                record,
            };
            self.append_event(&event, cursor)?;
            let task_key = (run_id.clone(), event.record.task_id.clone());
            if !event.record.orphaned {
                self.binding_cursors.insert(task_key.clone(), cursor);
            }
            self.orphans.insert(task_key, event.record);
            self.runs
                .get_mut(run_id)
                .ok_or(JournalError::Poisoned)?
                .event_cursor = cursor;
            Ok(cursor)
        }

        /// Read a single durable group association, including unresolved orphan status.
        /// # Errors
        /// Unknown runs, multiple task bindings and poisoned journals are rejected.
        pub fn orphan_record(&self, run_id: &Id) -> Result<Option<OrphanRecord>, JournalError> {
            let mut records = self.orphan_records(run_id)?;
            if records.len() > 1 {
                return Err(JournalError::Conflict);
            }
            Ok(records.pop())
        }

        /// Read every historical task binding in native launch order.
        /// # Errors
        /// Unknown runs and poisoned journals are rejected.
        pub fn orphan_records(&self, run_id: &Id) -> Result<Vec<OrphanRecord>, JournalError> {
            self.borrow_run(run_id)?;
            let mut records: Vec<_> = self
                .orphans
                .iter()
                .filter(|(key, _)| key.0 == *run_id)
                .collect();
            records.sort_by_key(|(key, _)| self.binding_cursors.get(*key).copied());
            Ok(records
                .into_iter()
                .map(|(_, record)| record.clone())
                .collect())
        }

        /// Read an exact durable native closure, without changing the old run result.
        /// # Errors
        /// Unknown runs and poisoned journals are rejected.
        pub fn reconciliation_record(
            &self,
            run_id: &Id,
            task_id: &Id,
        ) -> Result<Option<ReconciliationRecord>, JournalError> {
            self.borrow_run(run_id)?;
            Ok(self
                .reconciliations
                .get(&(run_id.clone(), task_id.clone()))
                .cloned())
        }

        /// Persist native-owned scoped absence before the next selected task can launch.
        /// # Errors
        /// Missing or foreign process/lease bindings, uncertain native scope, or failed sync leave ownership unresolved.
        pub fn record_native_absence(
            &mut self,
            run_id: &Id,
            task_id: &Id,
            proof: &NativeAbsenceProof,
            lease: &Lease,
        ) -> Result<u64, JournalError> {
            let run = self.load_run(run_id)?;
            let task_key = (run_id.clone(), task_id.clone());
            let binding = self.orphans.get(&task_key).ok_or(JournalError::Conflict)?;
            if proof.evidence() != &binding.evidence || binding.lease.as_ref() != Some(lease) {
                return Err(JournalError::Conflict);
            }
            if let Some(old) = self.reconciliations.get(&task_key) {
                return if old.proof == *proof.receipt() && old.lease == *lease {
                    Ok(old.cursor)
                } else {
                    Err(JournalError::Conflict)
                };
            }
            let cursor = self.cursor.checked_add(1).ok_or(JournalError::Capacity)?;
            let event = ReconciliationRecord {
                schema: "kioku/ci2/reconciliation/v1".into(),
                revision: 1,
                kind: "task-native-closed".into(),
                run_id: run_id.clone(),
                task_id: task_id.clone(),
                cursor,
                binding_cursor: *self
                    .binding_cursors
                    .get(&task_key)
                    .ok_or(JournalError::Conflict)?,
                service_id: self.config.service_id.clone(),
                resource_domain_id: self.config.resource_domain_id.clone(),
                lease: lease.clone(),
                proof: proof.receipt().clone(),
            };
            if !valid_reconciliation(&event, binding, event.binding_cursor, &run, &self.config) {
                return Err(JournalError::Conflict);
            }
            self.append_event(&event, cursor)?;
            self.reconciliations.insert(task_key, event);
            self.runs
                .get_mut(run_id)
                .ok_or(JournalError::Poisoned)?
                .event_cursor = cursor;
            Ok(cursor)
        }

        /// Reconcile a lost task only using a native-owned scoped absence proof.
        /// Bare legacy identities remain unsupported even if their PID disappeared.
        /// # Errors
        /// Unknown tuples, missing leases, unconfirmed evidence and uncertain writes fail closed.
        pub fn reconcile_orphan_stopped<'a>(
            &mut self,
            run_id: &Id,
            task_id: &Id,
            input: impl Into<ReconciliationEvidence<'a>>,
        ) -> Result<u64, JournalError> {
            self.load_run(run_id)?;
            let (evidence, proof) = match input.into() {
                ReconciliationEvidence::Unconfirmed(evidence) => (evidence, None),
                ReconciliationEvidence::Confirmed(proof) => (proof.evidence(), Some(proof)),
            };
            let record = self
                .orphans
                .get(&(run_id.clone(), task_id.clone()))
                .ok_or(JournalError::Conflict)?;
            if !record.orphaned || record.evidence != *evidence {
                return Err(JournalError::Conflict);
            }
            let Some(proof) = proof else {
                return Err(JournalError::Unsupported {
                    kind: "atomic_group_absence",
                });
            };
            let lease = record.lease.clone().ok_or(JournalError::Conflict)?;
            self.record_native_absence(run_id, task_id, proof, &lease)
        }

        /// Advance a nonterminal phase. Final outcomes require separate evidence.
        /// # Errors
        /// Invalid transitions, readonly access, capacity exhaustion or append failures reject the change.
        pub fn start_run(&mut self, run_id: &Id, phase: RunState) -> Result<u64, JournalError> {
            if !matches!(
                phase,
                RunState::Capturing | RunState::Running | RunState::Collecting
            ) {
                return Err(JournalError::Conflict);
            }
            self.advance(run_id, phase)
        }

        /// Refuse a still-queued run after a failed pre-execution check, without any observation.
        /// # Errors
        /// Only a queued run without a report can be refused; a verified report blob is required.
        pub fn refuse_before_start(
            &mut self,
            run_id: &Id,
            report: BlobRef,
        ) -> Result<RunState, JournalError> {
            if self.poisoned {
                return Err(JournalError::Poisoned);
            }
            let run = self.load_run(run_id)?;
            if run.state != RunState::Queued
                || run.report.is_some()
                || unresolved_orphan(&self.orphans, &self.reconciliations, run_id)
            {
                return Err(JournalError::Conflict);
            }
            report.validate()?;
            verify_blob(Path::new(&self.config.state_root), &report)?;
            let cursor = self.cursor.checked_add(1).ok_or(JournalError::Capacity)?;
            let event = RefusalEvent {
                schema: "kioku/ci2/refusal-event/v1".into(),
                revision: 1,
                run_id: run_id.clone(),
                cursor,
                report,
            };
            self.append_event(&event, cursor)?;
            let run = self.runs.get_mut(run_id).ok_or(JournalError::Poisoned)?;
            run.state = RunState::Incomplete;
            run.report = Some(event.report);
            run.event_cursor = cursor;
            Ok(RunState::Incomplete)
        }

        /// Cancel a run only before execution and only for its original principal.
        /// # Errors
        /// Only the original principal may apply a pre-execution cancel; replay/write errors are preserved.
        pub fn cancel(&mut self, run_id: &Id, principal: &Id) -> Result<RunState, JournalError> {
            let run = self.load_run(run_id)?;
            if run.principal_id != *principal {
                return Err(JournalError::Unauthorized);
            }
            if run.state == RunState::Cancelled {
                return Ok(RunState::Cancelled);
            }
            self.advance(run_id, RunState::Cancelled)?;
            Ok(RunState::Cancelled)
        }

        fn append_event<T: Serialize>(
            &mut self,
            event: &T,
            cursor: u64,
        ) -> Result<(), JournalError> {
            let mut line = serde_json::to_vec(event)
                .map_err(|_| JournalError::Corrupt("cannot encode event"))?;
            line.push(b'\n');
            let added = u64::try_from(line.len()).map_err(|_| JournalError::Capacity)?;
            if line.len() > MAX_EVENT
                || self
                    .bytes
                    .checked_add(added)
                    .is_none_or(|n| n > self.config.max_state_bytes)
            {
                return Err(JournalError::Capacity);
            }
            self.poisoned = true;
            let Some(journal) = self.journal.as_mut() else {
                self.poisoned = false;
                return Err(JournalError::Locked);
            };
            journal.write_all(&line)?;
            journal.sync_all()?;
            self.bytes += added;
            self.cursor = cursor;
            self.poisoned = false;
            Ok(())
        }

        /// Persist a single task observation; sequence starts at zero per run.
        /// # Errors
        /// Out-of-order observations, unreadable/digest-mismatched streams and readonly access are rejected.
        pub fn append_observation(
            &mut self,
            run_id: &Id,
            task_id: &Id,
            sequence: u64,
            observation: NativeObservation,
        ) -> Result<u64, JournalError> {
            if self.poisoned {
                return Err(JournalError::Poisoned);
            }
            let run = self.runs.get(run_id).ok_or(JournalError::Conflict)?;
            let outcome = TaskOutcome {
                task_id: task_id.clone(),
                observation,
            };
            if !can_observe(run, sequence, &outcome)
                || unresolved_orphan(&self.orphans, &self.reconciliations, run_id)
            {
                return Err(JournalError::Conflict);
            }
            verify_streams(Path::new(&self.config.state_root), run_id, &outcome)?;
            let cursor = self.cursor.checked_add(1).ok_or(JournalError::Capacity)?;
            let event = EvidenceEvent {
                schema: "kioku/ci2/observation-event/v1".into(),
                revision: 1,
                run_id: run_id.clone(),
                cursor,
                sequence,
                outcome,
            };
            self.append_event(&event, cursor)?;
            let run = self.runs.get_mut(run_id).ok_or(JournalError::Poisoned)?;
            run.observations.push(event.outcome);
            run.event_cursor = cursor;
            Ok(cursor)
        }

        /// Store a content-addressed report with a durable directory entry.
        /// # Errors
        /// Oversized content, filesystem failures and readonly access cannot create a blob.
        pub fn store_blob(&mut self, content: &[u8]) -> Result<BlobRef, JournalError> {
            use sha2::Digest as _;
            if self.poisoned {
                return Err(JournalError::Poisoned);
            }
            if u64::try_from(content.len()).map_err(|_| JournalError::Capacity)?
                > self.config.max_state_bytes
            {
                return Err(JournalError::Capacity);
            }
            let digest = Digest::new(&format!("sha256:{:x}", sha2::Sha256::digest(content)))?;
            let blob = BlobRef {
                digest,
                size_bytes: content.len() as u64,
                media_type: "application/json".into(),
            };
            let root = Path::new(&self.config.state_root);
            let dir = root.join("blobs");
            if !dir.exists() {
                fs::DirBuilder::new().mode(0o700).create(&dir)?;
                File::open(root)?.sync_all()?;
            }
            let meta = fs::symlink_metadata(&dir)?;
            if !meta.file_type().is_dir() || meta.mode() & 0o077 != 0 {
                return Err(JournalError::RootPermissions);
            }
            let path = blob_path(root, &blob.digest);
            if path.exists() {
                verify_blob(root, &blob)?;
                return Ok(blob);
            }
            let mut random = [0u8; 16];
            getrandom::fill(&mut random).map_err(|_| JournalError::Random)?;
            let temp = dir.join(format!(".tmp-{}", crate::ci2::domain::format_hex(&random)));
            self.poisoned = true;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)?;
            file.write_all(content)?;
            file.sync_all()?;
            fs::rename(temp, path)?;
            File::open(&dir)?.sync_all()?;
            self.poisoned = false;
            Ok(blob)
        }

        /// Finalize only observations already persisted and an existing report.
        /// # Errors
        /// Missing matching evidence, report digests or unsafe final state reject durable completion.
        pub fn finalize_run(
            &mut self,
            run_id: &Id,
            state: RunState,
            evidence: &[(Id, NativeObservation)],
            report: BlobRef,
        ) -> Result<u64, JournalError> {
            if self.poisoned {
                return Err(JournalError::Poisoned);
            }
            let run = self.runs.get(run_id).ok_or(JournalError::Conflict)?;
            let stored: Vec<_> = run
                .observations
                .iter()
                .map(|o| (o.task_id.clone(), o.observation.clone()))
                .collect();
            if stored != evidence {
                return Err(JournalError::Conflict);
            }
            if run.report.as_ref() == Some(&report) && run.state == state {
                return Ok(run.event_cursor);
            }
            if !valid_final(run, state)
                || (self.intents.keys().any(|(id, _)| id == run_id)
                    && !all_bindings_closed(
                        &self.orphans,
                        &self.reconciliations,
                        &self.intents,
                        run_id,
                    ))
                || unresolved_orphan(&self.orphans, &self.reconciliations, run_id)
            {
                return Err(JournalError::Conflict);
            }
            report.validate()?;
            verify_blob(Path::new(&self.config.state_root), &report)?;
            let cursor = self.cursor.checked_add(1).ok_or(JournalError::Capacity)?;
            let event = FinalEvent {
                schema: "kioku/ci2/final-event/v1".into(),
                revision: 1,
                run_id: run_id.clone(),
                cursor,
                state,
                report,
            };
            self.append_event(&event, cursor)?;
            let run = self.runs.get_mut(run_id).ok_or(JournalError::Poisoned)?;
            run.state = state;
            run.report = Some(event.report);
            run.event_cursor = cursor;
            Ok(cursor)
        }
        /// # Errors
        /// Unknown runs and a poisoned writer have no verified report reference.
        pub fn report_ref(&self, run_id: &Id) -> Result<Option<BlobRef>, JournalError> {
            Ok(self.borrow_run(run_id)?.report.clone())
        }
        /// # Errors
        /// Unknown runs and a poisoned writer cannot yield observations.
        pub fn observations(
            &self,
            run_id: &Id,
        ) -> Result<Vec<(Id, NativeObservation)>, JournalError> {
            Ok(self
                .borrow_run(run_id)?
                .observations
                .iter()
                .map(|o| (o.task_id.clone(), o.observation.clone()))
                .collect())
        }
        /// # Errors
        /// Invalid/changed paths, unknown tasks and readonly folder creation are rejected.
        pub fn location_for_task(
            &self,
            run_id: &Id,
            task_id: &Id,
            channel: &str,
        ) -> Result<std::path::PathBuf, JournalError> {
            let run = self.borrow_run(run_id)?;
            if !run.request.requested_task_ids.contains(task_id)
                || !matches!(channel, "stdout" | "stderr")
            {
                return Err(JournalError::Conflict);
            }
            let root = Path::new(&self.config.state_root);
            let results = root.join("results");
            let dir = results.join(run_id.as_str());
            for (parent, path) in [
                (root, results.as_path()),
                (results.as_path(), dir.as_path()),
            ] {
                if !path.exists() {
                    if self.journal.is_none() {
                        return Err(JournalError::Locked);
                    }
                    fs::DirBuilder::new().mode(0o700).create(path)?;
                    File::open(parent)?.sync_all()?;
                }
                let meta = fs::symlink_metadata(path)?;
                if !meta.file_type().is_dir() || meta.mode() & 0o077 != 0 {
                    return Err(JournalError::RootPermissions);
                }
            }
            Ok(dir.join(format!("{task_id}.{channel}")))
        }

        /// Read-only stream path, checked against durable observation without creating a folder.
        ///
        /// # Errors
        /// Unknown runs, non-private paths, symlinks or a task lacking a validated observation fail closed.
        pub fn artifact_for_task(
            &self,
            run_id: &Id,
            task_id: &Id,
            channel: &str,
        ) -> Result<std::path::PathBuf, JournalError> {
            let run = self.borrow_run(run_id)?;
            if !matches!(channel, "stdout" | "stderr")
                || !run
                    .observations
                    .iter()
                    .any(|outcome| outcome.task_id == *task_id)
            {
                return Err(JournalError::Conflict);
            }
            let root = Path::new(&self.config.state_root);
            let directory = root.join("results").join(run_id.as_str());
            for path in [root, &directory] {
                let metadata = fs::symlink_metadata(path)?;
                if !metadata.file_type().is_dir() || metadata.mode() & 0o077 != 0 {
                    return Err(JournalError::RootPermissions);
                }
            }
            let path = directory.join(format!("{task_id}.{channel}"));
            let metadata = fs::symlink_metadata(&path)?;
            if !metadata.file_type().is_file() || metadata.mode() & 0o077 != 0 {
                return Err(JournalError::RootPermissions);
            }
            Ok(path)
        }

        /// Returns the same durable acceptance on an identical retry.
        /// # Errors
        /// Invalid authorization, digest conflicts, capacity exhaustion or append errors reject admission.
        #[allow(clippy::too_many_lines)]
        pub fn accept(
            &mut self,
            identity: &ClientIdentity,
            request: &LocalRequest,
            request_digest: Digest,
        ) -> Result<LocalAccepted, JournalError> {
            if self.poisoned {
                return Err(JournalError::Poisoned);
            }
            identity.validate()?;
            request.validate()?;
            if !identity.roles.contains(&Role::Requester)
                || !identity.project_ids.contains(&request.project_id)
            {
                return Err(JournalError::Unauthorized);
            }
            let authorized_root = match &request.origin_claim {
                OriginClaim::LocalRegisteredCheckout { root_id, .. } => {
                    self.config.registered_roots.iter().any(|root| {
                        root.id == *root_id
                            && root.project_id == request.project_id
                            && root.allowed_principals.contains(&identity.principal_id)
                    })
                }
                _ => false,
            };
            if !authorized_root
                || !self
                    .config
                    .approved_manifest_refs
                    .iter()
                    .any(|r| r.digest == request.manifest_digest)
            {
                return Err(JournalError::Unauthorized);
            }
            if digest(request)? != request_digest {
                return Err(JournalError::Conflict);
            }
            let key = key(&identity.principal_id, request);
            if let Some(existing) = self.records.get(&key) {
                return if existing.request_digest == request_digest {
                    Ok(existing.clone())
                } else {
                    Err(JournalError::Conflict)
                };
            }
            let next_cursor = self.cursor.checked_add(1).ok_or(JournalError::Capacity)?;
            let mut random = [0u8; 16];
            getrandom::fill(&mut random).map_err(|_| JournalError::Random)?;
            let run_id = Id::new(format!("run-{}", crate::ci2::domain::format_hex(&random)))?;
            let accepted = LocalAccepted {
                schema: "kioku/ci2/local-accepted/v1".into(),
                revision: 1,
                request_id: request.request_id.clone(),
                run_id: run_id.clone(),
                request_digest,
                resume_argv: {
                    let bootstrap = Path::new(&self.config.state_root).join("bootstrap.json");
                    vec![
                        "kioku".into(),
                        "ci".into(),
                        "show".into(),
                        "--config".into(),
                        bootstrap.to_string_lossy().into_owned(),
                        "--run".into(),
                        run_id.to_string(),
                    ]
                },
                state: RunState::Queued,
            };
            accepted.validate()?;
            let event = Admission {
                schema: "kioku/ci2/admission-event/v1".into(),
                revision: 1,
                principal_id: identity.principal_id.clone(),
                request: request.clone(),
                accepted: accepted.clone(),
            };
            let mut line = serde_json::to_vec(&event)
                .map_err(|_| JournalError::Corrupt("cannot encode event"))?;
            line.push(b'\n');
            let added = u64::try_from(line.len()).map_err(|_| JournalError::Capacity)?;
            if line.len() > MAX_EVENT
                || self
                    .bytes
                    .checked_add(added)
                    .as_ref()
                    .is_none_or(|n| *n > self.config.max_state_bytes)
            {
                return Err(JournalError::Capacity);
            }
            // After an uncertain I/O result this handle must never accept another request.
            self.poisoned = true;
            let Some(journal) = self.journal.as_mut() else {
                self.poisoned = false;
                return Err(JournalError::Locked);
            };
            journal.write_all(&line)?;
            journal.sync_all()?;
            self.bytes += added;
            self.cursor = next_cursor;
            self.runs.insert(
                run_id,
                RunRecord {
                    principal_id: identity.principal_id.clone(),
                    request: request.clone(),
                    accepted: accepted.clone(),
                    state: RunState::Queued,
                    accepted_cursor: self.cursor,
                    event_cursor: self.cursor,
                    observations: Vec::new(),
                    report: None,
                    interrupted_from: None,
                },
            );
            self.records.insert(key, accepted.clone());
            self.poisoned = false;
            Ok(accepted)
        }
    }

    #[cfg(all(test, target_os = "linux"))]
    mod tests {
        use super::*;
        use crate::ci2::domain::{
            AdmissionPolicy, CancellationPolicy, CommandSpec, RegisteredRoot, RelPath,
            ResourceRecipe, ResultContract, ResultKind, RunMode, ServiceMode, TaskSpec,
            ToolIdentity, WaitClass,
        };
        use crate::ci2::native::{self, Observation, SupervisorError};
        use std::os::unix::process::CommandExt;
        use std::process::{Child, Command, Stdio};

        fn id(s: &str) -> Id {
            Id::new(s).unwrap()
        }
        fn blob() -> BlobRef {
            BlobRef {
                digest: Digest::blake3("fixture", b"content").unwrap(),
                size_bytes: 7,
                media_type: "application/json".into(),
            }
        }
        fn setup(root: &std::path::Path) -> (ServiceBootstrap, ClientIdentity, LocalRequest) {
            let reference = blob();
            let config = ServiceBootstrap {
                schema: "kioku/ci2/bootstrap/v1".into(),
                revision: 1,
                service_id: id("service"),
                mode: ServiceMode::LocalCooperative,
                resource_domain_id: id("domain"),
                state_root: root.join("state").to_str().unwrap().into(),
                socket_path: root.join("socket").to_str().unwrap().into(),
                stop_file: root.join("stop").to_str().unwrap().into(),
                registered_roots: vec![RegisteredRoot {
                    id: id("root"),
                    project_id: id("project"),
                    canonical_path: root.to_str().unwrap().into(),
                    allowed_principals: vec![id("alice")],
                    protected_refs: vec![],
                }],
                approved_manifest_refs: vec![reference.clone()],
                profiles_ref: reference.clone(),
                policy_ref: reference.clone(),
                auth_config_ref: None,
                capture_slots: 1,
                heavy_slots: 1,
                max_state_bytes: 1_000_000,
                max_queue_wait_ms: 1,
                drain_grace_ms: 1,
            };
            let identity = ClientIdentity {
                principal_id: id("alice"),
                project_ids: vec![id("project")],
                roles: vec![Role::Requester],
                trust_scope: id("local"),
                lane_id: None,
            };
            let request = LocalRequest {
                schema: "kioku/ci2/local-request/v1".into(),
                revision: 1,
                request_id: id("request"),
                project_id: id("project"),
                origin_claim: OriginClaim::LocalRegisteredCheckout {
                    root_id: id("root"),
                    relative_path: RelPath::new("work").unwrap(),
                },
                manifest_digest: reference.digest,
                profile_id: id("profile"),
                requested_task_ids: vec![id("task")],
                mode: RunMode::Normal,
                wait_class: WaitClass::Agent,
            };
            (config, identity, request)
        }
        fn digest(request: &LocalRequest) -> Digest {
            digest_request_document(&serde_json::to_value(request).unwrap()).unwrap()
        }

        // Synthetic, in-memory state-read fixtures only: these do not establish native lifecycle evidence.
        fn state_view_fixture(observation_count: usize) -> (LocalJournal, Id) {
            let (config, identity, mut request) = setup(Path::new("/synthetic-state-view"));
            request.requested_task_ids = (0..observation_count)
                .map(|index| id(&format!("task-{index}")))
                .collect();
            let run_id = id("state-view-run");
            let accepted = LocalAccepted {
                schema: "kioku/ci2/local-accepted/v1".into(),
                revision: 1,
                request_id: request.request_id.clone(),
                run_id: run_id.clone(),
                request_digest: digest(&request),
                resume_argv: vec!["kioku".into(), "ci".into(), "show".into()],
                state: RunState::Queued,
            };
            let observations = request
                .requested_task_ids
                .iter()
                .map(|task_id| TaskOutcome {
                    task_id: task_id.clone(),
                    observation: NativeObservation {
                        exit_code: Some(0),
                        signal: None,
                        stdout_digest: blob().digest,
                        stderr_digest: blob().digest,
                        stdout_bytes: 7,
                        stderr_bytes: 7,
                        wall_ns: 100,
                        cpu_user_ns: 20,
                        cpu_system_ns: 10,
                        cpu_complete: true,
                        descendants_drained: true,
                        started_ms: 1,
                        ended_ms: 2,
                        timed_out: false,
                    },
                })
                .collect();
            let record = RunRecord {
                principal_id: identity.principal_id,
                request,
                accepted,
                state: RunState::Collecting,
                accepted_cursor: 1,
                event_cursor: 2,
                observations,
                report: Some(blob()),
                interrupted_from: None,
            };
            let journal = LocalJournal {
                _lock: None,
                journal: None,
                config,
                records: HashMap::new(),
                runs: HashMap::from([(run_id.clone(), record)]),
                orphans: HashMap::new(),
                binding_cursors: HashMap::new(),
                intents: HashMap::new(),
                reconciliations: HashMap::new(),
                cursor: 2,
                bytes: 0,
                poisoned: false,
            };
            (journal, run_id)
        }

        #[test]
        fn state_view_matches_owned_read_without_mutating_evidence() {
            let (mut journal, run_id) = state_view_fixture(32);
            for state in [
                RunState::Accepted,
                RunState::Queued,
                RunState::Capturing,
                RunState::Running,
                RunState::Collecting,
                RunState::Passed,
                RunState::Failed,
                RunState::Incomplete,
                RunState::Cancelled,
            ] {
                journal.runs.get_mut(&run_id).unwrap().state = state;
                let before = serde_json::to_vec(&journal.load_run(&run_id).unwrap()).unwrap();
                assert_eq!(journal.run_state(&run_id).unwrap(), state);
                assert_eq!(
                    journal.run_state(&run_id).unwrap(),
                    journal.load_run(&run_id).unwrap().state
                );
                assert_eq!(
                    serde_json::to_vec(&journal.load_run(&run_id).unwrap()).unwrap(),
                    before
                );
                assert_eq!(journal.cursor, 2);
                assert_eq!(journal.bytes, 0);
            }
        }

        #[test]
        fn state_view_preserves_missing_and_poison_precedence() {
            let (mut journal, run_id) = state_view_fixture(32);
            let missing = id("missing");
            assert!(matches!(
                journal.run_state(&missing),
                Err(JournalError::NotFound)
            ));
            assert!(matches!(
                journal.load_run(&missing),
                Err(JournalError::NotFound)
            ));
            journal.poisoned = true;
            for run in [&run_id, &missing] {
                assert!(matches!(
                    journal.run_state(run),
                    Err(JournalError::Poisoned)
                ));
                assert!(matches!(journal.load_run(run), Err(JournalError::Poisoned)));
            }
        }

        #[test]
        #[ignore = "local synthetic state-read timing only; not native lifecycle evidence"]
        fn state_view_local_benchmark() {
            use std::hint::black_box;
            use std::time::Instant;

            // The original state-only operation is retained verbatim via the unchanged load_run.
            #[inline(never)]
            fn owned(journal: &LocalJournal, run_id: &Id) -> Result<RunState, JournalError> {
                Ok(journal.load_run(run_id)?.state)
            }
            #[inline(never)]
            fn borrowed(journal: &LocalJournal, run_id: &Id) -> Result<RunState, JournalError> {
                journal.run_state(run_id)
            }
            fn measure(journal: &LocalJournal, run_id: &Id, borrowed_first: bool) -> (u128, u128) {
                let time = |read: fn(&LocalJournal, &Id) -> Result<RunState, JournalError>| {
                    let start = Instant::now();
                    for _ in 0..10_000 {
                        assert_eq!(
                            black_box(read(black_box(journal), black_box(run_id))).unwrap(),
                            RunState::Collecting
                        );
                    }
                    start.elapsed().as_nanos()
                };
                if borrowed_first {
                    let b = time(borrowed);
                    (time(owned), b)
                } else {
                    let a = time(owned);
                    (a, time(borrowed))
                }
            }
            for count in [1, 32, 128] {
                let (journal, run_id) = state_view_fixture(count);
                assert_eq!(journal.load_run(&run_id).unwrap().observations.len(), count);
                assert_eq!(
                    owned(&journal, &run_id).unwrap(),
                    borrowed(&journal, &run_id).unwrap()
                );
                let _ = measure(&journal, &run_id, false);
                let _ = measure(&journal, &run_id, true);
                for sample in 0..31 {
                    let (baseline_ns, candidate_ns) = measure(&journal, &run_id, sample % 2 == 1);
                    println!(
                        "state-read observations={count} sample={sample} iterations=10000 baseline_ns={baseline_ns} candidate_ns={candidate_ns}"
                    );
                }
            }
        }

        fn native_task() -> TaskSpec {
            TaskSpec {
                id: id("fixture-parent"),
                adapter: id("python-eval-none"),
                command: CommandSpec {
                    executable: ToolIdentity {
                        name: "python3".into(),
                        version: "3.11".into(),
                        binary_digest: blob().digest,
                        installation_digest: blob().digest,
                    },
                    argv: vec![],
                    cwd: String::new(),
                    timeout_ms: 15_000,
                    recipe_revision: id("v1"),
                },
                inputs: vec![],
                outputs: vec![],
                dependencies: vec![],
                env: vec![],
                resources: ResourceRecipe {
                    heavy_slots: 1,
                    capture_slots: 1,
                    max_state_bytes: 65536,
                    max_stderr_bytes: 65536,
                },
                required: true,
                result_contract: ResultContract {
                    id: id("nontest"),
                    kind: ResultKind::NonTest,
                    adapter_id: id("python-eval"),
                    reporter_version: id("explicit-exit-native-v1"),
                    inventory_recipe_ref: None,
                    execution_recipe_ref: blob(),
                    identity_rule_digest: blob().digest,
                    normalization_policy_ref: blob(),
                    native_skips: vec![],
                    expected_empty: false,
                    complete_required: true,
                },
                cancellation: CancellationPolicy {
                    cancellable: true,
                    term_grace_ms: 200,
                    max_drain_ms: 800,
                },
                admission: AdmissionPolicy {
                    result_cache: false,
                    single_flight: false,
                },
            }
        }

        fn capture_owned(
            journal: &mut LocalJournal,
            run_id: &Id,
            task_id: &Id,
            lease: &Lease,
            source: &Path,
        ) -> Observation {
            capture_owned_program(
                journal,
                run_id,
                task_id,
                lease,
                source,
                "import subprocess; child=subprocess.Popen(['/usr/bin/true']); child.wait()",
            )
        }

        fn capture_owned_program(
            journal: &mut LocalJournal,
            run_id: &Id,
            task_id: &Id,
            lease: &Lease,
            source: &Path,
            program: &str,
        ) -> Observation {
            journal.begin_native_task(run_id, task_id, lease).unwrap();
            let mut task = native_task();
            task.id = task_id.clone();
            task.command.argv = vec!["-c".into(), program.into()];
            let work = source.join(task_id.as_str());
            let supervisor =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ci2/native/supervisor.py");
            let mut bind = |evidence: &ProcessEvidence| {
                journal
                    .bind_process_with_lease(run_id, task_id, evidence.clone(), lease.clone())
                    .map(|_| ())
                    .map_err(|error| SupervisorError::Configuration(error.to_string()))
            };
            native::supervise_with_stops_and_possession(
                &task,
                Path::new("/usr/bin/python3"),
                source,
                &work,
                &supervisor,
                &source.join("stop"),
                &[],
                Some(&mut bind),
            )
            .unwrap()
        }

        fn append_owned_observation(
            journal: &mut LocalJournal,
            run: &Id,
            task: &Id,
            sequence: u64,
            observed: &Observation,
        ) {
            for (channel, stream) in [
                ("stdout", &observed.streams.0),
                ("stderr", &observed.streams.1),
            ] {
                let destination = journal.location_for_task(run, task, channel).unwrap();
                fs::copy(&stream.path, &destination).unwrap();
                fs::set_permissions(
                    destination,
                    std::os::unix::fs::PermissionsExt::from_mode(0o600),
                )
                .unwrap();
            }
            journal
                .append_observation(run, task, sequence, observed.native.clone())
                .unwrap();
        }

        #[test]
        #[allow(clippy::too_many_lines)]
        fn a9_ledger_native_incomplete_preserves_exact_closure_and_history() {
            let dir = tempfile::Builder::new()
                .prefix("kioku ledger spaced cwd ")
                .tempdir()
                .unwrap();
            let (config, identity, mut request) = setup(dir.path());
            let first_task = id("task");
            let second_task = id("second");
            request.requested_task_ids.push(second_task.clone());
            let mut journal = LocalJournal::open(config.clone()).unwrap();
            let accepted = journal
                .accept(&identity, &request, digest(&request))
                .unwrap();
            let run = &accepted.run_id;
            journal.start_run(run, RunState::Running).unwrap();
            let first_lease = lease(run);
            let first = capture_owned_program(
                &mut journal,
                run,
                &first_task,
                &first_lease,
                dir.path(),
                "import pathlib,subprocess; pathlib.Path('first-cwd').write_text(str(pathlib.Path.cwd())); subprocess.run(['/usr/bin/true'],check=True); print('first stream')",
            );
            assert_eq!(first.native.exit_code, Some(0));
            assert!(first.native.cpu_complete && first.native.descendants_drained);
            let first_proof = first.absence_proof.as_ref().unwrap();
            let first_cursor = journal
                .record_native_absence(run, &first_task, first_proof, &first_lease)
                .unwrap();
            append_owned_observation(&mut journal, run, &first_task, 0, &first);
            let first_closure = journal
                .reconciliation_record(run, &first_task)
                .unwrap()
                .unwrap();
            let mut second_lease = first_lease.clone();
            second_lease.lease_id = id("second-lease");
            second_lease.epoch += 1;
            let second = capture_owned_program(
                &mut journal,
                run,
                &second_task,
                &second_lease,
                dir.path(),
                "import pathlib,subprocess; pathlib.Path('second-cwd').write_text(str(pathlib.Path.cwd())); child=subprocess.Popen(['/usr/bin/python3','-c','import os,time; os.close(1); os.close(2); time.sleep(10)']); pathlib.Path('second-child').write_text(str(child.pid)); print('second stream')",
            );
            assert_eq!(second.native.exit_code, Some(0));
            assert!(!second.native.timed_out);
            assert!(!second.native.cpu_complete);
            assert!(!second.native.descendants_drained);
            assert!(second.supervision_error.is_some());
            assert!(second.absence_proof.is_none());
            let second_evidence = second.process_evidence.as_ref().unwrap();
            let checkpoint = dir.path().join("second/possession-closure.json");
            let checkpoint_bytes = fs::read(&checkpoint).unwrap();
            assert!(
                native::read_absence_proof(&dir.path().join("second"), second_evidence)
                    .unwrap()
                    .is_none()
            );
            let child: u32 = fs::read_to_string(dir.path().join("second-child"))
                .unwrap()
                .parse()
                .unwrap();
            assert!(second.reaped_pids.contains(&child));
            for name in ["first-cwd", "second-cwd"] {
                assert_eq!(
                    fs::read_to_string(dir.path().join(name)).unwrap(),
                    dir.path().to_str().unwrap()
                );
            }
            let bindings = journal.orphan_records(run).unwrap();
            let intents = journal.native_task_intents(run).unwrap();
            assert_eq!(bindings.len(), 2);
            assert_eq!(intents.len(), 2);
            assert_eq!(bindings[0].evidence, *first_proof.evidence());
            assert_eq!(bindings[1].evidence, *second_evidence);
            assert_eq!(bindings[1].lease.as_ref(), Some(&second_lease));
            assert_eq!(intents[1].lease, second_lease);
            assert!(first_closure.binding_cursor < first_cursor);
            assert!(first_cursor < intents[1].cursor);
            assert_ne!(bindings[0].evidence, bindings[1].evidence);
            for (task, bound_lease) in [
                (&first_task, &second_lease),
                (&second_task, &first_lease),
                (&second_task, &second_lease),
            ] {
                assert!(matches!(
                    journal.record_native_absence(run, task, first_proof, bound_lease),
                    Err(JournalError::Conflict)
                ));
            }
            append_owned_observation(&mut journal, run, &second_task, 1, &second);
            let observations = journal.observations(run).unwrap();
            assert_eq!(
                observations,
                vec![
                    (first_task.clone(), first.native.clone()),
                    (second_task.clone(), second.native.clone()),
                ]
            );
            for (task, text) in [
                (&first_task, "first stream\n"),
                (&second_task, "second stream\n"),
            ] {
                assert_eq!(
                    fs::read_to_string(journal.artifact_for_task(run, task, "stdout").unwrap())
                        .unwrap(),
                    text
                );
            }
            journal.start_run(run, RunState::Collecting).unwrap();
            let raw_report = serde_json::to_vec(&observations).unwrap();
            let report = journal.store_blob(&raw_report).unwrap();
            for terminal in [RunState::Passed, RunState::Incomplete] {
                assert!(matches!(
                    journal.finalize_run(run, terminal, &observations, report.clone()),
                    Err(JournalError::Conflict)
                ));
            }
            journal
                .mark_orphaned(run, &second_task, second_evidence)
                .unwrap();
            assert!(matches!(
                journal.reconcile_orphan_stopped(run, &second_task, second_evidence),
                Err(JournalError::Unsupported {
                    kind: "atomic_group_absence"
                })
            ));
            journal.interrupt_run(run, report.clone()).unwrap();
            let historical = journal.load_run(run).unwrap();
            assert_eq!(historical.state, RunState::Incomplete);
            assert_eq!(historical.interrupted_from, Some(RunState::Collecting));
            assert_eq!(
                journal
                    .record_native_absence(run, &first_task, first_proof, &first_lease)
                    .unwrap(),
                first_cursor
            );
            assert_eq!(journal.load_run(run).unwrap(), historical);
            drop(journal);
            let mut journal = LocalJournal::open(config.clone()).unwrap();
            assert_eq!(journal.load_run(run).unwrap(), historical);
            assert_eq!(journal.observations(run).unwrap(), observations);
            assert_eq!(journal.orphan_records(run).unwrap().len(), 2);
            assert_eq!(
                journal.reconciliation_record(run, &first_task).unwrap(),
                Some(first_closure)
            );
            assert!(
                journal
                    .reconciliation_record(run, &second_task)
                    .unwrap()
                    .is_none()
            );
            assert!(!journal.native_bindings_closed(run).unwrap());
            assert!(journal.run_has_unresolved_execution(run).unwrap());
            assert!(journal.has_unresolved_execution().unwrap());
            assert_eq!(
                fs::read(blob_path(Path::new(&config.state_root), &report.digest)).unwrap(),
                raw_report
            );
            assert_eq!(fs::read(checkpoint).unwrap(), checkpoint_bytes);
            assert_eq!(
                journal
                    .accept(&identity, &request, digest(&request))
                    .unwrap(),
                accepted
            );
            let mut fresh = request.clone();
            fresh.request_id = id("fresh");
            let fresh_run = journal
                .accept(&identity, &fresh, digest(&fresh))
                .unwrap()
                .run_id;
            assert!(matches!(
                journal.start_run(&fresh_run, RunState::Running),
                Err(JournalError::Conflict)
            ));
            assert!(matches!(
                journal.record_native_absence(
                    &id("missing"),
                    &first_task,
                    first_proof,
                    &first_lease
                ),
                Err(JournalError::NotFound)
            ));
            journal.poisoned = true;
            for query_run in [run, &id("missing")] {
                assert!(matches!(
                    journal.record_native_absence(
                        query_run,
                        &first_task,
                        first_proof,
                        &first_lease
                    ),
                    Err(JournalError::Poisoned)
                ));
                assert!(matches!(
                    journal.location_for_task(query_run, &first_task, "stdout"),
                    Err(JournalError::Poisoned)
                ));
                assert!(matches!(
                    journal.artifact_for_task(query_run, &first_task, "stdout"),
                    Err(JournalError::Poisoned)
                ));
            }
        }

        #[test]
        fn a9_ledger_same_task_across_runs_cannot_exchange_native_proofs() {
            let dir = tempfile::tempdir().unwrap();
            let (config, identity, mut request) = setup(dir.path());
            let mut journal = LocalJournal::open(config.clone()).unwrap();
            let task = id("task");
            let mut closed = Vec::new();
            for name in ["first", "second"] {
                request.request_id = id(name);
                let run = journal
                    .accept(&identity, &request, digest(&request))
                    .unwrap()
                    .run_id;
                journal.start_run(&run, RunState::Running).unwrap();
                let source = dir.path().join(name);
                fs::create_dir(&source).unwrap();
                let bound_lease = lease(&run);
                let observed = capture_owned(&mut journal, &run, &task, &bound_lease, &source);
                let proof = observed.absence_proof.unwrap();
                if let Some((old_run, old_lease, old_proof, _)) = closed.first() {
                    assert!(matches!(
                        journal.record_native_absence(&run, &task, old_proof, &bound_lease),
                        Err(JournalError::Conflict)
                    ));
                    assert!(matches!(
                        journal.record_native_absence(old_run, &task, &proof, old_lease),
                        Err(JournalError::Conflict)
                    ));
                    assert!(matches!(
                        journal.record_native_absence(&run, &task, &proof, old_lease),
                        Err(JournalError::Conflict)
                    ));
                }
                let cursor = journal
                    .record_native_absence(&run, &task, &proof, &bound_lease)
                    .unwrap();
                closed.push((run, bound_lease, proof, cursor));
            }
            drop(journal);
            let mut journal = LocalJournal::open(config).unwrap();
            for (run, bound_lease, proof, cursor) in closed {
                assert_eq!(
                    journal
                        .record_native_absence(&run, &task, &proof, &bound_lease)
                        .unwrap(),
                    cursor
                );
                assert!(journal.native_bindings_closed(&run).unwrap());
                assert!(!journal.run_has_unresolved_execution(&run).unwrap());
                let record = journal.reconciliation_record(&run, &task).unwrap().unwrap();
                assert_eq!(record.proof, *proof.receipt());
                assert_eq!(record.lease, bound_lease);
                assert_eq!(record.run_id, run);
                assert_eq!(record.task_id, task);
            }
        }

        #[test]
        fn custody_queries_preserve_legacy_states_and_error_precedence() {
            let dir = tempfile::tempdir().unwrap();
            let (config, identity, request) = setup(dir.path());
            let mut journal = LocalJournal::open(config).unwrap();
            let run = journal
                .accept(&identity, &request, digest(&request))
                .unwrap()
                .run_id;
            let states = [
                RunState::Accepted,
                RunState::Capturing,
                RunState::Queued,
                RunState::Running,
                RunState::Collecting,
                RunState::Passed,
                RunState::Failed,
                RunState::Incomplete,
                RunState::Cancelled,
            ];
            // Exercise legacy in-memory records without inventing native closure evidence.
            for state in states {
                journal.runs.get_mut(&run).unwrap().state = state;
                assert_eq!(
                    journal.run_has_unresolved_execution(&run).unwrap(),
                    matches!(state, RunState::Running | RunState::Collecting)
                );
                assert!(!journal.native_bindings_closed(&run).unwrap());
                journal.runs.get_mut(&run).unwrap().interrupted_from = Some(RunState::Running);
                assert!(journal.run_has_unresolved_execution(&run).unwrap());
                assert!(journal.has_unresolved_execution().unwrap());
                journal.runs.get_mut(&run).unwrap().interrupted_from = None;
            }
            journal.runs.get_mut(&run).unwrap().state = RunState::Running;
            journal
                .begin_native_task(&run, &id("task"), &lease(&run))
                .unwrap();
            for state in states {
                journal.runs.get_mut(&run).unwrap().state = state;
                assert!(journal.run_has_unresolved_execution(&run).unwrap());
                assert!(!journal.native_bindings_closed(&run).unwrap());
            }
            let historical = journal.load_run(&run).unwrap();
            let bytes = journal.bytes;
            let cursor = journal.cursor;
            assert!(journal.run_has_unresolved_execution(&run).unwrap());
            assert_eq!(journal.load_run(&run).unwrap(), historical);
            assert_eq!((journal.bytes, journal.cursor), (bytes, cursor));
            let missing = id("missing");
            assert!(matches!(
                journal.run_has_unresolved_execution(&missing),
                Err(JournalError::NotFound)
            ));
            assert!(matches!(
                journal.native_bindings_closed(&missing),
                Err(JournalError::NotFound)
            ));
            journal.poisoned = true;
            for run_id in [&run, &missing] {
                assert!(matches!(
                    journal.load_run(run_id),
                    Err(JournalError::Poisoned)
                ));
                assert!(matches!(
                    journal.run_has_unresolved_execution(run_id),
                    Err(JournalError::Poisoned)
                ));
                assert!(matches!(
                    journal.native_bindings_closed(run_id),
                    Err(JournalError::Poisoned)
                ));
                assert!(matches!(
                    journal.native_task_intents(run_id),
                    Err(JournalError::Poisoned)
                ));
                assert!(matches!(
                    journal.orphan_records(run_id),
                    Err(JournalError::Poisoned)
                ));
                assert!(matches!(
                    journal.reconciliation_record(run_id, &id("task")),
                    Err(JournalError::Poisoned)
                ));
                assert!(matches!(
                    journal.report_ref(run_id),
                    Err(JournalError::Poisoned)
                ));
                assert!(matches!(
                    journal.observations(run_id),
                    Err(JournalError::Poisoned)
                ));
            }
            assert!(matches!(
                journal.has_unresolved_execution(),
                Err(JournalError::Poisoned)
            ));
        }

        #[test]
        fn all_runs_matches_durable_prefix_and_preserves_poison() {
            let dir = tempfile::tempdir().unwrap();
            let (mut config, identity, mut request) = setup(dir.path());
            config.max_state_bytes = 32 * 1024 * 1024;
            request.requested_task_ids =
                (0..64).map(|index| id(&format!("task-{index}"))).collect();
            let mut journal = LocalJournal::open(config).unwrap();
            for count in 0..=1024 {
                if [0, 1, 256, 257, 1024].contains(&count) {
                    let durable = journal.durable_runs().unwrap();
                    let expected = &durable[..durable.len().min(256)];
                    assert_eq!(journal.all_runs().unwrap(), expected, "count={count}");
                    let mut owned = journal.all_runs().unwrap();
                    if let Some(first) = owned.first_mut() {
                        first.request.requested_task_ids.clear();
                    }
                    assert_eq!(journal.all_runs().unwrap(), expected);
                }
                if count < 1024 {
                    request.request_id = id(&format!("request-{}", 1024 - count));
                    let accepted = journal
                        .accept(&identity, &request, digest(&request))
                        .unwrap();
                    if count % 2 == 0 {
                        journal
                            .cancel(&accepted.run_id, &identity.principal_id)
                            .unwrap();
                    }
                }
            }
            journal.poisoned = true;
            assert!(matches!(journal.all_runs(), Err(JournalError::Poisoned)));
            assert!(matches!(
                journal.load_run(&id("missing")),
                Err(JournalError::Poisoned)
            ));
            journal.runs.clear();
            assert!(matches!(journal.all_runs(), Err(JournalError::Poisoned)));
        }

        #[test]
        fn global_request_lookup_preserves_acceptance_order_and_poison() {
            let dir = tempfile::tempdir().unwrap();
            let (mut config, identity, mut request) = setup(dir.path());
            config.registered_roots[0]
                .allowed_principals
                .push(id("bob"));
            let other_path = dir.path().join("other");
            fs::create_dir(&other_path).unwrap();
            let mut other_root = config.registered_roots[0].clone();
            other_root.id = id("other-root");
            other_root.project_id = id("other-project");
            other_root.canonical_path = other_path.to_str().unwrap().into();
            config.registered_roots.push(other_root);
            let mut journal = LocalJournal::open(config.clone()).unwrap();
            let earliest = journal
                .accept(&identity, &request, digest(&request))
                .unwrap();
            journal
                .cancel(&earliest.run_id, &identity.principal_id)
                .unwrap();
            let mut bob = identity.clone();
            bob.principal_id = id("bob");
            let second = journal.accept(&bob, &request, digest(&request)).unwrap();
            let shared_id = request.request_id.clone();
            let mut other_identity = identity.clone();
            other_identity.project_ids.push(id("other-project"));
            let mut other_request = request.clone();
            other_request.project_id = id("other-project");
            other_request.origin_claim = OriginClaim::LocalRegisteredCheckout {
                root_id: id("other-root"),
                relative_path: RelPath::new("work").unwrap(),
            };
            let third = journal
                .accept(&other_identity, &other_request, digest(&other_request))
                .unwrap();
            assert_ne!(earliest.run_id, second.run_id);
            assert_ne!(earliest.run_id, third.run_id);
            for index in 0..253 {
                request.request_id = id(&format!("filler-{index}"));
                journal
                    .accept(&identity, &request, digest(&request))
                    .unwrap();
            }
            request.request_id = id("beyond-display-limit");
            let last = journal
                .accept(&identity, &request, digest(&request))
                .unwrap();
            assert_eq!(journal.durable_runs().unwrap().len(), 257);
            assert_eq!(journal.all_runs().unwrap().len(), 256);
            drop(journal);
            let mut journal = LocalJournal::open(config).unwrap();
            for request_id in [&shared_id, &request.request_id] {
                assert_eq!(
                    journal.load_request_for_id(request_id).unwrap(),
                    journal
                        .durable_runs()
                        .unwrap()
                        .into_iter()
                        .find(|run| run.request.request_id == *request_id)
                );
            }
            let selected = journal.load_request_for_id(&shared_id).unwrap().unwrap();
            assert_eq!(selected.accepted, earliest);
            assert_eq!(selected.principal_id, identity.principal_id);
            assert_eq!(selected.request.project_id, id("project"));
            assert_eq!(selected.state, RunState::Cancelled);
            assert_eq!(
                journal
                    .load_request_for_id(&request.request_id)
                    .unwrap()
                    .unwrap()
                    .accepted,
                last
            );
            let missing = id("missing");
            assert!(journal.load_request_for_id(&missing).unwrap().is_none());
            let before = (journal.cursor, journal.bytes);
            journal.load_request_for_id(&shared_id).unwrap();
            assert_eq!((journal.cursor, journal.bytes), before);
            journal.poisoned = true;
            for request_id in [&shared_id, &request.request_id, &missing] {
                assert!(matches!(
                    journal.load_request_for_id(request_id),
                    Err(JournalError::Poisoned)
                ));
            }
        }

        #[test]
        fn interrupted_runs_match_durable_filter_in_accepted_order() {
            let dir = tempfile::tempdir().unwrap();
            let (config, identity, mut request) = setup(dir.path());
            let mut journal = LocalJournal::open(config).unwrap();
            let mut runs = Vec::new();
            for index in 0..3 {
                request.request_id = id(&format!("request-{index}"));
                runs.push(
                    journal
                        .accept(&identity, &request, digest(&request))
                        .unwrap()
                        .run_id,
                );
            }
            assert_eq!(journal.interrupted_runs().unwrap(), Vec::<RunRecord>::new());
            for run in &runs {
                journal.start_run(run, RunState::Running).unwrap();
            }
            let report = journal.store_blob(b"{}").unwrap();
            for (index, terminal) in [(1, RunState::Failed), (0, RunState::Incomplete)] {
                journal
                    .interrupt_run_with_result(&runs[index], report.clone(), terminal)
                    .unwrap();
            }
            let interrupted = journal.interrupted_runs().unwrap();
            assert_eq!(
                interrupted
                    .iter()
                    .map(|run| &run.accepted.run_id)
                    .collect::<Vec<_>>(),
                vec![&runs[0], &runs[1]]
            );
            assert_eq!(
                interrupted,
                journal
                    .durable_runs()
                    .unwrap()
                    .into_iter()
                    .filter(|run| run.interrupted_from.is_some())
                    .collect::<Vec<_>>()
            );
            journal.poisoned = true;
            assert!(matches!(
                journal.interrupted_runs(),
                Err(JournalError::Poisoned)
            ));
        }

        #[test]
        fn actual_native_proof_durably_reconciles_without_rewriting_history() {
            for terminal in [RunState::Incomplete, RunState::Failed] {
                let dir = tempfile::tempdir().unwrap();
                let (config, identity, request) = setup(dir.path());
                let mut journal = LocalJournal::open(config.clone()).unwrap();
                let accepted = journal
                    .accept(&identity, &request, digest(&request))
                    .unwrap();
                let run = &accepted.run_id;
                journal.start_run(run, RunState::Running).unwrap();
                let bound_lease = lease(run);
                let observed =
                    capture_owned(&mut journal, run, &id("task"), &bound_lease, dir.path());
                let proof = observed.absence_proof.as_ref().unwrap();
                journal
                    .mark_orphaned(run, &id("task"), proof.evidence())
                    .unwrap();
                let report = journal.store_blob(b"{}").unwrap();
                journal
                    .interrupt_run_with_result(run, report, terminal)
                    .unwrap();
                let historical = journal.load_run(run).unwrap();
                assert!(journal.has_unresolved_execution().unwrap());
                let mut wrong = bound_lease.clone();
                wrong.epoch += 1;
                assert!(
                    journal
                        .record_native_absence(run, &id("task"), proof, &wrong)
                        .is_err()
                );
                assert!(
                    journal
                        .record_native_absence(run, &id("other"), proof, &bound_lease)
                        .is_err()
                );
                let mut readonly = LocalJournal::open_read_only(config.clone()).unwrap();
                assert!(matches!(
                    readonly.record_native_absence(run, &id("task"), proof, &bound_lease),
                    Err(JournalError::Io(_))
                ));
                assert!(matches!(
                    readonly.native_bindings_closed(run),
                    Err(JournalError::Poisoned)
                ));
                drop(readonly);
                let cursor = journal
                    .reconcile_orphan_stopped(run, &id("task"), proof)
                    .unwrap();
                let bytes = journal.bytes;
                assert_eq!(
                    journal
                        .reconcile_orphan_stopped(run, &id("task"), proof)
                        .unwrap(),
                    cursor
                );
                assert_eq!(journal.bytes, bytes);
                assert!(journal.orphan_record(run).unwrap().unwrap().orphaned);
                let after = journal.load_run(run).unwrap();
                assert_eq!(after.state, historical.state);
                assert_eq!(after.interrupted_from, historical.interrupted_from);
                assert_eq!(after.report, historical.report);
                assert_eq!(after.observations, historical.observations);
                assert!(journal.native_bindings_closed(run).unwrap());
                assert!(!journal.has_unresolved_execution().unwrap());
                drop(journal);
                let mut journal = LocalJournal::open(config).unwrap();
                assert_eq!(
                    journal
                        .reconciliation_record(run, &id("task"))
                        .unwrap()
                        .unwrap()
                        .cursor,
                    cursor
                );
                assert_eq!(
                    journal
                        .reconcile_orphan_stopped(run, &id("task"), proof)
                        .unwrap(),
                    cursor
                );
                assert!(!journal.has_unresolved_execution().unwrap());
                assert_eq!(
                    journal
                        .accept(&identity, &request, digest(&request))
                        .unwrap(),
                    accepted
                );
            }
        }

        #[test]
        fn second_task_intent_without_header_cannot_reuse_first_task_closure() {
            let dir = tempfile::tempdir().unwrap();
            let (config, identity, mut request) = setup(dir.path());
            request.requested_task_ids.push(id("second"));
            let mut journal = LocalJournal::open(config.clone()).unwrap();
            let run = journal
                .accept(&identity, &request, digest(&request))
                .unwrap()
                .run_id;
            journal.start_run(&run, RunState::Running).unwrap();
            let bound_lease = lease(&run);
            let first = capture_owned(&mut journal, &run, &id("task"), &bound_lease, dir.path());
            let proof = first.absence_proof.as_ref().unwrap();
            journal
                .record_native_absence(&run, &id("task"), proof, &bound_lease)
                .unwrap();
            assert!(journal.native_bindings_closed(&run).unwrap());
            journal
                .begin_native_task(&run, &id("second"), &bound_lease)
                .unwrap();
            assert!(
                journal
                    .begin_native_task(&run, &id("second"), &bound_lease)
                    .is_err()
            );
            assert!(!journal.native_bindings_closed(&run).unwrap());
            assert!(journal.run_has_unresolved_execution(&run).unwrap());
            assert!(
                journal
                    .record_native_absence(&run, &id("second"), proof, &bound_lease)
                    .is_err()
            );
            let report = journal.store_blob(b"{}").unwrap();
            journal.interrupt_run(&run, report).unwrap();
            assert!(journal.has_unresolved_execution().unwrap());
            drop(journal);
            let journal = LocalJournal::open(config).unwrap();
            assert_eq!(journal.native_task_intents(&run).unwrap().len(), 2);
            assert_eq!(journal.orphan_records(&run).unwrap().len(), 1);
            assert!(journal.has_unresolved_execution().unwrap());
            assert!(!journal.native_bindings_closed(&run).unwrap());
        }

        #[test]
        fn all_task_closures_survive_restart_and_tampered_lease_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let (config, identity, mut request) = setup(dir.path());
            request.requested_task_ids.push(id("second"));
            let mut journal = LocalJournal::open(config.clone()).unwrap();
            let run = journal
                .accept(&identity, &request, digest(&request))
                .unwrap()
                .run_id;
            journal.start_run(&run, RunState::Running).unwrap();
            let bound_lease = lease(&run);
            let first = capture_owned(&mut journal, &run, &id("task"), &bound_lease, dir.path());
            let first_proof = first.absence_proof.as_ref().unwrap();
            let first_cursor = journal
                .record_native_absence(&run, &id("task"), first_proof, &bound_lease)
                .unwrap();
            let second = capture_owned(&mut journal, &run, &id("second"), &bound_lease, dir.path());
            let second_proof = second.absence_proof.as_ref().unwrap();
            assert!(
                journal
                    .record_native_absence(&run, &id("task"), second_proof, &bound_lease)
                    .is_err()
            );
            journal
                .record_native_absence(&run, &id("second"), second_proof, &bound_lease)
                .unwrap();
            assert_eq!(journal.orphan_records(&run).unwrap().len(), 2);
            assert!(matches!(
                journal.orphan_record(&run),
                Err(JournalError::Conflict)
            ));
            assert!(journal.native_bindings_closed(&run).unwrap());
            let report = journal.store_blob(b"{}").unwrap();
            journal.interrupt_run(&run, report).unwrap();
            assert!(!journal.run_has_unresolved_execution(&run).unwrap());
            assert!(!journal.has_unresolved_execution().unwrap());
            drop(journal);
            let mut journal = LocalJournal::open(config.clone()).unwrap();
            assert_eq!(
                journal
                    .record_native_absence(&run, &id("task"), first_proof, &bound_lease)
                    .unwrap(),
                first_cursor
            );
            assert!(journal.native_bindings_closed(&run).unwrap());
            assert_eq!(
                journal.load_run(&run).unwrap().interrupted_from,
                Some(RunState::Running)
            );
            drop(journal);
            let log = Path::new(&config.state_root).join("admissions.jsonl");
            let bytes = fs::read_to_string(&log).unwrap();
            let mut events: Vec<serde_json::Value> = bytes
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            let closure = events
                .iter_mut()
                .find(|event| event["schema"] == "kioku/ci2/reconciliation/v1")
                .unwrap();
            closure["lease"]["epoch"] = serde_json::json!(99);
            let tampered = events
                .into_iter()
                .map(|event| serde_json::to_string(&event).unwrap() + "\n")
                .collect::<String>();
            fs::write(log, tampered).unwrap();
            assert!(matches!(
                LocalJournal::open_read_only(config),
                Err(JournalError::Corrupt(_))
            ));
        }
        struct OwnedChild(Child);
        impl Drop for OwnedChild {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        fn owned_process() -> (OwnedChild, ProcessEvidence) {
            let child = OwnedChild(
                Command::new("/usr/bin/sleep")
                    .arg("60")
                    .process_group(0)
                    .env_clear()
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap(),
            );
            let pid = child.0.id();
            let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
            let fields: Vec<_> = stat
                .rsplit_once(") ")
                .unwrap()
                .1
                .split_ascii_whitespace()
                .collect();
            let evidence = ProcessEvidence {
                process: ProcessIdentity {
                    pid,
                    pgid: pid,
                    started_ms: 1,
                },
                start_ticks: fields[19].parse().unwrap(),
                boot_id: fs::read_to_string("/proc/sys/kernel/random/boot_id")
                    .unwrap()
                    .trim()
                    .into(),
            };
            (child, evidence)
        }
        fn lease(run_id: &Id) -> Lease {
            Lease {
                lease_id: run_id.clone(),
                item_id: run_id.clone(),
                owner: id("service"),
                epoch: 0,
                acquired_ms: 0,
                process_identity: None,
            }
        }
        #[test]
        fn native_binding_keeps_exact_lease_across_restart() {
            let dir = tempfile::tempdir().unwrap();
            let (config, identity, request) = setup(dir.path());
            let mut journal = LocalJournal::open(config.clone()).unwrap();
            let accepted = journal
                .accept(&identity, &request, digest(&request))
                .unwrap();
            journal
                .start_run(&accepted.run_id, RunState::Running)
                .unwrap();
            let (child, evidence) = owned_process();
            let original = lease(&accepted.run_id);
            journal
                .begin_native_task(&accepted.run_id, &id("task"), &original)
                .unwrap();
            let mut foreign = original.clone();
            foreign.owner = id("foreign");
            assert!(
                journal
                    .bind_process_with_lease(
                        &accepted.run_id,
                        &id("task"),
                        evidence.clone(),
                        foreign
                    )
                    .is_err()
            );
            journal
                .bind_process_with_lease(
                    &accepted.run_id,
                    &id("task"),
                    evidence.clone(),
                    original.clone(),
                )
                .unwrap();
            journal
                .mark_orphaned(&accepted.run_id, &id("task"), &evidence)
                .unwrap();
            let report = journal.store_blob(b"{}").unwrap();
            journal.interrupt_run(&accepted.run_id, report).unwrap();
            drop(child);
            drop(journal);
            let mut journal = LocalJournal::open(config).unwrap();
            assert_eq!(
                journal
                    .orphans
                    .get(&(accepted.run_id.clone(), id("task")))
                    .unwrap()
                    .lease
                    .as_ref(),
                Some(&original)
            );
            assert!(journal.has_unresolved_execution().unwrap());
            assert!(matches!(
                journal.reconcile_orphan_stopped(&accepted.run_id, &id("task"), &evidence),
                Err(JournalError::Unsupported { .. })
            ));
            assert_eq!(
                journal
                    .accept(&identity, &request, digest(&request))
                    .unwrap(),
                accepted
            );
            assert_eq!(
                journal.load_run(&accepted.run_id).unwrap().interrupted_from,
                Some(RunState::Running)
            );
        }
    }
    impl LocalStoragePort for LocalJournal {
        type Error = JournalError;
        fn accept_request(
            &mut self,
            identity: &ClientIdentity,
            request: &LocalRequest,
            digest: Digest,
        ) -> Result<LocalAccepted, Self::Error> {
            self.accept(identity, request, digest)
        }
        fn start_run(&mut self, run_id: &Id, phase: RunState) -> Result<u64, Self::Error> {
            LocalJournal::start_run(self, run_id, phase)
        }
        fn enqueue(&mut self, _: &QueueItem) -> Result<Lease, Self::Error> {
            Err(JournalError::Unsupported { kind: "enqueue" })
        }
        fn mark_started(&mut self, _: &Lease, _: ProcessIdentity) -> Result<(), Self::Error> {
            Err(JournalError::Unsupported {
                kind: "mark_started",
            })
        }
        fn append_observation(
            &mut self,
            _: &Id,
            _: u64,
            _: &NativeObservation,
        ) -> Result<(), Self::Error> {
            Err(JournalError::Unsupported {
                kind: "append_observation_without_task",
            })
        }
        fn finalize_run(&mut self, _: &Id, _: RunState, _: BlobRef) -> Result<(), Self::Error> {
            Err(JournalError::Unsupported {
                kind: "finalize_without_evidence",
            })
        }
        fn reconcile_stopped(
            &mut self,
            _: &Lease,
            _: &ProcessIdentity,
            _: bool,
        ) -> Result<(), Self::Error> {
            Err(JournalError::Unsupported {
                kind: "reconcile_stopped",
            })
        }
    }
}
#[cfg(unix)]
pub use unix::{
    JournalError, LocalJournal, NativeTaskIntent, OrphanRecord, ProcessEvidence,
    ReconciliationEvidence, ReconciliationRecord, RunRecord, TaskOutcome,
};
