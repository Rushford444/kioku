//! Lot L3 : filet couverture (S6).
//!
//! La couverture réelle par test, prise sur la base, révèle les dépendances que le graphe
//! statique ne voit pas : un test qui exécute un fichier non atteint depuis ses racines
//! par la clôture de `graph.edges` gagne une racine supplémentaire. Le filet ne fait
//! qu'AJOUTER ([`apply`] n'enlève jamais une racine ni une arête) et lève une alerte par
//! arête apprise. Un test sans racine connue reste « inconnu » : le plan le traite en doute.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::journal::{read_bounded, write_atomic};
use super::seal;
use super::{GraphData, LEARNED_SCHEMA, RelPath, SelectError, TestId};

/// Arêtes apprises sur la base (test → fichiers exécutés non reliés par le graphe).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearnedEdges {
    /// Toujours [`LEARNED_SCHEMA`].
    pub schema: String,
    /// Commit de la base. [`learn`] le laisse vide : l'appelant l'estampille avant [`write`],
    /// qui scelle le fichier (`edge_count` + `content_digest`, voir [`super::seal`]).
    pub base_commit: String,
    /// Digest du graphe sur lequel la couverture a été comparée.
    pub graph_digest: String,
    /// Fichiers à ajouter aux racines de chaque test. Sérialisé en liste
    /// `[{test, files}]` car JSON n'accepte pas une structure comme clé d'objet.
    #[serde(with = "test_map")]
    pub edges: BTreeMap<TestId, BTreeSet<RelPath>>,
    /// Une alerte par arête apprise, et par test sans racine connue.
    pub alerts: Vec<LearnAlert>,
}

/// Alerte du filet couverture.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LearnAlert {
    /// Le test concerné.
    pub test: TestId,
    /// Le fichier exécuté mais non relié (`None` : alerte sur le test entier).
    pub file: Option<RelPath>,
    /// Message lisible.
    pub message: String,
}

mod test_map {
    use super::{BTreeMap, BTreeSet, Deserialize, RelPath, Serialize, TestId};
    use serde::de::Error as _;
    use serde::{Deserializer, Serializer};

    #[derive(Serialize, Deserialize)]
    struct Entry {
        test: TestId,
        files: BTreeSet<RelPath>,
    }

    pub fn serialize<S: Serializer>(
        map: &BTreeMap<TestId, BTreeSet<RelPath>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(map.iter().map(|(test, files)| Entry {
            test: test.clone(),
            files: files.clone(),
        }))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<TestId, BTreeSet<RelPath>>, D::Error> {
        let entries = Vec::<Entry>::deserialize(deserializer)?;
        let mut map = BTreeMap::new();
        for entry in entries {
            let label = format!("{} {}", entry.test.binary_id, entry.test.name);
            if map.insert(entry.test, entry.files).is_some() {
                return Err(D::Error::custom(format!("duplicate test {label}")));
            }
        }
        Ok(map)
    }
}

// ---------------------------------------------------------------------------------------
// Lecture de la couverture
// ---------------------------------------------------------------------------------------

/// Chemin relatif au dépôt : séparateur `/`, ni vide, ni absolu, ni `.`/`..`/segment vide,
/// ni octet nul, ni lettre de lecteur.
fn is_repo_relative(path: &str) -> bool {
    let bad_segment = path
        .split('/')
        .any(|c| c.is_empty() || c == "." || c == "..");
    let drive_letter = path.as_bytes().get(1) == Some(&b':');
    !(path.is_empty()
        || path.starts_with('/')
        || path.contains(['\\', '\0'])
        || bad_segment
        || drive_letter)
}

fn invalid(what: &str, message: impl Into<String>) -> SelectError {
    SelectError::Invalid {
        what: what.to_owned(),
        message: message.into(),
    }
}

/// Lit le format `cov/lines.json` : `{"<binaire> <test>": {"<fichier>": [lignes…]}}`.
///
/// La clé est coupée au PREMIER espace (un identifiant de binaire n'en contient pas). Ne
/// garde que les fichiers du dépôt (chemins relatifs sans `..`, voir [`is_repo_relative`]) qui
/// ont au moins une ligne exécutée ; un test sans fichier gardé est omis.
///
/// # Errors
/// [`SelectError::Invalid`] si le JSON est illisible, si une clé n'a pas de nom de binaire ou
/// de test, ou si les lignes d'un fichier ne sont pas une liste (fail closed).
pub fn coverage_from_json(text: &str) -> Result<BTreeMap<TestId, BTreeSet<RelPath>>, SelectError> {
    const WHAT: &str = "coverage lines.json";
    let raw: BTreeMap<String, BTreeMap<String, serde_json::Value>> =
        serde_json::from_str(text).map_err(|e| invalid(WHAT, e.to_string()))?;
    let mut out = BTreeMap::new();
    for (key, files) in raw {
        let (binary_id, name) = match key.split_once(' ') {
            Some((b, n)) if !b.is_empty() && !n.is_empty() => (b.to_owned(), n.to_owned()),
            _ => return Err(invalid(WHAT, format!("malformed test key {key:?}"))),
        };
        let mut kept = BTreeSet::new();
        for (file, lines) in files {
            let serde_json::Value::Array(lines) = lines else {
                return Err(invalid(WHAT, format!("lines of {file:?} are not a list")));
            };
            if !lines.is_empty() && is_repo_relative(&file) {
                kept.insert(file);
            }
        }
        if !kept.is_empty() {
            out.insert(TestId { binary_id, name }, kept);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// learn / apply
// ---------------------------------------------------------------------------------------

/// Clôture de `edges` depuis `roots` (les racines sont incluses).
fn closure(
    edges: &BTreeMap<RelPath, BTreeSet<RelPath>>,
    roots: &BTreeSet<RelPath>,
) -> BTreeSet<RelPath> {
    let mut seen = BTreeSet::new();
    let mut todo: Vec<&RelPath> = roots.iter().collect();
    while let Some(file) = todo.pop() {
        if !seen.insert(file.clone()) {
            continue;
        }
        if let Some(next) = edges.get(file) {
            todo.extend(next.iter().filter(|n| !seen.contains(*n)));
        }
    }
    seen
}

/// Compare la couverture au graphe : pour chaque test, les fichiers exécutés non atteints
/// depuis ses `test_roots` par la clôture de `graph.edges` deviennent des arêtes apprises
/// (test → fichier) avec une alerte. Un test sans racine connue ne produit qu'une alerte
/// « racine inconnue » : on ne peut pas dire ce qui lui manque, le plan le traitera en doute.
#[must_use]
pub fn learn(graph: &GraphData, coverage: &BTreeMap<TestId, BTreeSet<RelPath>>) -> LearnedEdges {
    let mut edges = BTreeMap::new();
    let mut alerts = Vec::new();
    for (test, executed) in coverage {
        let roots = graph.test_roots.get(test).filter(|r| !r.is_empty());
        let Some(roots) = roots else {
            alerts.push(LearnAlert {
                test: test.clone(),
                file: None,
                message: "racine inconnue : le graphe ne donne aucune racine à ce test".to_owned(),
            });
            continue;
        };
        let reached = closure(&graph.edges, roots);
        let missing: BTreeSet<RelPath> = executed.difference(&reached).cloned().collect();
        for file in &missing {
            alerts.push(LearnAlert {
                test: test.clone(),
                file: Some(file.clone()),
                message: format!("exécuté sous couverture mais non relié par le graphe : {file}"),
            });
        }
        if !missing.is_empty() {
            edges.insert(test.clone(), missing);
        }
    }
    alerts.sort();
    LearnedEdges {
        schema: LEARNED_SCHEMA.to_owned(),
        base_commit: String::new(),
        graph_digest: graph.digest.clone(),
        edges,
        alerts,
    }
}

/// Applique les arêtes apprises : chaque fichier devient une racine supplémentaire du test
/// (élargissement), avec la provenance `learned`. **Ne retire jamais rien** : les fichiers
/// atteignables après `apply` sont un sur-ensemble de ceux d'avant, pour tous les tests.
///
/// Un test sans racine connue dans `graph.test_roots` est laissé tel quel : lui créer une
/// racine à partir des seuls fichiers appris lui ferait perdre son statut « inconnu » (doute).
pub fn apply(graph: &mut GraphData, learned: &LearnedEdges) {
    for (test, files) in &learned.edges {
        let Some(roots) = graph.test_roots.get_mut(test).filter(|r| !r.is_empty()) else {
            continue;
        };
        for file in files {
            if roots.insert(file.clone()) {
                graph
                    .provenance
                    .entry(format!("test:{} {} -> {file}", test.binary_id, test.name))
                    .or_default()
                    .insert("learned".to_owned());
            }
        }
    }
}

/// Message d'erreur quand le scellé ne correspond pas au contenu (A4).
const INCONSISTENT: &str = "arêtes apprises incohérentes : contenu altéré ou tronqué";

/// Nombre de couples test → fichier.
fn edge_count(learned: &LearnedEdges) -> usize {
    learned.edges.values().map(BTreeSet::len).sum()
}

/// Écrit les arêtes apprises de façon atomique, scellées : le fichier porte `edge_count` et
/// `content_digest` (calculés ici, sur la valeur reçue : `base_commit` doit déjà être
/// estampillé). Schéma vérifié avant écriture.
///
/// # Errors
/// [`SelectError::Invalid`] si le schéma est faux, [`SelectError::Io`] si l'écriture échoue.
pub fn write(path: &Path, learned: &LearnedEdges) -> Result<(), SelectError> {
    if learned.schema != LEARNED_SCHEMA {
        return Err(invalid(
            "learned edges",
            format!("unknown schema {:?}", learned.schema),
        ));
    }
    let sealed = seal::seal(
        "learned edges",
        learned,
        &[("edge_count", edge_count(learned))],
    )?;
    let mut bytes =
        serde_json::to_vec_pretty(&sealed).map_err(|e| invalid("learned edges", e.to_string()))?;
    bytes.push(b'\n');
    write_atomic(path, &bytes)
}

/// Lit des arêtes apprises (schéma et scellé vérifiés).
///
/// # Errors
/// [`SelectError::Io`] si le fichier est illisible, [`SelectError::Invalid`] si le JSON, le
/// schéma (y compris l'ancien `v1`, sans scellé) ou le scellé est invalide : `edge_count` ou
/// `content_digest` absent ou différent du contenu relu (arête retirée, alerte modifiée,
/// estampille changée, fichier tronqué…).
pub fn read(path: &Path) -> Result<LearnedEdges, SelectError> {
    let bytes = read_bounded(path)?;
    let document: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| invalid("learned edges", e.to_string()))?;
    let learned = LearnedEdges::deserialize(&document)
        .map_err(|e| invalid("learned edges", e.to_string()))?;
    if learned.schema != LEARNED_SCHEMA {
        let hint = if learned.schema.ends_with("/v1") {
            " ; ancien format sans scellé : refaire `kioku select learn`"
        } else {
            ""
        };
        return Err(invalid(
            "learned edges",
            format!("unknown schema {:?}{hint}", learned.schema),
        ));
    }
    seal::verify(
        "learned edges",
        INCONSISTENT,
        &document,
        &learned,
        &[("edge_count", edge_count(&learned))],
    )?;
    Ok(learned)
}

// ---------------------------------------------------------------------------------------
// run_coverage : couverture par test (port de cov_run.py)
// ---------------------------------------------------------------------------------------

/// Options de [`run_coverage`].
#[derive(Debug, Clone)]
pub struct CoverageOptions {
    /// Dossier de `llvm-profdata` et `llvm-cov`. `None` : `<sysroot>/lib/rustlib/<hôte>/bin`
    /// (composant `llvm-tools`), puis `PATH`.
    pub llvm_bin_dir: Option<PathBuf>,
    /// Dossier cible Cargo. `None` : `<repo>/target/select-cov` (jamais le `target/` normal).
    pub target_dir: Option<PathBuf>,
    /// Features Cargo (`--features a,b`), pour coller à l'inventaire de la base.
    pub features: Vec<String>,
    /// Délai total borné (compilation + exécution + traitement).
    pub timeout: Duration,
    /// Délai d'un test ; au-delà, le test est tué et reste sans couverture.
    pub test_timeout: Duration,
    /// Nombre de tests en parallèle (au moins 1).
    pub jobs: usize,
}

impl Default for CoverageOptions {
    fn default() -> Self {
        Self {
            llvm_bin_dir: None,
            target_dir: None,
            features: Vec::new(),
            timeout: Duration::from_secs(3600),
            test_timeout: Duration::from_secs(180),
            jobs: thread::available_parallelism().map_or(1, usize::from),
        }
    }
}

/// Échéance globale partagée.
#[derive(Clone, Copy)]
struct Deadline {
    at: Instant,
    total_secs: u64,
}

impl Deadline {
    fn expired(self) -> bool {
        Instant::now() >= self.at
    }

    fn timeout(self, what: &str) -> SelectError {
        SelectError::Timeout {
            what: what.to_owned(),
            seconds: self.total_secs,
        }
    }
}

fn command_error(command: &str, message: impl Into<String>) -> SelectError {
    SelectError::Command {
        command: command.to_owned(),
        message: message.into(),
    }
}

/// Attend la fin d'un enfant jusqu'à `until` ; le tue au-delà. `None` : tué par délai.
fn wait_until(
    child: &mut Child,
    until: Instant,
    what: &str,
) -> Result<Option<ExitStatus>, SelectError> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(Some(status)),
            Ok(None) => {}
            Err(e) => return Err(command_error(what, e.to_string())),
        }
        if Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Lit `reader` jusqu'à `limit` octets conservés, puis vide le reste (l'enfant ne bloque pas).
fn read_capped(mut reader: impl std::io::Read, limit: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let room = limit.saturating_sub(kept.len());
        truncated |= n > room;
        kept.extend_from_slice(&buf[..n.min(room)]);
    }
    Ok((kept, truncated))
}

