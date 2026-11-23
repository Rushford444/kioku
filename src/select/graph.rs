//! Lot L2 : graphe fichier → fichier de la base (union, aucune source ne retire d'arête).
//!
//! Sources, chacune avec sa provenance dans [`GraphData::provenance`] :
//! 1. `scip` — rust-analyzer : fichier utilisant un symbole → fichier le définissant, crates
//!    séparés ([`scip`]) ;
//! 2. `mod`, `path`, `include` — `mod x;`, `#[path = "…"]`, `include!`, `include_str!`,
//!    `include_bytes!` ([`scan`]) ;
//! 3. `literal` — chaînes littérales qui nomment un fichier existant du dépôt ;
//! 4. `dep-info` — fichiers compilés dans un binaire que le balayage syntaxique n'a pas
//!    reliés (les `.d` de rustc sont l'autorité d'appartenance fichier → binaire,
//!    [`GraphData::binary_files`]) ;
//! 5. `cli` — un fichier qui lance le binaire du paquet → `src/main.rs`.
//!
//! [`build`] produit le graphe ; [`load_or_build`] le met en cache par digest. Toute erreur
//! (outil absent, délai, entrée illisible) est remontée telle quelle : l'appelant joue
//! « erreur → tout » (S3).
//!
//! Note de contrat : [`GraphData::test_roots`] a pour clé un [`TestId`] (structure), que
//! `serde_json` ne sait pas écrire comme clé d'objet. Utiliser [`to_json`] / [`from_json`]
//! (liste de couples) et non `serde_json::to_*` directement sur [`GraphData`].

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::select::{GRAPH_SCHEMA, GraphData, RelPath, SelectError, TestId, seal};

pub mod depinfo;
pub mod scan;
pub mod scip;

use depinfo::{CargoContext, CargoTarget, NextestSuite};
use scan::{FileScan, ModuleTree, build_tree, dir_of, include_candidates, normalize_join};

/// Taille maximale d'un graphe en cache relu.
const MAX_CACHE_BYTES: u64 = 512 * 1024 * 1024;
/// Délai des sondes de version.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// Nombre maximal de fichiers parcourus dans le dépôt.
const MAX_REPO_FILES: usize = 500_000;
/// Taille maximale d'un littéral retenu comme chemin.
const MAX_PATH_LITERAL: usize = 512;
/// Octets de stderr conservés dans un message d'erreur.
const STDERR_TAIL: usize = 2048;

/// Options de construction du graphe.
#[derive(Debug, Clone)]
pub struct GraphOptions {
    /// Features cargo (`--features a,b`).
    pub features: Vec<String>,
    /// `--all-features` (prioritaire sur `features`).
    pub all_features: bool,
    /// Entrées hors dépôt déclarées (chemins absolus).
    pub external_inputs: Vec<PathBuf>,
    /// Exécutable rust-analyzer.
    pub rust_analyzer: PathBuf,
    /// Délai de `rust-analyzer scip`.
    pub scip_timeout: Duration,
    /// Délai de la compilation des binaires de test (`cargo nextest run --no-run`).
    pub build_timeout: Duration,
}

impl Default for GraphOptions {
    fn default() -> Self {
        Self {
            features: Vec::new(),
            all_features: false,
            external_inputs: Vec::new(),
            rust_analyzer: PathBuf::from("rust-analyzer"),
            scip_timeout: Duration::from_secs(300),
            build_timeout: Duration::from_secs(3600),
        }
    }
}

/// Durées des phases de [`build_with_stats`] (millisecondes).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildStats {
    /// Fichiers du dépôt parcourus.
    pub repo_files: usize,
    /// Fichiers Rust analysés.
    pub rust_files: usize,
    /// Documents de l'index SCIP.
    pub scip_documents: usize,
    /// Parcours, analyse syntaxique et digest.
    pub scan_ms: u64,
    /// `rust-analyzer scip` + décodage + arêtes.
    pub scip_ms: u64,
    /// `cargo nextest run --no-run` (compilation des binaires de test).
    pub compile_ms: u64,
    /// `cargo nextest list` + lecture des dep-info.
    pub list_ms: u64,
    /// Total.
    pub total_ms: u64,
}

