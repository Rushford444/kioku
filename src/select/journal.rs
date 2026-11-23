//! Lot L3 : journal de sélection (S4, S8).
//!
//! Le journal nomme chaque test de l'inventaire de la base (plus les tests nouveaux)
//! exactement une fois, avec sa décision (lancé ou sauté) et au moins une raison. Un test
//! absent, en double ou sans raison est un échec du run ([`check_complete`]). Le statut ne
//! peut pas surévaluer le run : `full-green` exige une portée « tout » et zéro saut (S8).

use std::collections::BTreeSet;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use super::{Decision, JOURNAL_SCHEMA, JournalEntry, Reason, RelPath, SelectError, TestId};

/// Taille maximale d'un journal lu (borne contre un fichier corrompu ou hostile).
const MAX_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;

/// Statut du run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JournalStatus {
    /// Run partiel, tous les tests lancés verts. Jamais présenté comme complet.
    PartialGreen,
    /// Au moins un test lancé est rouge.
    Red,
    /// Journal incomplet ou run interrompu : la suite complète doit être lancée.
    Incomplete,
    /// Run complet (portée « tout », aucun saut), tout vert.
    FullGreen,
    /// Plan seulement, rien n'a été exécuté.
    Planned,
}

/// Portée du run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JournalScope {
    /// Suite complète.
    Everything,
    /// Sélection partielle.
    Partial,
}

/// Comptes du journal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalCounts {
    /// Tests de l'inventaire de la base.
    pub inventory: usize,
    /// Tests nouveaux (absents de l'inventaire).
    pub new: usize,
    /// Tests lancés.
    pub run: usize,
    /// Tests sautés.
    pub skipped: usize,
}

/// Résultat constaté d'un test (réconciliation avec le `JUnit` de nextest).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    /// Passé.
    Passed,
    /// Rouge.
    Failed,
    /// Vert après relance (instable).
    Flaky,
    /// `#[ignore]` : nextest ne l'a pas lancé.
    Ignored,
    /// Exclu par le filtre `-E` de l'utilisateur (seulement avec `user_filter`).
    FilteredOut,
    /// Aucun résultat alors que le test devait tourner : le run est incomplet.
    NotRun,
}

/// Résultat d'un test dans le journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestOutcome {
    /// Le test.
    pub test: TestId,
    /// Son résultat.
    pub outcome: Outcome,
}

/// Journal d'un run de sélection (schéma `kioku/select/v1`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    /// Toujours [`JOURNAL_SCHEMA`].
    pub schema: String,
    /// Statut du run.
    pub status: JournalStatus,
    /// Commit de la base.
    pub base_commit: String,
    /// Digest du graphe utilisé.
    pub graph_digest: String,
    /// Fichiers modifiés (base → arbre de travail).
    pub modified_files: Vec<RelPath>,
    /// Portée.
    pub scope: JournalScope,
    /// Raisons de la portée « tout » (au moins une si `scope == Everything`).
    pub scope_reasons: Vec<Reason>,
    /// Une entrée par test.
    pub entries: Vec<JournalEntry>,
    /// Comptes.
    pub counts: JournalCounts,
    /// Résultat de chaque test lancé (réconciliation avec le `JUnit`) ; vide pour un plan.
    #[serde(default)]
    pub outcomes: Vec<TestOutcome>,
    /// Filtre `-E` de l'utilisateur, s'il y en avait un : le run n'est alors jamais complet.
    #[serde(default)]
    pub user_filter: Option<String>,
}

impl JournalCounts {
    /// Comptes d'un journal : `run` et `skipped` calculés depuis les entrées, `inventory`
    /// et `new` fournis par l'appelant.
    #[must_use]
    pub fn from_entries(entries: &[JournalEntry], inventory: usize, new: usize) -> Self {
        let skipped = entries
            .iter()
            .filter(|e| e.decision == Decision::Skip)
            .count();
        Self {
            inventory,
            new,
            run: entries.len() - skipped,
            skipped,
        }
    }
}

/// Statut final d'un run (S8). Jamais `full-green` si la portée est partielle ou si un
/// test a été sauté ; `incomplete` l'emporte sur tout.
#[must_use]
pub fn final_status(
    scope: JournalScope,
    skipped: usize,
    complete: bool,
    any_red: bool,
) -> JournalStatus {
    if !complete {
        JournalStatus::Incomplete
    } else if any_red {
        JournalStatus::Red
    } else if scope == JournalScope::Everything && skipped == 0 {
        JournalStatus::FullGreen
    } else {
        JournalStatus::PartialGreen
    }
}

fn invalid(message: impl Into<String>) -> SelectError {
    SelectError::Invalid {
        what: "select journal".to_owned(),
        message: message.into(),
    }
}

