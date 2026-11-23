//! Lot L5 : injection de fautes de `kioku test`, de bout en bout (binaire réel).
//!
//! Chaque cas copie la mini-crate `tests/fixtures/select/l4exec/minicrate` (lib `minix` :
//! `a`, `b`, `solo` ; binaires d'intégration `ta`, `tb`), y ajoute un fichier non Rust cité
//! (`src/data.txt`, par `include_str!` dans `b.rs`) et un module sans test (`src/helper.rs`),
//! fait `git init` (branche `main`) et appelle `kioku`. La base est enregistrée comme en
//! production : `kioku select base` puis `kioku select learn --coverage-json` d'un JSON écrit
//! par le test. Les helpers sont recopiés de `select_l4_exec.rs` (fichier d'un autre lot).
//!
//! * **S3 (erreur → tout)** : un test par cause ; portée `everything`, raison `Error`,
//!   suite complète lancée, **exit 3** (`incomplete`), jamais 0 ni 5.
//! * **S2 (doute → élargir)** : un test par ligne du § 4 ; une raison de doute (`BuildInput`,
//!   `Macro`, `BuildEnvironment`…) n'est pas une erreur : la suite complète qui passe est un
//!   vrai vert complet, **exit 5**. Les élargissements partiels restent à l'exit 0.
//! * **S1**, **S8** : `main`, demande de merge, jamais de `full-green` partiel ou filtré.
//!
//! Le test `#[ignore]` final rejoue `kioku test` sur `main` dans un clone jetable de kioku.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use kioku::select::journal::{self, Journal, JournalScope, JournalStatus, Outcome};
use kioku::select::{BaseStatus, Decision, GraphData, Reason, TestId, base, classify, graph};
use serde_json::Value;

// ---------------------------------------------------------------------------------------
// Harnais
// ---------------------------------------------------------------------------------------

/// Les 5 tests que nextest lance (le 6e, `b_ignored`, est `#[ignore]`).
const RUNNABLE: [(&str, &str); 5] = [
    ("minix", "a::tests::a_unit"),
    ("minix", "b::tests::b_unit"),
    ("minix", "solo::tests::solo_unit"),
    ("minix::ta", "ta_uses_a"),
    ("minix::tb", "tb_uses_b"),
];
const IGNORED: (&str, &str) = ("minix", "b::tests::b_ignored");

fn fixture_crate() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/select/l4exec/minicrate")
}

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
                    | "CARGO_CRATE_NAME"
                    | "CARGO_PRIMARY_PACKAGE"
                    | "CARGO_TARGET_TMPDIR"
                    | "CARGO_TARGET_DIR"
                    | "CARGO_BUILD_TARGET_DIR"
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

fn tid(binary: &str, name: &str) -> TestId {
    TestId {
        binary_id: binary.to_owned(),
        name: name.to_owned(),
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn rust_analyzer_present() -> bool {
    Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
}

/// `JUnit` d'un run de la base : tout vert, sauf `a_unit` selon `a_unit`.
fn junit(a_unit: &str) -> String {
    let mut xml = String::from("<?xml version=\"1.0\"?>\n<testsuites>\n");
    for binary in ["minix", "minix::ta", "minix::tb"] {
        writeln!(xml, "<testsuite name=\"{binary}\">").expect("write");
        for (id, name) in RUNNABLE {
            if id != binary {
                continue;
            }
            if name == "a::tests::a_unit" && !a_unit.is_empty() {
                writeln!(
                    xml,
                    "<testcase name=\"{name}\"><{a_unit} message=\"x\"/></testcase>"
                )
                .expect("write");
            } else {
                writeln!(xml, "<testcase name=\"{name}\" time=\"0.001\"/>").expect("write");
            }
        }
        xml.push_str("</testsuite>\n");
    }
    xml.push_str("</testsuites>\n");
    xml
}

const COVERAGE: &str =
    r#"{"minix::ta ta_uses_a": {"tests/ta.rs": [1], "src/a.rs": [1], "src/solo.rs": [1]}}"#;

/// Résultat d'un `kioku test --json`.
struct Run {
    code: i32,
    stdout: String,
    stderr: String,
    summary: Value,
    journal: Journal,
}

impl Run {
    fn status(&self) -> &str {
        self.summary["status"].as_str().expect("status")
    }

    fn entry(&self, binary: &str, name: &str) -> &kioku::select::JournalEntry {
        self.journal
            .entries
            .iter()
            .find(|e| e.test == tid(binary, name))
            .unwrap_or_else(|| panic!("{binary} {name} absent du journal"))
    }

    fn outcome(&self, binary: &str, name: &str) -> Option<Outcome> {
        self.journal
            .outcomes
            .iter()
            .find(|o| o.test == tid(binary, name))
            .map(|o| o.outcome)
    }

    fn skipped(&self) -> BTreeSet<String> {
        self.journal
            .entries
            .iter()
            .filter(|e| e.decision == Decision::Skip)
            .map(|e| format!("{} {}", e.test.binary_id, e.test.name))
            .collect()
    }

    fn context(&self) -> String {
        format!(
            "exit {}\nstdout: {}\nstderr: {}\nreasons: {:?}",
            self.code, self.stdout, self.stderr, self.journal.scope_reasons
        )
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

    fn write(&self, rel: &str, content: &str) {
        let path = self.path.join(rel);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, content).expect("write");
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.path.join(rel)).expect("read")
    }

    fn append(&self, rel: &str, extra: &str) {
        let text = self.read(rel) + extra;
        self.write(rel, &text);
    }

    fn replace(&self, rel: &str, from: &str, to: &str) {
        let text = self.read(rel);
        assert!(text.contains(from), "{rel} ne contient pas {from:?}");
        self.write(rel, &text.replacen(from, to, 1));
    }

    fn feature_branch(&self) {
        git(&self.path, &["checkout", "-q", "-b", "feature"]);
    }

    fn base_file(&self) -> PathBuf {
        self.store.join(format!("base-{}.json", self.commit))
    }

    fn learned_file(&self) -> PathBuf {
        self.store.join(format!("learned-{}.json", self.commit))
    }

    fn graph_file(&self) -> PathBuf {
        let inventory = base::load(&self.base_file()).expect("base lisible");
        let digest = inventory.graph_digest.expect("digest");
        self.store.join(format!("graph-{digest}.json"))
    }

    fn command_output(command: &mut Command) -> Output {
        command.output().expect("spawn kioku")
    }

    /// `kioku select base` (`JUnit` fourni si `junit_file`) puis `kioku select learn`.
    fn prepare_store(&self, junit_file: Option<&Path>) {
        assert!(
            rust_analyzer_present(),
            "rust-analyzer absent du PATH : ces tests exigent le graphe réel"
        );
        let mut args = vec!["select", "base", "--repo", "."];
        let junit_arg = junit_file.map(|p| p.to_str().expect("utf8").to_owned());
        if let Some(path) = &junit_arg {
            args.extend(["--junit", path]);
        }
        let out = Self::command_output(&mut self.kioku(&args));
        assert_eq!(
            out.status.code(),
            Some(0),
            "select base: {}",
            text(&out.stderr)
        );
        let coverage = self.store.join("coverage.json");
        fs::write(&coverage, COVERAGE).expect("coverage");
        let out = Self::command_output(&mut self.kioku(&[
            "select",
            "learn",
            "--repo",
            ".",
            "--coverage-json",
            coverage.to_str().expect("utf8"),
        ]));
        assert_eq!(
            out.status.code(),
            Some(0),
            "select learn: {}",
            text(&out.stderr)
        );
        assert!(self.learned_file().is_file());
    }

    /// `kioku test --repo . --json` + arguments, avec réglage de la commande.
    fn run_with(&self, extra: &[&str], tune: impl FnOnce(&mut Command)) -> Run {
        let mut args = vec!["test", "--repo", ".", "--json"];
        args.extend_from_slice(extra);
        let mut command = self.kioku(&args);
        tune(&mut command);
        let out = Self::command_output(&mut command);
        let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
        let code = out.status.code().expect("code de sortie");
        let summary: Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|e| panic!("stdout n'est pas le JSON ({e}): {stdout}\n{stderr}"));
        let path = summary["journal"]
            .as_str()
            .unwrap_or_else(|| panic!("pas de journal: {summary}\n{stderr}"));
        let journal = journal::read(Path::new(path)).expect("journal lisible");
        Run {
            code,
            stdout,
            stderr,
            summary,
            journal,
        }
    }

    fn run(&self, extra: &[&str]) -> Run {
        self.run_with(extra, |_| {})
    }
}

