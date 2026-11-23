//! Fuzzing borné des lecteurs d'inventaire : `cargo nextest list --message-format json`
//! (`select::base` et `select::graph::depinfo`) et `cargo metadata`.
//!
//! Le corpus (`tests/fixtures/select/fuzz/nextest`) contient une capture réelle de
//! `cargo nextest list` sur kioku (67 binaires, 634 cas, 59 ignorés) et des cas limites
//! par grammaire. Les mutations sont déterministes (graine fixe) et ne vérifient que des
//! invariants : aucune panique, et un document accepté ne cite que des chemins relatifs
//! au dépôt, sans identifiant ni test en double.

use std::collections::BTreeSet;
use std::path::Path;

use kioku::select::BaseStatus;
use kioku::select::base::{Metadata, parse_metadata, parse_nextest_list};
use kioku::select::graph::depinfo::parse_nextest_list as parse_nextest_suites;
use proptest::prelude::*;
use sha2::Digest as _;

const CORPUS: &str = "tests/fixtures/select/fuzz/nextest";
const EXPECTATIONS: &str = "tests/fixtures/select/fuzz/expectations-nextest.json";
const METADATA: &str = "tests/fixtures/select/fuzz/nextest/edge-metadata.json";
const REAL_LIST: &str = "tests/fixtures/select/fuzz/nextest/real-kioku-67suites.json";
const REAL_METADATA: &str = "tests/fixtures/select/fuzz/nextest/derived-metadata-67suites.json";

fn read(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn metadata() -> Metadata {
    parse_metadata(&read(METADATA)).expect("edge metadata")
}

/// Même règle que `is_repo_relative` de `select::learn` (copie privée là-bas) :
/// relatif, séparateur `/`, aucun composant vide, `.` ou `..`, pas de `\\`, de NUL ni de
/// lettre de lecteur.
fn is_repo_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains(['\\', '\0'])
        && path.as_bytes().get(1) != Some(&b':')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// Compare un document du corpus au verdict attendu du lecteur `select::base`.
fn check_base_reader(name: &str, expected: &serde_json::Value, text: &str, meta: &Metadata) {
    let parsed = parse_nextest_list(text, meta);
    let verdict = if parsed.is_ok() { "accept" } else { "reject" };
    assert_eq!(
        expected["base"]["verdict"].as_str(),
        Some(verdict),
        "{name} (base)"
    );
    let Ok(binaries) = parsed else {
        return;
    };
    let want: Vec<(&str, &str, Vec<&str>)> = expected["base"]["binaries"]
        .as_array()
        .expect("binaries")
        .iter()
        .map(|binary| {
            let tests = binary["tests"]
                .as_array()
                .expect("tests")
                .iter()
                .map(|test| test.as_str().expect("test"))
                .collect();
            (
                binary["binary_id"].as_str().expect("binary_id"),
                binary["src_path"].as_str().expect("src_path"),
                tests,
            )
        })
        .collect();
    let got: Vec<(&str, &str, Vec<&str>)> = binaries
        .iter()
        .map(|binary| {
            (
                binary.binary_id.as_str(),
                binary.src_path.as_str(),
                binary.tests.iter().map(|test| test.name.as_str()).collect(),
            )
        })
        .collect();
    assert_eq!(got, want, "{name} (base binaries)");
    for binary in &binaries {
        assert!(is_repo_relative(&binary.src_path), "{name} {binary:?}");
    }
}

/// Compare un document du corpus au verdict attendu du lecteur `select::graph::depinfo`.
fn check_depinfo_reader(name: &str, expected: &serde_json::Value, text: &str) {
    let parsed = parse_nextest_suites(text.as_bytes());
    let verdict = if parsed.is_ok() { "accept" } else { "reject" };
    assert_eq!(
        expected["depinfo"]["verdict"].as_str(),
        Some(verdict),
        "{name} (depinfo)"
    );
    let Ok(suites) = parsed else {
        return;
    };
    let want: Vec<(&str, Vec<&str>)> = expected["depinfo"]["suites"]
        .as_array()
        .expect("suites")
        .iter()
        .map(|suite| {
            let ignored = suite["ignored"]
                .as_array()
                .expect("ignored")
                .iter()
                .map(|test| test.as_str().expect("test"))
                .collect();
            (suite["binary_id"].as_str().expect("binary_id"), ignored)
        })
        .collect();
    let got: Vec<(&str, Vec<&str>)> = suites
        .iter()
        .map(|suite| {
            let ignored = suite
                .tests
                .iter()
                .filter(|(_, ignored)| **ignored)
                .map(|(test, _)| test.as_str())
                .collect();
            (suite.binary_id.as_str(), ignored)
        })
        .collect();
    assert_eq!(got, want, "{name} (depinfo suites)");
}

/// Nombre de fichiers `edge-*.json` du corpus, hors `edge-metadata.json`.
fn edge_case_files() -> usize {
    std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join(CORPUS))
        .expect("corpus")
        .filter_map(Result::ok)
        .filter(|entry| {
            let path = entry.path();
            path.extension()
                .is_some_and(|extension| extension == "json")
                && path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| stem.starts_with("edge-") && stem != "edge-metadata")
        })
        .count()
}

