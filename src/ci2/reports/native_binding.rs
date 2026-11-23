//! Bind already-closed native libtest streams to their declared task pair.
//! Root must verify durable journal stream digests/lengths and declared report-file
//! equality before setting `streams_verified`. This module never reads report paths.
use crate::ci2::{
    domain::{
        BlobRef, CommandSpec, ContractError, Digest, InputSelector, NativeObservation, OutputKind,
        ResultKind, TaskSpec, Validate,
    },
    reports::{NativeEvaluation, REPORTER_VERSION, TaskReport, evaluate_with_context},
    snapshot::Snapshot,
};

pub(crate) struct NativeBinding<'a> {
    pub inventory: &'a TaskSpec,
    pub execution: &'a TaskSpec,
    pub inventory_snapshot: &'a Snapshot,
    pub execution_snapshot: &'a Snapshot,
    /// Includes native closure, successful exit and complete inventory collection.
    pub inventory_native_healthy: bool,
}

pub(crate) struct EvaluationOptions<'a> {
    /// An independently established profile; never a hash of a profile name.
    pub context: NativeEvaluation<'a>,
    /// Includes supervisor/collection health not expressible in `NativeObservation`.
    pub execution_native_healthy: bool,
    /// Root has verified each declared report equals its own closed native stdout,
    /// and these bytes equal the corresponding durable journal stream.
    pub streams_verified: bool,
}

/// Canonical rule implemented by `reports::case_id(project, adapter, raw, "")`.
pub(crate) const IDENTITY_RULE: &[u8] = b"kioku/ci2/libtest-pretty-v2";

pub(crate) fn identity_ref() -> Result<Digest, ContractError> {
    Digest::sha256("kioku/ci2/libtest-case-identity/v1", IDENTITY_RULE)
}

fn rejected(reason: &str) -> ContractError {
    ContractError::Unsupported(format!("native libtest binding: {reason}"))
}

fn recipe(command: &CommandSpec) -> Result<BlobRef, ContractError> {
    let bytes = serde_json::to_vec(command).map_err(|_| rejected("recipe serialization"))?;
    Ok(BlobRef {
        digest: Digest::sha256("kioku/ci2/libtest-recipe/v1", &bytes)?,
        size_bytes: u64::try_from(bytes.len()).map_err(|_| rejected("recipe size"))?,
        media_type: "application/json".into(),
    })
}

fn snapshot_digest(snapshot: &Snapshot) -> Result<Digest, ContractError> {
    let bytes = serde_json::to_vec(snapshot).map_err(|_| rejected("snapshot serialization"))?;
    Digest::sha256("kioku/ci2/libtest-snapshot/v1", &bytes)
}

fn report_output(
    task: &TaskSpec,
    kind: OutputKind,
) -> Result<&crate::ci2::domain::OutputSpec, ContractError> {
    let mut outputs = task.outputs.iter().filter(|output| output.kind == kind);
    let output = outputs
        .next()
        .ok_or_else(|| rejected("missing report output"))?;
    if outputs.next().is_some() || !output.required || output.path.is_none() {
        return Err(rejected("ambiguous or optional report output"));
    }
    Ok(output)
}

