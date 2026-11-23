//! C04 task-key material; this never authorizes verdict reuse.
use serde::Serialize;

#[derive(Clone, Debug, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProducerOutputRef {
    pub task_id: String,
    pub output_id: String,
    pub digest: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct TaskKeyMaterial {
    pub project_id: String,
    pub trust_scope: String,
    pub task_id: String,
    pub command_digest: String,
    pub inputs_manifest_digest: String,
    pub global_inputs_digest: String,
    pub producer_outputs: Vec<ProducerOutputRef>,
    pub env_digest: String,
    pub installation_digest: String,
    pub profile_digest: String,
    pub resource_recipe_digest: String,
    pub outputs_spec_digest: String,
    pub effects_qualification_digest: String,
    pub adapter_semantics_version: String,
}

fn id(value: &str) -> bool {
    let mut chars = value.bytes();
    value.len() <= 128
        && chars.next().is_some_and(|x| x.is_ascii_alphanumeric())
        && chars.all(|x| x.is_ascii_alphanumeric() || matches!(x, b'.' | b'_' | b'-'))
}
fn digest(value: &str) -> bool {
    let body = value
        .strip_prefix("b3:")
        .or_else(|| value.strip_prefix("sha256:"));
    body.is_some_and(|b| {
        b.len() == 64
            && b.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    })
}

/// Canonicalize an ASCII-only C04 material object and compute a domain-separated key.
///
/// # Errors
/// Returns an error for malformed identities, digests or ambiguous producer references. No verdict reuse is authorized.
pub fn task_key(material: &TaskKeyMaterial) -> Result<String, &'static str> {
    if ![
        &material.project_id,
        &material.trust_scope,
        &material.task_id,
        &material.adapter_semantics_version,
    ]
    .iter()
    .all(|x| id(x))
    {
        return Err("invalid identity");
    }
    if ![
        &material.command_digest,
        &material.inputs_manifest_digest,
        &material.global_inputs_digest,
        &material.env_digest,
        &material.installation_digest,
        &material.profile_digest,
        &material.resource_recipe_digest,
        &material.outputs_spec_digest,
        &material.effects_qualification_digest,
    ]
    .iter()
    .all(|x| digest(x))
    {
        return Err("invalid digest");
    }
    let mut canonical = material.clone();
    canonical.producer_outputs.sort_unstable();
    for item in &canonical.producer_outputs {
        if !id(&item.task_id) || !id(&item.output_id) || !digest(&item.digest) {
            return Err("invalid producer output");
        }
    }
    if canonical
        .producer_outputs
        .windows(2)
        .any(|pair| pair[0].task_id == pair[1].task_id && pair[0].output_id == pair[1].output_id)
    {
        return Err("duplicate producer output");
    }
    let mut fields = std::collections::BTreeMap::new();
    for (name, value) in [
        ("project_id", &canonical.project_id),
        ("trust_scope", &canonical.trust_scope),
        ("task_id", &canonical.task_id),
        ("command_digest", &canonical.command_digest),
        ("inputs_manifest_digest", &canonical.inputs_manifest_digest),
        ("global_inputs_digest", &canonical.global_inputs_digest),
        ("env_digest", &canonical.env_digest),
        ("installation_digest", &canonical.installation_digest),
        ("profile_digest", &canonical.profile_digest),
        ("resource_recipe_digest", &canonical.resource_recipe_digest),
        ("outputs_spec_digest", &canonical.outputs_spec_digest),
        (
            "effects_qualification_digest",
            &canonical.effects_qualification_digest,
        ),
        (
            "adapter_semantics_version",
            &canonical.adapter_semantics_version,
        ),
    ] {
        fields.insert(
            name,
            serde_json::to_string(value).map_err(|_| "encoding failure")?,
        );
    }
    let outputs = canonical
        .producer_outputs
        .iter()
        .map(|p| {
            let digest = serde_json::to_string(&p.digest).map_err(|_| "encoding failure")?;
            let output = serde_json::to_string(&p.output_id).map_err(|_| "encoding failure")?;
            let task = serde_json::to_string(&p.task_id).map_err(|_| "encoding failure")?;
            Ok(format!(
                "{{\"digest\":{digest},\"output_id\":{output},\"task_id\":{task}}}"
            ))
        })
        .collect::<Result<Vec<_>, &'static str>>()?;
    fields.insert("producer_outputs", format!("[{}]", outputs.join(",")));
    fields.insert("revision", "1".to_owned());
    fields.insert("schema", "\"kioku/ci2/task-key/v1\"".to_owned());
    let pairs = fields
        .iter()
        .map(|(key, value)| format!("\"{key}\":{value}"))
        .collect::<Vec<_>>()
        .join(",");
    let bytes = format!("{{{pairs}}}").into_bytes();
    let mut hash = blake3::Hasher::new();
    hash.update(b"kioku/ci2/task-key/v1\0");
    hash.update(&bytes);
    Ok(format!("b3:{}", hash.finalize().to_hex()))
}