fn millis(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

// --- exécution de commandes bornée -------------------------------------------------------

/// Commande externe à lancer avec un délai.
pub(crate) struct CommandSpec {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub timeout: Duration,
    /// Fichier qui reçoit la sortie standard (abandonnée si `None`).
    pub stdout: Option<PathBuf>,
    /// Nom lisible pour les erreurs.
    pub label: String,
}

fn temp_name(kind: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "kioku-select-{}-{}.{kind}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

fn io_error(path: &Path, error: &std::io::Error) -> SelectError {
    SelectError::Io {
        path: path.display().to_string(),
        message: error.to_string(),
    }
}

fn tail_of(bytes: &[u8]) -> String {
    let start = bytes.len().saturating_sub(STDERR_TAIL);
    String::from_utf8_lossy(&bytes[start..]).trim().to_string()
}

#[cfg(unix)]
fn group_kill_command(pid: u32) -> Option<Command> {
    if pid <= 1 {
        return None;
    }
    let mut command = Command::new("/bin/kill");
    command.args(["-KILL", "--", &format!("-{pid}")]);
    Some(command)
}

fn kill_group(child: &mut std::process::Child) {
    // Signal au groupe de notre propre enfant (créé avec `process_group(0)`), jamais par motif.
    #[cfg(unix)]
    if let Some(mut command) = group_kill_command(child.id()) {
        let _ = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Lance la commande ; échec si code non nul ou délai dépassé.
pub(crate) fn run_command(spec: &CommandSpec) -> Result<(), SelectError> {
    let stderr_path = temp_name("err");
    let result = run_with_stderr(spec, &stderr_path);
    let _ = std::fs::remove_file(&stderr_path);
    result
}

fn run_with_stderr(spec: &CommandSpec, stderr_path: &Path) -> Result<(), SelectError> {
    let stderr = File::create(stderr_path).map_err(|error| io_error(stderr_path, &error))?;
    let stdout = match &spec.stdout {
        Some(path) => Stdio::from(File::create(path).map_err(|error| io_error(path, &error))?),
        None => Stdio::null(),
    };
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(Stdio::from(stderr));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|error| SelectError::Command {
        command: spec.label.clone(),
        message: error.to_string(),
    })?;
    let deadline = Instant::now() + spec.timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return Ok(());
                }
                let detail = std::fs::read(stderr_path).map(|bytes| tail_of(&bytes));
                return Err(SelectError::Command {
                    command: spec.label.clone(),
                    message: format!("{status}: {}", detail.unwrap_or_default()),
                });
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    kill_group(&mut child);
                    return Err(SelectError::Timeout {
                        what: spec.label.clone(),
                        seconds: spec.timeout.as_secs(),
                    });
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                kill_group(&mut child);
                return Err(SelectError::Command {
                    command: spec.label.clone(),
                    message: error.to_string(),
                });
            }
        }
    }
}

/// Lance la commande et retourne sa sortie standard (bornée par [`MAX_CACHE_BYTES`]).
pub(crate) fn capture(spec: &CommandSpec) -> Result<Vec<u8>, SelectError> {
    let path = temp_name("out");
    let with_stdout = CommandSpec {
        program: spec.program.clone(),
        args: spec.args.clone(),
        cwd: spec.cwd.clone(),
        timeout: spec.timeout,
        stdout: Some(path.clone()),
        label: spec.label.clone(),
    };
    let result = run_command(&with_stdout).and_then(|()| {
        let length = std::fs::metadata(&path)
            .map_err(|error| io_error(&path, &error))?
            .len();
        if length > MAX_CACHE_BYTES {
            return Err(SelectError::Invalid {
                what: spec.label.clone(),
                message: "output larger than the bound".to_string(),
            });
        }
        std::fs::read(&path).map_err(|error| io_error(&path, &error))
    });
    let _ = std::fs::remove_file(&path);
    result
}

fn probe(program: &OsString, args: &[&str], label: &str) -> Result<String, SelectError> {
    let output = capture(&CommandSpec {
        program: program.clone(),
        args: args.iter().map(OsString::from).collect(),
        cwd: std::env::temp_dir(),
        timeout: PROBE_TIMEOUT,
        stdout: None,
        label: label.to_string(),
    })?;
    Ok(String::from_utf8_lossy(&output).trim().to_string())
}

// --- parcours et analyse du dépôt --------------------------------------------------------

/// Dépôt analysé : fichiers, analyses syntaxiques, références résolues et digest.
struct Scanned {
    files: BTreeSet<RelPath>,
    scans: BTreeMap<RelPath, FileScan>,
    /// Citant → fichiers existants nommés par un littéral.
    literal_edges: BTreeMap<RelPath, BTreeSet<RelPath>>,
    /// Citant → fichiers existants nommés par `include*!`.
    include_edges: BTreeMap<RelPath, BTreeSet<RelPath>>,
    digest: String,
}

fn walk_repo(repo: &Path) -> Result<BTreeSet<RelPath>, SelectError> {
    let mut files = BTreeSet::new();
    let mut stack = vec![(repo.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|error| io_error(&dir, &error))?;
        for entry in entries {
            let entry = entry.map_err(|error| io_error(&dir, &error))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let kind = entry
                .file_type()
                .map_err(|error| io_error(&entry.path(), &error))?;
            let relative = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if kind.is_dir() {
                if !matches!(
                    name.as_str(),
                    ".git" | "target" | "node_modules" | ".hg" | ".svn"
                ) {
                    stack.push((entry.path(), relative));
                }
            } else if kind.is_file() {
                files.insert(relative);
                if files.len() > MAX_REPO_FILES {
                    return Err(SelectError::Invalid {
                        what: "repository".to_string(),
                        message: "too many files".to_string(),
                    });
                }
            }
        }
    }
    Ok(files)
}

