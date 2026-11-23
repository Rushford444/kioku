//! Lot L4a : plan de sélection (`plan`, `nextest_args`) et d1 (S5).
//!
//! Les cas unitaires utilisent de petits graphes écrits à la main ; la preuve sur kioku
//! charge la fixture L3 (`kioku-941cec0-*`, graphe SCIP + `#[path]`, couverture mesurée) et
//! y applique les arêtes apprises (`learn::apply`). Le filtre généré est aussi soumis au vrai
//! `cargo nextest list` sur la mini-crate de la fixture L3.

// Les cas de test nomment les tests d'un petit dépôt a, b, c, x, y… (noms à une lettre voulus) ;
// le test nextest réel est une seule histoire séquentielle (copie, inventaire, six plans).
#![allow(
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::too_many_lines
)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use kioku::select::classify::reach;
use kioku::select::journal::{Journal, JournalCounts, JournalScope, JournalStatus, check_complete};
use kioku::select::learn::{apply, coverage_from_json, learn};
use kioku::select::plan::{nextest_args, plan};
use kioku::select::{
    BASE_SCHEMA, BaseBinary, BaseInventory, BaseStatus, BaseTest, BinaryKind, D1Proposal, Decision,
    GRAPH_SCHEMA, GraphData, JOURNAL_SCHEMA, NextestTarget, PartialScope, Plan, Reason, RelPath,
    Scope, TestId,
};

// ---------------------------------------------------------------------------------------
// Aides
// ---------------------------------------------------------------------------------------

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

fn tid(binary: &str, name: &str) -> TestId {
    TestId {
        binary_id: binary.to_owned(),
        name: name.to_owned(),
    }
}

fn set(items: &[&str]) -> BTreeSet<RelPath> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

fn binary(id: &str, target: &str, kind: BinaryKind, tests: &[&str]) -> BaseBinary {
    BaseBinary {
        binary_id: id.to_owned(),
        target_name: target.to_owned(),
        kind,
        src_path: format!("src/{target}.rs"),
        tests: tests
            .iter()
            .map(|n| BaseTest {
                name: (*n).to_owned(),
                status: BaseStatus::Pass,
                duration_ms: None,
            })
            .collect(),
    }
}

fn base(binaries: Vec<BaseBinary>) -> BaseInventory {
    BaseInventory {
        schema: BASE_SCHEMA.to_owned(),
        commit: COMMIT.to_owned(),
        toolchain: BTreeMap::new(),
        features: Vec::new(),
        build_env: BTreeMap::new(),
        green: true,
        binaries,
        graph_digest: Some("digest-g".to_owned()),
    }
}

/// `edges`: `a → [b…]` ; `roots`: test → fichiers racines.
fn graph(edges: &[(&str, &[&str])], roots: &[(&TestId, &[&str])]) -> GraphData {
    GraphData {
        schema: GRAPH_SCHEMA.to_owned(),
        digest: "digest-g".to_owned(),
        edges: edges
            .iter()
            .map(|(a, bs)| ((*a).to_owned(), set(bs)))
            .collect(),
        test_roots: roots.iter().map(|(t, r)| ((*t).clone(), set(r))).collect(),
        ..GraphData::default()
    }
}

/// Fichiers compilés dans chaque binaire (dep-info) : sans eux, un binaire est un doute et
/// devient cible dès qu'un fichier est modifié (cibles propriétaires, A5).
fn with_binary_files(mut g: GraphData, files: &[(&str, &[&str])]) -> GraphData {
    for (binary, list) in files {
        g.binary_files.insert((*binary).to_owned(), set(list));
    }
    g
}

fn partial(modified: &[&str]) -> PartialScope {
    PartialScope {
        modified: set(modified),
        ..PartialScope::default()
    }
}

fn run_set(plan: &Plan) -> BTreeSet<TestId> {
    plan.entries
        .iter()
        .filter(|e| e.decision == Decision::Run)
        .map(|e| e.test.clone())
        .collect()
}

fn skip_set(plan: &Plan) -> BTreeSet<TestId> {
    plan.entries
        .iter()
        .filter(|e| e.decision == Decision::Skip)
        .map(|e| e.test.clone())
        .collect()
}

fn entry<'a>(plan: &'a Plan, binary: &str, name: &str) -> &'a kioku::select::JournalEntry {
    plan.entries
        .iter()
        .find(|e| e.test == tid(binary, name))
        .unwrap_or_else(|| panic!("no entry for {binary} {name}"))
}

fn inventory_ids(base: &BaseInventory) -> BTreeSet<TestId> {
    base.binaries
        .iter()
        .flat_map(|b| b.tests.iter().map(|t| tid(&b.binary_id, &t.name)))
        .collect()
}

/// Journal `planned` construit depuis le plan : doit être accepté par `check_complete`.
fn assert_journal_complete(plan: &Plan, base: &BaseInventory) {
    let scope = match &plan.scope {
        Scope::Everything { .. } => JournalScope::Everything,
        Scope::Partial(_) => JournalScope::Partial,
    };
    let scope_reasons = match &plan.scope {
        Scope::Everything { reasons } => reasons.clone(),
        Scope::Partial(_) => Vec::new(),
    };
    let journal = Journal {
        schema: JOURNAL_SCHEMA.to_owned(),
        status: JournalStatus::Planned,
        base_commit: plan.base_commit.clone(),
        graph_digest: plan.graph_digest.clone(),
        modified_files: plan.modified_files.clone(),
        scope,
        scope_reasons,
        counts: JournalCounts::from_entries(&plan.entries, inventory_ids(base).len(), 0),
        entries: plan.entries.clone(),
        outcomes: Vec::new(),
        user_filter: None,
    };
    check_complete(&journal, &inventory_ids(base), &BTreeSet::new())
        .unwrap_or_else(|e| panic!("journal from plan.entries rejected: {e}"));
}

/// Petit dépôt : lib (a, b, c), bin kioku (m), test `it` (x, y), exemple `ex` (e).
fn small_base() -> (BaseInventory, GraphData, [TestId; 8]) {
    let [a, b, c, m, x, y, e, z] = [
        tid("pkg", "mod_a::tests::a"),
        tid("pkg", "mod_b::tests::b"),
        tid("pkg", "mod_c::tests::c"),
        tid("pkg::bin/kioku", "m"),
        tid("pkg::it", "x"),
        tid("pkg::it", "y"),
        tid("pkg::example/ex", "e"),
        tid("pkg::it", "z"),
    ];
    let base = base(vec![
        binary(
            "pkg",
            "pkg",
            BinaryKind::Lib,
            &["mod_a::tests::a", "mod_b::tests::b", "mod_c::tests::c"],
        ),
        binary("pkg::bin/kioku", "kioku", BinaryKind::Bin, &["m"]),
        binary("pkg::it", "it", BinaryKind::Test, &["x", "y", "z"]),
        binary("pkg::example/ex", "ex", BinaryKind::Other, &["e"]),
    ]);
    let g = graph(
        &[
            ("src/mod_a.rs", &["src/util.rs"]),
            ("src/mod_b.rs", &["src/util.rs", "src/deep.rs"]),
            ("src/util.rs", &["src/deep.rs"]),
            ("tests/it.rs", &["src/mod_a.rs"]),
        ],
        &[
            (&a, &["src/mod_a.rs"]),
            (&b, &["src/mod_b.rs"]),
            (&c, &["src/mod_c.rs"]),
            (&m, &["src/main.rs"]),
            (&x, &["tests/it.rs"]),
            (&y, &["tests/it.rs"]),
            (&z, &["tests/z.rs"]),
            (&e, &["examples/ex.rs"]),
        ],
    );
    let g = with_binary_files(
        g,
        &[
            (
                "pkg",
                &[
                    "src/lib.rs",
                    "src/mod_a.rs",
                    "src/mod_b.rs",
                    "src/mod_c.rs",
                    "src/util.rs",
                    "src/deep.rs",
                ],
            ),
            ("pkg::bin/kioku", &["src/main.rs"]),
            ("pkg::it", &["tests/it.rs", "tests/z.rs"]),
            ("pkg::example/ex", &["examples/ex.rs"]),
        ],
    );
    (base, g, [a, b, c, m, x, y, e, z])
}