fn label(test: &TestId) -> String {
    format!("{} {}", test.binary_id, test.name)
}

/// Une raison qui justifie de LANCER un test (toute raison sauf les raisons de saut).
fn justifies_run(reason: &Reason) -> bool {
    !matches!(
        reason,
        Reason::NoPathToModified { .. } | Reason::D1SkipIgnored
    )
}

/// S8 : le statut ne surévalue jamais le run. `full-green` exige la portée « tout » et
/// zéro test sauté ; la portée « tout » ne saute rien et porte au moins une raison.
///
/// # Errors
/// [`SelectError::Invalid`] si le schéma est inconnu ou si le statut/la portée se contredisent.
pub fn check_status(journal: &Journal) -> Result<(), SelectError> {
    if journal.schema != JOURNAL_SCHEMA {
        return Err(invalid(format!("unknown schema {:?}", journal.schema)));
    }
    let skipped = journal
        .entries
        .iter()
        .filter(|e| e.decision == Decision::Skip)
        .count();
    if journal.status == JournalStatus::FullGreen
        && (journal.scope != JournalScope::Everything
            || skipped != 0
            || journal.counts.skipped != 0)
    {
        return Err(invalid(
            "full-green requires scope everything and zero skipped tests",
        ));
    }
    if journal.scope == JournalScope::Everything {
        if skipped != 0 {
            return Err(invalid("scope everything cannot skip a test"));
        }
        if journal.scope_reasons.is_empty() {
            return Err(invalid("scope everything has no reason"));
        }
    }
    let count = |wanted: Outcome| {
        journal
            .outcomes
            .iter()
            .filter(|o| o.outcome == wanted)
            .count()
    };
    if count(Outcome::FilteredOut) != 0 && journal.user_filter.is_none() {
        return Err(invalid("filtered-out outcome without a user filter"));
    }
    if matches!(
        journal.status,
        JournalStatus::FullGreen | JournalStatus::PartialGreen
    ) && (count(Outcome::Failed) != 0 || count(Outcome::NotRun) != 0)
    {
        return Err(invalid(
            "a green status cannot hide a failed or not-run test",
        ));
    }
    if journal.status == JournalStatus::FullGreen && journal.user_filter.is_some() {
        return Err(invalid("full-green is impossible with a user filter"));
    }
    Ok(())
}

/// S4 : sautés + lancés = inventaire de la base ∪ tests nouveaux, chaque test exactement
/// une fois, chaque entrée avec au moins une raison cohérente avec sa décision, comptes
/// exacts. Toute violation est un échec du run (l'appelant lance alors la suite complète).
///
/// # Errors
/// [`SelectError::Invalid`] décrivant la première violation : test manquant, doublon, test
/// inconnu, entrée sans raison, saut sans raison de saut (ou avec une raison de lancer),
/// lancement justifié seulement par une raison de saut, comptes faux, statut contradictoire.
pub fn check_complete(
    journal: &Journal,
    inventory_tests: &BTreeSet<TestId>,
    new_tests: &BTreeSet<TestId>,
) -> Result<(), SelectError> {
    check_status(journal)?;
    if let Some(both) = inventory_tests.intersection(new_tests).next() {
        return Err(invalid(format!(
            "test {} is both in the base inventory and new",
            label(both)
        )));
    }
    let mut seen: BTreeSet<&TestId> = BTreeSet::new();
    let (mut run, mut skipped) = (0usize, 0usize);
    for entry in &journal.entries {
        if !inventory_tests.contains(&entry.test) && !new_tests.contains(&entry.test) {
            return Err(invalid(format!(
                "test {} is not in the inventory nor new",
                label(&entry.test)
            )));
        }
        if !seen.insert(&entry.test) {
            return Err(invalid(format!(
                "duplicate entry for {}",
                label(&entry.test)
            )));
        }
        if entry.reasons.is_empty() {
            return Err(invalid(format!(
                "entry for {} has no reason",
                label(&entry.test)
            )));
        }
        let has_skip = entry
            .reasons
            .iter()
            .any(|r| matches!(r, Reason::NoPathToModified { .. }));
        let has_run = entry.reasons.iter().any(justifies_run);
        match entry.decision {
            Decision::Skip => {
                skipped += 1;
                if !has_skip {
                    return Err(invalid(format!(
                        "skipped test {} has no skip reason",
                        label(&entry.test)
                    )));
                }
                if has_run {
                    return Err(invalid(format!(
                        "skipped test {} carries a reason to run",
                        label(&entry.test)
                    )));
                }
            }
            Decision::Run => {
                run += 1;
                if !has_run {
                    return Err(invalid(format!(
                        "run test {} has only skip reasons",
                        label(&entry.test)
                    )));
                }
            }
        }
    }
    let expected = inventory_tests.len() + new_tests.len();
    if let Some(missing) = inventory_tests
        .iter()
        .chain(new_tests)
        .find(|t| !seen.contains(t))
    {
        return Err(invalid(format!(
            "test {} is missing from the journal ({} of {expected} listed)",
            label(missing),
            seen.len()
        )));
    }
    let counts = journal.counts;
    if counts.inventory != inventory_tests.len()
        || counts.new != new_tests.len()
        || counts.run != run
        || counts.skipped != skipped
    {
        return Err(invalid(format!(
            "counts mismatch: journal says {counts:?}, entries give run={run} skipped={skipped}, \
             inventory={} new={}",
            inventory_tests.len(),
            new_tests.len()
        )));
    }
    check_outcomes(journal)
}

