//! Per-command configuration from the repository root.
use crate::fingerprint::Mode;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot read kioku.toml: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid kioku.toml: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("duplicate command in kioku.toml")]
    Duplicate,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    commands: Vec<ConfiguredCommand>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfiguredCommand {
    argv: Vec<String>,
    mode: ConfigMode,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum ConfigMode {
    Ast,
    Bytes,
}

/// Resolve the mode configured for an exact command line.
///
/// # Errors
/// Returns an error when configuration is unreadable, invalid, or ambiguous.
pub fn mode(root: &Path, argv: &[String]) -> Result<Option<Mode>, Error> {
    let path = root.join("kioku.toml");
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::read_dir(root)?;
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    }
    let text = std::fs::read_to_string(path)?;
    let config: Config = toml::from_str(&text)?;
    let mut matched = config
        .commands
        .iter()
        .filter(|command| command.argv == argv);
    let result = matched.next().map(|command| match command.mode {
        ConfigMode::Ast => Mode::Ast,
        ConfigMode::Bytes => Mode::Bytes,
    });
    if matched.next().is_some() {
        return Err(Error::Duplicate);
    }
    Ok(result)
}
