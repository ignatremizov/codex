#!/usr/bin/env python3
"""Transfer completed release libraries between identical, same-source CI runners."""

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import platform
import posixpath
import subprocess
import tarfile


ARCHIVE = "application-libraries.tar.gz"
MANIFEST = "application-libraries.json"
ROOTS = (".cache/codex-ci/cargo-target", ".cargo/registry", ".cargo/git")
PACKAGES = ("codex-cli", "codex-app-server", "codex-responses-api-proxy")


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def command_output(*command: str) -> str:
    return subprocess.check_output(command, text=True).strip()


def build_identity(root: Path) -> dict:
    if os.environ.get("GITHUB_ACTIONS") != "true":
        raise ValueError("release checkpoints may only run on GitHub Actions")
    source = command_output("git", "-C", str(root), "rev-parse", "HEAD")
    if source != os.environ["GITHUB_SHA"]:
        raise ValueError("checkout does not match the requested release commit")
    subprocess.run(["git", "-C", str(root), "diff", "--exit-code", "HEAD"], check=True)
    home = Path.home().resolve()
    target = home / ROOTS[0]
    if Path(os.environ["CARGO_TARGET_DIR"]).resolve() != target:
        raise ValueError("unexpected Cargo target path for release checkpoint")
    if Path(os.environ["CARGO_HOME"]).resolve() != home / ".cargo":
        raise ValueError("unexpected Cargo home for release checkpoint")
    if platform.system() != "Darwin" or platform.machine() != "x86_64":
        raise ValueError("this checkpoint requires native Intel macOS")
    tracked_inputs = (
        "codex-rs/Cargo.toml",
        "codex-rs/Cargo.lock",
        "codex-rs/rust-toolchain.toml",
        "codex-rs/.cargo/config.toml",
        ".github/workflows/manual-release-build.yml",
    )
    return {
        "schema": 2,
        "repository": os.environ["GITHUB_REPOSITORY"],
        "source_commit": source,
        "workspace": str(root),
        "home": str(home),
        "target_directory": str(target),
        "target": "x86_64-apple-darwin",
        "profile": "release",
        "packages": list(PACKAGES),
        "inputs": {name: digest(root / name) for name in tracked_inputs},
        "rustc": command_output("rustc", "--version", "--verbose"),
        "xcode": command_output("xcodebuild", "-version"),
        "sdk": command_output("xcrun", "--show-sdk-path"),
        "sdk_version": command_output("xcrun", "--show-sdk-version"),
        "image": os.environ.get("ImageVersion", ""),
        "build_environment": {
            name: value
            for name, value in sorted(os.environ.items())
            if name
            in {
                "CARGO_INCREMENTAL",
                "CARGO_ENCODED_RUSTFLAGS",
                "RUSTFLAGS",
                "MACOSX_DEPLOYMENT_TARGET",
                "STABLE_GIT_COMMIT",
            }
            or name.startswith("CARGO_PROFILE_RELEASE_")
        },
    }


def workspace_stamps(root: Path) -> dict:
    paths = subprocess.check_output(["git", "-C", str(root), "ls-files", "-z"])
    stamps = {}
    directories = set()
    for raw_name in paths.split(b"\0"):
        if not raw_name:
            continue
        name = os.fsdecode(raw_name)
        path = root / name
        # No source contents are archived. Hashes prevent timestamp restoration
        # from convincing Cargo that a changed source file is still fresh.
        if path.is_symlink():
            stamps[name] = {"link": os.readlink(path)}
        elif path.is_file():
            stamps[name] = {"sha256": digest(path), "mtime_ns": path.stat().st_mtime_ns}
        else:
            raise ValueError(f"unsupported tracked source: {name}")
        for parent in Path(name).parents:
            if parent != Path("."):
                directories.add(parent.as_posix())
    # Cargo also fingerprints directories named by rerun-if-changed. A fresh
    # checkout gives those directories newer mtimes even when every file is
    # identical, invalidating the restored build-script outputs and dependents.
    for name in sorted(directories):
        path = root / name
        if path.is_symlink() or not path.is_dir():
            raise ValueError(f"unsupported source directory: {name}")
        stamps[name] = {
            "entries": sorted(child.name for child in path.iterdir()),
            "mtime_ns": path.stat().st_mtime_ns,
        }
    return stamps


