//! Index SCIP de rust-analyzer : exécution bornée, décodeur protobuf minimal (fait main,
//! borné, sans dépendance) et arêtes « fichier utilisant un symbole → fichier le définissant ».
//!
//! Schéma lu (`scip.proto`) : `Index { documents = 2 }`, `Document { relative_path = 1,
//! occurrences = 2 }`, `Occurrence { symbol = 2, symbol_roles = 3 }`. Tout le reste est
//! sauté. Une entrée tronquée ou incohérente est une erreur (jamais une lecture partielle).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use crate::select::SelectError;

use super::{CommandSpec, run_command};

/// Taille maximale d'un index accepté.
pub const MAX_INDEX_BYTES: usize = 1 << 30;
/// Taille maximale d'une chaîne (chemin, symbole).
const MAX_STRING_BYTES: usize = 1 << 16;
/// Nombre maximal de documents.
const MAX_DOCUMENTS: usize = 500_000;
/// Rôle SCIP « définition ».
const ROLE_DEFINITION: u64 = 1;

/// Un document de l'index : symboles définis et symboles utilisés (sans les locaux ni les
/// segments de module).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScipDocument {
    /// Chemin relatif à la racine du dépôt.
    pub path: String,
    /// Symboles définis dans ce document.
    pub definitions: BTreeSet<String>,
    /// Symboles utilisés (occurrences qui ne sont pas des définitions).
    pub references: BTreeSet<String>,
}

fn invalid(message: impl Into<String>) -> SelectError {
    SelectError::Invalid {
        what: "scip index".to_string(),
        message: message.into(),
    }
}

/// Lit un varint (au plus 10 octets) à partir de `*pos`.
///
/// # Errors
/// Entrée tronquée ou varint trop long.
pub fn read_varint(buf: &[u8], pos: &mut usize) -> Result<u64, SelectError> {
    let mut value: u64 = 0;
    for index in 0..10u32 {
        let byte = *buf.get(*pos).ok_or_else(|| invalid("truncated varint"))?;
        *pos += 1;
        value |= u64::from(byte & 0x7F)
            .checked_shl(7 * index)
            .ok_or_else(|| invalid("varint overflow"))?;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(invalid("varint longer than 10 bytes"))
}

/// Valeur d'un champ protobuf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldValue<'a> {
    /// Varint (type 0).
    Varint(u64),
    /// Champ préfixé par sa longueur (type 2).
    Bytes(&'a [u8]),
    /// Valeur fixe 32 ou 64 bits (types 5 et 1), sautée.
    Fixed,
}

/// Lit le champ suivant : `(numéro, valeur)`.
///
/// # Errors
/// Entrée tronquée, longueur supérieure aux octets restants, type de fil inconnu
/// (groupes compris) ou numéro de champ nul.
pub fn read_field<'a>(
    buf: &'a [u8],
    pos: &mut usize,
) -> Result<(u64, FieldValue<'a>), SelectError> {
    let key = read_varint(buf, pos)?;
    let number = key >> 3;
    if number == 0 {
        return Err(invalid("field number 0"));
    }
    let value = match key & 7 {
        0 => FieldValue::Varint(read_varint(buf, pos)?),
        1 => {
            skip(buf, pos, 8)?;
            FieldValue::Fixed
        }
        2 => {
            let length = read_varint(buf, pos)?;
            let length = usize::try_from(length).map_err(|_| invalid("length overflow"))?;
            let end = pos
                .checked_add(length)
                .filter(|end| *end <= buf.len())
                .ok_or_else(|| invalid("length exceeds remaining bytes"))?;
            let slice = &buf[*pos..end];
            *pos = end;
            FieldValue::Bytes(slice)
        }
        5 => {
            skip(buf, pos, 4)?;
            FieldValue::Fixed
        }
        other => return Err(invalid(format!("unsupported wire type {other}"))),
    };
    Ok((number, value))
}

fn skip(buf: &[u8], pos: &mut usize, count: usize) -> Result<(), SelectError> {
    let end = pos
        .checked_add(count)
        .filter(|end| *end <= buf.len())
        .ok_or_else(|| invalid("truncated fixed field"))?;
    *pos = end;
    Ok(())
}

fn string_of(bytes: &[u8]) -> Result<String, SelectError> {
    if bytes.len() > MAX_STRING_BYTES {
        return Err(invalid("string longer than the bound"));
    }
    std::str::from_utf8(bytes)
        .map(str::to_string)
        .map_err(|_| invalid("string is not utf-8"))
}

/// Décode un index SCIP complet.
///
/// # Errors
/// Octets tronqués ou incohérents, taille hors borne.
pub fn decode_index(bytes: &[u8]) -> Result<Vec<ScipDocument>, SelectError> {
    if bytes.len() > MAX_INDEX_BYTES {
        return Err(invalid("index larger than the bound"));
    }
    let mut documents = Vec::new();
    let mut pos = 0;
    while pos < bytes.len() {
        let (number, value) = read_field(bytes, &mut pos)?;
        if let (2, FieldValue::Bytes(document)) = (number, value) {
            if documents.len() >= MAX_DOCUMENTS {
                return Err(invalid("too many documents"));
            }
            documents.push(decode_document(document)?);
        }
    }
    Ok(documents)
}

