#!/usr/bin/python3
"""Linux subprocess monitor: pipe EOF is never proof of descendant termination."""
import ctypes, hashlib, json, os, select, signal, subprocess, sys, threading, time

POLL = .02
PR_SET_CHILD_SUBREAPER = 36

def enable_subreaper():
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) != 0:
        raise OSError(ctypes.get_errno(), "PR_SET_CHILD_SUBREAPER refused")

def identity(pid):
    try:
        with open("/proc/%d/stat" % pid, "rb") as source:
            stat = source.read()
        end = stat.rfind(b")")
        fields = stat[end + 2:].split() if end >= 0 else []
        if (not stat.startswith(("%d (" % pid).encode("ascii"))
                or stat[end:end + 2] != b") " or len(fields) < 20):
            raise ValueError("invalid proc stat")
        return (int(fields[2]), int(fields[19]), fields[0].decode("ascii"), int(fields[1]))
    except (FileNotFoundError, ProcessLookupError):
        # An open proc stat fd can return ESRCH after the task is reaped.
        return None

def process_group(pgid):
    members = {}
    with os.scandir("/proc") as entries:
        for entry in entries:
            if entry.name.isascii() and entry.name.isdigit():
                pid = int(entry.name)
                current = identity(pid)
                if current is not None and current[0] == pgid:
                    members[pid] = current
    return members

def live(members):
    return {pid: info for pid, info in members.items() if info[2] != "Z"}

def drain(stream, path, digest):
    total = 0
    with open(path, "xb") as output:
        while True:
            block = stream.read(65536)
            if not block:
                break
            digest.update(block)
            total += len(block)
            output.write(block)
    stream.close()
    return {"bytes": total, "digest": "sha256:" + digest.hexdigest(), "path": path}

def write_checkpoint(work, header, result, no_children, original_group_empty):
    # This private monitor control file is not a task output or a public report.
    path = os.path.join(work, "possession-closure.json")
    temporary = path + ".tmp"
    fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "w", encoding="ascii") as output:
        json.dump({
            "schema": "kioku/ci2/native-closure/v1", "header": header,
            "result": result, "no_children": no_children,
            "original_group_empty": original_group_empty,
        }, output, sort_keys=True)
        output.flush()
        os.fsync(output.fileno())
    # Refuse to replace any earlier checkpoint in this per-invocation directory.
    os.link(temporary, path, follow_symlinks=False)
    os.unlink(temporary)
    directory = os.open(work, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)

