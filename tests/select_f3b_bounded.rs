//! Bounded mutations of captured Kioku store documents and a rustc dep-info file.
use std::fs;
use std::path::{Path, PathBuf};

use kioku::select::graph::depinfo::{parse_dep_info, repo_files};
use kioku::select::{base, graph, learn};
use serde_json::{Value, json};

const BASE: &str =
    include_str!("fixtures/select/f3b_bounded/base-855d89b3a3d9a473eed747af5f4e442139399159.json");
const GRAPH: &str = include_str!(
    "fixtures/select/f3b_bounded/graph-e326c84c4fc855166bc57adcffdd187b5c1c1e35fea09e06cb4c04a62d618a57.json"
);
const LEARNED: &str = include_str!(
    "fixtures/select/f3b_bounded/learned-855d89b3a3d9a473eed747af5f4e442139399159.json"
);
const DEP_INFO: &str = include_str!("fixtures/select/f3b_bounded/rustc-v2-d1.d");

fn write(dir: &Path, text: &[u8]) -> PathBuf {
    let file = dir.join("case.json");
    fs::write(&file, text).unwrap();
    file
}

#[test]
fn real_dep_info_and_independent_expected_paths() {
    let deps = parse_dep_info(DEP_INFO).unwrap();
    assert_eq!(deps, ["tests/v2_d1.rs"]);
    let files = repo_files(&deps, &[Path::new("/repo")]);
    assert_eq!(files.into_iter().collect::<Vec<_>>(), ["tests/v2_d1.rs"]);
    assert!(parse_dep_info("").is_err());
    assert!(parse_dep_info("# env-dep:X=1\n").is_err());
    let escaped = parse_dep_info("target: src/a\\ b.rs ../outside.rs /repo/src/ok.rs\n").unwrap();
    assert_eq!(escaped, ["src/a b.rs", "../outside.rs", "/repo/src/ok.rs"]);
    let files = repo_files(&escaped, &[Path::new("/repo")]);
    assert!(!files.contains("../outside.rs"));
    assert!(files.contains("src/ok.rs"));
}

fn mutations(source: &str, count: &str) -> Vec<Vec<u8>> {
    let doc: Value = serde_json::from_str(source).unwrap();
    let mut cases = vec![Vec::new(), vec![0xff]];
    for part in 1..8 {
        cases.push(source.as_bytes()[..source.len() * part / 8].to_vec());
    }
    let mut bad = doc.clone();
    bad.as_object_mut().unwrap().remove("content_digest");
    cases.push(serde_json::to_vec(&bad).unwrap());
    let mut bad = doc.clone();
    bad["content_digest"] =
        json!("sha256:0000000000000000000000000000000000000000000000000000000000000000");
    cases.push(serde_json::to_vec(&bad).unwrap());
    let mut bad = doc.clone();
    bad[count] = json!(doc[count].as_u64().unwrap() + 1);
    cases.push(serde_json::to_vec(&bad).unwrap());
    let mut bad = doc;
    bad.as_object_mut().unwrap().remove(count);
    cases.push(serde_json::to_vec(&bad).unwrap());
    cases
}

#[test]
fn captured_store_rejects_bounded_partial_corruption() {
    let temp = tempfile::tempdir().unwrap();
    let original = write(temp.path(), BASE.as_bytes());
    let base = base::load(&original).unwrap();
    assert!(base.green);
    assert_eq!(base.binaries.len(), 2);
    assert!(graph::from_json(GRAPH.as_bytes()).is_ok());
    assert!(learn::read(&write(temp.path(), LEARNED.as_bytes())).is_ok());
    for bytes in mutations(BASE, "binary_count") {
        assert!(base::load(&write(temp.path(), &bytes)).is_err());
    }
    for bytes in mutations(GRAPH, "binary_count") {
        assert!(graph::from_json(&bytes).is_err());
    }
    for bytes in mutations(LEARNED, "edge_count") {
        assert!(learn::read(&write(temp.path(), &bytes)).is_err());
    }
}
