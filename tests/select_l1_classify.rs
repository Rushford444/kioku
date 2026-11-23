//! Porte L1 : `base`, `changes`, `classify` (spec I1-T § 2 et § 4).
//!
//! Un test par ligne du § 4 (`spec4_lNN_…`), un test par cause S3 (`s3_…`), S1 (`s1_…`),
//! les parseurs sur fixtures réelles minimales (`tests/fixtures/select/l1/`) et les essais
//! `git` sur un dépôt temporaire. Les essais sur le vrai dépôt sont `#[ignore]`
//! (`cargo nextest run --test select_l1_classify --run-ignored only`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use kioku::select::base::{
    self, apply_junit, hash_build_env, is_build_env_name, parse_metadata, parse_nextest_list,
};
use kioku::select::changes::{self, parse_name_status_z, parse_untracked_z};
use kioku::select::classify::{
    ClassifyContext, ContentSource, RepoContent, Version, classify, classify_noted, reach,
};
use kioku::select::{
    BASE_SCHEMA, BaseBinary, BaseInventory, BaseStatus, BaseTest, BinaryKind, Change, ChangeSet,
    GRAPH_SCHEMA, GraphData, PartialScope, Reason, Scope, SelectError, TestId,
};

const METADATA: &str = include_str!("fixtures/select/l1/cargo-metadata.json");
const NEXTEST_LIST: &str = include_str!("fixtures/select/l1/nextest-list.json");
const JUNIT_GREEN: &str = include_str!("fixtures/select/l1/junit-green.xml");
const JUNIT_MIXED: &str = include_str!("fixtures/select/l1/junit-mixed.xml");

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const LIB_TEST: &str = "ci2::domain::strict_json::tests::duplicate_inside_array_object_is_rejected_before_value_normalization";
const LIB_IGNORED: &str = "ci2::storage::unix::tests::state_view_local_benchmark";
const BIN_TEST: &str = "tests::formats_sizes_and_durations";
const CLI_TEST: &str = "a_command_that_reads_the_standard_input_is_never_cached";
const SCHEMA_TEST: &str = "input_selector_variant_refuses_an_undeclared_field";

// ---------------------------------------------------------------------------
// Aides : inventaire, graphe, contexte
// ---------------------------------------------------------------------------

fn test(name: &str, status: BaseStatus) -> BaseTest {
    BaseTest {
        name: name.to_owned(),
        status,
        duration_ms: Some(1),
    }
}

fn binary(id: &str, target: &str, kind: BinaryKind, src: &str, tests: Vec<BaseTest>) -> BaseBinary {
    BaseBinary {
        binary_id: id.to_owned(),
        target_name: target.to_owned(),
        kind,
        src_path: src.to_owned(),
        tests,
    }
}

