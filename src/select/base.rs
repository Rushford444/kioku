//! Lot L1 : inventaire de la base (dernier état vert de `main`).
//!
//! `collect` interroge `cargo metadata`, `cargo nextest list` et les outils (versions) puis
//! produit un [`BaseInventory`] **non vert** (aucun statut connu). `apply_junit` y reporte le
//! résultat d'un run complet (`JUnit` nextest) et calcule `green`. `load` / `save` fixent le
//! format JSON sur disque et refusent tout fichier invalide (fail closed).
//!
//! Les parseurs (`parse_metadata`, `parse_nextest_list`, `apply_junit`) sont purs : ils
//! travaillent sur des chaînes et sont testés sans processus. Aucune valeur de variable
//! d'environnement n'est conservée en clair (hachage blake3).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;

use super::{
    BASE_SCHEMA, BaseBinary, BaseInventory, BaseStatus, BaseTest, BinaryKind, RelPath, SelectError,
    seal,
};

/// Taille maximale d'une sortie JSON d'outil lue (`cargo metadata`, `nextest list`).
const MAX_TOOL_JSON: usize = 64 * 1024 * 1024;
/// Taille maximale d'un `JUnit` ou d'un fichier d'inventaire.
const MAX_DOCUMENT: usize = 128 * 1024 * 1024;
/// Octets de `stderr` conservés pour les messages d'erreur.
const STDERR_KEEP: usize = 2048;
/// Délai de `cargo metadata`.
const METADATA_TIMEOUT: Duration = Duration::from_secs(120);
/// Délai de `cargo nextest list` (inclut la compilation des binaires de test).
const LIST_TIMEOUT: Duration = Duration::from_secs(1800);
/// Délai des commandes de version et de `git`.
const SHORT_TIMEOUT: Duration = Duration::from_secs(60);
/// Attente maximale de la fin du drainage des tubes après la fin du processus.
const DRAIN_GRACE: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Utilitaires partagés avec `changes` et `classify`
// ---------------------------------------------------------------------------

/// Erreur `Invalid` à partir d'un libellé et d'un message.
pub(crate) fn invalid(what: &str, message: impl Into<String>) -> SelectError {
    SelectError::Invalid {
        what: what.to_owned(),
        message: message.into(),
    }
}

/// Erreur `Io` pour un chemin.
pub(crate) fn io_error(path: &Path, error: &std::io::Error) -> SelectError {
    SelectError::Io {
        path: path.display().to_string(),
        message: error.to_string(),
    }
}

/// Erreur `Command` (message borné).
pub(crate) fn command_error(command: &str, message: impl Into<String>) -> SelectError {
    let mut message: String = message.into();
    if message.len() > 600 {
        let mut end = 600;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
        message.push('…');
    }
    SelectError::Command {
        command: command.to_owned(),
        message,
    }
}

/// Valide un chemin relatif au dépôt : non vide, relatif, séparateur `/`, aucun
/// composant vide, `.` ou `..`, pas d'octet NUL.
pub(crate) fn validate_rel_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("chemin vide".to_owned());
    }
    if path.starts_with('/') || path.contains('\0') {
        return Err(format!("chemin non relatif ou illisible : {path:?}"));
    }
    if path
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(format!("chemin non normalisé : {path:?}"));
    }
    Ok(())
}

#[derive(Debug)]
struct Drained {
    data: Vec<u8>,
    truncated: bool,
}

/// Lit un tube jusqu'à sa fermeture dans un fil, en gardant au plus `keep` octets.
fn drain<R: Read + Send + 'static>(pipe: Option<R>, keep: usize) -> mpsc::Receiver<Drained> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut data = Vec::new();
        let mut truncated = false;
        if let Some(mut pipe) = pipe {
            let mut buffer = vec![0_u8; 65_536].into_boxed_slice();
            loop {
                match pipe.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        let room = keep.saturating_sub(data.len());
                        if read > room {
                            truncated = true;
                        }
                        data.extend_from_slice(&buffer[..read.min(room)]);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        }
        let _ = sender.send(Drained { data, truncated });
    });
    receiver
}

