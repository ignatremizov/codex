import hashlib
import importlib.util
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

helper_path = Path(__file__).resolve().parents[2] / "third_party/v8/prebuilt_binding.py"
helper_spec = importlib.util.spec_from_file_location("v8_prebuilt_binding", helper_path)
assert helper_spec is not None and helper_spec.loader is not None
compat = importlib.util.module_from_spec(helper_spec)
helper_spec.loader.exec_module(compat)


class PrebuiltBindingCompatibilityTest(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.archive = self.root / "native.a.gz"
        self.binding = self.root / "binding.rs"
        self.archive_bytes = b"reviewed native archive with a 64-byte threshold"
        self.raw = (
            b"pub const OTHER_ABI_SIZE: usize = 123;\n"
            + compat.RAW_CONSTANT
            + b"\npub type UnchangedAbi = u64;\n"
        )
        self.corrected = self.raw.replace(
            compat.RAW_CONSTANT, compat.CORRECTED_CONSTANT
        )
        self.archive.write_bytes(self.archive_bytes)
        self.binding.write_bytes(self.raw)
        # Compact stand-ins exercise pair authentication without embedding
        # a 30 MB native archive in this Python fixture.
        for name, data in (
            ("ARCHIVE_SHA256", self.archive_bytes),
            ("BINDING_SHA256", self.raw),
            ("CORRECTED_SHA256", self.corrected),
        ):
            override = patch.object(compat, name, hashlib.sha256(data).hexdigest())
            override.start()
            self.addCleanup(override.stop)

    def prepare(self, **changes) -> Path:
        arguments = {
            "version": compat.VERSION,
            "target": compat.TARGET,
            "profile": compat.PROFILE,
            "archive": self.archive,
            "binding": self.binding,
        }
        arguments.update(changes)
        return compat.prepare_binding(**arguments)

    def test_corrects_only_the_constant_and_retains_both_raw_inputs(self) -> None:
        output = self.prepare()
        self.assertNotEqual(output, self.binding)
        self.assertEqual(output.read_bytes(), self.corrected)
        self.assertEqual(self.archive.read_bytes(), self.archive_bytes)
        self.assertEqual(self.binding.read_bytes(), self.raw)

    def test_requires_both_reviewed_raw_artifacts(self) -> None:
        for path in (self.archive, self.binding):
            with self.subTest(path=path):
                original = path.read_bytes()
                path.write_bytes(b"another artifact")
                with self.assertRaisesRegex(
                    RuntimeError, "reviewed compatibility pair"
                ):
                    self.prepare()
                path.write_bytes(original)
                self.assertFalse(list(self.root.glob("*.typed-array-64.rs")))

    def test_other_targets_versions_and_profiles_do_not_read_inputs(self) -> None:
        self.archive.unlink()
        self.binding.unlink()
        for changes in (
            {"version": "152.2.1"},
            {"target": "x86_64-unknown-linux-musl"},
            {"target": "aarch64-unknown-linux-gnu"},
            {"target": "x86_64-pc-windows-msvc"},
            {"profile": "release"},
        ):
            with self.subTest(changes=changes):
                self.assertEqual(self.prepare(**changes), self.binding)
        self.assertEqual(list(self.root.iterdir()), [])

    def test_rejects_a_derived_output_with_the_wrong_expected_digest(self) -> None:
        with patch.object(compat, "CORRECTED_SHA256", "0" * 64):
            with self.assertRaisesRegex(RuntimeError, "reviewed SHA-256"):
                self.prepare()
        self.assertFalse(list(self.root.glob("*.typed-array-64.rs")))

    def test_output_cannot_replace_a_raw_input(self) -> None:
        for output in (self.archive, self.binding):
            with self.subTest(output=output):
                with self.assertRaisesRegex(RuntimeError, "either raw input"):
                    self.prepare(output=output)
        self.assertEqual(self.archive.read_bytes(), self.archive_bytes)
        self.assertEqual(self.binding.read_bytes(), self.raw)

    def test_corrupt_derived_cache_is_repaired_from_verified_raw_inputs(self) -> None:
        output = self.prepare()
        output.write_bytes(b"corrupt derived cache")
        self.assertEqual(self.prepare(), output)
        self.assertEqual(output.read_bytes(), self.corrected)
        self.assertEqual(self.binding.read_bytes(), self.raw)
        self.assertEqual(self.archive.read_bytes(), self.archive_bytes)


if __name__ == "__main__":
    unittest.main()
