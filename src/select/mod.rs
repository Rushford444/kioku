//! Sélection prouvée sûre des tests touchés (option A, spec `SPEC-I1-T-SELECTION.md`).
//!
//! Contrat partagé entre les lots L1 à L5. Ce fichier appartient au chef : les mains
//! ne le modifient pas (elles demandent un changement dans leur rapport). Chaque
//! sous-module ne consomme que les types définis ici.
//!
//! Principe : comparer le dépôt au dernier état vert connu de la base, ne compiler que
//! les binaires de test nécessaires, n'exécuter que les tests qui atteignent un fichier
//! modifié dans le graphe fichier → fichier de la base, et journaliser chaque test non
//! relancé avec sa raison. Doute → élargir ; erreur → tout.
//!
//! Pourquoi le graphe de la BASE suffit (pas de ré-indexation du snapshot modifié) :
//! un fichier ne gagne une arête sortante que s'il est modifié ; tout test qui atteint
//! un fichier modifié est déjà sélectionné. Le premier arc neuf d'un chemin
//! test → … → fichier modifié part donc d'un fichier modifié que le test atteint par
//! des arcs de base : le test est déjà retenu. Les fichiers ajoutés, supprimés ou
//! renommés et les nouveaux binaires sont traités par les règles de doute (§ 4).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

pub mod base;
pub mod cache;
pub mod changes;
pub mod classify;
pub mod d1_observe;
pub mod exec;
pub mod graph;
pub mod journal;
pub mod learn;
pub mod plan;
pub(crate) mod seal;

/// Schéma du journal de sélection (S4).
pub const JOURNAL_SCHEMA: &str = "kioku/select/v1";
/// Schéma de l'inventaire de la base (v2 : scellé d'intégrité, voir [`seal`]).
pub const BASE_SCHEMA: &str = "kioku/select/base/v2";
/// Schéma du graphe fichier → fichier (v2 : scellé d'intégrité).
pub const GRAPH_SCHEMA: &str = "kioku/select/graph/v2";
/// Schéma des arêtes apprises par le filet couverture (S6) (v2 : `edge_count` et
/// `content_digest` obligatoires).
pub const LEARNED_SCHEMA: &str = "kioku/select/learned/v2";

/// Chemin relatif à la racine du dépôt, séparateur `/`, jamais `..`, jamais absolu.
pub type RelPath = String;

/// Identifiant de binaire nextest (`kdw-kioku`, `kdw-kioku::bin/kioku`, `kdw-kioku::cli`).
pub type BinaryId = String;

/// Un test : binaire nextest + nom complet du test dans ce binaire.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TestId {
    /// Binaire nextest.
    pub binary_id: BinaryId,
    /// Nom du test (`ci2::domain::validate::tests::x` pour la lib).
    pub name: String,
}

/// Nature d'un binaire de test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BinaryKind {
    /// Tests unitaires de la bibliothèque (`--lib`).
    Lib,
    /// Tests unitaires du binaire (`--bin NAME`).
    Bin,
    /// Binaire d'intégration (`--test NAME`).
    Test,
    /// Autre cible (exemple, bench) : jamais sélectionnée partiellement.
    Other,
}

/// Statut d'un test dans le dernier run de la base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BaseStatus {
    /// Vert.
    Pass,
    /// Rouge.
    Fail,
    /// Vert après relance (instable).
    Flaky,
    /// `#[ignore]` : nextest ne le lance pas par défaut.
    Ignored,
    /// Pas de résultat lisible : traité comme doute.
    Unknown,
}

