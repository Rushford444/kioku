//! Cross-field validation for the local wire. No implicit permission is inferred.
use super::{
    primitives::{BlobRef, ContractError, Digest, Id, RelPath, Validate, check_range},
    schema::{
        AdmissionPolicy, BranchClass, CancellationPolicy, ClientIdentity, CommandSpec,
        DecisionEvent, EnvKind, EnvSpec, InputSelector, LocalAccepted, LocalRequest, ModelMode,
        NativeObservation, OriginAuthority, OriginClaim, OriginKind, OutputKind, OutputSpec,
        PrivateNativeState, RegisteredRoot, ResourceRecipe, ResultContract, ResultKind, Role,
        RunOrigin, RunState, RunView, ServiceBootstrap, ServiceMode, TaskManifest, TaskSpec,
    },
};
#[cfg(unix)]
use std::path::Path;
fn req_schema(actual: &str, expected: &str, rev: u16) -> Result<(), ContractError> {
    if actual != expected || rev != 1 {
        return Err(ContractError::Unsupported(format!(
            "unexpected wire {actual} revision {rev}"
        )));
    }
    Ok(())
}
fn valid_git(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn valid_protected_ref(value: &str) -> bool {
    if valid_git(value) {
        return true;
    }
    value.strip_prefix("refs/").is_some_and(|rest| {
        !rest.is_empty()
            && !value.contains("..")
            && !value.contains("@{")
            && rest.split('/').all(|component| {
                let bytes = component.as_bytes();
                !bytes.is_empty()
                    && bytes[0] != b'.'
                    && bytes[bytes.len() - 1] != b'.'
                    && !bytes.ends_with(b".lock")
                    && bytes
                        .iter()
                        .all(|b| *b > 0x20 && *b != 0x7f && !b"~^:?*[\\".contains(b))
            })
    })
}
fn abs_path(value: &str) -> Result<(), ContractError> {
    if !value.starts_with('/') || value.contains('\0') {
        return Err(ContractError::Configuration(
            "non absolute operator path".into(),
        ));
    }
    if !Path::new(value).components().all(|item| {
        matches!(
            item,
            std::path::Component::Normal(_) | std::path::Component::RootDir
        )
    }) {
        return Err(ContractError::Configuration(
            "non canonical operator path".into(),
        ));
    }
    Ok(())
}
fn positive(value: u64) -> Result<(), ContractError> {
    check_range(value)?;
    if value == 0 {
        Err(ContractError::Configuration(
            "timeout or capacity zero".into(),
        ))
    } else {
        Ok(())
    }
}
fn unique<T: std::hash::Hash + Eq>(
    values: impl IntoIterator<Item = T>,
    subject: &str,
) -> Result<(), ContractError> {
    let mut seen = std::collections::HashSet::new();
    for value in values {
        if !seen.insert(value) {
            return Err(ContractError::Configuration(format!("duplicate {subject}")));
        }
    }
    Ok(())
}
impl Validate for ClientIdentity {
    fn validate(&self) -> Result<(), ContractError> {
        self.principal_id.validate()?;
        self.trust_scope.validate()?;
        self.lane_id.as_ref().map_or(Ok(()), Validate::validate)?;
        for p in &self.project_ids {
            p.validate()?;
        }
        positive(u64::try_from(self.roles.len()).unwrap_or(u64::MAX))?;
        if self.project_ids.is_empty() {
            return Err(ContractError::Configuration(
                "empty identity projects".into(),
            ));
        }
        unique(&self.project_ids, "project id")?;
        unique(&self.roles, "role")?;
        if self.roles.contains(&Role::Operator) && self.roles.contains(&Role::Executor) {
            return Err(ContractError::Unsupported(
                "operator identity must not execute project-controlled code".into(),
            ));
        }
        Ok(())
    }
}
impl Validate for OriginClaim {
    fn validate(&self) -> Result<(), ContractError> {
        match self {
            Self::LocalRegisteredCheckout {
                root_id,
                relative_path,
            } => {
                root_id.validate()?;
                relative_path.validate()
            }
            Self::ForgeEvent { event_id } => event_id.validate(),
            Self::DiagnosticUpload { upload_id } => upload_id.validate(),
        }
    }
}
impl Validate for OriginKind {
    fn validate(&self) -> Result<(), ContractError> {
        Ok(())
    }
}

impl Validate for RegisteredRoot {
    fn validate(&self) -> Result<(), ContractError> {
        self.id.validate()?;
        self.project_id.validate()?;
        abs_path(&self.canonical_path)?;
        if self.allowed_principals.is_empty() {
            return Err(ContractError::Configuration(
                "cannot grant empty principal allowlist".into(),
            ));
        }
        unique(&self.allowed_principals, "root principal")?;
        for id in &self.allowed_principals {
            id.validate()?;
        }
        for reference in &self.protected_refs {
            if valid_protected_ref(reference) {
                continue;
            }
            return Err(ContractError::Configuration("protected ref invalid".into()));
        }
        Ok(())
    }
}
impl Validate for ServiceBootstrap {
    fn validate(&self) -> Result<(), ContractError> {
        req_schema(&self.schema, "kioku/ci2/bootstrap/v1", self.revision)?;
        if self.mode != ServiceMode::LocalCooperative {
            return Err(ContractError::Unsupported(
                "only local cooperative is enabled".into(),
            ));
        }
        self.service_id.validate()?;
        self.resource_domain_id.validate()?;
        abs_path(&self.state_root)?;
        abs_path(&self.socket_path)?;
        abs_path(&self.stop_file)?;
        positive(self.max_state_bytes)?;
        positive(self.max_queue_wait_ms)?;
        positive(self.drain_grace_ms)?;
        if self.capture_slots != 1 || self.heavy_slots != 1 {
            return Err(ContractError::Unsupported(
                "I1-L has exactly one capture and heavy slot".into(),
            ));
        }
        if self.registered_roots.is_empty() || self.approved_manifest_refs.is_empty() {
            return Err(ContractError::Configuration(
                "registered roots and approved manifests required".into(),
            ));
        }
        self.profiles_ref.validate()?;
        self.policy_ref.validate()?;
        self.auth_config_ref.as_ref().map_or(Ok(()), Id::validate)?;
        for item in &self.approved_manifest_refs {
            item.validate()?;
        }
        for root in &self.registered_roots {
            root.validate()?;
        }
        unique(
            self.registered_roots.iter().map(|r| r.id.clone()),
            "root ID",
        )?;
        unique(
            self.registered_roots
                .iter()
                .map(|r| r.canonical_path.as_str()),
            "root canonical path",
        )?;
        Ok(())
    }
}
impl Validate for RunOrigin {
    fn validate(&self) -> Result<(), ContractError> {
        if self.full_ci_publication_allowed {
            return Err(ContractError::Unsupported(
                "publication is always off in I1-L".into(),
            ));
        }
        self.evidence_ref.validate()?;
        self.snapshot_digest.validate()?;
        self.root_id.as_ref().map_or(Ok(()), Id::validate)?;
        self.event_id.as_ref().map_or(Ok(()), Id::validate)?;
        match (self.kind, self.authority) {
            (OriginKind::LocalRegisteredCheckout, OriginAuthority::LocalOperator) => {
                if self.root_id.is_none() {
                    return Err(ContractError::Configuration(
                        "local origin requires root ID".into(),
                    ));
                }
                if self.event_id.is_some() || self.verified_branch_class != BranchClass::Dev {
                    return Err(ContractError::Configuration(
                        "local origin cannot assert forge classification".into(),
                    ));
                }
            }
            (OriginKind::DiagnosticUpload, OriginAuthority::Diagnostic) => {
                if self.event_id.is_some()
                    || self.root_id.is_some()
                    || self.verified_branch_class != BranchClass::Unknown
                {
                    return Err(ContractError::Configuration(
                        "diagnostic origin must have no source identity".into(),
                    ));
                }
            }
            (OriginKind::ForgeEvent, OriginAuthority::VerifiedForge) => {
                if self.event_id.is_none()
                    || self.root_id.is_none()
                    || self.verified_branch_class == BranchClass::Dev
                {
                    return Err(ContractError::Configuration(
                        "forge requires consistent event/root/class".into(),
                    ));
                }
            }
            _ => {
                return Err(ContractError::Unsupported(
                    "origin authority does not match origin kind".into(),
                ));
            }
        }
        if let Some(commit) = &self.observed_commit
            && !valid_git(commit)
        {
            return Err(ContractError::Configuration("bad observed commit".into()));
        }
        Ok(())
    }
}
impl Validate for LocalRequest {
    fn validate(&self) -> Result<(), ContractError> {
        req_schema(&self.schema, "kioku/ci2/local-request/v1", self.revision)?;
        self.request_id.validate()?;
        self.project_id.validate()?;
        self.profile_id.validate()?;
        self.manifest_digest.validate()?;
        self.origin_claim.validate()?;
        unique(&self.requested_task_ids, "requested task ID")?;
        match &self.origin_claim {
            OriginClaim::LocalRegisteredCheckout { root_id, .. }
            | OriginClaim::DiagnosticUpload {
                upload_id: root_id, ..
            } => root_id.validate()?,
            OriginClaim::ForgeEvent { .. } => {
                return Err(ContractError::Unsupported(
                    "forge event not admitted local".into(),
                ));
            }
        }
        Ok(())
    }
}

impl Validate for ResultContract {
    fn validate(&self) -> Result<(), ContractError> {
        self.id.validate()?;
        self.adapter_id.validate()?;
        self.reporter_version.validate()?;
        self.identity_rule_digest.validate()?;
        self.execution_recipe_ref.validate()?;
        self.normalization_policy_ref.validate()?;
        self.inventory_recipe_ref
            .as_ref()
            .map_or(Ok(()), Validate::validate)?;
        if self.kind != ResultKind::NonTest && self.inventory_recipe_ref.is_none() {
            return Err(ContractError::Configuration(
                "test result contracts require inventory".into(),
            ));
        }
        for skip in &self.native_skips {
            skip.case_id.validate()?;
            skip.profile_digest.validate()?;
            skip.reason_code.validate()?;
            skip.approval_ref.validate()?;
            positive(skip.expires_at_ms)?;
            unique(
                self.native_skips.iter().map(|s| s.case_id.clone()),
                "native skip",
            )?;
        }
        if self.complete_required && self.kind == ResultKind::OpaqueDiagnostic {
            return Err(ContractError::Unsupported(
                "opaque diagnostic cannot satisfy a required pipeline".into(),
            ));
        }
        Ok(())
    }
}
impl Validate for CancellationPolicy {
    fn validate(&self) -> Result<(), ContractError> {
        positive(self.term_grace_ms)?;
        positive(self.max_drain_ms)?;
        if self.term_grace_ms > self.max_drain_ms {
            return Err(ContractError::Configuration(
                "termination period exceeds captured drain".into(),
            ));
        }
        Ok(())
    }
}
impl Validate for CommandSpec {
    fn validate(&self) -> Result<(), ContractError> {
        if self.executable.name.is_empty()
            || self.executable.name.contains('/')
            || self.executable.version.is_empty()
        {
            return Err(ContractError::Configuration(
                "command tool name or version missing".into(),
            ));
        }
        self.executable.binary_digest.validate()?;
        self.executable.installation_digest.validate()?;
        if self.argv.is_empty()
            || positive(self.timeout_ms).is_err()
            || self.recipe_revision.validate().is_err()
        {
            return Err(ContractError::Configuration(
                "empty argv bad timeout or recipe".into(),
            ));
        }
        if !self.cwd.is_empty() {
            RelPath::new(&self.cwd)?;
        } else if self.cwd.contains('\0') {
            return Err(ContractError::Configuration("nul cwd".into()));
        }
        for argument in &self.argv {
            if argument.is_empty() || argument.contains('\0') {
                return Err(ContractError::Configuration("empty argument or NUL".into()));
            }
            if argument.starts_with("http://")
                || argument.starts_with("https://")
                || argument.starts_with("file://")
            {
                return Err(ContractError::Unsupported(
                    "network URL cannot be a task command".into(),
                ));
            }
        }
        Ok(())
    }
}
impl Validate for ResourceRecipe {
    fn validate(&self) -> Result<(), ContractError> {
        positive(self.max_state_bytes)?;
        positive(self.max_stderr_bytes)?;
        if self.heavy_slots > 1 || self.capture_slots > 1 || self.capture_slots == 0 {
            return Err(ContractError::Unsupported(
                "task exceeds cooperative capacity".into(),
            ));
        }
        if self.heavy_slots == 0 && self.capture_slots > 1 {
            return Err(ContractError::Configuration(
                "invalid capture slot request".into(),
            ));
        }
        Ok(())
    }
}
impl Validate for OutputSpec {
    fn validate(&self) -> Result<(), ContractError> {
        self.id.validate()?;
        positive(self.max_bytes)?;
        match self.kind {
            OutputKind::Stdout | OutputKind::Stderr => {
                if self.path.is_some() {
                    return Err(ContractError::Configuration(
                        "stream path must be null".into(),
                    ));
                }
            }
            OutputKind::TestInventory
            | OutputKind::TestResults
            | OutputKind::Artifact
            | OutputKind::Diagnostics => {
                self.path.as_ref().map_or(
                    Err(ContractError::Configuration(
                        "report or artifact path required".into(),
                    )),
                    Validate::validate,
                )?;
            }
        }
        Ok(())
    }
}

impl Validate for EnvSpec {
    fn validate(&self) -> Result<(), ContractError> {
        if self.name.is_empty()
            || !self
                .name
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b == b'_' || b.is_ascii_digit())
        {
            return Err(ContractError::Configuration(
                "closed environment name required".into(),
            ));
        }
        match self.kind {
            EnvKind::Absent => {
                if self.value.is_some() || self.capability_ref.is_some() {
                    return Err(ContractError::Configuration(
                        "absent env has payload".into(),
                    ));
                }
            }
            EnvKind::PublicValue => {
                if self.capability_ref.is_some() {
                    return Err(ContractError::Configuration(
                        "public env references secret".into(),
                    ));
                }
                let Some(value) = &self.value else {
                    return Err(ContractError::Configuration(
                        "public env missing value".into(),
                    ));
                };
                if value.contains('\0') {
                    return Err(ContractError::Configuration("NUL in env".into()));
                }
            }
            EnvKind::SecretCapability => {
                if self.value.is_some() || self.capability_ref.is_none() {
                    return Err(ContractError::Configuration(
                        "invalid secret variant".into(),
                    ));
                }
                return Err(ContractError::Unsupported(
                    "local recipe has no secrets".into(),
                ));
            }
        }
        Ok(())
    }
}
impl Validate for AdmissionPolicy {
    fn validate(&self) -> Result<(), ContractError> {
        if self.result_cache || self.single_flight {
            Err(ContractError::Unsupported(
                "automatic cache or single flight is off".into(),
            ))
        } else {
            Ok(())
        }
    }
}
impl Validate for InputSelector {
    fn validate(&self) -> Result<(), ContractError> {
        match self {
            InputSelector::Tree { root } => root.validate(),
            InputSelector::File { path } | InputSelector::Absent { path } => path.validate(),
            InputSelector::Glob { root, pattern } => {
                root.validate()?;
                if pattern.is_empty()
                    || pattern == ".."
                    || pattern.contains("/../")
                    || pattern.contains('\0')
                {
                    Err(ContractError::Configuration("unsupported glob".into()))
                } else {
                    Ok(())
                }
            }
            InputSelector::Producer { task_id, output_id } => {
                task_id.validate()?;
                output_id.validate()
            }
        }
    }
}
impl Validate for TaskSpec {
    fn validate(&self) -> Result<(), ContractError> {
        self.id.validate()?;
        self.adapter.validate()?;
        self.command.validate()?;
        self.result_contract.validate()?;
        self.cancellation.validate()?;
        self.resources.validate()?;
        self.admission.validate()?;
        if self.result_contract.adapter_id != self.adapter {
            return Err(ContractError::Configuration("adapter mismatch".into()));
        }
        self.inputs.iter().try_for_each(Validate::validate)?;
        self.outputs.iter().try_for_each(Validate::validate)?;
        self.env.iter().try_for_each(Validate::validate)?;
        for dependency in &self.dependencies {
            dependency.task_id.validate()?;
            dependency
                .consumed_output_ids
                .iter()
                .try_for_each(Id::validate)?;
            unique(&dependency.consumed_output_ids, "dependency output")?;
        }
        unique(self.outputs.iter().map(|o| o.id.clone()), "output")?;
        unique(self.env.iter().map(|e| e.name.as_str()), "env")?;
        unique(
            self.dependencies.iter().map(|d| d.task_id.clone()),
            "dependency",
        )?;
        if self.outputs.is_empty() {
            return Err(ContractError::Configuration("task outputs absent".into()));
        }
        if self
            .command
            .argv
            .iter()
            .any(|arg| arg == "sh" || arg == "-c")
        {
            return Err(ContractError::Unsupported(
                "opaque shell execution is not admitted".into(),
            ));
        }
        Ok(())
    }
}

