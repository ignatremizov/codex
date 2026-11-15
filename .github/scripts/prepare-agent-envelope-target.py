#!/usr/bin/env python3
"""Install a recorded test-only envelope producer into the pinned upstream checkout."""

import argparse
import hashlib
import json
from pathlib import Path
import subprocess


TARGET_SHA = "fa266e73c414c1798f6ada3f85a151044e07f7c4"
MODULE = Path("codex-rs/core/src/tools/handlers/multi_agents_v2.rs")
FIXTURE = Path(
    "codex-rs/core/src/tools/handlers/multi_agents_v2/envelope_target_baseline_tests.rs"
)


def git(root: Path, *arguments: str) -> str:
    return subprocess.check_output(["git", *arguments], cwd=root, text=True).strip()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-root", required=True, type=Path)
    parser.add_argument("--evidence-dir", required=True, type=Path)
    parser.add_argument("--harness-sha", required=True)
    args = parser.parse_args()
    root = args.source_root.resolve(strict=True)
    harness = Path(__file__).resolve().parents[2]
    if git(root, "rev-parse", "HEAD") != TARGET_SHA:
        raise ValueError(
            "upstream envelope capture requires the exact pinned code parent"
        )
    if git(root, "status", "--porcelain=v1", "--untracked-files=no"):
        raise ValueError(
            "upstream tracked files must be pristine before the measurement overlay"
        )
    if git(harness, "rev-parse", "HEAD") != args.harness_sha:
        raise ValueError(
            "measurement harness revision does not match the dispatched workflow"
        )
    if git(harness, "status", "--porcelain=v1", "--untracked-files=no"):
        raise ValueError("measurement harness tracked files must be pristine")
    if (root / FIXTURE).exists():
        raise ValueError("measurement fixture already exists")

    fixture_source = harness / ".github/fixtures/agent-envelope-target-baseline.rs"
    fixture_bytes = fixture_source.read_bytes()
    original_module = (root / MODULE).read_bytes()
    registration = (
        "\n#[cfg(test)]\n"
        '#[path = "multi_agents_v2/envelope_target_baseline_tests.rs"]\n'
        "mod envelope_target_baseline;\n"
    ).encode()
    if b"mod envelope_target_baseline;" in original_module:
        raise ValueError("measurement module already registered")
    evidence = args.evidence_dir.resolve()
    evidence.mkdir(parents=True, exist_ok=False)
    (evidence / "module-before.rs").write_bytes(original_module)
    (evidence / "measurement-fixture.rs").write_bytes(fixture_bytes)
    (root / FIXTURE).write_bytes(fixture_bytes)
    (root / MODULE).write_bytes(original_module + registration)

    tracked_patch = subprocess.check_output(["git", "diff", "--binary"], cwd=root)
    fixture_patch = subprocess.run(
        ["git", "diff", "--no-index", "--binary", "--", "/dev/null", str(FIXTURE)],
        cwd=root,
        capture_output=True,
        check=False,
    )
    if fixture_patch.returncode != 1:
        raise RuntimeError(
            "new measurement fixture did not produce its expected creation patch"
        )
    overlay = tracked_patch + fixture_patch.stdout
    (evidence / "measurement-overlay.patch").write_bytes(overlay)
    receipt = {
        "format": 1,
        "source_sha": TARGET_SHA,
        "source_tree": git(root, "rev-parse", "HEAD^{tree}"),
        "harness_sha": args.harness_sha,
        "module_before_blob": git(root, "rev-parse", f"HEAD:{MODULE}"),
        "fixture_sha256": hashlib.sha256(fixture_bytes).hexdigest(),
        "overlay_sha256": hashlib.sha256(overlay).hexdigest(),
        "modified_paths": [str(MODULE), str(FIXTURE)],
        "evidence": "Test-only overlay; production factories, serializers, and dependencies are pinned upstream source.",
        "runtime_delivery_exercised": False,
    }
    (evidence / "prepare-receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps(receipt, indent=2))


if __name__ == "__main__":
    main()