fn looks_like_path(literal: &str) -> bool {
    !literal.is_empty()
        && literal.len() <= MAX_PATH_LITERAL
        && !literal.contains("://")
        && !literal
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
}

fn is_rust(path: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("rs"))
}

fn hash_file(path: &Path) -> Result<[u8; 32], SelectError> {
    let file = File::open(path).map_err(|error| io_error(path, &error))?;
    let mut hasher = blake3::Hasher::new();
    hasher
        .update_reader(file)
        .map_err(|error| io_error(path, &error))?;
    Ok(*hasher.finalize().as_bytes())
}

/// Lit, hache et analyse un fichier Rust.
fn scan_one(repo: &Path, path: &RelPath) -> Result<(RelPath, [u8; 32], FileScan), SelectError> {
    let full = repo.join(path);
    let bytes = std::fs::read(&full).map_err(|error| io_error(&full, &error))?;
    let scan = scan::scan_source(&bytes).map_err(|error| match error {
        SelectError::Invalid { what, message } => SelectError::Invalid {
            what: format!("{what} {path}"),
            message,
        },
        other => other,
    })?;
    Ok((path.clone(), *blake3::hash(&bytes).as_bytes(), scan))
}

/// Analyse les fichiers en parallèle (tranches contiguës, résultats dans l'ordre des chemins :
/// le résultat et la première erreur rapportée sont déterministes).
fn scan_files(
    repo: &Path,
    paths: &[&RelPath],
) -> Result<Vec<(RelPath, [u8; 32], FileScan)>, SelectError> {
    let threads = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(8);
    let chunk = paths.len().div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        let handles: Vec<_> = paths
            .chunks(chunk)
            .map(|part| {
                scope.spawn(move || {
                    part.iter()
                        .map(|path| scan_one(repo, path))
                        .collect::<Result<Vec<_>, _>>()
                })
            })
            .collect();
        let mut out = Vec::with_capacity(paths.len());
        for handle in handles {
            let part = handle.join().map_err(|_| SelectError::Invalid {
                what: "rust source scan".to_string(),
                message: "worker thread panicked".to_string(),
            })?;
            out.extend(part?);
        }
        Ok(out)
    })
}

/// Parcourt le dépôt, analyse chaque `.rs`, résout littéraux et inclusions, calcule le digest.
fn scan_repo(repo: &Path, opts: &GraphOptions) -> Result<Scanned, SelectError> {
    let files = walk_repo(repo)?;
    let mut scans = BTreeMap::new();
    let mut hashes: BTreeMap<RelPath, [u8; 32]> = BTreeMap::new();
    let rust_files: Vec<&RelPath> = files.iter().filter(|path| is_rust(path)).collect();
    for (path, hash, scan) in scan_files(repo, &rust_files)? {
        hashes.insert(path.clone(), hash);
        scans.insert(path, scan);
    }
    let (literal_edges, include_edges) = resolve_references(&files, &scans);
    // Non-Rust cités : leur contenu entre dans le digest.
    let cited: BTreeSet<&RelPath> = literal_edges
        .values()
        .chain(include_edges.values())
        .flatten()
        .filter(|path| !is_rust(path))
        .collect();
    for path in cited {
        hashes.insert(path.clone(), hash_file(&repo.join(path))?);
    }
    let digest = compute_digest(repo, opts, &hashes)?;
    Ok(Scanned {
        files,
        scans,
        literal_edges,
        include_edges,
        digest,
    })
}

type EdgeMap = BTreeMap<RelPath, BTreeSet<RelPath>>;

fn resolve_references(
    files: &BTreeSet<RelPath>,
    scans: &BTreeMap<RelPath, FileScan>,
) -> (EdgeMap, EdgeMap) {
    let mut literal_edges: EdgeMap = BTreeMap::new();
    let mut include_edges: EdgeMap = BTreeMap::new();
    for (path, scan) in scans {
        let dir = dir_of(path);
        for literal in scan.literals.iter().filter(|l| looks_like_path(l)) {
            let mut candidates = Vec::new();
            if let Some(root_relative) = normalize_join("", literal) {
                candidates.push(root_relative);
            }
            if !is_rust(literal)
                && let Some(file_relative) = normalize_join(dir, literal)
            {
                candidates.push(file_relative);
            }
            for candidate in candidates {
                if candidate != *path && files.contains(&candidate) {
                    literal_edges
                        .entry(path.clone())
                        .or_default()
                        .insert(candidate);
                }
            }
        }
        for include in &scan.includes {
            for literal in &include.literals {
                for candidate in include_candidates(path, literal) {
                    if candidate != *path && files.contains(&candidate) {
                        include_edges
                            .entry(path.clone())
                            .or_default()
                            .insert(candidate);
                    }
                }
            }
        }
    }
    (literal_edges, include_edges)
}

