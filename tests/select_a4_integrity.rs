//! Correctif A4 : intégrité du CONTENU du magasin de sélection, de bout en bout (binaire réel).
//!
//! Défaut reproduit par la contre-vérification : dans une copie du magasin, remplacer `edges`
//! par `[]` dans `learned-<commit>.json` en gardant les estampilles (`base_commit`,
//! `graph_digest`) était accepté ; les tests reliés seulement par une arête apprise étaient
//! sautés (`no-path-to-modified`) et, avec `-E`, `kioku test` sortait 0 (`partial-green`) sans
//! rien lancer. Même faiblesse pour les arêtes du graphe.
//!
//! Chaque cas prépare un vrai magasin (`kioku select base` puis `select learn`) sur la
//! mini-crate `tests/fixtures/select/l4exec/minicrate`, vérifie d'abord la sélection PARTIELLE
//! avec le magasin intact (témoin), altère le fichier en gardant les estampilles et le reste du
//! scellé, puis exige : portée `everything`, raison `Error` qui nomme l'incohérence, exit 1 ou 3
//! (3 si la suite complète est verte, 1 si rouge) — jamais 0. Les helpers sont recopiés de
//! `select_l5_faults.rs` (fichier d'un autre lot).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use kioku::select::journal::{self, Journal, JournalScope};
use kioku::select::{Decision, Reason, TestId};
use serde_json::{Value, json};

const TA: (&str, &str) = ("minix::ta", "ta_uses_a");

/// `ta_uses_a` couvre `src/solo.rs`, que le graphe statique ne relie pas : arête apprise.
const COVERAGE: &str =
    r#"{"minix::ta ta_uses_a": {"tests/ta.rs": [1], "src/a.rs": [1], "src/solo.rs": [1]}}"#;

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create dir");
    for entry in fs::read_dir(from).expect("read dir") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            if entry.file_name() != "target" {
                copy_dir(&entry.path(), &target);
            }
        } else {
            fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

/// Environnement propre : rien de l'exécution de nextest qui porte ce test, ni de la CI.
fn clean_env(command: &mut Command) -> &mut Command {
    for (name, _) in std::env::vars_os() {
        let name = name.to_string_lossy().into_owned();
        if name.starts_with("NEXTEST")
            || name.starts_with("CARGO_PKG_")
            || name.starts_with("CARGO_BIN_")
            || matches!(
                name.as_str(),
                "CARGO_MANIFEST_DIR"
                    | "CARGO_TARGET_DIR"
                    | "CARGO_BUILD_TARGET_DIR"
                    | "CARGO_CRATE_NAME"
                    | "CARGO_PRIMARY_PACKAGE"
                    | "CARGO_TARGET_TMPDIR"
                    | "KIOKU_MERGE_REQUEST"
                    | "CI_MERGE_REQUEST_IID"
                    | "GITHUB_EVENT_NAME"
                    | "KIOKU_SELECT_STORE"
                    | "RUSTFLAGS"
                    | "CARGO_ENCODED_RUSTFLAGS"
            )
        {
            command.env_remove(&name);
        }
    }
    command.env("CARGO_NET_OFFLINE", "true")
}

fn git(repo: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    clean_env(&mut command)
        .current_dir(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args);
    let out = command.output().expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
    summary: Value,
    journal: Journal,
}

impl Run {
    fn context(&self) -> String {
        format!(
            "exit {}\nstdout: {}\nstderr: {}\nreasons: {:?}",
            self.code, self.stdout, self.stderr, self.journal.scope_reasons
        )
    }

    fn decision(&self, (binary, name): (&str, &str)) -> Decision {
        self.journal
            .entries
            .iter()
            .find(|e| {
                e.test
                    == TestId {
                        binary_id: binary.to_owned(),
                        name: name.to_owned(),
                    }
            })
            .unwrap_or_else(|| panic!("{binary} {name} absent du journal"))
            .decision
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
    store: PathBuf,
    commit: String,
}

impl Fixture {
    fn kioku(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kioku"));
        clean_env(&mut command)
            .current_dir(&self.path)
            .args(args)
            .arg("--store")
            .arg(&self.store);
        command
    }

    fn append(&self, rel: &str, extra: &str) {
        let path = self.path.join(rel);
        let text = fs::read_to_string(&path).expect("read") + extra;
        fs::write(path, text).expect("write");
    }

    fn base_file(&self) -> PathBuf {
        self.store.join(format!("base-{}.json", self.commit))
    }

    fn learned_file(&self) -> PathBuf {
        self.store.join(format!("learned-{}.json", self.commit))
    }

    fn graph_file(&self) -> PathBuf {
        let base: Value = read_json(&self.base_file());
        let digest = base["graph_digest"].as_str().expect("graph_digest");
        self.store.join(format!("graph-{digest}.json"))
    }

    /// `kioku select base` puis `kioku select learn` : l'état normal d'un dépôt prêt.
    fn prepare_store(&self) {
        let out = self
            .kioku(&["select", "base", "--repo", "."])
            .output()
            .expect("spawn");
        assert_eq!(
            out.status.code(),
            Some(0),
            "select base: {}",
            text(&out.stderr)
        );
        let coverage = self.store.join("coverage.json");
        fs::write(&coverage, COVERAGE).expect("coverage");
        let out = self
            .kioku(&[
                "select",
                "learn",
                "--repo",
                ".",
                "--coverage-json",
                coverage.to_str().expect("utf8"),
            ])
            .output()
            .expect("spawn");
        assert_eq!(
            out.status.code(),
            Some(0),
            "select learn: {}",
            text(&out.stderr)
        );
    }

    fn run(&self, extra: &[&str]) -> Run {
        let mut args = vec!["test", "--repo", ".", "--json"];
        args.extend_from_slice(extra);
        let out = self.kioku(&args).output().expect("spawn kioku");
        let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
        let summary: Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|e| panic!("stdout n'est pas le JSON ({e}): {stdout}\n{stderr}"));
        let path = summary["journal"]
            .as_str()
            .unwrap_or_else(|| panic!("pas de journal: {summary}\n{stderr}"));
        Run {
            code: out.status.code().expect("code de sortie"),
            journal: journal::read(Path::new(path)).expect("journal lisible"),
            stdout,
            stderr,
            summary,
        }
    }
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).expect("read")).expect("json")
}

