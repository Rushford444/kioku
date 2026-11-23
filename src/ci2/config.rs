//! Approved local operator configuration, parsed independently of execution.
use super::domain::{
    ContractError, Digest, Id, RegisteredRoot, RelPath, ServiceBootstrap, TaskManifest, TaskSpec,
    Validate, decode_bounded,
};
use sha2::Digest as _;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("configuration file: {0}")]
    Io(#[from] std::io::Error),
    #[error("configuration contract: {0}")]
    Contract(#[from] ContractError),
    #[error("invalid approved configuration: {0}")]
    Rule(String),
}

/// Reads and validates an operator bootstrap before anything runs.
///
/// # Errors
/// A missing/non-regular file, malformed JSON, wrong wire or bad bootstrap fails closed.
pub fn read_bootstrap(path: &Path) -> Result<ServiceBootstrap, ConfigError> {
    let metadata = fs::symlink_metadata(path).map_err(ConfigError::Io)?;
    if !metadata.is_file() {
        return Err(ConfigError::Rule(
            "configuration must be a regular file".into(),
        ));
    }
    if metadata.len() > 4 * 1024 * 1024 {
        return Err(ConfigError::Rule("configuration larger than 4 MiB".into()));
    }
    let data = fs::read(path)?;
    let bootstrap: ServiceBootstrap = decode_bounded(&data)?;
    if bootstrap.auth_config_ref.is_some() {
        return Err(ConfigError::Rule(
            "authentication capability is unsupported".into(),
        ));
    }
    for reference in &bootstrap.approved_manifest_refs {
        if reference.size_bytes > 64 * 1024 * 1024 {
            return Err(ConfigError::Rule(
                "manifest reference exceeds configured cap".into(),
            ));
        }
    }
    Ok(bootstrap)
}

// This is a cooperative filesystem check, not a hostile-writer sandbox. The final
// file is opened without following a symlink; callers must keep roots private.
fn checked_root(path: &Path) -> Result<PathBuf, ConfigError> {
    if !path.is_absolute() || path.canonicalize()? != path || !path.is_dir() {
        return Err(ConfigError::Rule("registered root is not canonical".into()));
    }
    reject_protected(path)?;
    Ok(path.to_path_buf())
}

#[allow(clippy::case_sensitive_file_extension_comparisons)]
// Component names are lowercased before extension tests.
fn reject_protected(path: &Path) -> Result<(), ConfigError> {
    for part in path.components() {
        let name = part.as_os_str().to_string_lossy().to_ascii_lowercase();
        if name == "qa"
            || name.starts_with(".env")
            || name.ends_with(".dump")
            || name.ends_with(".sql")
            || name.ends_with(".sqlite")
            || name.ends_with(".sqlite3")
        {
            return Err(ConfigError::Rule("protected path excluded".into()));
        }
    }
    Ok(())
}

// The manifest reader is only reached from the Linux capture path; on other
// targets the parameter and the `Read` import are unused. Scoped to non-Linux.
#[cfg_attr(not(target_os = "linux"), allow(unused_variables, unused_imports))]
fn read_approved_manifest(
    bootstrap: &ServiceBootstrap,
    root: &Path,
    relative: &Path,
) -> Result<TaskManifest, ConfigError> {
    use std::io::Read;
    use std::path::Component;
    let root = checked_root(root)?;
    if relative
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(ConfigError::Rule("invalid manifest path".into()));
    }
    reject_protected(relative)?;
    let mut path = root;
    for component in relative.components() {
        path.push(component.as_os_str());
        if fs::symlink_metadata(&path)?.file_type().is_symlink() {
            return Err(ConfigError::Rule("manifest symlink forbidden".into()));
        }
    }
    #[cfg(target_os = "linux")]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        // Linux O_NOFOLLOW: reject replacement of the final component with a symlink.
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(0o400_000)
            .open(&path)?
    };
    #[cfg(not(target_os = "linux"))]
    return Err(ConfigError::Rule(
        "safe manifest opening unsupported on this platform".into(),
    ));
    #[cfg(target_os = "linux")]
    {
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > 64 * 1024 * 1024 {
            return Err(ConfigError::Rule(
                "manifest is not a bounded regular file".into(),
            ));
        }
        let mut bytes = Vec::new();
        file.take(64 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 64 * 1024 * 1024 {
            return Err(ConfigError::Rule("manifest exceeded size cap".into()));
        }
        let actual = Digest::sha256("kioku/ci2/workspace-file/v1", &bytes)?;
        if !bootstrap.approved_manifest_refs.iter().any(|reference| {
            reference.digest == actual && reference.size_bytes == bytes.len() as u64
        }) {
            return Err(ConfigError::Rule(
                "manifest digest or size not approved".into(),
            ));
        }
        let manifest: TaskManifest = decode_bounded(&bytes)?;
        if manifest
            .tasks
            .iter()
            .flat_map(|task| &task.env)
            .any(|env| env.capability_ref.is_some())
        {
            return Err(ConfigError::Rule("secret capability is unsupported".into()));
        }
        Ok(manifest)
    }
}

/// Reads an approved manifest from its registered source root.
///
/// # Errors
/// Rejects unregistered, protected, linked, unapproved or malformed paths.
pub fn read_manifest(
    bootstrap: &ServiceBootstrap,
    root_id: &Id,
    relative: &RelPath,
) -> Result<TaskManifest, ConfigError> {
    let root = bootstrap
        .registered_roots
        .iter()
        .find(|root| &root.id == root_id)
        .ok_or_else(|| ConfigError::Rule("unregistered project root".into()))?;
    read_approved_manifest(
        bootstrap,
        Path::new(&root.canonical_path),
        Path::new(relative.as_str()),
    )
}