/// Lance une commande avec délai, sortie bornée et `stdin` fermé ; renvoie `stdout`.
///
/// Toute sortie non nulle, tout délai dépassé et toute sortie plus grande que
/// `max_stdout` sont des erreurs (fail closed).
pub(crate) fn run_command(
    program: &str,
    args: &[&str],
    cwd: &Path,
    timeout: Duration,
    max_stdout: usize,
) -> Result<Vec<u8>, SelectError> {
    let label = {
        let mut label = format!("{program} {}", args.join(" "));
        if label.len() > 120 {
            let mut end = 120;
            while !label.is_char_boundary(end) {
                end -= 1;
            }
            label.truncate(end);
        }
        label
    };
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| command_error(&label, format!("lancement impossible : {error}")))?;
    let stdout = drain(child.stdout.take(), max_stdout);
    let stderr = drain(child.stderr.take(), STDERR_KEEP);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(command_error(
                    &label,
                    format!("attente impossible : {error}"),
                ));
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SelectError::Timeout {
                what: label,
                seconds: timeout.as_secs(),
            });
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout
        .recv_timeout(DRAIN_GRACE)
        .map_err(|_| command_error(&label, "sortie standard non drainée"))?;
    let stderr = stderr.recv_timeout(DRAIN_GRACE).unwrap_or(Drained {
        data: Vec::new(),
        truncated: false,
    });
    if !status.success() {
        let code = status
            .code()
            .map_or_else(|| "signal".to_owned(), |code| code.to_string());
        let tail = String::from_utf8_lossy(&stderr.data);
        return Err(command_error(
            &label,
            format!("sortie {code} : {}", tail.trim()),
        ));
    }
    if stdout.truncated {
        return Err(invalid(&label, format!("sortie > {max_stdout} octets")));
    }
    Ok(stdout.data)
}

fn utf8<'a>(bytes: &'a [u8], what: &str) -> Result<&'a str, SelectError> {
    std::str::from_utf8(bytes).map_err(|_| invalid(what, "sortie non UTF-8"))
}

fn is_commit_hex(commit: &str) -> bool {
    commit.len() == 40
        && commit
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

// ---------------------------------------------------------------------------
// Environnement : toolchain et variables de build
// ---------------------------------------------------------------------------

/// Variables exactes qui influencent la compilation ou l'exécution des tests.
const BUILD_ENV_EXACT: [&str; 9] = [
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "RUSTDOCFLAGS",
    "CARGO_ENCODED_RUSTDOCFLAGS",
    "RUSTC",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "RUSTUP_TOOLCHAIN",
    "CARGO_INCREMENTAL",
];
/// Préfixes de variables de build (`CARGO_BUILD_*`, `CARGO_PROFILE_*`, `CARGO_TARGET_*`).
const BUILD_ENV_PREFIXES: [&str; 3] = ["CARGO_BUILD_", "CARGO_PROFILE_", "CARGO_TARGET_"];
/// Exceptions : elles ne déplacent que les artefacts, jamais la sémantique des tests.
const BUILD_ENV_EXCLUDED: [&str; 2] = ["CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"];

/// Cette variable d'environnement fait-elle partie de l'environnement de build suivi ?
#[must_use]
pub fn is_build_env_name(name: &str) -> bool {
    if BUILD_ENV_EXCLUDED.contains(&name) {
        return false;
    }
    BUILD_ENV_EXACT.contains(&name)
        || BUILD_ENV_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// Retient les variables de build et remplace chaque valeur par son hachage blake3
/// (`blake3(nom 0x00 valeur)`, hexadécimal). Aucune valeur n'est conservée en clair.
pub fn hash_build_env<I>(vars: I) -> BTreeMap<String, String>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    let mut out = BTreeMap::new();
    for (name, value) in vars {
        let Some(name) = name.to_str() else { continue };
        if !is_build_env_name(name) {
            continue;
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update(name.as_bytes());
        hasher.update(&[0]);
        hasher.update(value.as_encoded_bytes());
        out.insert(name.to_owned(), hasher.finalize().to_hex().to_string());
    }
    out
}

/// Variables de build du processus courant, valeurs hachées.
#[must_use]
pub fn current_build_env() -> BTreeMap<String, String> {
    hash_build_env(std::env::vars_os())
}

fn first_useful_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

/// Versions des outils (`rustc -vV`, `cargo -V`, `cargo nextest --version`), première ligne
/// utile de chacun, sous les clés `rustc`, `cargo`, `nextest`. Exécuté dans `repo` (un
/// `rust-toolchain` du dépôt compte).
///
/// # Errors
///
/// Outil absent, en échec, en dépassement de délai ou sortie vide.
pub fn current_toolchain(repo: &Path) -> Result<BTreeMap<String, String>, SelectError> {
    let probes: [(&str, &str, &[&str]); 3] = [
        ("rustc", "rustc", &["-vV"]),
        ("cargo", "cargo", &["-V"]),
        ("nextest", "cargo", &["nextest", "--version"]),
    ];
    let mut out = BTreeMap::new();
    for (key, program, args) in probes {
        let stdout = run_command(program, args, repo, SHORT_TIMEOUT, 64 * 1024)?;
        let line = first_useful_line(&stdout)
            .ok_or_else(|| invalid(&format!("{program} version"), "sortie vide"))?;
        out.insert(key.to_owned(), line);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// cargo metadata
// ---------------------------------------------------------------------------

/// Une cible Cargo (`cargo metadata --no-deps`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaTarget {
    /// Nom du paquet.
    pub package: String,
    /// Nom de la cible.
    pub name: String,
    /// Nature normalisée : `lib`, `proc-macro`, `bin`, `test`, `example`, `bench`,
    /// `custom-build`.
    pub kind: String,
    /// Fichier racine, relatif à `workspace_root`.
    pub src_path: RelPath,
}

/// Résultat du parse de `cargo metadata`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    /// `workspace_root` tel que publié par Cargo.
    pub workspace_root: String,
    /// Toutes les cibles des membres du workspace.
    pub targets: Vec<MetaTarget>,
}

#[derive(Deserialize)]
struct RawMetadata {
    workspace_root: String,
    packages: Vec<RawPackage>,
}

#[derive(Deserialize)]
struct RawPackage {
    name: String,
    targets: Vec<RawTarget>,
}

#[derive(Deserialize)]
struct RawTarget {
    name: String,
    kind: Vec<String>,
    src_path: String,
}

fn normalize_target_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" => Some("lib"),
        "proc-macro" => Some("proc-macro"),
        "bin" => Some("bin"),
        "test" => Some("test"),
        "example" => Some("example"),
        "bench" => Some("bench"),
        "custom-build" => Some("custom-build"),
        _ => None,
    }
}

