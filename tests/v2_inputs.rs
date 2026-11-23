#![cfg(target_os = "linux")]
use kioku::ci2::{snapshot, taskkey};
use std::fs::{self, File, FileTimes};
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;

fn fixture() -> TempDir {
    let temp = tempfile::tempdir().unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(temp.path())
            .status()
            .unwrap()
            .success()
    );
    fs::create_dir(temp.path().join("src")).unwrap();
    fs::write(temp.path().join("src/a.ts"), b"alpha").unwrap();
    fs::write(temp.path().join("tsconfig.json"), b"{}").unwrap();
    temp
}
fn capture(temp: &TempDir) -> snapshot::Snapshot {
    snapshot::capture(
        temp.path(),
        &[PathBuf::from("src"), PathBuf::from("tsconfig.json")],
    )
    .unwrap()
}

#[test]
fn captures_multiple_inputs_and_content_changes_with_mtime_restored() {
    let temp = fixture();
    let first = capture(&temp);
    let file = temp.path().join("src/a.ts");
    let modified = fs::metadata(&file).unwrap().modified().unwrap();
    fs::write(&file, b"omega").unwrap();
    File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_times(FileTimes::new().set_modified(modified))
        .unwrap();
    let second = capture(&temp);
    assert_ne!(
        first.files["src/a.ts"].digest,
        second.files["src/a.ts"].digest
    );
    assert_eq!(first.files["tsconfig.json"], second.files["tsconfig.json"]);
}

#[test]
fn detects_rename_remove_config_and_empty_directories() {
    let temp = fixture();
    let first = capture(&temp);
    fs::rename(temp.path().join("src/a.ts"), temp.path().join("src/b.ts")).unwrap();
    let second = capture(&temp);
    assert_ne!(first, second);
    assert!(!second.files.contains_key("src/a.ts"));
    fs::remove_file(temp.path().join("src/b.ts")).unwrap();
    fs::create_dir(temp.path().join("src/empty")).unwrap();
    fs::write(temp.path().join("tsconfig.json"), b"{ } ").unwrap();
    let third = capture(&temp);
    assert!(third.directories.contains("src/empty"));
    assert_ne!(second.files["tsconfig.json"], third.files["tsconfig.json"]);
}

#[test]
fn rejects_missing_secret_overlap_and_symlink_escape() {
    let temp = fixture();
    symlink("/etc/passwd", temp.path().join("src/escape")).unwrap();
    assert!(snapshot::capture(temp.path(), &["src".into()]).is_err());
    assert!(snapshot::capture(temp.path(), &["missing".into()]).is_err());
    assert!(snapshot::capture(temp.path(), &[".env".into()]).is_err());
    assert!(snapshot::capture(temp.path(), &["../outside".into()]).is_err());
    fs::remove_file(temp.path().join("src/escape")).unwrap();
    assert!(snapshot::capture(temp.path(), &["src".into(), "src/a.ts".into()]).is_err());
}

#[test]
fn generated_and_tool_files_are_explicit_inputs() {
    let temp = fixture();
    fs::create_dir_all(temp.path().join(".next/types")).unwrap();
    fs::create_dir_all(temp.path().join("tools")).unwrap();
    fs::write(temp.path().join(".next/types/routes.d.ts"), b"one").unwrap();
    fs::write(temp.path().join("tools/compiler"), b"compiler-a").unwrap();
    let paths = [
        PathBuf::from(".next/types/routes.d.ts"),
        PathBuf::from("tools/compiler"),
    ];
    let first = snapshot::capture(temp.path(), &paths).unwrap();
    fs::write(temp.path().join(".next/types/routes.d.ts"), b"two").unwrap();
    fs::write(temp.path().join("tools/compiler"), b"compiler-b").unwrap();
    let second = snapshot::capture(temp.path(), &paths).unwrap();
    assert_ne!(
        first.files[".next/types/routes.d.ts"],
        second.files[".next/types/routes.d.ts"]
    );
    assert_ne!(
        first.files["tools/compiler"],
        second.files["tools/compiler"]
    );
    fs::remove_file(temp.path().join(".next/types/routes.d.ts")).unwrap();
    assert!(snapshot::capture(temp.path(), &paths).is_err());
}

fn material() -> taskkey::TaskKeyMaterial {
    let digest = format!("b3:{}", "a".repeat(64));
    taskkey::TaskKeyMaterial {
        project_id: "p".into(),
        trust_scope: "local".into(),
        task_id: "task".into(),
        command_digest: digest.clone(),
        inputs_manifest_digest: digest.clone(),
        global_inputs_digest: digest.clone(),
        producer_outputs: vec![],
        env_digest: digest.clone(),
        installation_digest: digest.clone(),
        profile_digest: digest.clone(),
        resource_recipe_digest: digest.clone(),
        outputs_spec_digest: digest.clone(),
        effects_qualification_digest: digest,
        adapter_semantics_version: "v1".into(),
    }
}
#[test]
fn key_is_stable_sensitive_and_rejects_ambiguity() {
    let mut a = material();
    let x = taskkey::ProducerOutputRef {
        task_id: "a".into(),
        output_id: "o".into(),
        digest: a.command_digest.clone(),
    };
    let y = taskkey::ProducerOutputRef {
        task_id: "b".into(),
        output_id: "o".into(),
        digest: a.command_digest.clone(),
    };
    a.producer_outputs = vec![x.clone(), y.clone()];
    let key = taskkey::task_key(&a).unwrap();
    a.producer_outputs.reverse();
    assert_eq!(key, taskkey::task_key(&a).unwrap());
    a.command_digest = format!("b3:{}", "b".repeat(64));
    assert_ne!(key, taskkey::task_key(&a).unwrap());
    a.producer_outputs.push(x);
    assert!(taskkey::task_key(&a).is_err());
    a.producer_outputs.pop();
    a.profile_digest = "short".into();
    assert!(taskkey::task_key(&a).is_err());
}
