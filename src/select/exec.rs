//! Lot L4b : exécution ciblée, magasin, `kioku test` / `kioku select …`.
//!
//! Flux de [`run_test`] : charger la base (commit le plus récent du magasin, graphe, arêtes
//! apprises), collecter les changements, classer (§ 4), planifier (L4a), lancer nextest avec
//! un `JUnit`, réconcilier, écrire le journal, en déduire statut et code de sortie (S8).
//!
//! S3 : toute erreur de chargement ou de planification donne la portée « tout » avec
//! `Reason::Error` ; un run partiel incomplet est suivi de la suite complète. Un run qui
//! passe par S3 n'est jamais « vert complet » : au mieux `incomplete` (code 3).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::json;

use super::base::{self, invalid, io_error, run_command};
use super::journal::{
    self, Journal, JournalCounts, JournalScope, JournalStatus, Outcome, final_status,
};
use super::{
    BaseInventory, BaseStatus, ChangeSet, D1Proposal, Decision, GraphData, JOURNAL_SCHEMA, Plan,
    Reason, Scope, SelectError, TestId, changes, classify, plan,
};
use super::{graph, learn};

mod commands;
mod nextest;
mod reconcile;
mod store;

pub use commands::{
    BaseOptions, BaseReport, LearnOptions, LearnReport, build_base, learn_from_coverage,
};
pub use store::Store;

use reconcile::{End, Input, Observed, OracleInput};

/// Délai par défaut d'un lancement de nextest (secondes).
pub const DEFAULT_TIMEOUT_SECS: u64 = 3600;
/// Nombre de commits de la base examinés (`git rev-list --first-parent -n`).
const BASE_CANDIDATES: &str = "50";
const GIT_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_GIT_OUTPUT: usize = 1024 * 1024;

/// Code de sortie : vert partiel.
pub const EXIT_PARTIAL_GREEN: u8 = 0;
/// Code de sortie : rouge (test en échec ou compilation en échec).
pub const EXIT_RED: u8 = 1;
/// Code de sortie : incomplet ou erreur de sélection (la suite complète a été lancée).
pub const EXIT_INCOMPLETE: u8 = 3;
/// Code de sortie : vert complet.
pub const EXIT_FULL_GREEN: u8 = 5;

/// Options de `kioku test` et de `kioku select plan`.
#[derive(Debug, Clone)]
pub struct TestOptions {
    /// Racine du dépôt.
    pub repo: PathBuf,
    /// Magasin.
    pub store: Store,
    /// Référence de la base (défaut : `main`, sinon `master`).
    pub base_ref: Option<String>,
    /// Proposition d1 (JSON `D1Proposal`).
    pub d1: Option<PathBuf>,
    /// Filtre `-E` de l'utilisateur.
    pub user_filter: Option<String>,
    /// Features Cargo demandées.
    pub features: Vec<String>,
    /// Chemin du journal (défaut : `target/kioku-select/journal.json` ; aucun pour un plan).
    pub journal: Option<PathBuf>,
    /// Délai de chaque lancement de nextest.
    pub timeout: Duration,
    /// Planifier sans exécuter (`kioku select plan`).
    pub plan_only: bool,
}

/// Résultat de `kioku test`.
#[derive(Debug)]
pub struct TestReport {
    /// Journal final.
    pub journal: Journal,
    /// Où le journal a été écrit (`None` : non écrit).
    pub journal_path: Option<PathBuf>,
    /// Code de sortie du processus.
    pub exit_code: u8,
    /// Code brut du dernier lancement nextest, si disponible.
    pub native_exit_code: Option<u8>,
    /// Pourquoi le run est incomplet (S3, délai, résultats manquants), s'il l'est.
    pub detail: Option<String>,
    /// Messages à afficher sur stderr.
    pub notes: Vec<String>,
    /// Durée totale de `kioku test`.
    pub elapsed: Duration,
    /// Dont durée des lancements de nextest (compilation comprise).
    pub nextest_elapsed: Duration,
    /// Dont durée de l'oracle de complétude (`cargo nextest list`), hors `nextest_elapsed`.
    pub oracle_elapsed: Duration,
}

// ---------------------------------------------------------------------------------------
// Base, graphe, plan
// ---------------------------------------------------------------------------------------

/// Base et graphe (avec les arêtes apprises appliquées).
#[derive(Debug)]
struct Loaded {
    base: BaseInventory,
    graph: GraphData,
}

fn git_text(repo: &Path, args: &[&str]) -> Result<String, SelectError> {
    let mut full = vec!["--no-optional-locks"];
    full.extend_from_slice(args);
    let out = run_command("git", &full, repo, GIT_TIMEOUT, MAX_GIT_OUTPUT)?;
    String::from_utf8(out)
        .map(|text| text.trim().to_owned())
        .map_err(|_| invalid("git", "sortie non UTF-8"))
}

