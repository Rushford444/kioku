//! Lot F1 (nuit 1711) : tests de propriétés de la sélection.
//!
//! Trois familles de propriétés, toutes sur de petits graphes générés :
//!
//! 1. **Complétude** : l'ensemble lancé par [`plan`] contient exactement les tests qu'un
//!    oracle indépendant déclare impactés. L'oracle ne réutilise ni `reach` ni `plan` : il
//!    calcule la clôture par relaxation de point fixe sur la liste d'arêtes (style
//!    `Bellman-Ford`), là où la production fait un parcours en profondeur. Un oracle dérivé de
//!    la fonction testée ne prouverait rien ; celui-ci est un second algorithme.
//! 2. **Idempotence et monotonie** : `learn::apply` appliqué deux fois vaut une fois, une
//!    seconde passe de `learn` ne trouve plus rien, et l'ensemble atteint ne rétrécit jamais.
//! 3. **Stabilité** : permuter l'ordre des binaires, des tests, des arêtes, des fichiers
//!    modifiés et des propositions d1 ne change pas le plan.
//!
//! S'y ajoutent des cas nommés pour les formes que le brief exige de couvrir : racines
//! multiples, cycles, fichier inconnu du graphe, ajout/suppression/renommage, changement de
//! fichier de build, et d1 (ajout seul, saut ignoré).
//!
//! Aucune de ces propriétés ne dépend d'un run natif : elles portent sur les fonctions pures
//! de `select` et ne compilent ni ne lancent nextest.

#![allow(
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::too_many_lines
)]

use std::collections::{BTreeMap, BTreeSet};

use kioku::select::classify::{ClassifyContext, Version, classify, reach};
use kioku::select::journal::{Journal, JournalCounts, JournalScope, JournalStatus, check_complete};
use kioku::select::learn::{apply, learn};
use kioku::select::plan::{FilterSet, nextest_args, plan};
use kioku::select::{
    BASE_SCHEMA, BaseBinary, BaseInventory, BaseStatus, BaseTest, BinaryKind, Change, ChangeSet,
    D1Proposal, Decision, GRAPH_SCHEMA, GraphData, JOURNAL_SCHEMA, NextestTarget, PartialScope,
    Plan, Reason, RelPath, Scope, TestId,
};
use proptest::prelude::*;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const DIGEST: &str = "digest-g";

// ---------------------------------------------------------------------------------------
// Modèle : un petit dépôt décrit par des indices de fichiers
// ---------------------------------------------------------------------------------------

/// Un test du modèle : nom, fichiers racines (vide = racine inconnue), statut de base.
#[derive(Debug, Clone)]
struct ModelTest {
    name: String,
    roots: Vec<usize>,
    status: BaseStatus,
}

/// Un binaire du modèle.
#[derive(Debug, Clone)]
struct ModelBinary {
    id: String,
    target: String,
    kind: BinaryKind,
    tests: Vec<ModelTest>,
}

/// Un dépôt généré : `files` fichiers `src/f<i>.rs`, des arêtes (cycles permis), des
/// binaires, et l'ensemble des fichiers modifiés.
#[derive(Debug, Clone)]
struct Model {
    files: usize,
    seed: u64,
    edges: Vec<(usize, usize)>,
    binaries: Vec<ModelBinary>,
    modified: Vec<usize>,
}

fn file_name(index: usize) -> RelPath {
    format!("src/f{index}.rs")
}

fn test_strategy(files: usize) -> impl Strategy<Value = Vec<ModelTest>> {
    (0usize..=3).prop_flat_map(move |count| {
        prop::collection::vec(prop::collection::vec(0..files, 0..=2), count).prop_map(
            move |items| {
                items
                    .into_iter()
                    .enumerate()
                    .map(|(index, roots)| ModelTest {
                        name: format!("t{index}"),
                        roots,
                        status: BaseStatus::Pass,
                    })
                    .collect()
            },
        )
    })
}

fn binaries_strategy(files: usize) -> impl Strategy<Value = Vec<ModelBinary>> {
    (1usize..=3).prop_flat_map(move |count| {
        let per = prop::collection::vec(test_strategy(files), count);
        per.prop_map(move |tests| {
            tests
                .into_iter()
                .enumerate()
                .map(|(index, tests)| ModelBinary {
                    id: format!("pkg::b{index}"),
                    target: format!("b{index}"),
                    kind: BinaryKind::Test,
                    tests,
                })
                .collect()
        })
    })
}