fn compute_digest(
    repo: &Path,
    opts: &GraphOptions,
    hashes: &BTreeMap<RelPath, [u8; 32]>,
) -> Result<String, SelectError> {
    let mut state = blake3::Hasher::new();
    let mut field = |label: &str, value: &[u8]| {
        state.update(label.as_bytes());
        state.update(&(value.len() as u64).to_le_bytes());
        state.update(value);
    };
    field("domain", GRAPH_SCHEMA.as_bytes());
    field(
        "rustc",
        probe(&"rustc".into(), &["-vV"], "rustc -vV")?.as_bytes(),
    );
    field(
        "rust-analyzer",
        probe(
            &opts.rust_analyzer.clone().into_os_string(),
            &["--version"],
            "rust-analyzer --version",
        )?
        .as_bytes(),
    );
    field("all-features", &[u8::from(opts.all_features)]);
    let mut features = opts.features.clone();
    features.sort();
    features.dedup();
    field("features", features.join(",").as_bytes());
    let mut externals: Vec<String> = opts
        .external_inputs
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    externals.sort();
    field("external-inputs", externals.join("\n").as_bytes());
    for name in ["Cargo.lock", "Cargo.toml"] {
        let full = repo.join(name);
        match std::fs::metadata(&full) {
            Ok(_) => field(name, &hash_file(&full)?),
            Err(_) => field(name, b"absent"),
        }
    }
    for (path, hash) in hashes {
        field("file", path.as_bytes());
        field("hash", hash);
    }
    Ok(state.finalize().to_hex().to_string())
}

/// Digest des entrées du graphe : `.rs` du dépôt et non-Rust cités (avec leur contenu),
/// `Cargo.lock`, `Cargo.toml`, versions de rustc et de rust-analyzer, features, entrées
/// externes déclarées.
///
/// # Errors
/// Dépôt illisible, source Rust non analysable, outil de version introuvable.
pub fn digest(repo: &Path, opts: &GraphOptions) -> Result<String, SelectError> {
    Ok(scan_repo(repo, opts)?.digest)
}

// --- membres des crates et arêtes SCIP ---------------------------------------------------

/// Genre de cible → préfixe de clé de crate.
fn crate_prefix(target: &CargoTarget) -> &'static str {
    let has = |kind: &str| target.kinds.iter().any(|k| k == kind);
    if has("test") {
        "test"
    } else if has("bin") {
        "bin"
    } else if has("example") {
        "example"
    } else if has("bench") {
        "bench"
    } else if has("custom-build") {
        "build"
    } else {
        "lib"
    }
}

struct Trees {
    /// Clé de crate (`lib:kioku`, `test:cli`…) → arbre.
    by_crate: BTreeMap<String, ModuleTree>,
    /// Clés par cible cargo (`package/kind/name`).
    by_target: BTreeMap<(String, String, String), String>,
    /// Fichier → crates auxquels il appartient.
    membership: BTreeMap<RelPath, BTreeSet<String>>,
}

fn build_trees(targets: &[CargoTarget], scanned: &Scanned) -> Result<Trees, SelectError> {
    let exists = |path: &str| scanned.files.contains(path);
    let mut trees = Trees {
        by_crate: BTreeMap::new(),
        by_target: BTreeMap::new(),
        membership: BTreeMap::new(),
    };
    for target in targets {
        if !scanned.scans.contains_key(&target.src_path) {
            return Err(SelectError::Invalid {
                what: "cargo metadata".to_string(),
                message: format!(
                    "target {} has no scanned source {}",
                    target.name, target.src_path
                ),
            });
        }
        let prefix = crate_prefix(target);
        let key = format!("{prefix}:{}", target.name);
        let tree = build_tree(&target.src_path, &scanned.scans, &exists);
        for member in &tree.members {
            trees
                .membership
                .entry(member.clone())
                .or_default()
                .insert(key.clone());
        }
        trees.by_target.insert(
            (
                target.package.clone(),
                prefix.to_string(),
                target.name.clone(),
            ),
            key.clone(),
        );
        trees.by_crate.insert(key, tree);
    }
    Ok(trees)
}

/// Crates d'un fichier hors arbre (heuristique par chemin, héritée du prototype).
fn heuristic_crate(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    match parts.as_slice() {
        ["src", "main.rs"] => "bin:main".to_string(),
        ["src", "bin", rest @ ..] => format!(
            "bin:{}",
            rest.first().copied().unwrap_or("").trim_end_matches(".rs")
        ),
        ["src", ..] => "lib:".to_string(),
        ["tests", file] => format!("test:{}", file.trim_end_matches(".rs")),
        ["tests", dir, "main.rs"] => format!("test:{dir}"),
        ["tests", dir, ..] => format!("shared:{dir}"),
        [group @ ("examples" | "benches"), rest @ ..] => {
            format!(
                "{group}:{}",
                rest.first().copied().unwrap_or("").trim_end_matches(".rs")
            )
        }
        ["build.rs"] => "build:build".to_string(),
        _ => format!("other:{path}"),
    }
}

impl Trees {
    fn crates_of(&self, path: &str) -> BTreeSet<String> {
        let mut crates = self.membership.get(path).cloned().unwrap_or_default();
        crates.insert(heuristic_crate(path));
        crates
    }

    fn is_lib(&self, path: &str) -> bool {
        self.crates_of(path)
            .iter()
            .any(|key| key.starts_with("lib:"))
    }