/// `main`, sinon `master`.
fn default_base_ref(repo: &Path) -> Result<String, SelectError> {
    for candidate in ["main", "master"] {
        let spec = format!("{candidate}^{{commit}}");
        if git_text(repo, &["rev-parse", "--verify", "--quiet", &spec]).is_ok() {
            return Ok(candidate.to_owned());
        }
    }
    Err(invalid(
        "base",
        "ni `main` ni `master` dans ce dépôt (utiliser --base)",
    ))
}

fn check_ref(reference: &str) -> Result<(), SelectError> {
    if reference.is_empty() || reference.starts_with('-') || reference.contains(['\0', '\n']) {
        return Err(invalid(
            "base",
            format!("référence refusée : {reference:?}"),
        ));
    }
    Ok(())
}

/// Premier commit (de HEAD vers le passé, parents directs) qui a son inventaire ET ses
/// arêtes apprises dans le magasin.
fn pick_base_commit(repo: &Path, store: &Store, reference: &str) -> Result<String, SelectError> {
    check_ref(reference)?;
    let merge_base = git_text(repo, &["merge-base", "HEAD", reference])?;
    let listing = git_text(
        repo,
        &[
            "rev-list",
            "--first-parent",
            "-n",
            BASE_CANDIDATES,
            &merge_base,
        ],
    )?;
    listing
        .lines()
        .map(str::trim)
        .find(|commit| store.has_base_and_learned(commit))
        .map(str::to_owned)
        .ok_or_else(|| {
            invalid(
                "base",
                format!(
                    "aucun des {BASE_CANDIDATES} derniers commits de {reference} n'a base-<commit>.json \
                     et learned-<commit>.json dans {} (kioku select base, puis select learn)",
                    store.root().display()
                ),
            )
        })
}

/// Échec du chargement : l'erreur, et la base si elle avait pu être lue (elle fournit alors
/// l'inventaire du journal, S3).
struct LoadFailure {
    error: SelectError,
    base: Option<BaseInventory>,
}

fn load_graph_and_learned(
    opts: &TestOptions,
    base: &BaseInventory,
) -> Result<GraphData, SelectError> {
    let digest = base.graph_digest.clone().ok_or_else(|| {
        invalid(
            "base inventory",
            "aucun digest de graphe : refaire select base",
        )
    })?;
    let mut graph = opts.store.load_graph(&digest)?;
    let learned = opts.store.load_learned(&base.commit, &digest)?;
    learn::apply(&mut graph, &learned);
    Ok(graph)
}

fn load(repo: &Path, opts: &TestOptions) -> Result<Loaded, Box<LoadFailure>> {
    let no_base = |error| Box::new(LoadFailure { error, base: None });
    let reference = match &opts.base_ref {
        Some(reference) => reference.clone(),
        None => default_base_ref(repo).map_err(no_base)?,
    };
    let commit = pick_base_commit(repo, &opts.store, &reference).map_err(no_base)?;
    let base = opts.store.load_base(&commit).map_err(no_base)?;
    match load_graph_and_learned(opts, &base) {
        Ok(graph) => Ok(Loaded { base, graph }),
        Err(error) => Err(Box::new(LoadFailure {
            error,
            base: Some(base),
        })),
    }
}

/// Graphe de remplacement quand le vrai est illisible : il ne sert qu'à construire un plan
/// « tout » (aucune sélection n'en dépend).
fn placeholder_graph(base: &BaseInventory) -> GraphData {
    GraphData {
        schema: crate::select::GRAPH_SCHEMA.to_owned(),
        digest: base
            .graph_digest
            .clone()
            .unwrap_or_else(|| "none".to_owned()),
        ..GraphData::default()
    }
}

