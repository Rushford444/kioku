//! Lot L3 : filet couverture (`learn`, S6) et journal de sélection (S4, S8).
//!
//! Les graphes sont construits à la main ou chargés depuis les fixtures `kioku-941cec0-*`
//! (graphe SCIP + `#[path]` et couverture par test mesurés sur kioku `941cec0`, voir
//! `tests/fixtures/select/l3/gen_fixture.py`).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kioku::select::journal::{
    self, Journal, JournalCounts, JournalScope, JournalStatus, Outcome, TestOutcome,
    check_complete, final_status,
};
use kioku::select::learn::{
    self, CoverageOptions, LearnedEdges, apply, coverage_from_json, learn, run_coverage,
};
use kioku::select::{
    BaseStatus, Decision, GRAPH_SCHEMA, GraphData, JOURNAL_SCHEMA, JournalEntry, LEARNED_SCHEMA,
    Reason, RelPath, SelectError, TestId,
};

// ---------------------------------------------------------------------------------------
// Aides
// ---------------------------------------------------------------------------------------

fn tid(binary: &str, name: &str) -> TestId {
    TestId {
        binary_id: binary.to_owned(),
        name: name.to_owned(),
    }
}

fn set(items: &[&str]) -> BTreeSet<RelPath> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

fn graph(edges: &[(&str, &[&str])], roots: &[(&TestId, &[&str])]) -> GraphData {
    GraphData {
        schema: GRAPH_SCHEMA.to_owned(),
        digest: "digest-a".to_owned(),
        edges: edges
            .iter()
            .map(|(a, bs)| ((*a).to_owned(), set(bs)))
            .collect(),
        test_roots: roots.iter().map(|(t, r)| ((*t).clone(), set(r))).collect(),
        ..GraphData::default()
    }
}

/// Fichiers atteignables depuis les racines d'un test (clôture indépendante de `learn.rs`).
fn reach(g: &GraphData, test: &TestId) -> BTreeSet<RelPath> {
    let mut seen = BTreeSet::new();
    let mut todo: Vec<RelPath> = g
        .test_roots
        .get(test)
        .into_iter()
        .flatten()
        .cloned()
        .collect();
    while let Some(f) = todo.pop() {
        if seen.insert(f.clone()) {
            todo.extend(g.edges.get(&f).into_iter().flatten().cloned());
        }
    }
    seen
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/select/l3")
        .join(name)
}

fn split_key(key: &str) -> TestId {
    let (binary, name) = key.split_once(' ').expect("key with a space");
    tid(binary, name)
}

/// Graphe kioku 941cec0 : arêtes fichier → fichier et racines des 575 tests.
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

fn temp() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

// ---------------------------------------------------------------------------------------
// coverage_from_json
// ---------------------------------------------------------------------------------------

#[test]
fn coverage_key_is_split_at_the_first_space() {
    let cov = coverage_from_json(r#"{"kdw-kioku::cli a test with spaces": {"src/a.rs": [3, 4]}}"#)
        .expect("parses");
    let expected = tid("kdw-kioku::cli", "a test with spaces");
    assert_eq!(cov.keys().collect::<Vec<_>>(), vec![&expected]);
    assert_eq!(cov[&expected], set(&["src/a.rs"]));
}

#[test]
fn coverage_keeps_only_executed_repository_files() {
    let json = r#"{"b t": {
        "src/ok.rs": [1],
        "src/never_executed.rs": [],
        "/abs/path.rs": [1],
        "../outside.rs": [1],
        "src/../up.rs": [1],
        "./dot.rs": [1],
        "C:/win.rs": [1],
        "back\\slash.rs": [1],
        "": [1]
    },
    "b only_outside": {"/x.rs": [1]}}"#;
    let cov = coverage_from_json(json).expect("parses");
    assert_eq!(
        cov.len(),
        1,
        "a test left without repository file is omitted"
    );
    assert_eq!(cov[&tid("b", "t")], set(&["src/ok.rs"]));
}

#[test]
fn coverage_refuses_malformed_input() {
    for bad in [
        "not json",
        r#"{"nospace": {"src/a.rs": [1]}}"#,
        r#"{" leading": {"src/a.rs": [1]}}"#,
        r#"{"trailing ": {"src/a.rs": [1]}}"#,
        r#"{"b t": {"src/a.rs": "1"}}"#,
        r#"{"b t": ["src/a.rs"]}"#,
    ] {
        let err = coverage_from_json(bad).expect_err(bad);
        assert!(matches!(err, SelectError::Invalid { .. }), "{bad}: {err}");
    }
}

// ---------------------------------------------------------------------------------------
// learn / apply sur des graphes écrits à la main
// ---------------------------------------------------------------------------------------

#[test]
fn learn_adds_only_the_files_the_graph_does_not_reach() {
    let t = tid("b", "t");
    let g = graph(
        &[
            ("t.rs", &["a.rs"]),
            ("a.rs", &["b.rs"]),
            ("b.rs", &["a.rs"]),
        ], // cycle a <-> b
        &[(&t, &["t.rs"])],
    );
    let cov = BTreeMap::from([(t.clone(), set(&["t.rs", "a.rs", "b.rs", "hidden.rs"]))]);
    let learned = learn(&g, &cov);
    assert_eq!(learned.schema, LEARNED_SCHEMA);
    assert_eq!(learned.graph_digest, "digest-a");
    assert_eq!(
        learned.edges,
        BTreeMap::from([(t.clone(), set(&["hidden.rs"]))])
    );
    assert_eq!(learned.alerts.len(), 1);
    assert_eq!(learned.alerts[0].test, t);
    assert_eq!(learned.alerts[0].file.as_deref(), Some("hidden.rs"));
    assert!(learned.alerts[0].message.contains("hidden.rs"));
}

#[test]
fn learn_is_silent_when_the_graph_already_covers_the_coverage() {
    let t = tid("b", "t");
    let g = graph(&[("t.rs", &["a.rs"])], &[(&t, &["t.rs"])]);
    let cov = BTreeMap::from([(t, set(&["t.rs", "a.rs"]))]);
    let learned = learn(&g, &cov);
    assert!(learned.edges.is_empty());
    assert_eq!(learned.alerts.len(), 0);
}