    /// Un fichier ne dépend que de la lib, de son propre crate ou d'un dossier de tests
    /// partagé (`tests/<dir>/**`) ; jamais d'un autre crate de test ni du binaire.
    fn allowed(&self, user: &str, definer: &str) -> bool {
        if self.is_lib(definer) {
            return true;
        }
        let user_crates = self.crates_of(user);
        let definer_crates = self.crates_of(definer);
        if user_crates.intersection(&definer_crates).next().is_some() {
            return true;
        }
        definer_crates.iter().any(|key| key.starts_with("shared:")) && user.starts_with("tests/")
    }
}

fn rust_analyzer_config(opts: &GraphOptions) -> Option<String> {
    if opts.all_features {
        Some(r#"{"cargo":{"features":"all"}}"#.to_string())
    } else if opts.features.is_empty() {
        None
    } else {
        serde_json::to_string(&serde_json::json!({"cargo": {"features": opts.features}})).ok()
    }
}

// --- assemblage --------------------------------------------------------------------------

#[derive(Default)]
struct Edges {
    edges: BTreeMap<RelPath, BTreeSet<RelPath>>,
    provenance: BTreeMap<String, BTreeSet<String>>,
}

impl Edges {
    fn add(&mut self, from: &str, to: &str, source: &str) {
        if from == to {
            return;
        }
        self.edges
            .entry(from.to_string())
            .or_default()
            .insert(to.to_string());
        self.provenance
            .entry(format!("{from} -> {to}"))
            .or_default()
            .insert(source.to_string());
    }
}

/// Nombre d'arêtes par provenance (une arête de plusieurs sources compte pour chacune).
#[must_use]
pub fn provenance_counts(graph: &GraphData) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for sources in graph.provenance.values() {
        for source in sources {
            *counts.entry(source.clone()).or_insert(0) += 1;
        }
    }
    counts
}

/// Provenance des arêtes que la sélection ignore : exactement `{"mod"}`.
const PURE_MOD: &str = "mod";

/// Vue du graphe pour la SÉLECTION : le graphe complet moins les arêtes `mod` pures.
///
/// Une arête `mod` pure est une arête `parent → enfant` dont l'unique source est la
/// déclaration `mod enfant;`. Elle est retirée de la vue ; une arête qui a aussi `scip`,
/// `path`, `include`, `literal`, `cli` ou `learned` pour source est gardée, de même qu'une
/// arête sans provenance connue (fail closed). `digest`, `binary_files`, `literal_refs`,
/// `external_inputs` et `test_roots` sont recopiés tels quels ; la provenance des arêtes
/// retirées disparaît avec elles. Le graphe d'origine (union, audit) n'est pas modifié.
///
/// # Pourquoi c'est sûr
/// - Un parent dépend réellement de son enfant quand il en utilise un symbole (appel, type,
///   `pub use`) : SCIP voit cette dépendance, l'arête reste (avec ou sans `mod`). Une simple
///   déclaration `mod enfant;` sans usage ne propage pas de comportement du parent vers
///   l'enfant : modifier le parent ne change pas ce que l'enfant fait.
/// - Ajout, suppression et renommage de fichier ne passent pas par ces arêtes : les règles
///   de la matrice § 4 (`classify`) élargissent d'elles-mêmes (binaire entier, tests qui
///   atteignaient le fichier dans le graphe de la base).
/// - Ce que ni SCIP ni les autres sources ne voient (chemin dynamique) est rattrapé par le
///   filet couverture S6 (`learn`), qui n'ajoute que des racines.
///
/// Sans cette vue, `src/ci2/runtime/mod.rs` fait atteindre 375 tests au lieu de 7 : les
/// arêtes `mod` relient `lib.rs` et chaque `mod.rs` à tout leur sous-arbre.
#[must_use]
pub fn selection_view(graph: &GraphData) -> GraphData {
    let mut view = graph.clone();
    view.edges.clear();
    for (from, targets) in &graph.edges {
        let mut kept = BTreeSet::new();
        for to in targets {
            let key = format!("{from} -> {to}");
            let pure_mod = graph
                .provenance
                .get(&key)
                .is_some_and(|sources| sources.len() == 1 && sources.contains(PURE_MOD));
            if pure_mod {
                view.provenance.remove(&key);
            } else {
                kept.insert(to.clone());
            }
        }
        if !kept.is_empty() {
            view.edges.insert(from.clone(), kept);
        }
    }
    view
}

fn syntactic_edges(edges: &mut Edges, scanned: &Scanned, trees: &Trees, targets: &[CargoTarget]) {
    for tree in trees.by_crate.values() {
        for (parent, child, via_path) in &tree.links {
            edges.add(parent, child, if *via_path { "path" } else { "mod" });
        }
    }
    for (from, targets) in &scanned.include_edges {
        for to in targets {
            edges.add(from, to, "include");
        }
    }
    for (from, targets) in &scanned.literal_edges {
        for to in targets {
            edges.add(from, to, "literal");
        }
    }
    let bins: Vec<&CargoTarget> = targets
        .iter()
        .filter(|target| target.kinds.iter().any(|kind| kind == "bin"))
        .collect();
    for (path, scan) in &scanned.scans {
        if scan.launches_cli {
            for bin in &bins {
                edges.add(path, &bin.src_path, "cli");
            }
        }
    }
}