/// Binds the relative command identity to the exact approved executable in the registered checkout.
///
/// # Errors
/// Refuses absent executable, symlink exit, wrong path or digest mismatch.
pub fn resolve_approved_executable(
    root: &RegisteredRoot,
    task: &TaskSpec,
) -> Result<PathBuf, ConfigError> {
    // A project recipe may not silently name a new system authority by absolute free-form path.
    let lexical = Path::new(&root.canonical_path)
        .join("tools/ci2")
        .join(&task.command.executable.name);
    let registered_base = Path::new(&root.canonical_path)
        .canonicalize()
        .map_err(ConfigError::Io)?;
    let path = lexical.canonicalize().map_err(ConfigError::Io)?;
    if !path.starts_with(&registered_base) || !path.is_file() {
        return Err(ConfigError::Rule(
            "approved executable not regular inside root".into(),
        ));
    }
    let content = fs::read(&path).map_err(ConfigError::Io)?;
    let digest = Digest::new(&format!("sha256:{:x}", sha2::Sha256::digest(&content)))?;
    if digest != task.command.executable.binary_digest {
        return Err(ConfigError::Rule("tool binary hash mismatch".into()));
    }
    if task.command.executable.installation_digest != digest {
        return Err(ConfigError::Rule(
            "local tool packaging digest diverges".into(),
        ));
    }
    if !registered_base.join(&lexical).is_file() {
        return Err(ConfigError::Rule(
            "tool path not in approved checkout context".into(),
        ));
    }
    Ok(path)
}

/// Read only the approved root-relative manifest with an exact SHA-256 pin.
///
/// # Errors
/// Unregistered source roots, bad files, undeclared digest or malformed manifests are rejected before any run begins.
pub fn approved_workspace_manifest(
    bootstrap: &ServiceBootstrap,
    source: &Path,
) -> Result<TaskManifest, ConfigError> {
    let canonical = checked_root(source)?;
    if !bootstrap
        .registered_roots
        .iter()
        .any(|root| Path::new(&root.canonical_path) == canonical)
    {
        return Err(ConfigError::Rule("source root is not registered".into()));
    }
    read_approved_manifest(bootstrap, &canonical, Path::new("ci2-manifest.json"))
}

/// Returns the durable reference consumed by resume commands and replay.
/// # Errors
/// Private, regular file, directory permissions and write/sync failures are preserved.
pub fn persist_bootstrap(config: &ServiceBootstrap) -> Result<PathBuf, ConfigError> {
    use std::io::{Read, Write};
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    config.validate()?;
    let root = Path::new(&config.state_root);
    if !root.is_dir() {
        fs::create_dir(root)?;
    }
    fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
    let path = root.join("bootstrap.json");
    let bytes = serde_json::to_vec(config)
        .map_err(|_| ConfigError::Rule("cannot encode bootstrap".into()))?;
    let read_durable = || -> Result<Vec<u8>, ConfigError> {
        if !fs::symlink_metadata(&path)?.is_file() {
            return Err(ConfigError::Rule(
                "durable bootstrap must be a regular file".into(),
            ));
        }
        let mut file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        if !file.metadata()?.is_file() {
            return Err(ConfigError::Rule(
                "durable bootstrap must be a regular file".into(),
            ));
        }
        let mut existing = Vec::new();
        file.read_to_end(&mut existing)?;
        Ok(existing)
    };
    if path.exists() && read_durable()? != bytes {
        return Err(ConfigError::Rule(
            "durable bootstrap differs; do not alias this service identity".into(),
        ));
    }
    if !path.exists() {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::File::open(root)?.sync_all()?;
    }
    if read_durable()? != bytes {
        return Err(ConfigError::Rule(
            "durable bootstrap verification failed".into(),
        ));
    }
    Ok(path)
}

/// Publishes `.ci2/bootstrap.json` only inside the explicitly approved private cooperation home.
/// # Errors
/// Missing/untrusted `KIOKU_CI2`, cycles, other service mappings and durable I/O failures reject the alias.
#[cfg(unix)]
pub fn publish_local_default(config: &ServiceBootstrap) -> Result<PathBuf, ConfigError> {
    use std::{
        io::{Read, Write},
        os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    };
    let home = std::env::var_os("KIOKU_CI2").ok_or_else(|| {
        ConfigError::Rule("KIOKU_CI2 private cooperation home was not provided".into())
    })?;
    let home = PathBuf::from(home);
    if !home.is_absolute() || !home.is_dir() {
        return Err(ConfigError::Rule(
            "cooperation home must be an existing absolute directory".into(),
        ));
    }
    let directory = home.join(".ci2");
    if directory.exists() {
        let meta = fs::symlink_metadata(&directory)?;
        if !meta.is_dir() || meta.permissions().mode() & 0o077 != 0 {
            return Err(ConfigError::Rule(
                "local .ci2 default directory is not private".into(),
            ));
        }
    } else {
        fs::DirBuilder::new().mode(0o700).create(&directory)?;
    }
    let path = directory.join("bootstrap.json");
    let bytes = serde_json::to_vec(config)
        .map_err(|_| ConfigError::Rule("cannot encode local default".into()))?;
    let file = match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err(ConfigError::Rule(
                    "local default must be a regular file".into(),
                ));
            }
            let mut file = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)?;
            let mut existing = Vec::new();
            file.read_to_end(&mut existing)?;
            if existing != bytes {
                return Err(ConfigError::Rule(
                    "local default already belongs to another service".into(),
                ));
            }
            file
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::File::open(directory)?.sync_all()?;
            file
        }
        Err(error) => return Err(error.into()),
    };
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(path)
}