/// Dépôt git sur `main`, sans magasin.
fn bare_repo() -> Fixture {
    bare_repo_with(|_| {})
}

/// Comme `bare_repo`, après `setup(dépôt)` et avant le premier commit.
fn bare_repo_with(setup: impl FnOnce(&Path)) -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    copy_dir(&fixture_crate(), &repo);
    fs::write(repo.join(".gitignore"), "/target\n").expect("gitignore");
    fs::write(repo.join("src/data.txt"), "donnée citée\n").expect("data");
    fs::write(
        repo.join("src/helper.rs"),
        "#[must_use]\npub fn helper() -> u32 {\n    5\n}\n",
    )
    .expect("helper");
    let b = fs::read_to_string(repo.join("src/b.rs")).expect("b.rs");
    fs::write(
        repo.join("src/b.rs"),
        format!("#[must_use]\npub fn data() -> &'static str {{\n    include_str!(\"data.txt\")\n}}\n\n{b}"),
    )
    .expect("b.rs");
    let lib = fs::read_to_string(repo.join("src/lib.rs")).expect("lib.rs");
    fs::write(repo.join("src/lib.rs"), lib + "pub mod helper;\n").expect("lib.rs");
    setup(&repo);
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    Fixture {
        store: dir.path().join("store"),
        _dir: dir,
        path: repo,
        commit,
    }
}

/// Base enregistrée (vert), arêtes apprises : l'état normal d'un dépôt prêt pour `kioku test`.
fn ready() -> Fixture {
    let fixture = bare_repo();
    fixture.prepare_store(None);
    fixture
}

/// Comme `ready`, après `setup(dépôt)` (fichiers ajoutés à la fixture avant la base).
fn ready_with(setup: impl FnOnce(&Path)) -> Fixture {
    let fixture = bare_repo_with(setup);
    fixture.prepare_store(None);
    fixture
}

/// Base dont le run de `a_unit` est `failure` (rouge) ou `flakyFailure` (instable).
fn ready_with_a_unit(kind: &str) -> Fixture {
    let fixture = bare_repo();
    fs::create_dir_all(&fixture.store).expect("store");
    let xml = fixture.store.join("junit.xml");
    fs::write(&xml, junit(kind)).expect("junit");
    fixture.prepare_store(Some(&xml));
    fixture
}