/// Pourquoi une décision a été prise. Le texte reste lisible tel quel dans le journal.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "code", content = "detail", rename_all = "kebab-case")]
pub enum Reason {
    // --- S1 / S3 : tout ---
    /// Branche protégée (`main`) ou demande de merge : jamais de sélection.
    ProtectedBranch(String),
    /// Erreur : index absent/corrompu, base introuvable/non verte, diff impossible,
    /// inventaire illisible, version d'outil différente, délai dépassé…
    Error(String),
    // --- S2 : doute → tout ---
    /// Fichier de build/outillage modifié (`Cargo.toml`, `Cargo.lock`, `build.rs`,
    /// `.cargo/`, `rust-toolchain*`, `.config/`, `deny.toml`).
    BuildInput(RelPath),
    /// Fichier non Rust sous un arbre compilé, sans référence littérale trouvée.
    UnreferencedNonRust(RelPath),
    /// Macro (`macro_rules!`, `#[macro_export]`) ou crate proc-macro modifié.
    Macro(RelPath),
    /// Features, cfg, environnement de build ou toolchain différents de la base.
    BuildEnvironment(String),
    /// Fichier Rust hors des arbres connus (exemples, benches…).
    UnknownRustFile(RelPath),
    // --- S2 : élargissements partiels ---
    /// Le test atteint ce fichier modifié dans le graphe.
    ReachesModified { file: RelPath, via: Vec<RelPath> },
    /// Binaire nouveau (absent de l'inventaire de la base) : binaire entier.
    NewBinary(BinaryId),
    /// Fichier supprimé ou renommé : son binaire et tous les tests qui l'atteignaient.
    DeletedOrRenamed(RelPath),
    /// Le test lit une entrée hors dépôt déclarée : toujours lancé.
    ExternalInput(String),
    /// Test rouge, ignoré, instable ou inconnu dans la base : toujours lancé.
    BaseNotGreen(BaseStatus),
    /// Test absent de l'inventaire de la base (nouveau test d'un binaire connu).
    NewTest,
    /// Arête apprise par le filet couverture (S6).
    Learned { file: RelPath },
    /// Ajout proposé par d1 (S5 : d1 ne peut qu'ajouter).
    D1Added,
    // --- sauts ---
    /// Aucun chemin du test vers les fichiers modifiés.
    NoPathToModified { graph_digest: String },
    /// Proposition de saut de d1 ignorée (S5) — le test reste dans sa décision.
    D1SkipIgnored,
    /// Exclu par le filtre de l'utilisateur (`-E …`), comme le ferait nextest seul.
    UserFilter(String),
}

/// Cible Cargo à compiler et lancer.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", content = "name", rename_all = "kebab-case")]
pub enum NextestTarget {
    /// `--lib`.
    Lib,
    /// `--bin NAME`.
    Bin(String),
    /// `--test NAME`.
    Test(String),
}

/// Proposition d1 (S5) : ne peut qu'ajouter. Les sauts proposés sont ignorés et journalisés.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct D1Proposal {
    /// Tests à ajouter.
    #[serde(default)]
    pub add: Vec<TestId>,
    /// Tests que d1 voudrait sauter (ignoré).
    #[serde(default)]
    pub skip: Vec<TestId>,
}

/// Plan d'exécution (L4) : ce que `kioku test` va compiler et lancer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// Portée retenue par la classification.
    pub scope: Scope,
    /// Commit de la base.
    pub base_commit: String,
    /// Digest du graphe utilisé.
    pub graph_digest: String,
    /// Fichiers modifiés (après classification).
    pub modified_files: Vec<RelPath>,
    /// Une entrée par test de l'inventaire de la base (les tests nouveaux des binaires
    /// nouveaux sont ajoutés après exécution).
    pub entries: Vec<JournalEntry>,
    /// Cibles à compiler/lancer. `None` = suite complète (aucun argument de cible).
    pub targets: Option<Vec<NextestTarget>>,
    /// Expression `-E` transmise à nextest (`None` = aucun filtre).
    pub filter: Option<String>,
}

/// Décision finale pour un test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Decision {
    /// Relancé.
    Run,
    /// Sauté (vert partiel seulement).
    Skip,
}

/// Graphe fichier → fichier de la base (L2) + arêtes apprises (L3). Union : aucune source
/// ne retire d'arête.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphData {
    /// Toujours [`GRAPH_SCHEMA`].
    pub schema: String,
    /// Digest des entrées du graphe (sources, `Cargo.lock`, version de toolchain/rust-analyzer).
    pub digest: String,
    /// `a → {b…}` : le fichier `a` dépend du fichier `b` (utilise un symbole défini dans
    /// `b`, l'inclut par `mod`/`#[path]`/`include*!`, ou le cite littéralement).
    pub edges: BTreeMap<RelPath, BTreeSet<RelPath>>,
    /// Fichiers compilés dans chaque binaire (dep-info rustc) : autorité d'appartenance.
    pub binary_files: BTreeMap<BinaryId, BTreeSet<RelPath>>,
    /// Fichier non Rust → fichiers Rust qui le citent (littéral ou `include_*!`).
    pub literal_refs: BTreeMap<RelPath, BTreeSet<RelPath>>,
    /// Entrée hors dépôt déclarée (chemin absolu) → fichiers Rust qui la citent.
    pub external_inputs: BTreeMap<String, BTreeSet<RelPath>>,
    /// Fichiers racines de chaque test (module du test unitaire, ou fichier racine du
    /// binaire d'intégration ; `src/main.rs` ajouté si le test lance le CLI).
    pub test_roots: BTreeMap<TestId, BTreeSet<RelPath>>,
    /// Provenance des arêtes, pour l'audit (`scip`, `mod`, `path`, `include`, `literal`,
    /// `cli`, `learned`) : `"a -> b"` → sources.
    pub provenance: BTreeMap<String, BTreeSet<String>>,
}

