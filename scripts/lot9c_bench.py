#!/usr/bin/env python3
"""Local opt-in lot9c campaign. Never invoked by the normal test suite."""
import fcntl
import hashlib
from datetime import datetime, timezone
import json
import math
import os
from pathlib import Path
import statistics
import subprocess
import sys
import time
import uuid
from lot9c_measure import measure

REPO = Path(__file__).resolve().parents[1]
# The heavy-build lock lives outside the repository on the machine that runs the
# campaign. Point KIOKU_HEAVY_LOCK at a file two processes can flock; the
# default keeps the lock inside this checkout's target directory.
LOCK = Path(os.environ.get("KIOKU_HEAVY_LOCK", REPO / "target" / ".heavy.lock"))
MIN_FREE = 30 * 1024**3
STOP_FREE = 25 * 1024**3
MAX_TRIAL = 2 * 1024**3
THRESHOLDS = {"wall": 0.80, "cpu": 0.80, "disk": 2.00}


def check(condition, message):
    if not condition:
        raise RuntimeError(message)


def command(args, **kwargs):
    return subprocess.run(args, check=True, text=True, **kwargs)


def allocated(path):
    result = command(["du", "-sB1", "--", str(path)], capture_output=True)
    return int(result.stdout.split("\t", 1)[0])


def free(path):
    st = os.statvfs(path)
    return st.f_bavail * st.f_frsize


def ratios(trials):
    check(len(trials) == 6, "six complete trials required")
    pairs = []
    for i in range(0, 6, 2):
        baseline, cache = trials[i:i + 2]
        check(baseline["mode"] == "baseline" and cache["mode"] == "cache", "trial order")
        values = {}
        for metric in THRESHOLDS:
            denominator = baseline[metric]
            numerator = cache[metric]
            check(isinstance(denominator, (int, float)) and isinstance(numerator, (int, float))
                  and math.isfinite(denominator) and math.isfinite(numerator)
                  and denominator > 0 and numerator >= 0, "missing/invalid measurement: " + metric)
            values[metric] = numerator / denominator
        pairs.append(values)
    medians = {metric: statistics.median(pair[metric] for pair in pairs) for metric in THRESHOLDS}
    return pairs, medians


def mounts(paths):
    result = []
    for path in paths:
        data = json.loads(command(["findmnt", "-J", "-T", str(path), "-o", "SOURCE,FSTYPE,TARGET"], capture_output=True).stdout)
        fs = data["filesystems"]
        check(len(fs) == 1, "cannot identify filesystem for " + str(path))
        result.append({"path": str(path), "device": os.stat(path).st_dev, **fs[0]})
    return result


def check_space(paths, minimum, stop):
    check(stop is None or not stop.exists(), "operator DISQUE-BAS stop file exists")
    for path in paths:
        check(free(path) >= minimum, "insufficient free space at " + str(path))


def tool_version(args, required):
    text = command(args, capture_output=True).stdout
    check(required in text, "required GNU tool missing: " + " ".join(args))
    return text.splitlines()[0]