def restore_stamps(root: Path, saved: dict) -> None:
    current = workspace_stamps(root)
    if current.keys() != saved.keys():
        raise ValueError("checkpoint source inventory differs from checkout")
    # Validate every file and directory inventory before modifying even one
    # timestamp. In particular, do not hide newly added untracked inputs from
    # Cargo's directory-based change detection.
    for name, actual in current.items():
        expected = saved[name]
        for key in ("sha256", "link", "entries"):
            if actual.get(key) != expected.get(key):
                raise ValueError(f"checkpoint source differs: {name}")
    for name, stamp in saved.items():
        if "mtime_ns" in stamp:
            path = root / name
            os.utime(path, ns=(path.stat().st_atime_ns, stamp["mtime_ns"]))


def validate_member(member: tarfile.TarInfo) -> None:
    name = PurePosixPath(member.name)
    if name.is_absolute() or ".." in name.parts:
        raise ValueError(f"unsafe checkpoint member: {member.name}")
    if not any(
        name == PurePosixPath(prefix) or name.is_relative_to(prefix) for prefix in ROOTS
    ):
        raise ValueError(f"unexpected checkpoint member: {member.name}")
    if not (member.isfile() or member.isdir() or member.issym() or member.islnk()):
        raise ValueError(f"unsupported checkpoint member: {member.name}")
    if member.issym() or member.islnk():
        linked = PurePosixPath(member.linkname)
        if linked.is_absolute():
            raise ValueError(f"absolute checkpoint link: {member.name}")
        destination = name.parent / linked if member.issym() else linked
        destination = PurePosixPath(posixpath.normpath(str(destination)))
        if not any(
            destination == PurePosixPath(prefix) or destination.is_relative_to(prefix)
            for prefix in ROOTS
        ):
            raise ValueError(f"escaping checkpoint link: {member.name}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=["pack", "restore"])
    parser.add_argument("--directory", type=Path, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[2]
    identity = build_identity(root)
    archive = args.directory / ARCHIVE
    manifest = args.directory / MANIFEST
    if args.operation == "pack":
        args.directory.mkdir(parents=True, exist_ok=True)
        stamps = workspace_stamps(root)
        with tarfile.open(
            archive, "w:gz", compresslevel=1, dereference=False
        ) as output:
            for name in ROOTS:
                path = Path.home() / name
                if not path.is_dir() or path.is_symlink():
                    raise ValueError(f"missing regular checkpoint directory: {path}")
                output.add(path, arcname=name)
        payload = {
            "identity": identity,
            "source_stamps": stamps,
            "archive_sha256": digest(archive),
            "archive_size": archive.stat().st_size,
        }
        manifest.write_text(json.dumps(payload, sort_keys=True) + "\n")
        print(
            f"Checkpoint: {archive.stat().st_size} bytes at {identity['source_commit']}"
        )
    else:
        payload = json.loads(manifest.read_text())
        if payload["identity"] != identity:
            raise ValueError(
                "checkpoint source, toolchain, SDK, paths, or build inputs differ"
            )
        if (
            archive.stat().st_size != payload["archive_size"]
            or digest(archive) != payload["archive_sha256"]
        ):
            raise ValueError("checkpoint archive checksum or size differs")
        with tarfile.open(archive, "r:gz") as incoming:
            for member in incoming.getmembers():
                validate_member(member)
            # Python's data filter additionally rejects escaping link targets,
            # devices, and writes through symlinks outside the extraction root.
            incoming.extractall(Path.home(), filter="data")
        restore_stamps(root, payload["source_stamps"])
        print(f"Restored completed libraries from {identity['source_commit']}")


if __name__ == "__main__":
    main()