#[test]
fn test_without_roots_raises_an_unknown_root_alert_and_learns_nothing() {
    let known = tid("b", "known");
    let absent = tid("b", "absent");
    let empty = tid("b", "empty");
    let g = graph(&[], &[(&known, &["k.rs"]), (&empty, &[])]);
    let cov = BTreeMap::from([
        (absent.clone(), set(&["x.rs"])),
        (empty.clone(), set(&["y.rs"])),
        (known, set(&["k.rs"])),
    ]);
    let learned = learn(&g, &cov);
    assert!(
        learned.edges.is_empty(),
        "nothing can be compared without a root"
    );
    let alerted: BTreeSet<&TestId> = learned.alerts.iter().map(|a| &a.test).collect();
    assert_eq!(alerted, BTreeSet::from([&absent, &empty]));
    for alert in &learned.alerts {
        assert!(alert.file.is_none());
        assert!(
            alert.message.contains("racine inconnue"),
            "{}",
            alert.message
        );
    }
}

#[test]
fn apply_adds_roots_with_learned_provenance() {
    let t = tid("b", "t");
    let mut g = graph(&[("t.rs", &["a.rs"])], &[(&t, &["t.rs"])]);
    let learned = learn(
        &g,
        &BTreeMap::from([(t.clone(), set(&["t.rs", "hidden.rs"]))]),
    );
    apply(&mut g, &learned);
    assert_eq!(g.test_roots[&t], set(&["t.rs", "hidden.rs"]));
    assert!(reach(&g, &t).contains("hidden.rs"));
    let key = "test:b t -> hidden.rs";
    assert_eq!(
        g.provenance.get(key),
        Some(&set(&["learned"])),
        "{:?}",
        g.provenance
    );
    // L'arête t.rs -> a.rs d'origine est intacte.
    assert_eq!(g.edges["t.rs"], set(&["a.rs"]));
}

#[test]
fn apply_never_gives_roots_to_a_test_the_graph_does_not_know() {
    let known = tid("b", "known");
    let unknown = tid("b", "unknown");
    let mut g = graph(&[], &[(&known, &["k.rs"])]);
    let learned = LearnedEdges {
        schema: LEARNED_SCHEMA.to_owned(),
        base_commit: String::new(),
        graph_digest: "digest-a".to_owned(),
        edges: BTreeMap::from([
            (unknown.clone(), set(&["x.rs"])),
            (known.clone(), set(&["y.rs"])),
        ]),
        alerts: vec![],
    };
    apply(&mut g, &learned);
    assert!(
        !g.test_roots.contains_key(&unknown),
        "an unknown-root test must stay unknown (doubt), not become 'rooted at the learned file only'"
    );
    assert_eq!(g.test_roots[&known], set(&["k.rs", "y.rs"]));
}

#[test]
fn apply_is_idempotent_and_relearning_converges() {
    let t = tid("b", "t");
    let mut g = graph(&[], &[(&t, &["t.rs"])]);
    let cov = BTreeMap::from([(t, set(&["t.rs", "h.rs"]))]);
    let learned = learn(&g, &cov);
    apply(&mut g, &learned);
    let once = g.clone();
    apply(&mut g, &learned);
    assert_eq!(g, once);
    assert!(
        learn(&g, &cov).edges.is_empty(),
        "a second pass finds nothing left to learn"
    );
}

#[test]
fn learned_edges_roundtrip_through_a_file_with_struct_keys() {
    let dir = temp();
    let path = dir.path().join("nested/learned.json");
    let t = tid("kdw-kioku::v2_r14_schema", "x");
    let mut learned = learn(
        &graph(&[], &[(&t, &["tests/v2_r14_schema.rs"])]),
        &BTreeMap::from([(t, set(&["src/ci2/domain/validate.rs"]))]),
    );
    learned.base_commit = "0123456789abcdef0123456789abcdef01234567".to_owned();
    learn::write(&path, &learned).expect("write");
    assert_eq!(learn::read(&path).expect("read"), learned);
    let leftovers: Vec<_> = fs::read_dir(path.parent().expect("parent"))
        .expect("dir")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(
        leftovers.len(),
        1,
        "no temporary file left behind: {leftovers:?}"
    );
}

#[test]
fn learned_edges_reject_a_wrong_schema_and_duplicate_tests() {
    let dir = temp();
    let path = dir.path().join("l.json");
    let mut learned = learn(&GraphData::default(), &BTreeMap::new());
    learned.schema = "kioku/select/learned/v0".to_owned();
    assert!(matches!(
        learn::write(&path, &learned),
        Err(SelectError::Invalid { .. })
    ));
    assert!(!path.exists(), "an invalid value is never written");

    fs::write(
        &path,
        r#"{"schema":"kioku/select/learned/v0","base_commit":"","graph_digest":"","edges":[],"alerts":[]}"#,
    )
    .expect("write");
    assert!(matches!(
        learn::read(&path),
        Err(SelectError::Invalid { .. })
    ));

    let dup = format!(
        r#"{{"schema":"{LEARNED_SCHEMA}","base_commit":"","graph_digest":"","edges":[
        {{"test":{{"binary_id":"b","name":"t"}},"files":["a.rs"]}},
        {{"test":{{"binary_id":"b","name":"t"}},"files":["c.rs"]}}],"alerts":[]}}"#
    );
    fs::write(&path, dup).expect("write");
    assert!(
        matches!(learn::read(&path), Err(SelectError::Invalid { message, .. }) if message.contains("duplicate test")),
        "duplicate test"
    );
}

// --- A4 : scellé d'intégrité (`edge_count` + `content_digest`) -----------------------------

const STAMP: &str = "0123456789abcdef0123456789abcdef01234567";