fn fake_cargo_dir(fixture: &Fixture, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let real = text(
        &Command::new("which")
            .arg("cargo")
            .output()
            .expect("which")
            .stdout,
    )
    .trim()
    .to_owned();
    assert!(!real.is_empty(), "cargo introuvable");
    let bin = fixture.store.join("fakebin");
    fs::create_dir_all(&bin).expect("fakebin");
    let script = format!("#!/bin/sh\n{body}\nexec {real} \"$@\"\n");
    fs::write(bin.join("cargo"), script).expect("script");
    fs::set_permissions(bin.join("cargo"), fs::Permissions::from_mode(0o755)).expect("chmod");
    format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

// ---------------------------------------------------------------------------------------
// Assertions communes
// ---------------------------------------------------------------------------------------

/// La suite complète a tourné : rien de sauté, les 5 tests lancés par nextest ont passé.
fn assert_full_suite_ran(run: &Run) {
    let ctx = run.context();
    assert_eq!(run.journal.scope, JournalScope::Everything, "{ctx}");
    assert_eq!(run.journal.counts.skipped, 0, "{ctx}");
    assert!(
        run.journal
            .entries
            .iter()
            .all(|e| e.decision == Decision::Run),
        "{ctx}"
    );
    let passed = run
        .journal
        .outcomes
        .iter()
        .filter(|o| o.outcome == Outcome::Passed)
        .count();
    assert_eq!(passed, RUNNABLE.len(), "{ctx}");
    assert!(
        run.journal
            .outcomes
            .iter()
            .all(|o| matches!(o.outcome, Outcome::Passed | Outcome::Ignored)),
        "{ctx}"
    );
}

/// S3 : erreur de sélection → suite complète lancée, exit 3, jamais un vert.
fn assert_s3(run: &Run, needle: &str) {
    let ctx = run.context();
    assert_eq!(run.code, 3, "{ctx}");
    assert_eq!(run.status(), "incomplete", "{ctx}");
    assert_eq!(run.summary["exit_code"], 3, "{ctx}");
    assert_eq!(run.summary["scope"], "everything", "{ctx}");
    assert_ne!(run.journal.status, JournalStatus::FullGreen);
    assert_ne!(run.journal.status, JournalStatus::PartialGreen);
    assert_full_suite_ran(run);
    assert!(
        run.journal
            .scope_reasons
            .iter()
            .any(|r| matches!(r, Reason::Error(m) if m.contains(needle))),
        "raison Error contenant {needle:?} attendue\n{ctx}"
    );
    let detail = run.summary["detail"].as_str().unwrap_or_default();
    assert!(!detail.is_empty(), "un run incomplet dit pourquoi\n{ctx}");
}

/// S2 (doute) : portée `everything`, raison de doute, vrai vert complet (exit 5).
fn assert_s2_everything(run: &Run, doubt: impl Fn(&Reason) -> bool) {
    let ctx = run.context();
    assert_eq!(run.code, 5, "{ctx}");
    assert_eq!(run.status(), "full-green", "{ctx}");
    assert_full_suite_ran(run);
    assert!(run.journal.scope_reasons.iter().any(doubt), "{ctx}");
    assert!(
        run.journal
            .scope_reasons
            .iter()
            .all(|r| !matches!(r, Reason::Error(_))),
        "un doute S2 n'est pas une erreur\n{ctx}"
    );
}

/// Élargissement partiel : vert partiel, exit 0, jamais `full-green`.
fn assert_partial(run: &Run) {
    let ctx = run.context();
    assert_eq!(run.code, 0, "{ctx}");
    assert_eq!(run.status(), "partial-green", "{ctx}");
    assert_eq!(run.journal.scope, JournalScope::Partial, "{ctx}");
    assert!(run.journal.counts.skipped > 0, "{ctx}");
}

fn names(items: &[(&str, &str)]) -> BTreeSet<String> {
    items.iter().map(|(b, n)| format!("{b} {n}")).collect()
}

// ---------------------------------------------------------------------------------------
// S3 : erreur → tout (un test par cause)
// ---------------------------------------------------------------------------------------

#[test]
fn s3_empty_store() {
    let repo = bare_repo();
    repo.feature_branch();
    repo.append("src/b.rs", "// pas de base\n");
    assert!(!repo.store.exists());
    let run = repo.run(&[]);
    assert_s3(&run, "base-<commit>.json");
    assert_eq!(
        run.journal.counts.inventory, 0,
        "sans base tout est nouveau"
    );
}

#[test]
fn s3_corrupt_base_inventory() {
    let repo = ready();
    repo.feature_branch();
    repo.append("src/b.rs", "// base corrompue\n");
    fs::write(repo.base_file(), b"{ pas du json").expect("corrupt");
    assert_s3(&repo.run(&[]), "base inventory");
}

#[test]
fn s3_graph_file_missing() {
    let repo = ready();
    repo.feature_branch();
    repo.append("src/b.rs", "// graphe absent\n");
    fs::remove_file(repo.graph_file()).expect("remove");
    let run = repo.run(&[]);
    assert_s3(&run, "graph");
    assert_eq!(run.journal.counts.inventory, 6, "la base reste lisible");
}

#[test]
fn s3_corrupt_graph_file() {
    let repo = ready();
    repo.feature_branch();
    repo.append("src/b.rs", "// graphe corrompu\n");
    fs::write(repo.graph_file(), b"{ pas du json").expect("corrupt");
    let run = repo.run(&[]);
    assert_s3(&run, "graph");
    assert_eq!(run.journal.counts.inventory, 6, "la base reste lisible");
}

#[test]
fn s3_graph_digest_differs() {
    let repo = ready();
    repo.feature_branch();
    repo.append("src/b.rs", "// digest\n");
    let file = repo.graph_file();
    let mut data: GraphData =
        graph::from_json(&fs::read(&file).expect("graph")).expect("graph valide");
    data.digest = "digest-d-un-autre-graphe".to_owned();
    fs::write(&file, graph::to_json(&data).expect("json")).expect("swap");
    let run = repo.run(&[]);
    assert_s3(&run, "digest");
    assert_eq!(run.journal.counts.inventory, 6);
}

#[test]
fn s3_learned_edges_missing() {
    let repo = ready();
    repo.feature_branch();
    repo.append("src/b.rs", "// appris absent\n");
    fs::remove_file(repo.learned_file()).expect("remove");
    let run = repo.run(&[]);
    assert_s3(&run, "learned-<commit>.json");
    assert_eq!(
        run.journal.counts.inventory, 0,
        "la base n'est plus candidate"
    );
}

#[test]
fn s3_base_not_green() {
    let repo = ready_with_a_unit("failure");
    repo.feature_branch();
    repo.append("src/b.rs", "// base rouge\n");
    let inventory = base::load(&repo.base_file()).expect("base");
    assert!(!inventory.green, "la base enregistrée est rouge");
    let run = repo.run(&[]);
    assert_s3(&run, "non verte");
}

#[test]
fn s3_base_not_found() {
    let repo = ready();
    repo.feature_branch();
    repo.append("src/b.rs", "// base inconnue\n");
    let run = repo.run(&["--base", "branche-qui-n-existe-pas"]);
    assert_s3(&run, "base");
}

#[test]
fn s3_not_a_git_repository() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    copy_dir(&fixture_crate(), &repo);
    let mut command = Command::new(env!("CARGO_BIN_EXE_kioku"));
    clean_env(&mut command)
        .current_dir(&repo)
        .args(["test", "--repo", ".", "--json", "--store"])
        .arg(dir.path().join("store"));
    let out = command.output().expect("spawn");
    let stdout = text(&out.stdout);
    let summary: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("{e}: {stdout}{}", text(&out.stderr)));
    assert_eq!(out.status.code(), Some(3), "{stdout}{}", text(&out.stderr));
    assert_eq!(summary["status"], "incomplete");
    assert_eq!(summary["scope"], "everything");
    let journal = journal::read(Path::new(summary["journal"].as_str().expect("journal")))
        .expect("journal lisible");
    assert!(
        journal
            .scope_reasons
            .iter()
            .any(|r| matches!(r, Reason::Error(_))),
        "{:?}",
        journal.scope_reasons
    );
    assert_eq!(journal.counts.skipped, 0);
    assert_eq!(
        journal
            .outcomes
            .iter()
            .filter(|o| o.outcome == Outcome::Passed)
            .count(),
        RUNNABLE.len(),
        "la suite complète a tourné"
    );
}

/// Un doute S2, pas une erreur : toolchain, features et variables de build différentes
/// élargissent à tout ; la suite complète qui passe est un vert complet (exit 5).
#[test]
fn s2_doubt_different_toolchain_runs_everything() {
    let repo = ready();
    repo.feature_branch();
    repo.append("src/b.rs", "// toolchain\n");
    let path = fake_cargo_dir(
        &repo,
        "if [ \"$1\" = \"-V\" ]; then echo 'cargo 0.0.0-l5-fake'; exit 0; fi",
    );
    let run = repo.run_with(&[], |c| {
        c.env("PATH", path);
    });
    assert_s2_everything(
        &run,
        |r| matches!(r, Reason::BuildEnvironment(m) if m.contains("toolchain cargo")),
    );
}

