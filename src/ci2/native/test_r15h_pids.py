"""Synthetic proc-stat fixtures; no process lookup or signalling."""
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import supervisor


class ProcStatIdentity(unittest.TestCase):
    # Fields 3 through 22: state, PPID, PGID, then starttime at index 19.
    fields = b"S 17 42 " + b"0 " * 16 + b"12345"

    def read_identity(self, stat):
        with tempfile.TemporaryDirectory() as directory:
            fixture = Path(directory) / "stat"
            fixture.write_bytes(stat)
            with patch.object(supervisor, "open", return_value=fixture.open("rb"),
                              create=True) as read_stat:
                try:
                    return supervisor.identity(42)
                finally:
                    read_stat.assert_called_once_with("/proc/42/stat", "rb")

    def test_malformed_header_is_not_identity_evidence(self):
        for header in (b"42 worker) ", b"43 (worker) ", b"42 (worker)X"):
            with self.subTest(header=header):
                with self.assertRaises(ValueError):
                    self.read_identity(header + self.fields)

    def test_comm_parentheses_and_spaces_preserve_identity_fields(self):
        self.assertEqual(
            self.read_identity(b"42 (worker (nested) name) " + self.fields),
            (42, 12345, "S", 17),
        )


if __name__ == "__main__":
    unittest.main()