fn scip_edges(
    edges: &mut Edges,
    repo: &Path,
    opts: &GraphOptions,
    scanned: &Scanned,
    trees: &Trees,
) -> Result<usize, SelectError> {
    let config = rust_analyzer_config(opts);
    let bytes = scip::run_rust_analyzer(
        &opts.rust_analyzer,
        repo,
        config.as_deref(),
        opts.scip_timeout,
    )?;
    let documents = scip::decode_index(&bytes)?;
    let documents: Vec<scip::ScipDocument> = documents
        .into_iter()
        .filter(|document| scanned.files.contains(&document.path))
        .collect();
    let found = scip::file_edges(&documents, &|user, definer| trees.allowed(user, definer));
    for (user, definers) in &found {
        for definer in definers {
            edges.add(user, definer, "scip");
        }
    }
    Ok(documents.len())
}

fn find_target_key(trees: &Trees, suite: &NextestSuite) -> Option<String> {
    let prefix = match suite.kind.as_str() {
        "lib" => "lib",
        "bin" => "bin",
        "test" => "test",
        "example" => "example",
        "bench" => "bench",
        _ => return None,
    };
    if prefix == "lib" {
        // La cible lib porte le nom du paquet (avec `_`) ou son nom `[lib] name`.
        return trees
            .by_target
            .iter()
            .find(|((package, kind, _), _)| *package == suite.package && kind == "lib")
            .map(|(_, key)| key.clone());
    }
    trees
        .by_target
        .get(&(
            suite.package.clone(),
            prefix.to_string(),
            suite.binary_name.clone(),
        ))
        .cloned()
}

fn is_test_like(segment: &str) -> bool {
    matches!(segment, "tests" | "test") || segment.ends_with("_tests") || segment.ends_with("_test")
}

fn test_roots_of(
    suite: &NextestSuite,
    test: &str,
    tree: &ModuleTree,
    scanned: &Scanned,
    bin_sources: &[&str],
) -> BTreeSet<RelPath> {
    let mut roots = BTreeSet::new();
    match suite.kind.as_str() {
        "lib" | "bin" => {
            let mut path: Vec<String> = test.split("::").map(str::to_string).collect();
            path.pop();
            roots.insert(tree.resolve(&path).clone());
            if let (Some(last), Some(parent)) = (path.last(), tree.declaring_file(&path))
                && is_test_like(last)
            {
                roots.insert(parent.clone());
            }
            if suite.kind == "bin" {
                roots.insert(tree.root.clone());
            }
        }
        _ => {
            roots.insert(tree.root.clone());
        }
    }
    let launches_cli = tree.members.iter().any(|member| {
        scanned
            .scans
            .get(member)
            .is_some_and(|scan| scan.launches_cli)
    });
    if launches_cli {
        roots.extend(bin_sources.iter().map(|source| (*source).to_string()));
    }
    roots
}

/// Construit le graphe et mesure chaque phase.
///
/// # Errors
/// Voir [`build`].
#[allow(clippy::too_many_lines)]
pub fn build_with_stats(
    repo: &Path,
    opts: &GraphOptions,
) -> Result<(GraphData, BuildStats), SelectError> {
    let started = Instant::now();
    let mut stats = BuildStats::default();
    let canonical = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let roots: [&Path; 2] = [repo, &canonical];

    let phase = Instant::now();
    let scanned = scan_repo(repo, opts)?;
    stats.scan_ms = millis(phase);
    stats.repo_files = scanned.files.len();
    stats.rust_files = scanned.scans.len();

    let context = CargoContext {
        repo,
        features: &opts.features,
        all_features: opts.all_features,
        build_timeout: opts.build_timeout,
    };
    let targets = context.targets(&roots)?;
    let trees = build_trees(&targets, &scanned)?;

    let mut edges = Edges::default();
    syntactic_edges(&mut edges, &scanned, &trees, &targets);

    let phase = Instant::now();
    stats.scip_documents = scip_edges(&mut edges, repo, opts, &scanned, &trees)?;
    stats.scip_ms = millis(phase);

    let phase = Instant::now();
    context.compile_tests()?;
    stats.compile_ms = millis(phase);

    let phase = Instant::now();
    let suites = context.list_tests()?;
    let mut graph = GraphData {
        schema: GRAPH_SCHEMA.to_string(),
        digest: scanned.digest.clone(),
        ..GraphData::default()
    };
    let bin_sources: Vec<&str> = targets
        .iter()
        .filter(|target| target.kinds.iter().any(|kind| kind == "bin"))
        .map(|target| target.src_path.as_str())
        .collect();
    for suite in &suites {
        let key = find_target_key(&trees, suite).ok_or_else(|| SelectError::Invalid {
            what: "nextest list".to_string(),
            message: format!("no cargo target for {}", suite.binary_id),
        })?;
        let tree = &trees.by_crate[&key];
        let compiled = depinfo::binary_files(suite, &roots)?;
        add_dep_info_edges(&mut edges, tree, &compiled, &scanned);
        graph.binary_files.insert(suite.binary_id.clone(), compiled);
        for name in suite.tests.keys() {
            graph.test_roots.insert(
                TestId {
                    binary_id: suite.binary_id.clone(),
                    name: name.clone(),
                },
                test_roots_of(suite, name, tree, &scanned, &bin_sources),
            );
        }
    }
    stats.list_ms = millis(phase);

    for (citing, targets) in scanned.literal_edges.iter().chain(&scanned.include_edges) {
        for target in targets.iter().filter(|target| !is_rust(target)) {
            graph
                .literal_refs
                .entry(target.clone())
                .or_default()
                .insert(citing.clone());
        }
    }
    graph.external_inputs = external_inputs(opts, &scanned)?;
    graph.edges = edges.edges;
    graph.provenance = edges.provenance;
    stats.total_ms = millis(started);
    Ok((graph, stats))
}