const STDOUT_LIMIT: usize = 512 * 1024 * 1024;
const STDERR_LIMIT: usize = 64 * 1024;

type CaptureResult = Option<std::io::Result<(Vec<u8>, bool)>>;

/// Lance une commande avec sortie capturée et bornée, sous l'échéance globale.
fn capture_result(
    result: thread::Result<CaptureResult>,
    what: &str,
    stream: &str,
) -> Result<Vec<u8>, SelectError> {
    let (bytes, truncated) = result
        .map_err(|_| command_error(what, format!("{stream} reader panicked")))?
        .ok_or_else(|| command_error(what, format!("{stream} unavailable")))?
        .map_err(|e| command_error(what, e.to_string()))?;
    if truncated {
        return Err(command_error(
            what,
            format!("{stream} exceeded capture limit"),
        ));
    }
    Ok(bytes)
}

fn run_captured(
    cmd: Command,
    what: &str,
    deadline: Deadline,
) -> Result<(ExitStatus, Vec<u8>, String), SelectError> {
    run_captured_limits(cmd, what, deadline, STDOUT_LIMIT, STDERR_LIMIT)
}

fn run_captured_limits(
    cmd: Command,
    what: &str,
    deadline: Deadline,
    stdout_limit: usize,
    stderr_limit: usize,
) -> Result<(ExitStatus, Vec<u8>, String), SelectError> {
    run_captured_readers(
        cmd,
        what,
        deadline,
        move |s| s.map(|s| read_capped(s, stdout_limit)),
        move |s| s.map(|s| read_capped(s, stderr_limit)),
    )
}

fn run_captured_readers(
    mut cmd: Command,
    what: &str,
    deadline: Deadline,
    stdout_reader: impl FnOnce(
        Option<std::process::ChildStdout>,
    ) -> Option<std::io::Result<(Vec<u8>, bool)>>
    + Send
    + 'static,
    stderr_reader: impl FnOnce(
        Option<std::process::ChildStderr>,
    ) -> Option<std::io::Result<(Vec<u8>, bool)>>
    + Send
    + 'static,
) -> Result<(ExitStatus, Vec<u8>, String), SelectError> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| command_error(what, e.to_string()))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_thread = thread::spawn(move || stdout_reader(stdout));
    let err_thread = thread::spawn(move || stderr_reader(stderr));
    let Some(status) = wait_until(&mut child, deadline.at, what)? else {
        // Délai dépassé : on ne joint pas les lecteurs (un petit-enfant peut garder les tubes).
        return Err(deadline.timeout(what));
    };
    // Les descendants peuvent conserver les tubes après la sortie du parent.
    while !out_thread.is_finished() || !err_thread.is_finished() {
        if deadline.expired() {
            return Err(deadline.timeout(what));
        }
        thread::sleep(Duration::from_millis(10));
    }
    if deadline.expired() {
        return Err(deadline.timeout(what));
    }
    let out = capture_result(out_thread.join(), what, "stdout")?;
    let err = capture_result(err_thread.join(), what, "stderr")?;
    // Message d'erreur borné, jamais l'environnement.
    let tail = String::from_utf8_lossy(&err);
    let tail: String = tail
        .chars()
        .rev()
        .take(2000)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Ok((status, out, tail))
}

struct LlvmTools {
    profdata: PathBuf,
    cov: PathBuf,
}

fn find_in(dir: &Path) -> Option<LlvmTools> {
    let profdata = dir.join("llvm-profdata");
    let cov = dir.join("llvm-cov");
    (profdata.is_file() && cov.is_file()).then_some(LlvmTools { profdata, cov })
}

fn find_llvm_tools(opts: &CoverageOptions, deadline: Deadline) -> Result<LlvmTools, SelectError> {
    if let Some(dir) = &opts.llvm_bin_dir {
        return find_in(dir).ok_or_else(|| {
            command_error(
                "llvm-profdata",
                format!("llvm-profdata and llvm-cov not found in {}", dir.display()),
            )
        });
    }
    let mut rustc = Command::new("rustc");
    rustc.args(["--print", "sysroot"]);
    if let Ok((status, out, _)) = run_captured(rustc, "rustc --print sysroot", deadline)
        && status.success()
    {
        let sysroot = PathBuf::from(String::from_utf8_lossy(&out).trim());
        let host = host_triple(deadline);
        if let Some(found) =
            host.and_then(|h| find_in(&sysroot.join("lib/rustlib").join(h).join("bin")))
        {
            return Ok(found);
        }
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        if let Some(found) = find_in(&dir) {
            return Ok(found);
        }
    }
    Err(command_error(
        "llvm-profdata",
        "llvm-profdata/llvm-cov not found (install the rustup component `llvm-tools` or set llvm_bin_dir)",
    ))
}

fn host_triple(deadline: Deadline) -> Option<String> {
    let mut rustc = Command::new("rustc");
    rustc.arg("-vV");
    let (status, out, _) = run_captured(rustc, "rustc -vV", deadline).ok()?;
    if !status.success() {
        return None;
    }
    String::from_utf8_lossy(&out)
        .lines()
        .find_map(|l| l.strip_prefix("host: ").map(|h| h.trim().to_owned()))
}

/// Un exécutable de test instrumenté.
struct TestExe {
    binary_id: String,
    path: PathBuf,
    manifest_dir: PathBuf,
}

struct Built {
    tests: Vec<TestExe>,
    /// Binaires non test, ajoutés comme objets à `llvm-cov` (tests qui lancent le CLI).
    bins: Vec<PathBuf>,
}

fn cargo_command() -> Command {
    Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
}

/// Noms de paquets par chemin de manifeste (`cargo metadata --no-deps`).
fn package_names(
    repo: &Path,
    deadline: Deadline,
) -> Result<BTreeMap<PathBuf, String>, SelectError> {
    let mut cmd = cargo_command();
    cmd.args([
        "metadata",
        "--no-deps",
        "--format-version",
        "1",
        "--locked",
        "--offline",
    ])
    .current_dir(repo);
    let (status, out, tail) = run_captured(cmd, "cargo metadata", deadline)?;
    if !status.success() {
        return Err(command_error("cargo metadata", tail));
    }
    let value: serde_json::Value =
        serde_json::from_slice(&out).map_err(|e| invalid("cargo metadata", e.to_string()))?;
    let mut names = BTreeMap::new();
    for pkg in value["packages"].as_array().into_iter().flatten() {
        if let (Some(name), Some(manifest)) = (pkg["name"].as_str(), pkg["manifest_path"].as_str())
        {
            names.insert(PathBuf::from(manifest), name.to_owned());
        }
    }
    Ok(names)
}