impl Validate for TaskManifest {
    fn validate(&self) -> Result<(), ContractError> {
        req_schema(&self.schema, "kioku/ci2/tasks/v1", self.revision)?;
        if self.tasks.is_empty() || self.required_task_ids.is_empty() {
            return Err(ContractError::Configuration(
                "tasks and required IDs are required".into(),
            ));
        }
        unique(self.tasks.iter().map(|t| t.id.clone()), "tasks ID")?;
        unique(&self.required_task_ids, "required task ID")?;
        self.tasks.iter().try_for_each(Validate::validate)?;
        self.required_task_ids.iter().try_for_each(Id::validate)?;
        let by_id: std::collections::BTreeMap<_, _> =
            self.tasks.iter().map(|t| (&t.id, t)).collect();
        for id in &self.required_task_ids {
            if !by_id.contains_key(id) || !by_id[id].required {
                return Err(ContractError::Configuration(
                    "task required-set mismatch".into(),
                ));
            }
        }
        for t in &self.tasks {
            if t.required && !self.required_task_ids.contains(&t.id) {
                return Err(ContractError::Configuration(
                    "required task missing in registry".into(),
                ));
            }
            for dep in &t.dependencies {
                if dep.task_id == t.id {
                    return Err(ContractError::Configuration("self dependency".into()));
                }
                let Some(parent) = by_id.get(&dep.task_id) else {
                    return Err(ContractError::Configuration("dependency not found".into()));
                };
                if dep.consumed_output_ids.is_empty() {
                    return Err(ContractError::Configuration(
                        "producer outputs must bind task".into(),
                    ));
                }
                for output_id in &dep.consumed_output_ids {
                    if !parent.outputs.iter().any(|output| &output.id == output_id) {
                        return Err(ContractError::Configuration(
                            "task references unknown producer output".into(),
                        ));
                    }
                }
            }
            for input in &t.inputs {
                if let InputSelector::Producer { task_id, output_id } = input
                    && !t.dependencies.iter().any(|dependency| {
                        &dependency.task_id == task_id
                            && dependency.consumed_output_ids.contains(output_id)
                    })
                {
                    return Err(ContractError::Configuration(
                        "producer input must bind a dependency output".into(),
                    ));
                }
            }
        }
        let mut marks = std::collections::BTreeMap::new();
        for task in &self.tasks {
            check_dependency_cycle(task, &self.tasks, &mut marks, 0)?;
        }
        Ok(())
    }
}
fn check_dependency_cycle(
    task: &TaskSpec,
    tasks: &[TaskSpec],
    active: &mut std::collections::BTreeMap<Id, u8>,
    depth: usize,
) -> Result<(), ContractError> {
    if depth > tasks.len() {
        return Err(ContractError::Configuration("dependency cycle".into()));
    }
    if active.get(&task.id) == Some(&2) {
        return Ok(());
    }
    if active.get(&task.id) == Some(&1) {
        return Err(ContractError::Configuration("dependency cycle".into()));
    }
    active.insert(task.id.clone(), 1);
    for edge in &task.dependencies {
        let next = tasks
            .iter()
            .find(|t| t.id == edge.task_id)
            .ok_or_else(|| ContractError::Configuration("unknown dependency".into()))?;
        check_dependency_cycle(next, tasks, active, depth + 1)?;
    }
    active.insert(task.id.clone(), 2);
    Ok(())
}
impl Validate for NativeObservation {
    fn validate(&self) -> Result<(), ContractError> {
        if self.exit_code.is_some() && self.signal.is_some() {
            return Err(ContractError::Configuration(
                "native exit and signal are mutually exclusive".into(),
            ));
        }
        if self.cpu_complete != self.descendants_drained {
            return Err(ContractError::Configuration(
                "CPU completeness requires drained descendants".into(),
            ));
        }
        if (self.timed_out || (self.exit_code != Some(0)) || self.signal.is_some())
            && self.cpu_complete
            && !self.descendants_drained
        {
            return Err(ContractError::Configuration(
                "post-failure drains absent".into(),
            ));
        }
        if let Some(exit) = self.exit_code
            && !(-128..=255).contains(&exit)
        {
            return Err(ContractError::Configuration("unknown exit range".into()));
        }
        self.stdout_digest.validate()?;
        self.stderr_digest.validate()?;
        for amount in [
            self.stdout_bytes,
            self.stderr_bytes,
            self.wall_ns,
            self.cpu_system_ns,
            self.cpu_user_ns,
            self.started_ms,
            self.ended_ms,
        ] {
            check_range(amount)?;
        }
        if self.ended_ms < self.started_ms {
            return Err(ContractError::Configuration(
                "terminal observation before execution".into(),
            ));
        }
        Ok(())
    }
}
impl Validate for LocalAccepted {
    fn validate(&self) -> Result<(), ContractError> {
        req_schema(&self.schema, "kioku/ci2/local-accepted/v1", self.revision)?;
        if self.state != RunState::Queued {
            return Err(ContractError::Configuration(
                "accepted sync code must be queued".into(),
            ));
        }
        self.request_id.validate()?;
        self.run_id.validate()?;
        self.request_digest.validate()?;
        if self.resume_argv.is_empty() {
            return Err(ContractError::Configuration(
                "acceptance must produce resume argv".into(),
            ));
        }
        Ok(())
    }
}
impl Validate for RunView {
    fn validate(&self) -> Result<(), ContractError> {
        self.run_id.validate()?;
        self.request_id.validate()?;
        self.scope.validate()?;
        self.origin.validate()?;
        self.report_ref
            .as_ref()
            .map_or(Ok(()), Validate::validate)?;
        self.delivery_ref
            .as_ref()
            .map_or(Ok(()), Validate::validate)?;
        self.limits.iter().try_for_each(|r| {
            r.subject_id.as_ref().map_or(Ok(()), Validate::validate)?;
            r.details_ref.as_ref().map_or(Ok(()), Validate::validate)?;
            Ok(())
        })?;
        check_range(self.event_cursor)?;
        let terminal = matches!(
            self.state,
            RunState::Passed | RunState::Failed | RunState::Incomplete | RunState::Cancelled
        );
        if terminal != self.exit_code.is_some() {
            return Err(ContractError::Configuration(
                "only terminal has an exit".into(),
            ));
        }
        if (self.state == RunState::Passed) != matches!(self.exit_code, Some(0)) {
            return Err(ContractError::Configuration(
                "passed requires actual zero".into(),
            ));
        }
        if !terminal && self.report_ref.is_some() {
            return Err(ContractError::Configuration(
                "nonterminal cannot publish complete report ref".into(),
            ));
        }
        Ok(())
    }
}
impl Validate for PrivateNativeState {
    fn validate(&self) -> Result<(), ContractError> {
        self.state_id.validate()?;
        self.project_id.validate()?;
        self.owner_id.validate()?;
        self.adapter_id.validate()?;
        self.recipe_digest.validate()?;
        self.toolchain_digest.validate()?;
        self.layout_digest.validate()?;
        self.current_snapshot_digest
            .as_ref()
            .map_or(Ok(()), Digest::validate)?;
        self.qualification_ref
            .as_ref()
            .map_or(Ok(()), BlobRef::validate)?;
        self.cold_audit_ref
            .as_ref()
            .map_or(Ok(()), BlobRef::validate)?;
        positive(self.last_cold_at_ms)?;
        Ok(())
    }
}
impl Validate for DecisionEvent {
    fn validate(&self) -> Result<(), ContractError> {
        req_schema(&self.schema, "kioku/ci2/decision-event/v1", self.revision)?;
        self.request_id.validate()?;
        self.run_id.validate()?;
        check_range(self.sequence)?;
        positive(self.observed_at_ms)?;
        self.evidence_ref
            .as_ref()
            .map_or(Ok(()), BlobRef::validate)?;
        self.model_input_digest
            .as_ref()
            .map_or(Ok(()), Digest::validate)?;
        if self.model_mode != Some(ModelMode::LogOnly) && self.model_mode.is_some() {
            return Err(ContractError::Unsupported(
                "local model must be off or explicit log only".into(),
            ));
        }
        if self.model_mode.is_none() && self.model_input_digest.is_some() {
            return Err(ContractError::Configuration(
                "model metadata without mode".into(),
            ));
        }
        if self.label.is_some() && self.unresolved.is_some() {
            return Err(ContractError::Configuration(
                "labelled and unresolved contradict".into(),
            ));
        }
        match self.model_mode {
            Some(ModelMode::LogOnly) if self.outcome.is_empty() => Err(
                ContractError::Configuration("observational decision must have outcome".into()),
            ),
            _ => Ok(()),
        }
    }
}