fn toolchain() -> BTreeMap<String, String> {
    [
        ("rustc", "rustc 1.99.1 (58cf8f9c3 2026-10-13)"),
        ("cargo", "cargo 1.99.1 (5f94df478 2026-08-27)"),
        ("nextest", "cargo-nextest 0.9.151"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}

fn base_inventory() -> BaseInventory {
    BaseInventory {
        schema: BASE_SCHEMA.to_owned(),
        commit: COMMIT.to_owned(),
        toolchain: toolchain(),
        features: Vec::new(),
        build_env: BTreeMap::new(),
        green: true,
        binaries: vec![
            binary(
                "kdw-kioku",
                "kioku",
                BinaryKind::Lib,
                "src/lib.rs",
                vec![
                    test("a::t1", BaseStatus::Pass),
                    test("b::t2", BaseStatus::Pass),
                ],
            ),
            binary(
                "kdw-kioku::bin/kioku",
                "kioku",
                BinaryKind::Bin,
                "src/main.rs",
                vec![test("m1", BaseStatus::Pass)],
            ),
            binary(
                "kdw-kioku::cli",
                "cli",
                BinaryKind::Test,
                "tests/cli.rs",
                vec![test("c1", BaseStatus::Pass), test("c2", BaseStatus::Pass)],
            ),
        ],
        graph_digest: Some("g1".to_owned()),
    }
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

fn graph() -> GraphData {
    let mut graph = GraphData {
        schema: GRAPH_SCHEMA.to_owned(),
        digest: "g1".to_owned(),
        ..GraphData::default()
    };
    graph
        .edges
        .insert("src/lib.rs".into(), set(&["src/a.rs", "src/gone.rs"]));
    graph.edges.insert("src/a.rs".into(), set(&["src/deep.rs"]));
    graph
        .edges
        .insert("tests/cli.rs".into(), set(&["tests/support/mod.rs"]));
    graph.binary_files.insert(
        "kdw-kioku".into(),
        set(&["src/lib.rs", "src/a.rs", "src/gone.rs", "src/deep.rs"]),
    );
    graph.binary_files.insert(
        "kdw-kioku::cli".into(),
        set(&["tests/cli.rs", "tests/support/mod.rs"]),
    );
    graph
        .literal_refs
        .insert("src/data.txt".into(), set(&["src/a.rs"]));
    graph
        .literal_refs
        .insert("README.md".into(), set(&["src/lib.rs"]));
    graph.test_roots.insert(
        TestId {
            binary_id: "kdw-kioku".into(),
            name: "a::t1".into(),
        },
        set(&["src/a.rs"]),
    );
    graph.test_roots.insert(
        TestId {
            binary_id: "kdw-kioku::cli".into(),
            name: "c1".into(),
        },
        set(&["tests/cli.rs"]),
    );
    graph.test_roots.insert(
        TestId {
            binary_id: "kdw-kioku::cli".into(),
            name: "c2".into(),
        },
        set(&["tests/cli.rs"]),
    );
    graph
}

fn context() -> ClassifyContext {
    ClassifyContext {
        branch: Some("feature/x".to_owned()),
        toolchain: toolchain(),
        ..ClassifyContext::default()
    }
}

#[allow(clippy::unnecessary_wraps)] // signature imposée par le lecteur de contenu
fn plain(_path: &str, _version: Version) -> Option<String> {
    Some("pub fn f() {}\n".to_owned())
}

fn changeset(changes: Vec<Change>) -> ChangeSet {
    ChangeSet {
        base: COMMIT.to_owned(),
        changes,
    }
}

fn added(path: &str) -> Change {
    Change::Added {
        path: path.to_owned(),
    }
}
fn modified(path: &str) -> Change {
    Change::Modified {
        path: path.to_owned(),
    }
}
fn deleted(path: &str) -> Change {
    Change::Deleted {
        path: path.to_owned(),
    }
}
fn renamed(from: &str, to: &str) -> Change {
    Change::Renamed {
        from: from.to_owned(),
        to: to.to_owned(),
    }
}

/// Classe avec l'inventaire, le graphe et le contexte par défaut des tests.
fn run(changes: Vec<Change>) -> Scope {
    classify(
        &changeset(changes),
        &base_inventory(),
        &graph(),
        &context(),
        &plain,
    )
}

fn run_with(
    changes: Vec<Change>,
    base: &BaseInventory,
    graph: &GraphData,
    ctx: &ClassifyContext,
) -> Scope {
    classify(&changeset(changes), base, graph, ctx, &plain)
}

fn reasons(scope: &Scope) -> &[Reason] {
    match scope {
        Scope::Everything { reasons } => reasons,
        Scope::Partial(partial) => panic!("Everything attendu, obtenu {partial:?}"),
    }
}

fn partial(scope: Scope) -> PartialScope {
    match scope {
        Scope::Partial(partial) => partial,
        Scope::Everything { reasons } => panic!("Partial attendu, obtenu {reasons:?}"),
    }
}

fn is_error_containing(reason: &Reason, needle: &str) -> bool {
    matches!(reason, Reason::Error(message) if message.contains(needle))
}

// ---------------------------------------------------------------------------
// § 4, une ligne = un test
// ---------------------------------------------------------------------------

#[test]
fn spec4_l01_build_files_force_everything() {
    for path in [
        "Cargo.toml",
        "Cargo.lock",
        "build.rs",
        "crates/x/Cargo.toml",
        "src/bin/tool/build.rs",
        ".cargo/config.toml",
        "rust-toolchain",
        "rust-toolchain.toml",
        ".config/nextest.toml",
        "deny.toml",
    ] {
        let scope = run(vec![modified(path)]);
        assert!(
            reasons(&scope).contains(&Reason::BuildInput(path.to_owned())),
            "{path} : {scope:?}"
        );
    }
}

#[test]
fn spec4_l02_unreferenced_non_rust_under_compiled_trees_forces_everything() {
    for path in [
        "src/blob.bin",
        "tests/fixtures/new.json",
        "examples/data.txt",
        "config/app.yaml",
    ] {
        let scope = run(vec![modified(path)]);
        assert_eq!(
            reasons(&scope),
            [Reason::UnreferencedNonRust(path.to_owned())],
            "{path}"
        );
    }
}

#[test]
fn spec4_l03_referenced_non_rust_adds_the_citing_files_to_modified() {
    let result = partial(run(vec![modified("src/data.txt")]));
    assert_eq!(result.modified, set(&["src/a.rs"]));
    assert!(result.whole_binaries.is_empty());
    // Un Markdown racine cité par le code n'est plus inerte.
    let result = partial(run(vec![modified("README.md")]));
    assert_eq!(result.modified, set(&["src/lib.rs"]));
    assert!(result.inert.is_empty());
}

#[test]
fn spec4_l03b_inert_globs_are_inert_when_not_referenced() {
    let result = partial(run(vec![
        modified("docs/guide.md"),
        added("scratch/notes.txt"),
        modified("CHANGELOG.md"),
    ]));
    assert_eq!(
        result.inert,
        set(&["docs/guide.md", "scratch/notes.txt", "CHANGELOG.md"])
    );
    assert!(result.modified.is_empty());
    // `*.md` ne franchit pas `/` : un Markdown sous un dossier quelconque est un doute.
    let scope = run(vec![modified("notes/x.md")]);
    assert_eq!(
        reasons(&scope),
        [Reason::UnreferencedNonRust("notes/x.md".to_owned())]
    );
}

#[test]
fn spec4_l04_new_test_file_without_binary_in_base_runs_the_whole_binary() {
    let result = partial(run(vec![
        added("tests/fresh.rs"),
        added("tests/folder/main.rs"),
    ]));
    assert_eq!(
        result.whole_binaries,
        BTreeMap::from([
            (
                "kdw-kioku::fresh".to_owned(),
                Reason::NewBinary("kdw-kioku::fresh".to_owned())
            ),
            (
                "kdw-kioku::folder".to_owned(),
                Reason::NewBinary("kdw-kioku::folder".to_owned())
            ),
        ])
    );
    // Un fichier de test dont la cible existe déjà, ou un module de test, reste « modifié ».
    let result = partial(run(vec![
        added("tests/cli.rs"),
        added("tests/support/extra.rs"),
    ]));
    assert!(result.whole_binaries.is_empty());
    assert_eq!(
        result.modified,
        set(&["tests/cli.rs", "tests/support/extra.rs"])
    );
    // Un `.rs` ajouté sous src/ est atteint via le `mod` de son parent, modifié aussi.
    let result = partial(run(vec![added("src/new_mod.rs"), modified("src/lib.rs")]));
    assert_eq!(result.modified, set(&["src/new_mod.rs", "src/lib.rs"]));
}

#[test]
fn spec4_l04b_new_binary_with_ambiguous_package_forces_everything() {
    let mut base = base_inventory();
    base.binaries.push(binary(
        "other-pkg::t",
        "t",
        BinaryKind::Test,
        "tests/t.rs",
        Vec::new(),
    ));
    let scope = run_with(vec![added("tests/fresh.rs")], &base, &graph(), &context());
    assert!(
        reasons(&scope)
            .iter()
            .any(|r| is_error_containing(r, "indéterminable"))
    );
    let mut empty = base_inventory();
    empty.binaries.clear();
    let scope = run_with(vec![added("tests/fresh.rs")], &empty, &graph(), &context());
    assert!(
        reasons(&scope)
            .iter()
            .any(|r| is_error_containing(r, "indéterminable"))
    );
}

#[test]
fn spec4_l05_deleted_or_renamed_file_runs_its_binary_and_what_reached_it() {
    let result = partial(run(vec![deleted("src/gone.rs")]));
    assert_eq!(result.modified, set(&["src/gone.rs"]));
    assert_eq!(
        result.whole_binaries,
        BTreeMap::from([(
            "kdw-kioku".to_owned(),
            Reason::DeletedOrRenamed("src/gone.rs".to_owned())
        )])
    );
    // Renommage : le chemin de base compte, le nouveau chemin suit les règles ordinaires.
    let result = partial(run(vec![renamed(
        "tests/support/mod.rs",
        "tests/support/helpers.rs",
    )]));
    assert_eq!(
        result.modified,
        set(&["tests/support/mod.rs", "tests/support/helpers.rs"])
    );
    assert_eq!(
        result.whole_binaries.get("kdw-kioku::cli"),
        Some(&Reason::DeletedOrRenamed("tests/support/mod.rs".to_owned()))
    );
    // Renommer un fichier de test vers une cible neuve : binaire neuf + ancien binaire.
    let result = partial(run(vec![renamed("tests/cli.rs", "tests/cli2.rs")]));
    assert_eq!(
        result.whole_binaries.get("kdw-kioku::cli2"),
        Some(&Reason::NewBinary("kdw-kioku::cli2".to_owned()))
    );
    assert_eq!(
        result.whole_binaries.get("kdw-kioku::cli"),
        Some(&Reason::DeletedOrRenamed("tests/cli.rs".to_owned()))
    );
    // Un fichier non Rust supprimé et cité fait rejoindre ses citeurs.
    let result = partial(run(vec![deleted("src/data.txt")]));
    assert!(result.modified.contains("src/a.rs"));
}

#[test]
fn spec4_l06_macro_in_base_or_current_version_forces_everything() {
    let base_only = |path: &str, version: Version| match version {
        Version::Base => Some(format!("// {path}\nmacro_rules! m {{ () => {{}} }}\n")),
        Version::Current => Some("fn f() {}\n".to_owned()),
    };
    let current_only = |_: &str, version: Version| match version {
        Version::Base => Some("fn f() {}\n".to_owned()),
        Version::Current => Some("#[macro_export]\nmacro_rules! m {() => {}}\n".to_owned()),
    };
    let spaced = |_: &str, _: Version| Some("macro_rules !  m { () => {} }".to_owned());
    let proc_macro =
        |_: &str, _: Version| Some("#[proc_macro_derive(X)]\npub fn x() {}".to_owned());
    let changes = changeset(vec![modified("src/a.rs")]);
    let (b, g, c) = (base_inventory(), graph(), context());
    for source in [
        &base_only as &dyn ContentSource,
        &current_only,
        &spaced,
        &proc_macro,
    ] {
        let scope = classify(&changes, &b, &g, &c, source);
        assert_eq!(reasons(&scope), [Reason::Macro("src/a.rs".to_owned())]);
    }
    // Ajouté (version actuelle seule), supprimé (version de base seule).
    let scope = classify(
        &changeset(vec![added("src/new.rs")]),
        &b,
        &g,
        &c,
        &current_only,
    );
    assert_eq!(reasons(&scope), [Reason::Macro("src/new.rs".to_owned())]);
    let scope = classify(
        &changeset(vec![deleted("src/gone.rs")]),
        &b,
        &g,
        &c,
        &base_only,
    );
    assert_eq!(reasons(&scope), [Reason::Macro("src/gone.rs".to_owned())]);
    // Un `.rs` sans macro ne déclenche rien.
    assert!(matches!(run(vec![modified("src/a.rs")]), Scope::Partial(_)));
}

#[test]
fn spec4_l06b_unreadable_rust_content_forces_everything() {
    let unreadable = |_: &str, _: Version| None;
    let scope = classify(
        &changeset(vec![modified("src/a.rs")]),
        &base_inventory(),
        &graph(),
        &context(),
        &unreadable,
    );
    assert!(
        reasons(&scope)
            .iter()
            .any(|r| is_error_containing(r, "src/a.rs"))
    );
}

#[test]
fn spec4_l06c_rust_outside_src_and_tests_forces_everything() {
    for path in [
        "examples/demo.rs",
        "benches/b.rs",
        "build.rs",
        "scripts/tool.rs",
        "crates/x/src/lib.rs",
    ] {
        let scope = run(vec![modified(path)]);
        assert!(
            reasons(&scope).contains(&Reason::UnknownRustFile(path.to_owned())),
            "{path} : {scope:?}"
        );
    }
}

#[test]
fn spec4_l07_toolchain_features_and_build_env_changes_force_everything() {
    // Détail par cause dans les tests `s3_*` ; ici la ligne § 4 dans son ensemble.
    let mut ctx = context();
    ctx.features = vec!["fixture-feature".to_owned()];
    ctx.toolchain
        .insert("rustc".to_owned(), "rustc 1.100.0".to_owned());
    ctx.build_env
        .insert("RUSTFLAGS".to_owned(), "abc".to_owned());
    let scope = run_with(
        vec![modified("src/a.rs")],
        &base_inventory(),
        &graph(),
        &ctx,
    );
    let found = reasons(&scope);
    assert_eq!(found.len(), 3, "{found:?}");
    assert!(
        found
            .iter()
            .all(|r| matches!(r, Reason::BuildEnvironment(_)))
    );
}

#[test]
fn spec4_l08_external_input_always_runs_the_tests_that_reach_it() {
    let mut g = graph();
    g.edges.insert(
        "tests/support/mod.rs".into(),
        set(&["tests/support/stop.rs"]),
    );
    g.external_inputs.insert(
        "/home/node/work/kioku-04/ARRET-KIOKU.txt".into(),
        set(&["tests/support/stop.rs"]),
    );
    // c1/c2 atteignent stop.rs (tests/cli.rs -> support/mod.rs -> support/stop.rs) ; a::t1 non.
    g.test_roots.insert(
        TestId {
            binary_id: "kdw-kioku::cli".into(),
            name: "c1".into(),
        },
        set(&["tests/cli.rs", "tests/support/mod.rs"]),
    );
    let result = partial(run_with(
        vec![modified("docs/x.md")],
        &base_inventory(),
        &g,
        &context(),
    ));
    let forced: Vec<_> = result
        .forced_tests
        .keys()
        .map(|t| t.name.as_str())
        .collect();
    assert_eq!(forced, ["c1", "c2"]);
    assert_eq!(
        result.forced_tests[&TestId {
            binary_id: "kdw-kioku::cli".into(),
            name: "c1".into()
        }],
        Reason::ExternalInput("/home/node/work/kioku-04/ARRET-KIOKU.txt".to_owned())
    );
}

#[test]
fn spec4_l09_graph_digest_different_from_the_base_forces_everything() {
    // Le calcul de l'« union des deux graphes » appartient à L2/L4 : classify ne reçoit que
    // le graphe de la base et refuse un graphe dont le digest n'est pas celui de l'inventaire.
    let mut g = graph();
    g.digest = "g2".to_owned();
    let scope = run_with(
        vec![modified("docs/x.md")],
        &base_inventory(),
        &g,
        &context(),
    );
    assert!(
        reasons(&scope)
            .iter()
            .any(|r| is_error_containing(r, "digest du graphe"))
    );
    let mut base = base_inventory();
    base.graph_digest = None;
    let scope = run_with(vec![modified("docs/x.md")], &base, &graph(), &context());
    assert!(
        reasons(&scope)
            .iter()
            .any(|r| is_error_containing(r, "digest du graphe"))
    );
}

#[test]
fn spec4_l10_red_ignored_flaky_or_unknown_base_tests_are_always_forced() {
    let mut base = base_inventory();
    base.binaries[0].tests = vec![
        test("a::pass", BaseStatus::Pass),
        test("a::fail", BaseStatus::Fail),
        test("a::flaky", BaseStatus::Flaky),
        test("a::ignored", BaseStatus::Ignored),
        test("a::unknown", BaseStatus::Unknown),
    ];
    let result = partial(run_with(
        vec![modified("docs/x.md")],
        &base,
        &graph(),
        &context(),
    ));
    let id = |name: &str| TestId {
        binary_id: "kdw-kioku".into(),
        name: name.to_owned(),
    };
    assert_eq!(result.forced_tests.len(), 4);
    assert_eq!(
        result.forced_tests[&id("a::fail")],
        Reason::BaseNotGreen(BaseStatus::Fail)
    );
    assert_eq!(
        result.forced_tests[&id("a::flaky")],
        Reason::BaseNotGreen(BaseStatus::Flaky)
    );
    assert_eq!(
        result.forced_tests[&id("a::ignored")],
        Reason::BaseNotGreen(BaseStatus::Ignored)
    );
    assert_eq!(
        result.forced_tests[&id("a::unknown")],
        Reason::BaseNotGreen(BaseStatus::Unknown)
    );
    assert!(!result.forced_tests.contains_key(&id("a::pass")));
}

#[test]
fn spec4_l07_to_l10_rule7_modified_rust_under_src_and_tests_is_modified() {
    let result = partial(run(vec![modified("src/a.rs"), modified("tests/cli.rs")]));
    assert_eq!(result.modified, set(&["src/a.rs", "tests/cli.rs"]));
    assert!(result.whole_binaries.is_empty() && result.forced_tests.is_empty());
}

// ---------------------------------------------------------------------------
// S1 : main et demande de merge
// ---------------------------------------------------------------------------

#[test]
fn s1_main_branch_runs_everything() {
    for branch in ["main", "master", "MAIN", "refs/heads/main"] {
        let mut ctx = context();
        ctx.branch = Some(branch.to_owned());
        let scope = run_with(
            vec![modified("docs/x.md")],
            &base_inventory(),
            &graph(),
            &ctx,
        );
        assert!(
            reasons(&scope)
                .iter()
                .any(|r| matches!(r, Reason::ProtectedBranch(_))),
            "{branch}"
        );
    }
}

#[test]
fn s1_merge_request_runs_everything_even_on_a_feature_branch() {
    let mut ctx = context();
    ctx.is_merge_request = true;
    let scope = run_with(
        vec![modified("docs/x.md")],
        &base_inventory(),
        &graph(),
        &ctx,
    );
    assert_eq!(
        reasons(&scope),
        [Reason::ProtectedBranch("demande de merge".to_owned())]
    );
}

#[test]
fn s1_detached_head_is_not_protected_but_is_noted() {
    let mut ctx = context();
    ctx.branch = None;
    let (scope, notes) = classify_noted(
        &changeset(vec![modified("docs/x.md")]),
        &base_inventory(),
        &graph(),
        &ctx,
        &plain,
    );
    assert!(matches!(scope, Scope::Partial(_)));
    assert!(
        notes.iter().any(|n| n.contains("branche inconnue")),
        "{notes:?}"
    );
    // Une branche nommée ne produit pas cette note.
    let (_, notes) = classify_noted(
        &changeset(Vec::new()),
        &base_inventory(),
        &graph(),
        &context(),
        &plain,
    );
    assert!(notes.is_empty(), "{notes:?}");
}

#[test]
fn s1_custom_protected_branches_are_honoured() {
    let mut ctx = context();
    ctx.protected_branches = vec!["release".to_owned()];
    ctx.branch = Some("release".to_owned());
    let scope = run_with(Vec::new(), &base_inventory(), &graph(), &ctx);
    assert_eq!(
        reasons(&scope),
        [Reason::ProtectedBranch("release".to_owned())]
    );
    ctx.branch = Some("main".to_owned());
    assert!(matches!(
        run_with(Vec::new(), &base_inventory(), &graph(), &ctx),
        Scope::Partial(_)
    ));
}

// ---------------------------------------------------------------------------
// S3 : une cause = un test
// ---------------------------------------------------------------------------

#[test]
fn s3_base_not_green_runs_everything() {
    let mut base = base_inventory();
    base.green = false;
    let scope = run_with(vec![modified("docs/x.md")], &base, &graph(), &context());
    assert_eq!(
        reasons(&scope),
        [Reason::Error("base non verte".to_owned())]
    );
}

#[test]
fn s3_graph_digest_mismatch_runs_everything() {
    let mut g = graph();
    g.digest = "other".to_owned();
    let scope = run_with(Vec::new(), &base_inventory(), &g, &context());
    assert!(
        reasons(&scope)
            .iter()
            .any(|r| is_error_containing(r, "digest du graphe"))
    );
}

#[test]
fn spec4_l07_s3_toolchain_difference_runs_everything() {
    let mut ctx = context();
    ctx.toolchain
        .insert("cargo".to_owned(), "cargo 1.100.0".to_owned());
    let scope = run_with(Vec::new(), &base_inventory(), &graph(), &ctx);
    assert!(matches!(reasons(&scope), [Reason::BuildEnvironment(m)] if m.contains("cargo")));
    // Un outil absent du contexte actuel est aussi une différence.
    ctx.toolchain = toolchain();
    ctx.toolchain.remove("nextest");
    let scope = run_with(Vec::new(), &base_inventory(), &graph(), &ctx);
    assert!(matches!(reasons(&scope), [Reason::BuildEnvironment(m)] if m.contains("nextest")));
}

#[test]
fn spec4_l07_s3_features_difference_runs_everything() {
    let mut ctx = context();
    ctx.features = vec!["fixture-feature".to_owned()];
    let scope = run_with(Vec::new(), &base_inventory(), &graph(), &ctx);
    assert!(matches!(reasons(&scope), [Reason::BuildEnvironment(m)] if m.contains("features")));
    // L'ordre et les doublons ne comptent pas.
    let mut base = base_inventory();
    base.features = vec!["a".to_owned(), "b".to_owned()];
    ctx.features = vec!["b".to_owned(), "a".to_owned(), "a".to_owned()];
    assert!(matches!(
        run_with(Vec::new(), &base, &graph(), &ctx),
        Scope::Partial(_)
    ));
}

#[test]
fn spec4_l07_s3_build_env_difference_runs_everything_without_leaking_values() {
    let mut ctx = context();
    ctx.build_env
        .insert("RUSTFLAGS".to_owned(), "hash-secret-value".to_owned());
    let scope = run_with(Vec::new(), &base_inventory(), &graph(), &ctx);
    let [Reason::BuildEnvironment(message)] = reasons(&scope) else {
        panic!("{scope:?}")
    };
    assert!(message.contains("RUSTFLAGS"));
    assert!(!message.contains("hash-secret-value"));
}

#[test]
fn s3_diff_from_another_base_runs_everything() {
    let mut changes = changeset(vec![modified("docs/x.md")]);
    changes.base = "f".repeat(40);
    let scope = classify(&changes, &base_inventory(), &graph(), &context(), &plain);
    assert!(
        reasons(&scope)
            .iter()
            .any(|r| is_error_containing(r, "le diff part de"))
    );
}

#[test]
fn s3_foreign_schemas_run_everything() {
    let mut base = base_inventory();
    base.schema = "kioku/select/base/v0".to_owned();
    let scope = run_with(Vec::new(), &base, &graph(), &context());
    assert!(
        reasons(&scope)
            .iter()
            .any(|r| is_error_containing(r, "schéma de la base"))
    );
    let mut g = graph();
    g.schema = "nope".to_owned();
    let scope = run_with(Vec::new(), &base_inventory(), &g, &context());
    assert!(
        reasons(&scope)
            .iter()
            .any(|r| is_error_containing(r, "schéma du graphe"))
    );
}

#[test]
fn s3_invalid_change_path_runs_everything() {
    for path in ["../escape.rs", "/abs/path.rs", "a//b.rs", ""] {
        let scope = run(vec![modified(path)]);
        assert!(
            reasons(&scope)
                .iter()
                .any(|r| is_error_containing(r, "chemin de changement")),
            "{path:?}"
        );
    }
}

#[test]
fn accumulation_reports_every_reason_found() {
    let mut base = base_inventory();
    base.green = false;
    let mut ctx = context();
    ctx.branch = Some("main".to_owned());
    let scope = run_with(
        vec![
            modified("Cargo.toml"),
            modified("src/blob.bin"),
            modified("examples/x.rs"),
        ],
        &base,
        &graph(),
        &ctx,
    );
    let found = reasons(&scope);
    for expected in [
        Reason::ProtectedBranch("main".to_owned()),
        Reason::Error("base non verte".to_owned()),
        Reason::BuildInput("Cargo.toml".to_owned()),
        Reason::UnreferencedNonRust("src/blob.bin".to_owned()),
        Reason::UnknownRustFile("examples/x.rs".to_owned()),
    ] {
        assert!(
            found.contains(&expected),
            "{expected:?} manquant dans {found:?}"
        );
    }
    let unique: BTreeSet<_> = found.iter().collect();
    assert_eq!(unique.len(), found.len(), "raisons en double : {found:?}");
}

#[test]
fn reach_follows_edges_transitively_and_includes_the_roots() {
    let g = graph();
    assert_eq!(
        reach(&g, &set(&["src/lib.rs"])),
        set(&["src/lib.rs", "src/a.rs", "src/gone.rs", "src/deep.rs"])
    );
    assert_eq!(reach(&g, &set(&["unknown.rs"])), set(&["unknown.rs"]));
    let mut cyclic = graph();
    cyclic
        .edges
        .insert("src/deep.rs".into(), set(&["src/lib.rs"]));
    assert_eq!(reach(&cyclic, &set(&["src/deep.rs"])).len(), 4);
}

// ---------------------------------------------------------------------------
// changes : parse name-status -z
// ---------------------------------------------------------------------------

#[test]
fn name_status_parses_added_modified_deleted_renamed_copied_and_type_change() {
    let raw = b"A\0src/new.rs\0M\0src/m.rs\0D\0src/d.rs\0R087\0src/old.rs\0src/renamed.rs\0C75\0src/x.rs\0src/copy.rs\0T\0src/t.rs\0";
    assert_eq!(
        parse_name_status_z(raw).expect("parse"),
        vec![
            added("src/new.rs"),
            modified("src/m.rs"),
            deleted("src/d.rs"),
            renamed("src/old.rs", "src/renamed.rs"),
            added("src/copy.rs"),
            modified("src/t.rs"),
        ]
    );
    assert_eq!(parse_name_status_z(b"").expect("vide"), Vec::new());
}

#[test]
fn name_status_keeps_spaces_and_newlines_in_paths() {
    let raw = b"A\0src/with space.rs\0M\0src/new\nline.rs\0";
    assert_eq!(
        parse_name_status_z(raw).expect("parse"),
        vec![added("src/with space.rs"), modified("src/new\nline.rs")]
    );
}

#[test]
fn name_status_refuses_non_utf8_paths() {
    let error = parse_name_status_z(b"A\0src/\xff\xfe.rs\0").expect_err("non UTF-8");
    assert!(matches!(error, SelectError::Invalid { .. }), "{error:?}");
    assert!(parse_untracked_z(b"src/\xff.rs\0").is_err());
}

#[test]
fn name_status_refuses_anything_ambiguous() {
    for raw in [
        &b"U\0src/conflict.rs\0"[..],
        b"X\0src/x.rs\0",
        b"B\0src/x.rs\0",
        b"A\0src/new.rs",            // NUL final manquant
        b"A\0",                      // chemin manquant
        b"R100\0src/only_from.rs\0", // destination manquante
        b"R100\0src/a.rs\0src/a.rs\0",
        b"M5\0src/x.rs\0", // score sur un statut simple
        b"Rabc\0src/a.rs\0src/b.rs\0",
        b"\0src/x.rs\0",
        b"A\0../escape.rs\0",
        b"A\0\0",
    ] {
        assert!(
            parse_name_status_z(raw).is_err(),
            "devrait être refusé : {:?}",
            String::from_utf8_lossy(raw)
        );
    }
}

#[test]
fn untracked_paths_become_added_and_nested_repos_lose_their_slash() {
    assert_eq!(
        parse_untracked_z(b"src/new.rs\0scratch/nested/\0").expect("parse"),
        vec![added("src/new.rs"), added("scratch/nested")]
    );
    assert!(parse_untracked_z(b"src/new.rs").is_err());
}

// ---------------------------------------------------------------------------
// changes : essais git sur un dépôt temporaire
// ---------------------------------------------------------------------------

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(["-c", "commit.gpgsign=false", "-c", "core.autocrlf=false"])
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git lance");
    assert!(
        output.status.success(),
        "git {args:?} : {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

fn write(repo: &Path, relative: &str, content: &str) {
    let path = repo.join(relative);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, content).expect("write");
}

const BODY: &str = "line one\nline two\nline three\nline four\nline five\nline six\nline seven\n";

/// Dépôt de départ : un commit de base puis un second commit (docs).
fn fixture_repo() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    git(repo, &["init", "-q", "-b", "work"]);
    write(repo, ".gitignore", "ignored.log\n");
    write(repo, "src/lib.rs", "pub fn f() {}\n");
    write(repo, "src/old.rs", BODY);
    write(repo, "src/keep.rs", &format!("// keep\n{BODY}"));
    write(repo, "README.md", "# readme\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "base"]);
    let base = git(repo, &["rev-parse", "HEAD"]);
    write(repo, "docs/x.md", "docs\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "docs"]);
    (dir, base)
}

#[test]
fn changes_collect_sees_commits_index_worktree_and_untracked_files() {
    let (dir, base) = fixture_repo();
    let repo = dir.path();
    write(repo, "src/lib.rs", "pub fn f() { /* edited */ }\n"); // modifié (arbre de travail)
    std::fs::remove_file(repo.join("src/old.rs")).expect("rm"); // supprimé
    git(repo, &["mv", "src/keep.rs", "src/kept.rs"]); // renommé (index)
    write(repo, "src/staged.rs", "pub fn s() {}\n");
    git(repo, &["add", "src/staged.rs"]); // ajouté (index)
    write(repo, "tests/untracked.rs", "fn u() {}\n"); // non suivi
    write(repo, "ignored.log", "noise\n"); // non suivi mais ignoré
    let set = changes::collect(repo, &base).expect("collect");
    assert_eq!(set.base, base);
    assert_eq!(
        set.changes,
        vec![
            added("docs/x.md"), // commit après la base
            added("src/staged.rs"),
            added("tests/untracked.rs"),
            modified("src/lib.rs"),
            deleted("src/old.rs"),
            renamed("src/keep.rs", "src/kept.rs"),
        ]
    );
}

#[test]
fn changes_collect_ignores_a_pure_touch_and_resolves_short_refs() {
    let (dir, base) = fixture_repo();
    let repo = dir.path();
    // `touch` : mtime changé, contenu identique.
    write(repo, "src/lib.rs", "pub fn f() {}\n");
    let set = changes::collect(repo, "HEAD").expect("collect");
    assert_eq!(set.base, git(repo, &["rev-parse", "HEAD"]));
    assert!(set.changes.is_empty(), "{:?}", set.changes);
    // Une référence courte ou une branche résout vers les 40 hex.
    let short = &base[..10];
    assert_eq!(changes::collect(repo, short).expect("short").base, base);
    // Après une vraie modification : le fichier apparaît.
    write(repo, "src/lib.rs", "pub fn f() { 1; }\n");
    let set = changes::collect(repo, "HEAD").expect("collect");
    assert_eq!(set.changes, vec![modified("src/lib.rs")]);
}

#[test]
fn s3_changes_collect_fails_on_unknown_base_bad_refs_and_non_repo() {
    let (dir, _base) = fixture_repo();
    let repo = dir.path();
    for bad in [
        "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        "no-such-branch",
        "",
        "--output=x",
    ] {
        assert!(changes::collect(repo, bad).is_err(), "{bad:?}");
    }
    // Un sous-dossier n'est pas la racine : refusé plutôt que des chemins décalés.
    assert!(changes::collect(&repo.join("src"), "HEAD").is_err());
    let plain = tempfile::tempdir().expect("tempdir");
    assert!(changes::collect(plain.path(), "HEAD").is_err());
    assert!(changes::collect(&repo.join("absent"), "HEAD").is_err());
}

#[test]
fn repo_content_reads_base_and_current_versions() {
    let (dir, base) = fixture_repo();
    let repo = dir.path();
    write(repo, "src/lib.rs", "macro_rules! m { () => {} }\n");
    let source = RepoContent::new(repo, &base);
    assert_eq!(
        source.read("src/lib.rs", Version::Base).as_deref(),
        Some("pub fn f() {}\n")
    );
    assert_eq!(
        source.read("src/lib.rs", Version::Current).as_deref(),
        Some("macro_rules! m { () => {} }\n")
    );
    assert_eq!(source.read("src/absent.rs", Version::Base), None);
    assert_eq!(source.read("src/absent.rs", Version::Current), None);
    assert_eq!(source.read("../outside", Version::Current), None);
    // De bout en bout : le macro ajouté dans l'arbre de travail est vu.
    let set = changes::collect(repo, &base).expect("collect");
    let mut inventory = base_inventory();
    inventory.commit.clone_from(&set.base);
    let scope = classify(&set, &inventory, &graph(), &context(), &source);
    assert!(
        reasons(&scope).contains(&Reason::Macro("src/lib.rs".to_owned())),
        "{scope:?}"
    );
}

// ---------------------------------------------------------------------------
// base : parseurs sur fixtures, JUnit, load/save
// ---------------------------------------------------------------------------

fn inventory_from_fixtures() -> BaseInventory {
    let meta = parse_metadata(METADATA).expect("metadata");
    let binaries = parse_nextest_list(NEXTEST_LIST, &meta).expect("list");
    BaseInventory {
        schema: BASE_SCHEMA.to_owned(),
        commit: COMMIT.to_owned(),
        toolchain: toolchain(),
        features: Vec::new(),
        build_env: BTreeMap::new(),
        green: false,
        binaries,
        graph_digest: None,
    }
}

fn status_of(inv: &BaseInventory, binary_id: &str, name: &str) -> (BaseStatus, Option<u64>) {
    let test = inv
        .binaries
        .iter()
        .find(|b| b.binary_id == binary_id)
        .and_then(|b| b.tests.iter().find(|t| t.name == name))
        .unwrap_or_else(|| panic!("{binary_id} {name} introuvable"));
    (test.status, test.duration_ms)
}

#[test]
fn cargo_metadata_fixture_is_parsed_with_relative_sources() {
    let meta = parse_metadata(METADATA).expect("metadata");
    assert_eq!(meta.workspace_root, "/home/node/work/kioku-optA-m2");
    let found: BTreeSet<(&str, &str, &str)> = meta
        .targets
        .iter()
        .map(|t| (t.kind.as_str(), t.name.as_str(), t.src_path.as_str()))
        .collect();
    assert_eq!(
        found,
        BTreeSet::from([
            ("lib", "kioku", "src/lib.rs"),
            ("bin", "kioku", "src/main.rs"),
            ("test", "cli", "tests/cli.rs"),
            ("test", "v2_r14_schema", "tests/v2_r14_schema.rs"),
            (
                "example",
                "ci2_resolve_cost",
                "examples/ci2_resolve_cost.rs"
            ),
        ])
    );
    assert!(parse_metadata("not json").is_err());
    let outside = METADATA.replace(
        "/home/node/work/kioku-optA-m2/src/lib.rs",
        "/elsewhere/src/lib.rs",
    );
    assert!(parse_metadata(&outside).is_err());
}

#[test]
fn nextest_list_fixture_gives_binaries_tests_and_ignored_flags() {
    let inv = inventory_from_fixtures();
    let ids: Vec<&str> = inv.binaries.iter().map(|b| b.binary_id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "kdw-kioku",
            "kdw-kioku::bin/kioku",
            "kdw-kioku::cli",
            "kdw-kioku::v2_r14_schema"
        ]
    );
    let kinds: Vec<BinaryKind> = inv.binaries.iter().map(|b| b.kind).collect();
    assert_eq!(
        kinds,
        [
            BinaryKind::Lib,
            BinaryKind::Bin,
            BinaryKind::Test,
            BinaryKind::Test
        ]
    );
    let sources: Vec<&str> = inv.binaries.iter().map(|b| b.src_path.as_str()).collect();
    assert_eq!(
        sources,
        [
            "src/lib.rs",
            "src/main.rs",
            "tests/cli.rs",
            "tests/v2_r14_schema.rs"
        ]
    );
    let total: usize = inv.binaries.iter().map(|b| b.tests.len()).sum();
    assert_eq!(total, 7);
    assert_eq!(
        status_of(&inv, "kdw-kioku", LIB_IGNORED).0,
        BaseStatus::Ignored
    );
    assert_eq!(
        status_of(&inv, "kdw-kioku", LIB_TEST).0,
        BaseStatus::Unknown
    );
    assert_eq!(inv.binaries[1].target_name, "kioku");
}

#[test]
fn nextest_list_refuses_inconsistent_documents() {
    let meta = parse_metadata(METADATA).expect("metadata");
    assert!(parse_nextest_list("{}", &meta).is_err());
    // test-count qui ment.
    let wrong = NEXTEST_LIST.replace("\"test-count\": 7", "\"test-count\": 8");
    assert_ne!(wrong, NEXTEST_LIST);
    assert!(parse_nextest_list(&wrong, &meta).is_err());
    // Binaire absent de cargo metadata.
    let renamed = NEXTEST_LIST.replace("\"binary-name\": \"cli\"", "\"binary-name\": \"nope\"");
    assert_ne!(renamed, NEXTEST_LIST);
    assert!(parse_nextest_list(&renamed, &meta).is_err());
    // Binaire non listé.
    let skipped = NEXTEST_LIST.replacen("\"status\": \"listed\"", "\"status\": \"skipped\"", 1);
    assert!(parse_nextest_list(&skipped, &meta).is_err());
    // Clé différente de binary-id.
    let key = NEXTEST_LIST.replacen(
        "\"binary-id\": \"kdw-kioku::cli\"",
        "\"binary-id\": \"x\"",
        1,
    );
    assert_ne!(key, NEXTEST_LIST);
    assert!(parse_nextest_list(&key, &meta).is_err());
}

#[test]
fn junit_green_fixture_sets_statuses_durations_and_green() {
    let mut inv = inventory_from_fixtures();
    assert!(!inv.green);
    apply_junit(&mut inv, JUNIT_GREEN).expect("junit");
    assert!(inv.green);
    assert_eq!(
        status_of(&inv, "kdw-kioku::bin/kioku", BIN_TEST),
        (BaseStatus::Pass, Some(3))
    );
    assert_eq!(
        status_of(&inv, "kdw-kioku::cli", CLI_TEST),
        (BaseStatus::Pass, Some(80))
    );
    assert_eq!(
        status_of(&inv, "kdw-kioku::v2_r14_schema", SCHEMA_TEST),
        (BaseStatus::Pass, Some(4))
    );
    // Les tests `ignored` absents du JUnit restent Ignored et ne cassent pas `green`.
    assert_eq!(
        status_of(&inv, "kdw-kioku", LIB_IGNORED).0,
        BaseStatus::Ignored
    );
}

#[test]
fn junit_mixed_fixture_maps_fail_flaky_skipped_pass_and_absent() {
    let mut inv = inventory_from_fixtures();
    apply_junit(&mut inv, JUNIT_MIXED).expect("junit");
    assert_eq!(
        status_of(&inv, "kdw-kioku", LIB_TEST),
        (BaseStatus::Fail, Some(4))
    );
    assert_eq!(
        status_of(&inv, "kdw-kioku::cli", CLI_TEST),
        (BaseStatus::Flaky, Some(1250))
    );
    assert_eq!(
        status_of(&inv, "kdw-kioku::bin/kioku", BIN_TEST),
        (BaseStatus::Ignored, Some(0))
    );
    assert_eq!(
        status_of(
            &inv,
            "kdw-kioku::v2_r14_schema",
            "origin_claim_variant_refuses_an_undeclared_field"
        )
        .0,
        BaseStatus::Pass
    );
    // Listé mais absent du JUnit : Unknown.
    assert_eq!(
        status_of(&inv, "kdw-kioku::v2_r14_schema", SCHEMA_TEST).0,
        BaseStatus::Unknown
    );
    assert!(!inv.green);
}

fn junit_for_all_listed(lib: &str, cli: &str) -> String {
    let case = |suite: &str, name: &str, inner: &str| {
        format!("<testcase name=\"{name}\" classname=\"{suite}\" time=\"0.010\">{inner}</testcase>")
    };
    format!(
        "<?xml version=\"1.0\"?><testsuites>\
         <testsuite name=\"kdw-kioku\">{}</testsuite>\
         <testsuite name=\"kdw-kioku::bin/kioku\">{}</testsuite>\
         <testsuite name=\"kdw-kioku::cli\">{}</testsuite>\
         <testsuite name=\"kdw-kioku::v2_r14_schema\">{}{}</testsuite></testsuites>",
        case("kdw-kioku", LIB_TEST, lib),
        case("kdw-kioku::bin/kioku", BIN_TEST, ""),
        case("kdw-kioku::cli", CLI_TEST, cli),
        case("kdw-kioku::v2_r14_schema", SCHEMA_TEST, ""),
        case(
            "kdw-kioku::v2_r14_schema",
            "origin_claim_variant_refuses_an_undeclared_field",
            ""
        ),
    )
}

#[test]
fn junit_flaky_still_counts_green_but_error_and_unknown_do_not() {
    let flaky = "<flakyFailure message=\"m\" type=\"t\"/>";
    let mut inv = inventory_from_fixtures();
    apply_junit(
        &mut inv,
        &junit_for_all_listed(flaky, "<rerunFailure message=\"x\"/>"),
    )
    .expect("junit");
    assert_eq!(status_of(&inv, "kdw-kioku", LIB_TEST).0, BaseStatus::Flaky);
    assert_eq!(
        status_of(&inv, "kdw-kioku::cli", CLI_TEST).0,
        BaseStatus::Flaky
    );
    assert!(
        inv.green,
        "des tests instables ne rendent pas la base rouge"
    );
    let mut inv = inventory_from_fixtures();
    apply_junit(
        &mut inv,
        &junit_for_all_listed("<error message=\"boom\"/>", ""),
    )
    .expect("junit");
    assert_eq!(status_of(&inv, "kdw-kioku", LIB_TEST).0, BaseStatus::Fail);
    assert!(!inv.green);
    // Un échec final reste un échec même avec des relances.
    let mut inv = inventory_from_fixtures();
    apply_junit(
        &mut inv,
        &junit_for_all_listed("<failure message=\"f\"/><rerunFailure message=\"r\"/>", ""),
    )
    .expect("junit");
    assert_eq!(status_of(&inv, "kdw-kioku", LIB_TEST).0, BaseStatus::Fail);
}

#[test]
fn junit_refuses_malformed_or_ambiguous_documents_and_leaves_the_inventory_untouched() {
    let ok = junit_for_all_listed("", "");
    let cases: Vec<(&str, String)> = vec![
        ("vide", String::new()),
        ("pas du XML", "not xml".to_owned()),
        ("tronqué", ok[..ok.len() - 20].to_owned()),
        (
            "racine inattendue",
            "<testsuite name=\"kdw-kioku\"/>".to_owned(),
        ),
        ("deux racines", format!("{ok}{ok}")),
        ("DOCTYPE", format!("<!DOCTYPE x [<!ENTITY a \"b\">]>{ok}")),
        (
            "entité inconnue",
            ok.replace("time=\"0.010\"", "time=\"&bogus;\""),
        ),
        (
            "durée illisible",
            ok.replace("time=\"0.010\"", "time=\"1e3\""),
        ),
        (
            "durée négative",
            ok.replace("time=\"0.010\"", "time=\"-1\""),
        ),
        (
            "attribut en double",
            ok.replacen(
                "classname=\"kdw-kioku\"",
                "classname=\"a\" classname=\"b\"",
                1,
            ),
        ),
        (
            "balise inconnue",
            ok.replacen("</testcase>", "<surprise/></testcase>", 1),
        ),
        (
            "fermeture croisée",
            ok.replacen("</testsuite>", "</testsuites>", 1),
        ),
        (
            "skipped et failure",
            junit_for_all_listed("<skipped/><failure message=\"x\"/>", ""),
        ),
        (
            "test en double",
            ok.replacen(
                "</testsuite>",
                &format!("<testcase name=\"{LIB_TEST}\" time=\"1\"/></testsuite>"),
                1,
            ),
        ),
        (
            "suite en double",
            ok.replacen(
                "</testsuites>",
                "<testsuite name=\"kdw-kioku\"/></testsuites>",
                1,
            ),
        ),
        (
            "test inconnu de l'inventaire",
            ok.replacen(
                "</testsuite>",
                "<testcase name=\"ghost\" time=\"1\"/></testsuite>",
                1,
            ),
        ),
        (
            "suite inconnue de l'inventaire",
            ok.replacen(
                "</testsuites>",
                "<testsuite name=\"ghost\"><testcase name=\"x\"/></testsuite></testsuites>",
                1,
            ),
        ),
        (
            "testcase sans nom",
            ok.replacen(&format!("name=\"{LIB_TEST}\""), "", 1),
        ),
    ];
    for (label, xml) in cases {
        let mut inv = inventory_from_fixtures();
        let before = inv.clone();
        assert!(
            apply_junit(&mut inv, &xml).is_err(),
            "devrait être refusé : {label}"
        );
        assert_eq!(inv, before, "inventaire modifié malgré l'erreur : {label}");
    }
    // Le document sain passe, lui.
    let mut inv = inventory_from_fixtures();
    apply_junit(&mut inv, &ok).expect("sain");
    assert!(inv.green);
}

#[test]
fn junit_decodes_entities_and_skips_comments_and_cdata() {
    let mut inv = inventory_from_fixtures();
    inv.binaries[0].tests[0].name = "a<b&\"c\"".to_owned();
    let xml = "<?xml version=\"1.0\"?><!-- c --><testsuites><testsuite name='kdw-kioku'>\
               <testcase name=\"a&lt;b&amp;&quot;c&quot;\" time=\"2.5\"><system-out><![CDATA[<x> & </testcase>]]></system-out></testcase>\
               </testsuite></testsuites>";
    let target = inv.binaries[0].tests[0].name.clone();
    apply_junit(&mut inv, xml).expect("junit");
    assert_eq!(
        status_of(&inv, "kdw-kioku", &target),
        (BaseStatus::Pass, Some(2500))
    );
}

#[test]
fn empty_inventory_is_never_green() {
    let mut inv = inventory_from_fixtures();
    inv.binaries.clear();
    apply_junit(&mut inv, "<testsuites/>").expect("junit vide");
    assert!(!inv.green);
}

#[test]
fn load_and_save_round_trip_and_refuse_invalid_inventories() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("base.json");
    let mut inv = inventory_from_fixtures();
    apply_junit(&mut inv, JUNIT_GREEN).expect("junit");
    inv.graph_digest = Some("g1".to_owned());
    base::save(&path, &inv).expect("save");
    assert_eq!(base::load(&path).expect("load"), inv);
    assert!(
        std::fs::read_dir(dir.path()).expect("ls").count() == 1,
        "pas de fichier temporaire"
    );

    let text = std::fs::read_to_string(&path).expect("read");
    let mutations: Vec<(&str, String)> = vec![
        ("schéma", text.replace(BASE_SCHEMA, "kioku/select/base/v0")),
        ("commit court", text.replace(COMMIT, "0123abc")),
        (
            "commit majuscules",
            text.replace(COMMIT, &COMMIT.to_uppercase()),
        ),
        (
            "commit non hexadécimal",
            text.replace(COMMIT, &"g".repeat(40)),
        ),
        (
            "binaire en double",
            text.replacen(
                "\"binary_id\": \"kdw-kioku::cli\"",
                "\"binary_id\": \"kdw-kioku\"",
                1,
            ),
        ),
        ("JSON invalide", "{".to_owned()),
        (
            "chemin source absolu",
            text.replacen("src/lib.rs", "/etc/passwd", 1),
        ),
    ];
    for (label, mutated) in mutations {
        assert_ne!(mutated, text, "mutation sans effet : {label}");
        std::fs::write(&path, mutated).expect("write");
        let error = base::load(&path).expect_err(label);
        assert!(
            matches!(error, SelectError::Invalid { .. }),
            "{label} : {error:?}"
        );
    }
    assert!(matches!(
        base::load(&dir.path().join("missing.json")),
        Err(SelectError::Io { .. })
    ));
    // `save` refuse d'écrire un inventaire invalide.
    let mut bad = inv.clone();
    bad.schema = "x".to_owned();
    assert!(base::save(&dir.path().join("bad.json"), &bad).is_err());
    assert!(!dir.path().join("bad.json").exists());
}

// --- A4 : scellé d'intégrité de l'inventaire -------------------------------------------------

fn sealed_inventory_document(dir: &Path) -> (std::path::PathBuf, serde_json::Value) {
    let path = dir.join("base.json");
    let mut inv = inventory_from_fixtures();
    apply_junit(&mut inv, JUNIT_GREEN).expect("junit");
    inv.graph_digest = Some("g1".to_owned());
    base::save(&path, &inv).expect("save");
    let doc = serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("json");
    (path, doc)
}

#[test]
fn sealed_inventory_carries_counts_and_a_digest() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (_, doc) = sealed_inventory_document(dir.path());
    assert_eq!(doc["schema"], "kioku/select/base/v2");
    let binaries = doc["binaries"].as_array().expect("binaries");
    assert_eq!(doc["binary_count"], binaries.len());
    let tests: usize = binaries
        .iter()
        .map(|b| b["tests"].as_array().expect("tests").len())
        .sum();
    assert!(tests > 0);
    assert_eq!(doc["test_count"], tests);
    let digest = doc["content_digest"].as_str().expect("digest");
    assert!(
        digest.starts_with("sha256:") && digest.len() == 7 + 64,
        "{digest}"
    );
}

#[test]
fn every_alteration_of_a_sealed_inventory_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (path, good) = sealed_inventory_document(dir.path());
    let edit = |label: &str, change: &dyn Fn(&mut serde_json::Value)| {
        let mut doc = good.clone();
        change(&mut doc);
        assert_ne!(doc, good, "mutation sans effet : {label}");
        std::fs::write(&path, serde_json::to_vec_pretty(&doc).expect("json")).expect("write");
        match base::load(&path) {
            Err(SelectError::Invalid { message, .. }) => assert!(
                message.contains("inventaire de la base incohérent"),
                "{label} : {message}"
            ),
            other => panic!("{label} : refus attendu, obtenu {other:?}"),
        }
    };
    edit("un test retiré", &|d| {
        d["binaries"][0]["tests"]
            .as_array_mut()
            .expect("tests")
            .pop();
    });
    edit("un binaire retiré", &|d| {
        d["binaries"].as_array_mut().expect("binaries").pop();
    });
    edit("un statut changé", &|d| {
        let status = &mut d["binaries"][0]["tests"][0]["status"];
        *status = if *status == "fail" { "pass" } else { "fail" }.into();
    });
    edit("une durée changée", &|d| {
        d["binaries"][0]["tests"][0]["duration_ms"] = 123_456.into();
    });
    edit("un binaire renommé", &|d| {
        d["binaries"][0]["target_name"] = "autre".into();
    });
    edit("green changé", &|d| {
        d["green"] = (!d["green"].as_bool().expect("green")).into();
    });
    edit("graph_digest changé", &|d| d["graph_digest"] = "g2".into());
    edit("toolchain changée", &|d| d["toolchain"]["x"] = "1".into());
    edit("binary_count changé", &|d| d["binary_count"] = 99.into());
    edit("test_count changé", &|d| d["test_count"] = 99.into());
    edit("binaires vidés et binary_count/test_count à 0", &|d| {
        d["binaries"] = serde_json::json!([]);
        d["binary_count"] = 0.into();
        d["test_count"] = 0.into();
    });
    edit("binary_count absent", &|d| {
        d.as_object_mut().expect("object").remove("binary_count");
    });
    edit("test_count absent", &|d| {
        d.as_object_mut().expect("object").remove("test_count");
    });
    edit("content_digest absent", &|d| {
        d.as_object_mut().expect("object").remove("content_digest");
    });
    edit("content_digest changé", &|d| {
        d["content_digest"] = format!("sha256:{}", "0".repeat(64)).into();
    });
    // Ancien schéma v1 : refusé par le schéma.
    let mut v1 = good.clone();
    v1["schema"] = "kioku/select/base/v1".into();
    std::fs::write(&path, serde_json::to_vec_pretty(&v1).expect("json")).expect("write");
    assert!(matches!(
        base::load(&path),
        Err(SelectError::Invalid { .. })
    ));
    // Intact : toujours lisible.
    std::fs::write(&path, serde_json::to_vec_pretty(&good).expect("json")).expect("write");
    base::load(&path).expect("intact");
}