fn env_flag(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Demande de merge (S1) : `KIOKU_MERGE_REQUEST=1`, `CI_MERGE_REQUEST_IID` non vide ou
/// `GITHUB_EVENT_NAME=pull_request`.
fn merge_request_from(kioku: Option<&str>, gitlab: Option<&str>, github: Option<&str>) -> bool {
    kioku == Some("1") || gitlab.is_some() || github == Some("pull_request")
}

fn classify_context(
    repo: &Path,
    features: &[String],
) -> Result<classify::ClassifyContext, SelectError> {
    let branch = git_text(repo, &["symbolic-ref", "--short", "-q", "HEAD"])
        .ok()
        .filter(|name| !name.is_empty());
    Ok(classify::ClassifyContext {
        branch,
        is_merge_request: merge_request_from(
            env_flag("KIOKU_MERGE_REQUEST").as_deref(),
            env_flag("CI_MERGE_REQUEST_IID").as_deref(),
            env_flag("GITHUB_EVENT_NAME").as_deref(),
        ),
        toolchain: base::current_toolchain(repo)?,
        features: features.to_vec(),
        build_env: base::current_build_env(),
        ..classify::ClassifyContext::default()
    })
}

fn read_d1(path: &Path) -> Result<D1Proposal, SelectError> {
    let bytes = journal::read_bounded(path)?;
    serde_json::from_slice(&bytes).map_err(|e| invalid("d1 proposal", e.to_string()))
}

/// Chemins touchés par les changements (pour le journal quand le plan n'en donne pas).
fn changed_paths(set: &ChangeSet) -> Vec<String> {
    let mut paths = BTreeSet::new();
    for change in &set.changes {
        match change {
            crate::select::Change::Added { path }
            | crate::select::Change::Modified { path }
            | crate::select::Change::Deleted { path } => {
                paths.insert(path.clone());
            }
            crate::select::Change::Renamed { from, to } => {
                paths.insert(from.clone());
                paths.insert(to.clone());
            }
        }
    }
    paths.into_iter().collect()
}

/// Fichiers modifiés qu'aucune racine de test n'atteint dans `graph`.
fn orphan_files(graph: &GraphData, modified: &BTreeSet<String>) -> Vec<String> {
    let roots: BTreeSet<String> = graph.test_roots.values().flatten().cloned().collect();
    let reached = classify::reach(graph, &roots);
    modified
        .iter()
        .filter(|file| !reached.contains(*file))
        .cloned()
        .collect()
}

fn make_plan(
    repo: &Path,
    loaded: &Loaded,
    opts: &TestOptions,
    features: &[String],
) -> Result<Plan, SelectError> {
    let changes = changes::collect(repo, &loaded.base.commit)?;
    let ctx = classify_context(repo, features)?;
    let content = classify::RepoContent::new(repo, &loaded.base.commit);
    // L4d : la sélection ignore les arêtes `mod` pures (graphe complet gardé pour l'audit ;
    // 6 ratés de couverture identiques avec ou sans, 0 après filet S6).
    let view = graph::selection_view(&loaded.graph);
    let scope = classify::classify(&changes, &loaded.base, &view, &ctx, &content);
    let d1 = opts.d1.as_deref().map(read_d1).transpose()?;
    let mut planned = plan::plan(&scope, &loaded.base, &view, d1.as_ref());
    // Doute (S2) : un fichier modifié qu'aucun test n'atteint dans la vue (ex. un `mod.rs` qui ne
    // fait que déclarer des modules) → plan sur le graphe COMPLET ; encore orphelin → tout.
    // Sinon une modif cassant la compilation d'un tel fichier pourrait finir « vert partiel »
    // sans qu'aucun binaire ne soit compilé.
    if let Scope::Partial(partial) = &scope
        && !orphan_files(&view, &partial.modified).is_empty()
    {
        planned = plan::plan(&scope, &loaded.base, &loaded.graph, d1.as_ref());
        let still = orphan_files(&loaded.graph, &partial.modified);
        if !still.is_empty() {
            let reasons = still.into_iter().map(Reason::UnknownRustFile).collect();
            planned = plan::plan(
                &Scope::Everything { reasons },
                &loaded.base,
                &loaded.graph,
                None,
            );
        }
    }
    if planned.modified_files.is_empty() {
        planned.modified_files = changed_paths(&changes);
    }
    if let Some(expr) = opts.user_filter.as_deref() {
        // L4e : ne compiler que les binaires que le `-E` de l'utilisateur peut faire tourner.
        plan::restrict_to_user_filter(&mut planned, &loaded.base, &view, expr);
    }
    Ok(planned)
}

/// Plan « tout » : la base (si lisible) fournit l'inventaire, sinon les entrées viendront des
/// résultats de nextest.
fn everything_plan(loaded: Option<&Loaded>, reasons: Vec<Reason>, modified: &[String]) -> Plan {
    let scope = Scope::Everything { reasons };
    let mut planned = match loaded {
        Some(loaded) => plan::plan(&scope, &loaded.base, &loaded.graph, None),
        None => Plan {
            scope,
            base_commit: "unknown".to_owned(),
            graph_digest: "none".to_owned(),
            modified_files: Vec::new(),
            entries: Vec::new(),
            targets: None,
            filter: None,
        },
    };
    planned.modified_files = modified.to_vec();
    planned
}

// ---------------------------------------------------------------------------------------
// Exécution d'un plan
// ---------------------------------------------------------------------------------------

/// Un plan exécuté et réconcilié.
struct Stage {
    journal: Journal,
    native_exit_code: Option<u8>,
    /// Pourquoi le run est incomplet (si `journal.status == Incomplete`).
    why: Option<String>,
    /// Durée du lancement de nextest.
    nextest_elapsed: Duration,
    /// Durée de l'oracle de complétude.
    oracle_elapsed: Duration,
}

fn scope_parts(scope: &Scope) -> (JournalScope, Vec<Reason>) {
    match scope {
        Scope::Everything { reasons } => (JournalScope::Everything, reasons.clone()),
        Scope::Partial(_) => (JournalScope::Partial, Vec::new()),
    }
}

/// Résultat d'un lancement de nextest suivi de l'oracle de complétude.
struct Launched {
    end: End,
    native_exit_code: Option<u8>,
    /// Ce que l'oracle a vu en plus des résultats.
    observed: Observed,
    nextest_elapsed: Duration,
    oracle_elapsed: Duration,
}

impl Launched {
    fn without_oracle(end: End, nextest_elapsed: Duration) -> Self {
        Self {
            end,
            native_exit_code: None,
            observed: Observed::default(),
            nextest_elapsed,
            oracle_elapsed: Duration::ZERO,
        }
    }
}

fn run_nextest(
    repo: &Path,
    planned: &Plan,
    loaded: Option<&Loaded>,
    opts: &TestOptions,
    features: &[String],
) -> Launched {
    let extra = plan::nextest_args(planned, opts.user_filter.as_deref());
    let run = match nextest::run(repo, features, &extra, opts.timeout) {
        Ok(run) => run,
        Err(error) => {
            return Launched::without_oracle(End::Abnormal(error.to_string()), Duration::ZERO);
        }
    };
    let native_exit_code = run.exit_code.and_then(|code| u8::try_from(code).ok());
    if run.build_failed() {
        return Launched {
            native_exit_code,
            ..Launched::without_oracle(End::BuildFailed, run.elapsed)
        };
    }
    if !run.ended_normally() {
        return Launched {
            native_exit_code,
            ..Launched::without_oracle(End::Abnormal(run.describe_end()), run.elapsed)
        };
    }
    let results = match run.junit.as_deref().map(base::read_junit) {
        Some(Ok(results)) => results,
        Some(Err(error)) => {
            let why = format!("JUnit illisible : {error}");
            return Launched {
                native_exit_code,
                ..Launched::without_oracle(End::Abnormal(why), run.elapsed)
            };
        }
        None => {
            let why = "nextest n'a pas écrit de JUnit".to_owned();
            return Launched {
                native_exit_code,
                ..Launched::without_oracle(End::Abnormal(why), run.elapsed)
            };
        }
    };
    // Oracle de complétude (A5) : la liste de nextest, indépendante des résultats retournés.
    let started = Instant::now();
    let inventory = loaded
        .map(|l| plan::base_targets(&l.base))
        .unwrap_or_default();
    let verdict = nextest::list(repo, features, &extra, opts.timeout)
        .map_err(|error| error.to_string())
        .and_then(|listing| {
            reconcile::check_listing(&OracleInput {
                plan: planned,
                listing: &listing,
                results: &results,
                inventory: &inventory,
                user_filter: opts.user_filter.is_some(),
            })
        });
    let oracle_elapsed = started.elapsed();
    match verdict {
        Ok(observed) => Launched {
            native_exit_code,
            end: End::Normal(results),
            observed,
            nextest_elapsed: run.elapsed,
            oracle_elapsed,
        },
        Err(why) => Launched {
            native_exit_code,
            end: End::Unverified(results, format!("oracle de complétude : {why}")),
            observed: Observed::default(),
            nextest_elapsed: run.elapsed,
            oracle_elapsed,
        },
    }
}

/// Plan partiel dont `-E` écarte toutes les cibles, ou sans aucune cible à compiler
/// (`targets == Some([])`, L4e) : on ne compile ni ne lance rien. Les tests `Run` que le
/// filtre écarte finissent `filtered-out`.
///
/// Un plan partiel sans test `Run` mais avec des cibles propriétaires (fichier modifié
/// compilé dans un binaire dont aucun test connu ne tourne, A5) n'est PAS « rien à lancer » :
/// ce binaire peut contenir un test neuf. Sans cible bornée (`None`), nextest tourne aussi.
fn nothing_to_run(planned: &Plan) -> bool {
    matches!(planned.scope, Scope::Partial(_))
        && planned.targets.as_ref().is_some_and(Vec::is_empty)
}

fn count_of(journal: &Journal, wanted: Outcome) -> usize {
    journal
        .outcomes
        .iter()
        .filter(|o| o.outcome == wanted)
        .count()
}

#[allow(
    clippy::too_many_lines,
    reason = "un seul endroit décide le statut du run (S8)"
)]
fn run_stage(
    repo: &Path,
    planned: &Plan,
    loaded: Option<&Loaded>,
    opts: &TestOptions,
    features: &[String],
    final_stage: bool,
) -> Stage {
    let Launched {
        native_exit_code,
        end,
        observed,
        nextest_elapsed,
        oracle_elapsed,
    } = if opts.plan_only || nothing_to_run(planned) {
        Launched::without_oracle(End::NothingToRun, Duration::ZERO)
    } else {
        run_nextest(repo, planned, loaded, opts, features)
    };
    let inventory_binaries: BTreeSet<String> = loaded
        .map(|l| {
            l.base
                .binaries
                .iter()
                .map(|b| b.binary_id.clone())
                .collect()
        })
        .unwrap_or_default();
    let ignored: BTreeSet<TestId> = loaded
        .into_iter()
        .flat_map(|l| {
            l.base.binaries.iter().flat_map(|b| {
                b.tests
                    .iter()
                    .filter(|t| t.status == BaseStatus::Ignored)
                    .map(|t| TestId {
                        binary_id: b.binary_id.clone(),
                        name: t.name.clone(),
                    })
            })
        })
        .collect();
    let rec = reconcile::reconcile(&Input {
        plan: planned,
        inventory_binaries: &inventory_binaries,
        ignored: &ignored,
        user_filter: opts.user_filter.as_deref(),
        end: &end,
        observed: &observed,
    });
    let (scope, scope_reasons) = scope_parts(&planned.scope);
    let skipped = rec
        .entries
        .iter()
        .filter(|e| e.decision == Decision::Skip)
        .count();
    let counts = JournalCounts::from_entries(&rec.entries, rec.inventory.len(), rec.new.len());
    let mut why: Option<String> = match &end {
        End::Abnormal(reason) | End::Unverified(_, reason) => Some(reason.clone()),
        _ if !opts.plan_only && rec.any_not_run() => Some(format!(
            "{} test(s) lancés sans résultat",
            rec.count(Outcome::NotRun)
        )),
        _ => None,
    };
    let complete = why.is_none() && !matches!(end, End::BuildFailed);
    let mut status = if matches!(end, End::BuildFailed) {
        why = Some("échec de compilation".to_owned());
        JournalStatus::Red
    } else if opts.plan_only {
        JournalStatus::Planned
    } else {
        final_status(scope, skipped, complete, rec.any_failed())
    };
    if status == JournalStatus::FullGreen
        && (opts.user_filter.is_some() || rec.count(Outcome::FilteredOut) != 0)
    {
        status = JournalStatus::PartialGreen;
    }
    if matches!(
        status,
        JournalStatus::FullGreen | JournalStatus::PartialGreen
    ) && scope_reasons
        .iter()
        .any(|reason| matches!(reason, Reason::Error(_)))
    {
        // S3 : une erreur de sélection n'est jamais un vert.
        status = JournalStatus::Incomplete;
        why = scope_reasons.iter().find_map(|r| match r {
            Reason::Error(message) => Some(format!("erreur de sélection (S3) : {message}")),
            _ => None,
        });
    }
    let (outcomes, entries) = if opts.plan_only {
        (Vec::new(), rec.entries.clone())
    } else {
        (rec.outcomes.clone(), rec.entries.clone())
    };
    let mut journal = Journal {
        schema: JOURNAL_SCHEMA.to_owned(),
        status,
        base_commit: planned.base_commit.clone(),
        graph_digest: planned.graph_digest.clone(),
        modified_files: planned.modified_files.clone(),
        scope,
        scope_reasons,
        entries,
        counts,
        outcomes,
        user_filter: opts.user_filter.clone(),
    };
    if let Err(error) = journal::check_complete(&journal, &rec.inventory, &rec.new) {
        why = Some(format!("journal incohérent : {error}"));
        journal.status = JournalStatus::Incomplete;
    }
    if final_stage && journal.status == JournalStatus::Incomplete && rec.any_failed() {
        // Un test rouge reste rouge : S7.
        journal.status = JournalStatus::Red;
    }
    Stage {
        journal,
        native_exit_code,
        why,
        nextest_elapsed,
        oracle_elapsed,
    }
}

