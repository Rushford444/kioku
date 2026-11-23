"""Real process regressions for the native monitor; no external fixtures."""
import ctypes
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time
import unittest

SUPERVISOR = Path(__file__).with_name("supervisor.py").resolve()

CHILD = r"""
import os, signal, sys, time
pidfile, escape = sys.argv[1:]
r, w = os.pipe()
pid = os.fork()
if pid == 0:
    os.close(r)
    if escape == 'yes':
        os.setsid()
    with open('/proc/self/stat', 'rb') as f:
        ticks = f.read().rsplit(b') ', 1)[1].split()[19].decode()
    with open(pidfile, 'w') as f:
        f.write('%d %s' % (os.getpid(), ticks))
    os.close(1)
    os.close(2)
    os.write(w, b'1')
    os.close(w)
    while True:
        time.sleep(1)
os.close(w)
os.read(r, 1)
os.close(r)
"""

class SupervisorDescendants(unittest.TestCase):
    def setUp(self):
        if ctypes.CDLL(None, use_errno=True).prctl(36, 1, 0, 0, 0) != 0:
            raise OSError(ctypes.get_errno(), "subreaper unavailable")
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.pidfile = self.root / "child.pid"
        self.addCleanup(self.cleanup_child)

    def cleanup_child(self):
        if not self.pidfile.exists():
            return
        pid, ticks = self.pidfile.read_text().split()
        try:
            fd = os.pidfd_open(int(pid))
        except ProcessLookupError:
            return
        try:
            try:
                stat = Path('/proc', pid, 'stat').read_bytes().rsplit(b') ', 1)[1].split()
                if stat[19].decode() == ticks:
                    signal.pidfd_send_signal(fd, signal.SIGKILL)
            except ProcessLookupError:
                pass
        finally:
            os.close(fd)
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            try:
                waited, _ = os.waitpid(int(pid), os.WNOHANG)
            except ChildProcessError:
                return
            if waited:
                return
            time.sleep(.01)
        self.fail("owned detached fixture was not reaped")

    def run_monitor(self, argv, timeout=1000, possession=False):
        request = {
            'schema': 'kioku/ci2/native-supervisor-request/v1',
            'argv': argv, 'cwd': str(self.root), 'private_home': str(self.root),
            'environment': {}, 'artifacts': {'stdout': str(self.root/'out'), 'stderr': str(self.root/'err')},
            'stop_file': str(self.root/'stop'), 'stop_files': [],
            'grace_ms': 100, 'timeout_ms': timeout, 'max_drain_ms': 500,
            'possession': possession,
        }
        path = self.root/'request.json'
        path.write_text(json.dumps(request))
        result = subprocess.run(['/usr/bin/python3', str(SUPERVISOR), str(path)], input=b'continue\n' if possession else None, capture_output=True, timeout=8, check=True)
        return json.loads(result.stdout.split(b'\n', 1)[1] if possession else result.stdout)

    def test_fd_closed_live_descendant_is_killed_and_reaped(self):
        result = self.run_monitor(['/usr/bin/python3', '-c', CHILD, str(self.pidfile), 'no'])
        self.assertTrue(result['descendants_drained'])
        self.assertIsNotNone(result['error'])
        self.assertEqual(len(result['reaped_direct_and_adopted']), 2)
        self.assertFalse(Path('/proc', self.pidfile.read_text().split()[0]).exists())

    def test_detached_fd_closed_descendant_is_not_reported_drained(self):
        result = self.run_monitor(['/usr/bin/python3', '-c', CHILD, str(self.pidfile), 'yes'], possession=True)
        checkpoint = json.loads((self.root/'possession-closure.json').read_bytes())
        self.assertFalse(checkpoint['no_children'])
        self.assertFalse(checkpoint['result']['cpu_complete'])
        self.assertFalse(result['descendants_drained'])
        self.assertFalse(result['cpu_complete'])
        self.assertIsNotNone(result['error'])
        self.assertTrue(Path('/proc', self.pidfile.read_text().split()[0]).exists())

    def test_possession_header_precedes_terminal_and_matches_owned_group(self):
        request = {
            'schema': 'kioku/ci2/native-supervisor-request/v1',
            'argv': ['/usr/bin/python3', '-c', 'import time; time.sleep(.2)'],
            'cwd': str(self.root), 'private_home': str(self.root),
            'environment': {}, 'artifacts': {'stdout': str(self.root/'out'), 'stderr': str(self.root/'err')},
            'stop_file': str(self.root/'stop'), 'stop_files': [],
            'grace_ms': 100, 'timeout_ms': 1000, 'max_drain_ms': 500,
            'possession': True,
        }
        path = self.root/'request.json'
        path.write_text(json.dumps(request))
        with subprocess.Popen(['/usr/bin/python3', str(SUPERVISOR), str(path)],
                              stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as monitor:
            header = json.loads(monitor.stdout.readline(4097))
            self.assertEqual(header['schema'], 'kioku/ci2/native-possession/v1')
            self.assertEqual(header['monitor_pid'], monitor.pid)
            evidence = header['evidence']
            pid = evidence['process']['pid']
            self.assertEqual(evidence['process']['pgid'], pid)
            self.assertEqual(evidence['boot_id'], Path('/proc/sys/kernel/random/boot_id').read_text().strip())
            fields = Path('/proc', str(pid), 'stat').read_bytes().rsplit(b') ', 1)[1].split()
            self.assertEqual(int(fields[1]), monitor.pid)
            self.assertEqual(int(fields[19]), evidence['start_ticks'])
            monitor.stdin.write(b'continue\n')
            monitor.stdin.flush()
            output, errors = monitor.communicate(timeout=5)
            self.assertEqual(monitor.returncode, 0, errors)
            result = json.loads(output)
            self.assertTrue(result['descendants_drained'])
            self.assertTrue(result['cpu_complete'])

    def test_possession_checkpoint_survives_lost_controller_pipe(self):
        request = {
            'schema': 'kioku/ci2/native-supervisor-request/v1',
            'argv': ['/usr/bin/python3', '-c', 'import time; time.sleep(.1)'],
            'cwd': str(self.root), 'private_home': str(self.root),
            'environment': {}, 'artifacts': {'stdout': str(self.root/'out'), 'stderr': str(self.root/'err')},
            'stop_file': str(self.root/'stop'), 'stop_files': [],
            'grace_ms': 100, 'timeout_ms': 1000, 'max_drain_ms': 500,
            'possession': True,
        }
        path = self.root/'request.json'
        path.write_text(json.dumps(request))
        with subprocess.Popen(['/usr/bin/python3', str(SUPERVISOR), str(path)],
                              stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as monitor:
            header = json.loads(monitor.stdout.readline(4097))
            monitor.stdin.write(b'continue\n')
            monitor.stdin.close()
            monitor.stdout.close()
            monitor.wait(timeout=5)
        checkpoint = self.root/'possession-closure.json'
        self.assertTrue(checkpoint.exists(), 'lost stdout must not lose native closure')
        self.assertEqual(checkpoint.stat().st_mode & 0o777, 0o600)
        record = json.loads(checkpoint.read_bytes())
        self.assertEqual(record['header'], header)
        self.assertTrue(record['no_children'])
        self.assertTrue(record['original_group_empty'])
        self.assertTrue(record['result']['descendants_drained'])
        self.assertIsNone(record['result']['error'])

    def test_timeout_does_not_become_success(self):
        result = self.run_monitor(['/usr/bin/python3', '-c', 'import time; time.sleep(30)'], 300)
        self.assertTrue(result['timed_out'])
        self.assertIsNone(result['exit_code'])
        self.assertEqual(result['returncode'], -signal.SIGTERM)
        self.assertEqual(result['signal'], signal.SIGTERM)

if __name__ == '__main__':
    unittest.main()
