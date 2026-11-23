//! Closed local run and admission messages.
use super::primitives::{BlobRef, Digest, Id, RelPath};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceMode {
    LocalCooperative,
    Trusted,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityProfile {
    LabCooperative,
    RunnerIsolated,
    RunnerEnforced,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustModel {
    LocalCooperative,
    Disabled,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectFormat {
    Sha1,
    Sha256,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchClass {
    Dev,
    Protected,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginAuthority {
    LocalOperator,
    VerifiedForge,
    Diagnostic,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginKind {
    LocalRegisteredCheckout,
    ForgeEvent,
    DiagnosticUpload,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    Normal,
    Full,
    FullCold,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitClass {
    Agent,
    HumanRequested,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Accepted,
    Capturing,
    Queued,
    Running,
    Collecting,
    Passed,
    Failed,
    Incomplete,
    Cancelled,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Reader,
    Requester,
    Operator,
    Executor,
    Supervisor,
    Publisher,
    Verifier,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisteredRoot {
    pub id: Id,
    pub project_id: Id,
    pub canonical_path: String,
    pub allowed_principals: Vec<Id>,
    pub protected_refs: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientIdentity {
    pub principal_id: Id,
    pub project_ids: Vec<Id>,
    pub roles: Vec<Role>,
    pub trust_scope: Id,
    pub lane_id: Option<Id>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceBootstrap {
    pub schema: String,
    pub revision: u16,
    pub service_id: Id,
    pub mode: ServiceMode,
    pub resource_domain_id: Id,
    pub state_root: String,
    pub socket_path: String,
    pub stop_file: String,
    pub registered_roots: Vec<RegisteredRoot>,
    pub approved_manifest_refs: Vec<BlobRef>,
    pub profiles_ref: BlobRef,
    pub policy_ref: BlobRef,
    pub auth_config_ref: Option<Id>,
    pub capture_slots: u16,
    pub heavy_slots: u8,
    pub max_state_bytes: u64,
    pub max_queue_wait_ms: u64,
    pub drain_grace_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reason {
    pub code: ReasonCode,
    pub subject_id: Option<Id>,
    pub details_ref: Option<BlobRef>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    FullRequired,
    InputChanged,
    NoEvidence,
    ClosureUnknown,
    ProfileMismatch,
    EpochChanged,
    Expired,
    PendingVerification,
    Quarantined,
    NativeSeedUnqualified,
    Corruption,
    SecretInput,
    ExternalEffect,
    LeaderLost,
    ResourceUnavailable,
    DependencyFailed,
    FailFast,
    NativeSkip,
    SelectedOut,
    ModelAbstained,
    Unsupported,
    NotFound,
    InfrastructureError,
    Cancelled,
    InvalidCounts,
    DeliveryUnknown,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OriginClaim {
    LocalRegisteredCheckout { root_id: Id, relative_path: RelPath },
    ForgeEvent { event_id: Id },
    DiagnosticUpload { upload_id: Id },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunOrigin {
    pub kind: OriginKind,
    pub root_id: Option<Id>,
    pub event_id: Option<Id>,
    pub observed_ref: Option<String>,
    pub observed_commit: Option<String>,
    pub snapshot_digest: Digest,
    pub verified_branch_class: BranchClass,
    pub authority: OriginAuthority,
    pub full_ci_publication_allowed: bool,
    pub evidence_ref: BlobRef,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalRequest {
    pub schema: String,
    pub revision: u16,
    pub request_id: Id,
    pub project_id: Id,
    pub origin_claim: OriginClaim,
    pub manifest_digest: Digest,
    pub profile_id: Id,
    pub requested_task_ids: Vec<Id>,
    pub mode: RunMode,
    pub wait_class: WaitClass,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalAccepted {
    pub schema: String,
    pub revision: u16,
    pub request_id: Id,
    pub run_id: Id,
    pub request_digest: Digest,
    pub resume_argv: Vec<String>,
    pub state: RunState,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunView {
    pub run_id: Id,
    pub request_id: Id,
    pub state: RunState,
    pub scope: Id,
    pub report_ref: Option<BlobRef>,
    pub delivery_ref: Option<BlobRef>,
    pub event_cursor: u64,
    pub exit_code: Option<i32>,
    pub origin: RunOrigin,
    pub limits: Vec<Reason>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelMode {
    Off,
    LogOnly,
    Shadow,
    Active,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionEvent {
    pub schema: String,
    pub revision: u16,
    pub request_id: Id,
    pub run_id: Id,
    pub sequence: u64,
    pub observed_at_ms: u64,
    pub outcome: String,
    pub reason: Option<ReasonCode>,
    pub evidence_ref: Option<BlobRef>,
    pub model_mode: Option<ModelMode>,
    pub model_input_digest: Option<Digest>,
    pub label: Option<String>,
    pub unresolved: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultKind {
    NonTest,
    Test,
    OpaqueDiagnostic,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeStateMode {
    PerLane,
    SerialLocalSlot,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivateState {
    Ready,
    InUse,
    Suspect,
    Disabled,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultContract {
    pub id: Id,
    pub kind: ResultKind,
    pub adapter_id: Id,
    pub reporter_version: Id,
    pub inventory_recipe_ref: Option<BlobRef>,
    pub execution_recipe_ref: BlobRef,
    pub identity_rule_digest: Digest,
    pub normalization_policy_ref: BlobRef,
    pub native_skips: Vec<NativeSkipRule>,
    pub expected_empty: bool,
    pub complete_required: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeSkipRule {
    pub case_id: Id,
    pub profile_digest: Digest,
    pub reason_code: Id,
    pub approval_ref: BlobRef,
    pub expires_at_ms: u64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancellationPolicy {
    pub cancellable: bool,
    pub term_grace_ms: u64,
    pub max_drain_ms: u64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceRecipe {
    pub heavy_slots: u16,
    pub capture_slots: u16,
    pub max_state_bytes: u64,
    pub max_stderr_bytes: u64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionPolicy {
    pub result_cache: bool,
    pub single_flight: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvSpec {
    pub name: String,
    pub kind: EnvKind,
    pub value: Option<String>,
    pub capability_ref: Option<Id>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvKind {
    Absent,
    PublicValue,
    SecretCapability,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolIdentity {
    pub name: String,
    pub version: String,
    pub binary_digest: Digest,
    pub installation_digest: Digest,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSpec {
    pub executable: ToolIdentity,
    pub argv: Vec<String>,
    pub cwd: String,
    pub timeout_ms: u64,
    pub recipe_revision: Id,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskDependency {
    pub task_id: Id,
    pub consumed_output_ids: Vec<Id>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSpec {
    pub id: Id,
    pub kind: OutputKind,
    pub path: Option<RelPath>,
    pub required: bool,
    pub max_bytes: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputKind {
    Stdout,
    Stderr,
    Artifact,
    TestInventory,
    TestResults,
    Diagnostics,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputSelector {
    Tree { root: RelPath },
    File { path: RelPath },
    Glob { root: RelPath, pattern: String },
    Absent { path: RelPath },
    Producer { task_id: Id, output_id: Id },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateNativeState {
    pub state_id: Id,
    pub project_id: Id,
    pub owner_scope: NativeStateMode,
    pub owner_id: Id,
    pub adapter_id: Id,
    pub recipe_digest: Digest,
    pub toolchain_digest: Digest,
    pub layout_digest: Digest,
    pub current_snapshot_digest: Option<Digest>,
    pub qualification_ref: Option<BlobRef>,
    pub cold_audit_ref: Option<BlobRef>,
    pub uses_since_cold: u32,
    pub last_cold_at_ms: u64,
    pub state: PrivateState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSpec {
    pub id: Id,
    pub adapter: Id,
    pub command: CommandSpec,
    pub inputs: Vec<InputSelector>,
    pub outputs: Vec<OutputSpec>,
    pub dependencies: Vec<TaskDependency>,
    pub env: Vec<EnvSpec>,
    pub resources: ResourceRecipe,
    pub required: bool,
    pub result_contract: ResultContract,
    pub cancellation: CancellationPolicy,
    pub admission: AdmissionPolicy,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskManifest {
    pub schema: String,
    pub revision: u16,
    pub tasks: Vec<TaskSpec>,
    pub required_task_ids: Vec<Id>,
    pub protected_refs: Vec<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Pending,
    Capturing,
    Queued,
    Running,
    Collecting,
    Passed,
    Failed,
    Incomplete,
    Cancelled,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopMode {
    None,
    Queued,
    TerminateGroup,
    KillGroup,
    Draining,
    Orphaned,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopToken {
    pub state: StopMode,
    pub reason: Option<ReasonCode>,
    pub stop_file: String,
    pub observed_at_ms: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeObservation {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout_digest: Digest,
    pub stderr_digest: Digest,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub wall_ns: u64,
    pub cpu_user_ns: u64,
    pub cpu_system_ns: u64,
    pub cpu_complete: bool,
    pub descendants_drained: bool,
    pub started_ms: u64,
    pub ended_ms: u64,
    pub timed_out: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueItem {
    pub item_id: Id,
    pub run_id: Id,
    pub task_id: Id,
    pub snapshot_digest: Digest,
    pub config_digest: Digest,
    pub actor: ClientIdentity,
    pub priority: u16,
    pub wait_class: WaitClass,
    pub lane_id: Id,
    pub resources: ResourceRecipe,
    pub submitted_ms: u64,
    pub expected_epoch: u64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lease {
    pub lease_id: Id,
    pub item_id: Id,
    pub owner: Id,
    pub epoch: u64,
    pub acquired_ms: u64,
    pub process_identity: Option<ProcessIdentity>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub pgid: u32,
    pub started_ms: u64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageSnapshot {
    pub run_id: Id,
    pub request_id: Id,
    pub execution_id: Id,
    pub phase: RunState,
    pub epoch: u64,
    pub next_sequence: u64,
}
