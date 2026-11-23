//! Opt-in synthetic multiprocess preparation benchmark, not a result cache.
use kioku::select::cache::{CacheKey, InputSnapshot, LocalCooperativeStore};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, io::Write, path::Path, process::Command};

const WORKERS: usize = 8;
const FILES: usize = 8;
const SIZE: usize = 65_536;
const HASH_ROUNDS: usize = 256;

fn key() -> CacheKey {
    CacheKey {
        repository: "/lot9c/synthetic".into(),
        commit: "a".repeat(40),
        input_snapshot: InputSnapshot {
            full_tree_sha256: "b".repeat(64),
            input_closure_sha256: "c".repeat(64),
        },
        toolchain: BTreeMap::from([("workload".into(), "lot9c-v1".into())]),
        preparation_options: BTreeMap::from([("hash-rounds".into(), HASH_ROUNDS.to_string())]),
        store_options: BTreeMap::from([("format".into(), "lot9c-v1".into())]),
    }
}

fn produce(out: &Path, calls: &Path) -> std::io::Result<()> {
    writeln!(
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(calls)?,
        "producer"
    )?;
    for i in 0..FILES {
        if calls
            .parent()
            .is_some_and(|root| root.join("abort").exists())
        {
            return Err(std::io::Error::other("benchmark aborted"));
        }
        let bytes = vec![u8::try_from(i).unwrap(); SIZE];
        let mut digest = Sha256::digest(&bytes);
        for _ in 0..HASH_ROUNDS {
            let mut hasher = Sha256::new();
            hasher.update(digest);
            hasher.update(&bytes);
            digest = hasher.finalize();
        }
        std::hint::black_box(digest);
        fs::write(out.join(format!("file-{i}")), bytes)?;
    }
    Ok(())
}

fn verify(out: &Path) -> std::io::Result<()> {
    assert_eq!(fs::read_dir(out)?.count(), FILES);
    for i in 0..FILES {
        assert_eq!(
            fs::read(out.join(format!("file-{i}")))?,
            vec![u8::try_from(i).unwrap(); SIZE]
        );
    }
    Ok(())
}

#[test]
#[ignore = "launched explicitly by the lot9c benchmark"]
fn lot9c_child() {
    let root = std::path::PathBuf::from(std::env::var_os("KIOKU_LOT9C_ROOT").unwrap());
    let id = std::env::var("KIOKU_LOT9C_ID").unwrap();
    let mode = std::env::var("KIOKU_LOT9C_MODE").unwrap();
    let private = root.join(format!("private-{id}"));
    fs::create_dir(root.join(format!("ready-{id}"))).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !root.join("release").exists() {
        assert!(!root.join("abort").exists(), "barrier aborted");
        assert!(std::time::Instant::now() < deadline, "barrier timeout");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if mode == "cache" {
        let store = LocalCooperativeStore::open(root.join("store")).unwrap();
        let entry = store
            .prepare(&key(), |out| Ok(produce(out, &root.join("calls"))?))
            .unwrap();
        let copy = entry.private_copy(&private).unwrap();
        copy.with_before_test_outputs(|out| Ok(verify(out)?))
            .unwrap();
    } else {
        assert_eq!(mode, "baseline");
        fs::create_dir(&private).unwrap();
        let prep = private.join("prepared");
        let out = private.join("outputs");
        fs::create_dir(&prep).unwrap();
        fs::create_dir(&out).unwrap();
        produce(&prep, &root.join("calls")).unwrap();
        for i in 0..FILES {
            fs::copy(
                prep.join(format!("file-{i}")),
                out.join(format!("file-{i}")),
            )
            .unwrap();
        }
        verify(&out).unwrap();
    }
}

#[test]
#[ignore = "quantitative campaign requires operator permission and heavy lock"]
fn lot9c_campaign() {
    let root = std::path::PathBuf::from(std::env::var_os("KIOKU_LOT9C_ROOT").unwrap());
    let mode = std::env::var("KIOKU_LOT9C_MODE").unwrap();
    assert!(root.is_absolute() && root.is_dir());
    assert_eq!(
        fs::read_dir(&root).unwrap().count(),
        0,
        "root must be empty"
    );
    assert!(mode == "cache" || mode == "baseline");
    let exe = std::env::current_exe().unwrap();
    let mut children: Vec<_> = (0..WORKERS)
        .map(|i| {
            Command::new(&exe)
                .args(["--ignored", "--exact", "lot9c_child"])
                .env("KIOKU_LOT9C_ID", i.to_string())
                .spawn()
                .unwrap()
        })
        .collect();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !(0..WORKERS).all(|i| root.join(format!("ready-{i}")).is_dir()) {
        if std::time::Instant::now() >= deadline {
            fs::write(root.join("abort"), b"barrier timeout").unwrap();
            for child in &mut children {
                let _ = child.wait();
            }
            panic!("barrier timeout");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    fs::write(root.join("release"), b"all ready").unwrap();
    let mut ok = true;
    for child in &mut children {
        ok &= child.wait().unwrap().success();
    }
    assert!(ok, "one or more workers failed");
    let count = fs::read_to_string(root.join("calls"))
        .unwrap()
        .lines()
        .count();
    assert_eq!(count, if mode == "cache" { 1 } else { WORKERS });
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mut inodes = std::collections::BTreeSet::new();
        for i in 0..WORKERS {
            for j in 0..FILES {
                let m = fs::metadata(root.join(format!("private-{i}/outputs/file-{j}"))).unwrap();
                assert!(
                    inodes.insert((m.dev(), m.ino())),
                    "copies must not share inodes"
                );
            }
        }
    }
    println!(
        "lot9c: {mode}, {WORKERS} workers, {count} preparations, {FILES} verified files per worker"
    );
}

#[test]
fn lot9c_explicit_poison_revokes_copy() {
    use kioku::select::cache::CacheError;
    let root = tempfile::tempdir().unwrap();
    let store = LocalCooperativeStore::open(root.path().join("store")).unwrap();
    let entry = store
        .prepare(&key(), |out| {
            fs::write(out.join("file"), b"ok")?;
            Ok(())
        })
        .unwrap();
    let copy = entry.private_copy(root.path().join("private")).unwrap();
    assert!(matches!(entry.poison(), Err(CacheError::Frozen(_))));
    assert!(matches!(
        copy.with_before_test_outputs(|_| Ok(())),
        Err(CacheError::Frozen(_))
    ));
    assert!(matches!(
        store.prepare(&key(), |_| panic!("must not retry")),
        Err(CacheError::Frozen(_))
    ));
}

#[test]
fn lot9c_interrupted_preparing_freezes() {
    use kioku::select::cache::CacheError;
    let root = tempfile::tempdir().unwrap();
    let store_root = root.path().join("store");
    let store = LocalCooperativeStore::open(&store_root).unwrap();
    let entry = store
        .prepare(&key(), |out| {
            fs::write(out.join("file"), b"ok")?;
            Ok(())
        })
        .unwrap();
    let copy = entry.private_copy(root.path().join("private")).unwrap();
    fs::write(
        store_root.join(entry.identity()).join("PREPARING"),
        b"interrupted",
    )
    .unwrap();
    assert!(matches!(
        copy.with_before_test_outputs(|_| Ok(())),
        Err(CacheError::Frozen(_))
    ));
    assert!(matches!(
        store.prepare(&key(), |_| panic!("must not retry")),
        Err(CacheError::Frozen(_))
    ));
}