#[test]
fn build_env_is_filtered_and_hashed_never_in_clear() {
    use std::ffi::OsString;
    let vars = [
        ("RUSTFLAGS", "-Dwarnings --secret-token=abc123"),
        ("CARGO_PROFILE_DEV_DEBUG", "0"),
        ("CARGO_BUILD_TARGET", "aarch64-unknown-linux-gnu"),
        ("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER", "clang"),
        ("RUSTC_WRAPPER", "sccache"),
        ("CARGO_TARGET_DIR", "/tmp/elsewhere"),
        ("CARGO_BUILD_TARGET_DIR", "/tmp/elsewhere"),
        ("HOME", "/home/node"),
        ("SECRET_API_KEY", "must-not-appear"),
    ]
    .map(|(k, v)| (OsString::from(k), OsString::from(v)));
    let hashed = hash_build_env(vars.clone());
    let names: Vec<&str> = hashed.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        [
            "CARGO_BUILD_TARGET",
            "CARGO_PROFILE_DEV_DEBUG",
            "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER",
            "RUSTC_WRAPPER",
            "RUSTFLAGS",
        ]
    );
    let dump = format!("{hashed:?}");
    for (_, value) in &vars {
        let value = value.to_str().expect("utf8");
        // Les valeurs courtes () se confondraient avec des chiffres hexadécimaux.
        if value.len() > 3 {
            assert!(!dump.contains(value), "valeur en clair : {value}");
        }
    }
    assert!(
        hashed
            .values()
            .all(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
    );
    // Même valeur, même hash ; autre valeur, autre hash.
    let again = hash_build_env([(
        OsString::from("RUSTFLAGS"),
        OsString::from("-Dwarnings --secret-token=abc123"),
    )]);
    assert_eq!(again["RUSTFLAGS"], hashed["RUSTFLAGS"]);
    let other = hash_build_env([(OsString::from("RUSTFLAGS"), OsString::from("-Wall"))]);
    assert_ne!(other["RUSTFLAGS"], hashed["RUSTFLAGS"]);
    assert!(is_build_env_name("CARGO_ENCODED_RUSTFLAGS") && !is_build_env_name("PATH"));
}