/// Test de l'inventaire de la base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseTest {
    /// Nom complet du test.
    pub name: String,
    /// Statut au dernier run de la base.
    pub status: BaseStatus,
    /// Durée mesurée (ms), si connue.
    pub duration_ms: Option<u64>,
}

/// Binaire de l'inventaire de la base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseBinary {
    /// Identifiant nextest.
    pub binary_id: BinaryId,
    /// Nom de la cible Cargo (`kioku`, `cli`, `v2_runtime`).
    pub target_name: String,
    /// Nature.
    pub kind: BinaryKind,
    /// Fichier racine de la cible (`cargo metadata` `src_path`), relatif au dépôt.
    pub src_path: RelPath,
    /// Tests listés par nextest.
    pub tests: Vec<BaseTest>,
}

/// Inventaire du dernier état vert de la base (`main`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseInventory {
    /// Toujours [`BASE_SCHEMA`].
    pub schema: String,
    /// Commit de la base (40 hex).
    pub commit: String,
    /// `rustc -vV`, `cargo -V`, `cargo nextest --version` (première ligne de chacun).
    pub toolchain: BTreeMap<String, String>,
    /// Features actives lors de l'inventaire (ex. `[]` ou `["fixture-feature"]`).
    pub features: Vec<String>,
    /// Variables d'environnement de build pertinentes (`RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`,
    /// `RUSTDOCFLAGS`, `CARGO_BUILD_*`, `CARGO_PROFILE_*`, `RUSTC_WRAPPER`…) : nom → valeur
    /// hachée (jamais la valeur en clair).
    pub build_env: BTreeMap<String, String>,
    /// Le run de la base était-il entièrement vert (hors ignorés) ?
    pub green: bool,
    /// Binaires et tests.
    pub binaries: Vec<BaseBinary>,
    /// Digest du graphe associé ([`GraphData::digest`]).
    pub graph_digest: Option<String>,
}

/// Nature d'un changement entre la base et l'arbre de travail.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Change {
    /// Ajouté (y compris non suivi par git, hors `.gitignore`).
    Added { path: RelPath },
    /// Modifié (contenu ou mode).
    Modified { path: RelPath },
    /// Supprimé.
    Deleted { path: RelPath },
    /// Renommé (`git diff -M`).
    Renamed { from: RelPath, to: RelPath },
}

/// Changements base → arbre de travail (commits + index + non suivis).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSet {
    /// Commit de la base.
    pub base: String,
    /// Changements triés.
    pub changes: Vec<Change>,
}

/// Résultat de la classification § 4.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "kebab-case")]
pub enum Scope {
    /// Suite complète (S1, S2, S3). Au moins une raison.
    Everything { reasons: Vec<Reason> },
    /// Sélection partielle.
    Partial(PartialScope),
}

/// Ce que le plan doit élargir en plus du calcul d'atteignabilité.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartialScope {
    /// Fichiers à considérer « modifiés » pour l'atteignabilité (y compris les fichiers
    /// Rust qui citent un non-Rust modifié, et les fichiers supprimés/renommés de la base).
    pub modified: BTreeSet<RelPath>,
    /// Binaires à lancer en entier, avec leur raison.
    pub whole_binaries: BTreeMap<BinaryId, Reason>,
    /// Tests à lancer quoi qu'il arrive, avec leur raison.
    pub forced_tests: BTreeMap<TestId, Reason>,
    /// Changements sans effet sur les tests (ex. `docs/**`), pour le journal.
    pub inert: BTreeSet<RelPath>,
}

/// Une ligne du journal (S4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    /// Le test.
    pub test: TestId,
    /// Lancé ou sauté.
    pub decision: Decision,
    /// Raisons (au moins une).
    pub reasons: Vec<Reason>,
}

/// Erreur de sélection : toujours convertie en « tout » par l'appelant (S3).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SelectError {
    /// Entrée/sortie.
    #[error("i/o error on {path}: {message}")]
    Io {
        /// Chemin concerné.
        path: String,
        /// Message.
        message: String,
    },
    /// Commande externe (`git`, `cargo`, `rust-analyzer`) en échec ou introuvable.
    #[error("{command} failed: {message}")]
    Command {
        /// Commande.
        command: String,
        /// Message (borné, sans environnement).
        message: String,
    },
    /// Données illisibles ou incohérentes (JSON, protobuf SCIP, dep-info, schéma).
    #[error("invalid {what}: {message}")]
    Invalid {
        /// Ce qui était lu.
        what: String,
        /// Message.
        message: String,
    },
    /// Délai dépassé.
    #[error("timeout after {seconds} s: {what}")]
    Timeout {
        /// Opération.
        what: String,
        /// Délai.
        seconds: u64,
    },
}

impl SelectError {
    /// Raison S3 correspondante pour le journal.
    #[must_use]
    pub fn reason(&self) -> Reason {
        Reason::Error(self.to_string())
    }
}