fn exit_code(status: JournalStatus) -> u8 {
    match status {
        JournalStatus::PartialGreen | JournalStatus::Planned => EXIT_PARTIAL_GREEN,
        JournalStatus::Red => EXIT_RED,
        JournalStatus::Incomplete => EXIT_INCOMPLETE,
        JournalStatus::FullGreen => EXIT_FULL_GREEN,
    }
}

fn default_journal_path(repo: &Path) -> PathBuf {
    repo.join("target")
        .join("kioku-select")
        .join("journal.json")
}

/// Lance `kioku test` (ou `kioku select plan` avec `plan_only`).
///
/// Toute erreur de sélection est absorbée (S3) : portée « tout », suite complète. Seules les
/// erreurs qui empêchent même de lancer quelque chose (dépôt introuvable, features invalides)
/// sont renvoyées.
///
/// # Errors
/// [`SelectError`] si `opts.repo` est introuvable ou si une feature est invalide.
pub fn run_test(opts: &TestOptions) -> Result<TestReport, SelectError> {
    let started = Instant::now();
    let repo = opts
        .repo
        .canonicalize()
        .map_err(|error| io_error(&opts.repo, &error))?;
    let features = base::normalize_features(&opts.features)?;
    let mut notes = Vec::new();
    let (loaded, prepared) = match load(&repo, opts) {
        Ok(loaded) => {
            let prepared = make_plan(&repo, &loaded, opts, &features);
            (Some(loaded), prepared)
        }
        Err(failure) => {
            let loaded = failure.base.map(|base| Loaded {
                graph: placeholder_graph(&base),
                base,
            });
            (loaded, Err(failure.error))
        }
    };
    let planned = match prepared {
        Ok(planned) => planned,
        Err(error) => {
            notes.push(format!(
                "kioku: sélection impossible, suite complète : {error}"
            ));
            everything_plan(loaded.as_ref(), vec![error.reason()], &[])
        }
    };
    let partial = matches!(planned.scope, Scope::Partial(_));
    let mut stage = run_stage(&repo, &planned, loaded.as_ref(), opts, &features, !partial);
    let mut earlier_runs = Duration::ZERO;
    let mut earlier_oracle = Duration::ZERO;
    if partial && !opts.plan_only && stage.journal.status == JournalStatus::Incomplete {
        let why = stage
            .why
            .clone()
            .unwrap_or_else(|| "run partiel incomplet".to_owned());
        notes.push(format!(
            "kioku: run partiel incomplet, suite complète : {why}"
        ));
        let fallback = everything_plan(
            loaded.as_ref(),
            vec![Reason::Error(format!("run partiel incomplet : {why}"))],
            &planned.modified_files,
        );
        earlier_runs = stage.nextest_elapsed;
        earlier_oracle = stage.oracle_elapsed;
        stage = run_stage(&repo, &fallback, loaded.as_ref(), opts, &features, true);
    }
    let Stage {
        native_exit_code,
        journal,
        why,
        nextest_elapsed,
        oracle_elapsed,
    } = stage;
    let mut exit = exit_code(journal.status);
    let path = match (&opts.journal, opts.plan_only) {
        (Some(path), _) => Some(path.clone()),
        (None, false) => Some(default_journal_path(&repo)),
        (None, true) => None,
    };
    let journal_path = path.and_then(|path| match journal::write(&path, &journal) {
        Ok(()) => Some(path),
        Err(error) => {
            notes.push(format!("kioku: journal non écrit : {error}"));
            // Un vert sans journal n'est pas prouvé.
            if exit == EXIT_PARTIAL_GREEN || exit == EXIT_FULL_GREEN {
                exit = EXIT_INCOMPLETE;
            }
            None
        }
    });
    Ok(TestReport {
        journal,
        journal_path,
        exit_code: exit,
        native_exit_code,
        detail: why,
        notes,
        elapsed: started.elapsed(),
        nextest_elapsed: earlier_runs + nextest_elapsed,
        oracle_elapsed: earlier_oracle + oracle_elapsed,
    })
}

