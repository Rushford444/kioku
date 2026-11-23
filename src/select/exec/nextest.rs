//! Lancement de `cargo nextest run` avec un `JUnit` lisible.
//!
//! Le `JUnit` est demandé pour le profil `default` par un fichier de configuration d'outil
//! (`--tool-config-file kioku:<toml>`), avec un chemin absolu dans un dossier temporaire
//! privé : aucun fichier du dépôt n'est touché et aucun ancien `JUnit` ne peut être relu.
//!
//! Pourquoi `default` et pas un profil à nous : nextest exporte `NEXTEST_PROFILE` aux tests,
//! et les tests qui lancent eux-mêmes `cargo nextest` (kioku en a) hériteraient d'un profil
//! introuvable dans leur propre dépôt. Limite : si le dépôt définit lui-même `junit` pour le
//! profil `default`, sa configuration l'emporte, aucun `JUnit` n'est écrit ici et le run est
//! déclaré incomplet (fail closed).
//!
//! La sortie de nextest (stdout et stderr) va sur stderr de kioku, pour que stdout reste
//! réservé au résumé (`--json`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;

use super::reconcile::{ListedMatch, ListedSuite};
use crate::select::SelectError;
use crate::select::base::{command_error, invalid, io_error, run_command};

/// Profil nextest porteur de la sortie `JUnit` (voir le module).
const PROFILE: &str = "default";
/// Taille maximale d'un `JUnit` lu.
const MAX_JUNIT: u64 = 128 * 1024 * 1024;
/// Attente entre `SIGTERM` et `SIGKILL` après un dépassement de délai.
const TERM_GRACE: Duration = Duration::from_secs(10);

/// Code de sortie de nextest : tests lancés, au moins un rouge.
pub(super) const EXIT_TEST_FAILED: i32 = 100;
/// Code de sortie de nextest : échec de compilation.
pub(super) const EXIT_BUILD_FAILED: i32 = 101;

/// Résultat d'un lancement de nextest.
#[derive(Debug)]
pub(super) struct NextestRun {
    /// Code de sortie (`None` : tué par un signal).
    pub exit_code: Option<i32>,
    /// Le délai a été dépassé et nextest a été arrêté.
    pub timed_out: bool,
    /// Contenu du `JUnit`, s'il a été écrit et est du texte UTF-8.
    pub junit: Option<String>,
    /// Durée du lancement.
    pub elapsed: Duration,
}

impl NextestRun {
    /// Fin « normale » de nextest : tests lancés, verts (0) ou avec des rouges (100).
    pub fn ended_normally(&self) -> bool {
        !self.timed_out && matches!(self.exit_code, Some(0 | EXIT_TEST_FAILED))
    }

    /// Échec de compilation : aucun test n'a tourné.
    pub fn build_failed(&self) -> bool {
        !self.timed_out && self.exit_code == Some(EXIT_BUILD_FAILED)
    }

    /// Phrase décrivant une fin anormale (pour le journal et les messages).
    pub fn describe_end(&self) -> String {
        if self.timed_out {
            "délai dépassé, nextest arrêté".to_owned()
        } else {
            match self.exit_code {
                Some(code) => format!("nextest a fini avec le code {code}"),
                None => "nextest tué par un signal".to_owned(),
            }
        }
    }
}

static RUN_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Dossier privé et unique, supprimé à la fin.
struct RunDir(PathBuf);

impl RunDir {
    fn create() -> Result<Self, SelectError> {
        let base = std::env::temp_dir();
        for _ in 0..16 {
            let nonce = RUN_COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos());
            let path = base.join(format!(
                "kioku-select-{}-{nonce}-{nanos}",
                std::process::id()
            ));
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            match builder.create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(io_error(&path, &error)),
            }
        }
        Err(invalid(
            "run dir",
            "impossible de créer un dossier temporaire unique",
        ))
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tool_config(junit_path: &Path) -> Result<String, SelectError> {
    let path = junit_path
        .to_str()
        .ok_or_else(|| invalid("run dir", "chemin temporaire non UTF-8"))?;
    let mut junit = toml::Table::new();
    junit.insert("path".to_owned(), toml::Value::String(path.to_owned()));
    let mut profile = toml::Table::new();
    profile.insert("junit".to_owned(), toml::Value::Table(junit));
    let mut profiles = toml::Table::new();
    profiles.insert(PROFILE.to_owned(), toml::Value::Table(profile));
    let mut root = toml::Table::new();
    root.insert("profile".to_owned(), toml::Value::Table(profiles));
    toml::to_string(&root).map_err(|e| invalid("nextest tool config", e.to_string()))
}

fn term_command(pid: u32) -> Command {
    let mut command = Command::new("/bin/kill");
    command.args(["-TERM", "--", &pid.to_string()]);
    command
}