#[test]
fn s2_doubt_different_features_runs_everything() {
    let repo = ready();
    repo.feature_branch();
    repo.append("src/b.rs", "// features\n");
    let run = repo.run(&["--features", "extra"]);
    assert_s2_everything(
        &run,
        |r| matches!(r, Reason::BuildEnvironment(m) if m.contains("features")),
    );
}

#[test]
fn s2_doubt_different_build_variable_runs_everything() {
    let repo = ready();
    repo.feature_branch();
    repo.append("src/b.rs", "// rustflags\n");
    let run = repo.run_with(&[], |c| {
        c.env("RUSTFLAGS", "-C opt-level=1");
    });
    assert_s2_everything(
        &run,
        |r| matches!(r, Reason::BuildEnvironment(m) if m.contains("RUSTFLAGS")),
    );
    let journal_text = fs::read_to_string(run.summary["journal"].as_str().expect("journal"))
        .expect("journal lisible");
    assert!(
        !run.stdout.contains("opt-level") && !journal_text.contains("opt-level"),
        "la valeur de la variable n'est jamais écrite"
    );
}

#[cfg(unix)]
#[test]
fn s3_minimal_timeout_is_incomplete() {
    let repo = ready();
    repo.feature_branch();
    repo.append("src/b.rs", "// délai minimal\n");
    // Faux `cargo` : tout `nextest run` dort (le reste passe au vrai).
    let path = fake_cargo_dir(
        &repo,
        "if [ \"$1\" = nextest ] && [ \"$2\" = run ]; then exec sleep 120; fi",
    );
    let started = std::time::Instant::now();
    let run = repo.run_with(&["--timeout", "1"], |c| {
        c.env("PATH", path);
    });
    assert!(
        started.elapsed() < std::time::Duration::from_secs(60),
        "le délai borne le run"
    );
    let ctx = run.context();
    assert_eq!(run.code, 3, "{ctx}");
    assert_eq!(run.status(), "incomplete", "{ctx}");
    assert_eq!(
        run.summary["scope"], "everything",
        "la suite complète a été tentée\n{ctx}"
    );
    assert!(
        run.journal
            .outcomes
            .iter()
            .all(|o| matches!(o.outcome, Outcome::NotRun | Outcome::Ignored)),
        "{ctx}"
    );
    assert!(
        run.journal
            .scope_reasons
            .iter()
            .any(|r| matches!(r, Reason::Error(m) if m.contains("délai") || m.contains("timeout"))),
        "{ctx}"
    );
}

/// Le run partiel dépasse le délai, la suite complète tourne ensuite pour de vrai.
#[cfg(unix)]
#[test]
fn s3_timeout_of_the_partial_run_falls_back_to_the_full_suite() {
    let repo = ready();
    repo.feature_branch();
    repo.append("src/b.rs", "// délai puis suite complète\n");
    let flag = repo.store.join("slept");
    fs::create_dir_all(&repo.store).expect("store");
    let path = fake_cargo_dir(
        &repo,
        &format!(
            "if [ \"$1\" = nextest ] && [ \"$2\" = run ] && [ ! -e '{0}' ]; then : > '{0}'; exec sleep 120; fi",
            flag.display()
        ),
    );
    let run = repo.run_with(&["--timeout", "8"], |c| {
        c.env("PATH", path);
    });
    assert!(flag.exists(), "le premier lancement a bien été bloqué");
    assert_s3(&run, "délai");
}

// ---------------------------------------------------------------------------------------
// S2 : doute → élargir (une ligne du § 4 par test)
// ---------------------------------------------------------------------------------------

fn build_input(path: &'static str) -> impl Fn(&Reason) -> bool {
    move |r| matches!(r, Reason::BuildInput(p) if p == path)
}

#[test]
fn s2_cargo_toml() {
    let repo = ready();
    repo.feature_branch();
    repo.append("Cargo.toml", "\n# l5\n");
    assert_s2_everything(&repo.run(&[]), build_input("Cargo.toml"));
}

#[test]
fn s2_cargo_lock() {
    let repo = ready();
    repo.feature_branch();
    repo.append("Cargo.lock", "\n# l5\n");
    assert_s2_everything(&repo.run(&[]), build_input("Cargo.lock"));
}

#[test]
fn s2_build_rs() {
    let repo = ready();
    repo.feature_branch();
    repo.write("build.rs", "fn main() {}\n");
    assert_s2_everything(&repo.run(&[]), build_input("build.rs"));
}

#[test]
fn s2_cargo_config() {
    let repo = ready();
    repo.feature_branch();
    repo.write(".cargo/config.toml", "# l5\n");
    assert_s2_everything(&repo.run(&[]), build_input(".cargo/config.toml"));
}

#[test]
fn s2_rust_toolchain() {
    let repo = ready();
    repo.feature_branch();
    repo.write("rust-toolchain.toml", "[toolchain]\nchannel = \"stable\"\n");
    assert_s2_everything(&repo.run(&[]), build_input("rust-toolchain.toml"));
}

#[test]
fn s2_nextest_config() {
    let repo = ready();
    repo.feature_branch();
    repo.write(".config/nextest.toml", "# l5\n");
    assert_s2_everything(&repo.run(&[]), build_input(".config/nextest.toml"));
}

#[test]
fn s2_deny_toml() {
    let repo = ready();
    repo.feature_branch();
    repo.write("deny.toml", "# l5\n");
    assert_s2_everything(&repo.run(&[]), build_input("deny.toml"));
}

#[test]
fn s2_unreferenced_non_rust_file_under_src() {
    let repo = ready();
    repo.feature_branch();
    repo.write("src/orphan.txt", "personne ne me cite\n");
    assert_s2_everything(
        &repo.run(&[]),
        |r| matches!(r, Reason::UnreferencedNonRust(p) if p == "src/orphan.txt"),
    );
}

