//! Lot L2 : graphe fichier → fichier (`kioku::select::graph`).
//!
//! Les tests purs (décodeur protobuf, dep-info, analyse syntaxique, arbre de modules, JSON)
//! tournent partout. Les tests « fixture » construisent un vrai petit paquet Cargo dans un
//! dossier temporaire et lancent `cargo`, `cargo nextest` et `rust-analyzer` (installés dans
//! l'environnement de build du projet ; leur absence fait échouer le test, jamais l'ignore).
//! Le test `#[ignore]` en fin de fichier mesure le graphe de kioku lui-même.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kioku::select::graph::depinfo::{parse_dep_info, repo_files};
use kioku::select::graph::scan::{
    IncludeKind, ModuleTree, build_tree, decode_string_literal, normalize_join, scan_source,
};
use kioku::select::graph::scip::{
    FieldValue, ScipDocument, decode_index, file_edges, read_field, read_varint,
};
use kioku::select::graph::{
    GraphOptions, build_with_stats, digest, from_json, load_or_build, provenance_counts, to_json,
};
use kioku::select::{GRAPH_SCHEMA, GraphData, SelectError, TestId};

// ---------------------------------------------------------------------------------------
// Décodeur protobuf SCIP
// ---------------------------------------------------------------------------------------

fn varint(mut value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let byte = u8::try_from(value & 0x7f).unwrap();
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return out;
        }
        out.push(byte | 0x80);
    }
}

fn field_bytes(number: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = varint((number << 3) | 2);
    out.extend(varint(payload.len() as u64));
    out.extend_from_slice(payload);
    out
}

fn field_varint(number: u64, value: u64) -> Vec<u8> {
    let mut out = varint(number << 3);
    out.extend(varint(value));
    out
}

fn occurrence(symbol: &str, roles: u64) -> Vec<u8> {
    let mut out = field_bytes(2, symbol.as_bytes());
    out.extend(field_varint(3, roles));
    out
}

fn document(path: &str, occurrences: &[Vec<u8>]) -> Vec<u8> {
    let mut out = field_bytes(1, path.as_bytes());
    for item in occurrences {
        out.extend(field_bytes(2, item));
    }
    out
}

/// Index : un champ `metadata` (1) à sauter, puis un champ par document (2).
fn index(documents: &[Vec<u8>]) -> Vec<u8> {
    let mut out = field_bytes(1, b"tool-info-to-skip");
    for item in documents {
        out.extend(field_bytes(2, item));
    }
    out
}

#[test]
fn varints_roundtrip_and_reject_bad_input() {
    for value in [
        0,
        1,
        127,
        128,
        300,
        16_383,
        16_384,
        u64::from(u32::MAX),
        u64::MAX,
    ] {
        let bytes = varint(value);
        let mut pos = 0;
        assert_eq!(read_varint(&bytes, &mut pos).unwrap(), value);
        assert_eq!(pos, bytes.len());
    }
    // vide, tronqué (bit de continuation sans suite), trop long (11 octets)
    assert!(read_varint(&[], &mut 0).is_err());
    assert!(read_varint(&[0x80], &mut 0).is_err());
    assert!(read_varint(&[0xff; 10], &mut 0).is_err());
    assert!(read_varint(&[0x80; 11], &mut 0).is_err());
}

#[test]
fn fields_are_bounded() {
    // longueur annoncée supérieure aux octets restants
    let mut pos = 0;
    assert!(read_field(&[0x12, 0x05, b'a'], &mut pos).is_err());
    // longueur énorme (pas de dépassement d'entier)
    let mut huge = vec![0x12];
    huge.extend(varint(u64::MAX));
    assert!(read_field(&huge, &mut 0).is_err());
    // numéro de champ nul, groupes (types 3 et 4), type de fil inconnu (6)
    assert!(read_field(&[0x00, 0x01], &mut 0).is_err());
    for wire in [3u8, 4, 6, 7] {
        assert!(read_field(&[(1 << 3) | wire, 0x00], &mut 0).is_err());
    }
    // valeurs fixes : complètes (sautées) ou tronquées (erreur)
    let mut pos = 0;
    let fixed64: Vec<u8> = [vec![(1 << 3) | 1], vec![0u8; 8]].concat();
    assert_eq!(
        read_field(&fixed64, &mut pos).unwrap(),
        (1, FieldValue::Fixed)
    );
    assert_eq!(pos, fixed64.len());
    assert!(read_field(&fixed64[..8], &mut 0).is_err());
    let fixed32: Vec<u8> = [vec![(2 << 3) | 5], vec![0u8; 4]].concat();
    assert_eq!(
        read_field(&fixed32, &mut 0).unwrap(),
        (2, FieldValue::Fixed)
    );
    assert!(read_field(&fixed32[..4], &mut 0).is_err());
    // varint et octets
    assert_eq!(
        read_field(&[0x08, 0x96, 0x01], &mut 0).unwrap(),
        (1, FieldValue::Varint(150))
    );
    assert_eq!(
        read_field(&[0x12, 0x02, b'h', b'i'], &mut 0).unwrap(),
        (2, FieldValue::Bytes(b"hi"))
    );
}

#[test]
fn index_decoding_keeps_definitions_and_references_only() {
    let bytes = index(&[
        document(
            "src/a.rs",
            &[
                occurrence("rust-analyzer cargo k 1 a/f().", 1),
                occurrence("rust-analyzer cargo k 1 b/g().", 0),
                occurrence("local 12", 0),
                occurrence("local 13", 1),
                occurrence("rust-analyzer cargo k 1 b/", 0), // segment de module
                occurrence("", 0),
            ],
        ),
        document(
            "src/b.rs",
            &[occurrence("rust-analyzer cargo k 1 b/g().", 1)],
        ),
    ]);
    let documents = decode_index(&bytes).unwrap();
    assert_eq!(documents.len(), 2);
    let a = &documents[0];
    assert_eq!(a.path, "src/a.rs");
    assert_eq!(
        a.definitions,
        BTreeSet::from(["rust-analyzer cargo k 1 a/f().".to_string()])
    );
    assert_eq!(
        a.references,
        BTreeSet::from(["rust-analyzer cargo k 1 b/g().".to_string()])
    );
    assert_eq!(documents[1].path, "src/b.rs");
    assert_eq!(documents[1].definitions.len(), 1);
    assert!(documents[1].references.is_empty());
}