/// Compile les binaires de test instrumentés et lit les artefacts JSON de Cargo.
fn build_instrumented(
    repo: &Path,
    target_dir: &Path,
    opts: &CoverageOptions,
    deadline: Deadline,
) -> Result<Built, SelectError> {
    let names = package_names(repo, deadline)?;
    let mut cmd = cargo_command();
    cmd.args([
        "test",
        "--locked",
        "--offline",
        "--no-run",
        "--message-format=json",
    ]);
    if !opts.features.is_empty() {
        cmd.arg("--features").arg(opts.features.join(","));
    }
    cmd.current_dir(repo)
        .env("RUSTFLAGS", "-C instrument-coverage")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env("CARGO_INCREMENTAL", "0")
        .env("CARGO_NET_OFFLINE", "true")
        .env("CARGO_TARGET_DIR", target_dir)
        .env("CI", "1")
        .env("LC_ALL", "C");
    let (status, out, tail) = run_captured(cmd, "cargo test --no-run", deadline)?;
    if !status.success() {
        return Err(command_error("cargo test --no-run", tail));
    }
    let mut built = Built {
        tests: Vec::new(),
        bins: Vec::new(),
    };
    for line in String::from_utf8_lossy(&out).lines() {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if msg["reason"] != "compiler-artifact" {
            continue;
        }
        let Some(exe) = msg["executable"].as_str() else {
            continue;
        };
        let kind = msg["target"]["kind"][0].as_str().unwrap_or_default();
        let target = msg["target"]["name"].as_str().unwrap_or_default();
        let manifest = PathBuf::from(msg["manifest_path"].as_str().unwrap_or_default());
        if msg["profile"]["test"] != true {
            if kind == "bin" {
                built.bins.push(PathBuf::from(exe));
            }
            continue;
        }
        let Some(package) = names.get(&manifest) else {
            return Err(invalid(
                "cargo artifact",
                format!("unknown package for {}", manifest.display()),
            ));
        };
        let binary_id = match kind {
            "test" => format!("{package}::{target}"),
            "bin" => format!("{package}::bin/{target}"),
            "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro" => package.clone(),
            _ => continue, // exemples, benchs : jamais sélectionnés partiellement
        };
        built.tests.push(TestExe {
            binary_id,
            path: PathBuf::from(exe),
            manifest_dir: manifest.parent().map(Path::to_path_buf).unwrap_or_default(),
        });
    }
    if built.tests.is_empty() {
        return Err(invalid("cargo artifacts", "no test executable was built"));
    }
    Ok(built)
}

/// Environnement commun des exécutables de test.
fn test_command(exe: &TestExe, repo: &Path, profile_pattern: &Path) -> Command {
    let mut cmd = Command::new(&exe.path);
    cmd.current_dir(repo)
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env("LLVM_PROFILE_FILE", profile_pattern)
        .env("CARGO_MANIFEST_DIR", &exe.manifest_dir)
        .env("CI", "1")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

fn libtest_names(
    exe: &TestExe,
    repo: &Path,
    profiles: &Path,
    ignored_only: bool,
    deadline: Deadline,
) -> Result<BTreeSet<String>, SelectError> {
    let mut cmd = test_command(exe, repo, &profiles.join("list-%p-%m.profraw"));
    cmd.args(["--list", "--format", "terse"]);
    if ignored_only {
        cmd.arg("--ignored");
    }
    let (status, out, tail) = run_captured(cmd, &format!("{} --list", exe.binary_id), deadline)?;
    if !status.success() {
        return Err(command_error(&format!("{} --list", exe.binary_id), tail));
    }
    Ok(String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|l| l.strip_suffix(": test").map(str::to_owned))
        .collect())
}

/// Fichiers du dépôt ayant au moins une ligne exécutée dans un export lcov. Les fichiers
/// hors dépôt (std, registre Cargo) et sous `target/` sont ignorés.
// La validation complète partage l'état de chaque record LCOV.
#[allow(clippy::too_many_lines)]
fn parse_lcov(
    text: &str,
    repo: &Path,
    target_dir: &Path,
) -> Result<BTreeSet<RelPath>, SelectError> {
    let bad = || invalid("llvm-cov lcov", "invalid or incomplete export");
    let mut files = BTreeSet::new();
    let mut current: Option<RelPath> = None;
    let mut in_record = false;
    let mut records = 0usize;
    let mut any_hit = false;
    let mut lines = BTreeMap::<u64, u64>::new();
    let mut summaries = BTreeMap::<&str, u64>::new();
    let mut functions = (0u64, 0u64, 0u64);
    let mut branches = (0u64, 0u64);
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("SF:") {
            if in_record || path.is_empty() {
                return Err(bad());
            }
            in_record = true;
            lines.clear();
            summaries.clear();
            functions = (0, 0, 0);
            branches = (0, 0);
            current = Path::new(path)
                .strip_prefix(repo)
                .ok()
                .filter(|_| !Path::new(path).starts_with(target_dir))
                .filter(|rel| rel.components().all(|c| matches!(c, Component::Normal(_))))
                .and_then(|rel| rel.to_str().map(str::to_owned))
                .filter(|rel| !rel.starts_with("target/") && is_repo_relative(rel));
        } else if let Some(da) = line.strip_prefix("DA:") {
            let mut fields = da.split(',');
            let number = fields
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|n| *n > 0)
                .ok_or_else(bad)?;
            let hits = fields
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or_else(bad)?;
            if !in_record
                || fields.next().is_some_and(str::is_empty)
                || fields.next().is_some()
                || lines.insert(number, hits).is_some()
            {
                return Err(bad());
            }
            any_hit |= hits > 0;
            if hits > 0
                && let Some(file) = &current
            {
                files.insert(file.clone());
            }
        } else if line == "end_of_record" {
            if !in_record
                || (lines.is_empty()
                    && (summaries.get("LF") != Some(&0) || summaries.get("LH") != Some(&0)))
            {
                return Err(bad());
            }
            match (summaries.get("LF"), summaries.get("LH")) {
                (None, None) => {}
                // Captured LLVM exports can report fewer LH hits than positive
                // DA entries. Executed files come from DA, not summary counts.
                (Some(f), Some(h))
                    if *f >= lines.len() as u64
                        && *h <= *f
                        && (*h > 0) == lines.values().any(|n| *n > 0) => {}
                _ => return Err(bad()),
            }
            if functions.0 != functions.1 {
                return Err(bad());
            }
            // LLVM function summaries count logical functions, while FN/FNDA
            // entries can include multiple monomorphized instances.
            match (summaries.get("FNF"), summaries.get("FNH")) {
                (None, None) => {}
                (Some(f), Some(h)) if *f <= functions.0 && *h <= functions.2 && *h <= *f => {}
                _ => return Err(bad()),
            }
            match (summaries.get("BRF"), summaries.get("BRH")) {
                (None, None) => {}
                (Some(f), Some(h)) if *f == branches.0 && *h == branches.1 => {}
                _ => return Err(bad()),
            }
            in_record = false;
            current = None;
            records += 1;
        } else if let Some((key, value)) = line.split_once(':') {
            match key {
                "TN" if !in_record => {}
                "LF" | "LH" | "FNF" | "FNH" | "BRF" | "BRH" if in_record => {
                    let n = value.parse::<u64>().map_err(|_| bad())?;
                    if summaries.insert(key, n).is_some() {
                        return Err(bad());
                    }
                }
                "FN" | "FNDA" if in_record => {
                    let (number, name) = value.split_once(',').ok_or_else(bad)?;
                    let number = number.parse::<u64>().map_err(|_| bad())?;
                    if name.is_empty() || (key == "FN" && number == 0) {
                        return Err(bad());
                    }
                    if key == "FN" {
                        functions.0 += 1;
                    } else {
                        functions.1 += 1;
                        functions.2 += u64::from(number > 0);
                    }
                }
                "BRDA" if in_record => {
                    let parts: Vec<_> = value.split(',').collect();
                    if parts.len() != 4 || parts[..3].iter().any(|s| s.parse::<u64>().is_err()) {
                        return Err(bad());
                    }
                    let hit = if parts[3] == "-" {
                        false
                    } else {
                        parts[3].parse::<u64>().map_err(|_| bad())? > 0
                    };
                    branches.0 += 1;
                    branches.1 += u64::from(hit);
                }
                _ => return Err(bad()),
            }
        } else {
            return Err(bad());
        }
    }
    if in_record || records == 0 || !any_hit || files.is_empty() {
        return Err(bad());
    }
    Ok(files)
}

struct Job<'a> {
    index: usize,
    exe: &'a TestExe,
    name: String,
}

struct Shared<'a> {
    repo: &'a Path,
    profiles: &'a Path,
    tools: &'a LlvmTools,
    built: &'a Built,
    opts: &'a CoverageOptions,
    deadline: Deadline,
    target_dir: &'a Path,
    evidence: Option<(&'a Path, &'a str)>,
    preserve_profiles: &'a AtomicBool,
}

#[cfg(unix)]
fn protect_profile(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(std::io::Error::other(
            "profile is not a private regular file",
        ));
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

fn private_file(path: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest as _, Sha256};
    use std::io::Read as _;
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 16_384];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn archive_failure(
    shared: &Shared<'_>,
    job: &Job<'_>,
    raw: &[PathBuf],
    merged: &Path,
    export: &Path,
    export_details: (&[String], ExitStatus),
    error: &SelectError,
) -> Result<(), SelectError> {
    let (store, commit) = shared
        .evidence
        .ok_or_else(|| command_error("coverage evidence", "missing evidence destination"))?;
    let (export_argv, status) = export_details;
    let directory = store.join(format!(
        "coverage-failure-{}-{}",
        std::process::id(),
        job.index
    ));
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(&directory)
        .map_err(|e| command_error("coverage evidence", e.to_string()))?;
    let mut files = Vec::new();
    for source in raw.iter().map(PathBuf::as_path).chain([merged, export]) {
        let name = source
            .file_name()
            .ok_or_else(|| command_error("coverage evidence", "missing filename"))?;
        let mut source_file = fs::File::open(source)
            .map_err(|e| command_error("coverage evidence", e.to_string()))?;
        let destination = directory.join(name);
        let mut output = private_file(&destination)
            .map_err(|e| command_error("coverage evidence", e.to_string()))?;
        std::io::copy(&mut source_file, &mut output)
            .map_err(|e| command_error("coverage evidence", e.to_string()))?;
        let sha = sha256_file(&destination)
            .map_err(|e| command_error("coverage evidence", e.to_string()))?;
        files.push(serde_json::json!({"name": name.to_string_lossy(), "sha256": sha}));
    }
    let merge_argv: Vec<String> =
        std::iter::once(shared.tools.profdata.to_string_lossy().into_owned())
            .chain(["merge".to_owned(), "-sparse".to_owned()])
            .chain(raw.iter().map(|p| p.to_string_lossy().into_owned()))
            .chain(["-o".to_owned(), merged.to_string_lossy().into_owned()])
            .collect();
    let metadata = serde_json::json!({
        "commit": commit, "binary_id": job.exe.binary_id, "test": job.name,
        "test_executable": job.exe.path, "test_argv": ["--exact", &job.name, "--test-threads=1", "-q"],
        "merge_argv": merge_argv, "merge_exit_code": 0,
        "export_argv": export_argv, "export_exit_code": status.code(),
        "error": error.to_string(), "files": files,
        "test_executable_sha256": sha256_file(&job.exe.path).map_err(|e| command_error("coverage evidence", e.to_string()))?,
        "llvm_cov_sha256": sha256_file(&shared.tools.cov).map_err(|e| command_error("coverage evidence", e.to_string()))?,
        "llvm_profdata_sha256": sha256_file(&shared.tools.profdata).map_err(|e| command_error("coverage evidence", e.to_string()))?,
        "objects": shared.built.bins.iter().map(|path| {
            sha256_file(path).map(|sha256| serde_json::json!({"path": path, "sha256": sha256}))
        }).collect::<std::io::Result<Vec<_>>>().map_err(|e| command_error("coverage evidence", e.to_string()))?,
    });
    let bytes = serde_json::to_vec_pretty(&metadata)
        .map_err(|e| command_error("coverage evidence", e.to_string()))?;
    private_file(&directory.join("metadata.json"))
        .and_then(|mut file| file.write_all(&bytes))
        .map_err(|e| command_error("coverage evidence", e.to_string()))?;
    Ok(())
}