/// Arêtes apprises estampillées, avec deux tests, trois arêtes et trois alertes.
fn sealed_sample() -> LearnedEdges {
    let (t1, t2) = (tid("b", "t1"), tid("b", "t2"));
    let g = graph(&[], &[(&t1, &["t1.rs"]), (&t2, &["t2.rs"])]);
    let cov = BTreeMap::from([
        (t1, set(&["t1.rs", "x.rs", "y.rs"])),
        (t2, set(&["t2.rs", "z.rs"])),
    ]);
    let mut learned = learn(&g, &cov);
    STAMP.clone_into(&mut learned.base_commit);
    assert_eq!(learned.edges.values().map(BTreeSet::len).sum::<usize>(), 3);
    assert_eq!(learned.alerts.len(), 3);
    learned
}

fn write_sealed(path: &Path, learned: &LearnedEdges) -> serde_json::Value {
    learn::write(path, learned).expect("write");
    serde_json::from_slice(&fs::read(path).expect("read")).expect("json")
}

fn refuses(path: &Path, label: &str) {
    match learn::read(path) {
        Err(SelectError::Invalid { message, .. }) => assert!(
            message.contains("arêtes apprises incohérentes"),
            "{label} : {message}"
        ),
        other => panic!("{label} : refus attendu, obtenu {other:?}"),
    }
}

fn store_json(path: &Path, value: &serde_json::Value) {
    fs::write(path, serde_json::to_vec_pretty(value).expect("json")).expect("write");
}

#[test]
fn sealed_learned_edges_carry_a_count_and_a_digest_and_round_trip() {
    let dir = temp();
    let path = dir.path().join("l.json");
    let learned = sealed_sample();
    let doc = write_sealed(&path, &learned);
    assert_eq!(doc["schema"], "kioku/select/learned/v2");
    assert_eq!(doc["edge_count"], 3);
    let digest = doc["content_digest"].as_str().expect("digest");
    assert!(
        digest.starts_with("sha256:") && digest.len() == 7 + 64,
        "{digest}"
    );
    assert_eq!(learn::read(&path).expect("read"), learned);
    // Déterministe : même contenu, même digest.
    let again = write_sealed(&dir.path().join("l2.json"), &learned);
    assert_eq!(again["content_digest"], doc["content_digest"]);
    // Un autre contenu change le digest.
    let mut other = learned;
    other.base_commit = "f".repeat(40);
    let changed = write_sealed(&dir.path().join("l3.json"), &other);
    assert_ne!(changed["content_digest"], doc["content_digest"]);
}

#[test]
fn every_alteration_of_sealed_learned_edges_is_refused() {
    let dir = temp();
    let path = dir.path().join("l.json");
    let good = write_sealed(&path, &sealed_sample());
    let edit = |label: &str, change: &dyn Fn(&mut serde_json::Value)| {
        let mut doc = good.clone();
        change(&mut doc);
        assert_ne!(doc, good, "mutation sans effet : {label}");
        store_json(&path, &doc);
        refuses(&path, label);
    };
    edit("arêtes vidées, estampilles gardées", &|d| {
        d["edges"] = serde_json::json!([]);
    });
    edit("un test retiré", &|d| {
        d["edges"].as_array_mut().expect("edges").pop();
    });
    edit("un fichier retiré d'une arête", &|d| {
        let entry = d["edges"]
            .as_array_mut()
            .expect("edges")
            .iter_mut()
            .find(|e| e["files"].as_array().is_some_and(|f| f.len() > 1))
            .expect("test à deux fichiers");
        entry["files"].as_array_mut().expect("files").pop();
    });
    edit("un fichier changé", &|d| {
        d["edges"][0]["files"][0] = "autre.rs".into();
    });
    edit("edge_count changé", &|d| d["edge_count"] = 2.into());
    edit("edge_count et arêtes vidés, digest gardé", &|d| {
        d["edges"] = serde_json::json!([]);
        d["edge_count"] = 0.into();
    });
    edit("digest changé", &|d| {
        d["content_digest"] = format!("sha256:{}", "0".repeat(64)).into();
    });
    edit("digest sans préfixe", &|d| {
        let hex = d["content_digest"]
            .as_str()
            .expect("digest")
            .replace("sha256:", "");
        d["content_digest"] = hex.into();
    });
    edit("digest absent", &|d| {
        d.as_object_mut().expect("object").remove("content_digest");
    });
    edit("edge_count absent", &|d| {
        d.as_object_mut().expect("object").remove("edge_count");
    });
    edit("alerte modifiée", &|d| {
        d["alerts"][0]["message"] = "autre".into();
    });
    edit("alerte retirée", &|d| {
        d["alerts"].as_array_mut().expect("alerts").pop();
    });
    edit("estampille base_commit changée", &|d| {
        d["base_commit"] = "1".repeat(40).into();
    });
    edit("estampille graph_digest changée", &|d| {
        d["graph_digest"] = "digest-b".into();
    });
    // Fichier tronqué.
    store_json(&path, &good);
    let bytes = fs::read(&path).expect("read");
    fs::write(&path, &bytes[..bytes.len() / 2]).expect("write");
    assert!(matches!(
        learn::read(&path),
        Err(SelectError::Invalid { .. })
    ));
    // Intact : toujours lisible.
    store_json(&path, &good);
    learn::read(&path).expect("intact");
}

#[test]
fn learned_edges_in_the_old_v1_format_are_refused() {
    let dir = temp();
    let path = dir.path().join("l.json");
    let v1 = r#"{"schema":"kioku/select/learned/v1","base_commit":"","graph_digest":"","edges":[],"alerts":[]}"#;
    fs::write(&path, v1).expect("write");
    match learn::read(&path) {
        Err(SelectError::Invalid { message, .. }) => {
            assert!(
                message.contains("learned/v1") && message.contains("select learn"),
                "{message}"
            );
        }
        other => panic!("v1 refusé attendu : {other:?}"),
    }
    // Schéma v2 mais sans les champs du scellé.
    let unsealed = v1.replace("learned/v1", "learned/v2");
    fs::write(&path, unsealed).expect("write");
    refuses(&path, "v2 sans scellé");
}

// ---------------------------------------------------------------------------------------
// Preuve sur kioku (porte L3) : les 6 tests `validate.rs`
// ---------------------------------------------------------------------------------------

const VALIDATE: &str = "src/ci2/domain/validate.rs";