#[test]
fn truncated_index_is_an_error_never_a_partial_read() {
    let bytes = index(&[
        document(
            "src/a.rs",
            &[occurrence("sym A", 1), occurrence("sym B", 0)],
        ),
        document("src/b.rs", &[occurrence("sym B", 1)]),
    ]);
    // Frontières des champs de premier niveau : seules coupes qui donnent un index valide.
    let mut boundaries = BTreeSet::from([0usize]);
    let mut pos = 0;
    while pos < bytes.len() {
        read_field(&bytes, &mut pos).unwrap();
        boundaries.insert(pos);
    }
    assert!(decode_index(&bytes).is_ok());
    for cut in 0..bytes.len() {
        let result = decode_index(&bytes[..cut]);
        if boundaries.contains(&cut) {
            assert!(
                result.is_ok(),
                "cut at a field boundary ({cut}) is a shorter index"
            );
        } else {
            assert!(
                result.is_err(),
                "cut inside a field ({cut}) must be an error"
            );
        }
    }
    // Un document interne incohérent (occurrence plus longue que son document) est refusé.
    let broken_occurrence = [vec![0x12, 0x40], b"short".to_vec()].concat();
    let broken_document = field_bytes(2, &field_bytes(1, b"src/x.rs"));
    let mut broken = field_bytes(
        2,
        &[field_bytes(1, b"src/x.rs"), broken_occurrence].concat(),
    );
    assert!(decode_index(&broken).is_err());
    broken.clear();
    broken.extend(broken_document);
    assert!(decode_index(&broken).is_ok());
    // Chemin non UTF-8.
    let bad_path = field_bytes(2, &field_bytes(1, &[0xff, 0xfe]));
    assert!(decode_index(&bad_path).is_err());
}

fn doc(path: &str, defs: &[&str], refs: &[&str]) -> ScipDocument {
    ScipDocument {
        path: path.to_string(),
        definitions: defs.iter().map(ToString::to_string).collect(),
        references: refs.iter().map(ToString::to_string).collect(),
    }
}

#[test]
fn homonymous_symbols_only_link_when_the_filter_allows_it() {
    // Deux crates de test qui définissent chacun un `setup()` : même symbole SCIP.
    let documents = [
        doc("tests/alpha.rs", &["k setup()."], &["k setup()."]),
        doc("tests/beta.rs", &["k setup()."], &["k setup()."]),
        doc("src/lib.rs", &["k core::compute()."], &[]),
        doc("src/main.rs", &[], &["k core::compute()."]),
    ];
    let everything = file_edges(&documents, &|_, _| true);
    assert!(
        everything["tests/alpha.rs"].contains("tests/beta.rs"),
        "bruit brut sans filtre"
    );
    assert!(everything["tests/beta.rs"].contains("tests/alpha.rs"));

    // Filtre « un test ne dépend que de la lib ou de lui-même ».
    let crate_of = |path: &str| path.to_string();
    let filtered = file_edges(&documents, &|user, definer| {
        definer.starts_with("src/") || crate_of(user) == crate_of(definer)
    });
    assert!(!filtered.contains_key("tests/alpha.rs"));
    assert!(!filtered.contains_key("tests/beta.rs"));
    assert_eq!(
        filtered["src/main.rs"],
        BTreeSet::from(["src/lib.rs".to_string()])
    );
}

#[test]
fn documents_of_the_same_path_are_merged() {
    // Un fichier compilé dans deux crates apparaît deux fois dans l'index.
    let documents = [
        doc("src/shared.rs", &["k shared()."], &[]),
        doc("src/user.rs", &[], &["k shared()."]),
        doc("src/user.rs", &[], &["k other()."]),
        doc("src/other.rs", &["k other()."], &[]),
    ];
    let edges = file_edges(&documents, &|_, _| true);
    assert_eq!(
        edges["src/user.rs"],
        BTreeSet::from(["src/shared.rs".to_string(), "src/other.rs".to_string()])
    );
}

// ---------------------------------------------------------------------------------------
// dep-info
// ---------------------------------------------------------------------------------------

#[test]
fn dep_info_unescapes_spaces_and_keeps_the_first_rule() {
    let text = "target/debug/deps/foo-1a2b: src/lib.rs src/with\\ space.rs \\\n  tests/../src/x.rs /abs/repo/src/y.rs \\\n  /outside/z.rs target/debug/build/out.rs\n\
# env-dep:CARGO_PKG_NAME=foo\n\
src/lib.rs:\n\
src/with\\ space.rs:\n";
    let dependencies = parse_dep_info(text).unwrap();
    assert_eq!(
        dependencies,
        vec![
            "src/lib.rs",
            "src/with space.rs",
            "tests/../src/x.rs",
            "/abs/repo/src/y.rs",
            "/outside/z.rs",
            "target/debug/build/out.rs",
        ]
    );
    let root = Path::new("/abs/repo");
    let files = repo_files(&dependencies, &[root]);
    assert_eq!(
        files,
        BTreeSet::from([
            "src/lib.rs".to_string(),
            "src/with space.rs".to_string(),
            "src/x.rs".to_string(),
            "src/y.rs".to_string(),
        ]),
        "hors dépôt et target/ ignorés, chemins normalisés"
    );
}

#[test]
fn dep_info_without_a_rule_is_suspect() {
    assert!(parse_dep_info("").is_err());
    assert!(parse_dep_info("# env-dep:X=1\n").is_err());
    assert!(parse_dep_info("src/a.rs src/b.rs\n").is_err());
}

#[test]
fn dep_info_escapes_hash_backslash_and_dollar() {
    let dependencies = parse_dep_info("t: a\\#b.rs c\\\\d.rs e$$f.rs\n").unwrap();
    assert_eq!(dependencies, vec!["a#b.rs", "c\\d.rs", "e$f.rs"]);
}

#[test]
fn repo_files_never_escape_the_repository() {
    let root = Path::new("/abs/repo");
    let files = repo_files(
        &[
            "../outside.rs".to_string(),
            "src/../../outside.rs".to_string(),
            ".git/config".to_string(),
            "/abs/repo/../elsewhere.rs".to_string(),
            "/abs/repo/target/debug/x.rs".to_string(),
            "src/ok.rs".to_string(),
        ],
        &[root],
    );
    assert_eq!(files, BTreeSet::from(["src/ok.rs".to_string()]));
    assert_eq!(
        normalize_join("tests", "../src/x.rs").as_deref(),
        Some("src/x.rs")
    );
    assert_eq!(normalize_join("", "../x.rs"), None);
}

