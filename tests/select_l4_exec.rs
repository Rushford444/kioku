//! Lot L4b : `kioku test`, `kioku select base|learn|plan` de bout en bout.
//!
//! Chaque cas copie la mini-crate `tests/fixtures/select/l4exec/minicrate` (lib `minix` avec
//! `a`, `b`, `solo` et deux binaires d'intégration `ta`, `tb`) dans un dossier temporaire,
//! y fait `git init` (branche `main`) et appelle le binaire `kioku`. Le magasin vient de
//! `kioku select base` (vrai graphe rust-analyzer) ; si `rust-analyzer` est absent du PATH, le
//! test le dit et écrit à la place un graphe fixture dans le magasin
//! (`KIOKU_L4_FIXTURE_GRAPH=1` force ce chemin).
//!
//! Tests : S1 (`main` = tout), S2 (`Cargo.toml` = tout), S3 (magasin vide, graphe corrompu,
//! arêtes apprises absentes, délai dépassé), S4 (journal complet), S6 (arête apprise), S7
//! (test cassé = rouge, exit 1), S8 (codes de sortie, jamais `full-green` partiel).

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use kioku::select::base;
use kioku::select::journal::{self, Journal, JournalScope, JournalStatus, Outcome};
use kioku::select::{
    BaseInventory, BaseStatus, Decision, GRAPH_SCHEMA, GraphData, Reason, TestId, graph,
};
use serde_json::Value;

const FIXTURE_GRAPH_DIGEST: &str = "fixture-graph-1";

// ---------------------------------------------------------------------------------------
// Harnais
// ---------------------------------------------------------------------------------------

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
                    | "CARGO_TARGET_DIR"
                    | "CARGO_BUILD_TARGET_DIR"
                    | "CARGO_CRATE_NAME"
                    | "CARGO_PRIMARY_PACKAGE"
                    | "CARGO_TARGET_TMPDIR"
                    | "KIOKU_MERGE_REQUEST"
                    | "CI_MERGE_REQUEST_IID"
                    | "GITHUB_EVENT_NAME"
                    | "KIOKU_SELECT_STORE"
            )
        {
            command.env_remove(&name);
        }
    }
    command.env("CARGO_NET_OFFLINE", "true")
}

fn collect_isolated(repo: &Path) -> BaseInventory {
    let dir = tempfile::tempdir().expect("inventory tempdir");
    let destination = dir.path().join("base.json");
    let mut command = Command::new(std::env::current_exe().expect("test executable"));
    let output = clean_env(&mut command)
        .current_dir(repo)
        .args([
            "--ignored",
            "--exact",
            "collect_base_in_clean_subprocess",
            "--nocapture",
        ])
        .env("KIOKU_L4_INVENTORY_CHILD", &destination)
        .output()
        .expect("spawn inventory worker");
    assert!(
        output.status.success(),
        "inventory worker failed: {}\n{}",
        text(&output.stdout),
        text(&output.stderr)
    );
    base::load(&destination).expect("load isolated inventory")
}

#[test]
fn fixture_build_ignores_external_target_directories() {
    const MARKER: &str = "KIOKU_L4_TARGET_REGRESSION";
    if let Some(root) = std::env::var_os(MARKER) {
        let root = PathBuf::from(root);
        let repo = bare_repo();
        let _inventory = collect_isolated(&repo.path);
        let deps = repo.path.join("target/debug/deps");
        assert!(
            fs::read_dir(&deps)
                .expect("fixture dependencies")
                .any(|entry| {
                    entry
                        .expect("dependency entry")
                        .file_name()
                        .to_string_lossy()
                        .starts_with("minix-")
                }),
            "fixture artifacts missing from {}",
            deps.display()
        );
        for name in ["outer-target", "outer-build-target"] {
            let outer = root.join(name);
            assert!(
                fs::read_dir(&outer).expect("outer target").next().is_none(),
                "fixture wrote to {}",
                outer.display()
            );
        }
        fs::write(root.join("completed"), b"ok").expect("regression completion marker");
        return;
    }
    let dir = tempfile::tempdir().expect("regression tempdir");
    let outer = dir.path().join("outer-target");
    let build_outer = dir.path().join("outer-build-target");
    fs::create_dir(&outer).expect("outer target");
    fs::create_dir(&build_outer).expect("outer build target");
    let mut command = Command::new(std::env::current_exe().expect("test executable"));
    let output = clean_env(&mut command)
        .args([
            "--exact",
            "fixture_build_ignores_external_target_directories",
            "--nocapture",
        ])
        .env(MARKER, dir.path())
        .env("CARGO_TARGET_DIR", &outer)
        .env("CARGO_BUILD_TARGET_DIR", &build_outer)
        .output()
        .expect("spawn isolation regression");
    assert!(
        output.status.success(),
        "isolation regression failed: {}\n{}",
        text(&output.stdout),
        text(&output.stderr)
    );
    assert!(
        dir.path().join("completed").is_file(),
        "regression child did not run"
    );
}