fn the_six_validate_tests() -> BTreeSet<TestId> {
    BTreeSet::from([
        tid(
            "kdw-kioku::v2_r14_schema",
            "input_selector_variant_refuses_an_undeclared_field",
        ),
        tid(
            "kdw-kioku::v2_r14_schema",
            "origin_claim_variant_refuses_an_undeclared_field",
        ),
        tid(
            "kdw-kioku::v2_schema_contracts_extra",
            "bounded_decode_accepts_the_small_valid_record",
        ),
        tid(
            "kdw-kioku::v2_schema_contracts_extra",
            "bounded_decode_refuses_bodies_over_the_size_cap",
        ),
        tid(
            "kdw-kioku::v2_schema_contracts_extra",
            "bounded_decode_refuses_trailing_content_but_allows_trailing_whitespace",
        ),
        tid(
            "kdw-kioku::v2_schema_contracts_extra",
            "bounded_decode_refuses_unknown_fields",
        ),
    ])
}

#[test]
fn kioku_fixture_has_the_575_tests_and_the_measured_coverage() {
    let g = kioku_graph();
    assert_eq!(g.test_roots.len(), 575);
    assert_eq!(
        g.edges.values().map(BTreeSet::len).sum::<usize>(),
        366,
        "SCIP + #[path] edges"
    );
    let cov = kioku_coverage();
    assert_eq!(cov.len(), 575);
    assert!(cov.keys().all(|t| g.test_roots.contains_key(t)));
}

#[test]
fn learn_catches_exactly_the_six_validate_rs_tests_on_kioku() {
    let g = kioku_graph();
    let learned = learn(&g, &kioku_coverage());
    let caught: BTreeSet<TestId> = learned.edges.keys().cloned().collect();
    assert_eq!(
        caught,
        the_six_validate_tests(),
        "the six tests of PLAFOND § 5, no more"
    );
    for (test, files) in &learned.edges {
        assert_eq!(files, &set(&[VALIDATE]), "{} {}", test.binary_id, test.name);
    }
    assert_eq!(learned.alerts.len(), 6);
    assert!(
        learned
            .alerts
            .iter()
            .all(|a| a.file.as_deref() == Some(VALIDATE))
    );
    // Avant : aucun des six n'atteint validate.rs ; après `apply` : tous l'atteignent.
    let mut after = g.clone();
    for t in &caught {
        assert!(
            !reach(&g, t).contains(VALIDATE),
            "{} {} was missed",
            t.binary_id,
            t.name
        );
    }
    apply(&mut after, &learned);
    for t in &caught {
        assert!(
            reach(&after, t).contains(VALIDATE),
            "{} {} caught",
            t.binary_id,
            t.name
        );
    }
    assert!(
        learn(&after, &kioku_coverage()).edges.is_empty(),
        "no miss left once applied"
    );
}

#[test]
fn apply_never_removes_anything_on_kioku() {
    let g = kioku_graph();
    let mut after = g.clone();
    apply(&mut after, &learn(&g, &kioku_coverage()));
    assert_eq!(after.edges, g.edges, "no edge is touched");
    for test in g.test_roots.keys() {
        let (before, now) = (reach(&g, test), reach(&after, test));
        assert!(
            now.is_superset(&before),
            "{} {} lost reach",
            test.binary_id,
            test.name
        );
        assert!(after.test_roots[test].is_superset(&g.test_roots[test]));
    }
    let widened = g
        .test_roots
        .keys()
        .filter(|t| reach(&after, t) != reach(&g, t))
        .count();
    assert_eq!(widened, 6, "only the six learned tests widen");
}

// ---------------------------------------------------------------------------------------
// run_coverage
// ---------------------------------------------------------------------------------------

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("mkdir");
    for entry in fs::read_dir(from).expect("read_dir") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy");
        }
    }
}

#[test]
fn run_coverage_fails_when_the_llvm_tools_are_missing() {
    let dir = temp();
    let opts = CoverageOptions {
        llvm_bin_dir: Some(dir.path().to_path_buf()),
        ..CoverageOptions::default()
    };
    let err = run_coverage(dir.path(), &opts).expect_err("no llvm tools");
    assert!(
        matches!(&err, SelectError::Command { command, .. } if command == "llvm-profdata"),
        "{err}"
    );
}

#[test]
fn run_coverage_fails_when_the_repository_is_missing() {
    let dir = temp();
    let err = run_coverage(&dir.path().join("absent"), &CoverageOptions::default())
        .expect_err("missing repo");
    assert!(matches!(err, SelectError::Io { .. }), "{err}");
}

#[cfg(unix)]
fn write_stub(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    fs::write(path, body).expect("stub");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod");
}

/// Faux `llvm-profdata`/`llvm-cov` : le « profil » fusionné est le marqueur que le test de la
/// fixture a écrit pour son propre indice, et l'« export » en fait un lcov.
#[cfg(unix)]
fn stub_llvm(dir: &Path, repo: &Path) {
    fs::create_dir_all(dir).expect("mkdir");
    write_stub(
        &dir.join("llvm-profdata"),
        "#!/bin/sh\nshift 2\nout=''\nfirst=''\nwhile [ $# -gt 0 ]; do\n  case \"$1\" in\n    -o) out=\"$2\"; shift 2;;\n    *) [ -z \"$first\" ] && first=\"$1\"; shift;;\n  esac\ndone\nd=$(dirname \"$first\"); b=$(basename \"$first\"); i=${b%%-*}\ncp \"$d/$i.mark\" \"$out\"\n",
    );
    let repo = repo.display();
    write_stub(
        &dir.join("llvm-cov"),
        &format!(
            "#!/bin/sh\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = '-instr-profile' ]; then p=\"$2\"; fi\n  shift\ndone\nwhile read -r f; do\n  case \"$f\" in\n    /*) echo \"SF:$f\";;\n    *) echo \"SF:{repo}/$f\";;\n  esac\n  echo 'DA:1,1'\n  echo 'end_of_record'\ndone < \"$p\"\n"
        ),
    );
}

