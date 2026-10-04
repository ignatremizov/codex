#!/usr/bin/env python3
"""Stamp or verify an exact-source macOS release component before artifact reuse."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tomllib


COMPONENTS = {
    "application": ["codex", "codex-app-server", "codex-responses-api-proxy"],
    "cli": ["codex"],
    "services": ["codex-app-server", "codex-responses-api-proxy"],
    "code-mode": ["codex-code-mode-host"],
}


def sha256(path: Path) -> str:
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def describe(directory: Path, component: str) -> dict:
    root = Path(__file__).resolve().parents[2]
    commit = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=root, text=True
    ).strip()
    if commit != os.environ["GITHUB_SHA"]:
        raise ValueError("checkout does not match the requested release commit")
    workspace = tomllib.loads((root / "codex-rs/Cargo.toml").read_text())
    files = {}
    for name in COMPONENTS[component]:
        path = directory / name
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"missing regular executable: {path}")
        files[name] = {"sha256": sha256(path), "size": path.stat().st_size}
    return {
        "schema": 1,
        "source_commit": commit,
        "repository": os.environ["GITHUB_REPOSITORY"],
        "target": "x86_64-apple-darwin",
        "component": component,
        "version": workspace["workspace"]["package"]["version"],
        "release_profile": workspace["profile"]["release"],
        "toolchain_sha256": sha256(root / "codex-rs/rust-toolchain.toml"),
        "lockfile_sha256": sha256(root / "codex-rs/Cargo.lock"),
        "files": files,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=["write", "verify"])
    parser.add_argument("--component", choices=COMPONENTS, required=True)
    parser.add_argument("--directory", type=Path, required=True)
    args = parser.parse_args()
    manifest = args.directory / "release-manifest.json"
    expected = describe(args.directory, args.component)
    if args.operation == "write":
        manifest.write_text(json.dumps(expected, indent=2, sort_keys=True) + "\n")
    elif json.loads(manifest.read_text()) != expected:
        raise ValueError(f"component provenance or checksums do not match: {manifest}")
    print(f"{args.operation}: {args.component} at {expected['source_commit']}")


if __name__ == "__main__":
    main()
