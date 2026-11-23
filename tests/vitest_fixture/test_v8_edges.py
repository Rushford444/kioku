import copy
import json
import runpy
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
UNION = runpy.run_path(str(ROOT / "scripts/vitest_v8_edges.py"))["union"]
FIXTURE = ROOT / "tests/fixtures/vitest-lot4"


class PerTestUnion(unittest.TestCase):
    def setUp(self):
        self.test = {"file": "math.test.ts", "name": "adds"}
        self.inventory = [self.test]
        self.static = [{"test": self.test, "files": ["static-only.ts"]}]
        self.report = json.loads((FIXTURE / "coverage-v8-istanbul.json").read_text())["math.test.ts"]

    def test_zero_hit_files_add_never_remove_and_never_claim_runtime_authority(self):
        self.report["math.ts"]["s"]["0"] = 0
        result = UNION(FIXTURE, self.inventory, self.static, [{"test": self.test, "report": self.report}])
        self.assertEqual(result["authority"], "unverified-prototype")
        self.assertEqual(result["entries"][0]["files"], ["math.ts", "static-only.ts"])

    def test_unknown_identity_missing_or_invalid_observations_fail(self):
        wrong = copy.deepcopy(self.test)
        wrong["name"] = "other"
        for observations in ([], [{"test": wrong, "report": self.report}], [{"test": self.test, "report": {}}]):
            with self.subTest(observations=observations), self.assertRaises(ValueError):
                UNION(FIXTURE, self.inventory, self.static, observations)
