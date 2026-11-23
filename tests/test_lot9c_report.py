"""Fast arithmetic tests; no build, subprocess benchmark, or measurement."""
import importlib.util
from pathlib import Path
import unittest
import json
import os
import tempfile
import sys
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from lot9c_measure import measure
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("lot9c_bench", Path(__file__).resolve().parents[1] / "scripts/lot9c_bench.py")
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)


class Ratios(unittest.TestCase):
    def test_paired_medians(self):
        trials = []
        for value in (0.5, 0.9, 0.7):
            trials.extend([dict(mode="baseline", wall=10, cpu=20, disk=100),
                           dict(mode="cache", wall=10 * value, cpu=20 * value, disk=100 * value)])
        pairs, medians = bench.ratios(trials)
        self.assertEqual(len(pairs), 3)
        for metric in ("wall", "cpu", "disk"):
            self.assertAlmostEqual(medians[metric], 0.7)

    def test_missing_zero_nan_and_wrong_order_rejected(self):
        baseline = dict(mode="baseline", wall=10, cpu=20, disk=100)
        cache = dict(mode="cache", wall=5, cpu=10, disk=50)
        for trials in ([baseline, cache] * 2, [cache, baseline] * 3,
                       [dict(baseline, wall=0), cache] * 3,
                       [baseline, dict(cache, cpu=float("nan"))] * 3):
            with self.assertRaises(RuntimeError):
                bench.ratios(trials)


class RootPaths(unittest.TestCase):
    def test_repo_is_the_candidate_worktree_that_holds_this_script(self):
        script = Path(bench.__file__).resolve()
        self.assertEqual(bench.REPO, script.parents[1])
        self.assertTrue(bench.REPO.is_absolute())
        self.assertEqual(bench.REPO, Path(os.path.realpath(str(bench.REPO))),
                         "REPO must not be a partial or symlinked alias")
        self.assertEqual(script, bench.REPO / "scripts" / "lot9c_bench.py")
        self.assertTrue((bench.REPO / "Cargo.toml").is_file())
        self.assertTrue((bench.REPO / "tests" / "select_cache_lot9c_bench.rs").is_file())


class Reporting(unittest.TestCase):
    def test_error_report_is_exclusive_and_indeterminate(self):
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            lock = directory / "lock"
            lock.touch()
            report = directory / "report.json"
            with patch.object(bench, "LOCK", lock), patch.dict(os.environ, {"KIOKU_LOT9C_REPORT": str(report)}), patch.object(bench, "campaign", side_effect=RuntimeError("missing tool")):
                self.assertEqual(bench.main(), 1)
                actual = json.loads(report.read_text())
                self.assertEqual(actual["status"], "INDETERMINE")
                self.assertTrue(actual["run_id"])
                self.assertTrue(actual["generated_start_utc"].endswith("+00:00"))
                self.assertIn("missing tool", actual["error"])
                with patch("sys.stderr"):
                    self.assertEqual(bench.main(), 2)
                self.assertEqual(actual, json.loads(report.read_text()))

    def test_missing_wait4_stops_before_build_or_trial(self):
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            lock = directory / "lock"
            lock.touch()
            report = directory / "report.json"
            with patch.object(bench, "LOCK", lock), patch.dict(os.environ, {"KIOKU_LOT9C_REPORT": str(report)}), patch.object(bench.os, "wait4", create=True) as wait4, patch.object(bench, "command") as command, patch.object(bench, "run_trial") as trial:
                del bench.os.wait4
                self.assertEqual(bench.main(), 1)
                actual = json.loads(report.read_text())
                self.assertEqual(actual["status"], "INDETERMINE")
                self.assertIn("wait4", actual["error"])
                command.assert_not_called()
                trial.assert_not_called()


class NativeMeasurement(unittest.TestCase):
    def test_fresh_children_cpu_for_each_trial_includes_joined_descendants(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            program = ("import subprocess,sys; "
                       "p=subprocess.Popen([sys.executable,'-c',"
                       "'import time; end=time.process_time()+0.08; "
                       "exec(\"while time.process_time() < end: pass\")']); p.wait()")
            results = [measure([sys.executable, "-c", program], os.environ.copy(),
                               root / f"out{i}", root / f"err{i}", lambda: None)
                       for i in range(2)]
            for result in results:
                self.assertEqual(result["exit_code"], 0)
                cpu = result["children_user"] + result["children_sys"]
                self.assertGreater(cpu, 0.05)
                self.assertAlmostEqual(cpu, result["user"] + result["sys"], delta=0.02)
            self.assertLess(results[1]["children_user"] + results[1]["children_sys"],
                            1.8 * (results[0]["children_user"] + results[0]["children_sys"]))

    def test_logs_exit_and_direct_usage(self):
        with tempfile.TemporaryDirectory() as temp:
            out, err = Path(temp) / "out", Path(temp) / "err"
            argv = [sys.executable, "-c", "import sys; print('out'); print('err', file=sys.stderr)"]
            result = measure(argv, os.environ.copy(), out, err, lambda: None)
            self.assertEqual(result["argv"], argv)
            self.assertEqual(result["exit_code"], 0)
            self.assertIsNone(result["interrupted"])
            self.assertGreater(result["wall"], 0)
            self.assertGreaterEqual(result["user"] + result["sys"], 0)
            self.assertEqual(out.read_text(), "out\n")
            self.assertEqual(err.read_text(), "err\n")
            with self.assertRaises(FileExistsError):
                measure(argv, os.environ.copy(), out, err, lambda: None)

    def test_exec_failure_and_timeout(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            result = measure([str(root / "missing")], os.environ.copy(), root / "out", root / "err", lambda: None)
            self.assertEqual(result["exit_code"], 127)
            self.assertIn("exec failed", (root / "err").read_text())
            result = measure([sys.executable, "-c", "import time; time.sleep(20)"], os.environ.copy(), root / "out2", root / "err2", lambda: None, timeout=0.1)
            self.assertEqual(result["interrupted"], "trial timeout")
            self.assertNotEqual(result["exit_code"], 0)

    def test_monitor_interruption_is_preserved(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            def stop():
                raise RuntimeError("operator stop")
            result = measure([sys.executable, "-c", "import time; time.sleep(.05)"], os.environ.copy(), root / "out", root / "err", stop)
            self.assertEqual(result["interrupted"], "operator stop")


if __name__ == "__main__":
    unittest.main()