fn relative_to(root: &Path, path: &str) -> Result<RelPath, String> {
    let relative = Path::new(path)
        .strip_prefix(root)
        .map_err(|_| format!("{path} hors de {}", root.display()))?;
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(part) => parts.push(
                part.to_str()
                    .ok_or_else(|| format!("chemin non UTF-8 : {path}"))?,
            ),
            _ => return Err(format!("chemin non normalisé : {path}")),
        }
    }
    let joined = parts.join("/");
    validate_rel_path(&joined)?;
    Ok(joined)
}

/// Lit la sortie de `cargo metadata --no-deps --format-version 1`.
///
/// # Errors
///
/// JSON illisible, nature de cible inconnue, `src_path` hors du workspace ou cibles en
/// double.
pub fn parse_metadata(json: &str) -> Result<Metadata, SelectError> {
    let raw: RawMetadata =
        serde_json::from_str(json).map_err(|error| invalid("cargo metadata", error.to_string()))?;
    let root = Path::new(&raw.workspace_root);
    let mut targets = Vec::new();
    let mut seen = BTreeSet::new();
    for package in &raw.packages {
        for target in &package.targets {
            let [kind] = target.kind.as_slice() else {
                return Err(invalid(
                    "cargo metadata",
                    format!("cible {} : nature multiple ou vide", target.name),
                ));
            };
            let kind = normalize_target_kind(kind).ok_or_else(|| {
                invalid(
                    "cargo metadata",
                    format!("cible {} : nature inconnue {kind}", target.name),
                )
            })?;
            let src_path = relative_to(root, &target.src_path)
                .map_err(|message| invalid("cargo metadata", message))?;
            if !seen.insert((package.name.clone(), target.name.clone(), kind)) {
                return Err(invalid(
                    "cargo metadata",
                    format!("cible en double : {} {} {kind}", package.name, target.name),
                ));
            }
            targets.push(MetaTarget {
                package: package.name.clone(),
                name: target.name.clone(),
                kind: kind.to_owned(),
                src_path,
            });
        }
    }
    Ok(Metadata {
        workspace_root: raw.workspace_root,
        targets,
    })
}

// ---------------------------------------------------------------------------
// cargo nextest list
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RawList {
    #[serde(rename = "rust-suites")]
    rust_suites: BTreeMap<String, RawSuite>,
    #[serde(rename = "test-count")]
    test_count: Option<u64>,
}

#[derive(Deserialize)]
struct RawSuite {
    #[serde(rename = "package-name")]
    package_name: String,
    #[serde(rename = "binary-id")]
    binary_id: String,
    #[serde(rename = "binary-name")]
    binary_name: String,
    kind: String,
    status: String,
    testcases: BTreeMap<String, RawCase>,
}

#[derive(Deserialize)]
struct RawCase {
    ignored: bool,
}

fn binary_kind(kind: &str) -> BinaryKind {
    match kind {
        "lib" => BinaryKind::Lib,
        "bin" => BinaryKind::Bin,
        "test" => BinaryKind::Test,
        _ => BinaryKind::Other,
    }
}

fn find_target<'a>(meta: &'a Metadata, suite: &RawSuite) -> Option<&'a MetaTarget> {
    let kind = normalize_target_kind(&suite.kind)?;
    meta.targets
        .iter()
        .find(|t| t.package == suite.package_name && t.kind == kind && t.name == suite.binary_name)
        .or_else(|| {
            // Une bibliothèque est unique par paquet : repli sur le paquet seul.
            if kind == "lib" || kind == "proc-macro" {
                let mut libs = meta
                    .targets
                    .iter()
                    .filter(|t| t.package == suite.package_name && t.kind == kind);
                match (libs.next(), libs.next()) {
                    (Some(only), None) => Some(only),
                    _ => None,
                }
            } else {
                None
            }
        })
}