fn decode_document(bytes: &[u8]) -> Result<ScipDocument, SelectError> {
    let mut document = ScipDocument::default();
    let mut pos = 0;
    while pos < bytes.len() {
        match read_field(bytes, &mut pos)? {
            (1, FieldValue::Bytes(path)) => document.path = string_of(path)?,
            (2, FieldValue::Bytes(occurrence)) => decode_occurrence(occurrence, &mut document)?,
            _ => {}
        }
    }
    Ok(document)
}

fn decode_occurrence(bytes: &[u8], document: &mut ScipDocument) -> Result<(), SelectError> {
    let mut symbol: Option<&[u8]> = None;
    let mut roles = 0u64;
    let mut pos = 0;
    while pos < bytes.len() {
        match read_field(bytes, &mut pos)? {
            (2, FieldValue::Bytes(raw)) => symbol = Some(raw),
            (3, FieldValue::Varint(value)) => roles = value,
            _ => {}
        }
    }
    let Some(raw) = symbol else {
        return Ok(());
    };
    if raw.is_empty() || raw.starts_with(b"local ") || raw.ends_with(b"/") {
        // Locaux et segments de module (espaces de noms) : pas une dépendance de code.
        return Ok(());
    }
    let symbol = string_of(raw)?;
    if roles & ROLE_DEFINITION != 0 {
        document.definitions.insert(symbol);
    } else {
        document.references.insert(symbol);
    }
    Ok(())
}

/// Arêtes `utilisateur → définisseur` : pour chaque symbole utilisé dans `p`, chaque
/// fichier `d ≠ p` qui le définit et pour lequel `allowed(p, d)` est vrai.
///
/// Les documents de même chemin sont fusionnés (un fichier compilé dans plusieurs crates
/// apparaît plusieurs fois).
#[must_use]
pub fn file_edges(
    documents: &[ScipDocument],
    allowed: &dyn Fn(&str, &str) -> bool,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut definers: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut users: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for document in documents {
        for symbol in &document.definitions {
            definers.entry(symbol).or_default().insert(&document.path);
        }
        for symbol in &document.references {
            users.entry(&document.path).or_default().insert(symbol);
        }
    }
    let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (user, symbols) in users {
        for symbol in symbols {
            for definer in definers.get(symbol).into_iter().flatten() {
                if *definer != user && allowed(user, definer) {
                    edges
                        .entry(user.to_string())
                        .or_default()
                        .insert((*definer).to_string());
                }
            }
        }
    }
    edges
}

/// Lance `rust-analyzer scip` sur `repo` et retourne les octets de l'index.
///
/// `config` (optionnel) est le contenu JSON passé à `--config-path` (features cargo).
///
/// # Errors
/// Exécutable introuvable, code de sortie non nul, délai dépassé
/// ([`SelectError::Timeout`]), index illisible ou trop gros.
pub fn run_rust_analyzer(
    executable: &Path,
    repo: &Path,
    config: Option<&str>,
    timeout: Duration,
) -> Result<Vec<u8>, SelectError> {
    let scratch = scratch_dir()?;
    let output = scratch.join("index.scip");
    let mut args: Vec<String> = vec![
        "scip".to_string(),
        repo.display().to_string(),
        "--output".to_string(),
        output.display().to_string(),
    ];
    if let Some(config) = config {
        let config_path = scratch.join("ra-config.json");
        std::fs::write(&config_path, config).map_err(|error| SelectError::Io {
            path: config_path.display().to_string(),
            message: error.to_string(),
        })?;
        args.push("--config-path".to_string());
        args.push(config_path.display().to_string());
    }
    let result = run_command(&CommandSpec {
        program: executable.as_os_str().to_owned(),
        args: args.iter().map(Into::into).collect(),
        cwd: repo.to_path_buf(),
        timeout,
        stdout: None,
        label: "rust-analyzer scip".to_string(),
    })
    .and_then(|()| {
        let metadata = std::fs::metadata(&output).map_err(|error| SelectError::Io {
            path: output.display().to_string(),
            message: error.to_string(),
        })?;
        if metadata.len() > MAX_INDEX_BYTES as u64 {
            return Err(invalid("index larger than the bound"));
        }
        std::fs::read(&output).map_err(|error| SelectError::Io {
            path: output.display().to_string(),
            message: error.to_string(),
        })
    });
    // Nettoyage de notre propre dossier temporaire (créé vide, deux fichiers au plus).
    let _ = std::fs::remove_file(&output);
    let _ = std::fs::remove_file(scratch.join("ra-config.json"));
    let _ = std::fs::remove_dir(&scratch);
    result
}

/// Dossier temporaire unique (hors dépôt) pour la sortie de rust-analyzer.
fn scratch_dir() -> Result<std::path::PathBuf, SelectError> {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = format!(
        "kioku-select-scip-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let dir = std::env::temp_dir().join(unique);
    std::fs::create_dir(&dir).map_err(|error| SelectError::Io {
        path: dir.display().to_string(),
        message: error.to_string(),
    })?;
    Ok(dir)
}