#[test]
fn s2_referenced_non_rust_file_reruns_only_its_citers() {
    let repo = ready();
    repo.feature_branch();
    repo.write("src/data.txt", "donnée modifiée\n");
    let run = repo.run(&[]);
    assert_partial(&run);
    // Le fichier cité est remplacé par ses citants : `b.rs` est « modifié ».
    assert_eq!(run.journal.modified_files, ["src/b.rs"]);
    // `b.rs` cite `data.txt` : les tests qui atteignent `b.rs` sont relancés, eux seuls.
    for (binary, name) in [("minix", "b::tests::b_unit"), ("minix::tb", "tb_uses_b")] {
        let entry = run.entry(binary, name);
        assert_eq!(entry.decision, Decision::Run, "{binary} {name}");
        assert!(
            entry
                .reasons
                .iter()
                .any(|r| matches!(r, Reason::ReachesModified { .. })),
            "{:?}",
            entry.reasons
        );
        assert_eq!(run.outcome(binary, name), Some(Outcome::Passed));
    }
    assert_eq!(
        run.skipped(),
        names(&[
            ("minix", "a::tests::a_unit"),
            ("minix", "solo::tests::solo_unit"),
            ("minix::ta", "ta_uses_a"),
        ])
    );
}

#[test]
fn s2_new_test_file_runs_its_whole_binary() {
    let repo = ready();
    repo.feature_branch();
    repo.write(
        "tests/tc.rs",
        "#[test]\nfn tc_one() {}\n\n#[test]\nfn tc_two() {}\n",
    );
    let run = repo.run(&[]);
    assert_partial(&run);
    for name in ["tc_one", "tc_two"] {
        let entry = run.entry("minix::tc", name);
        assert_eq!(entry.decision, Decision::Run, "{name}");
        assert!(
            entry
                .reasons
                .iter()
                .any(|r| matches!(r, Reason::NewBinary(b) if b == "minix::tc")),
            "{:?}",
            entry.reasons
        );
        assert_eq!(run.outcome("minix::tc", name), Some(Outcome::Passed));
    }
    assert_eq!(run.journal.counts.new, 2);
}

/// Depuis la règle du fichier orphelin (L5b), `helper.rs` (aucun test ne l'atteint) et `lib.rs`
/// (racine de crate, sans appelant dans cette mini-crate) sont orphelins : la suppression ou le
/// renommage élargit à « tout » (jamais un vert partiel), au lieu du binaire entier + atteignants.
fn assert_removed_helper_runs_everything(run: &Run) {
    let ctx = run.context();
    assert_eq!(run.code, 5, "{ctx}");
    assert_eq!(run.status(), "full-green", "{ctx}");
    assert_full_suite_ran(run);
    for orphan in ["src/helper.rs", "src/lib.rs"] {
        assert!(
            run.journal
                .scope_reasons
                .iter()
                .any(|r| matches!(r, Reason::UnknownRustFile(p) if p == orphan)),
            "{orphan} orphelin attendu\n{ctx}"
        );
    }
}

#[test]
fn s2_deleted_file_runs_everything() {
    let repo = ready();
    repo.feature_branch();
    fs::remove_file(repo.path.join("src/helper.rs")).expect("remove");
    repo.replace("src/lib.rs", "pub mod helper;\n", "");
    assert_removed_helper_runs_everything(&repo.run(&[]));
}

#[test]
fn s2_renamed_file_runs_everything() {
    let repo = ready();
    repo.feature_branch();
    git(&repo.path, &["mv", "src/helper.rs", "src/helper2.rs"]);
    repo.replace(
        "src/lib.rs",
        "pub mod helper;\n",
        "#[path = \"helper2.rs\"]\npub mod helper;\n",
    );
    git(&repo.path, &["add", "-A"]);
    assert_removed_helper_runs_everything(&repo.run(&[]));
}

#[test]
fn s2_macro_runs_everything() {
    let repo = ready();
    repo.feature_branch();
    repo.append(
        "src/solo.rs",
        "\n#[allow(unused_macros)]\n#[macro_export]\nmacro_rules! l5_macro {\n    () => {};\n}\n",
    );
    assert_s2_everything(
        &repo.run(&[]),
        |r| matches!(r, Reason::Macro(p) if p == "src/solo.rs"),
    );
}

/// Un test `#[ignore]` de la base est toujours lancé (journalisé `ignored`, pas sauté).
#[test]
fn s2_ignored_test_of_the_base_is_always_run() {
    let repo = ready();
    repo.feature_branch();
    repo.append("src/a.rs", "// sans lien avec b_ignored\n");
    let run = repo.run(&[]);
    assert_partial(&run);
    let entry = run.entry(IGNORED.0, IGNORED.1);
    assert_eq!(entry.decision, Decision::Run);
    assert!(
        entry
            .reasons
            .iter()
            .any(|r| matches!(r, Reason::BaseNotGreen(BaseStatus::Ignored))),
        "{:?}",
        entry.reasons
    );
    assert_eq!(run.outcome(IGNORED.0, IGNORED.1), Some(Outcome::Ignored));
}

/// Un test instable de la base est toujours lancé, même sans lien avec la modification.
#[test]
fn s2_flaky_test_of_the_base_is_always_run() {
    let repo = ready_with_a_unit("flakyFailure");
    repo.feature_branch();
    repo.append("src/b.rs", "// sans lien avec a_unit\n");
    let run = repo.run(&[]);
    assert_partial(&run);
    let entry = run.entry("minix", "a::tests::a_unit");
    assert_eq!(entry.decision, Decision::Run);
    assert!(
        entry
            .reasons
            .iter()
            .any(|r| matches!(r, Reason::BaseNotGreen(BaseStatus::Flaky))),
        "{:?}",
        entry.reasons
    );
    assert_eq!(
        run.outcome("minix", "a::tests::a_unit"),
        Some(Outcome::Passed)
    );
}

// ---------------------------------------------------------------------------------------
// S1 : jamais de sélection sur `main` ni sur une demande de merge
// ---------------------------------------------------------------------------------------

#[test]
fn s1_main_branch_runs_everything_exit_5() {
    let repo = ready();
    repo.append("src/b.rs", "// sur main\n");
    let run = repo.run(&[]);
    assert_eq!(run.code, 5, "{}", run.context());
    assert_eq!(run.status(), "full-green");
    assert_full_suite_ran(&run);
    assert!(
        run.journal
            .scope_reasons
            .iter()
            .any(|r| matches!(r, Reason::ProtectedBranch(b) if b == "main")),
        "{}",
        run.context()
    );
    assert_eq!(
        run.journal.counts.run, 6,
        "tout l'inventaire de la base est lancé"
    );
}

#[test]
fn s1_merge_request_runs_everything() {
    let repo = ready();
    repo.feature_branch();
    repo.append("src/b.rs", "// demande de merge\n");
    let run = repo.run_with(&[], |c| {
        c.env("KIOKU_MERGE_REQUEST", "1");
    });
    assert_eq!(run.code, 5, "{}", run.context());
    assert_full_suite_ran(&run);
    assert!(
        run.journal
            .scope_reasons
            .iter()
            .any(|r| matches!(r, Reason::ProtectedBranch(_))),
        "{}",
        run.context()
    );
}