fn model() -> impl Strategy<Value = Model> {
    (1usize..=5).prop_flat_map(|files| {
        let edges = prop::collection::vec((0..files, 0..files), 0..=8);
        let modified = prop::collection::vec(0..files, 0..=3);
        let binaries = binaries_strategy(files);
        (edges, modified, binaries, any::<u64>()).prop_map(
            move |(edges, modified, binaries, seed)| Model {
                files,
                seed,
                edges,
                binaries,
                modified,
            },
        )
    })
}

/// Dépôt fixe : cycle `f0 → f1 → f2 → f0`, plus `f2 → f3`, `f4` isolé.
fn fixed_model() -> Model {
    Model {
        files: 5,
        seed: 7,
        edges: vec![(0, 1), (1, 2), (2, 0), (2, 3)],
        binaries: vec![
            ModelBinary {
                id: "pkg::b0".to_owned(),
                target: "b0".to_owned(),
                kind: BinaryKind::Test,
                tests: vec![
                    ModelTest {
                        name: "t0".to_owned(),
                        roots: vec![0],
                        status: BaseStatus::Pass,
                    },
                    ModelTest {
                        name: "t1".to_owned(),
                        roots: vec![3],
                        status: BaseStatus::Pass,
                    },
                    ModelTest {
                        name: "t2".to_owned(),
                        roots: Vec::new(),
                        status: BaseStatus::Pass,
                    },
                ],
            },
            ModelBinary {
                id: "pkg::b1".to_owned(),
                target: "b1".to_owned(),
                kind: BinaryKind::Test,
                tests: vec![
                    ModelTest {
                        name: "t0".to_owned(),
                        roots: vec![4],
                        status: BaseStatus::Pass,
                    },
                    ModelTest {
                        name: "t1".to_owned(),
                        roots: vec![4, 0],
                        status: BaseStatus::Pass,
                    },
                ],
            },
        ],
        modified: vec![1],
    }
}

// ---------------------------------------------------------------------------------------
// Construction des entrées de `plan` / `classify`
// ---------------------------------------------------------------------------------------

fn inventory(model: &Model) -> BaseInventory {
    BaseInventory {
        schema: BASE_SCHEMA.to_owned(),
        commit: COMMIT.to_owned(),
        toolchain: BTreeMap::new(),
        features: Vec::new(),
        build_env: BTreeMap::new(),
        green: true,
        binaries: model
            .binaries
            .iter()
            .map(|binary| BaseBinary {
                binary_id: binary.id.clone(),
                target_name: binary.target.clone(),
                kind: binary.kind,
                src_path: {
                    let target = &binary.target;
                    format!("tests/{target}.rs")
                },
                tests: binary
                    .tests
                    .iter()
                    .map(|test| BaseTest {
                        name: test.name.clone(),
                        status: test.status,
                        duration_ms: None,
                    })
                    .collect(),
            })
            .collect(),
        graph_digest: Some(DIGEST.to_owned()),
    }
}

/// Graphe du modèle. `binary_files` (dep-info) est réaliste : le fichier de la cible plus la
/// clôture des racines de ses tests. Un binaire ne compile donc pas les fichiers qu'aucun de
/// ses tests n'atteint, ce qui rend les cibles propriétaires non triviales.
fn graph(model: &Model) -> GraphData {
    let mut graph = GraphData {
        schema: GRAPH_SCHEMA.to_owned(),
        digest: DIGEST.to_owned(),
        ..GraphData::default()
    };
    for (from, to) in &model.edges {
        graph
            .edges
            .entry(file_name(*from))
            .or_default()
            .insert(file_name(*to));
    }
    for binary in &model.binaries {
        let target = &binary.target;
        let mut compiled: BTreeSet<RelPath> = BTreeSet::from([format!("tests/{target}.rs")]);
        for test in &binary.tests {
            compiled.extend(
                oracle_reach(&model.edges, &test.roots)
                    .into_iter()
                    .map(file_name),
            );
            graph.test_roots.insert(
                TestId {
                    binary_id: binary.id.clone(),
                    name: test.name.clone(),
                },
                test.roots.iter().copied().map(file_name).collect(),
            );
        }
        graph.binary_files.insert(binary.id.clone(), compiled);
    }
    graph
}