fn archive_test_failure(
    shared: &Shared<'_>,
    job: &Job<'_>,
    raw: &[PathBuf],
    status: Option<ExitStatus>,
    expired: bool,
    error: &SelectError,
) -> Result<(), SelectError> {
    let (store, commit) = shared
        .evidence
        .ok_or_else(|| command_error("coverage evidence", "missing evidence destination"))?;
    let directory = store.join(format!(
        "coverage-failure-{}-{}",
        std::process::id(),
        job.index
    ));
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(&directory)
        .map_err(|e| command_error("coverage evidence", e.to_string()))?;
    let mut files = Vec::new();
    for path in raw {
        let name = path
            .file_name()
            .ok_or_else(|| command_error("coverage evidence", "missing filename"))?;
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::custom_flags(
            &mut options,
            libc::O_NOFOLLOW | libc::O_NONBLOCK,
        );
        let mut input = options
            .open(path)
            .map_err(|e| command_error("coverage evidence", e.to_string()))?;
        let metadata = input
            .metadata()
            .map_err(|e| command_error("coverage evidence", e.to_string()))?;
        if !metadata.is_file() {
            return Err(command_error(
                "coverage evidence",
                "profile is not a regular file",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
            if metadata.nlink() != 1 {
                return Err(command_error(
                    "coverage evidence",
                    "profile has multiple links",
                ));
            }
            input
                .set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|e| command_error("coverage evidence", e.to_string()))?;
        }
        let destination = directory.join(name);
        let mut output = private_file(&destination)
            .map_err(|e| command_error("coverage evidence", e.to_string()))?;
        std::io::copy(&mut input, &mut output)
            .map_err(|e| command_error("coverage evidence", e.to_string()))?;
        let sha256 = sha256_file(&destination)
            .map_err(|e| command_error("coverage evidence", e.to_string()))?;
        files.push(serde_json::json!({"name": name.to_string_lossy(), "sha256": sha256}));
    }
    #[cfg(unix)]
    let signal = status.and_then(|s| std::os::unix::process::ExitStatusExt::signal(&s));
    #[cfg(not(unix))]
    let signal: Option<i32> = None;
    let metadata = serde_json::json!({
        "commit": commit, "binary_id": job.exe.binary_id, "test": job.name,
        "phase": "test", "test_executable": job.exe.path,
        "test_executable_sha256": sha256_file(&job.exe.path).map_err(|e| command_error("coverage evidence", e.to_string()))?,
        "objects": shared.built.bins.iter().map(|path| {
            sha256_file(path).map(|sha256| serde_json::json!({"path": path, "sha256": sha256}))
        }).collect::<std::io::Result<Vec<_>>>().map_err(|e| command_error("coverage evidence", e.to_string()))?,
        "test_argv": ["--exact", &job.name, "--test-threads=1", "-q"],
        "test_exit_code": status.and_then(|s| s.code()), "test_signal": signal,
        "timed_out": status.is_none() || expired,
        "error": error.to_string(), "files": files,
    });
    let bytes = serde_json::to_vec_pretty(&metadata)
        .map_err(|e| command_error("coverage evidence", e.to_string()))?;
    private_file(&directory.join("metadata.json"))
        .and_then(|mut file| file.write_all(&bytes))
        .map_err(|e| command_error("coverage evidence", e.to_string()))
}

fn coverage_source_filter(repo: &Path) -> Result<String, SelectError> {
    let path = repo
        .to_str()
        .filter(|path| path.starts_with('/') && !path.chars().any(char::is_control))
        .ok_or_else(|| invalid("llvm-cov source filter", "unsupported repository path"))?;
    let mut pattern = String::from(r"^/+(\./+)*");
    for part in path.split('/').filter(|part| !part.is_empty()) {
        if part == "." || part == ".." {
            return Err(invalid(
                "llvm-cov source filter",
                "non-canonical repository path",
            ));
        }
        for ch in part.chars() {
            if ".^$|?*+()[]{}\\".contains(ch) {
                pattern.push('\\');
            }
            pattern.push(ch);
        }
        pattern.push_str(r"/+(\./+)*");
    }
    Ok(pattern)
}

/// Exécute un test sous profil, fusionne et exporte. Une observation incomplète est une erreur.
fn cover_one(shared: &Shared<'_>, job: &Job<'_>) -> Result<Option<BTreeSet<RelPath>>, SelectError> {
    let prefix = format!("{}-", job.index);
    let mut cmd = test_command(
        job.exe,
        shared.repo,
        &shared.profiles.join(format!("{prefix}%p-%m.profraw")),
    );
    cmd.args(["--exact", &job.name, "--test-threads=1", "-q"]);
    let what = format!("{} {}", job.exe.binary_id, job.name);
    let mut child = cmd
        .spawn()
        .map_err(|e| command_error(&what, e.to_string()))?;
    let until = shared
        .deadline
        .at
        .min(Instant::now() + shared.opts.test_timeout);
    let finished = wait_until(&mut child, until, &what)?;
    let expired = shared.deadline.expired();
    if expired && shared.evidence.is_none() {
        return Err(shared.deadline.timeout(&what));
    }
    if shared.evidence.is_some() && (expired || finished.is_none_or(|status| !status.success())) {
        shared.preserve_profiles.store(true, Ordering::Relaxed);
    }
    let raw = collect_test_profiles(shared, &prefix, &what)?;
    let problem = match finished {
        _ if expired => Some(shared.deadline.timeout(&what)),
        None => Some(command_error(&what, "incomplete coverage: test timed out")),
        Some(status) if !status.success() => {
            Some(command_error(&what, "incomplete coverage: test failed"))
        }
        Some(_) if raw.is_empty() => Some(command_error(&what, "incomplete coverage: no profile")),
        Some(_) => None,
    };
    if let Some(error) = problem {
        if shared.evidence.is_some() {
            shared.preserve_profiles.store(true, Ordering::Relaxed);
            archive_test_failure(shared, job, &raw, finished, expired, &error)?;
        } else {
            for f in &raw {
                let _ = fs::remove_file(f);
            }
        }
        return Err(error);
    }
    validate_profiles(&raw)?;
    let merged = shared.profiles.join(format!("{}.profdata", job.index));
    let mut merge = Command::new(&shared.tools.profdata);
    merge
        .args(["merge", "-sparse"])
        .args(&raw)
        .arg("-o")
        .arg(&merged);
    let (status, _, tail) = run_captured(merge, "llvm-profdata merge", shared.deadline)?;
    if !status.success() {
        return Err(command_error("llvm-profdata merge", tail));
    }
    #[cfg(unix)]
    protect_profile(&merged).map_err(|e| command_error("coverage profile", e.to_string()))?;
    let mut export = Command::new(&shared.tools.cov);
    // LLVM 23 supports this filter. Older versions that do not support it fail closed.
    export
        .args(["export", "-format=lcov", "-instr-profile"])
        .arg(&merged)
        .arg(format!(
            "--include-filename-regex={}",
            coverage_source_filter(shared.repo)?
        ))
        .arg(&job.exe.path);
    for bin in &shared.built.bins {
        export.arg("-object").arg(bin);
    }
    let export_argv: Vec<String> = std::iter::once(export.get_program())
        .chain(export.get_args())
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let (status, out, tail) = run_captured(export, "llvm-cov export", shared.deadline)?;
    if !status.success() {
        return Err(command_error("llvm-cov export", tail));
    }
    let parsed = std::str::from_utf8(&out)
        .map_err(|e| invalid("llvm-cov lcov", e.to_string()))
        .and_then(|text| parse_lcov(text, shared.repo, shared.target_dir));
    if let (Err(error), Some(_)) = (&parsed, shared.evidence) {
        retain_failure(
            shared,
            job,
            &raw,
            &merged,
            (&out, &export_argv, status),
            error,
        )?;
    } else {
        for f in &raw {
            let _ = fs::remove_file(f);
        }
        let _ = fs::remove_file(&merged);
    }
    parsed.map(Some)
}

fn collect_test_profiles(
    shared: &Shared<'_>,
    prefix: &str,
    what: &str,
) -> Result<Vec<PathBuf>, SelectError> {
    let profile_error = |e: std::io::Error| {
        if shared.evidence.is_some() {
            shared.preserve_profiles.store(true, Ordering::Relaxed);
        }
        command_error(what, e.to_string())
    };
    let raw = fs::read_dir(shared.profiles)
        .map_err(profile_error)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(profile_error)?
        .into_iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(prefix) && n.ends_with(".profraw"))
        })
        .collect();
    Ok(raw)
}

fn validate_profiles(raw: &[PathBuf]) -> Result<(), SelectError> {
    for path in raw {
        let metadata = fs::symlink_metadata(path)
            .map_err(|e| command_error("coverage profile", e.to_string()))?;
        if !metadata.file_type().is_file() {
            return Err(command_error(
                "coverage profile",
                "profile is not a regular file",
            ));
        }
    }
    #[cfg(unix)]
    for path in raw {
        protect_profile(path).map_err(|e| command_error("coverage profile", e.to_string()))?;
    }
    Ok(())
}

