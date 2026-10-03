#!/usr/bin/env python3
"""Run a CI build with durable logs, periodic resource samples, and Cargo timings."""

import argparse
import datetime
import os
from pathlib import Path
import platform
import shlex
import shutil
import signal
import subprocess
import sys
import threading
import time


def capture(command: list[str]) -> str:
    try:
        result = subprocess.run(
            command, capture_output=True, text=True, timeout=15, check=False
        )
        return f"$ {shlex.join(command)}\n{result.stdout}{result.stderr}"
    except (OSError, subprocess.TimeoutExpired) as error:
        return f"$ {shlex.join(command)}\n{error}\n"


def sample(path: Path, stop: threading.Event) -> None:
    while not stop.is_set():
        now = datetime.datetime.now(datetime.timezone.utc).isoformat()
        commands = [["df", "-h", os.environ.get("CARGO_TARGET_DIR", ".")]]
        if platform.system() == "Darwin":
            commands += [["vm_stat"], ["sysctl", "vm.swapusage"]]
        else:
            commands += [["free", "-h"]]
        # comm deliberately excludes command arguments and environment secrets.
        commands += [["ps", "-axo", "pid,ppid,comm,rss,pcpu,etime"]]
        if shutil.which("sccache"):
            commands += [["sccache", "--show-stats"]]
        with path.open("a") as output:
            output.write(f"\n=== {now} ===\n")
            for command in commands:
                result = capture(command)
                output.write(result)
                if command[0] in {"free", "sysctl", "df"}:
                    print(result.rstrip(), flush=True)
                elif command[0] == "sccache":
                    for line in result.splitlines():
                        if line.startswith(
                            (
                                "Compile requests",
                                "Cache hits",
                                "Cache misses",
                                "Cache read errors",
                                "Cache write errors",
                                "Cache location",
                            )
                        ):
                            print(line, flush=True)
        print(f"[release-build] {now}: resource/cache sample saved", flush=True)
        stop.wait(120)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--phase", required=True)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command
    if command[:1] == ["--"]:
        command = command[1:]
    if not command or not args.phase.replace("-", "").isalnum():
        parser.error("a simple phase name and a command after -- are required")
    diagnostics = Path(os.environ["RELEASE_DIAGNOSTICS_DIR"])
    diagnostics.mkdir(parents=True, exist_ok=True)
    stop = threading.Event()
    sampler = threading.Thread(
        target=sample,
        args=(diagnostics / f"{args.phase}-resources.log", stop),
        daemon=True,
    )
    # /usr/bin/time records peak RSS and paging counters when the child exits.
    timed_command = [
        "/usr/bin/time",
        "-l" if platform.system() == "Darwin" else "-v",
        *command,
    ]
    print(f"[release-build] {args.phase}: {shlex.join(command)}", flush=True)
    started = time.monotonic()
    with (diagnostics / f"{args.phase}.log").open("w") as output:
        process = subprocess.Popen(
            timed_command,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            errors="replace",
            start_new_session=True,
        )

        def forward_signal(number: int, _frame: object) -> None:
            if process.poll() is None:
                try:
                    os.killpg(process.pid, number)
                except ProcessLookupError:
                    pass

        signal.signal(signal.SIGTERM, forward_signal)
        signal.signal(signal.SIGINT, forward_signal)
        sampler.start()
        try:
            if process.stdout is None:
                raise RuntimeError("build stdout pipe was not created")
            for line in process.stdout:
                output.write(line)
                output.flush()
                print(line, end="", flush=True)
            returncode = process.wait()
        finally:
            stop.set()
            sampler.join(timeout=75)
            target_dir = Path(os.environ["CARGO_TARGET_DIR"])
            for timings in (
                target_dir / "cargo-timings" / "cargo-timing.html",
                target_dir
                / os.environ.get("RELEASE_TARGET", "")
                / "cargo-timings"
                / "cargo-timing.html",
            ):
                if timings.is_file():
                    shutil.copy2(timings, diagnostics / f"{args.phase}-timing.html")
                    break
    print(
        f"[release-build] {args.phase}: exit={returncode}, "
        f"wall_seconds={time.monotonic() - started:.1f}",
        flush=True,
    )
    return returncode if returncode >= 0 else 128 - returncode


if __name__ == "__main__":
    sys.exit(main())