fn plan_for(model: &Model, d1: Option<&D1Proposal>) -> Plan {
    let scope = Scope::Partial(PartialScope {
        modified: model.modified.iter().copied().map(file_name).collect(),
        ..PartialScope::default()
    });
    plan(&scope, &inventory(model), &graph(model), d1)
}

#[allow(clippy::unnecessary_wraps)] // signature imposée par le lecteur de contenu
fn plain(_path: &str, _version: Version) -> Option<String> {
    Some("pub fn f() {}\n".to_owned())
}

fn classify_model(model: &Model, changes: Vec<Change>) -> Scope {
    let ctx = ClassifyContext {
        branch: Some("feature/x".to_owned()),
        ..ClassifyContext::default()
    };
    let changes = ChangeSet {
        base: COMMIT.to_owned(),
        changes,
    };
    classify(&changes, &inventory(model), &graph(model), &ctx, &plain)
}

// ---------------------------------------------------------------------------------------
// Oracle indépendant
// ---------------------------------------------------------------------------------------

/// Clôture de `edges` depuis `roots` par relaxation de point fixe : on répète le balayage
/// complet de la liste d'arêtes jusqu'à stabilité. C'est un second algorithme (le parcours
/// en profondeur de `reach` est l'autre), pas une copie de la fonction testée.
fn oracle_reach(edges: &[(usize, usize)], roots: &[usize]) -> BTreeSet<usize> {
    let mut reached: BTreeSet<usize> = roots.iter().copied().collect();
    loop {
        let mut changed = false;
        for (from, to) in edges {
            if reached.contains(from) && reached.insert(*to) {
                changed = true;
            }
        }
        if !changed {
            return reached;
        }
    }
}

/// Tests que l'oracle déclare impactés : racine inconnue (doute) ou clôture qui touche un
/// fichier modifié.
fn oracle_impacted(model: &Model) -> BTreeSet<TestId> {
    let mut impacted = BTreeSet::new();
    for binary in &model.binaries {
        for test in &binary.tests {
            let id = TestId {
                binary_id: binary.id.clone(),
                name: test.name.clone(),
            };
            if test.roots.is_empty() {
                impacted.insert(id);
                continue;
            }
            let reached = oracle_reach(&model.edges, &test.roots);
            if reached.iter().any(|file| model.modified.contains(file)) {
                impacted.insert(id);
            }
        }
    }
    impacted
}

// ---------------------------------------------------------------------------------------
// Aides
// ---------------------------------------------------------------------------------------

fn run_set(plan: &Plan) -> BTreeSet<TestId> {
    plan.entries
        .iter()
        .filter(|entry| entry.decision == Decision::Run)
        .map(|entry| entry.test.clone())
        .collect()
}

fn decision(plan: &Plan, test: &TestId) -> Decision {
    plan.entries
        .iter()
        .find(|entry| &entry.test == test)
        .unwrap_or_else(|| panic!("aucune entrée pour {test:?}"))
        .decision
}

fn inventory_ids(base: &BaseInventory) -> BTreeSet<TestId> {
    base.binaries
        .iter()
        .flat_map(|binary| {
            binary.tests.iter().map(|test| TestId {
                binary_id: binary.binary_id.clone(),
                name: test.name.clone(),
            })
        })
        .collect()
}

fn expect_partial(scope: Scope) -> PartialScope {
    match scope {
        Scope::Partial(partial) => partial,
        Scope::Everything { reasons } => panic!("Partial attendu, obtenu {reasons:?}"),
    }
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
    let inventory = inventory_ids(base);
    let journal = Journal {
        schema: JOURNAL_SCHEMA.to_owned(),
        status: JournalStatus::Planned,
        base_commit: plan.base_commit.clone(),
        graph_digest: plan.graph_digest.clone(),
        modified_files: plan.modified_files.clone(),
        scope,
        scope_reasons,
        counts: JournalCounts::from_entries(&plan.entries, inventory.len(), 0),
        entries: plan.entries.clone(),
        outcomes: Vec::new(),
        user_filter: None,
    };
    check_complete(&journal, &inventory, &BTreeSet::new())
        .unwrap_or_else(|error| panic!("journal refusé : {error}"));
}