// ---------------------------------------------------------------------------------------
// Portée « tout »
// ---------------------------------------------------------------------------------------

#[test]
fn everything_runs_every_test_with_the_scope_reasons() {
    let (base, g, _) = small_base();
    let reasons = vec![Reason::BuildInput("Cargo.toml".to_owned())];
    let scope = Scope::Everything {
        reasons: reasons.clone(),
    };
    let d1 = D1Proposal {
        add: vec![tid("pkg", "mod_a::tests::a")],
        skip: vec![tid("pkg", "mod_b::tests::b")],
    };
    for d1 in [None, Some(&d1)] {
        let p = plan(&scope, &base, &g, d1);
        assert_eq!(run_set(&p), inventory_ids(&base));
        assert!(skip_set(&p).is_empty());
        assert!(p.entries.iter().all(|e| e.reasons == reasons));
        assert_eq!(p.targets, None);
        assert_eq!(p.filter, None);
        assert_eq!(p.modified_files, Vec::<String>::new());
        assert_eq!(p.base_commit, COMMIT);
        assert_eq!(p.graph_digest, "digest-g");
        assert_eq!(nextest_args(&p, None), Vec::<String>::new());
        assert_journal_complete(&p, &base);
    }
}

#[test]
fn entries_are_sorted_one_per_test_and_never_empty_of_reasons() {
    let (base, g, _) = small_base();
    let p = plan(&Scope::Partial(partial(&["src/util.rs"])), &base, &g, None);
    let ids: Vec<&TestId> = p.entries.iter().map(|e| &e.test).collect();
    let mut sorted = ids.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(ids, sorted, "sorted, no duplicate");
    assert_eq!(
        p.entries
            .iter()
            .map(|e| e.test.clone())
            .collect::<BTreeSet<_>>(),
        inventory_ids(&base)
    );
    assert!(p.entries.iter().all(|e| !e.reasons.is_empty()));
    assert_journal_complete(&p, &base);
}

// ---------------------------------------------------------------------------------------
// Atteinte, via, arêtes apprises
// ---------------------------------------------------------------------------------------

#[test]
fn simple_reach_runs_the_reaching_tests_and_skips_the_others_with_the_graph_digest() {
    let (base, g, [a, b, c, m, x, y, e, z]) = small_base();
    let p = plan(&Scope::Partial(partial(&["src/util.rs"])), &base, &g, None);
    // mod_a et mod_b atteignent util.rs ; x et y y arrivent par tests/it.rs → mod_a.
    assert_eq!(
        run_set(&p),
        BTreeSet::from([a.clone(), b.clone(), x.clone(), y.clone()])
    );
    for t in [&c, &m, &e, &z] {
        let en = entry(&p, &t.binary_id, &t.name);
        assert_eq!(en.decision, Decision::Skip);
        assert_eq!(
            en.reasons,
            vec![Reason::NoPathToModified {
                graph_digest: "digest-g".to_owned()
            }]
        );
    }
    assert_eq!(p.modified_files, vec!["src/util.rs".to_owned()]);
    assert_journal_complete(&p, &base);
}

#[test]
fn a_modified_root_is_reached_with_a_one_file_via() {
    let (base, g, [a, ..]) = small_base();
    let p = plan(&Scope::Partial(partial(&["src/mod_a.rs"])), &base, &g, None);
    let en = entry(&p, &a.binary_id, &a.name);
    assert_eq!(
        en.reasons,
        vec![Reason::ReachesModified {
            file: "src/mod_a.rs".to_owned(),
            via: vec!["src/mod_a.rs".to_owned()],
        }]
    );
}

#[test]
fn via_is_the_shortest_path_and_one_reason_per_modified_file() {
    // r → p1 → p2 → p3 → f ; r → q → f : le plus court passe par q.
    let t = tid("pkg", "t");
    let base = base(vec![binary("pkg", "pkg", BinaryKind::Lib, &["t"])]);
    let g = graph(
        &[
            ("r.rs", &["p1.rs", "q.rs"]),
            ("p1.rs", &["p2.rs"]),
            ("p2.rs", &["p3.rs"]),
            ("p3.rs", &["f.rs"]),
            ("q.rs", &["f.rs"]),
            ("f.rs", &["g.rs"]),
        ],
        &[(&t, &["r.rs"])],
    );
    let p = plan(&Scope::Partial(partial(&["f.rs", "g.rs"])), &base, &g, None);
    let reasons = &entry(&p, "pkg", "t").reasons;
    assert_eq!(
        reasons,
        &vec![
            Reason::ReachesModified {
                file: "f.rs".to_owned(),
                via: vec!["r.rs".to_owned(), "q.rs".to_owned(), "f.rs".to_owned()],
            },
            Reason::ReachesModified {
                file: "g.rs".to_owned(),
                via: vec![
                    "r.rs".to_owned(),
                    "q.rs".to_owned(),
                    "f.rs".to_owned(),
                    "g.rs".to_owned()
                ],
            },
        ]
    );
    // Déterministe : même résultat à chaque appel.
    assert_eq!(
        p,
        plan(&Scope::Partial(partial(&["f.rs", "g.rs"])), &base, &g, None)
    );
}

#[test]
fn via_picks_the_nearest_of_several_roots() {
    let t = tid("pkg", "t");
    let base = base(vec![binary("pkg", "pkg", BinaryKind::Lib, &["t"])]);
    let g = graph(
        &[("far.rs", &["mid.rs"]), ("mid.rs", &["f.rs"])],
        &[(&t, &["far.rs", "f.rs"])],
    );
    let p = plan(&Scope::Partial(partial(&["f.rs"])), &base, &g, None);
    assert_eq!(
        entry(&p, "pkg", "t").reasons,
        vec![Reason::ReachesModified {
            file: "f.rs".to_owned(),
            via: vec!["f.rs".to_owned()],
        }]
    );
}

#[test]
fn a_learned_edge_adds_the_learned_reason() {
    let t = tid("pkg", "t");
    let u = tid("pkg", "u");
    let base = base(vec![binary("pkg", "pkg", BinaryKind::Lib, &["t", "u"])]);
    let mut g = graph(
        &[("a.rs", &["b.rs"]), ("b.rs", &["c.rs"])],
        &[(&t, &["a.rs"]), (&u, &["a.rs"])],
    );
    // Arête b → c apprise seulement ; a → b vient de SCIP.
    g.provenance
        .insert("a.rs -> b.rs".to_owned(), set(&["scip"]));
    g.provenance
        .insert("b.rs -> c.rs".to_owned(), set(&["learned"]));
    let p = plan(&Scope::Partial(partial(&["c.rs"])), &base, &g, None);
    for name in ["t", "u"] {
        let r = &entry(&p, "pkg", name).reasons;
        assert!(r.contains(&Reason::ReachesModified {
            file: "c.rs".to_owned(),
            via: vec!["a.rs".to_owned(), "b.rs".to_owned(), "c.rs".to_owned()],
        }));
        assert!(
            r.contains(&Reason::Learned {
                file: "c.rs".to_owned()
            }),
            "{name}: {r:?}"
        );
    }
    // Une arête de provenance mixte (scip + learned) n'est pas « seulement apprise ».
    g.provenance
        .insert("b.rs -> c.rs".to_owned(), set(&["scip", "learned"]));
    let p = plan(&Scope::Partial(partial(&["c.rs"])), &base, &g, None);
    assert!(
        !entry(&p, "pkg", "t")
            .reasons
            .iter()
            .any(|r| matches!(r, Reason::Learned { .. }))
    );
}