fn write_json(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec_pretty(value).expect("json")).expect("write");
}

/// Dépôt git (branche `feature` à partir de `main`), base et arêtes apprises enregistrées.
fn ready() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/select/l4exec/minicrate"),
        &repo,
    );
    fs::write(repo.join(".gitignore"), "/target\n").expect("gitignore");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    let fixture = Fixture {
        store: dir.path().join("store"),
        _dir: dir,
        path: repo,
        commit,
    };
    assert!(
        Command::new("rust-analyzer")
            .arg("--version")
            .output()
            .is_ok_and(|out| out.status.success()),
        "rust-analyzer absent du PATH : ces tests exigent le graphe réel"
    );
    fixture.prepare_store();
    git(&fixture.path, &["checkout", "-q", "-b", "feature"]);
    fixture
}

/// Témoin : magasin intact, fichier modifié → sélection partielle (exit 0) où `ta_uses_a` est
/// lancé, grâce à l'arête apprise ou à l'arête du graphe selon le fichier modifié.
fn assert_intact_selection_is_partial(repo: &Fixture, filter: &[&str]) {
    let run = repo.run(filter);
    let ctx = run.context();
    assert_eq!(run.journal.scope, JournalScope::Partial, "{ctx}");
    assert_eq!(
        run.decision(TA),
        Decision::Run,
        "témoin : arête apprise\n{ctx}"
    );
    assert_eq!(run.code, 0, "{ctx}");
}

