"""Deterministic publication race: lease blocks the observed child, not prepare."""
import fcntl
import glob
import json
import os
import signal
import subprocess
import sys

repo, kioku, helper, tracer = sys.argv[1:]
signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGIO})
home = os.path.join(os.path.dirname(repo), "home")
os.mkdir(home)
env = {"PATH": "/usr/bin:/bin", "HOME": home, "LANG": "C.UTF-8",
       "KIOKU_SENTINEL_SECRET": "sk-test-SENTINEL", "KIOKU_STRACE": tracer}
subprocess.run(["git", "init", "--quiet", repo], env=env, check=True, timeout=20)
# Keep the leased input out of the AST diagnostic snapshot, which reads every
# git-visible file before tracing and would break the lease too early.
with open(os.path.join(repo, ".gitignore"), "w", encoding="ascii") as ignore:
    ignore.write("input\n")
input_path = os.path.join(repo, "input")
args = [kioku, "run", "--json", "--trust-trace", "--", helper, input_path]
state_glob = os.path.join(repo, ".git/kioku/trace-v2/state-*.json")
first = subprocess.run(args, cwd=repo, env=env, stdin=subprocess.DEVNULL,
                       stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30, check=True)
first_event = json.loads(first.stderr)
assert first_event["stored"] is True and first_event["trace_state"] == "complete"
assert first_event["decision"] == "executed" and first_event["exit_code"] == 0
assert len(glob.glob(state_glob)) == 1
with open(input_path, "wb") as file:
    file.write(b"input")
fd = os.open(input_path, os.O_RDWR)
proc = None
try:
    fcntl.fcntl(fd, fcntl.F_SETOWN, os.getpid())
    fcntl.fcntl(fd, fcntl.F_SETSIG, signal.SIGIO)
    try:
        fcntl.fcntl(fd, fcntl.F_SETLEASE, fcntl.F_WRLCK)
    except OSError as error:
        raise RuntimeError("Linux write lease unavailable for publication test") from error
    proc = subprocess.Popen(args, cwd=repo, env=env, stdin=subprocess.DEVNULL,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            start_new_session=True)
    assert signal.sigtimedwait({signal.SIGIO}, 30), "no lease-break signal"
    assert proc.poll() is None, "command exited before lease was released"
    children_path = f"/proc/{proc.pid}/task/{proc.pid}/children"
    with open(children_path, encoding="ascii") as children_file:
        children = children_file.read().split()
    names = []
    for child in children:
        with open(f"/proc/{int(child)}/comm", encoding="ascii") as comm:
            names.append(comm.read().strip())
    assert "strace" in names, "no live strace child after lease break"
    clean = subprocess.run([kioku, "clean", "--all"], cwd=repo, env=env,
                           stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                           stderr=subprocess.PIPE, timeout=30)
    assert clean.returncode == 0 and not glob.glob(state_glob), "clean did not remove state"
    fcntl.fcntl(fd, fcntl.F_SETLEASE, fcntl.F_UNLCK)
    _, stderr = proc.communicate(timeout=30)
    assert proc.returncode == 0, "traced command failed"
    event = json.loads(stderr)
    assert event["trace_state"] == "complete" and event["stored"] is False
    assert event["would_skip"] is False
    assert not glob.glob(state_glob), "concurrent clean was overwritten"
finally:
    os.close(fd)
    if proc is not None and proc.poll() is None:
        os.killpg(proc.pid, signal.SIGKILL)
        proc.wait(timeout=10)