#[test]
fn a_learned_root_after_apply_adds_the_learned_reason() {
    let t = tid("pkg", "t");
    let base = base(vec![binary("pkg", "pkg", BinaryKind::Lib, &["t"])]);
    let mut g = graph(&[("a.rs", &[])], &[(&t, &["a.rs"])]);
    let coverage = BTreeMap::from([(t.clone(), set(&["a.rs", "hidden.rs"]))]);
    let learned = learn(&g, &coverage);
    apply(&mut g, &learned);
    let p = plan(&Scope::Partial(partial(&["hidden.rs"])), &base, &g, None);
    assert_eq!(
        entry(&p, "pkg", "t").reasons,
        vec![
            Reason::ReachesModified {
                file: "hidden.rs".to_owned(),
                via: vec!["hidden.rs".to_owned()],
            },
            Reason::Learned {
                file: "hidden.rs".to_owned()
            },
        ]
    );
}

#[test]
fn a_whole_binary_and_a_forced_test_are_run_with_their_reason() {
    let (base, g, [a, b, c, _m, x, y, _e, z]) = small_base();
    let mut scope = partial(&["src/mod_c.rs"]);
    scope.whole_binaries.insert(
        "pkg::it".to_owned(),
        Reason::NewBinary("pkg::it".to_owned()),
    );
    scope
        .forced_tests
        .insert(b.clone(), Reason::BaseNotGreen(BaseStatus::Flaky));
    scope
        .forced_tests
        .insert(a.clone(), Reason::ExternalInput("/etc/x".to_owned()));
    let p = plan(&Scope::Partial(scope), &base, &g, None);
    for t in [&x, &y, &z] {
        assert_eq!(
            entry(&p, &t.binary_id, &t.name).reasons,
            vec![Reason::NewBinary("pkg::it".to_owned())]
        );
    }
    assert_eq!(
        entry(&p, &b.binary_id, &b.name).reasons,
        vec![Reason::BaseNotGreen(BaseStatus::Flaky)]
    );
    assert_eq!(
        entry(&p, &a.binary_id, &a.name).reasons,
        vec![Reason::ExternalInput("/etc/x".to_owned())]
    );
    assert_eq!(entry(&p, &c.binary_id, &c.name).decision, Decision::Run);
    assert_eq!(p.targets.as_ref().map(Vec::len), Some(2));
}

#[test]
fn an_unknown_test_root_means_run() {
    let (base, mut g, [_a, _b, c, ..]) = small_base();
    g.test_roots.remove(&c);
    let p = plan(&Scope::Partial(partial(&["docs.rs"])), &base, &g, None);
    assert_eq!(
        entry(&p, &c.binary_id, &c.name).reasons,
        vec![Reason::Error(
            "racine du test inconnue dans le graphe".to_owned()
        )]
    );
    // Racines vides : même doute.
    g.test_roots.insert(c.clone(), BTreeSet::new());
    let p = plan(&Scope::Partial(partial(&["docs.rs"])), &base, &g, None);
    assert_eq!(entry(&p, &c.binary_id, &c.name).decision, Decision::Run);
}

#[test]
fn nothing_modified_skips_every_known_test() {
    let (base, g, _) = small_base();
    let p = plan(&Scope::Partial(partial(&[])), &base, &g, None);
    assert!(run_set(&p).is_empty());
    assert_eq!(p.targets, Some(Vec::new()));
    assert_eq!(p.filter, None);
    assert_eq!(nextest_args(&p, None), vec!["-E", "none()"]);
    assert_eq!(nextest_args(&p, Some("test(x)")), vec!["-E", "none()"]);
    assert_journal_complete(&p, &base);
}

// ---------------------------------------------------------------------------------------
// d1 (S5)
// ---------------------------------------------------------------------------------------

#[test]
fn d1_add_of_a_known_test_runs_it() {
    let (base, g, [_a, _b, c, ..]) = small_base();
    let scope = Scope::Partial(partial(&["src/mod_a.rs"]));
    let d1 = D1Proposal {
        add: vec![c.clone()],
        skip: vec![],
    };
    let p = plan(&scope, &base, &g, Some(&d1));
    assert_eq!(
        entry(&p, &c.binary_id, &c.name).reasons,
        vec![Reason::D1Added]
    );
    assert_eq!(entry(&p, &c.binary_id, &c.name).decision, Decision::Run);
    assert_journal_complete(&p, &base);
}

#[test]
fn d1_add_of_an_unknown_test_runs_its_whole_known_binary() {
    let (base, g, [_a, _b, _c, _m, x, y, e, z]) = small_base();
    let scope = Scope::Partial(partial(&["src/mod_c.rs"]));
    let d1 = D1Proposal {
        add: vec![tid("pkg::it", "brand_new_test")],
        skip: vec![],
    };
    let without = plan(&scope, &base, &g, None);
    assert_eq!(skip_set(&without).len(), 7, "x, y, z skipped without d1");
    let p = plan(&scope, &base, &g, Some(&d1));
    for t in [&x, &y, &z] {
        let en = entry(&p, &t.binary_id, &t.name);
        assert_eq!(en.decision, Decision::Run);
        assert_eq!(en.reasons, vec![Reason::D1Added]);
    }
    assert_eq!(entry(&p, &e.binary_id, &e.name).decision, Decision::Skip);
    assert_journal_complete(&p, &base);
}

#[test]
fn d1_add_of_an_unknown_binary_is_ignored() {
    let (base, g, _) = small_base();
    let scope = Scope::Partial(partial(&["src/mod_c.rs"]));
    let d1 = D1Proposal {
        add: vec![tid("pkg::ghost", "t")],
        skip: vec![tid("pkg::ghost", "t")],
    };
    assert_eq!(
        plan(&scope, &base, &g, Some(&d1)),
        plan(&scope, &base, &g, None)
    );
}

#[test]
fn d1_skip_never_changes_a_decision_and_is_journaled() {
    let (base, g, [a, _b, c, ..]) = small_base();
    let scope = Scope::Partial(partial(&["src/mod_a.rs"]));
    let d1 = D1Proposal {
        add: vec![],
        skip: vec![a.clone(), c.clone()],
    };
    let without = plan(&scope, &base, &g, None);
    let p = plan(&scope, &base, &g, Some(&d1));
    assert_eq!(run_set(&p), run_set(&without));
    assert_eq!(skip_set(&p), skip_set(&without));
    // `a` est lancé : sa décision reste, la proposition ignorée est notée.
    let ea = entry(&p, &a.binary_id, &a.name);
    assert_eq!(ea.decision, Decision::Run);
    assert!(ea.reasons.contains(&Reason::D1SkipIgnored));
    // `c` est sauté : reste sauté, avec sa raison de saut ET la note.
    let ec = entry(&p, &c.binary_id, &c.name);
    assert_eq!(ec.decision, Decision::Skip);
    assert!(ec.reasons.contains(&Reason::D1SkipIgnored));
    assert!(
        ec.reasons
            .iter()
            .any(|r| matches!(r, Reason::NoPathToModified { .. }))
    );
    assert_journal_complete(&p, &base);
}

#[test]
fn d1_add_and_skip_of_the_same_test_runs_it() {
    let (base, g, [_a, _b, c, ..]) = small_base();
    let scope = Scope::Partial(partial(&["src/mod_a.rs"]));
    let d1 = D1Proposal {
        add: vec![c.clone()],
        skip: vec![c.clone()],
    };
    let p = plan(&scope, &base, &g, Some(&d1));
    let en = entry(&p, &c.binary_id, &c.name);
    assert_eq!(en.decision, Decision::Run);
    assert_eq!(en.reasons, vec![Reason::D1Added, Reason::D1SkipIgnored]);
    assert_journal_complete(&p, &base);
}