fn retain_failure(
    shared: &Shared<'_>,
    job: &Job<'_>,
    raw: &[PathBuf],
    merged: &Path,
    export: (&[u8], &[String], ExitStatus),
    error: &SelectError,
) -> Result<(), SelectError> {
    let (out, export_argv, status) = export;
    shared.preserve_profiles.store(true, Ordering::Relaxed);
    let fallback = shared.profiles.join(format!("{}.lcov", job.index));
    let incomplete = shared
        .profiles
        .join(format!("{}.lcov.incomplete", job.index));
    private_file(&incomplete)
        .and_then(|mut file| file.write_all(b"LCOV evidence write incomplete\n"))
        .map_err(|e| command_error("coverage evidence", e.to_string()))?;
    private_file(&fallback)
        .and_then(|mut file| file.write_all(out))
        .map_err(|e| command_error("coverage evidence", e.to_string()))?;
    fs::remove_file(&incomplete).map_err(|e| command_error("coverage evidence", e.to_string()))?;
    archive_failure(
        shared,
        job,
        raw,
        merged,
        &fallback,
        (export_argv, status),
        error,
    )?;
    Ok(())
}

/// Couverture par test : compile le dépôt avec `-C instrument-coverage` dans un dossier cible
/// séparé (`target/select-cov`), lance chaque test dans son propre processus et en déduit les
/// fichiers du dépôt exécutés. À ne lancer que sur la base (`kioku select learn`).
///
/// Un test en échec, expiré ou sans profil fait échouer la collecte : aucun filet partiel
/// n'est renvoyé pour être enregistré comme complet. Les tests ignorés ne sont pas lancés.
/// Les sous-processus d'un test tué par son délai ne sont pas tués.
///
/// # Errors
/// [`SelectError::Command`] si `llvm-profdata`/`llvm-cov`/`cargo` sont absents ou échouent,
/// [`SelectError::Timeout`] si le délai total est dépassé, [`SelectError::Invalid`] si la
/// sortie de Cargo est incohérente, [`SelectError::Io`] si le dossier de profils est illisible.
pub fn run_coverage(
    repo: &Path,
    opts: &CoverageOptions,
) -> Result<BTreeMap<TestId, BTreeSet<RelPath>>, SelectError> {
    run_coverage_inner(repo, opts, None)
}

pub(crate) fn run_coverage_with_evidence(
    repo: &Path,
    opts: &CoverageOptions,
    store: &Path,
    commit: &str,
) -> Result<BTreeMap<TestId, BTreeSet<RelPath>>, SelectError> {
    run_coverage_inner(repo, opts, Some((store, commit)))
}

fn run_coverage_inner(
    repo: &Path,
    opts: &CoverageOptions,
    evidence: Option<(&Path, &str)>,
) -> Result<BTreeMap<TestId, BTreeSet<RelPath>>, SelectError> {
    let deadline = Deadline {
        at: Instant::now() + opts.timeout,
        total_secs: opts.timeout.as_secs(),
    };
    let repo = repo.canonicalize().map_err(|e| SelectError::Io {
        path: repo.display().to_string(),
        message: e.to_string(),
    })?;
    let tools = find_llvm_tools(opts, deadline)?;
    let target_dir = opts
        .target_dir
        .clone()
        .unwrap_or_else(|| repo.join("target").join("select-cov"));
    let built = build_instrumented(&repo, &target_dir, opts, deadline)?;
    let target_dir = target_dir.canonicalize().unwrap_or(target_dir);

    let profiles = evidence
        .map_or(target_dir.as_path(), |(store, _)| store)
        .join(format!("select-profiles-{}", std::process::id()));
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(&profiles).map_err(|e| SelectError::Io {
        path: profiles.display().to_string(),
        message: e.to_string(),
    })?;
    let preserve_profiles = AtomicBool::new(false);
    let shared = Shared {
        repo: &repo,
        profiles: &profiles,
        tools: &tools,
        built: &built,
        opts,
        deadline,
        target_dir: &target_dir,
        evidence,
        preserve_profiles: &preserve_profiles,
    };
    let outcome = collect(&shared);
    finish_profiles(&profiles, preserve_profiles.load(Ordering::Relaxed))?;
    outcome
}