/// Réconciliation : chaque test lancé a un résultat (sauf pour un plan), chaque résultat
/// désigne une entrée du journal, une seule fois.
fn check_outcomes(journal: &Journal) -> Result<(), SelectError> {
    let mut with_outcome: BTreeSet<&TestId> = BTreeSet::new();
    for outcome in &journal.outcomes {
        if !with_outcome.insert(&outcome.test) {
            return Err(invalid(format!(
                "duplicate outcome for {}",
                label(&outcome.test)
            )));
        }
    }
    if journal.status == JournalStatus::Planned {
        return if journal.outcomes.is_empty() {
            Ok(())
        } else {
            Err(invalid("a plan cannot carry outcomes"))
        };
    }
    let listed: BTreeSet<&TestId> = journal.entries.iter().map(|e| &e.test).collect();
    if let Some(stray) = with_outcome.iter().find(|t| !listed.contains(**t)) {
        return Err(invalid(format!(
            "outcome for {} which is not a journal entry",
            label(stray)
        )));
    }
    if let Some(entry) = journal
        .entries
        .iter()
        .find(|e| e.decision == Decision::Run && !with_outcome.contains(&e.test))
    {
        return Err(invalid(format!(
            "run test {} has no outcome",
            label(&entry.test)
        )));
    }
    Ok(())
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn io_error(path: &Path, err: &std::io::Error) -> SelectError {
    SelectError::Io {
        path: path.display().to_string(),
        message: err.to_string(),
    }
}

/// Écriture atomique : fichier temporaire du même dossier, `fsync`, puis `rename`. Un
/// lecteur voit l'ancien contenu ou le nouveau, jamais un fichier tronqué. Le fichier
/// temporaire est supprimé en cas d'échec.
///
/// # Errors
/// [`SelectError::Io`] si le dossier, l'écriture ou le renommage échoue.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), SelectError> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    fs::create_dir_all(&parent).map_err(|e| io_error(&parent, &e))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| invalid(format!("path {} has no file name", path.display())))?
        .to_string_lossy()
        .into_owned();
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".{file_name}.tmp-{}-{nonce}", std::process::id()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if let Err(err) = result {
        let _ = fs::remove_file(&tmp);
        return Err(io_error(path, &err));
    }
    // Durabilité du renommage : best effort (certains systèmes refusent l'ouverture d'un dossier).
    if let Ok(dir) = fs::File::open(&parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Lit un fichier JSON borné.
pub(crate) fn read_bounded(path: &Path) -> Result<Vec<u8>, SelectError> {
    let meta = fs::metadata(path).map_err(|e| io_error(path, &e))?;
    if meta.len() > MAX_JOURNAL_BYTES {
        return Err(SelectError::Invalid {
            what: path.display().to_string(),
            message: format!("file larger than {MAX_JOURNAL_BYTES} bytes"),
        });
    }
    fs::read(path).map_err(|e| io_error(path, &e))
}

/// Écrit le journal de façon atomique, après avoir vérifié le schéma et le statut (S8) :
/// un journal incohérent n'est jamais écrit.
///
/// # Errors
/// [`SelectError::Invalid`] si le journal est incohérent, [`SelectError::Io`] sinon.
pub fn write(path: &Path, journal: &Journal) -> Result<(), SelectError> {
    check_status(journal)?;
    let mut bytes = serde_json::to_vec_pretty(journal).map_err(|e| invalid(e.to_string()))?;
    bytes.push(b'\n');
    write_atomic(path, &bytes)
}

/// Lit un journal ; schéma et cohérence du statut vérifiés.
///
/// # Errors
/// [`SelectError::Io`] si le fichier est illisible, [`SelectError::Invalid`] si le JSON, le
/// schéma ou le statut est invalide.
pub fn read(path: &Path) -> Result<Journal, SelectError> {
    let bytes = read_bounded(path)?;
    let journal: Journal = serde_json::from_slice(&bytes).map_err(|e| invalid(e.to_string()))?;
    check_status(&journal)?;
    Ok(journal)
}