// ---------------------------------------------------------------------------------------
// Cibles
// ---------------------------------------------------------------------------------------

#[test]
fn targets_are_the_sorted_union_of_the_binaries_with_a_run_test() {
    let (base, g, _) = small_base();
    // util.rs : lib (a, b) + `it` (x, y) ; ni le bin, ni l'exemple.
    let p = plan(&Scope::Partial(partial(&["src/util.rs"])), &base, &g, None);
    assert_eq!(
        p.targets,
        Some(vec![
            NextestTarget::Lib,
            NextestTarget::Test("it".to_owned())
        ])
    );
    // main.rs : seulement le bin.
    let p = plan(&Scope::Partial(partial(&["src/main.rs"])), &base, &g, None);
    assert_eq!(
        p.targets,
        Some(vec![NextestTarget::Bin("kioku".to_owned())])
    );
    assert_eq!(
        nextest_args(&p, None),
        vec!["--bin".to_owned(), "kioku".to_owned()]
    );
    // tests/z.rs : un seul test d'intégration.
    let p = plan(&Scope::Partial(partial(&["tests/z.rs"])), &base, &g, None);
    assert_eq!(p.targets, Some(vec![NextestTarget::Test("it".to_owned())]));
}

#[test]
fn a_run_test_in_an_other_binary_means_the_full_target_set() {
    let (base, g, [_a, _b, _c, _m, _x, _y, e, _z]) = small_base();
    let p = plan(
        &Scope::Partial(partial(&["examples/ex.rs"])),
        &base,
        &g,
        None,
    );
    assert!(run_set(&p).contains(&e));
    assert_eq!(p.targets, None);
    // Aucune cible n'est donnée à cargo : les sautés de TOUS les binaires sont exclus.
    let filter = p.filter.as_deref().expect("skips are excluded");
    assert!(filter.contains("binary_id(=pkg)"), "{filter}");
    assert!(filter.contains("binary_id(=pkg::it)"), "{filter}");
    assert!(!filter.contains("example/ex"), "{filter}");
    // Un `Other` seulement sauté ne force rien.
    let p = plan(&Scope::Partial(partial(&["src/main.rs"])), &base, &g, None);
    assert_eq!(
        p.targets,
        Some(vec![NextestTarget::Bin("kioku".to_owned())])
    );
}

#[test]
fn a_new_whole_binary_absent_from_the_base_adds_its_target() {
    let (base, g, _) = small_base();
    let mut scope = partial(&["src/main.rs"]);
    scope.whole_binaries.insert(
        "pkg::fresh".to_owned(),
        Reason::NewBinary("pkg::fresh".to_owned()),
    );
    let p = plan(&Scope::Partial(scope.clone()), &base, &g, None);
    assert_eq!(
        p.targets,
        Some(vec![
            NextestTarget::Bin("kioku".to_owned()),
            NextestTarget::Test("fresh".to_owned()),
        ])
    );
    // Seul : un binaire nouveau sans aucun test lancé de l'inventaire.
    let mut only = partial(&[]);
    only.whole_binaries.insert(
        "pkg::fresh".to_owned(),
        Reason::NewBinary("pkg::fresh".to_owned()),
    );
    let p = plan(&Scope::Partial(only), &base, &g, None);
    assert_eq!(
        p.targets,
        Some(vec![NextestTarget::Test("fresh".to_owned())])
    );
    assert_eq!(nextest_args(&p, None), vec!["--test", "fresh"]);
    // Un nouveau binaire de nature inconnue (exemple) : suite complète.
    let mut weird = partial(&["src/main.rs"]);
    weird.whole_binaries.insert(
        "pkg::example/new".to_owned(),
        Reason::NewBinary("pkg::example/new".to_owned()),
    );
    assert_eq!(plan(&Scope::Partial(weird), &base, &g, None).targets, None);
    // Un nouveau binaire `bin/` donne `--bin`.
    let mut newbin = partial(&[]);
    newbin.whole_binaries.insert(
        "pkg::bin/tool".to_owned(),
        Reason::NewBinary("pkg::bin/tool".to_owned()),
    );
    assert_eq!(
        plan(&Scope::Partial(newbin), &base, &g, None).targets,
        Some(vec![NextestTarget::Bin("tool".to_owned())])
    );
}

// ---------------------------------------------------------------------------------------
// Filtre et arguments
// ---------------------------------------------------------------------------------------

#[test]
fn filter_excludes_exactly_the_skipped_tests_of_the_targeted_binaries() {
    let (base, g, _) = small_base();
    let p = plan(&Scope::Partial(partial(&["src/mod_a.rs"])), &base, &g, None);
    // Lancés : a (lib), x et y (it, via tests/it.rs → mod_a). Sautés ciblés : b, c (lib), z (it).
    assert_eq!(
        p.filter.as_deref(),
        Some(
            "not ((binary_id(=pkg) & (test(=mod_b::tests::b) | test(=mod_c::tests::c))) \
             | (binary_id(=pkg::it) & (test(=z))))"
        )
    );
    // Le test sauté du bin et de l'exemple n'apparaissent pas : ces binaires ne sont pas compilés.
    assert!(!p.filter.as_deref().unwrap_or("").contains("example"));
    assert!(!p.filter.as_deref().unwrap_or("").contains("kioku"));
}

#[test]
fn filter_is_none_when_every_test_of_the_targets_runs() {
    let (base, g, _) = small_base();
    let mut scope = partial(&["tests/it.rs"]);
    scope.whole_binaries.insert(
        "pkg::it".to_owned(),
        Reason::NewBinary("pkg::it".to_owned()),
    );
    let p = plan(&Scope::Partial(scope), &base, &g, None);
    // `it` en entier ; mod_a (via tests/it.rs ? non : racine mod_a, pas it.rs) : lib non ciblée.
    assert_eq!(p.targets, Some(vec![NextestTarget::Test("it".to_owned())]));
    assert_eq!(p.filter, None);
}

#[test]
fn an_inexpressible_test_name_is_run_not_skipped() {
    let weird = tid("pkg", "name with (parens), and \\ backslash");
    let plain = tid("pkg", "plain");
    let other = tid("pkg", "other");
    let base = base(vec![binary(
        "pkg",
        "pkg",
        BinaryKind::Lib,
        &[weird.name.as_str(), "plain", "other"],
    )]);
    let g = graph(
        &[],
        &[
            (&weird, &["w.rs"]),
            (&plain, &["p.rs"]),
            (&other, &["o.rs"]),
        ],
    );
    let p = plan(&Scope::Partial(partial(&["o.rs"])), &base, &g, None);
    assert_eq!(entry(&p, "pkg", &weird.name).decision, Decision::Run);
    assert!(matches!(
        entry(&p, "pkg", &weird.name).reasons.as_slice(),
        [Reason::Error(_)]
    ));
    assert_eq!(entry(&p, "pkg", "plain").decision, Decision::Skip);
    assert_eq!(
        p.filter.as_deref(),
        Some("not ((binary_id(=pkg) & (test(=plain))))")
    );
    assert!(!p.filter.as_deref().unwrap_or("").contains("parens"));
    assert_journal_complete(&p, &base);
}

