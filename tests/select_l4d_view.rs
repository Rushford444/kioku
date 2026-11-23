//! Lot L4d : `graph::selection_view` — la vue de sélection ignore les arêtes `mod` pures.
//!
//! Tests purs sur des graphes construits à la main, plus une mesure `#[ignore]` sur kioku
//! lui-même (sous le verrou des builds lourds).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use kioku::select::graph::{GraphOptions, build, selection_view};
use kioku::select::learn::{apply, coverage_from_json, learn};
use kioku::select::{GRAPH_SCHEMA, GraphData, TestId};

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(ToString::to_string).collect()
}

fn test_id(binary: &str, name: &str) -> TestId {
    TestId {
        binary_id: binary.to_string(),
        name: name.to_string(),
    }
}

fn add(graph: &mut GraphData, from: &str, to: &str, sources: &[&str]) {
    graph
        .edges
        .entry(from.to_string())
        .or_default()
        .insert(to.to_string());
    graph
        .provenance
        .insert(format!("{from} -> {to}"), set(sources));
}

fn base_graph() -> GraphData {
    let mut graph = GraphData {
        schema: GRAPH_SCHEMA.to_string(),
        digest: "digest-complet".to_string(),
        ..GraphData::default()
    };
    add(&mut graph, "src/lib.rs", "src/only_mod.rs", &["mod"]);
    add(
        &mut graph,
        "src/lib.rs",
        "src/mod_and_scip.rs",
        &["mod", "scip"],
    );
    add(&mut graph, "src/lib.rs", "src/via_path.rs", &["path"]);
    add(&mut graph, "src/lib.rs", "src/via_include.rs", &["include"]);
    add(&mut graph, "src/lib.rs", "data/x.json", &["literal"]);
    add(&mut graph, "tests/t.rs", "src/main.rs", &["cli"]);
    add(&mut graph, "src/only_scip.rs", "src/lib.rs", &["scip"]);
    add(&mut graph, "src/a.rs", "src/b.rs", &["mod", "learned"]);
    add(&mut graph, "src/a.rs", "src/c.rs", &["mod", "path"]);
    add(
        &mut graph,
        "src/a.rs",
        "src/d.rs",
        &["mod", "include", "literal"],
    );
    graph
        .binary_files
        .insert("pkg::lib".into(), set(&["src/lib.rs", "src/only_mod.rs"]));
    graph
        .literal_refs
        .insert("data/x.json".into(), set(&["src/lib.rs"]));
    graph
        .external_inputs
        .insert("/abs/ARRET.txt".into(), set(&["src/lib.rs"]));
    graph
        .test_roots
        .insert(test_id("pkg", "t::un"), set(&["src/lib.rs"]));
    graph
}

fn targets(graph: &GraphData, from: &str) -> BTreeSet<String> {
    graph.edges.get(from).cloned().unwrap_or_default()
}

#[test]
fn pure_mod_edge_is_removed() {
    let view = selection_view(&base_graph());
    assert!(!targets(&view, "src/lib.rs").contains("src/only_mod.rs"));
    assert!(
        !view
            .provenance
            .contains_key("src/lib.rs -> src/only_mod.rs")
    );
}

#[test]
fn mod_with_scip_is_kept() {
    let view = selection_view(&base_graph());
    assert!(targets(&view, "src/lib.rs").contains("src/mod_and_scip.rs"));
    assert_eq!(
        view.provenance["src/lib.rs -> src/mod_and_scip.rs"],
        set(&["mod", "scip"]),
        "la provenance d'une arête gardée est inchangée"
    );
}

#[test]
fn every_other_provenance_is_kept() {
    let view = selection_view(&base_graph());
    assert!(targets(&view, "src/lib.rs").contains("src/via_path.rs"));
    assert!(targets(&view, "src/lib.rs").contains("src/via_include.rs"));
    assert!(targets(&view, "src/lib.rs").contains("data/x.json"));
    assert!(targets(&view, "tests/t.rs").contains("src/main.rs"));
    assert!(targets(&view, "src/only_scip.rs").contains("src/lib.rs"));
    assert_eq!(
        targets(&view, "src/a.rs"),
        set(&["src/b.rs", "src/c.rs", "src/d.rs"]),
        "mod+learned, mod+path, mod+include+literal restent"
    );
}