/// Arrête `child` : `SIGTERM` (nextest arrête ses tests proprement), puis `SIGKILL` après
/// [`TERM_GRACE`]. Ne cible que ce processus (jamais un motif).
fn stop(child: &mut std::process::Child) {
    let _ = term_command(child.id())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let until = Instant::now() + TERM_GRACE;
    while Instant::now() < until {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// `cargo nextest run --locked --no-fail-fast [--features=…] <extra>` dans `repo`, avec
/// `JUnit`, borné par `timeout`.
///
/// # Errors
/// [`SelectError`] si le dossier temporaire ou le processus ne peut pas être créé : le
/// lancement n'a pas eu lieu (l'appelant élargit ou signale l'échec).
pub(super) fn run(
    repo: &Path,
    features: &[String],
    extra: &[String],
    timeout: Duration,
) -> Result<NextestRun, SelectError> {
    let dir = RunDir::create()?;
    let junit_path = dir.0.join("junit.xml");
    let config_path = dir.0.join("nextest-tool.toml");
    std::fs::write(&config_path, tool_config(&junit_path)?)
        .map_err(|e| io_error(&config_path, &e))?;
    let config_arg = format!(
        "kioku:{}",
        config_path
            .to_str()
            .ok_or_else(|| invalid("run dir", "chemin temporaire non UTF-8"))?
    );
    let mut command = Command::new("cargo");
    command
        .args([
            "nextest",
            "run",
            "--locked",
            "--no-fail-fast",
            "--no-tests=pass",
            "--profile",
            PROFILE,
            "--tool-config-file",
            &config_arg,
        ])
        .current_dir(repo)
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::io::stderr()))
        .stderr(Stdio::from(std::io::stderr()));
    if !features.is_empty() {
        command.arg(format!("--features={}", features.join(",")));
    }
    command.args(extra);
    let started = Instant::now();
    let mut child = command
        .spawn()
        .map_err(|e| command_error("cargo nextest run", format!("lancement impossible : {e}")))?;
    let deadline = started + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(error) => {
                stop(&mut child);
                return Err(command_error(
                    "cargo nextest run",
                    format!("attente impossible : {error}"),
                ));
            }
        }
        if Instant::now() >= deadline {
            timed_out = true;
            stop(&mut child);
            break None;
        }
        thread::sleep(Duration::from_millis(20));
    };
    let junit = std::fs::metadata(&junit_path)
        .ok()
        .filter(|meta| meta.is_file() && meta.len() <= MAX_JUNIT)
        .and_then(|_| std::fs::read(&junit_path).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok());
    Ok(NextestRun {
        exit_code: status.and_then(|s| s.code()),
        timed_out,
        junit,
        elapsed: started.elapsed(),
    })
}

// ---------------------------------------------------------------------------------------
// Oracle de complétude : `cargo nextest list`
// ---------------------------------------------------------------------------------------

/// Taille maximale de la liste JSON lue.
const MAX_LIST: usize = 64 * 1024 * 1024;

#[derive(Deserialize)]
struct RawList {
    #[serde(rename = "rust-suites")]
    rust_suites: BTreeMap<String, RawSuite>,
}

#[derive(Deserialize)]
struct RawSuite {
    #[serde(rename = "binary-id")]
    binary_id: String,
    #[serde(rename = "binary-name")]
    binary_name: String,
    kind: String,
    status: String,
    #[serde(default)]
    testcases: BTreeMap<String, RawCase>,
}

#[derive(Deserialize)]
struct RawCase {
    #[serde(rename = "filter-match")]
    filter_match: RawMatch,
}

#[derive(Deserialize)]
struct RawMatch {
    status: String,
    reason: Option<String>,
}

/// Lit la sortie de `cargo nextest list --message-format json` (0.9.x) : un binaire par
/// `binary-id`, chaque test avec son `filter-match`.
///
/// # Errors
/// [`SelectError`] si le JSON est illisible, si une clé diffère de `binary-id`, ou si un
/// `filter-match` a un statut inconnu : l'oracle ne devine jamais.
pub(super) fn parse_list(json: &[u8]) -> Result<BTreeMap<String, ListedSuite>, SelectError> {
    let what = "nextest list (oracle)";
    let raw: RawList =
        serde_json::from_slice(json).map_err(|error| invalid(what, error.to_string()))?;
    let mut suites = BTreeMap::new();
    for (key, suite) in raw.rust_suites {
        if key != suite.binary_id {
            return Err(invalid(
                what,
                format!("clé {key} différente de binary-id {}", suite.binary_id),
            ));
        }
        let mut tests = BTreeMap::new();
        for (name, case) in suite.testcases {
            let listed = match (case.filter_match.status.as_str(), case.filter_match.reason) {
                ("matches", _) => ListedMatch::Matches,
                ("mismatch", Some(reason)) if reason == "ignored" => ListedMatch::Ignored,
                ("mismatch", Some(reason)) if reason == "expression" => ListedMatch::Expression,
                ("mismatch", reason) => ListedMatch::Other(reason.unwrap_or_default()),
                (other, _) => {
                    return Err(invalid(
                        what,
                        format!("test {name} : filter-match {other:?} inconnu"),
                    ));
                }
            };
            tests.insert(name, listed);
        }
        suites.insert(
            key,
            ListedSuite {
                binary_id: suite.binary_id,
                binary_name: suite.binary_name,
                kind: suite.kind,
                listed: suite.status == "listed",
                tests,
            },
        );
    }
    Ok(suites)
}