/// Cible Cargo d'un binaire de la base (même correspondance que `plan::target_of`, privée).
fn target_of(binary: &BaseBinary) -> Option<NextestTarget> {
    match binary.kind {
        BinaryKind::Lib => Some(NextestTarget::Lib),
        BinaryKind::Bin => Some(NextestTarget::Bin(binary.target_name.clone())),
        BinaryKind::Test => Some(NextestTarget::Test(binary.target_name.clone())),
        BinaryKind::Other => None,
    }
}

/// Binaires de la base effectivement compilés par les cibles du plan (`None` : tous).
fn compiled_binaries(plan: &Plan, base: &BaseInventory) -> Option<BTreeSet<String>> {
    let targets = plan.targets.as_ref()?;
    Some(
        base.binaries
            .iter()
            .filter(|binary| target_of(binary).is_some_and(|target| targets.contains(&target)))
            .map(|binary| binary.binary_id.clone())
            .collect(),
    )
}

/// Clé de tri pseudo-aléatoire mais déterministe (finaliseur `splitmix64`).
fn order_key(seed: u64, index: usize) -> u64 {
    let index = u64::try_from(index).unwrap_or(u64::MAX);
    let mut z = seed ^ index.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn permuted<T: Clone>(items: &[T], seed: u64) -> Vec<T> {
    let mut keyed: Vec<(u64, T)> = items
        .iter()
        .enumerate()
        .map(|(index, item)| (order_key(seed, index), item.clone()))
        .collect();
    keyed.sort_by_key(|(key, _)| *key);
    keyed.into_iter().map(|(_, item)| item).collect()
}

fn permuted_model(model: &Model) -> Model {
    let mut binaries = permuted(&model.binaries, model.seed ^ 0xA5A5);
    for binary in &mut binaries {
        binary.tests = permuted(&binary.tests, model.seed ^ 0x5A5A);
    }
    Model {
        files: model.files,
        seed: model.seed,
        edges: permuted(&model.edges, model.seed ^ 0x1234),
        binaries,
        modified: permuted(&model.modified, model.seed ^ 0x4321),
    }
}

/// Clé stable d'un test : dépend de son identifiant, jamais de sa position dans le modèle.
fn id_key(seed: u64, id: &TestId) -> u64 {
    let mut hash = seed;
    for byte in id
        .binary_id
        .bytes()
        .chain(b"|".iter().copied())
        .chain(id.name.bytes())
    {
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3) ^ u64::from(byte);
    }
    hash
}

/// Proposition d1 déterministe et indépendante de l'ordre : ajouts, sauts, et un test
/// inconnu du binaire d'identifiant le plus petit.
fn d1_for(model: &Model) -> D1Proposal {
    let mut add = Vec::new();
    let mut skip = Vec::new();
    for binary in &model.binaries {
        for test in &binary.tests {
            let id = TestId {
                binary_id: binary.id.clone(),
                name: test.name.clone(),
            };
            match id_key(model.seed, &id) % 3 {
                0 => add.push(id),
                1 => skip.push(id),
                _ => {}
            }
        }
    }
    if let Some(first) = model.binaries.iter().min_by(|a, b| a.id.cmp(&b.id)) {
        add.push(TestId {
            binary_id: first.id.clone(),
            name: "brand_new".to_owned(),
        });
    }
    D1Proposal { add, skip }
}

/// Couverture synthétique : la clôture réelle du test plus un fichier choisi par la graine.
fn coverage_for(model: &Model) -> BTreeMap<TestId, BTreeSet<RelPath>> {
    let mut coverage = BTreeMap::new();
    for (bi, binary) in model.binaries.iter().enumerate() {
        for (ti, test) in binary.tests.iter().enumerate() {
            let id = TestId {
                binary_id: binary.id.clone(),
                name: test.name.clone(),
            };
            let mut files: BTreeSet<RelPath> = oracle_reach(&model.edges, &test.roots)
                .into_iter()
                .map(file_name)
                .collect();
            let files_count = u64::try_from(model.files).unwrap_or(1);
            let extra = order_key(model.seed, bi * 7 + ti) % files_count;
            files.insert(file_name(usize::try_from(extra).unwrap_or(0)));
            coverage.insert(id, files);
        }
    }
    coverage
}

