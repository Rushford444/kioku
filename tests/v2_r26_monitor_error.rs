#![cfg(target_os = "linux")]
use kioku::ci2::*;
use std::{fs, path::Path};

fn digest() -> Digest {
    Digest::new(&("b3:".to_owned() + &"a".repeat(64))).unwrap()
}

fn id(value: &str) -> Id {
    Id::new(value).unwrap()
}

fn task() -> TaskSpec {
    let reference = BlobRef {
        digest: digest(),
        size_bytes: 0,
        media_type: "application/octet-stream".into(),
    };
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
            argv: vec![
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/ci2/native/child.py")
                    .to_string_lossy()
                    .into_owned(),
                "zero-fd".into(),
            ],
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
            execution_recipe_ref: reference.clone(),
            identity_rule_digest: digest(),
            normalization_policy_ref: reference,
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

fn failed_monitor(after: &str) -> Result<native::Observation, native::SupervisorError> {
    let base = tempfile::tempdir().unwrap();
    let supervisor = base.path().join("diagnostic-supervisor.py");
    let product = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ci2/native/supervisor.py");
    let wrapper = format!(
        r"
import runpy,sys
main=runpy.run_path({product:?},run_name='probe')['main']
code=main()
sys.stderr.write('deliberate monitor failure\n')
sys.stderr.flush()
{after}
sys.exit(code)
",
        product = product.to_string_lossy(),
        after = after,
    );
    fs::write(&supervisor, wrapper).unwrap();
    native::supervise_with_stops_and_possession(
        &task(),
        Path::new("/usr/bin/python3"),
        base.path(),
        &base.path().join("run"),
        &supervisor,
        &base.path().join("stop"),
        &[],
        Some(&mut |_| Ok(())),
    )
}

#[test]
fn nonzero_possession_monitor_reports_exit_code_and_stays_a_source_failure() {
    let result = failed_monitor("sys.exit(75)");
    assert!(matches!(
        result,
        Err(native::SupervisorError::Wire(message))
            if message == "possession monitor failed: exit code 75; monitor diagnostic unavailable"
    ));
}

#[test]
fn signaled_possession_monitor_reports_signal_and_stays_a_source_failure() {
    let result = failed_monitor("import os,signal; os.kill(os.getpid(),signal.SIGTERM)");
    assert!(matches!(
        result,
        Err(native::SupervisorError::Wire(message))
            if message == "possession monitor failed: signal 15; monitor diagnostic unavailable"
    ));
}