/// Fichiers compilés dans le binaire que ni les modules ni les inclusions n'ont reliés :
/// arête racine → fichier (le dep-info de rustc fait foi).
fn add_dep_info_edges(
    edges: &mut Edges,
    tree: &ModuleTree,
    compiled: &BTreeSet<RelPath>,
    scanned: &Scanned,
) {
    let mut known: BTreeSet<&RelPath> = tree.members.iter().collect();
    for member in &tree.members {
        known.extend(scanned.include_edges.get(member).into_iter().flatten());
        known.extend(scanned.literal_edges.get(member).into_iter().flatten());
    }
    for file in compiled {
        if !known.contains(file) && scanned.files.contains(file) {
            edges.add(&tree.root, file, "dep-info");
        }
    }
}

fn external_inputs(
    opts: &GraphOptions,
    scanned: &Scanned,
) -> Result<BTreeMap<String, BTreeSet<RelPath>>, SelectError> {
    let mut out = BTreeMap::new();
    for path in &opts.external_inputs {
        if !path.is_absolute() {
            return Err(SelectError::Invalid {
                what: "external input".to_string(),
                message: format!("{} is not absolute", path.display()),
            });
        }
        let needle = path.display().to_string();
        let citing: BTreeSet<RelPath> = scanned
            .scans
            .iter()
            .filter(|(_, scan)| {
                scan.literals
                    .iter()
                    .any(|literal| literal.contains(&needle))
            })
            .map(|(file, _)| file.clone())
            .collect();
        out.insert(needle, citing);
    }
    Ok(out)
}

/// Construit le graphe fichier → fichier de `repo`.
///
/// Lance `cargo metadata`, `rust-analyzer scip`, `cargo nextest run --no-run` puis
/// `cargo nextest list`. Le dépôt n'est pas modifié (sorties dans `target/` et `/tmp`).
///
/// # Errors
/// Dépôt ou source illisible, outil absent ou en échec, délai dépassé
/// ([`SelectError::Timeout`]), index SCIP ou inventaire incohérent.
pub fn build(repo: &Path, opts: &GraphOptions) -> Result<GraphData, SelectError> {
    build_with_stats(repo, opts).map(|(graph, _)| graph)
}

// --- cache -------------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct TestRootsEntry {
    test: TestId,
    roots: BTreeSet<RelPath>,
}

/// Forme JSON de [`GraphData`] : `test_roots` en liste de couples (clé de map non textuelle).
#[derive(Serialize, Deserialize)]
struct GraphFile {
    schema: String,
    digest: String,
    edges: BTreeMap<RelPath, BTreeSet<RelPath>>,
    binary_files: BTreeMap<String, BTreeSet<RelPath>>,
    literal_refs: BTreeMap<RelPath, BTreeSet<RelPath>>,
    external_inputs: BTreeMap<String, BTreeSet<RelPath>>,
    test_roots: Vec<TestRootsEntry>,
    provenance: BTreeMap<String, BTreeSet<String>>,
}

/// Message d'erreur quand le scellé ne correspond pas au contenu (A4).
const GRAPH_INCONSISTENT: &str = "graphe incohérent : contenu altéré ou tronqué";

fn graph_file(graph: &GraphData) -> GraphFile {
    GraphFile {
        schema: graph.schema.clone(),
        digest: graph.digest.clone(),
        edges: graph.edges.clone(),
        binary_files: graph.binary_files.clone(),
        literal_refs: graph.literal_refs.clone(),
        external_inputs: graph.external_inputs.clone(),
        test_roots: graph
            .test_roots
            .iter()
            .map(|(test, roots)| TestRootsEntry {
                test: test.clone(),
                roots: roots.clone(),
            })
            .collect(),
        provenance: graph.provenance.clone(),
    }
}