// ---------------------------------------------------------------------------------------
// Cas nommés : formes que le brief exige de couvrir
// ---------------------------------------------------------------------------------------

#[test]
fn a_cycle_is_traversed_to_reach_a_modified_member() {
    let model = fixed_model();
    let plan = plan_for(&model, None);
    let t0 = TestId {
        binary_id: "pkg::b0".to_owned(),
        name: "t0".to_owned(),
    };
    assert_eq!(decision(&plan, &t0), Decision::Run);
    let entry = plan
        .entries
        .iter()
        .find(|entry| entry.test == t0)
        .expect("entrée t0");
    assert!(
        entry.reasons.contains(&Reason::ReachesModified {
            file: "src/f1.rs".to_owned(),
            via: vec!["src/f0.rs".to_owned(), "src/f1.rs".to_owned()],
        }),
        "{:?}",
        entry.reasons
    );
}

#[test]
fn several_roots_are_all_explored() {
    let model = fixed_model();
    let plan = plan_for(&model, None);
    let t1 = TestId {
        binary_id: "pkg::b1".to_owned(),
        name: "t1".to_owned(),
    };
    // `t1` a deux racines : `src/f4.rs` (isolé) et `src/f0.rs` (qui mène à `src/f1.rs`).
    // Seule la seconde atteint le fichier modifié : le test n'est lancé que si les deux
    // racines sont explorées.
    assert_eq!(decision(&plan, &t1), Decision::Run);
    let entry = plan
        .entries
        .iter()
        .find(|entry| entry.test == t1)
        .expect("entrée t1");
    assert!(
        entry.reasons.contains(&Reason::ReachesModified {
            file: "src/f1.rs".to_owned(),
            via: vec!["src/f0.rs".to_owned(), "src/f1.rs".to_owned()],
        }),
        "{:?}",
        entry.reasons
    );
}

#[test]
fn a_modified_file_the_graph_does_not_know_runs_only_the_doubtful_tests() {
    let model = fixed_model();
    let scope = Scope::Partial(PartialScope {
        modified: BTreeSet::from(["src/ghost.rs".to_owned()]),
        ..PartialScope::default()
    });
    let plan = plan(&scope, &inventory(&model), &graph(&model), None);
    let expected: BTreeSet<TestId> = BTreeSet::from([TestId {
        binary_id: "pkg::b0".to_owned(),
        name: "t2".to_owned(),
    }]);
    assert_eq!(
        run_set(&plan),
        expected,
        "seul le test sans racine est un doute"
    );
}

#[test]
fn a_build_input_change_runs_everything() {
    let model = fixed_model();
    let scope = classify_model(
        &model,
        vec![Change::Modified {
            path: "Cargo.toml".to_owned(),
        }],
    );
    let Scope::Everything { reasons } = scope else {
        panic!("Everything attendu");
    };
    assert!(reasons.contains(&Reason::BuildInput("Cargo.toml".to_owned())));
    let base = inventory(&model);
    let plan = plan(&Scope::Everything { reasons }, &base, &graph(&model), None);
    assert_eq!(run_set(&plan), inventory_ids(&base));
    assert!(
        plan.targets.is_none(),
        "portée « tout » : aucune cible partielle"
    );
}

#[test]
fn a_new_integration_test_file_runs_its_whole_new_binary() {
    let model = fixed_model();
    let partial = expect_partial(classify_model(
        &model,
        vec![Change::Added {
            path: "tests/fresh.rs".to_owned(),
        }],
    ));
    assert_eq!(
        partial.whole_binaries.get("pkg::fresh"),
        Some(&Reason::NewBinary("pkg::fresh".to_owned()))
    );
}

#[test]
fn a_rename_runs_the_old_binary_and_marks_both_paths() {
    let model = fixed_model();
    let partial = expect_partial(classify_model(
        &model,
        vec![Change::Renamed {
            from: "src/f2.rs".to_owned(),
            to: "src/f9.rs".to_owned(),
        }],
    ));
    assert!(partial.modified.contains("src/f2.rs"));
    assert!(partial.modified.contains("src/f9.rs"));
    assert_eq!(
        partial.whole_binaries.get("pkg::b0"),
        Some(&Reason::DeletedOrRenamed("src/f2.rs".to_owned()))
    );
}

