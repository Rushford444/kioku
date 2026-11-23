#![cfg(target_os = "linux")]
use kioku::ci2::snapshot;
use std::fs::{self, File, FileTimes, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::time::{Duration, Instant, SystemTime};

fn directory(path: &std::path::Path) {
    fs::DirBuilder::new().mode(0o700).create(path).unwrap();
}

#[test]
fn root_path_transition_during_capture_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let current = temp.path().join("current");
    let replacement = temp.path().join("replacement");
    let retired = temp.path().join("retired");
    for parent in [&current, &replacement] {
        directory(parent);
        directory(&parent.join("root"));
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(parent.join("root/input"))
            .unwrap();
        file.set_len(128 * 1024 * 1024).unwrap();
    }
    let root = current.join("root");
    let before = snapshot::capture(&root, &["input".into()]).unwrap();
    let input = root.join("input");
    File::open(&input)
        .unwrap()
        .set_times(FileTimes::new().set_accessed(SystemTime::UNIX_EPOCH + Duration::from_secs(1)))
        .unwrap();
    let initial = fs::metadata(&input).unwrap();
    let root_initial = fs::metadata(&root).unwrap();
    let old_access = initial.accessed().unwrap();
    let worker = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        while fs::metadata(&input).unwrap().accessed().unwrap() == old_access {
            assert!(
                Instant::now() < deadline,
                "capture did not read the fixture"
            );
            std::thread::yield_now();
        }
        fs::rename(&current, &retired).unwrap();
        fs::rename(&replacement, &current).unwrap();
        retired
    });
    let result = snapshot::capture(&root, &["input".into()]);
    let retired = worker.join().unwrap();
    let after = fs::metadata(retired.join("root/input")).unwrap();
    let root_after = fs::metadata(retired.join("root")).unwrap();
    assert_eq!(
        (
            initial.dev(),
            initial.ino(),
            initial.len(),
            initial.mtime(),
            initial.mtime_nsec(),
            initial.ctime(),
            initial.ctime_nsec()
        ),
        (
            after.dev(),
            after.ino(),
            after.len(),
            after.mtime(),
            after.mtime_nsec(),
            after.ctime(),
            after.ctime_nsec()
        )
    );
    assert_eq!(
        (
            root_initial.dev(),
            root_initial.ino(),
            root_initial.ctime(),
            root_initial.ctime_nsec()
        ),
        (
            root_after.dev(),
            root_after.ino(),
            root_after.ctime(),
            root_after.ctime_nsec()
        )
    );
    assert_eq!(
        before,
        snapshot::capture(&root, &["input".into()]).unwrap(),
        "unchanged bytes remain valid at the replacement root"
    );
    assert!(
        result.is_err(),
        "capture accepted a root path rebound to another inode"
    );
}