/// Lit la sortie de `cargo nextest list --message-format json` et la relie aux cibles de
/// `cargo metadata`. Les tests `ignored` ont le statut `Ignored`, les autres `Unknown`
/// (aucun résultat n'est connu avant [`apply_junit`]).
///
/// # Errors
///
/// JSON illisible, binaire non `listed`, clé différente de `binary-id`, binaire sans cible
/// Cargo correspondante, binaire en double, ou `test-count` incohérent.
pub fn parse_nextest_list(json: &str, meta: &Metadata) -> Result<Vec<BaseBinary>, SelectError> {
    let what = "nextest list";
    let raw: RawList =
        serde_json::from_str(json).map_err(|error| invalid(what, error.to_string()))?;
    let mut binaries = Vec::new();
    let mut total = 0_u64;
    for (key, suite) in &raw.rust_suites {
        if *key != suite.binary_id {
            return Err(invalid(
                what,
                format!("clé {key} différente de binary-id {}", suite.binary_id),
            ));
        }
        if suite.status != "listed" {
            return Err(invalid(
                what,
                format!("binaire {key} : statut {} (attendu listed)", suite.status),
            ));
        }
        let target = find_target(meta, suite).ok_or_else(|| {
            invalid(
                what,
                format!("binaire {key} sans cible dans cargo metadata"),
            )
        })?;
        let tests: Vec<BaseTest> = suite
            .testcases
            .iter()
            .map(|(name, case)| BaseTest {
                name: name.clone(),
                status: if case.ignored {
                    BaseStatus::Ignored
                } else {
                    BaseStatus::Unknown
                },
                duration_ms: None,
            })
            .collect();
        total += tests.len() as u64;
        binaries.push(BaseBinary {
            binary_id: suite.binary_id.clone(),
            target_name: suite.binary_name.clone(),
            kind: binary_kind(&suite.kind),
            src_path: target.src_path.clone(),
            tests,
        });
    }
    if let Some(declared) = raw.test_count
        && declared != total
    {
        return Err(invalid(
            what,
            format!("test-count {declared} différent du nombre de tests lus {total}"),
        ));
    }
    binaries.sort_by(|a, b| a.binary_id.cmp(&b.binary_id));
    Ok(binaries)
}

// ---------------------------------------------------------------------------
// Collecte (processus)
// ---------------------------------------------------------------------------

pub(crate) fn normalize_features(features: &[String]) -> Result<Vec<String>, SelectError> {
    let mut out = Vec::new();
    for feature in features {
        let valid = !feature.is_empty()
            && feature.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'/' | b'+' | b'.')
            });
        if !valid {
            return Err(invalid("features", format!("nom refusé : {feature:?}")));
        }
        out.push(feature.clone());
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn ensure_checkout_is(repo: &Path, commit: &str) -> Result<(), SelectError> {
    let head = run_command(
        "git",
        &["--no-optional-locks", "rev-parse", "HEAD"],
        repo,
        SHORT_TIMEOUT,
        1024,
    )?;
    if utf8(&head, "git rev-parse")?.trim() != commit {
        return Err(invalid(
            "base",
            format!("HEAD n'est pas le commit de base {commit}"),
        ));
    }
    let dirty = run_command(
        "git",
        &[
            "--no-optional-locks",
            "status",
            "--porcelain",
            "-z",
            "--untracked-files=no",
        ],
        repo,
        SHORT_TIMEOUT,
        MAX_TOOL_JSON,
    )?;
    if !dirty.is_empty() {
        return Err(invalid(
            "base",
            "l'arbre de travail a des modifications suivies : l'inventaire ne décrirait pas le commit",
        ));
    }
    Ok(())
}

