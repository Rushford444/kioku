//! Lot L1 : changements entre la base et l'arbre de travail.
//!
//! `collect` rassemble ce que `git diff --name-status -M -z <base>` voit (commits, index
//! et arbre de travail, fichiers suivis) et les fichiers non suivis non ignorés
//! (`git ls-files --others --exclude-standard -z`, comptés `Added`). Base inconnue, git
//! absent, sortie illisible ou chemin non UTF-8 : erreur (S3, l'appelant élargit à tout).

use std::path::Path;
use std::time::Duration;

use super::base::{invalid, io_error, run_command, validate_rel_path};
use super::{Change, ChangeSet, RelPath, SelectError};

const GIT_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_GIT_OUTPUT: usize = 256 * 1024 * 1024;

fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>, SelectError> {
    let mut full = vec!["--no-optional-locks"];
    full.extend_from_slice(args);
    run_command("git", &full, repo, GIT_TIMEOUT, MAX_GIT_OUTPUT)
}

fn rel_path(bytes: &[u8], what: &str) -> Result<RelPath, SelectError> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| invalid(what, "chemin non UTF-8 (refusé)"))?;
    validate_rel_path(text).map_err(|message| invalid(what, message))?;
    Ok(text.to_owned())
}

/// Découpe une sortie `-z` en champs ; le dernier champ doit être terminé par NUL.
fn nul_fields<'a>(raw: &'a [u8], what: &str) -> Result<Vec<&'a [u8]>, SelectError> {
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    if raw.last() != Some(&0) {
        return Err(invalid(what, "terminaison NUL manquante"));
    }
    let mut fields: Vec<&[u8]> = raw.split(|byte| *byte == 0).collect();
    fields.pop();
    Ok(fields)
}

/// Lit la sortie de `git diff --name-status -M -z`.
///
/// `A` et `C<score>` → [`Change::Added`] (chemin de destination pour une copie), `M` et
/// `T` → [`Change::Modified`], `D` → [`Change::Deleted`], `R<score>` →
/// [`Change::Renamed`]. Tout autre statut (`U` non fusionné, `X`, `B`…) est refusé.
///
/// # Errors
///
/// Statut inconnu, champ manquant, terminaison NUL absente, chemin non UTF-8 ou non
/// normalisé, renommage d'un chemin vers lui-même.
pub fn parse_name_status_z(raw: &[u8]) -> Result<Vec<Change>, SelectError> {
    let what = "git diff --name-status";
    let fields = nul_fields(raw, what)?;
    let mut iter = fields.into_iter();
    let mut changes = Vec::new();
    while let Some(status) = iter.next() {
        let letter = *status.first().ok_or_else(|| invalid(what, "statut vide"))?;
        let score = &status[1..];
        let two_paths = matches!(letter, b'R' | b'C');
        let score_ok = if two_paths {
            score.len() <= 3 && score.iter().all(u8::is_ascii_digit)
        } else {
            score.is_empty()
        };
        if !score_ok || !matches!(letter, b'A' | b'M' | b'T' | b'D' | b'R' | b'C') {
            return Err(invalid(
                what,
                format!("statut refusé : {}", String::from_utf8_lossy(status)),
            ));
        }
        let mut next_path = || {
            iter.next()
                .ok_or_else(|| invalid(what, "chemin manquant"))
                .and_then(|bytes| rel_path(bytes, what))
        };
        let change = match letter {
            b'A' => Change::Added { path: next_path()? },
            b'M' | b'T' => Change::Modified { path: next_path()? },
            b'D' => Change::Deleted { path: next_path()? },
            b'C' => {
                let _from = next_path()?;
                Change::Added { path: next_path()? }
            }
            _ => {
                let from = next_path()?;
                let to = next_path()?;
                if from == to {
                    return Err(invalid(what, format!("renommage vers lui-même : {from}")));
                }
                Change::Renamed { from, to }
            }
        };
        changes.push(change);
    }
    Ok(changes)
}

/// Lit la sortie de `git ls-files --others --exclude-standard -z` : chaque chemin devient
/// [`Change::Added`]. Un dépôt imbriqué est listé par git avec un `/` final : il est
/// conservé sans ce `/` (une seule entrée opaque).
///
/// # Errors
///
/// Terminaison NUL absente, chemin non UTF-8 ou non normalisé.
pub fn parse_untracked_z(raw: &[u8]) -> Result<Vec<Change>, SelectError> {
    let what = "git ls-files --others";
    nul_fields(raw, what)?
        .into_iter()
        .map(|bytes| {
            let bytes = bytes.strip_suffix(b"/").unwrap_or(bytes);
            rel_path(bytes, what).map(|path| Change::Added { path })
        })
        .collect()
}

fn ensure_top_level(repo: &Path) -> Result<(), SelectError> {
    let out = git(repo, &["rev-parse", "--show-toplevel"])?;
    let text = std::str::from_utf8(&out)
        .map_err(|_| invalid("git rev-parse", "chemin non UTF-8"))?
        .trim_end_matches('\n');
    let top = Path::new(text)
        .canonicalize()
        .map_err(|error| io_error(Path::new(text), &error))?;
    if top != repo {
        return Err(invalid(
            "repo",
            format!(
                "{} n'est pas la racine du dépôt git ({})",
                repo.display(),
                top.display()
            ),
        ));
    }
    Ok(())
}

fn resolve_base(repo: &Path, base: &str) -> Result<String, SelectError> {
    if base.is_empty() || base.starts_with('-') || base.contains(['\0', '\n']) {
        return Err(invalid("base", format!("référence refusée : {base:?}")));
    }
    let spec = format!("{base}^{{commit}}");
    let out = git(repo, &["rev-parse", "--verify", "--quiet", &spec])?;
    let sha = std::str::from_utf8(&out)
        .map_err(|_| invalid("git rev-parse", "sortie non UTF-8"))?
        .trim();
    let valid = sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit());
    if !valid {
        return Err(invalid("base", format!("commit illisible pour {base:?}")));
    }
    Ok(sha.to_ascii_lowercase())
}

/// Changements entre `base` (commit ou référence) et l'arbre de travail de `repo`
/// (racine du dépôt) : commits, index, arbre de travail et fichiers non suivis non
/// ignorés. `ChangeSet::base` reçoit le commit résolu (40 hex), les changements sont
/// triés et sans doublon.
///
/// # Errors
///
/// `repo` qui n'est pas la racine d'un dépôt git, base inconnue, git absent ou en échec,
/// sortie illisible, chemin non UTF-8.
pub fn collect(repo: &Path, base: &str) -> Result<ChangeSet, SelectError> {
    let repo = repo
        .canonicalize()
        .map_err(|error| io_error(repo, &error))?;
    ensure_top_level(&repo)?;
    let sha = resolve_base(&repo, base)?;
    let diff = git(
        &repo,
        &[
            "diff",
            "--name-status",
            "-M",
            "-z",
            "--no-color",
            "--no-ext-diff",
            &sha,
            "--",
        ],
    )?;
    let others = git(&repo, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    let mut changes = parse_name_status_z(&diff)?;
    changes.extend(parse_untracked_z(&others)?);
    changes.sort();
    changes.dedup();
    Ok(ChangeSet { base: sha, changes })
}