// ---------------------------------------------------------------------------------------
// S8 : un run partiel n'est jamais présenté comme complet
// ---------------------------------------------------------------------------------------

#[test]
fn s8_partial_green_exits_0_and_is_never_full_green() {
    let repo = ready();
    repo.feature_branch();
    repo.append("src/b.rs", "// partiel\n");
    let run = repo.run(&[]);
    assert_partial(&run);
    assert_eq!(run.summary["exit_code"], 0);
    assert_eq!(run.summary["scope"], "partial");
    assert_ne!(run.status(), "full-green");
    assert!(run.summary["counts"]["skipped"].as_u64().expect("skipped") > 0);
    assert_eq!(
        run.skipped(),
        names(&[
            ("minix", "a::tests::a_unit"),
            ("minix", "solo::tests::solo_unit"),
            ("minix::ta", "ta_uses_a"),
        ]),
        "chaque saut est nommé"
    );
    // Sortie humaine : la même situation est annoncée « VERT PARTIEL ».
    let mut command = repo.kioku(&["test", "--repo", "."]);
    let out = command.output().expect("spawn");
    assert_eq!(out.status.code(), Some(0));
    let human = text(&out.stdout);
    assert!(
        human.contains("VERT PARTIEL") && !human.contains("VERT COMPLET"),
        "{human}"
    );
}

#[test]
fn s8_user_filter_is_never_full_green() {
    let repo = ready();
    // Sur `main` (portée « tout ») : sans le filtre ce serait un vert complet.
    let full = repo.run(&[]);
    assert_eq!(full.code, 5, "{}", full.context());
    let full_native = repo.run(&["--native-exit-code"]);
    assert_eq!(full_native.code, 0, "{}", full_native.context());
    let on_main_native = repo.run(&["--native-exit-code", "-E", "test(=a::tests::a_unit)"]);
    assert_eq!(on_main_native.code, 0, "{}", on_main_native.context());
    // A successful nextest must not hide a journal write failure.
    let out = repo
        .kioku(&[
            "test",
            "--repo",
            ".",
            "--json",
            "--native-exit-code",
            "--journal",
            ".",
        ])
        .output()
        .expect("spawn");
    assert_eq!(out.status.code(), Some(3), "{}", text(&out.stderr));
    let summary: Value = serde_json::from_slice(&out.stdout).expect("JSON summary");
    assert!(summary["journal"].is_null());
    assert_eq!(summary["exit_code"], 3);
    let on_main = repo.run(&["-E", "test(=a::tests::a_unit)"]);
    assert_eq!(on_main.code, 0, "{}", on_main.context());
    assert_eq!(on_main.status(), "partial-green");
    assert_ne!(on_main.code, 5);
    repo.feature_branch();
    repo.append("src/b.rs", "// filtre\n");
    let on_branch = repo.run(&["-E", "test(=b::tests::b_unit)"]);
    assert_eq!(on_branch.code, 0, "{}", on_branch.context());
    assert_eq!(on_branch.status(), "partial-green");
}

#[test]
fn s8_a_broken_reached_test_exits_1_and_is_red() {
    let repo = ready();
    repo.feature_branch();
    repo.replace("src/a.rs", "    1\n", "    2\n");
    let run = repo.run(&[]);
    let ctx = run.context();
    assert_eq!(run.code, 1, "{ctx}");
    assert_eq!(run.status(), "red", "{ctx}");
    let native = repo.run(&["--native-exit-code"]);
    assert_eq!(native.code, 100, "{}", native.context());
    assert_eq!(native.status(), "red");
    assert_eq!(
        run.outcome("minix", "a::tests::a_unit"),
        Some(Outcome::Failed)
    );
    assert_eq!(run.outcome("minix::ta", "ta_uses_a"), Some(Outcome::Failed));
    assert_eq!(
        run.entry("minix::tb", "tb_uses_b").decision,
        Decision::Skip,
        "ce qui n'atteint pas a.rs reste sauté"
    );
}

// ---------------------------------------------------------------------------------------
// L5b : fichier modifié orphelin (qu'aucune racine de test n'atteint dans la vue de sélection)
// ---------------------------------------------------------------------------------------

/// Module `grp` : `mod.rs` ne fait que déclarer `inner` (orphelin dans la vue : les arêtes `mod`
/// pures y sont ignorées), `inner.rs` porte un test unitaire. Avec `reached_from_root`, `lib.rs`
/// définit aussi `root()` et le binaire d'intégration `tg` l'appelle : dans le graphe COMPLET
/// `tg.rs → lib.rs → grp/mod.rs` (arête `mod`) atteint `mod.rs`, dans la vue non.
fn add_group(repo: &Path, reached_from_root: bool) {
    fs::create_dir_all(repo.join("src/grp")).expect("grp");
    fs::write(repo.join("src/grp/mod.rs"), "pub mod inner;\n").expect("mod.rs");
    fs::write(
        repo.join("src/grp/inner.rs"),
        "#[must_use]\npub fn gi() -> u32 {\n    4\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn inner_unit() {\n        assert_eq!(super::gi(), 4);\n    }\n}\n",
    )
    .expect("inner.rs");
    let mut lib = fs::read_to_string(repo.join("src/lib.rs")).expect("lib.rs");
    lib.push_str("pub mod grp;\n");
    if reached_from_root {
        lib.push_str("\n#[must_use]\npub fn root() -> u32 {\n    0\n}\n");
        fs::write(
            repo.join("tests/tg.rs"),
            "#[test]\nfn tg_uses_root() {\n    assert_eq!(minix::root(), 0);\n}\n",
        )
        .expect("tg.rs");
    }
    fs::write(repo.join("src/lib.rs"), lib).expect("lib.rs");
}

/// `mod.rs` orphelin dans la vue, atteint par le graphe complet (via `tg`).
fn group_reached() -> Fixture {
    ready_with(|repo| add_group(repo, true))
}

/// `mod.rs` orphelin dans la vue ET dans le graphe complet (aucun test n'atteint `lib.rs`).
fn group_unreached() -> Fixture {
    ready_with(|repo| add_group(repo, false))
}

const GROUP_INNER: (&str, &str) = ("minix", "grp::inner::tests::inner_unit");
const GROUP_TG: (&str, &str) = ("minix::tg", "tg_uses_root");

/// Tests lancés par nextest (hors `b_ignored`) : les 5 de la mini-crate + `inner_unit` (+ `tg`).
fn group_runnable(with_tg: bool) -> BTreeSet<String> {
    let mut all = names(&RUNNABLE);
    all.extend(names(&[GROUP_INNER]));
    if with_tg {
        all.extend(names(&[GROUP_TG]));
    }
    all
}

