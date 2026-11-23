//! `kioku select base` (inventaire + `JUnit` + graphe) et `kioku select learn` (filet couverture).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::select::base::{self, invalid, io_error, run_command};
use crate::select::journal::read_bounded;
use crate::select::learn::{self, CoverageOptions, LearnAlert};
use crate::select::{SelectError, graph};

use super::Store;
use super::nextest;

const GIT_TIMEOUT: Duration = Duration::from_secs(60);

fn git_text(repo: &Path, args: &[&str]) -> Result<String, SelectError> {
    let mut full = vec!["--no-optional-locks"];
    full.extend_from_slice(args);
    let out = run_command("git", &full, repo, GIT_TIMEOUT, 16 * 1024 * 1024)?;
    String::from_utf8(out)
        .map(|text| text.trim_end_matches('\n').to_owned())
        .map_err(|_| invalid("git", "sortie non UTF-8"))
}

/// Arbre propre : ni modification suivie, ni fichier non suivi non ignoré.
fn ensure_clean(repo: &Path) -> Result<(), SelectError> {
    let status = git_text(repo, &["status", "--porcelain"])?;
    if status.is_empty() {
        return Ok(());
    }
    let first = status.lines().next().unwrap_or_default();
    Err(invalid(
        "base",
        format!(
            "l'arbre de travail n'est pas propre (ex. `{first}`) : l'inventaire ne décrirait pas le commit"
        ),
    ))
}

/// Options de `kioku select base`.
#[derive(Debug, Clone)]
pub struct BaseOptions {
    /// Racine du dépôt.
    pub repo: PathBuf,
    /// Magasin.
    pub store: Store,
    /// `JUnit` déjà produit (sinon nextest est lancé).
    pub junit: Option<PathBuf>,
    /// Features Cargo.
    pub features: Vec<String>,
    /// Délai du lancement de nextest.
    pub timeout: Duration,
}

/// Résultat de `kioku select base`.
#[derive(Debug)]
pub struct BaseReport {
    /// Commit inventorié.
    pub commit: String,
    /// Nombre de binaires de test.
    pub binaries: usize,
    /// Nombre de tests.
    pub tests: usize,
    /// Le run de la base est-il vert ?
    pub green: bool,
    /// Digest du graphe.
    pub graph_digest: String,
    /// Fichier d'inventaire écrit.
    pub path: PathBuf,
    /// Durée totale.
    pub elapsed: Duration,
    /// Dont durée du lancement de nextest (0 si le `JUnit` était fourni).
    pub nextest_elapsed: Duration,
}

/// Inventorie `HEAD` (arbre propre exigé), lit ou produit le `JUnit`, construit le graphe
/// (toutes features, leçon L3) et écrit `base-<commit>.json`.
///
/// # Errors
/// [`SelectError`] : arbre sale, outil absent, nextest sans `JUnit` (compilation en échec,
/// délai…), graphe impossible, magasin illisible.
pub fn build_base(opts: &BaseOptions) -> Result<BaseReport, SelectError> {
    let started = Instant::now();
    let repo = opts
        .repo
        .canonicalize()
        .map_err(|error| io_error(&opts.repo, &error))?;
    let features = base::normalize_features(&opts.features)?;
    ensure_clean(&repo)?;
    let commit = git_text(&repo, &["rev-parse", "HEAD"])?;
    let mut inventory = base::collect(&repo, &commit, &features)?;
    let mut nextest_elapsed = Duration::ZERO;
    let xml = if let Some(path) = &opts.junit {
        String::from_utf8(read_bounded(path)?).map_err(|_| invalid("junit", "fichier non UTF-8"))?
    } else {
        let run = nextest::run(&repo, &features, &[], opts.timeout)?;
        if !run.ended_normally() {
            return Err(invalid(
                "base",
                format!(
                    "le run de la base n'a pas de résultats : {}",
                    run.describe_end()
                ),
            ));
        }
        nextest_elapsed = run.elapsed;
        run.junit
            .ok_or_else(|| invalid("base", "nextest n'a pas écrit de JUnit"))?
    };
    base::apply_junit(&mut inventory, &xml)?;
    let options = graph::GraphOptions {
        features: inventory.features.clone(),
        all_features: true,
        ..graph::GraphOptions::default()
    };
    let data = graph::load_or_build(&repo, opts.store.root(), &options)?;
    inventory.graph_digest = Some(data.digest.clone());
    let path = opts.store.save_base(&inventory)?;
    Ok(BaseReport {
        commit,
        binaries: inventory.binaries.len(),
        tests: inventory.binaries.iter().map(|b| b.tests.len()).sum(),
        green: inventory.green,
        graph_digest: data.digest,
        path,
        elapsed: started.elapsed(),
        nextest_elapsed,
    })
}