#[test]
fn s3_collect_refuses_bad_commit_head_mismatch_dirty_tree_and_bad_features() {
    let (dir, base_commit) = fixture_repo();
    let repo = dir.path();
    let head = git(repo, &["rev-parse", "HEAD"]);
    assert_ne!(head, base_commit);
    for bad in ["", "abc", &head.to_uppercase(), &format!("{head}0")] {
        assert!(base::collect(repo, bad, &[]).is_err(), "{bad:?}");
    }
    // HEAD n'est pas le commit annoncé.
    let error = base::collect(repo, &base_commit, &[]).expect_err("HEAD différent");
    assert!(matches!(error, SelectError::Invalid { .. }), "{error:?}");
    // Arbre suivi modifié.
    write(repo, "src/lib.rs", "pub fn changed() {}\n");
    let error = base::collect(repo, &head, &[]).expect_err("arbre modifié");
    assert!(matches!(error, SelectError::Invalid { .. }), "{error:?}");
    git(repo, &["checkout", "--", "src/lib.rs"]);
    // Nom de feature qui tenterait d'injecter une option.
    for feature in ["", "a,b", "--all-features", "a b", "x;y"] {
        assert!(
            base::collect(repo, &head, &[feature.to_owned()]).is_err(),
            "{feature:?}"
        );
    }
    // Pas un dépôt Cargo : metadata échoue, donc erreur (jamais d'inventaire partiel).
    assert!(base::collect(repo, &head, &[]).is_err());
    assert!(base::collect(&repo.join("absent"), &head, &[]).is_err());
}