#[test]
fn d1_add_of_a_known_test_runs_it_and_skip_never_removes_a_run() {
    let model = fixed_model();
    let skipped = TestId {
        binary_id: "pkg::b1".to_owned(),
        name: "t0".to_owned(),
    };
    let without = plan_for(&model, None);
    assert_eq!(decision(&without, &skipped), Decision::Skip);
    let proposal = D1Proposal {
        add: vec![skipped.clone()],
        skip: vec![skipped.clone()],
    };
    let with = plan_for(&model, Some(&proposal));
    assert_eq!(decision(&with, &skipped), Decision::Run);
    let entry = with
        .entries
        .iter()
        .find(|entry| entry.test == skipped)
        .expect("entrée");
    assert!(entry.reasons.contains(&Reason::D1Added));
    assert!(entry.reasons.contains(&Reason::D1SkipIgnored));
}

#[test]
fn owner_target_is_preserved_even_without_known_tests() {
    let mut model = fixed_model();
    model.binaries[0].tests.clear();
    let scope = Scope::Partial(PartialScope {
        modified: BTreeSet::from(["tests/b0.rs".to_owned()]),
        ..PartialScope::default()
    });
    let plan = plan(&scope, &inventory(&model), &graph(&model), None);
    assert!(run_set(&plan).is_empty());
    assert!(
        plan.targets
            .as_ref()
            .is_none_or(|targets| { targets.contains(&NextestTarget::Test("b0".to_owned())) })
    );
}

#[test]
fn a_plan_with_nothing_to_run_asks_nextest_for_none() {
    let model = Model {
        files: 2,
        seed: 1,
        edges: Vec::new(),
        binaries: vec![ModelBinary {
            id: "pkg::b0".to_owned(),
            target: "b0".to_owned(),
            kind: BinaryKind::Test,
            tests: vec![ModelTest {
                name: "t0".to_owned(),
                roots: vec![0],
                status: BaseStatus::Pass,
            }],
        }],
        modified: vec![1],
    };
    // Le binaire ne compile que `src/f0.rs` : le fichier modifié `src/f1.rs` ne le rend pas
    // cible propriétaire, et aucun test ne l'atteint.
    let scope = Scope::Partial(PartialScope {
        modified: BTreeSet::from([file_name(1)]),
        ..PartialScope::default()
    });
    let plan = plan(&scope, &inventory(&model), &graph(&model), None);
    assert!(run_set(&plan).is_empty());
    assert_eq!(plan.targets, Some(Vec::new()));
    assert_eq!(
        nextest_args(&plan, None),
        vec!["-E".to_owned(), "none()".to_owned()]
    );
}