fn passed(run: &Run) -> BTreeSet<String> {
    run.journal
        .outcomes
        .iter()
        .filter(|o| o.outcome == Outcome::Passed)
        .map(|o| format!("{} {}", o.test.binary_id, o.test.name))
        .collect()
}

fn unknown_rust_file(path: &'static str) -> impl Fn(&Reason) -> bool {
    move |r| matches!(r, Reason::UnknownRustFile(p) if p == path)
}

/// Portée « tout » par `UnknownRustFile(path)` : rien de sauté, `code` en sortie.
fn assert_unknown_rust_everything(run: &Run, path: &'static str, code: i32) {
    let ctx = run.context();
    assert_eq!(run.code, code, "{ctx}");
    assert_eq!(run.summary["scope"], "everything", "{ctx}");
    assert_eq!(run.journal.scope, JournalScope::Everything, "{ctx}");
    assert!(
        run.summary["scope_reasons"]
            .as_array()
            .expect("scope_reasons")
            .iter()
            .any(|r| r["code"] == "unknown-rust-file" && r["detail"] == path),
        "{ctx}"
    );
    assert!(
        run.journal
            .scope_reasons
            .iter()
            .any(unknown_rust_file(path)),
        "{ctx}"
    );
    assert_eq!(run.journal.counts.skipped, 0, "{ctx}");
    assert!(
        run.journal
            .entries
            .iter()
            .all(|e| e.decision == Decision::Run),
        "{ctx}"
    );
}

/// Un run rouge : exit 1, statut `red`, jamais un vert (complet ou partiel).
fn assert_red(run: &Run) {
    let ctx = run.context();
    assert_eq!(run.code, 1, "{ctx}");
    assert_eq!(run.summary["exit_code"], 1, "{ctx}");
    assert_eq!(run.status(), "red", "{ctx}");
    assert_ne!(run.journal.status, JournalStatus::FullGreen, "{ctx}");
    assert_ne!(run.journal.status, JournalStatus::PartialGreen, "{ctx}");
}

/// Précondition lue sur le graphe enregistré : `file` n'est atteint par aucune racine de test
/// dans la vue de sélection ; dans le graphe complet, selon `in_full`.
fn assert_reach(repo: &Fixture, file: &str, in_full: bool) {
    let data: GraphData =
        graph::from_json(&fs::read(repo.graph_file()).expect("graph")).expect("graph valide");
    let reached = |g: &GraphData| {
        let roots: BTreeSet<String> = g.test_roots.values().flatten().cloned().collect();
        classify::reach(g, &roots).contains(file)
    };
    assert!(
        !reached(&graph::selection_view(&data)),
        "{file} doit être orphelin dans la vue"
    );
    assert_eq!(
        reached(&data),
        in_full,
        "{file} atteint dans le graphe complet ?"
    );
}

/// 1. `mod.rs` qui ne fait que déclarer des modules : orphelin dans la vue, atteint par le graphe
///    complet (`tg → lib.rs → grp/mod.rs`). Le plan est recalculé sur le graphe complet : les tests
///    qui l'atteignent sont lancés, jamais « exit 0 avec 0 test lancé ».
#[test]
fn l5b_mod_only_file_orphan_in_view_runs_the_tests_reaching_it_in_the_full_graph() {
    let repo = group_reached();
    assert_reach(&repo, "src/grp/mod.rs", true);
    repo.feature_branch();
    repo.append("src/grp/mod.rs", "// commentaire\n");
    let run = repo.run(&[]);
    assert_partial(&run);
    let ctx = run.context();
    assert_eq!(run.journal.modified_files, ["src/grp/mod.rs"], "{ctx}");
    assert!(
        run.journal.scope_reasons.is_empty(),
        "élargi par le graphe complet, pas par « tout »\n{ctx}"
    );
    assert_eq!(passed(&run), names(&[GROUP_TG]), "{ctx}");
    let entry = run.entry(GROUP_TG.0, GROUP_TG.1);
    assert_eq!(entry.decision, Decision::Run, "{ctx}");
    assert!(
        entry
            .reasons
            .iter()
            .any(|r| matches!(r, Reason::ReachesModified { .. })),
        "{:?}",
        entry.reasons
    );
    let mut expected_skipped = group_runnable(true);
    expected_skipped.remove(&format!("{} {}", GROUP_TG.0, GROUP_TG.1));
    assert_eq!(run.skipped(), expected_skipped, "{ctx}");
}

/// 3. Le même `mod.rs` qui casse la compilation (`mod inexistant;`) : rouge (exit 1), jamais 0.
#[test]
fn l5b_mod_only_file_broken_compilation_is_red() {
    let repo = group_reached();
    assert_reach(&repo, "src/grp/mod.rs", true);
    repo.feature_branch();
    repo.append("src/grp/mod.rs", "mod inexistant;\n");
    let run = repo.run(&[]);
    assert_red(&run);
    let ctx = run.context();
    assert_ne!(run.code, 0, "{ctx}");
    assert!(
        run.stderr.contains("inexistant"),
        "l'erreur du compilateur est visible\n{ctx}"
    );
    assert!(
        run.journal
            .outcomes
            .iter()
            .all(|o| o.outcome != Outcome::Passed),
        "aucun test ne passe quand rien ne compile\n{ctx}"
    );
}

/// 2a. `mod.rs` orphelin dans la vue ET dans le graphe complet : portée « tout » (`unknown-rust-file`),
///     suite complète lancée, vrai vert complet (exit 5).
#[test]
fn l5b_mod_only_file_unreached_even_in_full_graph_runs_everything() {
    let repo = group_unreached();
    assert_reach(&repo, "src/grp/mod.rs", false);
    repo.feature_branch();
    repo.append("src/grp/mod.rs", "// commentaire\n");
    let run = repo.run(&[]);
    assert_unknown_rust_everything(&run, "src/grp/mod.rs", 5);
    assert_eq!(run.status(), "full-green", "{}", run.context());
    assert_eq!(passed(&run), group_runnable(false), "{}", run.context());
}

/// 2b. Fichier `.rs` compilé (`src/helper.rs`, déclaré dans `lib.rs`) qu'aucun test n'atteint, même
///     dans le graphe complet : portée « tout », `unknown-rust-file`, exit 5.
#[test]
fn l5b_compiled_file_unreached_by_any_test_runs_everything_exit_5() {
    let repo = ready();
    assert_reach(&repo, "src/helper.rs", false);
    repo.feature_branch();
    repo.replace("src/helper.rs", "    5\n", "    6\n");
    let run = repo.run(&[]);
    assert_unknown_rust_everything(&run, "src/helper.rs", 5);
    assert_eq!(run.status(), "full-green", "{}", run.context());
    assert_eq!(passed(&run), names(&RUNNABLE), "{}", run.context());
}