// ---------------------------------------------------------------------------
// Essais sur le vrai dépôt (lents) : `--run-ignored only`
// ---------------------------------------------------------------------------

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

#[test]
#[ignore = "essai réel : compile les binaires de test (cargo nextest list)"]
fn real_collect_on_head_matches_nextest_list() {
    let repo = manifest_dir();
    let head = git(repo, &["rev-parse", "HEAD"]);
    let inv = base::collect(repo, &head, &[]).expect("collect");
    assert_eq!(inv.commit, head);
    assert!(!inv.green, "aucun résultat n'est connu avant le JUnit");
    assert!(inv.graph_digest.is_none());
    for key in ["rustc", "cargo", "nextest"] {
        assert!(
            inv.toolchain.get(key).is_some_and(|v| !v.is_empty()),
            "{key}"
        );
    }
    let output = Command::new("cargo")
        .args(["nextest", "list", "--message-format", "json", "--locked"])
        .current_dir(repo)
        .output()
        .expect("nextest list");
    assert!(output.status.success());
    let reference: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    let suites = reference["rust-suites"].as_object().expect("suites");
    assert_eq!(inv.binaries.len(), suites.len());
    let expected_tests = reference["test-count"].as_u64().expect("count");
    let tests: usize = inv.binaries.iter().map(|b| b.tests.len()).sum();
    assert_eq!(tests as u64, expected_tests);
    for binary in &inv.binaries {
        assert_eq!(
            binary.tests.len(),
            suites[binary.binary_id.as_str()]["testcases"]
                .as_object()
                .expect("cases")
                .len()
        );
        assert!(repo.join(&binary.src_path).is_file(), "{}", binary.src_path);
    }
}