// ---------------------------------------------------------------------------------------
// Analyse syntaxique et arbre de modules
// ---------------------------------------------------------------------------------------

#[test]
fn scan_finds_mods_paths_includes_literals_and_cli() {
    let source = br##"
mod plain;
#[path = "elsewhere/other.rs"]
mod renamed;
#[cfg_attr(unix, path = "unix.rs")]
mod conditional;
mod outer {
    mod inner;
}
mod inline_only { fn f() {} }
const A: &str = include_str!("../assets/note.txt");
const B: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/data/blob.bin"));
include!("generated.rs");
const SCRIPT: &str = "scripts/run.py";
const RAW: &str = r#"raw "quoted" text"#;
const ESC: &str = "tab\there \u{e9}";
fn run() { let _ = env!("CARGO_BIN_EXE_kioku"); }
"##;
    let scan = scan_source(source).unwrap();
    let names: Vec<(&str, &[String], &[String])> = scan
        .mods
        .iter()
        .map(|m| {
            (
                m.name.as_str(),
                m.path_attrs.as_slice(),
                m.inline.as_slice(),
            )
        })
        .collect();
    assert_eq!(names.len(), 4);
    assert_eq!(names[0], ("plain", &[][..], &[][..]));
    assert_eq!(names[1].0, "renamed");
    assert_eq!(names[1].1, ["elsewhere/other.rs".to_string()]);
    assert_eq!(names[2].0, "conditional");
    assert_eq!(names[2].1, ["unix.rs".to_string()]);
    assert_eq!(names[3].0, "inner");
    assert_eq!(names[3].2, ["outer".to_string()]);
    assert!(scan.inline_mods.contains(&vec!["outer".to_string()]));
    assert!(scan.inline_mods.contains(&vec!["inline_only".to_string()]));

    let kinds: Vec<IncludeKind> = scan.includes.iter().map(|i| i.kind).collect();
    assert_eq!(
        kinds,
        [IncludeKind::Str, IncludeKind::Bytes, IncludeKind::Include]
    );
    assert_eq!(scan.includes[0].literals, ["../assets/note.txt"]);
    assert_eq!(
        scan.includes[1].literals,
        ["CARGO_MANIFEST_DIR", "/data/blob.bin"]
    );
    assert!(scan.literals.contains("scripts/run.py"));
    assert!(scan.literals.contains("raw \"quoted\" text"));
    assert!(
        scan.literals.contains("tab\there \\u{e9}"),
        "\\u{{..}} reste verbatim (jamais pris pour un chemin)"
    );
    assert!(scan.launches_cli);
}