/// Inventorie la base : binaires, tests, versions, environnement de build.
///
/// `repo` doit être la racine du dépôt, `HEAD` doit valoir `commit` et l'arbre suivi doit
/// être propre. Le résultat est **non vert** (`green == false`, tests `Unknown`) jusqu'à
/// [`apply_junit`]. Les valeurs des variables d'environnement sont hachées.
///
/// # Errors
///
/// Commit non conforme (40 hex minuscules), `HEAD` différent, arbre modifié, feature
/// illisible, outil absent/en échec/trop lent, sortie illisible, workspace différent de
/// `repo`, binaire sans cible Cargo.
pub fn collect(
    repo: &Path,
    commit: &str,
    features: &[String],
) -> Result<BaseInventory, SelectError> {
    if !is_commit_hex(commit) {
        return Err(invalid(
            "base",
            "commit attendu : 40 hexadécimaux minuscules",
        ));
    }
    let repo = repo
        .canonicalize()
        .map_err(|error| io_error(repo, &error))?;
    let features = normalize_features(features)?;
    ensure_checkout_is(&repo, commit)?;

    let metadata = run_command(
        "cargo",
        &["metadata", "--no-deps", "--format-version", "1", "--locked"],
        &repo,
        METADATA_TIMEOUT,
        MAX_TOOL_JSON,
    )?;
    let meta = parse_metadata(utf8(&metadata, "cargo metadata")?)?;
    let root = Path::new(&meta.workspace_root)
        .canonicalize()
        .map_err(|error| io_error(Path::new(&meta.workspace_root), &error))?;
    if root != repo {
        return Err(invalid(
            "cargo metadata",
            format!(
                "workspace_root {} différent du dépôt {}",
                root.display(),
                repo.display()
            ),
        ));
    }

    let features_arg = format!("--features={}", features.join(","));
    let mut args = vec!["nextest", "list", "--message-format", "json", "--locked"];
    if !features.is_empty() {
        args.push(&features_arg);
    }
    let listed = run_command("cargo", &args, &repo, LIST_TIMEOUT, MAX_TOOL_JSON)?;
    let binaries = parse_nextest_list(utf8(&listed, "nextest list")?, &meta)?;

    let inventory = BaseInventory {
        schema: BASE_SCHEMA.to_owned(),
        commit: commit.to_owned(),
        toolchain: current_toolchain(&repo)?,
        features,
        build_env: current_build_env(),
        green: false,
        binaries,
        graph_digest: None,
    };
    validate(&inventory)?;
    Ok(inventory)
}

// ---------------------------------------------------------------------------
// `JUnit` nextest
// ---------------------------------------------------------------------------

/// Jeton lexical d'un document XML.
#[derive(Debug)]
enum Token {
    Open {
        name: String,
        attrs: Vec<(String, String)>,
        empty: bool,
    },
    Close(String),
}

/// Lecteur XML minimal et borné : éléments, attributs, commentaires, CDATA ; refuse tout
/// le reste (DOCTYPE, entités inconnues, attributs en double, guillemets manquants).
struct Lexer<'a> {
    src: &'a str,
    pos: usize,
}

const MAX_NAME: usize = 128;
const MAX_ATTR_VALUE: usize = 16 * 1024;

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b':' | b'.' | b'-')
}

