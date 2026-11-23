//! Per-run artifact-path checks; caller keeps all raw data private.
use super::primitives::ContractError;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;

/// Creates one private, new directory for a run.
///
/// The parent must be an absolute, non-symlink directory; an absent parent is
/// created private. This checks path regularity and modes only: it does not
/// open, write or seal anything, and later writers must still keep their own
/// no-follow discipline.
///
/// # Errors
/// Missing/private/uncreatable paths, existing artifacts, or non-directory
/// components fail closed.
#[cfg(unix)]
pub fn prepare_one(path: &Path) -> Result<(), ContractError> {
    if !path.is_absolute() {
        return Err(ContractError::Configuration(
            "artifact path must be absolute".into(),
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| ContractError::Configuration("artifact path without parent".into()))?;
    match std::fs::symlink_metadata(parent) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(ContractError::Configuration(
                "artifact parent is not a directory".into(),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(parent)
                .map_err(|error| ContractError::Configuration(error.to_string()))?;
        }
        Err(error) => return Err(ContractError::Configuration(error.to_string())),
    }
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .map_err(|error| ContractError::Configuration(error.to_string()))?;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| ContractError::Configuration(error.to_string()))?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(ContractError::Configuration(
            "artifact path not private".into(),
        ));
    }
    Ok(())
}

/// Other platforms are explicitly unqualified.
///
/// # Errors
/// Always unsupported until the native-session drain contract is ported and measured.
#[cfg(not(unix))]
pub fn prepare_one(_path: &Path) -> Result<(), ContractError> {
    Err(ContractError::Unsupported(
        "private native runner platform".into(),
    ))
}