#[cfg(unix)]
#[test]
fn run_coverage_end_to_end_on_a_tiny_crate_with_stub_llvm() {
    let work = temp();
    let repo = work.path().join("minicrate");
    copy_dir(&fixture("minicrate"), &repo);
    let test_file = repo.join("tests/it.rs");
    let source = fs::read_to_string(&test_file).expect("test fixture");
    fs::write(
        &test_file,
        source.replace("fn slow()", "#[ignore]\nfn slow()"),
    )
    .expect("ignore slow test for successful collection");
    let repo = repo.canonicalize().expect("canonical");
    let tools = work.path().join("llvm");
    stub_llvm(&tools, &repo);

    let opts = CoverageOptions {
        llvm_bin_dir: Some(tools),
        test_timeout: Duration::from_secs(3),
        timeout: Duration::from_secs(300),
        jobs: 2,
        ..CoverageOptions::default()
    };
    let cov = run_coverage(&repo, &opts).expect("coverage");

    let integration = |name: &str| tid("minicov::it", name);
    let expected_files = |own: &str| set(&["src/lib.rs", own, "tests/it.rs"]);
    assert_eq!(
        cov.get(&integration("uses_a")),
        Some(&expected_files("src/a.rs")),
        "{cov:?}"
    );
    assert_eq!(
        cov.get(&integration("uses_b")),
        Some(&expected_files("src/b.rs")),
        "{cov:?}"
    );
    assert!(
        !cov.contains_key(&integration("slow")),
        "the deliberately ignored slow test is not run"
    );
    assert!(
        !cov.contains_key(&integration("ignored_one")),
        "ignored tests are not run"
    );
    assert_eq!(
        cov.get(&tid("minicov", "tests::lib_unit")),
        Some(&set(&["src/lib.rs", "src/a.rs"])),
        "{cov:?}"
    );
    assert_eq!(
        cov.len(),
        3,
        "uses_a, uses_b and the lib unit test only: {cov:?}"
    );
    assert!(
        repo.join("target/select-cov").is_dir(),
        "separate cov target dir is used"
    );
    assert!(
        !repo.join("target/debug").exists(),
        "the regular target/debug is never used by the coverage run"
    );
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)] // Chaque scénario vérifie l’absence de fichier appris.
fn learn_run_failed_test_never_writes_a_learned_file() {
    use kioku::select::exec::Store;
    use kioku::select::{BASE_SCHEMA, BaseInventory, graph};
    use std::process::Command;

    let work = temp();
    let repo = work.path().join("repo");
    copy_dir(&fixture("minicrate"), &repo);
    fs::write(repo.join("src/lib.rs"), "pub mod a; pub mod b;\n").expect("lib");
    fs::write(
        repo.join("tests/it.rs"),
        "#[test] fn fails() { if std::env::var_os(\"KIOKU_FAIL_TEST\").is_some() { eprintln!(\"{}\", std::env::var(\"KIOKU_SYNTHETIC_SECRET\").unwrap()); panic!(\"failure\"); } }\n",
    )
    .expect("test");
    fs::write(repo.join(".gitignore"), "/target\n").expect("ignore target");
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .current_dir(&repo)
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
            ])
            .args(args)
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}: {:?}", out.stderr);
        String::from_utf8(out.stdout)
            .expect("utf8")
            .trim()
            .to_owned()
    };
    git(&["init", "-q"]);
    git(&[
        "add",
        "Cargo.toml",
        "Cargo.lock",
        "src",
        "tests",
        ".gitignore",
    ]);
    git(&["commit", "-q", "-m", "base"]);
    let commit = git(&["rev-parse", "HEAD"]);
    let store = Store::new(work.path().join("store"));
    let graph = GraphData {
        schema: GRAPH_SCHEMA.to_owned(),
        digest: "fixture".into(),
        ..GraphData::default()
    };
    fs::create_dir_all(store.root()).expect("store");
    fs::write(
        store.graph_path("fixture").expect("graph path"),
        graph::to_json(&graph).expect("graph json"),
    )
    .expect("graph");
    store
        .save_base(&BaseInventory {
            schema: BASE_SCHEMA.into(),
            commit: commit.clone(),
            toolchain: BTreeMap::new(),
            features: Vec::new(),
            build_env: BTreeMap::new(),
            green: true,
            binaries: Vec::new(),
            graph_digest: Some("fixture".into()),
        })
        .expect("base");
    let tools = work.path().join("tools");
    stub_llvm(&tools, &repo);
    write_stub(
        &tools.join("llvm-profdata"),
        "#!/bin/sh\nwhile [ $# -gt 0 ]; do if [ \"$1\" = -o ]; then : > \"$2\"; exit 0; fi; shift; done\nexit 1\n",
    );
    write_stub(
        &tools.join("llvm-cov"),
        "#!/bin/sh\ncase \"$KIOKU_EXPORT_MODE\" in empty) exit 0;; incomplete) printf 'SF:/tmp/file\\nend_of_record\\n';; truncated) printf 'SF:/tmp/file\\nDA:1,1\\n';; stderr) head -c 65537 /dev/zero >&2;; esac\n",
    );
    // Force tool discovery to use the stub directory even when llvm-tools is installed.
    write_stub(
        &tools.join("rustc"),
        "#!/bin/sh\nif [ \"$1\" = --print ] && [ \"$2\" = sysroot ]; then echo /nonexistent-kioku-test-sysroot; exit 0; fi\nPATH=\"$KIOKU_ORIGINAL_PATH\" exec rustc \"$@\"\n",
    );
    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let path =
        std::env::join_paths(std::iter::once(tools).chain(std::env::split_paths(&original_path)))
            .expect("path");
    let learned_path = store.learned_path(&commit).expect("path");
    for previous in [None, Some(b"previous learned bytes\n".as_slice())] {
        if let Some(bytes) = previous {
            fs::create_dir_all(learned_path.parent().expect("parent")).expect("learned dir");
            fs::write(&learned_path, bytes).expect("previous learned");
        }
        for (timeout, diagnostic, mode, fail) in [
            ("300", "test failed", "empty", true),
            ("0", "timeout", "empty", true),
            ("300", "invalid or incomplete export", "empty", false),
            ("300", "invalid or incomplete export", "incomplete", false),
            ("300", "invalid or incomplete export", "truncated", false),
            ("300", "stderr exceeded capture limit", "stderr", false),
        ] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_kioku"));
            if fail {
                command.env("KIOKU_FAIL_TEST", "1");
            }
            let out = command
                .current_dir(&repo)
                .env("KIOKU_EXPORT_MODE", mode)
                .env("KIOKU_ORIGINAL_PATH", &original_path)
                .env("PATH", &path)
                .env("CARGO_NET_OFFLINE", "true")
                .env("KIOKU_SYNTHETIC_SECRET", "synthetic-coverage-secret")
                .args([
                    "select",
                    "learn",
                    "--run",
                    "--timeout",
                    timeout,
                    "--repo",
                    ".",
                    "--store",
                ])
                .arg(store.root())
                .output()
                .expect("kioku");
            assert!(!out.status.success());
            assert!(
                String::from_utf8_lossy(&out.stderr).contains(diagnostic),
                "{:?}",
                out.stderr
            );
            assert!(!String::from_utf8_lossy(&out.stdout).contains("synthetic-coverage-secret"));
            assert!(!String::from_utf8_lossy(&out.stderr).contains("synthetic-coverage-secret"));
            if let Some(bytes) = previous {
                assert_eq!(fs::read(&learned_path).expect("previous retained"), bytes);
            } else {
                assert!(!learned_path.exists());
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn run_coverage_honours_the_global_timeout() {
    let work = temp();
    let repo = work.path().join("minicrate");
    copy_dir(&fixture("minicrate"), &repo);
    let tools = work.path().join("llvm");
    stub_llvm(&tools, &repo);
    let opts = CoverageOptions {
        llvm_bin_dir: Some(tools),
        timeout: Duration::from_millis(1),
        ..CoverageOptions::default()
    };
    let err = run_coverage(&repo, &opts).expect_err("timeout");
    assert!(matches!(err, SelectError::Timeout { .. }), "{err}");
}

// ---------------------------------------------------------------------------------------
// Journal
// ---------------------------------------------------------------------------------------

fn no_path() -> Reason {
    Reason::NoPathToModified {
        graph_digest: "g".to_owned(),
    }
}

fn reaches() -> Reason {
    Reason::ReachesModified {
        file: "src/a.rs".to_owned(),
        via: vec!["src/a.rs".to_owned()],
    }
}

fn entry(test: &TestId, decision: Decision, reasons: Vec<Reason>) -> JournalEntry {
    JournalEntry {
        test: test.clone(),
        decision,
        reasons,
    }
}

fn journal(
    status: JournalStatus,
    scope: JournalScope,
    scope_reasons: Vec<Reason>,
    entries: Vec<JournalEntry>,
    inventory: usize,
    new: usize,
) -> Journal {
    let outcomes = entries
        .iter()
        .filter(|e| e.decision == Decision::Run)
        .map(|e| TestOutcome {
            test: e.test.clone(),
            outcome: Outcome::Passed,
        })
        .collect();
    Journal {
        schema: JOURNAL_SCHEMA.to_owned(),
        status,
        base_commit: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        graph_digest: "g".to_owned(),
        modified_files: vec!["src/a.rs".to_owned()],
        scope,
        scope_reasons,
        counts: JournalCounts::from_entries(&entries, inventory, new),
        entries,
        outcomes,
        user_filter: None,
    }
}

struct Small {
    inventory: BTreeSet<TestId>,
    new: BTreeSet<TestId>,
    journal: Journal,
}

/// Inventaire {a, b, c} + nouveau {n} : a lancé (atteint), b sauté, c lancé (base rouge), n lancé.
fn small() -> Small {
    let [a, b, c, n] = ["a", "b", "c", "n"].map(|x| tid("bin", x));
    let entries = vec![
        entry(&a, Decision::Run, vec![reaches()]),
        entry(&b, Decision::Skip, vec![no_path()]),
        entry(
            &c,
            Decision::Run,
            vec![Reason::BaseNotGreen(BaseStatus::Fail)],
        ),
        entry(&n, Decision::Run, vec![Reason::NewTest]),
    ];
    Small {
        inventory: BTreeSet::from([a, b, c]),
        new: BTreeSet::from([n]),
        journal: journal(
            JournalStatus::PartialGreen,
            JournalScope::Partial,
            vec![],
            entries,
            3,
            1,
        ),
    }
}

fn expect_invalid(journal: &Journal, s: &Small, needle: &str) {
    match check_complete(journal, &s.inventory, &s.new) {
        Err(SelectError::Invalid { message, .. }) => {
            assert!(
                message.contains(needle),
                "{message:?} should mention {needle:?}"
            );
        }
        other => panic!("expected Invalid({needle}), got {other:?}"),
    }
}

#[test]
fn complete_journal_passes_and_counts_add_up() {
    let s = small();
    check_complete(&s.journal, &s.inventory, &s.new).expect("complete");
    let c = s.journal.counts;
    assert_eq!((c.inventory, c.new, c.run, c.skipped), (3, 1, 3, 1));
    assert_eq!(
        c.run + c.skipped,
        c.inventory + c.new,
        "skipped + run = inventory ∪ new"
    );
}

#[test]
fn missing_test_is_an_error() {
    let mut s = small();
    s.journal.entries.retain(|e| e.test.name != "b");
    s.journal.counts = JournalCounts::from_entries(&s.journal.entries, 3, 1);
    expect_invalid(&s.journal, &s, "missing");
    // Un nouveau test oublié est aussi un test manquant.
    let mut s = small();
    s.journal.entries.retain(|e| e.test.name != "n");
    s.journal.counts = JournalCounts::from_entries(&s.journal.entries, 3, 1);
    expect_invalid(&s.journal, &s, "bin n");
}

#[test]
fn duplicate_entry_is_an_error() {
    let mut s = small();
    let twin = s.journal.entries[1].clone();
    s.journal.entries.push(twin);
    s.journal.counts = JournalCounts::from_entries(&s.journal.entries, 3, 1);
    expect_invalid(&s.journal, &s, "duplicate");
    // Même avec le bon nombre d'entrées : un doublon qui masque un manquant reste refusé.
    let mut s = small();
    s.journal.entries[2] = s.journal.entries[0].clone(); // c remplacé par un double de a
    s.journal.counts = JournalCounts::from_entries(&s.journal.entries, 3, 1);
    expect_invalid(&s.journal, &s, "duplicate");
}

#[test]
fn entry_without_reason_is_an_error() {
    for index in 0..4 {
        let mut s = small();
        s.journal.entries[index].reasons.clear();
        expect_invalid(&s.journal, &s, "no reason");
    }
}

#[test]
fn unknown_test_is_an_error() {
    let mut s = small();
    s.journal
        .entries
        .push(entry(&tid("bin", "ghost"), Decision::Run, vec![reaches()]));
    s.journal.counts = JournalCounts::from_entries(&s.journal.entries, 3, 1);
    expect_invalid(&s.journal, &s, "ghost");
}

#[test]
fn skip_needs_a_skip_reason_and_no_reason_to_run() {
    let mut s = small();
    s.journal.entries[1].reasons = vec![Reason::NewTest];
    expect_invalid(&s.journal, &s, "no skip reason");

    let mut s = small();
    s.journal.entries[1].reasons = vec![no_path(), reaches()];
    expect_invalid(&s.journal, &s, "reason to run");

    let mut s = small();
    s.journal.entries[0].reasons = vec![no_path()];
    expect_invalid(&s.journal, &s, "only skip reasons");

    // d1 ne peut pas sauter : sa proposition ignorée n'est pas une raison de sauter.
    let mut s = small();
    s.journal.entries[1].reasons = vec![Reason::D1SkipIgnored];
    expect_invalid(&s.journal, &s, "no skip reason");
}

#[test]
fn counts_must_match_the_entries_and_the_inventory() {
    let mut s = small();
    s.journal.counts.skipped = 0;
    expect_invalid(&s.journal, &s, "counts");
    let mut s = small();
    s.journal.counts.inventory = 2;
    expect_invalid(&s.journal, &s, "counts");
    let mut s = small();
    s.new = BTreeSet::new();
    expect_invalid(&s.journal.clone(), &s, "not in the inventory nor new");
    let mut s = small();
    s.new.insert(tid("bin", "a")); // même test dans l'inventaire et nouveau
    expect_invalid(&s.journal.clone(), &s, "both in the base inventory and new");
}

#[test]
fn everything_scope_cannot_skip_and_needs_a_reason() {
    let reasons = vec![Reason::ProtectedBranch("main".to_owned())];
    let a = tid("bin", "a");
    let b = tid("bin", "b");
    let inventory = BTreeSet::from([a.clone(), b.clone()]);
    let none = BTreeSet::new();

    let run_all = vec![
        entry(&a, Decision::Run, reasons.clone()),
        entry(&b, Decision::Run, reasons.clone()),
    ];
    let good = journal(
        JournalStatus::FullGreen,
        JournalScope::Everything,
        reasons.clone(),
        run_all,
        2,
        0,
    );
    check_complete(&good, &inventory, &none).expect("full run");

    let with_skip = vec![
        entry(&a, Decision::Run, reasons.clone()),
        entry(&b, Decision::Skip, vec![no_path()]),
    ];
    let bad = journal(
        JournalStatus::PartialGreen,
        JournalScope::Everything,
        reasons.clone(),
        with_skip,
        2,
        0,
    );
    assert!(
        check_complete(&bad, &inventory, &none).is_err(),
        "everything never skips"
    );

    let run_all = vec![
        entry(&a, Decision::Run, reasons.clone()),
        entry(&b, Decision::Run, reasons),
    ];
    let unexplained = journal(
        JournalStatus::Red,
        JournalScope::Everything,
        vec![],
        run_all,
        2,
        0,
    );
    assert!(
        check_complete(&unexplained, &inventory, &none).is_err(),
        "everything needs a reason"
    );
}

// --- S8 ----------------------------------------------------------------------------------

#[test]
fn full_green_requires_everything_scope_and_zero_skips() {
    use JournalScope::{Everything, Partial};
    use JournalStatus::{FullGreen, Incomplete, PartialGreen, Red};
    // complet + vert : seulement « tout » sans saut est full-green.
    assert_eq!(final_status(Everything, 0, true, false), FullGreen);
    assert_eq!(
        final_status(Partial, 0, true, false),
        PartialGreen,
        "partial scope, nothing skipped"
    );
    assert_eq!(final_status(Partial, 7, true, false), PartialGreen);
    assert_eq!(
        final_status(Everything, 7, true, false),
        PartialGreen,
        "a skip is never full-green"
    );
    // rouge et incomplet l'emportent, incomplet d'abord.
    assert_eq!(final_status(Everything, 0, true, true), Red);
    assert_eq!(final_status(Partial, 3, true, true), Red);
    assert_eq!(final_status(Everything, 0, false, true), Incomplete);
    assert_eq!(final_status(Partial, 3, false, false), Incomplete);
    // Jamais full-green hors de « tout » et 0 saut, sur toute la table.
    for scope in [Everything, Partial] {
        for skipped in [0, 1, 575] {
            for complete in [true, false] {
                for red in [true, false] {
                    let status = final_status(scope, skipped, complete, red);
                    assert!(
                        status != FullGreen
                            || (scope == Everything && skipped == 0 && complete && !red)
                    );
                }
            }
        }
    }
}

#[test]
fn a_journal_that_overstates_its_status_is_refused_everywhere() {
    let dir = temp();
    let path = dir.path().join("j.json");
    // full-green avec portée partielle.
    let mut s = small();
    s.journal.status = JournalStatus::FullGreen;
    expect_invalid(&s.journal, &s, "full-green");
    assert!(matches!(
        journal::write(&path, &s.journal),
        Err(SelectError::Invalid { .. })
    ));
    assert!(!path.exists());
    // full-green, portée « tout », mais un saut dans les entrées.
    let mut s = small();
    s.journal.status = JournalStatus::FullGreen;
    s.journal.scope = JournalScope::Everything;
    s.journal.scope_reasons = vec![Reason::Error("x".to_owned())];
    assert!(check_complete(&s.journal, &s.inventory, &s.new).is_err());
    // full-green, portée « tout », aucun saut mais comptes `skipped` > 0 : refusé aussi.
    let mut s = small();
    s.journal.entries[1] = entry(
        &tid("bin", "b"),
        Decision::Run,
        vec![Reason::Error("x".to_owned())],
    );
    s.journal.outcomes.push(TestOutcome {
        test: tid("bin", "b"),
        outcome: Outcome::Passed,
    });
    s.journal.status = JournalStatus::FullGreen;
    s.journal.scope = JournalScope::Everything;
    s.journal.scope_reasons = vec![Reason::Error("x".to_owned())];
    s.journal.counts = JournalCounts::from_entries(&s.journal.entries, 3, 1);
    check_complete(&s.journal, &s.inventory, &s.new).expect("a real full run is full-green");
    s.journal.counts.skipped = 1;
    assert!(check_complete(&s.journal, &s.inventory, &s.new).is_err());
}

// --- écriture / lecture -------------------------------------------------------------------

#[test]
fn journal_roundtrips_atomically_with_every_reason_variant() {
    let all_reasons = vec![
        Reason::ProtectedBranch("main".to_owned()),
        Reason::Error("boom".to_owned()),
        Reason::BuildInput("Cargo.toml".to_owned()),
        Reason::UnreferencedNonRust("src/x.py".to_owned()),
        Reason::Macro("src/m.rs".to_owned()),
        Reason::BuildEnvironment("RUSTFLAGS".to_owned()),
        Reason::UnknownRustFile("examples/e.rs".to_owned()),
        reaches(),
        Reason::NewBinary("kdw-kioku::new".to_owned()),
        Reason::DeletedOrRenamed("src/old.rs".to_owned()),
        Reason::ExternalInput("/abs/file".to_owned()),
        Reason::BaseNotGreen(BaseStatus::Flaky),
        Reason::NewTest,
        Reason::Learned {
            file: VALIDATE.to_owned(),
        },
        Reason::D1Added,
        no_path(),
        Reason::D1SkipIgnored,
    ];
    let mut s = small();
    s.journal.scope_reasons = all_reasons.clone();
    s.journal.entries[0].reasons = all_reasons;
    let dir = temp();
    let path = dir.path().join("out/journal.json");
    journal::write(&path, &s.journal).expect("write");
    let back = journal::read(&path).expect("read");
    assert_eq!(back, s.journal);
    // Réécrire remplace sans laisser de temporaire.
    journal::write(&path, &s.journal).expect("rewrite");
    let names: Vec<_> = fs::read_dir(path.parent().expect("parent"))
        .expect("dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["journal.json".to_owned()]);
    let text = fs::read_to_string(&path).expect("text");
    assert!(text.contains("\"status\": \"partial-green\""), "{text}");
    assert!(text.contains("\"scope\": \"partial\""), "{text}");
}

#[test]
fn read_refuses_bad_schema_garbage_and_oversized_status_lies() {
    let dir = temp();
    let path = dir.path().join("j.json");
    let s = small();
    journal::write(&path, &s.journal).expect("write");
    let good = fs::read_to_string(&path).expect("text");

    fs::write(&path, good.replace(JOURNAL_SCHEMA, "kioku/select/v0")).expect("w");
    assert!(
        matches!(journal::read(&path), Err(SelectError::Invalid { .. })),
        "schema"
    );

    fs::write(&path, good.replace("partial-green", "full-green")).expect("w");
    assert!(
        matches!(journal::read(&path), Err(SelectError::Invalid { .. })),
        "status lie"
    );

    fs::write(&path, "{ not json").expect("w");
    assert!(
        matches!(journal::read(&path), Err(SelectError::Invalid { .. })),
        "garbage"
    );

    fs::write(&path, good.replace("partial-green", "weird-green")).expect("w");
    assert!(
        matches!(journal::read(&path), Err(SelectError::Invalid { .. })),
        "unknown status"
    );

    assert!(matches!(
        journal::read(&dir.path().join("none.json")),
        Err(SelectError::Io { .. })
    ));
}

// --- journal complet sur l'inventaire réel de kioku ----------------------------------------

#[test]
fn journal_over_the_kioku_inventory_is_complete_for_a_real_selection() {
    let g = kioku_graph();
    let inventory: BTreeSet<TestId> = g.test_roots.keys().cloned().collect();
    assert_eq!(inventory.len(), 575);
    // Modification de src/ci2/runtime/mod.rs (le commit c9a59ca du plafond).
    let modified = "src/ci2/runtime/mod.rs";
    let new_test = tid("kdw-kioku::brand_new", "freshly_added");
    let new = BTreeSet::from([new_test.clone()]);
    let mut entries = Vec::new();
    for test in &inventory {
        if reach(&g, test).contains(modified) {
            entries.push(entry(
                test,
                Decision::Run,
                vec![Reason::ReachesModified {
                    file: modified.to_owned(),
                    via: vec![modified.to_owned()],
                }],
            ));
        } else {
            entries.push(entry(
                test,
                Decision::Skip,
                vec![Reason::NoPathToModified {
                    graph_digest: g.digest.clone(),
                }],
            ));
        }
    }
    entries.push(entry(&new_test, Decision::Run, vec![Reason::NewTest]));
    let j = journal(
        JournalStatus::PartialGreen,
        JournalScope::Partial,
        vec![],
        entries,
        575,
        1,
    );
    check_complete(&j, &inventory, &new).expect("every test accounted for exactly once");
    assert_eq!(j.counts.run + j.counts.skipped, 576);
    assert!(
        j.counts.skipped > 0 && j.counts.run > 1,
        "a real partial selection: {:?}",
        j.counts
    );

    // Cas d'échec sur ce journal : un test retiré, un test dupliqué.
    let mut missing = j.clone();
    let dropped = missing.entries.remove(100);
    missing.counts = JournalCounts::from_entries(&missing.entries, 575, 1);
    let err = check_complete(&missing, &inventory, &new).expect_err("missing");
    assert!(err.to_string().contains(&dropped.test.name), "{err}");
    let mut duplicated = j;
    duplicated.entries.push(duplicated.entries[7].clone());
    duplicated.counts = JournalCounts::from_entries(&duplicated.entries, 575, 1);
    assert!(check_complete(&duplicated, &inventory, &new).is_err());
}