#[test]
fn edge_without_provenance_is_kept_fail_closed() {
    let mut graph = base_graph();
    graph
        .edges
        .entry("src/z.rs".into())
        .or_default()
        .insert("src/y.rs".into());
    let view = selection_view(&graph);
    assert!(targets(&view, "src/z.rs").contains("src/y.rs"));
}

#[test]
fn a_node_left_with_no_edge_disappears_from_the_map() {
    let mut graph = GraphData::default();
    add(&mut graph, "src/lib.rs", "src/child.rs", &["mod"]);
    let view = selection_view(&graph);
    assert!(view.edges.is_empty(), "{:?}", view.edges);
    assert!(view.provenance.is_empty());
}

#[test]
fn everything_but_edges_and_their_provenance_is_copied() {
    let graph = base_graph();
    let view = selection_view(&graph);
    assert_eq!(view.digest, "digest-complet", "digest inchangé");
    assert_eq!(view.schema, graph.schema);
    assert_eq!(view.binary_files, graph.binary_files);
    assert_eq!(view.literal_refs, graph.literal_refs);
    assert_eq!(view.external_inputs, graph.external_inputs);
    assert_eq!(view.test_roots, graph.test_roots);
}

#[test]
fn the_view_is_a_subgraph_and_the_union_graph_is_untouched() {
    let graph = base_graph();
    let before = graph.clone();
    let view = selection_view(&graph);
    assert_eq!(graph, before, "le graphe complet reste l'union (audit)");
    for (from, to) in &view.edges {
        assert!(to.is_subset(&graph.edges[from]));
    }
    // Chaque arête de la vue a sa provenance, identique ; aucune provenance orpheline.
    for (key, sources) in &view.provenance {
        assert_eq!(&graph.provenance[key], sources);
    }
    let edge_keys: BTreeSet<String> = view
        .edges
        .iter()
        .flat_map(|(from, to)| to.iter().map(move |to| format!("{from} -> {to}")))
        .collect();
    assert_eq!(
        view.provenance.keys().cloned().collect::<BTreeSet<_>>(),
        edge_keys
    );
}

#[test]
fn applying_the_view_twice_changes_nothing() {
    let once = selection_view(&base_graph());
    assert_eq!(selection_view(&once), once);
}

#[test]
fn learned_provenance_entries_survive() {
    let mut graph = base_graph();
    graph
        .provenance
        .insert("test:pkg t::un -> src/x.rs".into(), set(&["learned"]));
    let view = selection_view(&graph);
    assert_eq!(
        view.provenance["test:pkg t::un -> src/x.rs"],
        set(&["learned"])
    );
}

// ---------------------------------------------------------------------------------------
// Mesure sur kioku lui-même (manuelle : `--run-ignored only`, sous le verrou des builds lourds)
// ---------------------------------------------------------------------------------------

fn closure(graph: &GraphData, roots: &BTreeSet<String>) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut todo: Vec<String> = roots.iter().cloned().collect();
    while let Some(file) = todo.pop() {
        if seen.insert(file.clone()) {
            todo.extend(graph.edges.get(&file).into_iter().flatten().cloned());
        }
    }
    seen
}

/// Tests de `graph` qui atteignent chaque `src/**` (clé : fichier ; valeur : nombre de tests).
fn reach_counts(graph: &GraphData) -> BTreeMap<String, usize> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for roots in graph.test_roots.values() {
        for file in closure(graph, roots) {
            if file.starts_with("src/") {
                *counts.entry(file).or_insert(0) += 1;
            }
        }
    }
    counts
}

fn stats_line(label: &str, counts: &BTreeMap<String, usize>, total: usize) {
    let mut values: Vec<usize> = counts.values().copied().collect();
    values.sort_unstable();
    let sum: usize = values.iter().sum();
    #[allow(clippy::cast_precision_loss)]
    let mean = sum as f64 / values.len().max(1) as f64;
    let median = values.get(values.len() / 2).copied().unwrap_or(0);
    let max = values.last().copied().unwrap_or(0);
    println!(
        "| {label} | {} fichiers src | {mean:.1} | {median} | {max} | sur {total} tests |",
        values.len()
    );
}

