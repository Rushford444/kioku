"""Focused release-profile tests for scripts/lot9c_bench.py.

No Cargo, no compilation, no benchmark: the build command and its JSON
artifacts are faked, and every trial is a stub. Only the profile selection,
the artifact path/optimization checks and the report path are exercised.
"""
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[1] / "scripts"
sys.path.insert(0, str(SCRIPTS))

spec = importlib.util.spec_from_file_location("lot9c_bench_release", SCRIPTS / "lot9c_bench.py")
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)

COMMIT = "20a716178558eec741d58d013fd7ee6ecf13d2fb"
DEBUG_BUILD_ARGV = ["cargo", "test", "--locked", "--offline", "--test",
                    "select_cache_lot9c_bench", "--no-run", "--message-format=json"]
RELEASE_BUILD_ARGV = ["cargo", "test", "--release", "--locked", "--offline", "--test",
                      "select_cache_lot9c_bench", "--no-run", "--message-format=json"]


MISSING = object()


def artifact_line(executable, opt_level):
    """One cargo JSON stream holding a single lot9c test artifact."""
    profile = {} if opt_level is MISSING else {"opt_level": opt_level}
    row = {"reason": "compiler-artifact", "target": {"name": "select_cache_lot9c_bench"},
           "executable": str(executable), "profile": profile}
    return json.dumps(row) + "\n" + json.dumps({"reason": "build-finished", "success": True}) + "\n"


class FakeCommand:
    """Stands in for bench.command; records argv and answers the fixed queries."""

    def __init__(self, build_stdout):
        self.build_stdout = build_stdout
        self.calls = []

    def __call__(self, args, **kwargs):
        args = list(args)
        self.calls.append(args)
        if args[:2] == ["git", "rev-parse"]:
            return SimpleNamespace(stdout=COMMIT + "\n")
        if args[:2] == ["git", "status"]:
            return SimpleNamespace(stdout="")
        if args[:2] == ["rustc", "-vV"]:
            return SimpleNamespace(stdout="rustc 1.89.0\n")
        if args[:2] == ["cargo", "test"] and "--no-run" in args:
            return SimpleNamespace(stdout=self.build_stdout)
        return SimpleNamespace(stdout="")

    def build_argv(self):
        return [call for call in self.calls if call[:2] == ["cargo", "test"] and "--no-run" in call]