/// Options de `kioku select learn`.
#[derive(Debug, Clone)]
pub struct LearnOptions {
    /// Racine du dépôt.
    pub repo: PathBuf,
    /// Magasin.
    pub store: Store,
    /// Commit ou référence de la base (défaut : `HEAD`).
    pub base_ref: Option<String>,
    /// Couverture déjà mesurée (`lines.json`) ; sinon [`learn::run_coverage`].
    pub coverage_json: Option<PathBuf>,
    /// Délai total de la couverture mesurée.
    pub timeout: Duration,
}

/// Résultat de `kioku select learn`.
#[derive(Debug)]
pub struct LearnReport {
    /// Commit de la base.
    pub commit: String,
    /// Fichier écrit.
    pub path: PathBuf,
    /// Tests qui ont gagné au moins une arête.
    pub tests_with_edges: usize,
    /// Arêtes apprises (test → fichier).
    pub edges: usize,
    /// Alertes (une par arête, et par test sans racine connue).
    pub alerts: Vec<LearnAlert>,
}

/// Apprend les arêtes que la couverture révèle et écrit `learned-<commit>.json`.
///
/// # Errors
/// [`SelectError`] : base ou graphe absents du magasin, couverture illisible ou en échec,
/// écriture impossible. Avec `--run`, `HEAD` doit être le commit de la base et l'arbre propre.
pub fn learn_from_coverage(opts: &LearnOptions) -> Result<LearnReport, SelectError> {
    let repo = opts
        .repo
        .canonicalize()
        .map_err(|error| io_error(&opts.repo, &error))?;
    let reference = opts.base_ref.as_deref().unwrap_or("HEAD");
    if reference.is_empty() || reference.starts_with('-') || reference.contains(['\0', '\n']) {
        return Err(invalid(
            "base",
            format!("référence refusée : {reference:?}"),
        ));
    }
    let spec = format!("{reference}^{{commit}}");
    let commit = git_text(&repo, &["rev-parse", "--verify", "--quiet", &spec])?;
    let inventory = opts.store.load_base(&commit)?;
    let digest = inventory.graph_digest.clone().ok_or_else(|| {
        invalid(
            "base inventory",
            "aucun digest de graphe : refaire select base",
        )
    })?;
    let data = opts.store.load_graph(&digest)?;
    let coverage = if let Some(path) = &opts.coverage_json {
        let text = String::from_utf8(read_bounded(path)?)
            .map_err(|_| invalid("coverage", "fichier non UTF-8"))?;
        learn::coverage_from_json(&text)?
    } else {
        let head = git_text(&repo, &["rev-parse", "HEAD"])?;
        if head != commit {
            return Err(invalid(
                "learn",
                format!(
                    "--run mesure l'arbre courant : HEAD ({head}) n'est pas la base ({commit})"
                ),
            ));
        }
        ensure_clean(&repo)?;
        let coverage_options = CoverageOptions {
            features: inventory.features.clone(),
            timeout: opts.timeout,
            ..CoverageOptions::default()
        };
        learn::run_coverage_with_evidence(&repo, &coverage_options, opts.store.root(), &commit)?
    };
    let mut learned = learn::learn(&data, &coverage);
    learned.base_commit.clone_from(&commit);
    let path = opts.store.save_learned(&commit, &learned)?;
    Ok(LearnReport {
        commit,
        path,
        tests_with_edges: learned.edges.len(),
        edges: learned
            .edges
            .values()
            .map(std::collections::BTreeSet::len)
            .sum(),
        alerts: learned.alerts,
    })
}