/// Comptes scellés : arêtes (couples fichier → fichier), tests, racines de test (couples
/// test → racine) et binaires.
fn graph_counts(graph: &GraphData) -> [(&'static str, usize); 4] {
    [
        ("edge_count", graph.edges.values().map(BTreeSet::len).sum()),
        ("test_count", graph.test_roots.len()),
        (
            "test_root_count",
            graph.test_roots.values().map(BTreeSet::len).sum(),
        ),
        ("binary_count", graph.binary_files.len()),
    ]
}

/// Sérialise un graphe en JSON, scellé (comptes + `content_digest`, voir [`crate::select::seal`]).
///
/// # Errors
/// Échec de sérialisation.
pub fn to_json(graph: &GraphData) -> Result<Vec<u8>, SelectError> {
    let sealed = seal::seal("graph", &graph_file(graph), &graph_counts(graph))?;
    serde_json::to_vec(&sealed).map_err(|error| SelectError::Invalid {
        what: "graph".to_string(),
        message: error.to_string(),
    })
}

/// Relit un graphe écrit par [`to_json`] ; refuse un autre schéma (dont l'ancien `v1`, sans
/// scellé) et tout fichier dont les comptes ou le `content_digest` ne correspondent pas au
/// contenu relu (arête ou racine retirée, fichier tronqué…).
///
/// # Errors
/// JSON illisible, schéma différent de [`GRAPH_SCHEMA`] ou scellé invalide.
pub fn from_json(bytes: &[u8]) -> Result<GraphData, SelectError> {
    let invalid = |error: serde_json::Error| SelectError::Invalid {
        what: "graph".to_string(),
        message: error.to_string(),
    };
    let document: serde_json::Value = serde_json::from_slice(bytes).map_err(invalid)?;
    let file = GraphFile::deserialize(&document).map_err(invalid)?;
    if file.schema != GRAPH_SCHEMA {
        return Err(SelectError::Invalid {
            what: "graph".to_string(),
            message: format!("schema {} is not {GRAPH_SCHEMA}", file.schema),
        });
    }
    let graph = GraphData {
        schema: file.schema,
        digest: file.digest,
        edges: file.edges,
        binary_files: file.binary_files,
        literal_refs: file.literal_refs,
        external_inputs: file.external_inputs,
        test_roots: file
            .test_roots
            .into_iter()
            .map(|entry| (entry.test, entry.roots))
            .collect(),
        provenance: file.provenance,
    };
    // Le scellé porte sur le graphe relu et re-sérialisé : un doublon de `test_roots` écrasé à
    // la relecture, un champ retiré ou une arête en moins ne peuvent pas le satisfaire.
    seal::verify(
        "graph",
        GRAPH_INCONSISTENT,
        &document,
        &graph_file(&graph),
        &graph_counts(&graph),
    )?;
    Ok(graph)
}

fn cache_file(cache_dir: &Path, digest: &str) -> PathBuf {
    cache_dir.join(format!("graph-{digest}.json"))
}

fn read_cache(path: &Path, digest: &str) -> Option<GraphData> {
    let length = std::fs::metadata(path).ok()?.len();
    if length > MAX_CACHE_BYTES {
        return None;
    }
    let graph = from_json(&std::fs::read(path).ok()?).ok()?;
    (graph.digest == digest).then_some(graph)
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), SelectError> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(directory).map_err(|error| io_error(directory, &error))?;
    let temporary = directory.join(format!(
        ".graph-tmp-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| {
        let mut file = File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temporary);
        return Err(io_error(path, &error));
    }
    Ok(())
}

/// Retourne le graphe en cache (`cache_dir/graph-<digest>.json`) s'il existe et se relit au
/// bon schéma ; sinon le construit puis l'écrit atomiquement (fichier temporaire + rename).
/// Un fichier corrompu n'est jamais utilisé : le graphe est reconstruit et le remplace.
///
/// # Errors
/// Voir [`build`] ; échec d'écriture du cache.
pub fn load_or_build(
    repo: &Path,
    cache_dir: &Path,
    opts: &GraphOptions,
) -> Result<GraphData, SelectError> {
    let scanned = scan_repo(repo, opts)?;
    let path = cache_file(cache_dir, &scanned.digest);
    if let Some(graph) = read_cache(&path, &scanned.digest) {
        return Ok(graph);
    }
    let graph = build(repo, opts)?;
    if graph.digest != scanned.digest {
        // Les sources ont changé pendant la construction : on ne fige pas un graphe incohérent.
        return Err(SelectError::Invalid {
            what: "graph".to_string(),
            message: "repository changed while the graph was being built".to_string(),
        });
    }
    write_atomically(&path, &to_json(&graph)?)?;
    Ok(graph)
}

#[cfg(all(test, unix))]
mod kill_command_tests {
    use super::group_kill_command;

    #[test]
    fn group_kill_uses_an_unambiguous_child_group_operand() {
        assert!(group_kill_command(0).is_none());
        assert!(group_kill_command(1).is_none());
        let command = group_kill_command(12345).unwrap();
        assert_eq!(command.get_program(), "/bin/kill");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["-KILL", "--", "-12345"]
        );
    }
}