class ReleaseProfile(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.dir = Path(self.temp.name)
        self.roots = []
        for index in range(6):
            root = self.dir / f"trial{index}"
            root.mkdir()
            self.roots.append(root)
        self.report = self.dir / "report.json"
        self.lock = self.dir / "lock"
        self.lock.touch()
        self.fake = None

    def tearDown(self):
        self.temp.cleanup()

    def binary(self, profile, name="select_cache_lot9c_bench-abc123"):
        path = self.dir / profile / "deps" / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(b"fake test binary")
        return path

    def environment(self, profile):
        env = {"KIOKU_LOT9C_TRIALS": json.dumps([str(root) for root in self.roots]),
               "KIOKU_LOT9C_MAX_BYTES": str(64 * 1024**2),
               "KIOKU_LOT9C_ROOT_APPROVED": "1",
               "KIOKU_LOT9C_STABLE_HOST": "1",
               "KIOKU_TEST_OPERATOR_STOP_FILE": ""}
        if profile is not None:
            env["KIOKU_LOT9C_PROFILE"] = profile
        return env

    def patches(self, profile, build_stdout):
        self.fake = FakeCommand(build_stdout)
        return [patch.dict(os.environ, self.environment(profile)),
                patch.object(bench, "command", self.fake),
                patch.object(bench, "tool_version", return_value="du (GNU coreutils) 9.1"),
                patch.object(bench, "mounts", return_value=[]),
                patch.object(bench, "check_space"),
                patch.object(bench, "run_trial", side_effect=self.trial),
                patch.object(bench.os, "chdir"),
                patch.object(bench.subprocess, "run",
                             side_effect=AssertionError("no real subprocess allowed"))]

    def trial(self, binary, root, mode, report_path, index, paths, stop, budget):
        return {"mode": mode, "wall": 1.0, "cpu": 1.0, "disk": 1.0}

    def campaign(self, profile, build_stdout):
        result = {"trials": []}
        patches = self.patches(profile, build_stdout)
        for item in patches:
            item.start()
        try:
            bench.campaign(result, self.report)
        finally:
            for item in reversed(patches):
                item.stop()
        return result

    def main(self, profile, build_stdout):
        patches = self.patches(profile, build_stdout)
        patches.append(patch.object(bench, "LOCK", self.lock))
        patches.append(patch.dict(os.environ, {"KIOKU_LOT9C_REPORT": str(self.report)}))
        for item in patches:
            item.start()
        try:
            return bench.main()
        finally:
            for item in reversed(patches):
                item.stop()

    def test_release_profile_builds_release_argv_and_accepts_release_artifact(self):
        binary = self.binary("release")
        result = self.campaign("release", artifact_line(binary, "3"))
        self.assertEqual(self.fake.build_argv(), [RELEASE_BUILD_ARGV])
        self.assertEqual(result["build"]["profile"], "release")
        self.assertEqual(result["build"]["artifact_profile"], {"opt_level": "3"})
        self.assertEqual(result["binary_path"], str(binary))
        self.assertIn(["cargo", "test", "--release", "--locked", "--offline",
                       "--test", "select_cache_lot9b"], self.fake.calls)
        self.assertEqual(result["status"], "NO-GO")

    def test_debug_default_keeps_the_historical_argv(self):
        binary = self.binary("debug")
        result = self.campaign(None, artifact_line(binary, "0"))
        self.assertEqual(self.fake.build_argv(), [DEBUG_BUILD_ARGV])
        self.assertEqual(result["build"]["profile"], "debug")
        self.assertIn(["cargo", "test", "--locked", "--offline",
                       "--test", "select_cache_lot9b"], self.fake.calls)

    def test_unknown_profile_is_blocked_instead_of_falling_back_to_debug(self):
        for profile in ("fast", "", "Release", "release "):
            with self.subTest(profile=profile):
                with self.assertRaisesRegex(RuntimeError, "profile must be debug or release"):
                    self.campaign(profile, artifact_line(self.binary("debug"), "0"))
                self.assertEqual(self.fake.build_argv(), [])

    def test_artifact_outside_the_declared_profile_directory_is_rejected(self):
        with self.assertRaisesRegex(RuntimeError, "artifact executable profile mismatch"):
            self.campaign("release", artifact_line(self.dir / "release-other" / "deps" / "foo", "3"))

    def test_artifact_with_parent_segments_into_another_profile_is_rejected(self):
        forged = self.dir / "release-other" / ".." / "debug" / "deps" / "foo"
        with self.assertRaisesRegex(RuntimeError, "artifact executable profile mismatch"):
            self.campaign("release", artifact_line(forged, "3"))

    def test_debug_profile_rejects_a_release_artifact(self):
        with self.assertRaisesRegex(RuntimeError, "artifact executable profile mismatch"):
            self.campaign(None, artifact_line(self.binary("release"), "0"))

    def test_release_profile_rejects_a_debug_optimization_level(self):
        with self.assertRaisesRegex(RuntimeError, "artifact optimization profile mismatch"):
            self.campaign("release", artifact_line(self.binary("release"), "0"))

    def test_missing_opt_level_is_denied_by_check_not_by_keyerror(self):
        """A cargo artifact without profile.opt_level is explicitly denied."""
        with self.assertRaisesRegex(RuntimeError, "artifact optimization profile mismatch"):
            self.campaign("release", artifact_line(self.binary("release"), MISSING))

    def test_parent_segments_that_resolve_to_the_declared_profile_are_accepted(self):
        # 'debug/../release/deps/foo' resolves to release/deps/foo: the names
        # still describe the declared profile, so this is not a forgery.
        self.binary("release", "foo")
        (self.dir / "debug").mkdir()
        forged = self.dir / "debug" / ".." / "release" / "deps" / "foo"
        result = self.campaign("release", artifact_line(forged, "3"))
        self.assertEqual(result["build"]["profile"], "release")

    def test_malformed_cargo_stdout_is_reported_as_indeterminate(self):
        self.assertEqual(self.main("release", "not json\n"), 1)
        report = json.loads(self.report.read_text())
        self.assertEqual(report["status"], "INDETERMINE")
        self.assertIn("JSONDecodeError", report["error"])
        self.assertNotIn("Traceback", report["error"])


if __name__ == "__main__":
    unittest.main()
