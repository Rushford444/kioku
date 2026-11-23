#![cfg(target_os = "linux")]
use kioku::ci2::*;
use std::{fs, path::Path};

fn digest() -> Digest {
    Digest::new(&("b3:".to_owned() + &"a".repeat(64))).unwrap()
}
fn id(value: &str) -> Id {
    Id::new(value).unwrap()
}
fn reference() -> BlobRef {
    BlobRef {
        digest: digest(),
        size_bytes: 0,
        media_type: "application/octet-stream".into(),
    }
}
fn task() -> TaskSpec {
    TaskSpec {
        id: id("fixture-parent"),
        adapter: id("python-eval-none"),
        command: CommandSpec {
            executable: ToolIdentity {
                name: "python3".into(),
                version: "3.11".into(),
                binary_digest: digest(),
                installation_digest: digest(),
            },
            argv: vec![],
            cwd: String::new(),
            timeout_ms: 15_000,
            recipe_revision: id("v1"),
        },
        inputs: vec![],
        outputs: vec![],
        dependencies: vec![],
        env: vec![],
        resources: ResourceRecipe {
            heavy_slots: 1,
            capture_slots: 1,
            max_state_bytes: 65536,
            max_stderr_bytes: 65536,
        },
        required: true,
        result_contract: ResultContract {
            id: id("nontest"),
            kind: ResultKind::NonTest,
            adapter_id: id("python-eval"),
            reporter_version: id("explicit-exit-native-v1"),
            inventory_recipe_ref: None,
            execution_recipe_ref: reference(),
            identity_rule_digest: digest(),
            normalization_policy_ref: reference(),
            native_skips: vec![],
            expected_empty: false,
            complete_required: true,
        },
        cancellation: CancellationPolicy {
            cancellable: true,
            term_grace_ms: 200,
            max_drain_ms: 800,
        },
        admission: AdmissionPolicy {
            result_cache: false,
            single_flight: false,
        },
    }
}

fn execute_fixture(
    mode: &str,
    after: &str,
) -> (
    tempfile::TempDir,
    Result<native::Observation, native::SupervisorError>,
) {
    let base = tempfile::tempdir().unwrap();
    let supervisor = base.path().join("diagnostic-supervisor.py");
    let product = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ci2/native/supervisor.py");
    let wrapper = format!(
        r"
import json,runpy,sys
from pathlib import Path
main=runpy.run_path({product:?},run_name='probe')['main']
raw=Path({base:?})/'monitor.stdout'
class Tee:
    def write(self,data):
        with raw.open('ab') as out: out.write(data.encode())
        return sys.__stdout__.write(data)
    def flush(self): sys.__stdout__.flush()
sys.stdout=Tee()
try:
    code=main()
    {after}
    sys.exit(code)
except SystemExit: raise
except BaseException as error:
    (Path({base:?})/'monitor-error.json').write_text(json.dumps({{'type':type(error).__name__,'message':str(error)}}))
    raise
",
        product = product.to_string_lossy(),
        base = base.path().to_string_lossy()
    );
    fs::write(&supervisor, wrapper).unwrap();
    let mut spec = task();
    spec.command.argv = vec![
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/ci2/native/child.py")
            .to_string_lossy()
            .into_owned(),
        mode.into(),
        base.path()
            .join("run/descendant.pid")
            .to_string_lossy()
            .into_owned(),
    ];
    let result = native::supervise_with_stops_and_possession(
        &spec,
        Path::new("/usr/bin/python3"),
        base.path(),
        &base.path().join("run"),
        &supervisor,
        &base.path().join("stop"),
        &[],
        Some(&mut |_| Ok(())),
    );
    (base, result)
}

#[test]
fn structured_native_unknown_is_observable_without_absence_proof() {
    let (_base, result) = execute_fixture("fd-closed-live", "pass");
    let observation = result.unwrap();
    let failure = observation.supervision_error.unwrap();
    assert_eq!(failure.code, "supervisor.error");
    assert_eq!(failure.reason, 3);
    assert_eq!(failure.message, "live descendant survived leader exit");
    assert!(!observation.native.cpu_complete);
    assert!(!observation.native.descendants_drained);
    assert!(observation.absence_proof.is_none());
    assert_eq!(observation.reaped_pids.len(), 2);
}

#[test]
fn nonzero_monitor_never_accepts_even_a_valid_terminal_record() {
    for mode in ["zero-fd", "fd-closed-live"] {
        let (base, result) = execute_fixture(mode, "sys.exit(75)");
        let bytes = fs::read(base.path().join("monitor.stdout")).unwrap();
        let frames: Vec<_> = bytes.splitn(2, |byte| *byte == b'\n').collect();
        assert_eq!(frames.len(), 2);
        let terminal: serde_json::Value = serde_json::from_slice(frames[1]).unwrap();
        assert_eq!(terminal["schema"], "kioku/ci2/native-supervision/v1");
        assert_eq!(terminal["cpu_complete"], mode == "zero-fd");
        assert!(matches!(
            result,
            Err(native::SupervisorError::Wire(message))
                if message == "possession monitor failed: exit code 75; monitor diagnostic unavailable"
        ));
    }
}