#[test]
#[ignore = "essai réel : modifie temporairement des fichiers du clone"]
fn real_changes_collect_after_touch_and_edit() {
    let repo = manifest_dir();
    let head = git(repo, &["rev-parse", "HEAD"]);
    let tracked = "tests/fixtures/select/l1/junit-green.xml";
    let probe = "tests/fixtures/select/l1/.changes-probe.txt";
    // Sur un arbre propre : aucun changement.
    let clean = changes::collect(repo, &head).expect("collect propre");
    assert!(clean.changes.is_empty(), "{:?}", clean.changes);
    // `touch` seul (contenu identique) : toujours aucun changement.
    let original = std::fs::read(repo.join(tracked)).expect("read");
    std::fs::write(repo.join(tracked), &original).expect("touch");
    let touched = changes::collect(repo, &head);
    // Modification réelle + fichier non suivi.
    let mut edited = original.clone();
    edited.extend_from_slice(b"<!-- essai -->\n");
    std::fs::write(repo.join(tracked), &edited).expect("edit");
    std::fs::write(repo.join(probe), "probe\n").expect("write");
    let result = changes::collect(repo, &head);
    std::fs::write(repo.join(tracked), &original).expect("restore");
    std::fs::remove_file(repo.join(probe)).expect("cleanup");
    let touched = touched.expect("collect après touch");
    assert!(touched.changes.is_empty(), "{:?}", touched.changes);
    let set = result.expect("collect");
    assert_eq!(set.base, head);
    assert_eq!(set.changes, vec![added(probe), modified(tracked)]);
}