// ---------------------------------------------------------------------------------------
// Rendu
// ---------------------------------------------------------------------------------------

/// Texte court d'une raison, lisible dans le terminal.
#[must_use]
pub fn describe_reason(reason: &Reason) -> String {
    match reason {
        Reason::ProtectedBranch(branch) => format!("branche protégée {branch}"),
        Reason::Error(message) => format!("erreur : {message}"),
        Reason::BuildInput(path) => format!("fichier de build modifié : {path}"),
        Reason::UnreferencedNonRust(path) => format!("fichier non Rust sans référence : {path}"),
        Reason::Macro(path) => format!("macro modifiée : {path}"),
        Reason::BuildEnvironment(message) => format!("environnement de build : {message}"),
        Reason::UnknownRustFile(path) => format!("fichier Rust hors des arbres connus : {path}"),
        Reason::ReachesModified { file, .. } => format!("atteint le fichier modifié {file}"),
        Reason::NewBinary(binary) => format!("binaire nouveau : {binary}"),
        Reason::DeletedOrRenamed(path) => format!("fichier supprimé ou renommé : {path}"),
        Reason::ExternalInput(input) => format!("entrée hors dépôt : {input}"),
        Reason::BaseNotGreen(status) => format!("non vert dans la base ({status:?})"),
        Reason::NewTest => "test nouveau".to_owned(),
        Reason::Learned { file } => format!("arête apprise vers {file}"),
        Reason::D1Added => "ajouté par d1".to_owned(),
        Reason::NoPathToModified { graph_digest } => {
            format!("aucun chemin vers un fichier modifié (graphe {graph_digest})")
        }
        Reason::D1SkipIgnored => "saut proposé par d1 ignoré".to_owned(),
        Reason::UserFilter(expr) => format!("exclu par votre filtre {expr}"),
    }
}