#[test]
#[ignore = "internal parent-invoked worker"]
fn collect_base_in_clean_subprocess() {
    let destination = PathBuf::from(
        std::env::var_os("KIOKU_L4_INVENTORY_CHILD").expect("inventory worker marker"),
    );
    assert!(
        destination.is_absolute(),
        "inventory destination must be absolute"
    );
    let repo = std::env::current_dir().expect("inventory repo");
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    let inventory = base::collect(&repo, &commit, &[]).expect("collect inventory");
    base::save(&destination, &inventory).expect("save inventory");
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

struct Repo {
    _dir: tempfile::TempDir,
    path: PathBuf,
    store: PathBuf,
    commit: String,
}

impl Repo {
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

    fn append(&self, rel: &str, extra: &str) {
        let mut text = fs::read_to_string(self.path.join(rel)).expect("read");
        text.push_str(extra);
        self.write(rel, &text);
    }

    fn replace(&self, rel: &str, from: &str, to: &str) {
        let text = fs::read_to_string(self.path.join(rel)).expect("read");
        assert!(text.contains(from), "{rel} does not contain {from:?}");
        self.write(rel, &text.replacen(from, to, 1));
    }

    fn feature_branch(&self) {
        git(&self.path, &["checkout", "-q", "-b", "feature"]);
    }

    fn journal_path(&self) -> PathBuf {
        self.path.join("target/kioku-select/journal.json")
    }

    fn journal(&self) -> Journal {
        journal::read(&self.journal_path()).expect("journal readable")
    }

    fn base_path(&self) -> PathBuf {
        self.store.join(format!("base-{}.json", self.commit))
    }
}

fn run(command: &mut Command) -> Output {
    command.output().expect("spawn kioku")
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("exit code")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn rust_analyzer_available() -> bool {
    Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
}

/// Dépôt git sur `main`, sans magasin.
fn bare_repo() -> Repo {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    copy_dir(&fixture_crate(), &repo);
    fs::write(repo.join(".gitignore"), "/target\n").expect("gitignore");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    let store = dir.path().join("store");
    Repo {
        _dir: dir,
        path: repo,
        store,
        commit,
    }
}

/// `JUnit` d'un run vert de tous les tests non ignorés de l'inventaire.
fn green_junit(inv: &BaseInventory) -> String {
    let mut xml = String::from("<?xml version=\"1.0\"?>\n<testsuites>\n");
    for binary in &inv.binaries {
        writeln!(xml, "<testsuite name=\"{}\">", binary.binary_id).expect("write");
        for test in binary
            .tests
            .iter()
            .filter(|t| t.status != BaseStatus::Ignored)
        {
            writeln!(xml, "<testcase name=\"{}\" time=\"0.001\"/>", test.name).expect("write");
        }
        xml.push_str("</testsuite>\n");
    }
    xml.push_str("</testsuites>\n");
    xml
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

/// Graphe écrit à la main (chemin sans rust-analyzer).
fn fixture_graph() -> GraphData {
    let mut data = GraphData {
        schema: GRAPH_SCHEMA.to_owned(),
        digest: FIXTURE_GRAPH_DIGEST.to_owned(),
        ..GraphData::default()
    };
    data.edges.insert(
        "src/lib.rs".to_owned(),
        set(&["src/a.rs", "src/b.rs", "src/solo.rs"]),
    );
    data.edges
        .insert("tests/ta.rs".to_owned(), set(&["src/a.rs"]));
    data.edges
        .insert("tests/tb.rs".to_owned(), set(&["src/b.rs"]));
    data.binary_files.insert(
        "minix".to_owned(),
        set(&["src/lib.rs", "src/a.rs", "src/b.rs", "src/solo.rs"]),
    );
    data.binary_files
        .insert("minix::ta".to_owned(), set(&["tests/ta.rs"]));
    data.binary_files
        .insert("minix::tb".to_owned(), set(&["tests/tb.rs"]));
    for (test, root) in [
        (tid("minix", "a::tests::a_unit"), "src/a.rs"),
        (tid("minix", "b::tests::b_unit"), "src/b.rs"),
        (tid("minix", "b::tests::b_ignored"), "src/b.rs"),
        (tid("minix", "solo::tests::solo_unit"), "src/solo.rs"),
        (tid("minix::ta", "ta_uses_a"), "tests/ta.rs"),
        (tid("minix::tb", "tb_uses_b"), "tests/tb.rs"),
    ] {
        data.test_roots.insert(test, set(&[root]));
    }
    data
}

/// Magasin : inventaire de `main`, graphe, puis `select learn` avec une couverture qui révèle
/// que `ta_uses_a` exécute `src/solo.rs` (arête invisible au graphe statique).
fn prepare_store(repo: &Repo) {
    let real = rust_analyzer_available() && std::env::var_os("KIOKU_L4_FIXTURE_GRAPH").is_none();
    if real {
        let out = run(&mut repo.kioku(&["select", "base", "--repo", "."]));
        assert_eq!(code(&out), 0, "select base: {}", text(&out.stderr));
        assert!(
            text(&out.stdout).contains("6 tests"),
            "{}",
            text(&out.stdout)
        );
    } else {
        eprintln!(
            "rust-analyzer absent du PATH (ou KIOKU_L4_FIXTURE_GRAPH) : graphe fixture écrit dans le magasin"
        );
        let mut inv = collect_isolated(&repo.path);
        let junit = green_junit(&inv);
        base::apply_junit(&mut inv, &junit).expect("junit");
        let data = fixture_graph();
        inv.graph_digest = Some(data.digest.clone());
        fs::create_dir_all(&repo.store).expect("store");
        fs::write(
            repo.store.join(format!("graph-{}.json", data.digest)),
            graph::to_json(&data).expect("graph json"),
        )
        .expect("write graph");
        base::save(&repo.base_path(), &inv).expect("save base");
    }
    let coverage = repo.store.join("coverage.json");
    fs::write(
        &coverage,
        r#"{"minix::ta ta_uses_a": {"tests/ta.rs": [1], "src/a.rs": [1], "src/solo.rs": [1]}}"#,
    )
    .expect("coverage");
    let out = run(&mut repo.kioku(&[
        "select",
        "learn",
        "--repo",
        ".",
        "--coverage-json",
        coverage.to_str().expect("utf8"),
    ]));
    assert_eq!(code(&out), 0, "select learn: {}", text(&out.stderr));
    assert!(
        repo.store
            .join(format!("learned-{}.json", repo.commit))
            .is_file()
    );
}

fn ready_repo() -> Repo {
    let repo = bare_repo();
    prepare_store(&repo);
    repo
}

fn inventory_ids(inv: &BaseInventory) -> BTreeSet<TestId> {
    inv.binaries
        .iter()
        .flat_map(|b| b.tests.iter().map(|t| tid(&b.binary_id, &t.name)))
        .collect()
}

/// S4 : le journal écrit est complet (sautés + lancés = inventaire ∪ nouveaux).
fn assert_journal_complete(repo: &Repo) -> Journal {
    let journal = repo.journal();
    // Sans base lisible (S3), tout vient des résultats : l'inventaire du journal est vide.
    let inventory = if journal.counts.inventory == 0 {
        BTreeSet::new()
    } else {
        inventory_ids(&base::load(&repo.base_path()).expect("base"))
    };
    let all: BTreeSet<TestId> = journal.entries.iter().map(|e| e.test.clone()).collect();
    let new: BTreeSet<TestId> = all.difference(&inventory).cloned().collect();
    let inventory: BTreeSet<TestId> = all.intersection(&inventory).cloned().collect();
    journal::check_complete(&journal, &inventory, &new)
        .unwrap_or_else(|e| panic!("journal incomplete: {e}"));
    assert_eq!(
        journal.counts.run + journal.counts.skipped,
        journal.counts.inventory + journal.counts.new
    );
    journal
}

fn entry<'a>(journal: &'a Journal, binary: &str, name: &str) -> &'a kioku::select::JournalEntry {
    journal
        .entries
        .iter()
        .find(|e| e.test == tid(binary, name))
        .unwrap_or_else(|| panic!("{binary} {name} not in the journal"))
}

fn outcome(journal: &Journal, binary: &str, name: &str) -> Option<Outcome> {
    journal
        .outcomes
        .iter()
        .find(|o| o.test == tid(binary, name))
        .map(|o| o.outcome)
}

fn skipped(journal: &Journal) -> BTreeSet<String> {
    journal
        .entries
        .iter()
        .filter(|e| e.decision == Decision::Skip)
        .map(|e| format!("{} {}", e.test.binary_id, e.test.name))
        .collect()
}

const ALL_TESTS: [(&str, &str); 6] = [
    ("minix", "a::tests::a_unit"),
    ("minix", "b::tests::b_unit"),
    ("minix", "b::tests::b_ignored"),
    ("minix", "solo::tests::solo_unit"),
    ("minix::ta", "ta_uses_a"),
    ("minix::tb", "tb_uses_b"),
];

// ---------------------------------------------------------------------------------------
// select base / learn
// ---------------------------------------------------------------------------------------

#[test]
fn select_base_inventories_the_clean_tree_with_junit_and_graph() {
    let repo = bare_repo();
    if !rust_analyzer_available() {
        eprintln!(
            "rust-analyzer absent du PATH : select base réel non joué (chemin fixture couvert ailleurs)"
        );
        return;
    }
    let out = run(&mut repo.kioku(&["select", "base", "--repo", ".", "--json"]));
    assert_eq!(code(&out), 0, "{}", text(&out.stderr));
    let summary: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(summary["commit"], repo.commit.as_str());
    assert_eq!(summary["binaries"], 3);
    assert_eq!(summary["tests"], 6);
    assert_eq!(summary["green"], true);
    let digest = summary["graph_digest"].as_str().expect("digest");
    assert!(repo.store.join(format!("graph-{digest}.json")).is_file());
    let inv = base::load(&repo.base_path()).expect("base");
    assert!(inv.green);
    assert_eq!(inv.graph_digest.as_deref(), Some(digest));
    let ignored: Vec<_> = inv
        .binaries
        .iter()
        .flat_map(|b| &b.tests)
        .filter(|t| t.status == BaseStatus::Ignored)
        .map(|t| t.name.as_str())
        .collect();
    assert_eq!(ignored, ["b::tests::b_ignored"]);
}

#[test]
fn select_base_refuses_a_dirty_tree_tracked_or_untracked() {
    let repo = bare_repo();
    repo.append("src/b.rs", "// sale\n");
    let out = run(&mut repo.kioku(&["select", "base", "--repo", "."]));
    assert_eq!(code(&out), 3);
    assert!(
        text(&out.stderr).contains("propre"),
        "{}",
        text(&out.stderr)
    );
    git(&repo.path, &["checkout", "-q", "--", "src/b.rs"]);
    repo.write("src/nouveau.rs", "// non suivi\n");
    let out = run(&mut repo.kioku(&["select", "base", "--repo", "."]));
    assert_eq!(
        code(&out),
        3,
        "an untracked file is compiled by cargo: refuse"
    );
    assert!(
        !repo
            .store
            .join(format!("base-{}.json", repo.commit))
            .exists()
    );
}

#[test]
fn select_learn_stamps_the_commit_and_reports_the_missed_edge() {
    let repo = bare_repo();
    prepare_store(&repo);
    let learned =
        kioku::select::learn::read(&repo.store.join(format!("learned-{}.json", repo.commit)))
            .expect("learned");
    assert_eq!(learned.base_commit, repo.commit);
    let files = learned
        .edges
        .get(&tid("minix::ta", "ta_uses_a"))
        .expect("edge for ta_uses_a");
    assert_eq!(files, &set(&["src/solo.rs"]));
    assert_eq!(learned.alerts.len(), 1);
}

#[test]
fn select_learn_needs_a_stored_base() {
    let repo = bare_repo();
    let out = run(&mut repo.kioku(&[
        "select",
        "learn",
        "--repo",
        ".",
        "--coverage-json",
        "x.json",
    ]));
    assert_eq!(code(&out), 3);
}

// ---------------------------------------------------------------------------------------
// kioku test : partiel, rouge, tout
// ---------------------------------------------------------------------------------------

#[test]
fn an_unrelated_change_is_partial_green_with_every_skip_listed() {
    let repo = ready_repo();
    repo.feature_branch();
    repo.append("src/b.rs", "// commentaire sans effet\n");
    let out = run(&mut repo.kioku(&["test", "--repo", "."]));
    assert_eq!(
        code(&out),
        0,
        "{}\n{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    let stdout = text(&out.stdout);
    assert!(stdout.contains("VERT PARTIEL"), "{stdout}");
    assert!(
        stdout.contains("sauté : minix a::tests::a_unit"),
        "{stdout}"
    );
    let journal = assert_journal_complete(&repo);
    assert_eq!(journal.status, JournalStatus::PartialGreen);
    assert_eq!(journal.scope, JournalScope::Partial);
    assert_eq!(
        skipped(&journal),
        BTreeSet::from([
            "minix a::tests::a_unit".to_owned(),
            "minix solo::tests::solo_unit".to_owned(),
            "minix::ta ta_uses_a".to_owned(),
        ])
    );
    for name in skipped(&journal) {
        let (binary, test) = name.split_once(' ').expect("binary test");
        let e = entry(&journal, binary, test);
        assert!(
            matches!(e.reasons.as_slice(), [Reason::NoPathToModified { .. }]),
            "{e:?}"
        );
        assert_eq!(
            outcome(&journal, binary, test),
            None,
            "a skipped test has no result"
        );
    }
    assert_eq!(
        outcome(&journal, "minix", "b::tests::b_unit"),
        Some(Outcome::Passed)
    );
    assert_eq!(
        outcome(&journal, "minix::tb", "tb_uses_b"),
        Some(Outcome::Passed)
    );
    assert_eq!(
        outcome(&journal, "minix", "b::tests::b_ignored"),
        Some(Outcome::Ignored),
        "the ignored test of the base is run-listed but nextest does not run it"
    );
    assert_eq!(journal.modified_files, ["src/b.rs"]);
    assert_eq!(journal.counts.skipped, 3);
}

#[test]
fn json_summary_has_the_contract_fields() {
    let repo = ready_repo();
    repo.feature_branch();
    repo.append("src/b.rs", "// commentaire\n");
    let out = run(&mut repo.kioku(&["test", "--repo", ".", "--json"]));
    assert_eq!(code(&out), 0);
    let summary: Value =
        serde_json::from_slice(&out.stdout).expect("stdout is exactly the JSON summary");
    assert_eq!(summary["schema"], "kioku/select/v1");
    assert_eq!(summary["status"], "partial-green");
    assert_eq!(summary["exit_code"], 0);
    assert_eq!(summary["scope"], "partial");
    assert_eq!(summary["base_commit"], repo.commit.as_str());
    assert_eq!(summary["modified_files"][0], "src/b.rs");
    assert_eq!(summary["counts"]["skipped"], 3);
    assert_eq!(summary["counts"]["run"], 3);
    assert_eq!(summary["outcomes"]["passed"], 2);
    assert_eq!(summary["outcomes"]["ignored"], 1);
    assert_eq!(summary["scope_reasons"], serde_json::json!([]));
    let journal = summary["journal"].as_str().expect("journal path");
    assert!(
        journal.ends_with("target/kioku-select/journal.json"),
        "{journal}"
    );
}

#[test]
fn breaking_a_reached_test_is_red_and_exits_1() {
    let repo = ready_repo();
    repo.feature_branch();
    repo.replace("src/a.rs", "    1\n", "    2\n");
    let out = run(&mut repo.kioku(&["test", "--repo", "."]));
    assert_eq!(
        code(&out),
        1,
        "{}\n{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    assert!(text(&out.stdout).contains("ROUGE"), "{}", text(&out.stdout));
    let journal = assert_journal_complete(&repo);
    assert_eq!(journal.status, JournalStatus::Red);
    assert_eq!(
        outcome(&journal, "minix", "a::tests::a_unit"),
        Some(Outcome::Failed)
    );
    assert_eq!(
        outcome(&journal, "minix::ta", "ta_uses_a"),
        Some(Outcome::Failed)
    );
    assert_eq!(
        entry(&journal, "minix::tb", "tb_uses_b").decision,
        Decision::Skip,
        "the tests that do not reach a.rs stay skipped"
    );
}

#[test]
fn a_compile_error_is_red_and_exits_1() {
    let repo = ready_repo();
    repo.feature_branch();
    repo.append("src/b.rs", "fn broken(\n");
    let out = run(&mut repo.kioku(&["test", "--repo", ".", "--json"]));
    assert_eq!(code(&out), 1, "{}", text(&out.stderr));
    let summary: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(summary["status"], "red");
    let journal = assert_journal_complete(&repo);
    assert_eq!(journal.status, JournalStatus::Red);
    assert!(
        journal
            .outcomes
            .iter()
            .all(|o| o.outcome == Outcome::NotRun),
        "nothing ran: {:?}",
        journal.outcomes
    );
}

#[test]
fn a_learned_edge_selects_the_test_the_static_graph_cannot_see() {
    let repo = ready_repo();
    repo.feature_branch();
    repo.append("src/solo.rs", "// touché\n");
    let out = run(&mut repo.kioku(&["test", "--repo", "."]));
    assert_eq!(
        code(&out),
        0,
        "{}\n{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    let journal = assert_journal_complete(&repo);
    let taa = entry(&journal, "minix::ta", "ta_uses_a");
    assert_eq!(taa.decision, Decision::Run, "S6: the learned edge widens");
    assert!(
        taa.reasons
            .iter()
            .any(|r| matches!(r, Reason::Learned { .. })),
        "{taa:?}"
    );
    assert_eq!(
        outcome(&journal, "minix::ta", "ta_uses_a"),
        Some(Outcome::Passed)
    );
    assert_eq!(
        entry(&journal, "minix::tb", "tb_uses_b").decision,
        Decision::Skip
    );
}

#[test]
fn a_new_test_in_a_changed_file_is_journaled_as_new() {
    let repo = ready_repo();
    repo.feature_branch();
    repo.replace(
        "src/b.rs",
        "    #[test]\n    fn b_unit() {",
        "    #[test]\n    fn b_fresh() {\n        assert_eq!(super::fb(), 2);\n    }\n\n    #[test]\n    fn b_unit() {",
    );
    let out = run(&mut repo.kioku(&["test", "--repo", ".", "--json"]));
    assert_eq!(
        code(&out),
        0,
        "{}\n{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    let summary: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(summary["counts"]["new"], 1, "{summary}");
    let journal = assert_journal_complete(&repo);
    let fresh = entry(&journal, "minix", "b::tests::b_fresh");
    assert_eq!(fresh.decision, Decision::Run);
    assert_eq!(fresh.reasons, vec![Reason::NewTest]);
    assert_eq!(
        outcome(&journal, "minix", "b::tests::b_fresh"),
        Some(Outcome::Passed)
    );
}

#[test]
fn main_branch_runs_everything_and_is_full_green_exit_5() {
    let repo = ready_repo();
    repo.append("src/b.rs", "// sur main\n");
    let out = run(&mut repo.kioku(&["test", "--repo", "."]));
    assert_eq!(
        code(&out),
        5,
        "{}\n{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    assert!(
        text(&out.stdout).contains("VERT COMPLET"),
        "{}",
        text(&out.stdout)
    );
    let journal = assert_journal_complete(&repo);
    assert_eq!(journal.status, JournalStatus::FullGreen);
    assert_eq!(journal.scope, JournalScope::Everything);
    assert!(
        journal
            .scope_reasons
            .iter()
            .any(|r| matches!(r, Reason::ProtectedBranch(b) if b == "main")),
        "{:?}",
        journal.scope_reasons
    );
    assert_eq!(journal.counts.skipped, 0);
    assert_eq!(
        journal.counts.run, 6,
        "every test of the base is launched (S1)"
    );
    for (binary, name) in ALL_TESTS {
        assert_eq!(entry(&journal, binary, name).decision, Decision::Run);
        assert!(outcome(&journal, binary, name).is_some(), "{binary} {name}");
    }
}

#[test]
fn a_merge_request_environment_runs_everything() {
    let repo = ready_repo();
    repo.feature_branch();
    repo.append("src/b.rs", "// mr\n");
    for (name, value) in [
        ("KIOKU_MERGE_REQUEST", "1"),
        ("CI_MERGE_REQUEST_IID", "17"),
        ("GITHUB_EVENT_NAME", "pull_request"),
    ] {
        let out = run(repo
            .kioku(&["test", "--repo", ".", "--json"])
            .env(name, value));
        assert_eq!(code(&out), 5, "{name}: {}", text(&out.stderr));
        let journal = assert_journal_complete(&repo);
        assert_eq!(journal.scope, JournalScope::Everything, "{name}");
        assert_eq!(journal.counts.skipped, 0, "{name}");
    }
}

#[test]
fn touching_cargo_toml_widens_to_everything() {
    let repo = ready_repo();
    repo.feature_branch();
    repo.append("Cargo.toml", "\n# commentaire\n");
    let out = run(&mut repo.kioku(&["test", "--repo", "."]));
    assert_eq!(
        code(&out),
        5,
        "{}\n{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    let journal = assert_journal_complete(&repo);
    assert_eq!(journal.scope, JournalScope::Everything);
    assert!(
        journal
            .scope_reasons
            .iter()
            .any(|r| matches!(r, Reason::BuildInput(p) if p == "Cargo.toml")),
        "{:?}",
        journal.scope_reasons
    );
    assert_eq!(journal.counts.skipped, 0);
}

#[test]
fn different_features_widen_to_everything() {
    let repo = ready_repo();
    repo.feature_branch();
    repo.append("src/b.rs", "// features\n");
    let out = run(&mut repo.kioku(&["test", "--repo", ".", "--features", "extra"]));
    assert_eq!(
        code(&out),
        5,
        "{}\n{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    let journal = assert_journal_complete(&repo);
    assert_eq!(journal.scope, JournalScope::Everything);
    assert!(
        journal
            .scope_reasons
            .iter()
            .any(|r| matches!(r, Reason::BuildEnvironment(_))),
        "{:?}",
        journal.scope_reasons
    );
}

#[test]
fn a_user_filter_is_journaled_filtered_out_and_never_full_green() {
    let repo = ready_repo();
    // Sur `main` : portée « tout », donc le seul écart avec le vert complet est le filtre.
    let out = run(&mut repo.kioku(&[
        "test",
        "--repo",
        ".",
        "-E",
        "test(=a::tests::a_unit)",
        "--json",
    ]));
    assert_eq!(
        code(&out),
        0,
        "{}\n{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    let summary: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(summary["status"], "partial-green");
    assert_eq!(summary["scope"], "everything");
    let journal = assert_journal_complete(&repo);
    assert_eq!(
        journal.user_filter.as_deref(),
        Some("test(=a::tests::a_unit)")
    );
    assert_eq!(
        outcome(&journal, "minix", "a::tests::a_unit"),
        Some(Outcome::Passed)
    );
    assert_eq!(
        outcome(&journal, "minix::tb", "tb_uses_b"),
        Some(Outcome::FilteredOut)
    );
    assert_eq!(
        outcome(&journal, "minix", "b::tests::b_ignored"),
        Some(Outcome::Ignored)
    );
    assert!(
        journal
            .outcomes
            .iter()
            .all(|o| o.outcome != Outcome::NotRun)
    );
    // Sur une branche, avec un saut : jamais « vert complet » non plus.
    repo.feature_branch();
    repo.append("src/b.rs", "// filtre\n");
    let out = run(&mut repo.kioku(&["test", "--repo", ".", "-E", "test(=b::tests::b_unit)"]));
    assert_eq!(
        code(&out),
        0,
        "{}\n{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    assert!(text(&out.stdout).contains("VERT PARTIEL"));
    let journal = assert_journal_complete(&repo);
    assert_eq!(journal.status, JournalStatus::PartialGreen);
}

#[test]
fn a_user_filter_that_matches_nothing_is_not_a_failure_nor_a_full_green() {
    let repo = ready_repo();
    let out = run(&mut repo.kioku(&["test", "--repo", ".", "-E", "test(zzz_nothing)"]));
    assert_eq!(
        code(&out),
        0,
        "{}\n{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    let journal = assert_journal_complete(&repo);
    assert_eq!(journal.status, JournalStatus::PartialGreen);
    assert!(
        journal
            .outcomes
            .iter()
            .all(|o| matches!(o.outcome, Outcome::FilteredOut | Outcome::Ignored))
    );
}

#[test]
fn a_red_test_fails_the_run_even_with_a_user_filter() {
    let repo = ready_repo();
    repo.feature_branch();
    repo.replace("src/a.rs", "    1\n", "    2\n");
    let out = run(&mut repo.kioku(&["test", "--repo", ".", "-E", "test(=a::tests::a_unit)"]));
    assert_eq!(
        code(&out),
        1,
        "{}\n{}",
        text(&out.stdout),
        text(&out.stderr)
    );
}

// ---------------------------------------------------------------------------------------
// S3 : toute erreur → suite complète, jamais un vert
// ---------------------------------------------------------------------------------------

fn assert_s3_full_suite(repo: &Repo, args: &[&str], needle: &str) -> Journal {
    let out = run(&mut repo.kioku(args));
    assert_eq!(
        code(&out),
        3,
        "{}\n{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    let stdout = text(&out.stdout);
    assert!(stdout.contains("INCOMPLET"), "{stdout}");
    let journal = assert_journal_complete(repo);
    assert_eq!(journal.status, JournalStatus::Incomplete);
    assert_eq!(journal.scope, JournalScope::Everything);
    assert_eq!(journal.counts.skipped, 0);
    let reasons = format!("{:?}", journal.scope_reasons);
    assert!(reasons.contains("Error"), "{reasons}");
    assert!(reasons.contains(needle), "{needle} not in {reasons}");
    // La suite complète a vraiment tourné : chaque test vu a un résultat.
    assert_ne!(journal.outcomes, Vec::new());
    assert!(
        journal
            .outcomes
            .iter()
            .all(|o| matches!(o.outcome, Outcome::Passed | Outcome::Ignored)),
        "{:?}",
        journal.outcomes
    );
    journal
}

#[test]
fn an_empty_store_runs_the_full_suite_and_exits_3() {
    let repo = bare_repo();
    repo.feature_branch();
    repo.append("src/b.rs", "// pas de base\n");
    let journal = assert_s3_full_suite(&repo, &["test", "--repo", "."], "base-<commit>.json");
    assert_eq!(journal.counts.inventory, 0, "no base: every test is new");
    // L'oracle de complétude (A5) journalise aussi le `#[ignore]` que nextest liste sans le
    // lancer : 5 tests lancés + 1 ignoré, jamais absent du journal.
    assert_eq!(
        journal.counts.new, 6,
        "the 5 tests nextest runs + 1 ignored"
    );
    assert_eq!(
        outcome(&journal, "minix", "b::tests::b_ignored"),
        Some(Outcome::Ignored)
    );
}

#[test]
fn a_corrupt_or_foreign_graph_file_runs_the_full_suite() {
    let repo = ready_repo();
    repo.feature_branch();
    repo.append("src/b.rs", "// graphe\n");
    let inv = base::load(&repo.base_path()).expect("base");
    let digest = inv.graph_digest.expect("digest");
    let graph_file = repo.store.join(format!("graph-{digest}.json"));
    let good = fs::read(&graph_file).expect("graph");
    // Illisible.
    fs::write(&graph_file, b"{ pas du json").expect("corrupt");
    let journal = assert_s3_full_suite(&repo, &["test", "--repo", "."], "graph");
    assert_eq!(journal.counts.inventory, 6, "the base is still readable");
    // Digest interne différent du nom : le fichier d'un autre graphe est refusé.
    let mut data = graph::from_json(&good).expect("parse");
    data.digest = "autre-digest".to_owned();
    fs::write(&graph_file, graph::to_json(&data).expect("json")).expect("swap");
    assert_s3_full_suite(&repo, &["test", "--repo", "."], "digest");
    // Fichier absent.
    fs::remove_file(&graph_file).expect("remove");
    assert_s3_full_suite(&repo, &["test", "--repo", "."], "graph");
}

#[test]
fn missing_or_foreign_learned_edges_run_the_full_suite() {
    let repo = ready_repo();
    repo.feature_branch();
    repo.append("src/b.rs", "// appris\n");
    let learned_file = repo.store.join(format!("learned-{}.json", repo.commit));
    let good = fs::read_to_string(&learned_file).expect("learned");
    // Estampillé pour un autre commit, mais proprement scellé (régénéré par le code : le
    // scellé A4 n'a rien à redire, c'est bien la vérification d'estampille qui refuse).
    let mut foreign = kioku::select::learn::read(&learned_file).expect("learned");
    foreign.base_commit = "0".repeat(40);
    kioku::select::learn::write(&learned_file, &foreign).expect("write");
    assert_s3_full_suite(&repo, &["test", "--repo", "."], "estampill");
    // Même remplacement fait à la main dans le fichier : le scellé le refuse (A4).
    fs::write(&learned_file, good.replace(&repo.commit, &"0".repeat(40))).expect("write");
    assert_s3_full_suite(
        &repo,
        &["test", "--repo", "."],
        "arêtes apprises incohérentes",
    );
    // Absent : la base n'est plus candidate, tout vient des résultats.
    fs::remove_file(&learned_file).expect("remove");
    let journal = assert_s3_full_suite(&repo, &["test", "--repo", "."], "learned-<commit>.json");
    assert_eq!(journal.counts.inventory, 0);
}

#[test]
fn a_corrupt_base_inventory_runs_the_full_suite() {
    let repo = ready_repo();
    repo.feature_branch();
    repo.append("src/b.rs", "// base corrompue\n");
    fs::write(repo.base_path(), b"[]").expect("corrupt");
    assert_s3_full_suite(&repo, &["test", "--repo", "."], "base inventory");
}

#[test]
fn an_unreadable_d1_file_runs_the_full_suite() {
    let repo = ready_repo();
    repo.feature_branch();
    repo.append("src/b.rs", "// d1\n");
    let journal = assert_s3_full_suite(
        &repo,
        &["test", "--repo", ".", "--d1", "/nonexistent/d1.json"],
        "d1",
    );
    assert_eq!(journal.counts.inventory, 6);
}

#[test]
fn d1_can_only_add_a_test() {
    let repo = ready_repo();
    repo.feature_branch();
    repo.append("src/b.rs", "// d1\n");
    let d1 = repo.store.join("d1.json");
    fs::write(
        &d1,
        r#"{"add":[{"binary_id":"minix::ta","name":"ta_uses_a"}],
            "skip":[{"binary_id":"minix::tb","name":"tb_uses_b"}]}"#,
    )
    .expect("d1");
    let out = run(&mut repo.kioku(&["test", "--repo", ".", "--d1", d1.to_str().expect("utf8")]));
    assert_eq!(
        code(&out),
        0,
        "{}\n{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    let journal = assert_journal_complete(&repo);
    assert_eq!(
        entry(&journal, "minix::ta", "ta_uses_a").decision,
        Decision::Run
    );
    assert_eq!(
        entry(&journal, "minix::tb", "tb_uses_b").decision,
        Decision::Run
    );
    assert_eq!(
        outcome(&journal, "minix::ta", "ta_uses_a"),
        Some(Outcome::Passed)
    );
}

#[cfg(unix)]
#[test]
fn a_nextest_timeout_is_incomplete_and_falls_back_to_the_full_suite() {
    use std::os::unix::fs::PermissionsExt;
    let repo = ready_repo();
    repo.feature_branch();
    repo.append("src/b.rs", "// délai\n");
    // Faux `cargo` : `nextest run` dort (processus remplacé par `sleep`), le reste passe au vrai.
    let real_cargo = text(&run(Command::new("which").arg("cargo")).stdout)
        .trim()
        .to_owned();
    assert_ne!(real_cargo, "");
    let bin = repo.store.join("fakebin");
    fs::create_dir_all(&bin).expect("fakebin");
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = nextest ] && [ \"$2\" = run ]; then exec sleep 120; fi\nexec {real_cargo} \"$@\"\n"
    );
    fs::write(bin.join("cargo"), script).expect("script");
    fs::set_permissions(bin.join("cargo"), fs::Permissions::from_mode(0o755)).expect("chmod");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let started = std::time::Instant::now();
    let out = run(repo
        .kioku(&["test", "--repo", ".", "--timeout", "1", "--json"])
        .env("PATH", path));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(60),
        "the timeout bounds the run"
    );
    assert_eq!(
        code(&out),
        3,
        "{}\n{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    let summary: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(summary["status"], "incomplete");
    assert_eq!(
        summary["scope"], "everything",
        "the full suite was attempted"
    );
    let journal = repo.journal();
    assert_eq!(journal.status, JournalStatus::Incomplete);
    assert!(
        journal
            .outcomes
            .iter()
            .all(|o| o.outcome == Outcome::NotRun)
    );
    assert!(format!("{:?}", journal.scope_reasons).contains("délai"));
}

// ---------------------------------------------------------------------------------------
// select plan
// ---------------------------------------------------------------------------------------

#[test]
fn select_plan_runs_nothing_and_reports_planned() {
    let repo = ready_repo();
    repo.feature_branch();
    repo.append("src/b.rs", "// plan\n");
    let planned = repo.store.join("plan.json");
    let out = run(&mut repo.kioku(&[
        "select",
        "plan",
        "--repo",
        ".",
        "--journal",
        planned.to_str().expect("utf8"),
        "--json",
    ]));
    assert_eq!(code(&out), 0, "{}", text(&out.stderr));
    let summary: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(summary["status"], "planned");
    assert_eq!(summary["counts"]["skipped"], 3);
    let journal = journal::read(&planned).expect("planned journal");
    assert_eq!(journal.status, JournalStatus::Planned);
    assert_eq!(journal.outcomes, Vec::new());
    assert!(
        !repo.journal_path().exists(),
        "plan writes no default journal"
    );
    assert!(
        !text(&out.stderr).contains("Nextest run ID"),
        "nextest was not launched"
    );
}

#[test]
fn a_docs_only_change_launches_nothing_but_the_forced_tests() {
    let repo = ready_repo();
    repo.feature_branch();
    repo.write("docs/notes.md", "# notes\n");
    let out = run(&mut repo.kioku(&["test", "--repo", "."]));
    assert_eq!(
        code(&out),
        0,
        "{}\n{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    let journal = assert_journal_complete(&repo);
    assert_eq!(journal.status, JournalStatus::PartialGreen);
    assert_eq!(
        journal.counts.skipped, 5,
        "everything but the ignored test is skipped"
    );
}

#[test]
fn the_default_store_follows_the_environment() {
    let repo = ready_repo();
    repo.feature_branch();
    repo.append("src/b.rs", "// env\n");
    // Sans --store : $KIOKU_SELECT_STORE puis $XDG_CACHE_HOME/kioku/select.
    let mut command = Command::new(env!("CARGO_BIN_EXE_kioku"));
    clean_env(&mut command)
        .current_dir(&repo.path)
        .args(["test", "--repo", ".", "--json"])
        .env("KIOKU_SELECT_STORE", &repo.store);
    let out = run(&mut command);
    assert_eq!(code(&out), 0, "{}", text(&out.stderr));
    let xdg = repo.store.parent().expect("parent").join("xdg");
    fs::create_dir_all(xdg.join("kioku")).expect("xdg");
    fs::rename(&repo.store, xdg.join("kioku/select")).expect("move store");
    let mut command = Command::new(env!("CARGO_BIN_EXE_kioku"));
    clean_env(&mut command)
        .current_dir(&repo.path)
        .args(["test", "--repo", ".", "--json"])
        .env("XDG_CACHE_HOME", &xdg);
    let out = run(&mut command);
    assert_eq!(code(&out), 0, "{}", text(&out.stderr));
}

#[cfg(unix)]
#[test]
fn nextest_runs_under_the_default_profile_so_nested_nextest_keeps_working() {
    use std::os::unix::fs::PermissionsExt;
    let repo = ready_repo();
    repo.feature_branch();
    repo.append("src/b.rs", "// profil\n");
    let real_cargo = text(&run(Command::new("which").arg("cargo")).stdout)
        .trim()
        .to_owned();
    let bin = repo.store.join("fakebin");
    fs::create_dir_all(&bin).expect("fakebin");
    let log = repo.store.join("argv.log");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nexec {real_cargo} \"$@\"\n",
        log.display()
    );
    fs::write(bin.join("cargo"), script).expect("script");
    fs::set_permissions(bin.join("cargo"), fs::Permissions::from_mode(0o755)).expect("chmod");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = run(repo.kioku(&["test", "--repo", "."]).env("PATH", path));
    assert_eq!(
        code(&out),
        0,
        "{}\n{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    let argv = fs::read_to_string(&log).expect("argv log");
    let run_line = argv
        .lines()
        .find(|line| line.starts_with("nextest run"))
        .unwrap_or_else(|| panic!("no nextest run in {argv}"));
    assert!(
        run_line.contains("--profile default"),
        "a profile of our own leaks through NEXTEST_PROFILE into nested nextest: {run_line}"
    );
    assert!(!run_line.contains("--profile kioku"), "{run_line}");
}