fn missed_by(
    graph: &GraphData,
    cov: &BTreeMap<TestId, BTreeSet<String>>,
) -> BTreeMap<TestId, BTreeSet<String>> {
    let mut missed = BTreeMap::new();
    for (test, files) in cov {
        let Some(roots) = graph.test_roots.get(test) else {
            continue;
        };
        let reached = closure(graph, roots);
        let lost: BTreeSet<String> = files
            .iter()
            .filter(|file| file.starts_with("src/") && !reached.contains(*file))
            .cloned()
            .collect();
        if !lost.is_empty() {
            missed.insert(test.clone(), lost);
        }
    }
    missed
}

#[test]
#[ignore = "mesure sur le dépôt réel : cargo nextest run --test select_l4d_view --run-ignored only --all-features (sous flock)"]
fn kioku_selection_view_evaluation() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let opts = GraphOptions {
        all_features: true,
        ..GraphOptions::default()
    };
    let full = build(&repo, &opts).unwrap();
    let mut view = selection_view(&full);
    let count = |g: &GraphData| g.edges.values().map(BTreeSet::len).sum::<usize>();
    println!(
        "arêtes : complet {} -> vue {} (retirées {})",
        count(&full),
        count(&view),
        count(&full) - count(&view)
    );
    assert_eq!(view.digest, full.digest);

    let cov_text = std::fs::read_to_string(repo.join("scratch/optA/cov/lines.json")).unwrap();
    let cov = coverage_from_json(&cov_text).unwrap();
    let missed_full = missed_by(&full, &cov);
    let missed_view = missed_by(&view, &cov);
    for (label, missed) in [("complet", &missed_full), ("vue", &missed_view)] {
        println!("ratés ({label}) : {} tests", missed.len());
        for (test, files) in missed {
            println!("  {} {} -> {files:?}", test.binary_id, test.name);
        }
    }
    assert_eq!(missed_full.len(), 6, "{missed_full:?}");
    assert_eq!(
        missed_view.keys().collect::<Vec<_>>(),
        missed_full.keys().collect::<Vec<_>>(),
        "mêmes ratés avec et sans la vue"
    );
    assert!(
        missed_view
            .values()
            .flatten()
            .all(|file| file == "src/ci2/domain/validate.rs"),
        "{missed_view:?}"
    );

    let total = view.test_roots.len();
    println!("| graphe | fichiers | moyenne | médiane | max | |");
    stats_line("complet", &reach_counts(&full), total);
    stats_line("vue", &reach_counts(&view), total);
    for file in ["src/ci2/runtime/mod.rs", "src/lib.rs", "src/ci2/mod.rs"] {
        println!(
            "  {file}: complet {:?} vue {:?}",
            reach_counts(&full).get(file),
            reach_counts(&view).get(file)
        );
    }

    // Fichiers `src/**` atteints par un test dans le graphe complet mais plus dans la vue.
    let (reach_full, reach_view) = (reach_counts(&full), reach_counts(&view));
    let dropped: Vec<&String> = reach_full
        .keys()
        .filter(|file| !reach_view.contains_key(*file))
        .collect();
    println!("fichiers src atteints seulement par arêtes mod : {dropped:?}");
    let executed: BTreeSet<&String> = cov.values().flatten().collect();
    for file in &dropped {
        println!(
            "  {file} exécuté par la couverture : {}",
            executed.contains(file)
        );
    }

    // Filet S6 : après `learn::apply` des arêtes apprises sur la vue, plus aucun raté.
    let learned = learn(&view, &cov);
    apply(&mut view, &learned);
    let missed_after = missed_by(&view, &cov);
    println!(
        "ratés après filet : {} (alertes {})",
        missed_after.len(),
        learned.alerts.len()
    );
    assert!(missed_after.is_empty(), "{missed_after:?}");
    stats_line("vue + filet", &reach_counts(&view), total);
}