impl TestReport {
    /// Résumé JSON (`--json`).
    #[must_use]
    pub fn summary_json(&self) -> serde_json::Value {
        let j = &self.journal;
        let tally = |wanted: Outcome| count_of(j, wanted);
        json!({
            "schema": JOURNAL_SCHEMA,
            "status": j.status,
            "exit_code": self.exit_code,
            "scope": j.scope,
            "scope_reasons": j.scope_reasons,
            "base_commit": j.base_commit,
            "modified_files": j.modified_files,
            "counts": j.counts,
            "outcomes": {
                "passed": tally(Outcome::Passed),
                "failed": tally(Outcome::Failed),
                "flaky": tally(Outcome::Flaky),
                "ignored": tally(Outcome::Ignored),
                "filtered-out": tally(Outcome::FilteredOut),
                "not-run": tally(Outcome::NotRun),
            },
            "detail": self.detail,
            "elapsed_ms": u64::try_from(self.elapsed.as_millis()).unwrap_or(u64::MAX),
            "nextest_ms": u64::try_from(self.nextest_elapsed.as_millis()).unwrap_or(u64::MAX),
            "oracle_ms": u64::try_from(self.oracle_elapsed.as_millis()).unwrap_or(u64::MAX),
            "journal": self.journal_path.as_ref().map(|p| p.display().to_string()),
        })
    }

