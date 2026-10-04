"""Correct the one proven Codex prebuilt V8 TypedArray binding mismatch.

Raw release inputs remain immutable. Only the exact 152.2.0 sandbox pair
for x86_64 GNU/Linux is eligible; its native archive uses a 64-byte inline
threshold while the published binding incorrectly declares zero.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import tempfile
from pathlib import Path

VERSION = "152.2.0"
TARGET = "x86_64-unknown-linux-gnu"
PROFILE = "ptrcomp_sandbox_release"
ARCHIVE_SHA256 = "b3b490f2aa809eda7f6ac727f851fb00ce8355be72827577cfaac28c7137dbcd"
BINDING_SHA256 = "3e4d5fecdbe0f98a6017e69d7ca82519a77fb032c5eb5e4cdbecdd3e2ec519d5"
CORRECTED_SHA256 = "a868b86ada12f86348bcb802384340a367a35e1843c86746f48ea34d23b7a79e"
RAW_CONSTANT = b"pub const v8__TYPED_ARRAY_MAX_SIZE_IN_HEAP: usize = 0;"
CORRECTED_CONSTANT = b"pub const v8__TYPED_ARRAY_MAX_SIZE_IN_HEAP: usize = 64;"


def file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def prepare_binding(
    *,
    version: str,
    target: str,
    profile: str,
    archive: Path,
    binding: Path,
    output: Path | None = None,
) -> Path:
    """Return a binding for a verified pair, preserving all other profiles."""
    if (version, target, profile) != (VERSION, TARGET, PROFILE):
        if output is not None:
            raise RuntimeError("No V8 binding correction exists for this profile.")
        return binding

    # Verify the raw pair again here, including for Bazel's direct inputs.
    # Never infer the native ABI from the binding or a target name alone.
    if file_sha256(archive) != ARCHIVE_SHA256:
        raise RuntimeError("V8 archive does not match the reviewed compatibility pair.")
    raw = binding.read_bytes()
    if hashlib.sha256(raw).hexdigest() != BINDING_SHA256:
        raise RuntimeError("V8 binding does not match the reviewed compatibility pair.")
    if raw.count(RAW_CONSTANT) != 1:
        raise RuntimeError("V8 binding has no unique reviewed TypedArray constant.")
    corrected = raw.replace(RAW_CONSTANT, CORRECTED_CONSTANT)
    if hashlib.sha256(corrected).hexdigest() != CORRECTED_SHA256:
        raise RuntimeError("Corrected V8 binding does not match its reviewed SHA-256.")

    output = output or binding.with_name(f"{binding.stem}.typed-array-64.rs")
    if output.resolve() in {archive.resolve(), binding.resolve()}:
        raise RuntimeError("Corrected V8 output must not replace either raw input.")
    if output.is_symlink():
        raise RuntimeError("Corrected V8 output must not be a symbolic link.")
    if output.is_file() and file_sha256(output) == CORRECTED_SHA256:
        return output

    output.parent.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(
            dir=output.parent, prefix=f".{output.name}.", delete=False
        ) as destination:
            temporary = Path(destination.name)
            destination.write(corrected)
        os.replace(temporary, output)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)
    return output


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--profile", required=True)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--binding", type=Path, required=True)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    try:
        output = prepare_binding(**vars(args))
    except (OSError, RuntimeError) as error:
        parser.error(str(error))
    print(output)


if __name__ == "__main__":
    main()
