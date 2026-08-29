#!/usr/bin/python3

import importlib.machinery
import importlib.util
import contextlib
import io
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "bin/hyprcorrect-companion"
loader = importlib.machinery.SourceFileLoader("hyprcorrect_companion", str(SCRIPT))
spec = importlib.util.spec_from_loader(loader.name, loader)
companion = importlib.util.module_from_spec(spec)
loader.exec_module(companion)


class CompanionPathTests(unittest.TestCase):
    def runtime(self, root):
        runtime = Path(root, "runtime")
        runtime.mkdir(mode=0o700)
        return runtime

    def test_pid_symlink_is_refused(self):
        with tempfile.TemporaryDirectory() as root:
            runtime = self.runtime(root)
            target = Path(root, "target")
            target.write_text("1")
            (runtime / "hyprcorrect.pid").symlink_to(target)
            with mock.patch.dict(os.environ, {"XDG_RUNTIME_DIR": str(runtime)}):
                with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                    companion.running_binary()

    def test_pid_fifo_is_refused_without_blocking(self):
        with tempfile.TemporaryDirectory() as root:
            runtime = self.runtime(root)
            os.mkfifo(runtime / "hyprcorrect.pid", 0o600)
            with mock.patch.dict(os.environ, {"XDG_RUNTIME_DIR": str(runtime)}):
                with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                    companion.running_binary()

    def test_oversized_pid_is_refused(self):
        with tempfile.TemporaryDirectory() as root:
            runtime = self.runtime(root)
            (runtime / "hyprcorrect.pid").write_bytes(b"1" * (companion.MAX_PID_BYTES + 1))
            with mock.patch.dict(os.environ, {"XDG_RUNTIME_DIR": str(runtime)}):
                with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                    companion.running_binary()

    def test_pid_exactly_at_the_limit_is_read_without_overflow(self):
        with tempfile.TemporaryDirectory() as root:
            runtime = self.runtime(root)
            (runtime / "hyprcorrect.pid").write_bytes(b"9" * companion.MAX_PID_BYTES)
            (runtime / "hyprcorrect.pid").chmod(0o600)
            with mock.patch.dict(os.environ, {"XDG_RUNTIME_DIR": str(runtime)}):
                self.assertIsNone(companion.running_binary())

    def test_pid_and_runtime_permissions_must_be_private(self):
        with tempfile.TemporaryDirectory() as root:
            runtime = self.runtime(root)
            pid = runtime / "hyprcorrect.pid"
            pid.write_text("1")
            pid.chmod(0o644)
            with mock.patch.dict(os.environ, {"XDG_RUNTIME_DIR": str(runtime)}):
                with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                    companion.running_binary()

            runtime.chmod(0o755)
            with mock.patch.dict(os.environ, {"XDG_RUNTIME_DIR": str(runtime)}):
                with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                    companion.runtime_directory()

if __name__ == "__main__":
    unittest.main()