#[test]
fn an_oversized_filter_widens_the_targeted_binaries() {
    let names: Vec<String> = (0..4000)
        .map(|i| format!("module_with_a_rather_long_name::tests::test_number_{i:05}"))
        .collect();
    let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut roots_owned: Vec<(TestId, Vec<&str>)> = names
        .iter()
        .map(|n| (tid("pkg", n), vec!["skipped.rs"]))
        .collect();
    let hot = tid("pkg", "hot");
    roots_owned.push((hot.clone(), vec!["changed.rs"]));
    let cold = tid("pkg::cold", "c");
    roots_owned.push((cold.clone(), vec!["cold.rs"]));
    let roots: Vec<(&TestId, &[&str])> =
        roots_owned.iter().map(|(t, r)| (t, r.as_slice())).collect();
    let mut all_names = name_refs.clone();
    all_names.push("hot");
    let base = base(vec![
        binary("pkg", "pkg", BinaryKind::Lib, &all_names),
        binary("pkg::cold", "cold", BinaryKind::Test, &["c"]),
    ]);
    let g = with_binary_files(
        graph(&[], &roots),
        &[
            ("pkg", &["changed.rs", "skipped.rs"]),
            ("pkg::cold", &["cold.rs"]),
        ],
    );
    let p = plan(&Scope::Partial(partial(&["changed.rs"])), &base, &g, None);
    assert_eq!(p.filter, None, "no filter: whole targeted binary instead");
    assert_eq!(p.targets, Some(vec![NextestTarget::Lib]));
    assert_eq!(entry(&p, "pkg", &names[0]).decision, Decision::Run);
    assert!(matches!(
        entry(&p, "pkg", &names[0]).reasons.as_slice(),
        [Reason::Error(_)]
    ));
    // Le binaire non ciblé reste sauté.
    assert_eq!(entry(&p, "pkg::cold", "c").decision, Decision::Skip);
    assert_journal_complete(&p, &base);
}

#[test]
fn nextest_args_combine_targets_user_filter_and_plan_filter() {
    let (base, g, _) = small_base();
    let p = plan(&Scope::Partial(partial(&["src/mod_a.rs"])), &base, &g, None);
    let filter = p.filter.clone().expect("filter");
    assert_eq!(
        nextest_args(&p, None),
        vec![
            "--lib".to_owned(),
            "--test".to_owned(),
            "it".to_owned(),
            "-E".to_owned(),
            filter.clone()
        ]
    );
    assert_eq!(
        nextest_args(&p, Some("test(mod_a)")),
        vec![
            "--lib".to_owned(),
            "--test".to_owned(),
            "it".to_owned(),
            "-E".to_owned(),
            format!("(test(mod_a)) & ({filter})")
        ]
    );
    // Sans filtre de plan : l'expression de l'utilisateur seule.
    let mut no_filter = p.clone();
    no_filter.filter = None;
    assert_eq!(
        nextest_args(&no_filter, Some("test(a)")),
        vec!["--lib", "--test", "it", "-E", "test(a)"]
    );
    // Suite complète sans filtre utilisateur : aucun argument ; avec : `-E` seul.
    let all = plan(
        &Scope::Everything {
            reasons: vec![Reason::BuildInput("Cargo.lock".to_owned())],
        },
        &base,
        &g,
        None,
    );
    assert_eq!(nextest_args(&all, None), Vec::<String>::new());
    assert_eq!(nextest_args(&all, Some("test(x)")), vec!["-E", "test(x)"]);
}

// ---------------------------------------------------------------------------------------
// Preuve sur le graphe réel de kioku (fixture L3)
// ---------------------------------------------------------------------------------------

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/select/l3")
        .join(name)
}

fn split_key(key: &str) -> TestId {
    let (binary, name) = key.split_once(' ').expect("key with a space");
    tid(binary, name)
}

/// Graphe kioku 941cec0 (575 tests) et sa couverture mesurée.
fn kioku_graph() -> GraphData {
    #[derive(serde::Deserialize)]
    struct Raw {
        edges: BTreeMap<String, BTreeSet<String>>,
        test_roots: BTreeMap<String, BTreeSet<String>>,
    }
    let raw: Raw = serde_json::from_str(
        &fs::read_to_string(fixture("kioku-941cec0-graph.json")).expect("graph fixture"),
    )
    .expect("graph json");
    GraphData {
        schema: GRAPH_SCHEMA.to_owned(),
        digest: "kioku-941cec0-scip".to_owned(),
        edges: raw.edges,
        test_roots: raw
            .test_roots
            .into_iter()
            .map(|(k, v)| (split_key(&k), v))
            .collect(),
        ..GraphData::default()
    }
}

fn kioku_coverage() -> BTreeMap<TestId, BTreeSet<RelPath>> {
    coverage_from_json(
        &fs::read_to_string(fixture("kioku-941cec0-coverage.json")).expect("coverage fixture"),
    )
    .expect("coverage parses")
}

/// Graphe + arêtes apprises appliquées.
fn kioku_learned_graph() -> GraphData {
    let mut g = kioku_graph();
    let learned = learn(&g, &kioku_coverage());
    apply(&mut g, &learned);
    g
}

/// Inventaire de la base déduit des 575 tests de la fixture.
fn kioku_base(g: &GraphData) -> BaseInventory {
    let mut by_binary: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for t in g.test_roots.keys() {
        by_binary
            .entry(t.binary_id.clone())
            .or_default()
            .push(t.name.clone());
    }
    let binaries = by_binary
        .into_iter()
        .map(|(id, names)| {
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            let (target, kind) = match id.split_once("::") {
                None => ("kioku".to_owned(), BinaryKind::Lib),
                Some(("kdw-kioku", rest)) => match rest.strip_prefix("bin/") {
                    Some(bin) => (bin.to_owned(), BinaryKind::Bin),
                    None => (rest.to_owned(), BinaryKind::Test),
                },
                Some(_) => panic!("unexpected binary id {id}"),
            };
            binary(&id, &target, kind, &refs)
        })
        .collect();
    base(binaries)
}

fn counts(plan: &Plan) -> (usize, usize) {
    (run_set(plan).len(), skip_set(plan).len())
}

#[test]
fn kioku_validate_rs_change_runs_the_v2_r14_schema_tests_thanks_to_learning() {
    let g = kioku_learned_graph();
    let base = kioku_base(&g);
    let validate = "src/ci2/domain/validate.rs";
    let p = plan(&Scope::Partial(partial(&[validate])), &base, &g, None);
    let (run, skip) = counts(&p);
    eprintln!(
        "L4a cas 1 validate.rs : {run} lancés, {skip} sautés (sur {})",
        run + skip
    );
    for name in [
        "input_selector_variant_refuses_an_undeclared_field",
        "origin_claim_variant_refuses_an_undeclared_field",
    ] {
        let en = entry(&p, "kdw-kioku::v2_r14_schema", name);
        assert_eq!(en.decision, Decision::Run, "{name}");
        assert!(
            en.reasons.contains(&Reason::Learned {
                file: validate.to_owned()
            }),
            "{name}: {:?}",
            en.reasons
        );
    }
    for name in [
        "bounded_decode_accepts_the_small_valid_record",
        "bounded_decode_refuses_bodies_over_the_size_cap",
        "bounded_decode_refuses_trailing_content_but_allows_trailing_whitespace",
        "bounded_decode_refuses_unknown_fields",
    ] {
        assert_eq!(
            entry(&p, "kdw-kioku::v2_schema_contracts_extra", name).decision,
            Decision::Run,
            "{name}"
        );
    }
    // Sans apprentissage, les deux tests de `v2_r14_schema` seraient sautés : c'est le filet.
    let bare = kioku_graph();
    let p0 = plan(&Scope::Partial(partial(&[validate])), &base, &bare, None);
    assert_eq!(
        entry(
            &p0,
            "kdw-kioku::v2_r14_schema",
            "input_selector_variant_refuses_an_undeclared_field"
        )
        .decision,
        Decision::Skip
    );
    assert!(run > 0 && skip > 0);
    assert_journal_complete(&p, &base);
}