fn decode_entities(raw: &str) -> Result<String, String> {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let end = after
            .find(';')
            .filter(|end| *end <= 10)
            .ok_or_else(|| "entité non terminée".to_owned())?;
        let entity = &after[..end];
        match entity {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ => {
                let digits = entity
                    .strip_prefix("#x")
                    .map(|hex| u32::from_str_radix(hex, 16))
                    .or_else(|| entity.strip_prefix('#').map(str::parse::<u32>));
                let character = match digits {
                    Some(Ok(code)) => char::from_u32(code).filter(|c| *c != '\0'),
                    _ => None,
                };
                out.push(character.ok_or_else(|| format!("entité inconnue &{entity};"))?);
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

impl Lexer<'_> {
    fn bytes(&self) -> &[u8] {
        self.src.as_bytes()
    }

    fn peek(&self) -> Option<u8> {
        self.bytes().get(self.pos).copied()
    }

    fn skip_space(&mut self) {
        while self.peek().is_some_and(|b| b.is_ascii_whitespace()) {
            self.pos += 1;
        }
    }

    fn skip_past(&mut self, open_len: usize, marker: &str) -> Result<(), String> {
        let from = self.pos + open_len;
        let found = self.src[from..]
            .find(marker)
            .ok_or_else(|| format!("{marker} manquant"))?;
        self.pos = from + found + marker.len();
        Ok(())
    }

    fn name(&mut self) -> Result<String, String> {
        let start = self.pos;
        while self.peek().is_some_and(is_name_byte) {
            self.pos += 1;
        }
        let name = &self.src[start..self.pos];
        if name.is_empty() || name.len() > MAX_NAME {
            return Err("nom d'élément ou d'attribut invalide".to_owned());
        }
        Ok(name.to_owned())
    }

    fn next_token(&mut self) -> Result<Option<Token>, String> {
        loop {
            let Some(offset) = self.src[self.pos..].find('<') else {
                self.pos = self.src.len();
                return Ok(None);
            };
            self.pos += offset;
            let rest = &self.src[self.pos..];
            if rest.starts_with("<!--") {
                self.skip_past(4, "-->")?;
            } else if rest.starts_with("<![CDATA[") {
                self.skip_past(9, "]]>")?;
            } else if rest.starts_with("<?") {
                self.skip_past(2, "?>")?;
            } else if rest.starts_with("<!") {
                return Err("déclaration <! refusée (DOCTYPE)".to_owned());
            } else if rest.starts_with("</") {
                self.pos += 2;
                let name = self.name()?;
                self.skip_space();
                if self.peek() != Some(b'>') {
                    return Err(format!("balise fermante </{name} mal formée"));
                }
                self.pos += 1;
                return Ok(Some(Token::Close(name)));
            } else {
                return self.open_tag().map(Some);
            }
        }
    }

    fn open_tag(&mut self) -> Result<Token, String> {
        self.pos += 1;
        let name = self.name()?;
        let mut attrs: Vec<(String, String)> = Vec::new();
        loop {
            let before = self.pos;
            self.skip_space();
            let spaced = self.pos > before;
            match self.peek() {
                None => return Err(format!("balise <{name} non terminée")),
                Some(b'>') => {
                    self.pos += 1;
                    return Ok(Token::Open {
                        name,
                        attrs,
                        empty: false,
                    });
                }
                Some(b'/') => {
                    if self.bytes().get(self.pos + 1) != Some(&b'>') {
                        return Err(format!("balise <{name} mal fermée"));
                    }
                    self.pos += 2;
                    return Ok(Token::Open {
                        name,
                        attrs,
                        empty: true,
                    });
                }
                Some(_) if !spaced => {
                    return Err(format!("balise <{name} : attributs mal séparés"));
                }
                Some(_) => {}
            }
            let key = self.name()?;
            self.skip_space();
            if self.peek() != Some(b'=') {
                return Err(format!("attribut {key} sans valeur"));
            }
            self.pos += 1;
            self.skip_space();
            let Some(quote @ (b'"' | b'\'')) = self.peek() else {
                return Err(format!("attribut {key} sans guillemets"));
            };
            self.pos += 1;
            let start = self.pos;
            while self.peek().is_some_and(|b| b != quote) {
                self.pos += 1;
            }
            if self.peek().is_none() {
                return Err(format!("attribut {key} non terminé"));
            }
            let raw = &self.src[start..self.pos];
            self.pos += 1;
            if raw.len() > MAX_ATTR_VALUE || raw.contains('<') {
                return Err(format!("attribut {key} refusé (taille ou '<')"));
            }
            if attrs.iter().any(|(existing, _)| *existing == key) {
                return Err(format!("attribut {key} en double"));
            }
            attrs.push((key, decode_entities(raw)?));
        }
    }
}

fn attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// Durée `secondes[.fraction]` en millisecondes (tronquée) ; refuse tout autre format.
fn parse_seconds_ms(text: &str) -> Option<u64> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if whole.is_empty()
        || !digits(whole)
        || !digits(fraction)
        || (text.contains('.') && fraction.is_empty())
    {
        return None;
    }
    let mut millis: u64 = 0;
    for position in 0..3 {
        let digit = fraction
            .as_bytes()
            .get(position)
            .map_or(0, |b| u64::from(b - b'0'));
        millis = millis * 10 + digit;
    }
    whole
        .parse::<u64>()
        .ok()?
        .checked_mul(1000)?
        .checked_add(millis)
}

#[derive(Default)]
struct CaseAcc {
    name: String,
    duration_ms: Option<u64>,
    failed: bool,
    rerun: bool,
    skipped: bool,
}

impl CaseAcc {
    fn status(&self) -> Result<BaseStatus, String> {
        if self.skipped && (self.failed || self.rerun) {
            return Err(format!(
                "test {} : skipped combiné à un échec (ambigu)",
                self.name
            ));
        }
        Ok(if self.failed {
            BaseStatus::Fail
        } else if self.rerun {
            BaseStatus::Flaky
        } else if self.skipped {
            BaseStatus::Ignored
        } else {
            BaseStatus::Pass
        })
    }
}

/// Résultats d'un `JUnit` nextest : `(binary-id, nom du test)` → (statut, durée en ms).
pub type JunitResults = BTreeMap<(String, String), (BaseStatus, Option<u64>)>;

fn open_case_child(case: &mut CaseAcc, name: &str) -> Result<(), String> {
    match name {
        "failure" | "error" => case.failed = true,
        "flakyFailure" | "flakyError" | "rerunFailure" | "rerunError" => case.rerun = true,
        "skipped" => case.skipped = true,
        "system-out" | "system-err" => {}
        other => {
            return Err(format!(
                "élément <{other}> inconnu dans le test {}",
                case.name
            ));
        }
    }
    Ok(())
}

/// Lit un `JUnit` nextest : `testsuites > testsuite[name] > testcase[name,time] > …`.
fn parse_junit(xml: &str) -> Result<JunitResults, String> {
    let mut lexer = Lexer { src: xml, pos: 0 };
    let mut stack: Vec<String> = Vec::new();
    let mut results = JunitResults::new();
    let mut suites = BTreeSet::new();
    let mut suite = String::new();
    let mut case = CaseAcc::default();
    let mut root_closed = false;
    let mut root_seen = false;
    while let Some(token) = lexer.next_token()? {
        let (name, attrs, empty) = match token {
            Token::Open { name, attrs, empty } => (name, attrs, empty),
            Token::Close(name) => {
                if stack.pop().as_deref() != Some(name.as_str()) {
                    return Err(format!("balise fermante </{name}> inattendue"));
                }
                match name.as_str() {
                    "testcase" => finish_case(&suite, &case, &mut results)?,
                    "testsuites" => root_closed = true,
                    _ => {}
                }
                continue;
            }
        };
        match stack.last().map(String::as_str) {
            None => {
                if root_seen || root_closed || name != "testsuites" {
                    return Err(format!("racine <{name}> inattendue"));
                }
                root_seen = true;
            }
            Some("testsuites") => {
                if name != "testsuite" {
                    return Err(format!("<{name}> inattendu sous testsuites"));
                }
                attr(&attrs, "name")
                    .filter(|n| !n.is_empty())
                    .ok_or("testsuite sans name")?
                    .clone_into(&mut suite);
                if !suites.insert(suite.clone()) {
                    return Err(format!("testsuite {suite} en double"));
                }
            }
            Some("testsuite") => {
                if name != "testcase" {
                    return Err(format!("<{name}> inattendu sous testsuite {suite}"));
                }
                let test = attr(&attrs, "name")
                    .filter(|n| !n.is_empty())
                    .ok_or_else(|| format!("testcase sans name dans {suite}"))?;
                let duration_ms = match attr(&attrs, "time") {
                    None => None,
                    Some(time) => Some(
                        parse_seconds_ms(time)
                            .ok_or_else(|| format!("time illisible pour {test} : {time:?}"))?,
                    ),
                };
                case = CaseAcc {
                    name: test.to_owned(),
                    duration_ms,
                    ..CaseAcc::default()
                };
            }
            Some("testcase") => open_case_child(&mut case, &name)?,
            Some(parent) => {
                return Err(format!("<{name}> inattendu sous <{parent}>"));
            }
        }
        if empty {
            // Équivaut à une ouverture immédiatement suivie de la fermeture.
            match name.as_str() {
                "testcase" => finish_case(&suite, &case, &mut results)?,
                "testsuites" => root_closed = true,
                _ => {}
            }
        } else {
            stack.push(name);
        }
    }
    if !stack.is_empty() || !root_seen {
        return Err("document incomplet".to_owned());
    }
    Ok(results)
}

fn finish_case(suite: &str, case: &CaseAcc, results: &mut JunitResults) -> Result<(), String> {
    let status = case.status()?;
    let key = (suite.to_owned(), case.name.clone());
    if results.insert(key, (status, case.duration_ms)).is_some() {
        return Err(format!("test {suite} {} en double", case.name));
    }
    Ok(())
}

/// Lit un `JUnit` nextest : statut et durée de chaque test, indexés par `(binary-id, nom)`.
///
/// Mêmes règles que [`apply_junit`] (qui s'appuie sur cette fonction) : `failure`/`error` →
/// `Fail`, `flakyFailure`/`rerunFailure` sans échec final → `Flaky`, `skipped` → `Ignored`,
/// sinon `Pass`.
///
/// # Errors
///
/// Document trop grand, XML mal formé ou hors du schéma attendu, balise inconnue, durée
/// illisible, test ou suite en double, `skipped` combiné à un échec.
pub fn read_junit(junit_xml: &str) -> Result<JunitResults, SelectError> {
    if junit_xml.len() > MAX_DOCUMENT {
        return Err(invalid("junit", "document trop grand"));
    }
    parse_junit(junit_xml).map_err(|message| invalid("junit", message))
}

/// Reporte le statut et la durée de chaque test depuis un `JUnit` nextest, puis calcule
/// `green` (aucun `Fail`/`Unknown` hors `Ignored`, au moins un test).
///
/// `testsuite name` = binary-id, `testcase name` = nom du test. `failure`/`error` → `Fail` ;
/// `flakyFailure`/`rerunFailure` sans échec final → `Flaky` ; `skipped` → `Ignored` ; test
/// listé mais absent du `JUnit` → `Unknown` (ou `Ignored` s'il l'était déjà dans la liste).
/// Durée tronquée à la milliseconde. À appliquer une seule fois sur un inventaire frais.
///
/// L'inventaire n'est modifié qu'en cas de succès.
///
/// # Errors
///
/// XML mal formé ou hors du schéma attendu, balise inconnue, durée illisible, test ou
/// suite en double, `skipped` combiné à un échec, binaire ou test du `JUnit` absent de
/// l'inventaire.
pub fn apply_junit(inv: &mut BaseInventory, junit_xml: &str) -> Result<(), SelectError> {
    let what = "junit";
    let mut results = read_junit(junit_xml)?;
    let mut binaries = inv.binaries.clone();
    for binary in &mut binaries {
        for test in &mut binary.tests {
            let key = (binary.binary_id.clone(), test.name.clone());
            if let Some((status, duration)) = results.remove(&key) {
                test.status = status;
                test.duration_ms = duration;
            } else if test.status != BaseStatus::Ignored {
                test.status = BaseStatus::Unknown;
                test.duration_ms = None;
            }
        }
    }
    if let Some(((suite, name), _)) = results.into_iter().next() {
        return Err(invalid(
            what,
            format!("test {suite} {name} absent de l'inventaire"),
        ));
    }
    let mut total = 0_usize;
    let mut green = true;
    for test in binaries.iter().flat_map(|b| &b.tests) {
        total += 1;
        if matches!(test.status, BaseStatus::Fail | BaseStatus::Unknown) {
            green = false;
        }
    }
    inv.binaries = binaries;
    inv.green = green && total > 0;
    Ok(())
}

// ---------------------------------------------------------------------------
// Fichier d'inventaire
// ---------------------------------------------------------------------------

fn validate(inv: &BaseInventory) -> Result<(), SelectError> {
    let what = "base inventory";
    if inv.schema != BASE_SCHEMA {
        return Err(invalid(
            what,
            format!("schéma {:?} (attendu {BASE_SCHEMA})", inv.schema),
        ));
    }
    if !is_commit_hex(&inv.commit) {
        return Err(invalid(
            what,
            "commit : 40 hexadécimaux minuscules attendus",
        ));
    }
    let mut ids = BTreeSet::new();
    for binary in &inv.binaries {
        if !ids.insert(binary.binary_id.as_str()) {
            return Err(invalid(
                what,
                format!("binaire en double : {}", binary.binary_id),
            ));
        }
        validate_rel_path(&binary.src_path)
            .map_err(|message| invalid(what, format!("{} : {message}", binary.binary_id)))?;
        let mut names = BTreeSet::new();
        for test in &binary.tests {
            if !names.insert(test.name.as_str()) {
                return Err(invalid(
                    what,
                    format!("test en double dans {} : {}", binary.binary_id, test.name),
                ));
            }
        }
    }
    Ok(())
}

/// Charge un inventaire JSON et le valide.
///
/// # Errors
///
/// Fichier illisible ou trop grand, JSON invalide, schéma différent de [`BASE_SCHEMA`],
/// commit qui n'est pas 40 hexadécimaux minuscules, binaires ou tests en double, chemin
/// source non relatif.
pub fn load(path: &Path) -> Result<BaseInventory, SelectError> {
    let file = std::fs::File::open(path).map_err(|error| io_error(path, &error))?;
    let mut text = String::new();
    file.take(MAX_DOCUMENT as u64 + 1)
        .read_to_string(&mut text)
        .map_err(|error| io_error(path, &error))?;
    if text.len() > MAX_DOCUMENT {
        return Err(invalid("base inventory", "fichier trop grand"));
    }
    let document: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| invalid("base inventory", error.to_string()))?;
    let inventory = BaseInventory::deserialize(&document)
        .map_err(|error| invalid("base inventory", error.to_string()))?;
    validate(&inventory)?;
    seal::verify(
        "base inventory",
        BASE_INCONSISTENT,
        &document,
        &inventory,
        &base_counts(&inventory),
    )?;
    Ok(inventory)
}