    /// Sortie humaine : groupes et liste complète des tests sautés, puis la ligne finale.
    #[must_use]
    pub fn human_lines(&self, plan_only: bool) -> Vec<String> {
        let j = &self.journal;
        let mut lines = Vec::new();
        let mut groups = std::collections::BTreeMap::<Vec<String>, usize>::new();
        for entry in j.entries.iter().filter(|e| e.decision == Decision::Skip) {
            let mut reasons: Vec<_> = entry.reasons.iter().map(describe_reason).collect();
            reasons.sort();
            reasons.dedup();
            if reasons.is_empty() {
                reasons.push("raison absente (journal incohérent)".to_owned());
            }
            *groups.entry(reasons).or_default() += 1;
        }
        for (reasons, count) in groups {
            lines.push(format!("  {count} sautés : {}", reasons.join(" ; ")));
        }
        for entry in j.entries.iter().filter(|e| e.decision == Decision::Skip) {
            let reasons: Vec<_> = entry.reasons.iter().map(describe_reason).collect();
            let why = if reasons.is_empty() {
                "raison absente (journal incohérent)".to_owned()
            } else {
                reasons.join(" ; ")
            };
            lines.push(format!(
                "  sauté : {} {} — {why}",
                entry.test.binary_id, entry.test.name
            ));
        }
        let journal = self.journal_path.as_ref().map_or_else(
            || "journal non écrit".to_owned(),
            |p| format!("journal {}", p.display()),
        );
        let journal = format!(
            "{journal}, {:.1} s dont nextest {:.1} s",
            self.elapsed.as_secs_f64(),
            self.nextest_elapsed.as_secs_f64()
        );
        let (run, skipped_count) = (j.counts.run, j.counts.skipped);
        let name = if plan_only {
            "kioku select plan"
        } else {
            "kioku test"
        };
        let last = match j.status {
            JournalStatus::Planned => {
                format!("{name}: PLAN — {run} à lancer, {skipped_count} sautés ({journal})")
            }
            JournalStatus::PartialGreen => {
                format!("{name}: VERT PARTIEL — {run} lancés, {skipped_count} sautés ({journal})")
            }
            JournalStatus::FullGreen => {
                format!("{name}: VERT COMPLET — {run} lancés ({journal})")
            }
            JournalStatus::Red => {
                let failed = count_of(j, Outcome::Failed);
                let why = self
                    .detail
                    .as_deref()
                    .map_or_else(|| format!("{failed} en échec"), str::to_owned);
                format!("{name}: ROUGE — {why}, {run} lancés, {skipped_count} sautés ({journal})")
            }
            JournalStatus::Incomplete => {
                let why = self.detail.as_deref().unwrap_or("résultats incomplets");
                format!("{name}: INCOMPLET — {why} ({journal})")
            }
        };
        lines.push(last);
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_request_detection() {
        assert!(merge_request_from(Some("1"), None, None));
        assert!(!merge_request_from(Some("0"), None, None));
        assert!(merge_request_from(None, Some("42"), None));
        assert!(merge_request_from(None, None, Some("pull_request")));
        assert!(!merge_request_from(None, None, Some("push")));
        assert!(!merge_request_from(None, None, None));
    }

    #[test]
    fn refs_cannot_inject_options() {
        assert!(check_ref("main").is_ok());
        assert!(check_ref("origin/main").is_ok());
        for bad in ["", "--upload-pack=x", "-x", "a\nb"] {
            assert!(check_ref(bad).is_err(), "{bad:?}");
        }
    }

    fn planned_with(
        scope: Scope,
        run: bool,
        targets: Option<Vec<crate::select::NextestTarget>>,
    ) -> Plan {
        let test = TestId {
            binary_id: "pkg".to_owned(),
            name: "t".to_owned(),
        };
        Plan {
            scope,
            base_commit: "c".repeat(40),
            graph_digest: "g".to_owned(),
            modified_files: Vec::new(),
            entries: vec![crate::select::JournalEntry {
                test,
                decision: if run { Decision::Run } else { Decision::Skip },
                reasons: vec![Reason::NewTest],
            }],
            targets,
            filter: None,
        }
    }

    #[test]
    fn only_an_empty_target_list_means_nothing_to_run() {
        use crate::select::{NextestTarget, PartialScope};
        let partial = || Scope::Partial(PartialScope::default());
        // A5 : aucun test lancé, mais une cible propriétaire à compiler → on lance nextest.
        let owner = planned_with(partial(), false, Some(vec![NextestTarget::Lib]));
        assert!(!nothing_to_run(&owner));
        // Aucune cible : rien à compiler ni à lancer.
        assert!(nothing_to_run(&planned_with(
            partial(),
            false,
            Some(Vec::new())
        )));
        // Le `-E` utilisateur a écarté toutes les cibles malgré des tests `Run`.
        assert!(nothing_to_run(&planned_with(
            partial(),
            true,
            Some(Vec::new())
        )));
        // Cibles inconnues (`None` : suite complète) : jamais « rien à lancer ».
        assert!(!nothing_to_run(&planned_with(partial(), false, None)));
        // Portée « tout » : toujours lancée.
        let everything = Scope::Everything {
            reasons: vec![Reason::ProtectedBranch("main".to_owned())],
        };
        assert!(!nothing_to_run(&planned_with(
            everything,
            false,
            Some(Vec::new())
        )));
    }

    #[test]
    fn human_summary_lists_every_skip_and_reason() {
        use crate::select::JournalEntry;
        let entry = |name: String, reasons| JournalEntry {
            test: TestId {
                binary_id: "pkg".into(),
                name,
            },
            decision: Decision::Skip,
            reasons,
        };
        let mut entries = vec![
            entry(
                "a".into(),
                vec![Reason::D1SkipIgnored, Reason::UserFilter("x".into())],
            ),
            entry(
                "b".into(),
                vec![Reason::UserFilter("x".into()), Reason::D1SkipIgnored],
            ),
            entry("c".into(), vec![]),
        ];
        for i in 0..25 {
            entries.push(entry(
                i.to_string(),
                vec![Reason::UserFilter(i.to_string())],
            ));
        }
        let report = TestReport {
            journal: Journal {
                schema: JOURNAL_SCHEMA.into(),
                status: JournalStatus::Planned,
                base_commit: String::new(),
                graph_digest: String::new(),
                modified_files: vec![],
                scope: JournalScope::Partial,
                scope_reasons: vec![],
                counts: JournalCounts::from_entries(&entries, entries.len(), 0),
                entries,
                outcomes: vec![],
                user_filter: None,
            },
            journal_path: None,
            exit_code: 0,
            native_exit_code: None,
            detail: None,
            notes: vec![],
            elapsed: Duration::ZERO,
            nextest_elapsed: Duration::ZERO,
            oracle_elapsed: Duration::ZERO,
        };
        let before = report.summary_json();
        let journal_before = serde_json::to_value(&report.journal).unwrap();
        let lines = report.human_lines(true);
        assert_eq!(lines.len(), 27 + 28 + 1);
        assert!(lines.iter().any(|line| line.contains("sauté : pkg 24")));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("sauté : pkg a") && line.contains("votre filtre x"))
        );
        assert_eq!(report.summary_json(), before);
        assert_eq!(
            serde_json::to_value(&report.journal).unwrap(),
            journal_before
        );
        assert_eq!(before["schema"], JOURNAL_SCHEMA);
        let mut missing = report;
        missing.journal.entries.truncate(3);
        assert!(
            missing
                .human_lines(true)
                .iter()
                .any(|line| line.contains("2 sautés"))
        );
        missing.journal.entries = vec![entry("c".into(), vec![])];
        assert!(
            missing
                .human_lines(true)
                .iter()
                .any(|line| line.contains("raison absente"))
        );
    }

    #[test]
    fn exit_codes_follow_the_contract() {
        assert_eq!(exit_code(JournalStatus::PartialGreen), 0);
        assert_eq!(exit_code(JournalStatus::Red), 1);
        assert_eq!(exit_code(JournalStatus::Incomplete), 3);
        assert_eq!(exit_code(JournalStatus::FullGreen), 5);
    }
}
