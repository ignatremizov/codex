"""Fast CI-only tests for release provenance and build monitoring helpers."""

import importlib.util
import io
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import tarfile
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
checkpoint = load_script("release-checkpoint")


@unittest.skipUnless(sys.platform == "linux", "macOS packaging uses GNU tar on Linux")
class VoiceArchiveTests(unittest.TestCase):
    def test_read_only_interleaved_directories_extract_with_permissions_intact(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "voice.tar.gz"
            output = root / "output"
            output.mkdir()
            with tarfile.open(archive, "w:gz") as outgoing:
                for name in ("runtime", "runtime/plugins", "runtime/lib"):
                    member = tarfile.TarInfo(name)
                    member.type = tarfile.DIRTYPE
                    member.mode = 0o555
                    outgoing.addfile(member)
                for name in (
                    "runtime/lib/library.dylib",
                    "runtime/plugins/plugin.dylib",
                ):
                    member = tarfile.TarInfo(name)
                    member.mode = 0o555
                    member.size = len(b"voice fixture")
                    outgoing.addfile(member, io.BytesIO(b"voice fixture"))
            try:
                result = subprocess.run(
                    [
                        "tar",
                        "--delay-directory-restore",
                        "-xzf",
                        str(archive),
                        "-C",
                        str(output),
                    ],
                    capture_output=True,
                    text=True,
                    check=False,
                    timeout=30,
                )
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                for name in (
                    "runtime/lib/library.dylib",
                    "runtime/plugins/plugin.dylib",
                ):
                    self.assertEqual((output / name).read_bytes(), b"voice fixture")
                for name in ("runtime", "runtime/plugins", "runtime/lib"):
                    self.assertEqual((output / name).stat().st_mode & 0o777, 0o555)
            finally:
                for path in output.rglob("*"):
                    if path.is_dir():
                        path.chmod(0o755)


class ReleaseCheckpointTests(unittest.TestCase):
    def test_modified_source_is_rejected_without_changing_timestamps(self):
        saved = {
            "good.rs": {"sha256": "same", "mtime_ns": 10},
            "changed.rs": {"sha256": "previous", "mtime_ns": 10},
        }
        current = {
            "good.rs": {"sha256": "same", "mtime_ns": 20},
            "changed.rs": {"sha256": "different", "mtime_ns": 20},
        }
        with (
            mock.patch.object(checkpoint, "workspace_stamps", return_value=current),
            mock.patch.object(checkpoint.os, "utime") as utime,
        ):
            with self.assertRaisesRegex(ValueError, "source differs"):
                checkpoint.restore_stamps(Path("unused"), saved)
            utime.assert_not_called()

    def test_unchanged_source_recovers_its_original_timestamp(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source.rs"
            source.write_text("same source")
            original = source.stat().st_mtime_ns - 60_000_000_000
            saved = {
                "source.rs": {"sha256": checkpoint.digest(source), "mtime_ns": original}
            }
            with mock.patch.object(checkpoint, "workspace_stamps", return_value=saved):
                checkpoint.restore_stamps(root, saved)
            self.assertEqual(source.stat().st_mtime_ns, original)

    def test_directory_inputs_recover_their_original_timestamps(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            names = ("src/grpc/api.proto", "src/assets/samples/example.md")
            for name in names:
                source = root / name
                source.parent.mkdir(parents=True, exist_ok=True)
                source.write_text("unchanged build input")
            tracked = "\0".join(names).encode() + b"\0"
            with mock.patch.object(
                checkpoint.subprocess, "check_output", return_value=tracked
            ):
                saved = checkpoint.workspace_stamps(root)
                self.assertEqual(
                    {name for name, stamp in saved.items() if "entries" in stamp},
                    {"src", "src/grpc", "src/assets", "src/assets/samples"},
                )
                for name, stamp in saved.items():
                    path = root / name
                    later = stamp["mtime_ns"] + 60_000_000_000
                    os.utime(path, ns=(path.stat().st_atime_ns, later))
                checkpoint.restore_stamps(root, saved)
                self.assertEqual(checkpoint.workspace_stamps(root), saved)

    def test_new_directory_input_is_rejected_without_changing_timestamps(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "src/grpc/api.proto"
            source.parent.mkdir(parents=True)
            source.write_text("tracked build input")
            with mock.patch.object(
                checkpoint.subprocess,
                "check_output",
                return_value=b"src/grpc/api.proto\0",
            ):
                saved = checkpoint.workspace_stamps(root)
                (source.parent / "extra.proto").write_text("new untracked input")
                with mock.patch.object(checkpoint.os, "utime") as utime:
                    with self.assertRaisesRegex(ValueError, "source differs"):
                        checkpoint.restore_stamps(root, saved)
                    utime.assert_not_called()

    def test_source_directory_cannot_be_replaced_by_a_symlink(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            elsewhere = root / "elsewhere"
            elsewhere.mkdir()
            (elsewhere / "api.proto").write_text("build input")
            (root / "src").symlink_to(elsewhere, target_is_directory=True)
            with mock.patch.object(
                checkpoint.subprocess, "check_output", return_value=b"src/api.proto\0"
            ):
                with self.assertRaisesRegex(ValueError, "unsupported source directory"):
                    checkpoint.workspace_stamps(root)

    def test_archive_cannot_write_outside_the_build_and_source_caches(self):
        for name in (
            "/etc/passwd",
            ".ssh/config",
            ".cargo/credentials.toml",
            ".cargo/git/../../.ssh/config",
        ):
            with self.subTest(name=name):
                with self.assertRaises(ValueError):
                    checkpoint.validate_member(tarfile.TarInfo(name))

    def test_archive_rejects_links_into_other_home_directories(self):
        member = tarfile.TarInfo(".cargo/git/redirect")
        member.type = tarfile.SYMTYPE
        member.linkname = "../../.ssh"
        with self.assertRaisesRegex(ValueError, "escaping checkpoint link"):
            checkpoint.validate_member(member)

    def test_archive_accepts_regular_outputs_and_internal_links(self):
        checkpoint.validate_member(
            tarfile.TarInfo(".cache/codex-ci/cargo-target/release/deps/lib.rlib")
        )
        member = tarfile.TarInfo(".cargo/git/checkouts/project/link")
        member.type = tarfile.SYMTYPE
        member.linkname = "source.rs"
        checkpoint.validate_member(member)


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