def run_trial(binary, root, mode, report_path, index, paths, stop, budget):
    check_space(paths, MIN_FREE, stop)
    check(not any(root.iterdir()), "trial directory not empty: " + str(root))
    stdout = report_path.with_name(report_path.name + f".trial-{index}.stdout")
    stderr = report_path.with_name(report_path.name + f".trial-{index}.stderr")
    env = os.environ.copy()
    env.update(KIOKU_LOT9C_ROOT=str(root), KIOKU_LOT9C_MODE=mode)
    argv = [str(binary), "--ignored", "--exact", "lot9c_campaign", "--nocapture"]

    def monitor():
        try:
            check_space(paths, STOP_FREE, stop)
        except Exception:
            (root / "abort").touch(exist_ok=True)
            raise

    observation = measure(argv, env, stdout, stderr, monitor)
    check(observation["interrupted"] is None, "trial interrupted: " + str(observation["interrupted"]))
    check(observation["exit_code"] == 0, "trial test failed: " + str(observation["exit_code"]) + "; stderr: " + str(stderr))
    check_space(paths, STOP_FREE, stop)
    wall, user, system, child_user, child_sys = (observation[key] for key in
        ("wall", "user", "sys", "children_user", "children_sys"))
    check(all(isinstance(x, (int, float)) and math.isfinite(x) and x >= 0
              for x in (wall, user, system, child_user, child_sys)) and wall > 0,
          "invalid time values")
    disk = allocated(root)
    check(disk <= budget, "trial directory exceeded configured budget")
    return {"mode": mode, "root": str(root), "wall": wall, "user": user,
            "sys": system, "wait4_cpu": user + system,
            "children_user": child_user, "children_sys": child_sys,
            "children_cpu_scope": "fresh supervisor RUSAGE_CHILDREN; native process and descendants it reaps",
            "cpu": child_user + child_sys, "disk": disk, "disk_budget": budget,
            "argv": observation["argv"], "stdout": str(stdout), "stderr": str(stderr),
            "stdout_sha256": hashlib.sha256(stdout.read_bytes()).hexdigest(),
            "stderr_sha256": hashlib.sha256(stderr.read_bytes()).hexdigest()}

def campaign(result, report_path):
    os.chdir(REPO)
    check(hasattr(os, "wait4"), "os.wait4 unavailable")
    result["tools"] = {"time": "fresh supervisor resource.RUSAGE_CHILDREN + os.wait4 + time.perf_counter_ns",
                       "du": tool_version(["du", "--version"], "GNU coreutils")}
    result["measurement"] = {"api": "resource.getrusage(RUSAGE_CHILDREN)", "wall": "perf_counter_ns",
        "scope": "whole lot9c_campaign process: barrier, preparation, private copy and verification; excludes compilation",
        "children_cpu_scope": "fresh supervisor; native process and descendants it reaps; unjoined descendants excluded",
        "secondary_api": "os.wait4 direct native PID; not added to RUSAGE_CHILDREN",
        "script_sha256": {name: hashlib.sha256((REPO / "scripts" / name).read_bytes()).hexdigest()
                          for name in ("lot9c_bench.py", "lot9c_measure.py")}}
    roots = [Path(p) for p in json.loads(os.environ["KIOKU_LOT9C_TRIALS"])]
    check(len(roots) == 6 and len({p.resolve() for p in roots}) == 6, "six distinct roots required")
    check(all(p.is_absolute() and p.is_dir() and not any(p.iterdir()) for p in roots), "six empty absolute directories required")
    check(all(not (a.resolve() in b.resolve().parents) for a in roots for b in roots if a != b), "nested trial roots")
    stop_value = os.environ.get("KIOKU_TEST_OPERATOR_STOP_FILE")
    stop = Path(stop_value) if stop_value else None
    check(stop is None or (stop.is_absolute() and stop.parent.is_dir()), "invalid operator stop file")
    budget = int(os.environ["KIOKU_LOT9C_MAX_BYTES"])
    check(32 * 1024**2 <= budget <= MAX_TRIAL, "budget must be 32 MiB through 2 GiB")
    check(os.environ.get("KIOKU_LOT9C_ROOT_APPROVED") == "1", "ROOT approval required")
    check(os.environ.get("KIOKU_LOT9C_STABLE_HOST") == "1", "operator must assert no concurrent reconstruction or hostile writers")
    paths = roots + [REPO, Path("/tmp"), report_path.parent, LOCK.parent]
    result["mounts"] = mounts(paths)
    result["conditions"] = {"min_free_bytes": MIN_FREE, "stop_free_bytes": STOP_FREE,
                            "max_trial_bytes": budget, "stop_file": str(stop) if stop else None,
                            "stable_host_asserted": True, "offline": True,
                            "scope": "synthetic; no CLI fallback or hostile writers"}
    check_space(paths, MIN_FREE, stop)
    env = os.environ.copy()
    cargo_bin = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo")) / "bin"
    env["PATH"] = f"{cargo_bin}:{env.get('PATH', '')}"
    env["CARGO_TARGET_DIR"] = str(REPO / "target")
    env["CARGO_NET_OFFLINE"] = "true"
    result["commit"] = command(["git", "rev-parse", "HEAD"], capture_output=True).stdout.strip()
    check(not command(["git", "status", "--porcelain"], capture_output=True).stdout.strip(), "clean frozen candidate required")
    profile = os.environ.get("KIOKU_LOT9C_PROFILE", "debug")
    check(profile in ("debug", "release"), "profile must be debug or release")
    profile_args = ["--release"] if profile == "release" else []
    build_args = ["cargo", "test", *profile_args, "--locked", "--offline", "--test", "select_cache_lot9c_bench", "--no-run", "--message-format=json"]
    build = command(build_args, env=env, capture_output=True)
    artifacts = [row for line in build.stdout.splitlines() if (row := json.loads(line)).get("reason") == "compiler-artifact" and row.get("target", {}).get("name") == "select_cache_lot9c_bench" and row.get("executable")]
    check(len(artifacts) == 1, "cannot identify test binary")
    artifact = artifacts[0]
    binary = Path(artifact["executable"])
    check(binary.parent.name == "deps" and binary.parent.parent.name == profile,
          "artifact executable profile mismatch")
    check(artifact["profile"].get("opt_level") == ("3" if profile == "release" else "0"),
          "artifact optimization profile mismatch")
    result["build"] = {"profile": profile, "argv": build_args, "artifact_profile": artifact["profile"]}
    result["binary_path"] = str(binary)
    result["binary_sha256"] = hashlib.sha256(binary.read_bytes()).hexdigest()
    result["rustc"] = command(["rustc", "-vV"], env=env, capture_output=True).stdout.strip()
    check_space(paths, MIN_FREE, stop)
    command(["cargo", "test", *profile_args, "--locked", "--offline", "--test", "select_cache_lot9b"], env=env)
    command([str(binary)])
    result["safety"] = "passed"
    for i, root in enumerate(roots):
        mode = "baseline" if i % 2 == 0 else "cache"
        result["trials"].append(run_trial(binary, root, mode, report_path, i, paths, stop, budget))
    result["ratios"], result["medians"] = ratios(result["trials"])
    result["status"] = "GO" if all(result["medians"][k] <= v for k, v in THRESHOLDS.items()) else "NO-GO"


