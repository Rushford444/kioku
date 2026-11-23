#!/usr/bin/env python3
"""Fictional build fixture; execute the real local packaging script."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class FictionPackageChecksumTests(unittest.TestCase):
    def test_existing_checksum_and_failed_link_leave_no_new_archive(self):
        for failure in ("existing", "link"):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory(prefix="kioku-fiction-") as tmp:
                root = Path(tmp)
                output = root / "output"
                tools = root / "tools"
                binary = root / "target" / "x86_64-unknown-linux-gnu" / "release" / "kioku"
                output.mkdir()
                tools.mkdir()
                binary.parent.mkdir(parents=True)
                binary.write_text("#!/bin/sh\nexit 0\n")
                binary.chmod(0o755)
                cargo = tools / "cargo"
                cargo.write_text("#!/bin/sh\nexit 0\n")
                cargo.chmod(0o755)
                if failure == "link":
                    ln = tools / "ln"
                    ln.write_text('#!/bin/sh\ncase "$2" in *.sha256) exit 1;; esac\nexec /usr/bin/ln "$@"\n')
                    ln.chmod(0o755)
                archive = output / "kioku-0.3.1-linux-x86_64.tar.gz"
                checksum = Path(str(archive) + ".sha256")
                if failure == "existing":
                    checksum.write_text("prior evidence\n")
                env = os.environ.copy()
                env.update(PATH=f"{tools}:/usr/bin:/bin", CARGO_TARGET_DIR=str(root / "target"))
                result = subprocess.run(
                    [str(ROOT / "scripts/package.sh"), str(output), "x86_64-unknown-linux-gnu"],
                    cwd=ROOT, env=env, capture_output=True, text=True, check=False,
                )
                self.assertEqual(result.returncode, 3, result.stderr)
                self.assertFalse(archive.exists())
                if failure == "existing":
                    self.assertEqual(checksum.read_text(), "prior evidence\n")
                else:
                    self.assertFalse(checksum.exists())
                self.assertEqual(sorted(p.name for p in output.iterdir()), [checksum.name] if failure == "existing" else [])


if __name__ == "__main__":
    unittest.main()