#[test]
fn scan_cli_detection_has_no_false_positive_on_the_bare_prefix() {
    assert!(
        !scan_source(br#"const P: &str = "CARGO_BIN_EXE_";"#)
            .unwrap()
            .launches_cli
    );
    assert!(!scan_source(b"fn main() {}").unwrap().launches_cli);
    assert!(
        scan_source(b"fn t() { let _ = Command::cargo_bin(\"x\"); }")
            .unwrap()
            .launches_cli
    );
    assert!(
        scan_source(b"fn t() { let _ = assert_cmd::cargo_bin_cmd!(\"x\"); }")
            .unwrap()
            .launches_cli
    );
}

#[test]
fn scan_rejects_syntax_errors_and_absurd_nesting() {
    assert!(scan_source(b"fn broken( {").is_err());
    let deep = format!(
        "fn f() {{ let _ = {}1{}; }}",
        "(".repeat(5_000),
        ")".repeat(5_000)
    );
    assert!(
        scan_source(deep.as_bytes()).is_err(),
        "arbre trop profond : erreur, pas de pile épuisée"
    );
}

#[test]
fn string_literals_decode_simple_escapes_and_keep_the_rest_verbatim() {
    assert_eq!(decode_string_literal("\"a\\\"b\"").as_deref(), Some("a\"b"));
    assert_eq!(
        decode_string_literal("r#\"x\\y\"#").as_deref(),
        Some("x\\y")
    );
    assert_eq!(
        decode_string_literal("\"a\\u{41}\"").as_deref(),
        Some("a\\u{41}")
    );
    assert_eq!(
        decode_string_literal("\"t\\tn\\n\"").as_deref(),
        Some("t\tn\n")
    );
}

fn scans_of(files: &[(&str, &str)]) -> BTreeMap<String, kioku::select::graph::scan::FileScan> {
    files
        .iter()
        .map(|(path, source)| ((*path).to_string(), scan_source(source.as_bytes()).unwrap()))
        .collect()
}

fn tree_of(root: &str, files: &[(&str, &str)]) -> ModuleTree {
    let scans = scans_of(files);
    let names: BTreeSet<String> = scans.keys().cloned().collect();
    build_tree(root, &scans, &|path| names.contains(path))
}

fn module(path: &[&str]) -> Vec<String> {
    path.iter().map(ToString::to_string).collect()
}

#[test]
fn module_tree_follows_mod_and_path_attributes() {
    let tree = tree_of(
        "src/lib.rs",
        &[
            ("src/lib.rs", "pub mod tools; pub mod ci2; mod absent;"),
            (
                "src/tools.rs",
                "#[cfg(test)]\n#[path = \"tools_tests.rs\"]\nmod tests;",
            ),
            ("src/tools_tests.rs", "#[test] fn x() {}"),
            ("src/ci2/mod.rs", "pub mod runtime;"),
            ("src/ci2/runtime/mod.rs", "pub fn f() {}"),
        ],
    );
    assert_eq!(
        tree.resolve(&module(&["tools", "tests"])),
        "src/tools_tests.rs"
    );
    assert_eq!(
        tree.declaring_file(&module(&["tools", "tests"]))
            .map(String::as_str),
        Some("src/tools.rs")
    );
    assert_eq!(
        tree.resolve(&module(&["ci2", "runtime", "tests"])),
        "src/ci2/runtime/mod.rs"
    );
    // module inconnu : le plus profond qui existe, la racine à défaut
    assert_eq!(
        tree.resolve(&module(&["tools", "nope", "deeper"])),
        "src/tools.rs"
    );
    assert_eq!(tree.resolve(&module(&["absent"])), "src/lib.rs");
    assert!(!tree.members.contains("src/absent.rs"));
    assert!(
        tree.links
            .contains(&("src/tools.rs".into(), "src/tools_tests.rs".into(), true))
    );
    assert!(
        tree.links
            .contains(&("src/lib.rs".into(), "src/tools.rs".into(), false))
    );
}

#[test]
fn integration_test_can_compile_a_library_file_with_path() {
    // `tests/v2_runtime.rs` → `src/ci2/runtime/mod.rs` (dépendance invisible pour SCIP).
    let tree = tree_of(
        "tests/v2_runtime.rs",
        &[
            (
                "tests/v2_runtime.rs",
                "#[path = \"../src/ci2/runtime/mod.rs\"]\nmod runtime;",
            ),
            ("src/ci2/runtime/mod.rs", "mod helper;"),
            ("src/ci2/runtime/helper.rs", ""),
        ],
    );
    assert!(tree.links.contains(&(
        "tests/v2_runtime.rs".into(),
        "src/ci2/runtime/mod.rs".into(),
        true
    )));
    assert!(tree.members.contains("src/ci2/runtime/helper.rs"));
    assert_eq!(
        tree.resolve(&module(&["runtime", "t"])),
        "src/ci2/runtime/mod.rs"
    );
}

#[test]
fn ambiguous_module_files_link_every_existing_candidate() {
    let tree = tree_of(
        "src/lib.rs",
        &[
            ("src/lib.rs", "mod twin;"),
            ("src/twin.rs", ""),
            ("src/twin/mod.rs", ""),
        ],
    );
    assert!(tree.members.contains("src/twin.rs"));
    assert!(tree.members.contains("src/twin/mod.rs"));
}

#[test]
fn include_macro_adds_the_included_rust_file_to_the_crate() {
    let tree = tree_of(
        "src/lib.rs",
        &[
            ("src/lib.rs", "include!(\"gen/part.rs\");"),
            ("src/gen/part.rs", "mod inner;"),
            ("src/gen/inner.rs", ""),
        ],
    );
    assert!(tree.members.contains("src/gen/part.rs"));
    assert!(
        tree.links
            .contains(&("src/lib.rs".into(), "src/gen/part.rs".into(), false))
    );
}

// ---------------------------------------------------------------------------------------
// JSON du graphe (clé de map non textuelle)
// ---------------------------------------------------------------------------------------

fn sample_graph(digest: &str) -> GraphData {
    let mut graph = GraphData {
        schema: GRAPH_SCHEMA.to_string(),
        digest: digest.to_string(),
        ..GraphData::default()
    };
    graph
        .edges
        .insert("a.rs".into(), BTreeSet::from(["b.rs".to_string()]));
    graph
        .provenance
        .insert("a.rs -> b.rs".into(), BTreeSet::from(["scip".to_string()]));
    graph
        .binary_files
        .insert("pkg::bin/x".into(), BTreeSet::from(["a.rs".to_string()]));
    graph
        .literal_refs
        .insert("run.py".into(), BTreeSet::from(["a.rs".to_string()]));
    graph.external_inputs.insert(
        "/abs/ARRET.txt".into(),
        BTreeSet::from(["a.rs".to_string()]),
    );
    graph.test_roots.insert(
        TestId {
            binary_id: "pkg".into(),
            name: "m::t".into(),
        },
        BTreeSet::from(["a.rs".to_string()]),
    );
    graph
}

#[test]
fn graph_json_roundtrips_even_with_struct_keys() {
    let graph = sample_graph("d1");
    assert!(
        serde_json::to_vec(&graph).is_err(),
        "le contrat brut ne se sérialise pas (clé TestId) : d'où to_json"
    );
    let bytes = to_json(&graph).unwrap();
    assert_eq!(from_json(&bytes).unwrap(), graph);
}

#[test]
fn graph_json_rejects_other_schema_and_garbage() {
    let mut other = sample_graph("d1");
    other.schema = "kioku/select/graph/v0".into();
    assert!(from_json(&to_json(&other).unwrap()).is_err());
    assert!(from_json(b"").is_err());
    assert!(from_json(b"{\"schema\":").is_err());
    assert!(from_json(b"[1,2,3]").is_err());
    let good = to_json(&sample_graph("d1")).unwrap();
    assert!(from_json(&good[..good.len() / 2]).is_err());
}

// --- A4 : scellé d'intégrité du graphe ----------------------------------------------------

fn graph_document() -> serde_json::Value {
    serde_json::from_slice(&to_json(&sample_graph("d1")).unwrap()).unwrap()
}

fn assert_graph_refused(label: &str, doc: &serde_json::Value) {
    match from_json(&serde_json::to_vec(doc).unwrap()) {
        Err(SelectError::Invalid { message, .. }) => {
            assert!(message.contains("graphe incohérent"), "{label} : {message}");
        }
        other => panic!("{label} : refus attendu, obtenu {other:?}"),
    }
}

#[test]
fn sealed_graph_carries_counts_and_a_digest() {
    let doc = graph_document();
    assert_eq!(doc["schema"], "kioku/select/graph/v2");
    assert_eq!(doc["edge_count"], 1);
    assert_eq!(doc["test_count"], 1);
    assert_eq!(doc["test_root_count"], 1);
    assert_eq!(doc["binary_count"], 1);
    let digest = doc["content_digest"].as_str().unwrap();
    assert!(
        digest.starts_with("sha256:") && digest.len() == 7 + 64,
        "{digest}"
    );
    assert_eq!(
        to_json(&sample_graph("d1")).unwrap(),
        to_json(&sample_graph("d1")).unwrap(),
        "sérialisation déterministe"
    );
}

#[test]
fn every_alteration_of_a_sealed_graph_is_refused() {
    let good = graph_document();
    let edit = |label: &str, change: &dyn Fn(&mut serde_json::Value)| {
        let mut doc = good.clone();
        change(&mut doc);
        assert_ne!(doc, good, "mutation sans effet : {label}");
        assert_graph_refused(label, &doc);
    };
    edit("arêtes vidées, scellé gardé", &|d| {
        d["edges"] = serde_json::json!({});
    });
    edit("cible d'arête retirée", &|d| {
        d["edges"]["a.rs"] = serde_json::json!([]);
    });
    edit("cible d'arête changée", &|d| {
        d["edges"]["a.rs"] = serde_json::json!(["c.rs"]);
    });
    edit("racines de test vidées", &|d| {
        d["test_roots"] = serde_json::json!([]);
    });
    edit("racine changée", &|d| {
        d["test_roots"][0]["roots"] = serde_json::json!(["z.rs"]);
    });
    edit("binaire retiré", &|d| {
        d["binary_files"] = serde_json::json!({});
    });
    edit("fichier de binaire retiré", &|d| {
        d["binary_files"]["pkg::bin/x"] = serde_json::json!([]);
    });
    edit("référence littérale retirée", &|d| {
        d["literal_refs"] = serde_json::json!({});
    });
    edit("entrée externe retirée", &|d| {
        d["external_inputs"] = serde_json::json!({});
    });
    edit("provenance retirée", &|d| {
        d["provenance"] = serde_json::json!({});
    });
    edit("digest d'entrées changé", &|d| d["digest"] = "d2".into());
    for field in [
        "edge_count",
        "test_count",
        "test_root_count",
        "binary_count",
    ] {
        edit(&format!("{field} changé"), &|d| d[field] = 9.into());
        edit(&format!("{field} absent"), &|d| {
            d.as_object_mut().unwrap().remove(field);
        });
    }
    edit("edges vidées ET edge_count mis à 0", &|d| {
        d["edges"] = serde_json::json!({});
        d["edge_count"] = 0.into();
    });
    edit("content_digest changé", &|d| {
        d["content_digest"] = format!("sha256:{}", "0".repeat(64)).into();
    });
    edit("content_digest absent", &|d| {
        d.as_object_mut().unwrap().remove("content_digest");
    });
    // Doublon CONTRADICTOIRE de test dans `test_roots` (le dernier écraserait le premier à la
    // relecture) : refusé aussi. Un doublon identique ne change rien au contenu : sans effet.
    edit("test dupliqué avec d'autres racines", &|d| {
        let mut other = d["test_roots"][0].clone();
        other["roots"] = serde_json::json!(["z.rs"]);
        d["test_roots"].as_array_mut().unwrap().push(other);
    });
    from_json(&serde_json::to_vec(&good).unwrap()).expect("intact");
}

#[test]
fn an_old_v1_graph_is_refused() {
    let mut doc = graph_document();
    doc["schema"] = "kioku/select/graph/v1".into();
    for field in [
        "edge_count",
        "test_count",
        "test_root_count",
        "binary_count",
        "content_digest",
    ] {
        doc.as_object_mut().unwrap().remove(field);
    }
    assert!(matches!(
        from_json(&serde_json::to_vec(&doc).unwrap()),
        Err(SelectError::Invalid { message, .. }) if message.contains("graph/v2")
    ));
}

// ---------------------------------------------------------------------------------------
// Fixture : un vrai petit paquet Cargo
// ---------------------------------------------------------------------------------------

const FIXTURE: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        "[package]\nname = \"l2mini\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    ),
    (
        "Cargo.lock",
        "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n[[package]]\nname = \"l2mini\"\nversion = \"0.1.0\"\n",
    ),
    (
        "src/lib.rs",
        "pub mod core;\npub mod data;\npub mod ext;\npub mod runtime;\npub mod script;\n",
    ),
    (
        "src/core.rs",
        "pub fn compute() -> u32 {\n    42\n}\n\n#[cfg(test)]\n#[path = \"core_tests.rs\"]\nmod tests;\n",
    ),
    (
        "src/core_tests.rs",
        "#[test]\nfn core_works() {\n    assert_eq!(super::compute(), 42);\n}\n",
    ),
    (
        "src/data.rs",
        "pub const NOTE: &str = include_str!(\"../assets/note.txt\");\n",
    ),
    (
        "src/script.rs",
        "pub const SCRIPT: &str = \"scripts/run.py\";\n",
    ),
    ("src/runtime/mod.rs", "pub fn tick() -> u32 {\n    1\n}\n"),
    (
        "src/main.rs",
        "fn main() {\n    println!(\"{}\", l2mini::core::compute());\n}\n",
    ),
    (
        "tests/alpha.rs",
        "fn setup() -> u32 {\n    1\n}\n\n#[test]\nfn alpha_uses_core() {\n    assert_eq!(setup() + l2mini::core::compute(), 43);\n}\n",
    ),
    (
        "tests/beta.rs",
        "fn setup() -> u32 {\n    2\n}\n\n#[test]\nfn beta_runs() {\n    assert_eq!(setup(), 2);\n}\n",
    ),
    (
        "tests/pathy.rs",
        "#[path = \"../src/runtime/mod.rs\"]\nmod runtime;\n\n#[test]\nfn ticks() {\n    assert_eq!(runtime::tick(), 1);\n}\n",
    ),
    (
        "tests/cli.rs",
        "#[test]\nfn cli_binary_exists() {\n    let exe = env!(\"CARGO_BIN_EXE_l2mini\");\n    assert!(std::path::Path::new(exe).exists());\n}\n",
    ),
    ("assets/note.txt", "a note\n"),
    ("scripts/run.py", "print('hello')\n"),
    ("README.md", "not cited by any source\n"),
];