#[test]
fn kioku_runtime_mod_change_runs_the_v2_runtime_tests() {
    let g = kioku_learned_graph();
    let base = kioku_base(&g);
    let file = "src/ci2/runtime/mod.rs";
    let p = plan(&Scope::Partial(partial(&[file])), &base, &g, None);
    let (run, skip) = counts(&p);
    let runtime: Vec<&TestId> = g
        .test_roots
        .keys()
        .filter(|t| t.binary_id == "kdw-kioku::v2_runtime")
        .collect();
    eprintln!(
        "L4a cas 2 runtime/mod.rs : {run} lancés, {skip} sautés (sur {}) ; v2_runtime : {} tests tous lancés",
        run + skip,
        runtime.len()
    );
    assert_ne!(runtime.len(), 0);
    for t in runtime {
        let en = entry(&p, &t.binary_id, &t.name);
        assert_eq!(en.decision, Decision::Run, "{}", t.name);
        assert!(matches!(
            en.reasons.first(),
            Some(Reason::ReachesModified { file: f, .. }) if f == file
        ));
    }
    assert_journal_complete(&p, &base);
}

#[test]
fn kioku_trace_rs_change_skips_the_unrelated_ci2_tests_with_a_reason() {
    let g = kioku_learned_graph();
    let base = kioku_base(&g);
    let file = "src/trace.rs";
    let p = plan(&Scope::Partial(partial(&[file])), &base, &g, None);
    let (run, skip) = counts(&p);
    let (mut ci2_total, mut ci2_skipped) = (0usize, 0usize);
    for t in g.test_roots.keys().filter(|t| {
        t.name.contains("ci2") || t.binary_id.contains("ci2") || t.name.contains("::ci2::")
    }) {
        ci2_total += 1;
        let roots = &g.test_roots[t];
        let reaches = reach(&g, roots).contains(file);
        let en = entry(&p, &t.binary_id, &t.name);
        if reaches {
            assert_eq!(en.decision, Decision::Run, "{} {}", t.binary_id, t.name);
        } else {
            ci2_skipped += 1;
            assert_eq!(en.decision, Decision::Skip, "{} {}", t.binary_id, t.name);
            assert_eq!(
                en.reasons,
                vec![Reason::NoPathToModified {
                    graph_digest: "kioku-941cec0-scip".to_owned()
                }]
            );
        }
    }
    eprintln!(
        "L4a cas 4 trace.rs : {run} lancés, {skip} sautés (sur {}) ; tests ci2 : {ci2_total}, dont {ci2_skipped} sautés",
        run + skip
    );
    assert!(
        ci2_total > 0 && ci2_skipped > 0,
        "ci2 tests exist and are skipped"
    );
    // Les lancés viennent bien d'un chemin vers trace.rs.
    for en in p.entries.iter().filter(|e| e.decision == Decision::Run) {
        assert!(en.reasons.iter().any(|r| matches!(
            r,
            Reason::ReachesModified { file: f, .. } if f == file
        )));
    }
    assert_journal_complete(&p, &base);
}

/// Générateur déterministe (xorshift64*) : aucune dépendance, résultats reproductibles.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % bound as u64).expect("fits")
    }
}

#[test]
fn d1_can_only_add_on_the_real_graph_for_a_hundred_modified_sets() {
    let g = kioku_learned_graph();
    let base = kioku_base(&g);
    let tests: Vec<TestId> = inventory_ids(&base).into_iter().collect();
    let files: Vec<RelPath> = {
        let mut all: BTreeSet<RelPath> = g.edges.keys().cloned().collect();
        all.extend(g.edges.values().flatten().cloned());
        all.extend(g.test_roots.values().flatten().cloned());
        all.into_iter().collect()
    };
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut strictly_more = 0usize;
    let mut with_skips = 0usize;
    for round in 0..120 {
        let k = 1 + rng.below(4);
        let modified: Vec<&str> = (0..k)
            .map(|_| files[rng.below(files.len())].as_str())
            .collect();
        let scope = Scope::Partial(partial(&modified));
        let add: Vec<TestId> = (0..rng.below(6))
            .map(|_| {
                if rng.below(4) == 0 {
                    // test inconnu d'un binaire connu, ou d'un binaire inconnu
                    let known = &tests[rng.below(tests.len())];
                    if rng.below(2) == 0 {
                        tid(&known.binary_id, "not_in_the_base")
                    } else {
                        tid("kdw-kioku::ghost", "t")
                    }
                } else {
                    tests[rng.below(tests.len())].clone()
                }
            })
            .collect();
        let skip: Vec<TestId> = (0..rng.below(40))
            .map(|_| tests[rng.below(tests.len())].clone())
            .collect();
        let d1 = D1Proposal { add, skip };
        let without = plan(&scope, &base, &g, None);
        let with = plan(&scope, &base, &g, Some(&d1));
        assert!(
            run_set(&without).is_subset(&run_set(&with)),
            "round {round}: d1 removed a test from the run set"
        );
        // Aucun saut de d1 ne change une décision hors des ajouts de d1.
        for en in &with.entries {
            if en.decision == Decision::Skip {
                assert_eq!(
                    entry(&without, &en.test.binary_id, &en.test.name).decision,
                    Decision::Skip
                );
            }
        }
        assert_eq!(with.entries.len(), without.entries.len());
        assert_eq!(with.entries.len(), tests.len(), "one entry per test");
        assert_journal_complete(&with, &base);
        assert_journal_complete(&without, &base);
        if run_set(&with).len() > run_set(&without).len() {
            strictly_more += 1;
        }
        if !skip_set(&without).is_empty() {
            with_skips += 1;
        }
        // Le plan sans d1 est aussi un sous-ensemble exact de « tout ».
        assert!(run_set(&with).is_subset(&inventory_ids(&base)));
    }
    assert!(
        strictly_more > 10,
        "d1 adds actually took effect ({strictly_more})"
    );
    assert!(
        with_skips > 60,
        "most random sets still skip tests ({with_skips})"
    );
}

// ---------------------------------------------------------------------------------------
// Le vrai nextest accepte le filtre généré
// ---------------------------------------------------------------------------------------

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("mkdir");
    for item in fs::read_dir(from).expect("read_dir") {
        let item = item.expect("entry");
        let dest = to.join(item.file_name());
        if item.file_type().expect("type").is_dir() {
            copy_dir(&item.path(), &dest);
        } else {
            fs::copy(item.path(), dest).expect("copy");
        }
    }
}