def main():
    report = os.environ.get("KIOKU_LOT9C_REPORT")
    if not report or not Path(report).is_absolute():
        print("KIOKU_LOT9C_REPORT must be an absolute new path; no report written", file=sys.stderr)
        return 2
    path = Path(report)
    result = {"schema": "kioku/lot9c-benchmark/v1", "status": "INDETERMINE", "error": None,
              "run_id": str(uuid.uuid4()),
              "generated_start_utc": datetime.now(timezone.utc).isoformat(),
              "thresholds": THRESHOLDS, "trials": [], "safety": "not-run"}
    try:
        with path.open("x", encoding="utf-8") as out:
            try:
                with LOCK.open("r") as lock:
                    while True:
                        try:
                            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                            break
                        except BlockingIOError:
                            stop_value = os.environ.get("KIOKU_TEST_OPERATOR_STOP_FILE")
                            check(not stop_value or not Path(stop_value).exists(), "operator stop file exists while waiting")
                            time.sleep(1)
                    campaign(result, path)
            except Exception as exc:
                result["status"] = "INDETERMINE"
                result["error"] = f"{type(exc).__name__}: {exc}"
            json.dump(result, out, indent=2, allow_nan=False)
            out.write("\n")
            out.flush()
            os.fsync(out.fileno())
    except Exception as exc:
        print(f"report creation/write failed: {exc}", file=sys.stderr)
        return 2
    return 0 if result["status"] == "GO" else 1


if __name__ == "__main__":
    sys.exit(main())
