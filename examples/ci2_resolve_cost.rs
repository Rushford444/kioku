//! Observational executable-approval cost probe, not a workload or cache benchmark.
use kioku::ci2::{
    config::resolve_approved_executable,
    domain::{RegisteredRoot, TaskSpec, Validate},
};
use serde::Deserialize;
use std::{error::Error, fs, time::Instant};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    root: RegisteredRoot,
    task: TaskSpec,
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let request_path = args.next().ok_or("one private request path is required")?;
    if args.next().is_some() {
        return Err("one private request path is required".into());
    }
    let request: Request = serde_json::from_slice(&fs::read(request_path)?)?;
    request.root.validate()?;
    request.task.validate()?;
    let started = Instant::now();
    let resolved = resolve_approved_executable(&request.root, &request.task)?;
    let duration_ns = u64::try_from(started.elapsed().as_nanos())?;
    println!(
        "{}",
        serde_json::json!({
            "schema": "kioku/bench/executable-approval-cost/v1",
            "duration_ns": duration_ns,
            "bytes": fs::metadata(resolved)?.len(),
            "binary_digest": request.task.command.executable.binary_digest,
            "scope": "resolve_approved_executable only; no native command executed",
        })
    );
    Ok(())
}