def main():
    # A separate session keeps an owned child PGID distinct from this monitor.
    os.setsid()
    with open(sys.argv[1], "rb") as source:
        payload = source.read(1048577)
    if len(payload) > 1048576:
        raise SystemExit(2)
    request = json.loads(payload)
    if not isinstance(request, dict) or request.get("schema") != "kioku/ci2/native-supervisor-request/v1":
        raise SystemExit(2)
    argv = request["argv"]
    if not isinstance(argv, list) or not argv or any(not isinstance(a, str) or not a or "\0" in a for a in argv):
        raise SystemExit(2)
    if not os.path.isabs(argv[0]) or not os.path.isfile(argv[0]) or not os.access(argv[0], os.X_OK):
        raise SystemExit(2)
    if not os.path.isdir(request["private_home"]) or not os.path.isdir(request["cwd"]):
        raise SystemExit(2)
    env = request["environment"]
    artifacts = request["artifacts"]
    if not isinstance(env, dict) or any(not isinstance(k, str) or not isinstance(v, str) for k, v in env.items()):
        raise SystemExit(2)
    if not isinstance(artifacts, dict) or set(artifacts) != {"stdout", "stderr"} or any(
        not isinstance(path, str) or not os.path.isabs(path) for path in artifacts.values()
    ):
        raise SystemExit(2)
    stop_files = [request["stop_file"]] + request["stop_files"]
    if any(not isinstance(path, str) or not os.path.isabs(path) for path in stop_files):
        raise SystemExit(2)
    grace = int(request["grace_ms"])
    timeout = int(request["timeout_ms"])
    drain_ms = int(request["max_drain_ms"])
    if min(grace, timeout, drain_ms) <= 0 or grace >= timeout or grace > drain_ms:
        raise SystemExit(2)

    # Only this short-lived monitor adopts orphans; the Rust parent is unchanged.
    enable_subreaper()
    process = subprocess.Popen(
        argv, cwd=request["cwd"], env=env, stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True,
    )
    leader = identity(process.pid)
    if leader is None or leader[0] != process.pid:
        # Still our unreaped direct child; never search for a replacement PID.
        process.kill()
        process.wait()
        raise RuntimeError("cannot establish child process group")
    start_ticks = leader[1]
    started = time.monotonic_ns()
    started_ms = time.time_ns() // 1000000
    possession_error = None
    header = None
    if request.get("possession", False):
        try:
            with open("/proc/sys/kernel/random/boot_id", encoding="ascii") as source:
                boot_id = source.read(37).strip()
            header = {
                "schema": "kioku/ci2/native-possession/v1",
                "monitor_pid": os.getpid(),
                "source": "owned-direct-child-original-process-group",
                "evidence": {
                    "process": {"pid": process.pid, "pgid": process.pid, "started_ms": started_ms},
                    "start_ticks": start_ticks, "boot_id": boot_id,
                },
            }
            sys.stdout.write(json.dumps(header, sort_keys=True) + "\n")
            sys.stdout.flush()
            # No wait4 before the durable association: even a fast child retains
            # its identity until the controller acknowledges this frame.
            ready, _, _ = select.select([sys.stdin], [], [], 5)
            if not ready or os.read(sys.stdin.fileno(), 9) != b"continue\n":
                possession_error = "possession acknowledgement missing"
        except (OSError, ValueError) as error:
            possession_error = "possession handshake: " + type(error).__name__
    results, stream_errors = [None, None], []
    def reader(channel):
        try:
            results[channel] = drain(
                process.stdout if channel == 0 else process.stderr,
                artifacts["stdout" if channel == 0 else "stderr"], hashlib.sha256(),
            )
        except BaseException as error:
            stream_errors.append("stream %d: %s" % (channel, type(error).__name__))
    workers = [threading.Thread(target=reader, args=(i,), daemon=True) for i in (0, 1)]
    for worker in workers:
        worker.start()

    # This single monitor owns every wait4. Popen.poll/wait must not race it.
    reaped = {}
    user_ns = system_ns = max_rss = 0
    unknown = [possession_error] if possession_error else []
    no_children = False
    def collect_children():
        nonlocal user_ns, system_ns, max_rss, no_children
        while True:
            try:
                pid, status, usage = os.wait4(-1, os.WNOHANG)
            except ChildProcessError:
                no_children = True
                return
            if pid == 0:
                no_children = False
                return
            reaped[pid] = os.waitstatus_to_exitcode(status)
            user_ns += int(usage.ru_utime * 1_000_000_000)
            system_ns += int(usage.ru_stime * 1_000_000_000)
            max_rss = max(max_rss, int(usage.ru_maxrss) * 1024)
    def members():
        found = process_group(process.pid)
        collect_children()
        for pid in list(found):
            if pid in reaped:
                del found[pid]
        return found
    def signal_members(sig):
        # A numeric PGID can be reused after its leader exits. Signal only
        # identified descendants, through pidfds, never a bare PID or PGID.
        all_processes = {}
        with os.scandir("/proc") as entries:
            for entry in entries:
                if entry.name.isascii() and entry.name.isdigit():
                    pid = int(entry.name)
                    info = identity(pid)
                    if info is not None:
                        all_processes[pid] = info
        owned = {os.getpid()}
        while True:
            children = {pid for pid, info in all_processes.items() if info[3] in owned}
            expanded = owned | children
            if expanded == owned:
                break
            owned = expanded
        for pid, info in live(members()).items():
            snapshot = all_processes.get(pid)
            if (pid not in owned or snapshot is None or
                    (info[0], info[1]) != (snapshot[0], snapshot[1]) or
                    (info[3] != snapshot[3] and info[3] != os.getpid()) or
                    (pid == process.pid and info[1] != start_ticks)):
                unknown.append("group member identity not established")
                continue
            try:
                fd = os.pidfd_open(pid)
                try:
                    current_pid = pid
                    verified = True
                    while current_pid != os.getpid():
                        expected = all_processes.get(current_pid)
                        current = identity(current_pid)
                        if (expected is None or current is None or
                                (current[0], current[1]) != (expected[0], expected[1]) or
                                (current[3] != expected[3] and current[3] != os.getpid())):
                            verified = False
                            break
                        # PR_SET_CHILD_SUBREAPER may adopt this already-owned
                        # process during signalling. Only our exact monitor can
                        # replace its parent; PGID and start ticks must still match.
                        current_pid = current[3]
                    if not verified:
                        unknown.append("group member identity changed")
                        continue
                    signal.pidfd_send_signal(fd, sig)
                finally:
                    os.close(fd)
            except ProcessLookupError:
                continue

    stopped, interrupted = [], None
    deadline = started + timeout * 1_000_000
    collect_children()
    if possession_error:
        interrupted = "possession"
        signal_members(signal.SIGTERM)
    while process.pid not in reaped:
        stopped = [path for path in stop_files if os.path.exists(path)]
        if stopped:
            interrupted = "stop"
        elif time.monotonic_ns() >= deadline:
            interrupted = "timeout"
        if interrupted is not None:
            signal_members(signal.SIGTERM)
            break
        time.sleep(POLL)
        collect_children()
    term_time = time.monotonic() if interrupted is not None else None
    grace_deadline = term_time + grace / 1000 if term_time is not None else None
    first_members = members()
    live_found = live(first_members)
    # ECHILD establishes that this monitor has no direct or adopted children.
    # Final proof still checks the original group with a fresh scan.
    terminal_empty = (no_children and process.pid in reaped and not first_members
                      and interrupted is None and not unknown and not stream_errors)
    if live_found and term_time is None:
        unknown.append("live descendant survived leader exit")
        signal_members(signal.SIGTERM)
        term_time = time.monotonic()
        grace_deadline = term_time + grace / 1000
    kill_deadline = time.monotonic() + drain_ms / 1000
    kill_time = None
    if not terminal_empty:
        while (process.pid not in reaped or live(members())) and time.monotonic() < min(kill_deadline, grace_deadline or kill_deadline):
            collect_children()
            time.sleep(.005)
        if process.pid not in reaped or live(members()):
            signal_members(signal.SIGKILL)
            kill_time = time.monotonic()
    final_deadline = time.monotonic() + drain_ms / 1000
    if not terminal_empty:
        while time.monotonic() < final_deadline:
            collect_children()
            # An empty original group does not close custody of adopted children
            # that changed session/group. Keep waiting within the existing budget.
            if process.pid in reaped and not live(members()) and no_children:
                break
            time.sleep(.005)
    collect_children()
    for worker in workers:
        worker.join(max(0, final_deadline - time.monotonic()))
    final_members = members()
    survivors = live(final_members)
    # ECHILD covers adopted descendants outside the original process group;
    # pipe EOF and an empty group alone do not establish their termination.
    collect_children()
    complete_group = not survivors and process.pid in reaped and no_children
    if not no_children:
        unknown.append("unreaped or escaped descendant remains")
    if survivors:
        unknown.append("live descendants after SIGKILL deadline: " + ",".join(map(str, survivors)))
    if process.pid not in reaped:
        unknown.append("direct leader wait did not finish")
    if any(worker.is_alive() for worker in workers) or any(not isinstance(r, dict) for r in results):
        unknown.append("stdout/stderr collection incomplete: " + ",".join(stream_errors or ["reader deadline"]))
    # Zombies adopted by us are reaped above. External-reaped members leave incomplete CPU/close proof.
    missing_zombies = [pid for pid, info in final_members.items() if pid not in reaped]
    if missing_zombies:
        unknown.append("process members not reaped by this monitor: " + ",".join(map(str, missing_zombies)))
    # A kill required because of a timeout/cancel means the final group resource interval is known
    # only when all direct/adopted members were waited and all streams have closed.
    complete_cpu = complete_group and not missing_zombies and not unknown
    if unknown:
        complete_cpu = False
    code = reaped.get(process.pid)
    if code is None:
        raise RuntimeError("direct leader status unavailable")
    # The signal is the observed child wait status, not the requested cancellation signal.
    record = {
        "schema": "kioku/ci2/native-supervision/v1",
        "returncode": code,
        "exit_code": code if code is not None and code >= 0 else None,
        "signal": -code if code is not None and code < 0 else None,
        "wall_ns": time.monotonic_ns() - started,
        "cpu_user_ns": user_ns, "cpu_system_ns": system_ns, "max_rss_bytes": max_rss,
        "cpu_complete": complete_cpu, "descendants_drained": complete_group,
        "started_ms": started_ms, "ended_ms": time.time_ns() // 1000000,
        "timed_out": interrupted == "timeout", "interrupted": interrupted == "stop",
        "stopped_files": stopped,
        "cpu_source": "wait4 cumulative direct and subreaper-adopted children; each PID reaped once",
        "streams": [result for result in results if isinstance(result, dict)],
        "reaped_direct_and_adopted": sorted(reaped),
        "error": None if not unknown else {
            "code": "supervisor.error", "reason": 3,
            "message": "; ".join(dict.fromkeys(unknown)),
        },
    }
    if term_time is not None:
        record["term_to_kill_ns"] = None if kill_time is None else int((kill_time - term_time) * 1_000_000_000)
    if header is not None:
        write_checkpoint(request["private_home"], header, record, no_children, not final_members)
    json.dump(record, sys.stdout, sort_keys=True)
    return 0

if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except SystemExit:
        raise
    except BaseException as error:
        # Exceptions may carry paths, argv or environment values. Never emit them.
        kind = type(error).__name__ if type(error) in (
            OSError, ProcessLookupError, FileNotFoundError, PermissionError,
            ValueError, RuntimeError,
        ) else "Other"
        number = error.errno if isinstance(error, OSError) else None
        number = number if isinstance(number, int) and 0 <= number <= 4095 else None
        print(json.dumps({"schema": "kioku/ci2/native-diagnostic/v1",
                          "kind": kind, "errno": number}), file=sys.stderr)
        raise SystemExit(75)