/// L'altération est refusée : tout, raison `Error` qui nomme `needle`, exit 1 ou 3, jamais 0.
fn assert_refused_everything(run: &Run, needle: &str) {
    let ctx = run.context();
    assert_eq!(run.summary["scope"], "everything", "{ctx}");
    assert_eq!(run.journal.scope, JournalScope::Everything, "{ctx}");
    assert!(
        run.journal
            .scope_reasons
            .iter()
            .any(|r| matches!(r, Reason::Error(m) if m.contains(needle))),
        "raison Error contenant {needle:?} attendue\n{ctx}"
    );
    assert!(
        matches!(run.code, 1 | 3),
        "exit 1 (rouge) ou 3 (incomplet) attendu, jamais 0 : {ctx}"
    );
    assert_ne!(run.summary["status"], "partial-green", "{ctx}");
    assert_eq!(run.decision(TA), Decision::Run, "{ctx}");
}

const LEARNED_MESSAGE: &str = "arêtes apprises incohérentes";
const GRAPH_MESSAGE: &str = "graphe incohérent";

#[test]
fn emptied_learned_edges_with_intact_stamps_run_everything() {
    let repo = ready();
    repo.append("src/solo.rs", "// A4\n");
    assert_intact_selection_is_partial(&repo, &[]);

    let mut learned = read_json(&repo.learned_file());
    assert!(
        learned["edges"].as_array().is_some_and(|e| !e.is_empty()),
        "la fixture doit avoir une arête apprise"
    );
    learned["edges"] = json!([]);
    write_json(&repo.learned_file(), &learned);
    assert_refused_everything(&repo.run(&[]), LEARNED_MESSAGE);
}

#[test]
fn emptied_learned_edges_with_a_user_filter_never_exit_zero() {
    let repo = ready();
    repo.append("src/solo.rs", "// A4\n");
    let filter = ["-E", "binary(=ta)"];
    assert_intact_selection_is_partial(&repo, &filter);

    let mut learned = read_json(&repo.learned_file());
    learned["edges"] = json!([]);
    write_json(&repo.learned_file(), &learned);
    assert_refused_everything(&repo.run(&filter), LEARNED_MESSAGE);
}

#[test]
fn a_learned_edge_removed_from_the_graph_file_runs_everything() {
    let repo = ready();
    repo.append("src/a.rs", "// A4\n");
    let filter = ["-E", "binary(=ta)"];
    assert_intact_selection_is_partial(&repo, &filter);

    // Le graphe réel relie `tests/ta.rs` à `src/a.rs` ; on vide toutes les arêtes, scellé gardé.
    let mut graph = read_json(&repo.graph_file());
    assert!(
        graph["edges"].as_object().is_some_and(|e| !e.is_empty()),
        "la fixture doit avoir des arêtes de graphe"
    );
    graph["edges"] = json!({});
    write_json(&repo.graph_file(), &graph);
    assert_refused_everything(&repo.run(&filter), GRAPH_MESSAGE);
    assert_refused_everything(&repo.run(&[]), GRAPH_MESSAGE);
}

#[test]
fn a_graph_root_removed_keeps_the_seal_and_is_refused() {
    let repo = ready();
    repo.append("src/a.rs", "// A4\n");
    let mut graph = read_json(&repo.graph_file());
    let roots = graph["test_roots"].as_array_mut().expect("test_roots");
    assert!(
        roots.len() > 1,
        "au moins deux tests dans le graphe : {roots:?}"
    );
    roots.pop();
    write_json(&repo.graph_file(), &graph);
    assert_refused_everything(&repo.run(&[]), GRAPH_MESSAGE);
}

#[test]
fn a_base_inventory_with_a_test_removed_runs_everything() {
    let repo = ready();
    repo.append("src/a.rs", "// A4\n");
    let mut base = read_json(&repo.base_file());
    let tests = base["binaries"][0]["tests"].as_array_mut().expect("tests");
    tests.pop();
    write_json(&repo.base_file(), &base);
    let run = repo.run(&[]);
    // Le journal n'a plus l'inventaire : on ne vérifie que la portée, la raison, le code.
    let ctx = run.context();
    assert_eq!(run.summary["scope"], "everything", "{ctx}");
    assert!(
        run.journal.scope_reasons.iter().any(
            |r| matches!(r, Reason::Error(m) if m.contains("inventaire de la base incohérent"))
        ),
        "{ctx}"
    );
    assert!(matches!(run.code, 1 | 3), "{ctx}");
}