fn finish_profiles(profiles: &Path, preserve: bool) -> Result<(), SelectError> {
    if preserve {
        let entries = fs::read_dir(profiles)
            .map_err(|e| command_error("coverage evidence", e.to_string()))?;
        let mut first_error = None;
        for entry in entries {
            let result = match entry {
                Ok(entry) => {
                    let path = entry.path();
                    if path.extension().is_some_and(|ext| ext == "profraw") {
                        validate_profiles(std::slice::from_ref(&path))
                    } else {
                        Ok(())
                    }
                }
                Err(error) => Err(command_error("coverage evidence", error.to_string())),
            };
            if first_error.is_none() {
                first_error = result.err();
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
    } else {
        let _ = fs::remove_dir_all(profiles);
    }
    Ok(())
}

fn collect(shared: &Shared<'_>) -> Result<BTreeMap<TestId, BTreeSet<RelPath>>, SelectError> {
    let mut jobs = Vec::new();
    for exe in &shared.built.tests {
        let all = libtest_names(exe, shared.repo, shared.profiles, false, shared.deadline)?;
        let ignored = libtest_names(exe, shared.repo, shared.profiles, true, shared.deadline)?;
        for name in all.difference(&ignored) {
            jobs.push(Job {
                index: jobs.len(),
                exe,
                name: name.clone(),
            });
        }
    }
    let next = AtomicUsize::new(0);
    let abort = AtomicBool::new(false);
    let failure: Mutex<Option<SelectError>> = Mutex::new(None);
    let result: Mutex<BTreeMap<TestId, BTreeSet<RelPath>>> = Mutex::new(BTreeMap::new());
    thread::scope(|scope| {
        for _ in 0..shared.opts.jobs.max(1) {
            scope.spawn(|| {
                while !abort.load(Ordering::Relaxed) {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(i) else { break };
                    match cover_one(shared, job) {
                        Ok(Some(files)) if !files.is_empty() => {
                            let id = TestId {
                                binary_id: job.exe.binary_id.clone(),
                                name: job.name.clone(),
                            };
                            if let Ok(mut map) = result.lock() {
                                map.insert(id, files);
                            }
                        }
                        Ok(_) => {}
                        Err(e) => {
                            abort.store(true, Ordering::Relaxed);
                            if let Ok(mut slot) = failure.lock() {
                                slot.get_or_insert(e);
                            }
                        }
                    }
                }
            });
        }
    });
    if let Some(err) = failure.into_inner().ok().flatten() {
        return Err(err);
    }
    result
        .into_inner()
        .map_err(|_| invalid("coverage result", "worker panicked"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use sha2::Digest as _;

    #[test]
    fn capped_capture_detects_truncation_and_read_errors() {
        struct Broken;
        impl std::io::Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("broken"))
            }
        }
        assert_eq!(
            read_capped(&b"abcd"[..], 4).unwrap(),
            (b"abcd".to_vec(), false)
        );
        assert_eq!(
            read_capped(&b"abcde"[..], 4).unwrap(),
            (b"abcd".to_vec(), true)
        );
        assert!(read_capped(Broken, 4).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn command_capture_rejects_truncation_and_reader_failures() {
        for (script, expected) in [
            ("printf 12345", "stdout"),
            ("printf 12345 >&2", "stderr"),
            (
                "printf 'SF:/repo/a.rs\\nDA:1,1\\nend_of_record\\n'",
                "stdout",
            ),
        ] {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", script]);
            let deadline = Deadline {
                at: Instant::now() + Duration::from_secs(5),
                total_secs: 5,
            };
            let err = run_captured_limits(cmd, "fixture", deadline, 4, 4).unwrap_err();
            assert!(err.to_string().contains(expected));
        }
        for stream in ["stdout", "stderr"] {
            for mode in 0..3 {
                let reader = move || -> Option<std::io::Result<(Vec<u8>, bool)>> {
                    match mode {
                        0 => None,
                        1 => Some(Err(std::io::Error::other("reader error"))),
                        _ => panic!("reader panic"),
                    }
                };
                let mut cmd = Command::new("sh");
                cmd.args(["-c", "exit 0"]);
                let deadline = Deadline {
                    at: Instant::now() + Duration::from_secs(5),
                    total_secs: 5,
                };
                let result = if stream == "stdout" {
                    run_captured_readers(
                        cmd,
                        "fixture",
                        deadline,
                        move |_| reader(),
                        |s| s.map(|s| read_capped(s, 4)),
                    )
                } else {
                    run_captured_readers(
                        cmd,
                        "fixture",
                        deadline,
                        |s| s.map(|s| read_capped(s, 4)),
                        move |_| reader(),
                    )
                };
                let error = result.unwrap_err().to_string();
                let diagnostic = match mode {
                    0 => "unavailable",
                    1 => "reader error",
                    _ => "reader panicked",
                };
                assert!(error.contains(diagnostic), "{error}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn command_capture_times_out_on_inherited_pipes_after_parent_exit() {
        for stream in ["stdout", "stderr"] {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join("closed");
            let mut cmd = Command::new("sh");
            let redirect = if stream == "stdout" {
                "2>/dev/null"
            } else {
                ">/dev/null"
            };
            cmd.arg("-c")
                .arg(format!("(sleep 0.4; printf done > \"$1\") {redirect} &"))
                .arg("fixture")
                .arg(&marker);
            let start = Instant::now();
            let deadline = Deadline {
                at: start + Duration::from_millis(100),
                total_secs: 1,
            };
            let result = run_captured(cmd, "fixture", deadline);
            let elapsed = start.elapsed();
            // Wait for the controlled descendant to finish even if an assertion fails.
            let cleanup_deadline = Instant::now() + Duration::from_secs(3);
            while !marker.exists() && Instant::now() < cleanup_deadline {
                thread::sleep(Duration::from_millis(10));
            }
            assert!(marker.exists(), "controlled descendant did not finish");
            assert!(matches!(result, Err(SelectError::Timeout { .. })));
            assert!(elapsed < Duration::from_millis(350), "{elapsed:?}");
        }
    }

    #[test]
    fn repo_relative_accepts_plain_paths_only() {
        assert!(is_repo_relative("src/a.rs"));
        assert!(is_repo_relative("tests/support/mod.rs"));
        for bad in [
            "",
            "/etc/passwd",
            "../x.rs",
            "src/../x.rs",
            "./src/a.rs",
            "src//a.rs",
            "src\\a.rs",
            "C:/x.rs",
            "src/\0a.rs",
            "src/.",
        ] {
            assert!(!is_repo_relative(bad), "{bad:?} must be refused");
        }
    }

    #[test]
    fn learning_preserves_per_test_mapping_and_only_adds_roots() {
        let one = TestId {
            binary_id: "b".into(),
            name: "one".into(),
        };
        let two = TestId {
            binary_id: "b".into(),
            name: "two".into(),
        };
        let mut graph = GraphData {
            test_roots: BTreeMap::from([
                (one.clone(), BTreeSet::from(["one.rs".into()])),
                (two.clone(), BTreeSet::from(["two.rs".into()])),
            ]),
            edges: BTreeMap::from([("one.rs".into(), BTreeSet::from(["dep.rs".into()]))]),
            ..GraphData::default()
        };
        let before = graph.clone();
        let coverage =
            coverage_from_json(r#"{"b one":{"hidden-one.rs":[1]},"b two":{"hidden-two.rs":[2]}}"#)
                .expect("coverage");
        let learned = learn(&graph, &coverage);
        assert_eq!(
            learned.edges[&one],
            BTreeSet::from(["hidden-one.rs".into()])
        );
        assert_eq!(
            learned.edges[&two],
            BTreeSet::from(["hidden-two.rs".into()])
        );
        apply(&mut graph, &learned);
        assert_eq!(graph.edges, before.edges);
        for test in [&one, &two] {
            assert!(before.test_roots[test].is_subset(&graph.test_roots[test]));
        }
        assert!(!graph.test_roots[&one].contains("hidden-two.rs"));
        assert!(!graph.test_roots[&two].contains("hidden-one.rs"));
    }

    #[cfg(unix)]
    #[test]
    fn rejected_export_is_archived_before_profile_teardown() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let store = repo.join("store");
        let profiles = store.join("profiles");
        fs::create_dir(&store).unwrap();
        fs::create_dir(&profiles).unwrap();
        let test_exe = repo.join("test");
        let profdata = repo.join("profdata");
        let cov = repo.join("cov");
        for (path, script) in [
            (&test_exe, "printf raw > \"$LLVM_PROFILE_FILE\""),
            (
                &profdata,
                "for arg in \"$@\"; do last=$arg; done; printf merged > \"$last\"",
            ),
            (&cov, "printf 'not lcov\\n'"),
        ] {
            fs::write(path, format!("#!/bin/sh\n{script}\n")).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let exe = TestExe {
            binary_id: "sample".into(),
            path: test_exe,
            manifest_dir: repo.to_owned(),
        };
        let built = Built {
            tests: Vec::new(),
            bins: Vec::new(),
        };
        let tools = LlvmTools { profdata, cov };
        let opts = CoverageOptions::default();
        let preserve_profiles = AtomicBool::new(false);
        let shared = Shared {
            repo,
            profiles: &profiles,
            tools: &tools,
            built: &built,
            opts: &opts,
            deadline: Deadline {
                at: Instant::now() + Duration::from_secs(10),
                total_secs: 10,
            },
            target_dir: repo,
            evidence: Some((&store, "candidate")),
            preserve_profiles: &preserve_profiles,
        };
        let job = Job {
            index: 0,
            exe: &exe,
            name: "case".into(),
        };
        assert!(cover_one(&shared, &job).is_err());
        assert!(preserve_profiles.load(Ordering::Relaxed));
        for entry in fs::read_dir(&profiles).unwrap() {
            let path = entry.unwrap().path();
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(&profiles).unwrap();
        let archived = fs::read_dir(&store)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(fs::read(archived.join("0.lcov")).unwrap(), b"not lcov\n");
        assert_eq!(fs::read(archived.join("0.profdata")).unwrap(), b"merged");
        let names: Vec<_> = fs::read_dir(&archived)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert!(names.iter().any(
            |p| p.extension().is_some_and(|e| e == "profraw") && fs::read(p).unwrap() == b"raw"
        ));
        assert!(archived.join("metadata.json").is_file());
        assert_eq!(
            fs::metadata(&archived).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(
            names
                .iter()
                .all(|p| fs::metadata(p).unwrap().permissions().mode() & 0o777 == 0o600)
        );
        fs::create_dir(&profiles).unwrap();
        let error = cover_one(&shared, &job).unwrap_err();
        assert!(error.to_string().contains("coverage evidence"));
        assert_eq!(fs::read(profiles.join("0.lcov")).unwrap(), b"not lcov\n");
        assert_eq!(fs::read(profiles.join("0.profdata")).unwrap(), b"merged");
        fs::remove_file(profiles.join("0.lcov")).unwrap();
        fs::create_dir(profiles.join("0.lcov")).unwrap();
        assert!(cover_one(&shared, &job).is_err());
        assert!(profiles.join("0.lcov.incomplete").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn incomplete_test_profiles_abort_collection_with_diagnostics() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path();
        let exe_path = repo.join("test-exe");
        let exe = TestExe {
            binary_id: "sample".into(),
            path: exe_path.clone(),
            manifest_dir: repo.to_path_buf(),
        };
        let built = Built {
            tests: Vec::new(),
            bins: Vec::new(),
        };
        let tools = LlvmTools {
            profdata: repo.join("profdata"),
            cov: repo.join("cov"),
        };
        let opts = CoverageOptions {
            test_timeout: Duration::from_millis(100),
            ..CoverageOptions::default()
        };
        let preserve_profiles = AtomicBool::new(false);
        let shared = Shared {
            repo,
            profiles: repo,
            tools: &tools,
            built: &built,
            opts: &opts,
            deadline: Deadline {
                at: Instant::now() + Duration::from_secs(10),
                total_secs: 10,
            },
            target_dir: repo,
            evidence: None,
            preserve_profiles: &preserve_profiles,
        };
        let job = Job {
            index: 0,
            exe: &exe,
            name: "case".into(),
        };
        for (body, diagnostic) in [
            ("exit 0", "no profile"),
            (" : > \"$LLVM_PROFILE_FILE\"; exit 1", "test failed"),
            ("while :; do :; done", "test timed out"),
        ] {
            fs::write(&exe_path, format!("#!/bin/sh\n{body}\n")).expect("script");
            fs::set_permissions(&exe_path, fs::Permissions::from_mode(0o755)).expect("chmod");
            let err = cover_one(&shared, &job).expect_err("incomplete must fail");
            assert!(err.to_string().contains(diagnostic), "{err}");
        }
        let external = repo.join("sensitive");
        fs::write(&external, b"untouched").unwrap();
        fs::set_permissions(&external, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(
            &exe_path,
            "#!/bin/sh\nln -s \"$PWD/sensitive\" \"$LLVM_PROFILE_FILE\"\n",
        )
        .unwrap();
        let err = cover_one(&shared, &job).unwrap_err();
        assert!(
            err.to_string().contains("profile is not a regular file"),
            "{err}"
        );
        assert_eq!(
            fs::metadata(&external).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(fs::read(&external).unwrap(), b"untouched");
    }

    #[cfg(unix)]
    fn assert_test_failure_receipt(
        archived: &Path,
        exe: &TestExe,
        object: &Path,
        expected: &str,
        exit: Option<i32>,
        signal: Option<i32>,
    ) {
        let metadata: serde_json::Value =
            serde_json::from_slice(&fs::read(archived.join("metadata.json")).unwrap()).unwrap();
        assert_eq!(metadata["phase"], "test");
        assert_eq!(metadata["test_exit_code"], serde_json::json!(exit));
        assert_eq!(metadata["test_signal"], serde_json::json!(signal));
        assert_eq!(metadata["timed_out"], expected == "test timed out");
        assert_eq!(
            metadata["test_executable_sha256"],
            sha256_file(&exe.path).unwrap()
        );
        assert_eq!(
            metadata["objects"],
            serde_json::json!([{
                "path": object, "sha256": sha256_file(object).unwrap()
            }])
        );
        assert!(metadata.get("merge_argv").is_none());
        assert!(metadata.get("export_argv").is_none());
        let files = metadata["files"].as_array().unwrap();
        assert_eq!(files.len(), usize::from(expected != "no profile"));
        if let Some(file) = files.first() {
            let path = archived.join(file["name"].as_str().unwrap());
            assert_eq!(fs::read(&path).unwrap(), b"raw");
            assert_eq!(file["sha256"], sha256_file(&path).unwrap());
        }
    }

    #[cfg(unix)]
    fn assert_raw_profile_preserved(profiles: &Path) {
        assert!(fs::read_dir(profiles).unwrap().any(|entry| {
            let path = entry.unwrap().path();
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("0-")
                && fs::read(path).unwrap() == b"raw"
        }));
    }

    #[cfg(unix)]
    fn assert_fifo_archive_failure(
        shared: &Shared<'_>,
        exe: &TestExe,
        profiles: &Path,
        store: &Path,
        preserve: &AtomicBool,
    ) {
        let fifo = profiles.join("fifo.profraw");
        assert!(
            Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let other = Job {
            index: 1,
            exe,
            name: "case".into(),
        };
        let err = command_error("test", "failed");
        assert!(
            archive_test_failure(
                shared,
                &other,
                std::slice::from_ref(&fifo),
                None,
                false,
                &err
            )
            .is_err()
        );
        let incomplete = store.join(format!("coverage-failure-{}-1", std::process::id()));
        assert!(!incomplete.join("metadata.json").exists());
        assert!(fifo.exists());
        assert!(preserve.load(Ordering::Relaxed));
    }

    #[cfg(unix)]
    #[test]
    fn failed_test_evidence_survives_profile_teardown() {
        use std::os::unix::fs::PermissionsExt as _;

        for (body, expected, exit, signal) in [
            (
                "printf raw > \"$LLVM_PROFILE_FILE\"; exit 7",
                "test failed",
                Some(7),
                None,
            ),
            ("exit 0", "no profile", Some(0), None),
            (
                "printf raw > \"$LLVM_PROFILE_FILE\"; while :; do :; done",
                "test timed out",
                None,
                None,
            ),
            (
                "printf raw > \"$LLVM_PROFILE_FILE\"; kill -TERM $$",
                "test failed",
                None,
                Some(libc::SIGTERM),
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let repo = temp.path();
            let store = repo.join("store");
            let profiles = store.join("profiles");
            fs::create_dir_all(&profiles).unwrap();
            fs::write(profiles.join("list-old.profraw"), b"list").unwrap();
            fs::set_permissions(
                profiles.join("list-old.profraw"),
                fs::Permissions::from_mode(0o644),
            )
            .unwrap();
            let exe_path = repo.join("test");
            fs::write(&exe_path, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&exe_path, fs::Permissions::from_mode(0o700)).unwrap();
            let exe = TestExe {
                binary_id: "sample".into(),
                path: exe_path,
                manifest_dir: repo.to_path_buf(),
            };
            let object = repo.join("object");
            fs::write(&object, b"object bytes").unwrap();
            let built = Built {
                tests: Vec::new(),
                bins: vec![object.clone()],
            };
            let tools = LlvmTools {
                profdata: repo.join("profdata"),
                cov: repo.join("cov"),
            };
            let opts = CoverageOptions {
                test_timeout: Duration::from_millis(100),
                ..CoverageOptions::default()
            };
            let preserve = AtomicBool::new(false);
            let shared = Shared {
                repo,
                profiles: &profiles,
                tools: &tools,
                built: &built,
                opts: &opts,
                deadline: Deadline {
                    at: Instant::now() + Duration::from_secs(10),
                    total_secs: 10,
                },
                target_dir: repo,
                evidence: Some((&store, "candidate")),
                preserve_profiles: &preserve,
            };
            let job = Job {
                index: 0,
                exe: &exe,
                name: "case".into(),
            };
            let err = cover_one(&shared, &job).unwrap_err();
            assert!(err.to_string().contains(expected), "{err}");
            finish_profiles(&profiles, preserve.load(Ordering::Relaxed)).unwrap();
            assert!(profiles.is_dir());
            let mode = fs::metadata(profiles.join("list-old.profraw"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
            let archived = store.join(format!("coverage-failure-{}-0", std::process::id()));
            assert_test_failure_receipt(&archived, &exe, &object, expected, exit, signal);
            assert!(cover_one(&shared, &job).is_err());
            assert!(preserve.load(Ordering::Relaxed));
            assert!(profiles.is_dir());
            if expected != "no profile" {
                assert_raw_profile_preserved(&profiles);
            }
            if expected == "test failed" {
                assert_fifo_archive_failure(&shared, &exe, &profiles, &store, &preserve);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn retained_listing_profiles_reject_links_without_chmod() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let temp = tempfile::tempdir().unwrap();
        let profiles = temp.path().join("profiles");
        fs::create_dir(&profiles).unwrap();
        let external = temp.path().join("external");
        fs::write(&external, b"secret").unwrap();
        fs::set_permissions(&external, fs::Permissions::from_mode(0o644)).unwrap();
        let link = profiles.join("list-test.profraw");
        let regular = profiles.join("list-regular.profraw");
        fs::write(&regular, b"list").unwrap();
        symlink(&external, &link).unwrap();
        for kind in 0..3 {
            if kind == 1 {
                fs::remove_file(&link).unwrap();
                fs::hard_link(&external, &link).unwrap();
            } else if kind == 2 {
                fs::remove_file(&link).unwrap();
                assert!(
                    Command::new("mkfifo")
                        .arg(&link)
                        .status()
                        .unwrap()
                        .success()
                );
            }
            fs::set_permissions(&regular, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(finish_profiles(&profiles, true).is_err());
            assert_eq!(
                fs::metadata(&regular).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&external).unwrap().permissions().mode() & 0o777,
                0o644
            );
            assert_eq!(fs::read(&external).unwrap(), b"secret");
            assert!(profiles.is_dir());
        }
    }

    #[test]
    fn lcov_rejects_truncated_and_malformed_records() {
        let repo = Path::new("/w/repo");
        let target = repo.join("target");
        for input in [
            "",
            "SF:/w/repo/src/a.rs\nDA:1,1\n",
            "SF:/w/repo/src/a.rs\nDA:1,nope\nend_of_record\n",
            "SF:/w/repo/src/a.rs\nDA:0,1\nend_of_record\n",
            "DA:1,1\nend_of_record\n",
            "SF:/w/repo/src/a.rs\nSF:/w/repo/src/b.rs\nend_of_record\n",
            "SF:/w/repo/src/a.rs\nend_of_record\n",
            "SF:/w/repo/src/a.rs\nDA:1,0\nend_of_record\n",
            "SF:/w/repo/src/a.rs\nLF:0\nLH:0\nend_of_record\n",
            "SF:/w/repo/src/a.rs\nDA:1,1\nend_of_record\nSF:/w/repo/src/b.rs\nLF:1\nLH:0\nend_of_record\n",
            "SF:/w/repo/src/a.rs\nDA:1,1\nUNKNOWN:x\nend_of_record\n",
            "SF:/w/repo/src/a.rs\nDA:1,1\nLF:0\nLH:1\nend_of_record\n",
            "SF:/w/repo/src/a.rs\nDA:1,1\nFNF:1\nFNH:1\nend_of_record\n",
        ] {
            assert!(parse_lcov(input, repo, &target).is_err(), "{input:?}");
        }
    }

    // LLVM 23.1.1 capture from a synthetic Rust crate; only SF paths are adapted.
    // Original e1.lcov SHA-256: 26f7a5f0678d44b0c26af86540d8f947b7194d714c22f1bc4108dcdbe4660ce9.
    #[test]
    fn lcov_accepts_captured_llvm_expansion_counts() {
        let text = r"SF:/w/repo/src/lib.rs
FN:14,_RNCNvCs3rv0duM1tXW_4mini12with_closure0B3_
FN:15,_RNCNvCs3rv0duM1tXW_4mini12with_closures_0B3_
FN:18,_RNvCs3rv0duM1tXW_4mini10with_match
FN:9,_RNvCs3rv0duM1tXW_4mini11macro_multi
FN:13,_RNvCs3rv0duM1tXW_4mini12with_closure
FN:26,_RNvCs3rv0duM1tXW_4mini13with_question
FN:31,_RNvCs3rv0duM1tXW_4mini9with_loop
FNDA:2,_RNCNvCs3rv0duM1tXW_4mini12with_closure0B3_
FNDA:2,_RNCNvCs3rv0duM1tXW_4mini12with_closures_0B3_
FNDA:1,_RNvCs3rv0duM1tXW_4mini10with_match
FNDA:1,_RNvCs3rv0duM1tXW_4mini11macro_multi
FNDA:1,_RNvCs3rv0duM1tXW_4mini12with_closure
FNDA:1,_RNvCs3rv0duM1tXW_4mini13with_question
FNDA:1,_RNvCs3rv0duM1tXW_4mini9with_loop
FNF:7
FNH:7
DA:9,1
DA:10,1
DA:11,1
DA:13,1
DA:14,2
DA:15,2
DA:16,1
DA:18,1
DA:19,1
DA:20,1
DA:21,0
DA:22,0
DA:24,1
DA:26,1
DA:27,1
DA:28,1
DA:29,1
DA:31,1
DA:32,1
DA:33,2
DA:34,2
DA:35,2
DA:36,1
DA:37,1
BRF:0
BRH:0
LF:26
LH:24
end_of_record
SF:/w/repo/tests/it.rs
FN:2,_RNvCsh12IUmPH7JG_2its_4hits
FNDA:1,_RNvCsh12IUmPH7JG_2its_4hits
FNF:1
FNH:1
DA:2,1
DA:3,1
DA:4,1
DA:5,1
DA:6,1
DA:7,1
DA:8,1
BRF:0
BRH:0
LF:7
LH:7
end_of_record
";
        let repo = Path::new("/w/repo");
        let target = repo.join("target");
        assert_eq!(
            parse_lcov(text, repo, &target).expect("captured LLVM lcov"),
            BTreeSet::from(["src/lib.rs".to_owned(), "tests/it.rs".to_owned()])
        );
        for corrupted in [
            text.replacen("LF:26\n", "LF:23\n", 1),
            text.replacen("LH:24\n", "LH:0\n", 1),
            text.replacen("LH:24\n", "LH:27\n", 1),
        ] {
            assert!(parse_lcov(&corrupted, repo, &target).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn coverage_export_passes_repository_filter_to_llvm() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path();
        let exe_path = repo.join("test-exe");
        let profdata = repo.join("profdata");
        let cov = repo.join("cov");
        let argv_path = repo.join("cov-argv");
        for (file, body) in [
            (&exe_path, "printf raw > \"$LLVM_PROFILE_FILE\"".to_owned()),
            (
                &profdata,
                "for arg in \"$@\"; do last=$arg; done; printf merged > \"$last\"".to_owned(),
            ),
            (
                &cov,
                format!(
                    "printf '%s\\n' \"$@\" > '{}'; printf 'SF:{}/src/a.rs\\nDA:1,1\\nLF:1\\nLH:1\\nend_of_record\\n'",
                    argv_path.display(),
                    repo.display()
                ),
            ),
        ] {
            fs::write(file, format!("#!/bin/sh\n{body}\n")).expect("write script");
            fs::set_permissions(file, fs::Permissions::from_mode(0o700)).expect("chmod script");
        }
        let exe = TestExe {
            binary_id: "sample".into(),
            path: exe_path,
            manifest_dir: repo.to_owned(),
        };
        let built = Built {
            tests: Vec::new(),
            bins: Vec::new(),
        };
        let tools = LlvmTools { profdata, cov };
        let opts = CoverageOptions::default();
        let target = repo.join("target");
        let preserve_profiles = AtomicBool::new(false);
        let shared = Shared {
            repo,
            profiles: repo,
            tools: &tools,
            built: &built,
            opts: &opts,
            deadline: Deadline {
                at: Instant::now() + Duration::from_secs(10),
                total_secs: 10,
            },
            target_dir: &target,
            evidence: None,
            preserve_profiles: &preserve_profiles,
        };
        let job = Job {
            index: 0,
            exe: &exe,
            name: "case".into(),
        };
        assert_eq!(
            cover_one(&shared, &job).expect("coverage"),
            Some(BTreeSet::from(["src/a.rs".into()]))
        );
        let argv = fs::read_to_string(argv_path).expect("LLVM argv");
        assert!(argv.lines().any(|arg| arg
            == format!(
                "--include-filename-regex={}",
                coverage_source_filter(repo).unwrap()
            )));
    }

    #[test]
    fn coverage_source_filter_anchors_and_escapes_repository() {
        assert_eq!(
            coverage_source_filter(Path::new("/tmp/a.b+[x](y){z}$^|?*\\c"))
                .expect("UTF-8 repository path"),
            r"^/+(\./+)*tmp/+(\./+)*a\.b\+\[x\]\(y\)\{z\}\$\^\|\?\*\\c/+(\./+)*"
        );
        assert_eq!(
            coverage_source_filter(Path::new("/")).unwrap(),
            r"^/+(\./+)*"
        );
        assert!(coverage_source_filter(Path::new("/tmp/a\nb")).is_err());
    }

    // LLVM 23.1.1 capture 314.lcov, record 23, SHA-256
    // 343cfb668e9de698513c68de1784b54a0c554538692641f4c90c1bd8ff17d96d.
    // SF is anonymized; positive DA lines and LF/LH are unchanged. Zero-hit
    // DA lines and function/branch metadata are omitted from this fixture.
    #[test]
    fn lcov_accepts_captured_summary_with_fewer_hits_than_positive_da() {
        let text = include_str!("../../tests/fixtures/select/llvm23-real-cache-lh-under-da.lcov");
        assert_eq!(
            parse_lcov(text, Path::new("/w/repo"), Path::new("/w/repo/target"))
                .expect("captured LLVM lcov"),
            BTreeSet::from(["src/select/cache.rs".to_owned()])
        );
    }

    // LLVM 23 export SHA-256 56b3c516eca56218015b0e2215455ccd72c13a72c2fe50d967af3006536b70c9,
    // record 379: SF anonymized, function and branch data removed; DA/LF/LH unchanged.
    #[test]
    fn lcov_accepts_captured_external_line_summary_discrepancy() {
        let text = include_str!("../../tests/fixtures/select/llvm23-record379.lcov");
        let repo = Path::new("/w/repo");
        let target = Path::new("/w/repo/target");
        assert_eq!(
            parse_lcov(text, repo, target).expect("captured LLVM lcov"),
            BTreeSet::from(["src/captured.rs".to_owned()])
        );
        let corrupted = text.replace("LH:391\n", "LH:1061\n");
        assert!(parse_lcov(&corrupted, repo, target).is_err());
    }

    #[test]
    fn lcov_rejects_covered_summary_without_a_positive_da() {
        let text = r"SF:/w/repo/src/a.rs
DA:1,0
LF:2
LH:1
end_of_record
SF:/w/repo/src/b.rs
DA:1,1
LF:1
LH:1
end_of_record
";
        assert!(parse_lcov(text, Path::new("/w/repo"), Path::new("/w/repo/target")).is_err());
    }

    // LLVM 23 capture, only SF paths adapted. Original SHA-256:
    // da2cf8a2ce879a5549d1b9fc80e8cd57e40e8399db1df29f810d7c117dbe1c0b.
    #[test]
    fn lcov_accepts_captured_llvm_monomorphizations() {
        let text = r"SF:/w/repo/src/lib.rs
FN:7,_RINvCs3rv0duM1tXW_4mini7generichEB2_
FN:7,_RINvCs3rv0duM1tXW_4mini7generictEB2_
FN:12,_RNvCs3rv0duM1tXW_4mini10uses_macro
FN:16,_RNvCs3rv0duM1tXW_4mini12uses_generic
FNDA:1,_RINvCs3rv0duM1tXW_4mini7generichEB2_
FNDA:1,_RINvCs3rv0duM1tXW_4mini7generictEB2_
FNDA:1,_RNvCs3rv0duM1tXW_4mini10uses_macro
FNDA:1,_RNvCs3rv0duM1tXW_4mini12uses_generic
FNF:3
FNH:3
DA:7,2
DA:8,2
DA:9,2
DA:10,2
DA:12,1
DA:13,1
DA:14,1
DA:16,1
DA:17,1
DA:18,1
BRF:0
BRH:0
LF:10
LH:10
end_of_record
SF:/w/repo/tests/it.rs
FN:2,_RNvCsh12IUmPH7JG_2its_4hits
FNDA:1,_RNvCsh12IUmPH7JG_2its_4hits
FNF:1
FNH:1
DA:2,1
DA:3,1
DA:4,1
DA:5,1
BRF:0
BRH:0
LF:4
LH:4
end_of_record
";
        let repo = Path::new("/w/repo");
        let target = repo.join("target");
        assert_eq!(
            parse_lcov(text, repo, &target).expect("captured LLVM lcov"),
            BTreeSet::from(["src/lib.rs".to_owned(), "tests/it.rs".to_owned()])
        );
        for corrupted in [
            text.replacen("FNF:3\n", "FNF:5\n", 1),
            text.replacen("FNH:3\n", "FNH:5\n", 1),
            text.replacen("FNH:3\n", "FNH:4\n", 1),
        ] {
            assert!(parse_lcov(&corrupted, repo, &target).is_err());
        }
    }

    #[test]
    fn lcov_accepts_llvm_export_fields_and_checksums() {
        let text = "TN:\nSF:/w/repo/src/a.rs\nFN:2,_Rfoo\nFNDA:1,_Rfoo\nFNF:1\nFNH:1\nDA:2,1,checksum\nDA:3,0\nLF:2\nLH:1\nend_of_record\nSF:/w/repo/src/unused.rs\nLF:0\nLH:0\nend_of_record\n";
        assert_eq!(
            parse_lcov(text, Path::new("/w/repo"), Path::new("/w/repo/target"))
                .expect("valid LLVM-shaped lcov"),
            BTreeSet::from(["src/a.rs".to_owned()])
        );
    }

    /// Corpus de fuzzing LCOV : chaque fichier de `tests/fixtures/select/fuzz/lcov` est
    /// comparé à son verdict attendu (`expectations-lcov.json`).
    #[test]
    fn lcov_fuzz_corpus_matches_expectations() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/select/fuzz");
        let expectations: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(dir.join("expectations-lcov.json")).expect("expectations"),
        )
        .expect("expectations json");
        let expectations = expectations.as_object().expect("object");
        let repo = Path::new("/w/repo");
        let target = repo.join("target");
        for (name, expected) in expectations {
            let text = fs::read_to_string(dir.join("lcov").join(name)).expect(name);
            let digest = format!("{:x}", sha2::Sha256::digest(text.as_bytes()));
            assert_eq!(expected["sha256"].as_str(), Some(digest.as_str()), "{name}");
            let got = parse_lcov(&text, repo, &target);
            let verdict = if got.is_ok() { "accept" } else { "reject" };
            assert_eq!(expected["verdict"].as_str(), Some(verdict), "{name}");
            if let Ok(kept) = got {
                let want: BTreeSet<RelPath> = expected["files"]
                    .as_array()
                    .expect("files")
                    .iter()
                    .map(|value| value.as_str().expect("path").to_owned())
                    .collect();
                assert_eq!(kept, want, "{name}");
            }
        }
        assert_eq!(
            fs::read_dir(dir.join("lcov")).expect("corpus").count(),
            expectations.len(),
            "chaque fichier du corpus doit avoir son verdict attendu"
        );
    }

    /// Fichiers à `DA` positif d'un export, lus sans la validation de [`parse_lcov`] :
    /// borne supérieure indépendante pour les mutations.
    fn positive_da_files(text: &str) -> BTreeSet<RelPath> {
        let mut out = BTreeSet::new();
        let mut current: Option<RelPath> = None;
        for line in text.lines() {
            if let Some(path) = line.strip_prefix("SF:") {
                current = path.strip_prefix("/w/repo/").map(str::to_owned);
            } else if let Some(da) = line.strip_prefix("DA:")
                && da
                    .split(',')
                    .nth(1)
                    .and_then(|hits| hits.parse::<u64>().ok())
                    .is_some_and(|hits| hits > 0)
                && let Some(file) = &current
            {
                out.insert(file.clone());
            }
        }
        out
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 96,
            rng_seed: proptest::test_runner::RngSeed::Fixed(0x006b_696f_6b75),
            failure_persistence: None,
            ..ProptestConfig::default()
        })]

        /// Une mutation d'un export réel ne panique jamais et n'invente aucun fichier :
        /// un export accepté ne cite que des fichiers dont une ligne `DA` est positive.
        #[test]
        fn lcov_mutations_never_panic_or_invent_files(
            base in 0usize..4,
            ops in proptest::collection::vec((0u8..5, any::<usize>(), 0u8..=255), 1..6),
        ) {
            const CORPUS: [&str; 4] = [
                include_str!("../../tests/fixtures/select/fuzz/lcov/real-llvm23-kioku-6records.lcov"),
                include_str!("../../tests/fixtures/select/fuzz/lcov/real-llvm23-constructs.lcov"),
                include_str!("../../tests/fixtures/select/fuzz/lcov/real-llvm23-monomorph.lcov"),
                include_str!("../../tests/fixtures/select/fuzz/lcov/edge-multi-record-mixed.lcov"),
            ];
            let mut bytes = CORPUS[base].as_bytes().to_vec();
            for (op, at, value) in ops {
                let at = at % (bytes.len() + 1);
                match op {
                    0 => bytes.truncate(at),
                    1 => bytes.insert(at, value),
                    2 => {
                        if at < bytes.len() {
                            bytes.remove(at);
                        }
                    }
                    3 => {
                        if at < bytes.len() {
                            bytes[at] = value;
                        }
                    }
                    _ => {
                        let start = bytes[..at]
                            .iter()
                            .rposition(|b| *b == b'\n')
                            .map_or(0, |index| index + 1);
                        let end = bytes[at..]
                            .iter()
                            .position(|b| *b == b'\n')
                            .map_or(bytes.len(), |index| at + index + 1);
                        let line = bytes[start..end].to_vec();
                        let mut next = Vec::with_capacity(bytes.len() + line.len());
                        next.extend_from_slice(&bytes[..at]);
                        next.extend_from_slice(&line);
                        next.extend_from_slice(&bytes[at..]);
                        bytes = next;
                    }
                }
            }
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let repo = Path::new("/w/repo");
            let target = repo.join("target");
            if let Ok(kept) = parse_lcov(&text, repo, &target) {
                let bound = positive_da_files(&text);
                prop_assert!(
                    kept.is_subset(&bound),
                    "fichiers inventés : {:?}",
                    kept.difference(&bound).collect::<Vec<_>>()
                );
                for path in &kept {
                    prop_assert!(is_repo_relative(path), "{path:?}");
                    prop_assert!(!path.starts_with("target/"), "{path:?}");
                }
            }
        }
    }

    #[test]
    fn lcov_keeps_executed_repo_files_only() {
        let repo = Path::new("/w/repo");
        let target = Path::new("/w/repo/target/select-cov");
        let lcov = "SF:/w/repo/src/a.rs\nDA:1,0\nDA:2,3\nend_of_record\n\
                    SF:/w/repo/src/never.rs\nDA:1,0\nend_of_record\n\
                    SF:/w/repo/target/select-cov/build/gen.rs\nDA:1,1\nend_of_record\n\
                    SF:/home/user/.cargo/registry/x.rs\nDA:1,1\nend_of_record\n\
                    SF:/w/repo/../escape.rs\nDA:1,1\nend_of_record\n\
                    SF:/w/repo/tests/t.rs\nDA:5,1\nend_of_record\n";
        let got = parse_lcov(lcov, repo, target).expect("valid lcov");
        assert_eq!(
            got.into_iter().collect::<Vec<_>>(),
            vec!["src/a.rs".to_owned(), "tests/t.rs".to_owned()]
        );
    }
}
