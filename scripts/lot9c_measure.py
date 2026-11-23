"""Measure one direct child on Linux, without global child accounting."""
import json
import math
import resource
import os
from pathlib import Path
import signal
import time


def _measure_native(argv, env, stdout_path, stderr_path, monitor, timeout=1800):
    if not hasattr(os, "wait4"):
        raise RuntimeError("os.wait4 unavailable")
    if not math.isfinite(timeout) or timeout <= 0:
        raise ValueError("timeout must be positive and finite")
    with Path(stdout_path).open("xb") as out, Path(stderr_path).open("xb") as err:
        start = time.perf_counter_ns()
        pid = os.fork()
        if pid == 0:
            try:
                os.dup2(out.fileno(), 1)
                os.dup2(err.fileno(), 2)
                os.execvpe(argv[0], argv, env)
            except BaseException as exc:
                os.write(2, (f"exec failed: {exc}\n").encode(errors="replace"))
                os._exit(127)
        deadline = start + int(timeout * 1_000_000_000)
        next_check = start
        interrupted = None
        terminated_at = None
        killed = False
        while True:
            try:
                waited, status, usage = os.wait4(pid, os.WNOHANG)
            except ChildProcessError as exc:
                raise RuntimeError("child status unavailable; measurement invalid") from exc
            now = time.perf_counter_ns()
            if waited == pid:
                break
            if interrupted is None and now >= next_check:
                try:
                    monitor()
                except Exception as exc:
                    interrupted = str(exc)
                next_check = now + 200_000_000
            if now >= deadline and terminated_at is None:
                interrupted = interrupted or "trial timeout"
                try:
                    os.kill(pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                terminated_at = now
            if terminated_at is not None and not killed and now - terminated_at >= 5_000_000_000:
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                killed = True
            time.sleep(0.005)
        return {"argv": list(argv), "stdout": str(stdout_path), "stderr": str(stderr_path),
                "wall": (now - start) / 1_000_000_000, "user": usage.ru_utime, "sys": usage.ru_stime,
                "exit_code": os.waitstatus_to_exitcode(status), "interrupted": interrupted}


def measure(argv, env, stdout_path, stderr_path, monitor, timeout=1800):
    # A fresh supervisor has no previously reaped children.
    read_fd, write_fd = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(read_fd)
        try:
            before = resource.getrusage(resource.RUSAGE_CHILDREN)
            if before.ru_utime != 0 or before.ru_stime != 0:
                raise RuntimeError("supervisor child accounting was not fresh")
            result = _measure_native(argv, env, stdout_path, stderr_path, monitor, timeout)
            after = resource.getrusage(resource.RUSAGE_CHILDREN)
            result["children_user"] = after.ru_utime - before.ru_utime
            result["children_sys"] = after.ru_stime - before.ru_stime
            payload = {"result": result}
        except BaseException as exc:
            payload = {"error": str(exc), "error_type": type(exc).__name__}
        try:
            os.write(write_fd, json.dumps(payload, allow_nan=False).encode())
        finally:
            os.close(write_fd)
        os._exit(0)
    os.close(write_fd)
    with os.fdopen(read_fd, "rb") as pipe:
        data = pipe.read()
    _, status = os.waitpid(pid, 0)
    if status != 0 or not data:
        raise RuntimeError("trial supervisor failed")
    payload = json.loads(data)
    if "error" in payload:
        if payload.get("error_type") == "FileExistsError":
            raise FileExistsError(payload["error"])
        raise RuntimeError("trial supervisor: " + payload["error"])
    return payload["result"]