/// Message d'erreur quand le scellé ne correspond pas au contenu (A4).
const BASE_INCONSISTENT: &str = "inventaire de la base incohérent : contenu altéré ou tronqué";

/// Comptes scellés : binaires et tests.
fn base_counts(inv: &BaseInventory) -> [(&'static str, usize); 2] {
    [
        ("binary_count", inv.binaries.len()),
        (
            "test_count",
            inv.binaries.iter().map(|b| b.tests.len()).sum(),
        ),
    ]
}

/// Écrit l'inventaire en JSON (écriture dans un fichier voisin puis renommage).
///
/// # Errors
///
/// Inventaire invalide (mêmes contrôles que [`load`]) ou erreur d'entrée/sortie.
pub fn save(path: &Path, inv: &BaseInventory) -> Result<(), SelectError> {
    validate(inv)?;
    let sealed = seal::seal("base inventory", inv, &base_counts(inv))?;
    let mut text = serde_json::to_string_pretty(&sealed)
        .map_err(|error| invalid("base inventory", error.to_string()))?;
    text.push('\n');
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("base inventory", "nom de fichier illisible"))?;
    let temporary = path.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()));
    std::fs::write(&temporary, text).map_err(|error| io_error(&temporary, &error))?;
    std::fs::rename(&temporary, path).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        io_error(path, &error)
    })
}