/// 2c. Idem, mais le fichier orphelin ne compile plus : « tout » est lancé, rouge (exit 1).
#[test]
fn l5b_compiled_file_unreached_by_any_test_broken_is_red() {
    let repo = ready();
    repo.feature_branch();
    repo.replace("src/helper.rs", "    5\n", "    nope(\n");
    let run = repo.run(&[]);
    assert_red(&run);
    assert_eq!(run.summary["scope"], "everything", "{}", run.context());
    assert!(
        run.journal
            .scope_reasons
            .iter()
            .any(unknown_rust_file("src/helper.rs")),
        "{}",
        run.context()
    );
    assert_eq!(run.journal.counts.skipped, 0, "{}", run.context());
}

/// 3b. `mod.rs` orphelin partout qui casse la compilation : « tout », rouge, jamais 0.
#[test]
fn l5b_mod_only_file_unreached_even_in_full_graph_broken_is_red() {
    let repo = group_unreached();
    repo.feature_branch();
    repo.append("src/grp/mod.rs", "mod inexistant;\n");
    let run = repo.run(&[]);
    assert_red(&run);
    assert_eq!(run.summary["scope"], "everything", "{}", run.context());
    assert!(
        run.journal
            .scope_reasons
            .iter()
            .any(unknown_rust_file("src/grp/mod.rs")),
        "{}",
        run.context()
    );
}

/// 4. Contrôle : un fichier atteint normalement dans la vue (`b.rs`) ne déclenche pas la règle ;
///    vert partiel, exit 0, aucune raison de portée, même dans la fixture à `mod.rs` orphelin.
#[test]
fn l5b_control_file_reached_in_the_view_stays_partial() {
    let repo = group_reached();
    repo.feature_branch();
    repo.append("src/b.rs", "// atteint normalement\n");
    let run = repo.run(&[]);
    assert_partial(&run);
    let ctx = run.context();
    assert_eq!(run.journal.modified_files, ["src/b.rs"], "{ctx}");
    assert!(run.journal.scope_reasons.is_empty(), "{ctx}");
    assert_eq!(
        passed(&run),
        names(&[("minix", "b::tests::b_unit"), ("minix::tb", "tb_uses_b")]),
        "{ctx}"
    );
    let mut expected_skipped = group_runnable(true);
    for gone in [("minix", "b::tests::b_unit"), ("minix::tb", "tb_uses_b")] {
        expected_skipped.remove(&format!("{} {}", gone.0, gone.1));
    }
    assert_eq!(run.skipped(), expected_skipped, "{ctx}");
}

// ---------------------------------------------------------------------------------------
// Sur kioku réel (manuel : sous le verrou des builds lourds, une fois)
// ---------------------------------------------------------------------------------------

fn command_text(command: &mut Command) -> (i32, String, String) {
    let out = command.output().expect("spawn");
    (
        out.status.code().expect("code"),
        text(&out.stdout),
        text(&out.stderr),
    )
}

/// Nombre de tests de `cargo nextest list` (sans features), ignorés compris.
fn nextest_list_count(repo: &Path) -> usize {
    let mut command = Command::new("cargo");
    clean_env(&mut command)
        .current_dir(repo)
        .args(["nextest", "list", "--message-format", "json"]);
    let (code, stdout, stderr) = command_text(&mut command);
    assert_eq!(code, 0, "{stderr}");
    let listing: Value = serde_json::from_str(&stdout).expect("json de nextest list");
    listing["rust-suites"]
        .as_object()
        .expect("rust-suites")
        .values()
        .map(|suite| {
            suite["testcases"]
                .as_object()
                .map_or(0, serde_json::Map::len)
        })
        .sum()
}

/// `kioku select base` + `learn --coverage-json` + `kioku test` sur `main` : toute
/// l'inventaire est lancé, exit 5. Clone jeté de ce dépôt dans `target/tmp/`.
#[test]
#[ignore = "kioku réel, plusieurs minutes : cargo nextest run --all-features --test select_l5_faults --run-ignored only (sous flock)"]
fn kioku_real_main_runs_the_whole_inventory() {
    let work = Path::new(env!("CARGO_TARGET_TMPDIR")).join("l5-real");
    assert!(
        !work.exists(),
        "{} existe déjà : à supprimer d'abord",
        work.display()
    );
    fs::create_dir_all(&work).expect("work");
    let repo = work.join("repo");
    let source = env!("CARGO_MANIFEST_DIR");
    git(
        &work,
        &[
            "clone",
            "-q",
            "--no-hardlinks",
            source,
            repo.to_str().expect("utf8"),
        ],
    );
    git(&repo, &["checkout", "-q", "-b", "main"]);
    let store = work.join("store");
    let cov = Path::new(source).join("scratch/optA/cov/lines.json");
    let kioku = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kioku"));
        clean_env(&mut command)
            .current_dir(&repo)
            .args(args)
            .arg("--store")
            .arg(&store);
        command_text(&mut command)
    };
    let started = std::time::Instant::now();
    let (code, stdout, stderr) = kioku(&["select", "base", "--repo", "."]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    println!("select base: {stdout}");
    let (code, stdout, stderr) = kioku(&[
        "select",
        "learn",
        "--repo",
        ".",
        "--coverage-json",
        cov.to_str().expect("utf8"),
    ]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    println!("select learn: {stdout}");
    let setup = started.elapsed();
    let started = std::time::Instant::now();
    let (code, stdout, stderr) = kioku(&["test", "--repo", ".", "--json"]);
    let duration = started.elapsed();
    assert_eq!(code, 5, "{stdout}{stderr}");
    let summary: Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(summary["status"], "full-green");
    assert_eq!(summary["scope"], "everything");
    let counts = &summary["counts"];
    let listed = nextest_list_count(&repo);
    println!(
        "setup {setup:?}; kioku test {duration:?}; nextest list {listed}; counts {counts}; outcomes {}",
        summary["outcomes"]
    );
    assert_eq!(counts["skipped"], 0);
    assert_eq!(
        counts["run"].as_u64().expect("run"),
        counts["inventory"].as_u64().expect("inv") + counts["new"].as_u64().expect("new")
    );
    assert_eq!(
        counts["run"].as_u64().expect("run"),
        u64::try_from(listed).expect("usize")
    );
}
