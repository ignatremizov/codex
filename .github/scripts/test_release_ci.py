"""Fast CI-only tests for release provenance and build monitoring helpers."""

import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


SCRIPTS = Path(__file__).resolve().parent


def load_script(name: str):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS / f"{name}.py")
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {name}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


component = load_script("release-component")
smoke = load_script("smoke-auth-file")


class ReleaseProvenanceTests(unittest.TestCase):
    def test_binary_changes_invalidate_component_fingerprint(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            for name in component.COMPONENTS["application"]:
                (directory / name).write_bytes(b"initial executable fixture")
            original = component.describe(directory, "application")
            (directory / "codex").write_bytes(b"modified executable fixture")
            changed = component.describe(directory, "application")
            self.assertNotEqual(original["files"]["codex"], changed["files"]["codex"])
            self.assertEqual(
                original["files"]["codex-app-server"],
                changed["files"]["codex-app-server"],
            )

    def test_symlink_cannot_substitute_for_a_release_binary(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            target = directory / "elsewhere"
            target.write_bytes(b"fixture")
            (directory / "codex-code-mode-host").symlink_to(target)
            with self.assertRaisesRegex(ValueError, "regular executable"):
                component.describe(directory, "code-mode")

    def test_different_checkout_is_rejected_before_reading_binaries(self):
        with mock.patch.dict(os.environ, {"GITHUB_SHA": "not-the-checkout"}):
            with self.assertRaisesRegex(ValueError, "checkout does not match"):
                component.describe(Path("missing-directory"), "application")


class ReleaseMonitorTests(unittest.TestCase):
    def test_child_failure_and_output_are_preserved(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            env = {
                **os.environ,
                "RELEASE_DIAGNOSTICS_DIR": str(root / "diagnostics"),
                "CARGO_TARGET_DIR": str(root),
            }
            result = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPTS / "run-release-build.py"),
                    "--phase",
                    "failure-propagation",
                    "--",
                    sys.executable,
                    "-c",
                    "print('build fixture output'); raise SystemExit(7)",
                ],
                env=env,
                capture_output=True,
                text=True,
                timeout=100,
                check=False,
            )
            self.assertEqual(result.returncode, 7, result.stdout + result.stderr)
            log = root / "diagnostics/failure-propagation.log"
            self.assertIn("build fixture output", log.read_text())

    def test_auth_rejection_must_have_a_failure_exit_status(self):
        result = subprocess.CompletedProcess([], 0, "Not logged in", "")
        with mock.patch.object(smoke.subprocess, "run", return_value=result):
            with self.assertRaisesRegex(RuntimeError, "expected rejection"):
                smoke.expect_rejected(["codex"], {}, Path("."), "Not logged in")


if __name__ == "__main__":
    unittest.main()