/// `cargo nextest list --message-format json --locked [--features=…] <extra>` dans `repo` :
/// mêmes cibles, même `-E` et même environnement que le run ; les binaires étant déjà
/// compilés, rien n'est recompilé.
///
/// # Errors
/// [`SelectError`] si la commande échoue, dépasse `timeout` ou produit une liste illisible :
/// l'appelant déclare alors le run incomplet.
pub(super) fn list(
    repo: &Path,
    features: &[String],
    extra: &[String],
    timeout: Duration,
) -> Result<BTreeMap<String, ListedSuite>, SelectError> {
    let mut args: Vec<String> = ["nextest", "list", "--message-format", "json", "--locked"]
        .iter()
        .map(|arg| (*arg).to_owned())
        .collect();
    if !features.is_empty() {
        args.push(format!("--features={}", features.join(",")));
    }
    args.extend(extra.iter().cloned());
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = run_command("cargo", &refs, repo, timeout, MAX_LIST)?;
    parse_list(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn term_command_targets_only_the_child_pid() {
        let command = term_command(12345);
        assert_eq!(command.get_program(), "/bin/kill");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["-TERM", "--", "12345"]
        );
    }

    #[test]
    fn list_json_of_nextest_is_parsed_with_every_filter_match() {
        let json = br#"{"rust-build-meta":{},"test-count":4,"rust-suites":{
          "mini":{"package-name":"mini","binary-id":"mini","binary-name":"mini","kind":"lib",
            "status":"listed","testcases":{
              "t::a":{"kind":"test","ignored":false,"filter-match":{"status":"matches"}},
              "t::i":{"kind":"test","ignored":true,"filter-match":{"status":"mismatch","reason":"ignored"}},
              "t::e":{"kind":"test","ignored":false,"filter-match":{"status":"mismatch","reason":"expression"}},
              "t::o":{"kind":"test","ignored":false,"filter-match":{"status":"mismatch","reason":"default-filter"}}}},
          "mini::it":{"package-name":"mini","binary-id":"mini::it","binary-name":"it","kind":"test",
            "status":"skipped"}}}"#;
        let suites = parse_list(json).unwrap();
        let lib = &suites["mini"];
        assert!(lib.listed && lib.kind == "lib" && lib.binary_name == "mini");
        assert_eq!(lib.tests["t::a"], ListedMatch::Matches);
        assert_eq!(lib.tests["t::i"], ListedMatch::Ignored);
        assert_eq!(lib.tests["t::e"], ListedMatch::Expression);
        assert_eq!(
            lib.tests["t::o"],
            ListedMatch::Other("default-filter".to_owned())
        );
        assert!(!suites["mini::it"].listed && suites["mini::it"].tests.is_empty());
    }

    #[test]
    fn an_unreadable_or_inconsistent_list_is_an_error() {
        assert!(parse_list(b"not json").is_err());
        assert!(parse_list(b"{}").is_err());
        let wrong_key = br#"{"rust-suites":{"a":{"binary-id":"b","binary-name":"b","kind":"lib","status":"listed"}}}"#;
        assert!(parse_list(wrong_key).is_err());
        let unknown_status = br#"{"rust-suites":{"a":{"binary-id":"a","binary-name":"a","kind":"lib","status":"listed",
            "testcases":{"t":{"filter-match":{"status":"weird"}}}}}}"#;
        assert!(parse_list(unknown_status).is_err());
    }

    #[test]
    fn tool_config_is_valid_toml_with_an_escaped_path() {
        let text = tool_config(Path::new("/tmp/a \"b\"/junit.xml")).unwrap();
        let parsed: toml::Table = toml::from_str(&text).unwrap();
        let path = parsed["profile"][PROFILE]["junit"]["path"]
            .as_str()
            .unwrap();
        assert_eq!(path, "/tmp/a \"b\"/junit.xml");
    }

    #[test]
    fn end_classification() {
        let mk = |exit_code, timed_out| NextestRun {
            exit_code,
            timed_out,
            junit: None,
            elapsed: Duration::ZERO,
        };
        assert!(mk(Some(0), false).ended_normally());
        assert!(mk(Some(100), false).ended_normally());
        assert!(!mk(Some(101), false).ended_normally());
        assert!(mk(Some(101), false).build_failed());
        assert!(!mk(Some(4), false).ended_normally());
        assert!(!mk(None, false).ended_normally());
        assert!(
            !mk(Some(0), true).ended_normally(),
            "a timeout is never normal"
        );
        assert!(!mk(Some(101), true).build_failed());
    }
}
