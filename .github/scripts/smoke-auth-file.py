#!/usr/bin/env python3
"""Exercise credential-file selection on CI-built executables, without real credentials."""

import argparse
import json
import os
from pathlib import Path
import queue
import subprocess
import tempfile
import threading
import time
import tomllib


DEFAULT_KEY = "sk-ci-default-not-a-real-key-000000000000"
SELECTED_KEY = "sk-ci-selected-not-a-real-key-111111111111"


def run(command: list[str], env: dict[str, str], cwd: Path, stdin: str = "") -> str:
    result = subprocess.run(
        command,
        input=stdin,
        env=env,
        cwd=cwd,
        capture_output=True,
        text=True,
        timeout=45,
        check=False,
    )
    if result.returncode:
        raise RuntimeError(f"{command} exited {result.returncode}: {result.stderr}")
    return result.stdout + result.stderr


def expect_rejected(
    command: list[str], env: dict[str, str], cwd: Path, text: str
) -> None:
    result = subprocess.run(
        command,
        input="",
        env=env,
        cwd=cwd,
        capture_output=True,
        text=True,
        timeout=45,
        check=False,
    )
    output = result.stdout + result.stderr
    if result.returncode == 0 or text not in output:
        raise RuntimeError(f"expected rejection containing {text!r}: {output}")


def account(server: Path, env: dict[str, str], cwd: Path) -> object:
    """Read account state over the release app-server's public stdio protocol."""
    with tempfile.TemporaryFile(mode="w+") as errors:
        process = subprocess.Popen(
            [str(server), "--listen", "stdio://"],
            env=env,
            cwd=cwd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=errors,
            text=True,
        )
        lines: queue.Queue[str | None] = queue.Queue()

        def read_output() -> None:
            if process.stdout is not None:
                for line in process.stdout:
                    lines.put(line)
            lines.put(None)

        reader = threading.Thread(target=read_output, daemon=True)
        reader.start()

        def send(message: dict) -> None:
            if process.stdin is None:
                raise RuntimeError("missing app-server stdin")
            process.stdin.write(json.dumps(message) + "\n")
            process.stdin.flush()

        def response(identifier: int) -> dict:
            deadline = time.monotonic() + 45
            while time.monotonic() < deadline:
                try:
                    line = lines.get(timeout=max(0.01, deadline - time.monotonic()))
                except queue.Empty as error:
                    raise RuntimeError("app-server response timed out") from error
                if line is None:
                    errors.seek(0)
                    raise RuntimeError(f"app-server closed stdout: {errors.read()}")
                message = json.loads(line)
                if message.get("id") == identifier:
                    if "error" in message:
                        raise RuntimeError(f"app-server RPC error: {message['error']}")
                    return message["result"]
            raise RuntimeError("app-server response timed out")

        try:
            send(
                {
                    "id": 0,
                    "method": "initialize",
                    "params": {
                        "clientInfo": {"name": "release_auth_smoke", "version": "1"},
                        "capabilities": {"experimentalApi": True},
                    },
                }
            )
            response(0)
            send({"method": "initialized", "params": {}})
            send({"id": 1, "method": "account/read", "params": {"refreshToken": False}})
            return response(1)["account"]
        finally:
            if process.stdin is not None:
                process.stdin.close()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
            reader.join(timeout=5)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    args = parser.parse_args()
    cli = (args.bin_dir / "codex").resolve(strict=True)
    server = (args.bin_dir / "codex-app-server").resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="codex-release-auth-") as temporary:
        root = Path(temporary).resolve()
        home = root / "home"
        codex_home = home / ".codex"
        codex_home.mkdir(parents=True)
        (codex_home / "config.toml").write_text('cli_auth_credentials_store = "file"\n')
        # A fresh home/cwd and an allowlisted environment cannot touch the user's
        # credentials, contact their daemon, or inherit API tokens from CI.
        env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(home),
            "CODEX_HOME": str(codex_home),
            "XDG_CONFIG_HOME": str(home / ".config"),
            "XDG_CACHE_HOME": str(home / ".cache"),
            "TMPDIR": str(root),
            "TERM": "dumb",
        }
        default = codex_home / "auth.json"
        default.write_text(json.dumps({"OPENAI_API_KEY": DEFAULT_KEY}))
        original = default.read_bytes()
        selected = codex_home / "auth-ci-selected.json"
        selected_env = {**env, "CODEX_AUTH_FILE": selected.name}
        login = [str(cli), "login", "--with-api-key"]
        status = [str(cli), "login", "status"]
        run(login, selected_env, home, SELECTED_KEY + "\n")
        if json.loads(selected.read_text())["OPENAI_API_KEY"] != SELECTED_KEY:
            raise RuntimeError("login did not write the selected credential file")
        if "Logged in using an API key" not in run(status, selected_env, home):
            raise RuntimeError("selected-file login status did not report API-key auth")
        missing_env = {**env, "CODEX_AUTH_FILE": "auth-ci-missing.json"}
        expect_rejected(status, missing_env, home, "Not logged in")
        if account(server, selected_env, home) != {"type": "apiKey"}:
            raise RuntimeError("app-server did not load the selected credential file")
        if account(server, missing_env, home) is not None:
            raise RuntimeError(
                "app-server fell back to default auth despite an explicit selector"
            )
        invalid_env = {**env, "CODEX_AUTH_FILE": "../escape.json"}
        error = "CODEX_AUTH_FILE must be a filename or an absolute file path"
        expect_rejected(status, invalid_env, home, error)
        expect_rejected([str(server), "--listen", "stdio://"], invalid_env, home, error)
        absolute = root / "absolute-auth.json"
        absolute_env = {**env, "CODEX_AUTH_FILE": str(absolute)}
        run(login, absolute_env, home, SELECTED_KEY + "\n")
        if json.loads(absolute.read_text())["OPENAI_API_KEY"] != SELECTED_KEY:
            raise RuntimeError("absolute credential selector was not respected")
        run([str(cli), "logout"], selected_env, home)
        if (
            selected.exists()
            or default.read_bytes() != original
            or not absolute.is_file()
        ):
            raise RuntimeError("selected logout changed the wrong credential profile")
        version = run([str(cli), "--version"], env, home).strip()
        workspace = Path(__file__).resolve().parents[2] / "codex-rs/Cargo.toml"
        expected_version = tomllib.loads(workspace.read_text())["workspace"]["package"][
            "version"
        ]
        if not version.startswith(f"codex-cli {expected_version}\n") and version != (
            f"codex-cli {expected_version}"
        ):
            raise RuntimeError(f"unexpected release version: {version!r}")
        print(version)
        print(
            "PASS: CODEX_AUTH_FILE login/status/logout, absolute paths, and invalid selectors"
        )
        print(
            "PASS: app-server selected-file account/read and no fallback to default credentials"
        )


if __name__ == "__main__":
    main()