struct Fixture {
    /// Dépôt (copie de travail).
    repo: tempfile::TempDir,
    /// Dossier du cache.
    cache: tempfile::TempDir,
    /// Entrée hors dépôt déclarée.
    external: PathBuf,
}

fn write_file(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn new_fixture() -> Fixture {
    let repo = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    for (relative, content) in FIXTURE {
        write_file(repo.path(), relative, content);
    }
    let external = cache.path().join("outside").join("ARRET.txt");
    write_file(cache.path(), "outside/ARRET.txt", "stop\n");
    write_file(
        repo.path(),
        "src/ext.rs",
        &format!(
            "pub const ARRET: &str = {:?};\n",
            external.display().to_string()
        ),
    );
    Fixture {
        repo,
        cache,
        external,
    }
}

fn fixture_options(fixture: &Fixture) -> GraphOptions {
    GraphOptions {
        external_inputs: vec![fixture.external.clone()],
        scip_timeout: Duration::from_secs(300),
        build_timeout: Duration::from_secs(900),
        ..GraphOptions::default()
    }
}

fn sources(graph: &GraphData, from: &str, to: &str) -> BTreeSet<String> {
    graph
        .provenance
        .get(&format!("{from} -> {to}"))
        .cloned()
        .unwrap_or_default()
}

fn has_edge(graph: &GraphData, from: &str, to: &str) -> bool {
    graph
        .edges
        .get(from)
        .is_some_and(|targets| targets.contains(to))
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(ToString::to_string).collect()
}

fn test_id(binary: &str, name: &str) -> TestId {
    TestId {
        binary_id: binary.to_string(),
        name: name.to_string(),
    }
}

#[test]
fn fixture_graph_has_every_source_and_keeps_crates_apart() {
    let fixture = new_fixture();
    let opts = fixture_options(&fixture);
    let (graph, stats) = build_with_stats(fixture.repo.path(), &opts).unwrap();
    assert_eq!(graph.schema, GRAPH_SCHEMA);
    assert_eq!(graph.digest, digest(fixture.repo.path(), &opts).unwrap());
    assert!(stats.scip_documents >= 8, "{stats:?}");

    // mod, #[path] dans la lib (module de tests), include_str!
    assert!(sources(&graph, "src/lib.rs", "src/core.rs").contains("mod"));
    assert!(sources(&graph, "src/core.rs", "src/core_tests.rs").contains("path"));
    assert!(sources(&graph, "src/data.rs", "assets/note.txt").contains("include"));
    assert_eq!(graph.literal_refs["assets/note.txt"], set(&["src/data.rs"]));

    // #[path = "../src/…"] depuis un test d'intégration (le cas v2_runtime)
    assert!(sources(&graph, "tests/pathy.rs", "src/runtime/mod.rs").contains("path"));

    // littéral non Rust
    assert!(sources(&graph, "src/script.rs", "scripts/run.py").contains("literal"));
    assert_eq!(
        graph.literal_refs["scripts/run.py"],
        set(&["src/script.rs"])
    );
    assert!(
        !graph.literal_refs.contains_key("README.md"),
        "non cité : aucune référence"
    );

    // CLI → src/main.rs
    assert!(sources(&graph, "tests/cli.rs", "src/main.rs").contains("cli"));

    // SCIP : usage de symbole → fichier définissant
    assert!(sources(&graph, "tests/alpha.rs", "src/core.rs").contains("scip"));
    assert!(sources(&graph, "src/main.rs", "src/core.rs").contains("scip"));
    assert!(sources(&graph, "src/core_tests.rs", "src/core.rs").contains("scip"));

    // Séparation par crate : `setup()` homonyme dans deux crates de test, aucun lien.
    assert!(!has_edge(&graph, "tests/alpha.rs", "tests/beta.rs"));
    assert!(!has_edge(&graph, "tests/beta.rs", "tests/alpha.rs"));
    assert!(sources(&graph, "tests/alpha.rs", "tests/beta.rs").is_empty());
    // Les tests ne dépendent jamais du binaire par SCIP, ni la lib d'un test.
    assert!(!sources(&graph, "tests/alpha.rs", "src/main.rs").contains("scip"));
    for (from, targets) in &graph.edges {
        if from.starts_with("src/") && from != "src/main.rs" {
            assert!(
                targets.iter().all(|to| !to.starts_with("tests/")),
                "{from} → test"
            );
        }
    }

    // Aucune arête vers soi-même ni vers un fichier inexistant ; provenance complète.
    for (from, targets) in &graph.edges {
        for to in targets {
            assert_ne!(from, to);
            assert!(
                fixture.repo.path().join(to).is_file(),
                "{from} → {to} cible un fichier absent"
            );
            assert!(
                !sources(&graph, from, to).is_empty(),
                "{from} → {to} sans provenance"
            );
        }
    }
    let counts = provenance_counts(&graph);
    for source in ["scip", "mod", "path", "include", "literal", "cli"] {
        assert!(
            counts.get(source).copied().unwrap_or(0) > 0,
            "aucune arête {source}"
        );
    }

    // Entrée hors dépôt déclarée.
    assert_eq!(
        graph.external_inputs[&fixture.external.display().to_string()],
        set(&["src/ext.rs"])
    );
}

#[test]
fn fixture_dep_info_and_test_roots() {
    let fixture = new_fixture();
    let opts = fixture_options(&fixture);
    let graph = kioku::select::graph::build(fixture.repo.path(), &opts).unwrap();

    // dep-info rustc : appartenance exacte fichier → binaire.
    assert_eq!(graph.binary_files["l2mini::beta"], set(&["tests/beta.rs"]));
    assert_eq!(
        graph.binary_files["l2mini::pathy"],
        set(&["tests/pathy.rs", "src/runtime/mod.rs"])
    );
    let lib = &graph.binary_files["l2mini"];
    for file in [
        "src/lib.rs",
        "src/core.rs",
        "src/core_tests.rs",
        "src/data.rs",
        "src/runtime/mod.rs",
    ] {
        assert!(lib.contains(file), "{file} absent de la lib");
    }
    assert!(!lib.contains("tests/alpha.rs"));
    assert!(graph.binary_files.contains_key("l2mini::bin/l2mini"));

    // Racines des tests.
    assert_eq!(
        graph.test_roots[&test_id("l2mini", "core::tests::core_works")],
        set(&["src/core_tests.rs", "src/core.rs"]),
        "module de tests via #[path] et fichier qui le déclare"
    );
    assert_eq!(
        graph.test_roots[&test_id("l2mini::alpha", "alpha_uses_core")],
        set(&["tests/alpha.rs"])
    );
    assert_eq!(
        graph.test_roots[&test_id("l2mini::pathy", "ticks")],
        set(&["tests/pathy.rs"])
    );
    assert_eq!(
        graph.test_roots[&test_id("l2mini::cli", "cli_binary_exists")],
        set(&["tests/cli.rs", "src/main.rs"]),
        "un test qui lance le CLI atteint src/main.rs"
    );
    assert_eq!(graph.test_roots.len(), 5);
}

#[test]
fn unfiltered_scip_would_have_linked_the_homonymous_crates() {
    // Contrôle de la fixture : sans séparation par crate, l'index brut relie bien alpha et
    // beta ; le test précédent prouve que le graphe final ne le fait pas.
    let fixture = new_fixture();
    let bytes = kioku::select::graph::scip::run_rust_analyzer(
        Path::new("rust-analyzer"),
        fixture.repo.path(),
        None,
        Duration::from_secs(300),
    )
    .unwrap();
    let documents = decode_index(&bytes).unwrap();
    let raw = file_edges(&documents, &|_, _| true);
    let linked = raw
        .get("tests/alpha.rs")
        .is_some_and(|set| set.contains("tests/beta.rs"))
        || raw
            .get("tests/beta.rs")
            .is_some_and(|set| set.contains("tests/alpha.rs"));
    assert!(linked, "la fixture doit exercer le cas homonyme : {raw:?}");
}

// --- cache -----------------------------------------------------------------------------

#[test]
fn digest_tracks_sources_cited_files_and_options() {
    let fixture = new_fixture();
    let repo = fixture.repo.path();
    let opts = fixture_options(&fixture);
    let base = digest(repo, &opts).unwrap();
    assert_eq!(digest(repo, &opts).unwrap(), base, "déterministe");

    // un `.rs` change → le digest change
    let original = std::fs::read_to_string(repo.join("src/core.rs")).unwrap();
    write_file(repo, "src/core.rs", &format!("{original}// edit\n"));
    let edited = digest(repo, &opts).unwrap();
    assert_ne!(edited, base);
    write_file(repo, "src/core.rs", &original);
    assert_eq!(
        digest(repo, &opts).unwrap(),
        base,
        "contenu identique, digest identique"
    );

    // un non-Rust cité change → le digest change ; un non-Rust non cité, non.
    write_file(repo, "assets/note.txt", "another note\n");
    assert_ne!(digest(repo, &opts).unwrap(), base);
    write_file(repo, "assets/note.txt", "a note\n");
    write_file(repo, "README.md", "changed but never cited\n");
    assert_eq!(digest(repo, &opts).unwrap(), base);

    // un `.rs` ajouté ou supprimé, Cargo.lock, features, entrées externes
    write_file(repo, "tests/extra.rs", "#[test]\nfn t() {}\n");
    assert_ne!(digest(repo, &opts).unwrap(), base);
    std::fs::remove_file(repo.join("tests/extra.rs")).unwrap();
    assert_eq!(digest(repo, &opts).unwrap(), base);
    let lock = std::fs::read_to_string(repo.join("Cargo.lock")).unwrap();
    write_file(repo, "Cargo.lock", &format!("{lock}\n# touched\n"));
    assert_ne!(digest(repo, &opts).unwrap(), base);
    write_file(repo, "Cargo.lock", &lock);
    let with_features = GraphOptions {
        features: vec!["x".into()],
        ..fixture_options(&fixture)
    };
    assert_ne!(digest(repo, &with_features).unwrap(), base);
    let all = GraphOptions {
        all_features: true,
        ..fixture_options(&fixture)
    };
    assert_ne!(digest(repo, &all).unwrap(), base);
    let no_external = GraphOptions {
        external_inputs: Vec::new(),
        ..fixture_options(&fixture)
    };
    assert_ne!(digest(repo, &no_external).unwrap(), base);
}

#[test]
fn cache_hit_returns_the_stored_graph_without_rebuilding() {
    let fixture = new_fixture();
    let opts = fixture_options(&fixture);
    let key = digest(fixture.repo.path(), &opts).unwrap();
    // Graphe sentinelle : aucune construction réelle ne pourrait produire cette arête.
    let mut sentinel = sample_graph(&key);
    sentinel
        .edges
        .insert("sentinel.rs".into(), set(&["never-built.rs"]));
    let cache_dir = fixture.cache.path().join("graphs");
    std::fs::create_dir_all(&cache_dir).unwrap();
    std::fs::write(
        cache_dir.join(format!("graph-{key}.json")),
        to_json(&sentinel).unwrap(),
    )
    .unwrap();
    let started = Instant::now();
    let loaded = load_or_build(fixture.repo.path(), &cache_dir, &opts).unwrap();
    assert_eq!(
        loaded, sentinel,
        "le graphe en cache est réutilisé tel quel"
    );
    assert!(started.elapsed() < Duration::from_secs(30));
}

#[test]
fn cache_with_wrong_digest_or_schema_is_not_trusted() {
    let fixture = new_fixture();
    let opts = fixture_options(&fixture);
    let key = digest(fixture.repo.path(), &opts).unwrap();
    let cache_dir = fixture.cache.path().join("graphs");
    std::fs::create_dir_all(&cache_dir).unwrap();
    let path = cache_dir.join(format!("graph-{key}.json"));
    // Bon nom, contenu d'un autre digest : refusé et reconstruit.
    let mut stale = sample_graph("not-the-digest");
    stale.edges.insert("stale.rs".into(), set(&["old.rs"]));
    std::fs::write(&path, to_json(&stale).unwrap()).unwrap();
    let rebuilt = load_or_build(fixture.repo.path(), &cache_dir, &opts).unwrap();
    assert_eq!(rebuilt.digest, key);
    assert!(!rebuilt.edges.contains_key("stale.rs"));
    assert_eq!(from_json(&std::fs::read(&path).unwrap()).unwrap(), rebuilt);
}

#[test]
fn corrupted_cache_is_rebuilt_and_replaced_atomically() {
    let fixture = new_fixture();
    let opts = fixture_options(&fixture);
    let key = digest(fixture.repo.path(), &opts).unwrap();
    let cache_dir = fixture.cache.path().join("graphs");
    std::fs::create_dir_all(&cache_dir).unwrap();
    let path = cache_dir.join(format!("graph-{key}.json"));
    std::fs::write(
        &path,
        b"{\"schema\": \"kioku/select/graph/v1\", \"digest\": \"tru",
    )
    .unwrap();

    let rebuilt = load_or_build(fixture.repo.path(), &cache_dir, &opts).unwrap();
    assert_eq!(rebuilt.digest, key);
    assert!(
        sources(&rebuilt, "src/lib.rs", "src/core.rs").contains("mod"),
        "vraie construction"
    );
    // Le fichier corrompu est remplacé par un graphe valide, aucun temporaire ne reste.
    assert_eq!(from_json(&std::fs::read(&path).unwrap()).unwrap(), rebuilt);
    let names: Vec<String> = std::fs::read_dir(&cache_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        [format!("graph-{key}.json")],
        "pas de fichier temporaire résiduel"
    );
    // Deuxième appel : cache chaud, même graphe.
    let again = load_or_build(fixture.repo.path(), &cache_dir, &opts).unwrap();
    assert_eq!(again, rebuilt);

    // Une source modifiée change la clé : nouveau fichier, l'ancien reste intact.
    let original = std::fs::read_to_string(fixture.repo.path().join("src/data.rs")).unwrap();
    write_file(
        fixture.repo.path(),
        "src/data.rs",
        &format!("{original}// edit\n"),
    );
    let new_key = digest(fixture.repo.path(), &opts).unwrap();
    assert_ne!(new_key, key);
    assert!(!cache_dir.join(format!("graph-{new_key}.json")).exists());
}

// --- erreurs d'outils --------------------------------------------------------------------

#[cfg(unix)]
fn fake_rust_analyzer(dir: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("fake-rust-analyzer");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo fake-ra 0.0; exit 0; fi\n{body}\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[cfg(unix)]
#[test]
fn rust_analyzer_overrun_is_a_timeout_error() {
    let fixture = new_fixture();
    let tools = tempfile::tempdir().unwrap();
    let opts = GraphOptions {
        rust_analyzer: fake_rust_analyzer(tools.path(), "sleep 120"),
        scip_timeout: Duration::from_secs(1),
        ..fixture_options(&fixture)
    };
    let started = Instant::now();
    let error = kioku::select::graph::build(fixture.repo.path(), &opts).unwrap_err();
    assert!(matches!(error, SelectError::Timeout { .. }), "{error:?}");
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "le processus est bien tué"
    );
}

#[cfg(unix)]
#[test]
fn rust_analyzer_failure_and_absence_are_errors() {
    let fixture = new_fixture();
    let tools = tempfile::tempdir().unwrap();
    let failing = GraphOptions {
        rust_analyzer: fake_rust_analyzer(tools.path(), "echo boom >&2; exit 3"),
        ..fixture_options(&fixture)
    };
    let error = kioku::select::graph::build(fixture.repo.path(), &failing).unwrap_err();
    assert!(
        matches!(&error, SelectError::Command { message, .. } if message.contains("boom")),
        "{error:?}"
    );

    // Sortie vide : le fichier d'index n'existe pas → erreur, jamais « graphe sans SCIP ».
    let silent = GraphOptions {
        rust_analyzer: fake_rust_analyzer(tools.path(), "exit 0"),
        ..fixture_options(&fixture)
    };
    assert!(kioku::select::graph::build(fixture.repo.path(), &silent).is_err());

    let absent = GraphOptions {
        rust_analyzer: tools.path().join("does-not-exist"),
        ..fixture_options(&fixture)
    };
    assert!(digest(fixture.repo.path(), &absent).is_err());
    assert!(kioku::select::graph::build(fixture.repo.path(), &absent).is_err());
}

#[test]
fn unparsable_source_is_an_error_not_a_hole_in_the_graph() {
    let fixture = new_fixture();
    write_file(fixture.repo.path(), "src/broken.rs", "fn broken( {");
    let error = digest(fixture.repo.path(), &fixture_options(&fixture)).unwrap_err();
    assert!(matches!(error, SelectError::Invalid { .. }), "{error:?}");
}

#[test]
fn relative_external_input_is_refused() {
    let fixture = new_fixture();
    let opts = GraphOptions {
        external_inputs: vec![PathBuf::from("relative/ARRET.txt")],
        ..fixture_options(&fixture)
    };
    let error = kioku::select::graph::build(fixture.repo.path(), &opts).unwrap_err();
    assert!(matches!(error, SelectError::Invalid { .. }), "{error:?}");
}

// ---------------------------------------------------------------------------------------
// Évaluation sur kioku lui-même (manuelle : `-- --ignored`, sous le verrou des builds lourds)
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

/// Pour chaque test de `scratch/optA/cov/lines.json` encore présent : fichiers `src/**`
/// exécutés mais non atteints depuis ses racines. Attendu : les 6 tests de `validate.rs`.
#[test]
#[ignore = "mesure sur le dépôt réel : cargo nextest run --test select_l2_graph --run-ignored only (sous flock)"]
fn kioku_coverage_evaluation() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let opts = GraphOptions {
        all_features: true,
        ..GraphOptions::default()
    };
    let (graph, stats) = build_with_stats(&repo, &opts).unwrap();
    let edge_count: usize = graph.edges.values().map(BTreeSet::len).sum();
    println!("stats {stats:?}");
    println!(
        "edges {edge_count} provenance {:?}",
        provenance_counts(&graph)
    );
    println!(
        "tests {} binaries {} literal_refs {} external_inputs {}",
        graph.test_roots.len(),
        graph.binary_files.len(),
        graph.literal_refs.len(),
        graph.external_inputs.len()
    );
    let cov: BTreeMap<String, BTreeMap<String, serde_json::Value>> =
        serde_json::from_slice(&std::fs::read(repo.join("scratch/optA/cov/lines.json")).unwrap())
            .unwrap();
    let mut evaluated = 0usize;
    let mut absent = Vec::new();
    let mut missed: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (key, files) in &cov {
        let (binary, name) = key.split_once(' ').unwrap();
        let id = test_id(binary, name);
        let Some(roots) = graph.test_roots.get(&id) else {
            absent.push(key.clone());
            continue;
        };
        evaluated += 1;
        let reached = closure(&graph, roots);
        for file in files.keys().filter(|file| file.starts_with("src/")) {
            if !reached.contains(file) {
                missed.entry(key.clone()).or_default().insert(file.clone());
            }
        }
    }
    println!(
        "evaluated {evaluated} absent {} missed {}",
        absent.len(),
        missed.len()
    );
    for key in &absent {
        println!("  absent {key}");
    }
    for (key, files) in &missed {
        println!("  raté {key} -> {files:?}");
    }
    assert!(
        missed
            .values()
            .flatten()
            .all(|file| file == "src/ci2/domain/validate.rs"),
        "tout autre raté est un défaut du graphe : {missed:?}"
    );
    assert!(missed.len() <= 6, "{missed:?}");
    assert!(
        stats.total_ms.saturating_sub(stats.compile_ms) <= 15_000,
        "index > 15 s hors compilation : {stats:?}"
    );

    // Cache : écriture puis relecture à chaud.
    let cache = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let built = load_or_build(&repo, cache.path(), &opts).unwrap();
    let cold = started.elapsed();
    let started = Instant::now();
    let warm_graph = load_or_build(&repo, cache.path(), &opts).unwrap();
    let warm = started.elapsed();
    println!("load_or_build froid {cold:?}, chaud {warm:?}");
    assert_eq!(warm_graph, built);
    assert!(
        warm < Duration::from_secs(1),
        "cache chaud trop lent : {warm:?}"
    );
}