/// `cargo nextest list` sur la copie ; renvoie les lignes « binaire test » listées.
fn nextest_list(dir: &Path, args: &[String]) -> BTreeSet<String> {
    let out = Command::new("cargo")
        .args(["nextest", "list", "--locked", "--message-format", "oneline"])
        .args(args)
        .current_dir(dir)
        .env("CARGO_NET_OFFLINE", "true")
        .output()
        .expect("cargo nextest list runs");
    assert!(
        out.status.success(),
        "nextest rejected {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

#[test]
fn real_nextest_accepts_the_generated_filter_and_lists_exactly_the_expected_tests() {
    // Copie de la mini-crate sous un nom à tiret (`mini-cov`) : le binaire `mini-cov::it` a un
    // identifiant avec `-` et `::`, comme `kdw-kioku::…`.
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("mini-cov");
    copy_dir(&fixture("minicrate"), &dir);
    for rel in ["Cargo.toml", "Cargo.lock"] {
        let text = fs::read_to_string(dir.join(rel)).expect("read");
        fs::write(
            dir.join(rel),
            text.replace("name = \"minicov\"", "name = \"mini-cov\""),
        )
        .expect("write");
    }
    let it = dir.join("tests/it.rs");
    let text = fs::read_to_string(&it).expect("read it.rs");
    fs::write(&it, text.replace("minicov::", "mini_cov::")).expect("write it.rs");

    // Inventaire réel, relevé par nextest lui-même.
    let listed = nextest_list(&dir, &[]);
    assert_eq!(
        listed,
        BTreeSet::from([
            "mini-cov tests::lib_unit".to_owned(),
            "mini-cov::it slow".to_owned(),
            "mini-cov::it uses_a".to_owned(),
            "mini-cov::it uses_b".to_owned(),
        ])
    );
    let base = base(vec![
        binary(
            "mini-cov",
            "mini-cov",
            BinaryKind::Lib,
            &["tests::lib_unit"],
        ),
        binary(
            "mini-cov::it",
            "it",
            BinaryKind::Test,
            &["slow", "uses_a", "uses_b"],
        ),
    ]);
    let lib = tid("mini-cov", "tests::lib_unit");
    let (slow, a, b) = (
        tid("mini-cov::it", "slow"),
        tid("mini-cov::it", "uses_a"),
        tid("mini-cov::it", "uses_b"),
    );
    let g = with_binary_files(
        graph(
            &[("tests/it.rs", &[])],
            &[
                (&lib, &["src/lib.rs", "src/a.rs"]),
                (&a, &["tests/it.rs", "src/a.rs"]),
                (&b, &["tests/it.rs", "src/b.rs"]),
                (&slow, &["tests/it.rs"]),
            ],
        ),
        &[
            ("mini-cov", &["src/lib.rs", "src/a.rs", "src/b.rs"]),
            ("mini-cov::it", &["tests/it.rs"]),
        ],
    );

    // (1) src/b.rs : seul `uses_b` est lancé ; cibles `--lib` (propriétaire de `src/b.rs`,
    // aucun de ses tests connus n'est lancé : un test neuf pourrait y être) et `--test it` ;
    // `lib_unit`, `slow` et `uses_a` sont exclus.
    let p = plan(&Scope::Partial(partial(&["src/b.rs"])), &base, &g, None);
    assert_eq!(run_set(&p), BTreeSet::from([b.clone()]));
    assert_eq!(
        p.targets,
        Some(vec![
            NextestTarget::Lib,
            NextestTarget::Test("it".to_owned())
        ])
    );
    let args = nextest_args(&p, None);
    assert_eq!(
        nextest_list(&dir, &args),
        BTreeSet::from(["mini-cov::it uses_b".to_owned()]),
        "args: {args:?}"
    );

    // (2) src/a.rs : lib_unit + uses_a ; deux cibles ; `slow` (et `uses_b`) exclus.
    let p = plan(&Scope::Partial(partial(&["src/a.rs"])), &base, &g, None);
    assert_eq!(run_set(&p), BTreeSet::from([lib.clone(), a.clone()]));
    let args = nextest_args(&p, None);
    assert_eq!(
        nextest_list(&dir, &args),
        BTreeSet::from([
            "mini-cov tests::lib_unit".to_owned(),
            "mini-cov::it uses_a".to_owned(),
        ]),
        "args: {args:?}"
    );

    // (3) filtre utilisateur combiné : `(user) & (filtre du plan)`.
    let args = nextest_args(&p, Some("test(lib_unit)"));
    assert_eq!(
        nextest_list(&dir, &args),
        BTreeSet::from(["mini-cov tests::lib_unit".to_owned()]),
        "args: {args:?}"
    );

    // (4) src/lib.rs seul : lib_unit ; aucune autre cible compilée.
    let p = plan(&Scope::Partial(partial(&["src/lib.rs"])), &base, &g, None);
    assert_eq!(run_set(&p), BTreeSet::from([lib.clone()]));
    let args = nextest_args(&p, None);
    assert_eq!(args[0], "--lib");
    assert_eq!(
        nextest_list(&dir, &args),
        BTreeSet::from(["mini-cov tests::lib_unit".to_owned()]),
        "args: {args:?}"
    );

    // (5) rien à lancer : `-E none()` n'élargit pas, nextest l'accepte et ne liste rien.
    let p = plan(&Scope::Partial(partial(&[])), &base, &g, None);
    let args = nextest_args(&p, None);
    assert!(nextest_list(&dir, &args).is_empty(), "args: {args:?}");

    // (6) un test nouveau d'un binaire ciblé tourne toujours (on exclut, on ne liste pas).
    fs::write(
        &it,
        format!(
            "{}\n#[test]\nfn brand_new() {{}}\n",
            fs::read_to_string(&it).expect("read")
        ),
    )
    .expect("append");
    let p = plan(&Scope::Partial(partial(&["src/b.rs"])), &base, &g, None);
    assert_eq!(
        nextest_list(&dir, &nextest_args(&p, None)),
        BTreeSet::from([
            "mini-cov::it brand_new".to_owned(),
            "mini-cov::it uses_b".to_owned(),
        ])
    );

    // (7) A5 : un test NEUF d'un fichier de production, dans la lib dont aucun test connu
    // n'est lancé pour `src/b.rs` : la cible propriétaire le fait tourner, le filtre n'exclut
    // que `lib_unit` (connu et sauté).
    let lib_rs = dir.join("src/lib.rs");
    fs::write(
        &lib_rs,
        format!(
            "{}\n#[cfg(test)]\nmod fresh {{\n    #[test]\n    fn lib_brand_new() {{}}\n}}\n",
            fs::read_to_string(&lib_rs).expect("read lib.rs")
        ),
    )
    .expect("append lib test");
    let p = plan(&Scope::Partial(partial(&["src/b.rs"])), &base, &g, None);
    assert_eq!(
        nextest_list(&dir, &nextest_args(&p, None)),
        BTreeSet::from([
            "mini-cov fresh::lib_brand_new".to_owned(),
            "mini-cov::it brand_new".to_owned(),
            "mini-cov::it uses_b".to_owned(),
        ])
    );
}

#[test]
fn a_binary_whose_only_run_tests_are_ignored_in_the_base_is_not_compiled() {
    // `kdw::slow` ne contient qu'un test `#[ignore]` (forcé « lancé », BaseNotGreen(Ignored))
    // et rien n'y atteint le fichier modifié : nextest ne l'exécuterait pas, on ne le compile
    // pas. `kdw::cli` atteint le fichier modifié : compilé.
    let mut slow = binary("kdw::slow", "slow", BinaryKind::Test, &["heavy"]);
    slow.tests[0].status = BaseStatus::Ignored;
    let cli = binary("kdw::cli", "cli", BinaryKind::Test, &["uses_a"]);
    let inv = base(vec![slow, cli]);
    let heavy = tid("kdw::slow", "heavy");
    let uses_a = tid("kdw::cli", "uses_a");
    let g = with_binary_files(
        graph(
            &[],
            &[(&heavy, &["tests/slow.rs"]), (&uses_a, &["src/a.rs"])],
        ),
        &[
            ("kdw::slow", &["tests/slow.rs"]),
            ("kdw::cli", &["tests/cli.rs", "src/a.rs"]),
        ],
    );
    let mut p = partial(&["src/a.rs"]);
    p.forced_tests
        .insert(heavy.clone(), Reason::BaseNotGreen(BaseStatus::Ignored));
    let planned = plan(&Scope::Partial(p.clone()), &inv, &g, None);
    assert_eq!(
        entry(&planned, "kdw::slow", "heavy").decision,
        Decision::Run
    );
    assert_eq!(
        planned.targets,
        Some(vec![NextestTarget::Test("cli".to_owned())])
    );
    assert_journal_complete(&planned, &inv);

    // Le même test ignoré, mais atteint par la modification : son binaire est compilé.
    let mut p2 = partial(&["tests/slow.rs"]);
    p2.forced_tests
        .insert(heavy.clone(), Reason::BaseNotGreen(BaseStatus::Ignored));
    let planned = plan(&Scope::Partial(p2), &inv, &g, None);
    assert_eq!(
        planned.targets,
        Some(vec![NextestTarget::Test("slow".to_owned())])
    );
}

// ---------------------------------------------------------------------------------------
// A5 : cibles propriétaires (un test neuf dans un fichier de production existant)
// ---------------------------------------------------------------------------------------

/// Lib `kdw` : un seul test connu, `#[ignore]` ; intégration `kdw::cli` : atteint `src/d1.rs`.
fn owner_fixture() -> (BaseInventory, GraphData, TestId, TestId) {
    let mut lib = binary("kdw", "kdw", BinaryKind::Lib, &["d1::tests::slow"]);
    lib.tests[0].status = BaseStatus::Ignored;
    let cli = binary("kdw::cli", "cli", BinaryKind::Test, &["uses_d1", "other"]);
    let inv = base(vec![lib, cli]);
    let (slow, uses_d1) = (tid("kdw", "d1::tests::slow"), tid("kdw::cli", "uses_d1"));
    let other = tid("kdw::cli", "other");
    let g = with_binary_files(
        graph(
            &[("tests/cli.rs", &["src/d1.rs"])],
            &[
                (&slow, &["src/d1.rs"]),
                (&uses_d1, &["tests/cli.rs"]),
                (&other, &["tests/other.rs"]),
            ],
        ),
        &[
            ("kdw", &["src/lib.rs", "src/d1.rs"]),
            ("kdw::cli", &["tests/cli.rs"]),
        ],
    );
    (inv, g, slow, uses_d1)
}

#[test]
fn a_binary_compiling_a_modified_file_is_a_target_even_when_none_of_its_tests_run() {
    let (inv, g, slow, uses_d1) = owner_fixture();
    let p = plan(&Scope::Partial(partial(&["src/d1.rs"])), &inv, &g, None);
    // Le seul test connu de la lib est `#[ignore]` : lancé pour la forme (journal), sans
    // justifier de compiler. C'est le fichier modifié, compilé dans la lib, qui la rend cible.
    assert!(run_set(&p).contains(&uses_d1));
    assert!(matches!(
        entry(&p, "kdw", &slow.name).reasons.as_slice(),
        [Reason::ReachesModified { .. }]
    ));
    assert_eq!(
        p.targets,
        Some(vec![
            NextestTarget::Lib,
            NextestTarget::Test("cli".to_owned())
        ])
    );
    // Sans l'intégration atteinte (aucun test lancé nulle part sauf le `#[ignore]`) : la lib
    // reste cible, c'est tout le défaut A5.
    let other = tid("kdw::cli", "other");
    let g_orphan = with_binary_files(
        graph(
            &[],
            &[
                (&slow, &["src/other.rs"]),
                (&uses_d1, &["tests/cli.rs"]),
                (&other, &["tests/other.rs"]),
            ],
        ),
        &[
            ("kdw", &["src/lib.rs", "src/d1.rs"]),
            ("kdw::cli", &["tests/cli.rs"]),
        ],
    );
    let p = plan(
        &Scope::Partial(partial(&["src/d1.rs"])),
        &inv,
        &g_orphan,
        None,
    );
    assert!(run_set(&p).is_empty(), "aucun test connu n'est lancé");
    assert_eq!(p.targets, Some(vec![NextestTarget::Lib]));
    // Le test connu (sauté ici) est exclu ; tout test neuf de la lib reste sélectionné.
    assert_eq!(
        nextest_args(&p, None),
        vec![
            "--lib",
            "-E",
            "not ((binary_id(=kdw) & (test(=d1::tests::slow))))"
        ]
    );
}

#[test]
fn the_filter_covers_every_compiled_binary_and_never_a_new_test() {
    // Lib de 3 tests connus sautés, aucun lancé ; `src/d1.rs` y est compilé : lib = cible. Le
    // filtre exclut exactement ses 3 tests connus (sinon ils tourneraient tous) ; un test
    // absent de l'inventaire n'y figure jamais, donc il tourne.
    let inv = base(vec![
        binary("kdw", "kdw", BinaryKind::Lib, &["m::a", "m::b", "m::c"]),
        binary("kdw::cold", "cold", BinaryKind::Test, &["z"]),
    ]);
    let ids = [tid("kdw", "m::a"), tid("kdw", "m::b"), tid("kdw", "m::c")];
    let z = tid("kdw::cold", "z");
    let g = with_binary_files(
        graph(
            &[],
            &[
                (&ids[0], &["src/m.rs"]),
                (&ids[1], &["src/m.rs"]),
                (&ids[2], &["src/m.rs"]),
                (&z, &["tests/cold.rs"]),
            ],
        ),
        &[
            ("kdw", &["src/lib.rs", "src/d1.rs", "src/m.rs"]),
            ("kdw::cold", &["tests/cold.rs"]),
        ],
    );
    let p = plan(&Scope::Partial(partial(&["src/d1.rs"])), &inv, &g, None);
    assert!(run_set(&p).is_empty());
    assert_eq!(p.targets, Some(vec![NextestTarget::Lib]));
    assert_eq!(
        p.filter.as_deref(),
        Some("not ((binary_id(=kdw) & (test(=m::a) | test(=m::b) | test(=m::c))))")
    );
    // `kdw::cold` n'est pas compilé : ni cible ni terme du filtre.
    assert!(!p.filter.as_deref().unwrap_or("").contains("cold"));
    assert_eq!(entry(&p, "kdw::cold", "z").decision, Decision::Skip);
    assert_journal_complete(&p, &inv);
}

#[test]
fn an_oversized_filter_widens_an_owner_binary_without_any_run_test() {
    let names: Vec<String> = (0..4000)
        .map(|i| format!("module_with_a_rather_long_name::tests::test_number_{i:05}"))
        .collect();
    let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let inv = base(vec![binary("pkg", "pkg", BinaryKind::Lib, &name_refs)]);
    let roots_owned: Vec<(TestId, Vec<&str>)> = names
        .iter()
        .map(|n| (tid("pkg", n), vec!["skipped.rs"]))
        .collect();
    let roots: Vec<(&TestId, &[&str])> =
        roots_owned.iter().map(|(t, r)| (t, r.as_slice())).collect();
    let g = with_binary_files(
        graph(&[], &roots),
        &[("pkg", &["changed.rs", "skipped.rs"])],
    );
    let p = plan(&Scope::Partial(partial(&["changed.rs"])), &inv, &g, None);
    assert_eq!(p.targets, Some(vec![NextestTarget::Lib]));
    assert_eq!(
        p.filter, None,
        "filtre trop long : la lib propriétaire tourne en entier"
    );
    assert_eq!(entry(&p, "pkg", &names[0]).decision, Decision::Run);
    assert_journal_complete(&p, &inv);
}

#[test]
fn a_binary_missing_from_binary_files_is_a_doubt_and_a_target() {
    let (inv, g, _slow, _uses_d1) = owner_fixture();
    let mut g = g;
    g.binary_files.remove("kdw::cli");
    // `src/other.rs` ne touche aucun test : sans le doute, rien à compiler.
    let p = plan(&Scope::Partial(partial(&["docs/x.md"])), &inv, &g, None);
    assert_eq!(p.targets, Some(vec![NextestTarget::Test("cli".to_owned())]));
    // Rien de modifié : aucune cible, aucun doute.
    let p = plan(&Scope::Partial(partial(&[])), &inv, &g, None);
    assert_eq!(p.targets, Some(vec![]));
}

#[test]
fn an_owner_of_an_unknown_kind_means_the_full_suite() {
    let ex = tid("pkg::example/ex", "e");
    let inv = base(vec![
        binary("pkg", "pkg", BinaryKind::Lib, &["t"]),
        binary("pkg::example/ex", "ex", BinaryKind::Other, &["e"]),
    ]);
    let t = tid("pkg", "t");
    let g = with_binary_files(
        graph(&[], &[(&t, &["src/t.rs"]), (&ex, &["examples/ex.rs"])]),
        &[
            ("pkg", &["src/lib.rs", "src/t.rs"]),
            ("pkg::example/ex", &["examples/ex.rs", "src/shared.rs"]),
        ],
    );
    let p = plan(&Scope::Partial(partial(&["src/shared.rs"])), &inv, &g, None);
    assert_eq!(p.targets, None, "un `Other` propriétaire : suite complète");
}