/// Chaque fichier du corpus est comparé à son verdict attendu, pour les deux lecteurs.
#[test]
fn nextest_corpus_matches_expectations() {
    let expectations: serde_json::Value =
        serde_json::from_str(&read(EXPECTATIONS)).expect("expectations json");
    let expectations = expectations.as_object().expect("object");
    let meta = metadata();
    for (name, expected) in expectations {
        let text = read(&format!("{CORPUS}/{name}"));
        let digest = format!("{:x}", sha2::Sha256::digest(text.as_bytes()));
        assert_eq!(expected["sha256"].as_str(), Some(digest.as_str()), "{name}");
        assert_eq!(
            expected["bytes"].as_u64(),
            Some(text.len() as u64),
            "{name} (taille)"
        );
        check_base_reader(name, expected, &text, &meta);
        check_depinfo_reader(name, expected, &text);
    }
    assert_eq!(
        edge_case_files(),
        expectations.len(),
        "corpus et attentes désynchronisés"
    );
}

/// La capture réelle est acceptée par les deux lecteurs, avec les comptes annoncés.
#[test]
fn real_nextest_capture_is_accepted_with_declared_counts() {
    let text = read(REAL_LIST);
    let meta = parse_metadata(&read(REAL_METADATA)).expect("derived metadata");
    let binaries = parse_nextest_list(&text, &meta).expect("real capture");
    assert_eq!(binaries.len(), 67);
    let tests: usize = binaries.iter().map(|binary| binary.tests.len()).sum();
    assert_eq!(tests, 634);
    let ignored = binaries
        .iter()
        .flat_map(|binary| &binary.tests)
        .filter(|test| test.status == BaseStatus::Ignored)
        .count();
    assert_eq!(ignored, 59);
    let suites = parse_nextest_suites(text.as_bytes()).expect("real capture (depinfo)");
    assert_eq!(suites.len(), 67);
    assert_eq!(
        suites.iter().map(|suite| suite.tests.len()).sum::<usize>(),
        634
    );
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        rng_seed: proptest::test_runner::RngSeed::Fixed(0x6e65_7874),
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// Mutations d'une capture réelle : aucune panique, et un document accepté reste
    /// cohérent (chemins relatifs, identifiants et tests uniques).
    #[test]
    fn nextest_mutations_never_panic_and_stay_consistent(
        ops in proptest::collection::vec((0u8..4, 0usize..200_000, any::<u8>()), 1..5),
    ) {
        let mut bytes = read(REAL_LIST).into_bytes();
        for (op, at, value) in ops {
            let at = at % (bytes.len() + 1);
            match op {
                0 => bytes.truncate(at),
                1 => bytes.insert(at, value),
                2 => {
                    if at < bytes.len() {
                        bytes.remove(at);
                    }
                }
                _ => {
                    if at < bytes.len() {
                        bytes[at] = value;
                    }
                }
            }
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let meta = parse_metadata(&read(REAL_METADATA)).expect("derived metadata");
        if let Ok(binaries) = parse_nextest_list(&text, &meta) {
            let mut ids = BTreeSet::new();
            for binary in &binaries {
                prop_assert!(ids.insert(binary.binary_id.clone()), "identifiant en double");
                prop_assert!(is_repo_relative(&binary.src_path), "{binary:?}");
                let mut names = BTreeSet::new();
                for test in &binary.tests {
                    prop_assert!(names.insert(test.name.clone()), "test en double");
                }
            }
        }
        if let Ok(suites) = parse_nextest_suites(text.as_bytes()) {
            let mut ids = BTreeSet::new();
            for suite in &suites {
                prop_assert!(ids.insert(suite.binary_id.clone()), "identifiant en double");
            }
        }
    }

    /// Un document JSON construit par grammaire (parfois valide, parfois corrompu) ne fait
    /// jamais paniquer les lecteurs ; un document accepté n'expose que des chemins relatifs
    /// au dépôt et ne perd ni ne duplique de binaire.
    #[test]
    fn generated_json_never_panics(
        suites in proptest::collection::vec((0u8..3, 0u8..4, any::<bool>()), 0..4),
        corrupt in proptest::collection::vec(0u8..6, 0..3),
    ) {
        let text = generated_document(&suites, &corrupt);
        let meta = metadata();
        if let Ok(binaries) = parse_nextest_list(&text, &meta) {
            let mut ids = BTreeSet::new();
            for binary in &binaries {
                prop_assert!(ids.insert(binary.binary_id.clone()), "binaire en double");
                prop_assert!(is_repo_relative(&binary.src_path), "{binary:?}");
            }
        }
        if let Ok(suites) = parse_nextest_suites(text.as_bytes())
            && let Ok(document) = serde_json::from_str::<serde_json::Value>(&text)
            && let Some(declared) = document["rust-suites"].as_object()
        {
            prop_assert_eq!(suites.len(), declared.len(), "binaires perdus ou inventés");
        }
        if let Ok(parsed) = parse_metadata(&text) {
            for target in &parsed.targets {
                prop_assert!(is_repo_relative(&target.src_path), "{target:?}");
            }
        }
    }
}

/// Construit un `cargo nextest list` JSON plausible pour `edge-metadata.json`, puis applique
/// les corruptions demandées. Sert à rendre la branche « accepté » réellement atteignable.
fn generated_document(suites: &[(u8, u8, bool)], corrupt: &[u8]) -> String {
    const KINDS: [(&str, &str); 3] = [("lib", "kioku"), ("bin", "kioku"), ("test", "cli")];
    let mut rust_suites = serde_json::Map::new();
    for (index, (kind, cases, ignored)) in suites.iter().enumerate() {
        let (kind, name) = KINDS[usize::from(*kind) % KINDS.len()];
        let binary_id = if index == 0 {
            "kdw-kioku".to_owned()
        } else {
            format!("kdw-kioku::{index}")
        };
        let mut testcases = serde_json::Map::new();
        for case in 0..*cases {
            testcases.insert(
                format!("tests::case_{case}"),
                serde_json::json!({"ignored": ignored, "kind": "test"}),
            );
        }
        rust_suites.insert(
            binary_id.clone(),
            serde_json::json!({
                "binary-id": binary_id,
                "binary-name": name,
                "binary-path": "/w/repo/target/debug/deps/x",
                "kind": kind,
                "package-name": "kdw-kioku",
                "status": "listed",
                "testcases": testcases,
            }),
        );
    }
    let mut document = serde_json::json!({"rust-suites": rust_suites});
    for corruption in corrupt {
        match corruption {
            0 => document["rust-suites"] = serde_json::json!([]),
            1 => document["test-count"] = serde_json::json!(999),
            2 => {
                if let Some(suite) = document["rust-suites"].as_object_mut()
                    && let Some((_, first)) = suite.iter_mut().next()
                {
                    first["status"] = serde_json::json!("skipped");
                }
            }
            3 => {
                if let Some(suite) = document["rust-suites"].as_object_mut()
                    && let Some((_, first)) = suite.iter_mut().next()
                {
                    first["kind"] = serde_json::json!("weird");
                }
            }
            4 => {
                if let Some(suite) = document["rust-suites"].as_object_mut()
                    && let Some((_, first)) = suite.iter_mut().next()
                {
                    first["testcases"] = serde_json::json!({"tests::x": {"ignored": "yes"}});
                }
            }
            _ => {
                if let Some(suite) = document["rust-suites"].as_object_mut()
                    && let Some((_, first)) = suite.iter_mut().next()
                {
                    first["binary-path"] = serde_json::json!(7);
                }
            }
        }
    }
    document.to_string()
}

/// La branche « accepté » de `generated_json_never_panics` est atteignable : un document
/// construit sans corruption est accepté par les deux lecteurs.
#[test]
fn generated_document_without_corruption_is_accepted() {
    let text = generated_document(&[(0, 2, false), (2, 1, true)], &[]);
    let meta = metadata();
    let binaries = parse_nextest_list(&text, &meta).expect("generated document (base)");
    assert_eq!(binaries.len(), 2);
    assert_eq!(
        binaries
            .iter()
            .map(|binary| binary.tests.len())
            .sum::<usize>(),
        3
    );
    let suites = parse_nextest_suites(text.as_bytes()).expect("generated document (depinfo)");
    assert_eq!(suites.len(), 2);
}