// ---------------------------------------------------------------------------------------
// Propriétés
// ---------------------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Complétude : l'ensemble lancé est exactement celui de l'oracle indépendant, et le
    /// journal qui en découle est accepté par `check_complete`.
    #[test]
    fn selection_matches_the_independent_oracle(model in model()) {
        let plan = plan_for(&model, None);
        let run = run_set(&plan);
        prop_assert_eq!(&run, &oracle_impacted(&model));
        if let Some(targets) = &plan.targets {
            for binary in &model.binaries {
                let owns_modified = binary.tests.iter().any(|test| {
                    oracle_reach(&model.edges, &test.roots)
                        .iter()
                        .any(|file| model.modified.contains(file))
                });
                if owns_modified || run.iter().any(|test| test.binary_id == binary.id) {
                    prop_assert!(targets.contains(&NextestTarget::Test(binary.target.clone())));
                }
            }
        }
        assert_journal_complete(&plan, &inventory(&model));
    }

    /// Stabilité : permuter binaires, tests, arêtes, fichiers modifiés et propositions d1
    /// ne change pas le plan.
    #[test]
    fn the_plan_is_stable_under_permutations(model in model()) {
        let d1 = d1_for(&model);
        let reference = plan_for(&model, Some(&d1));
        let shuffled_model = permuted_model(&model);
        let permuted_d1 = D1Proposal {
            add: permuted(&d1.add, model.seed ^ 0xDEAD),
            skip: permuted(&d1.skip, model.seed ^ 0xBEEF),
        };
        prop_assert_eq!(plan_for(&shuffled_model, Some(&permuted_d1)), reference);
    }

    /// d1 ne peut qu'ajouter : aucun test lancé sans d1 ne devient sauté avec d1, et un saut
    /// proposé ne change jamais une décision.
    #[test]
    fn d1_can_only_add(model in model()) {
        let d1 = d1_for(&model);
        let without = plan_for(&model, None);
        let with = plan_for(&model, Some(&d1));
        prop_assert!(run_set(&with).is_superset(&run_set(&without)));
        for entry in &without.entries {
            if entry.decision == Decision::Run {
                prop_assert_eq!(decision(&with, &entry.test), Decision::Run);
            }
        }
        let add_only = D1Proposal {
            add: d1.add.clone(),
            skip: Vec::new(),
        };
        let without_skips = plan_for(&model, Some(&add_only));
        prop_assert_eq!(run_set(&with), run_set(&without_skips));
    }

    /// Le filtre `-E` généré laisse passer exactement les tests lancés des binaires compilés.
    #[test]
    fn the_generated_filter_matches_exactly_the_run_tests(model in model()) {
        let plan = plan_for(&model, None);
        let base = inventory(&model);
        if let Some(filter) = plan.filter.as_deref() {
            let parsed = FilterSet::parse(filter)
                .expect("le filtre généré doit rester dans le sous-ensemble supporté");
            let compiled = compiled_binaries(&plan, &base);
            for entry in &plan.entries {
                let is_compiled = compiled
                    .as_ref()
                    .is_none_or(|set| set.contains(&entry.test.binary_id));
                if is_compiled {
                    prop_assert_eq!(
                        parsed.matches(&entry.test.binary_id, &entry.test.name),
                        entry.decision == Decision::Run
                    );
                }
            }
        }
    }

    /// `apply` est idempotent, une seconde passe de `learn` ne trouve plus rien, et
    /// l'ensemble atteint ne rétrécit jamais.
    #[test]
    fn learn_and_apply_are_idempotent_and_monotone(model in model()) {
        let coverage = coverage_for(&model);
        let mut g = graph(&model);
        let before = g.test_roots.clone();
        let learned = learn(&g, &coverage);
        apply(&mut g, &learned);
        let once = g.clone();
        apply(&mut g, &learned);
        prop_assert_eq!(&g, &once);
        prop_assert!(learn(&g, &coverage).edges.is_empty());
        let original = graph(&model);
        for (test, roots) in &before {
            prop_assert!(g.test_roots[test].is_superset(roots));
            prop_assert!(
                reach(&g, &g.test_roots[test]).is_superset(&reach(&original, roots))
            );
        }
    }

    /// Un fichier supprimé lance en entier chaque binaire qui le compile, et seulement ceux-là.
    #[test]
    fn a_deleted_file_runs_exactly_the_binaries_that_compile_it(model in model()) {
        let path = file_name(0);
        let partial = expect_partial(classify_model(
            &model,
            vec![Change::Deleted { path: path.clone() }],
        ));
        prop_assert!(partial.modified.contains(&path));
        let g = graph(&model);
        for binary in &model.binaries {
            let expected = g.binary_files[&binary.id].contains(&path);
            let found = partial.whole_binaries.get(&binary.id);
            if expected {
                let wanted = Reason::DeletedOrRenamed(path.clone());
                prop_assert_eq!(found, Some(&wanted));
            } else {
                prop_assert!(found.is_none());
            }
        }
    }

    /// Un test non vert dans la base est forcé, quel que soit le statut.
    #[test]
    fn a_non_green_base_test_is_forced(status in prop_oneof![
        Just(BaseStatus::Pass),
        Just(BaseStatus::Ignored),
        Just(BaseStatus::Fail),
        Just(BaseStatus::Flaky),
        Just(BaseStatus::Unknown),
    ]) {
        let mut model = fixed_model();
        model.binaries[0].tests[1].status = status;
        let partial = expect_partial(classify_model(
            &model,
            vec![Change::Modified {
                path: "src/f1.rs".to_owned(),
            }],
        ));
        let id = TestId {
            binary_id: "pkg::b0".to_owned(),
            name: "t1".to_owned(),
        };
        if status == BaseStatus::Pass {
            prop_assert!(!partial.forced_tests.contains_key(&id));
        } else {
            let wanted = Reason::BaseNotGreen(status);
            prop_assert_eq!(partial.forced_tests.get(&id), Some(&wanted));
        }
    }
}
