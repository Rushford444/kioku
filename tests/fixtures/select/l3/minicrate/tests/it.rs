//! Tests de la fixture `run_coverage` : chaque test écrit un marqueur que les faux outils
//! LLVM du test d'intégration de kioku transforment en export lcov.

fn mark(lines: &str) {
    let pattern = std::env::var("LLVM_PROFILE_FILE").expect("LLVM_PROFILE_FILE");
    let path = std::path::Path::new(&pattern);
    let name = path.file_name().and_then(|n| n.to_str()).expect("name");
    let index = name.split('-').next().expect("index");
    let dir = path.parent().expect("dir");
    std::fs::write(dir.join(format!("{index}.mark")), lines).expect("mark");
}

#[test]
fn uses_a() {
    assert_eq!(minicov::a::fa(), 1);
    mark("src/lib.rs\nsrc/a.rs\ntests/it.rs\ntarget/junk.rs\n../escape.rs\n/usr/lib/outside.rs\n");
}

#[test]
fn uses_b() {
    assert_eq!(minicov::b::fb(), 2);
    mark("src/lib.rs\nsrc/b.rs\ntests/it.rs\n");
}

#[test]
fn slow() {
    std::thread::sleep(std::time::Duration::from_secs(120));
}

#[test]
#[ignore = "jamais lance par le filet couverture"]
fn ignored_one() {
    mark("src/a.rs\n");
}