impl NativeBinding<'_> {
    /// Proves topology and recipe identity, not stream provenance or file equality.
    pub(crate) fn validate(&self) -> Result<(), ContractError> {
        let inventory = self.inventory;
        let execution = self.execution;
        inventory.validate()?;
        execution.validate()?;
        if !self.inventory_native_healthy
            || inventory.id == execution.id
            || inventory.result_contract.kind != ResultKind::NonTest
            || execution.result_contract.kind != ResultKind::Test
            || inventory.adapter != execution.adapter
            || !inventory.result_contract.native_skips.is_empty()
            || !execution.result_contract.native_skips.is_empty()
        {
            return Err(rejected("task kind, native health or skips"));
        }
        if inventory.command.argv != ["--list", "--format", "terse"]
            || execution.command.argv != ["--format", "pretty", "--test-threads", "1"]
        {
            return Err(rejected("unsupported argv"));
        }
        if inventory.command.executable != execution.command.executable {
            return Err(rejected("tool identity mismatch"));
        }
        if inventory.command.cwd != execution.command.cwd {
            return Err(rejected("cwd mismatch"));
        }
        if inventory.command.timeout_ms != execution.command.timeout_ms {
            return Err(rejected("timeout mismatch"));
        }
        if inventory.command.recipe_revision != execution.command.recipe_revision {
            return Err(rejected("recipe revision mismatch"));
        }
        if inventory.resources != execution.resources {
            return Err(rejected("resource mismatch"));
        }
        if inventory.env != execution.env {
            return Err(rejected("environment mismatch"));
        }
        let inventory_recipe = recipe(&inventory.command)?;
        if inventory.result_contract.reporter_version.as_str() != REPORTER_VERSION
            || execution.result_contract.reporter_version.as_str() != REPORTER_VERSION
            || inventory.result_contract.inventory_recipe_ref.is_some()
            || inventory.result_contract.execution_recipe_ref != inventory_recipe
            || execution.result_contract.inventory_recipe_ref.as_ref() != Some(&inventory_recipe)
            || execution.result_contract.execution_recipe_ref != recipe(&execution.command)?
            || inventory.result_contract.identity_rule_digest != identity_ref()?
            || execution.result_contract.identity_rule_digest != identity_ref()?
            || inventory.result_contract.normalization_policy_ref != inventory_recipe
            || execution.result_contract.normalization_policy_ref
                != execution.result_contract.execution_recipe_ref
        {
            return Err(rejected("recipe reference or reporter mismatch"));
        }
        let output = report_output(inventory, OutputKind::TestInventory)?;
        report_output(execution, OutputKind::TestResults)?;
        if inventory
            .outputs
            .iter()
            .any(|o| o.kind == OutputKind::TestResults)
            || execution
                .outputs
                .iter()
                .any(|o| o.kind == OutputKind::TestInventory)
            || !inventory.dependencies.is_empty()
            || execution.dependencies.len() != 1
        {
            return Err(rejected("report ownership or dependency count"));
        }
        let dependency = &execution.dependencies[0];
        if dependency.task_id != inventory.id
            || dependency.consumed_output_ids != [output.id.clone()]
        {
            return Err(rejected("inventory dependency"));
        }
        let mut workspace = Vec::new();
        let mut producers = 0;
        for input in &execution.inputs {
            if let InputSelector::Producer { task_id, output_id } = input {
                producers += 1;
                if task_id != &inventory.id || output_id != &output.id {
                    return Err(rejected("unknown producer"));
                }
            } else {
                workspace.push(input);
            }
        }
        if producers != 1
            || inventory
                .inputs
                .iter()
                .any(|input| matches!(input, InputSelector::Producer { .. }))
            || workspace != inventory.inputs.iter().collect::<Vec<_>>()
            || snapshot_digest(self.inventory_snapshot)?
                != snapshot_digest(self.execution_snapshot)?
        {
            return Err(rejected("input declarations or captured snapshot mismatch"));
        }
        Ok(())
    }
}

/// Root supplies only durable journal bytes, never arbitrary report-file contents.
/// Rejected bindings must become Incomplete in the coordinator, not a fallback pass.
pub(crate) fn evaluate_bound(
    binding: &NativeBinding<'_>,
    native: &NativeObservation,
    inventory: &[u8],
    results: &[u8],
    stderr: &[u8],
    options: &EvaluationOptions<'_>,
) -> Result<TaskReport, ContractError> {
    binding.validate()?;
    native.validate()?;
    if !options.streams_verified {
        return Err(rejected("unverified streams"));
    }
    let context = NativeEvaluation {
        project: options.context.project,
        profile: options.context.profile,
        now_ms: options.context.now_ms,
        complete: options.context.complete && options.execution_native_healthy,
        cancelled: options.context.cancelled,
    };
    Ok(evaluate_with_context(
        &binding.execution.result_contract,
        native,
        inventory,
        results,
        stderr,
        &context,
    ))
}
