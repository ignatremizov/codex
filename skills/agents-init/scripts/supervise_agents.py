#!/usr/bin/env python3
import argparse
import json
import os
import queue
import re
import shlex
import subprocess
import sys
import threading
import time
from collections import deque
from typing import Deque, Dict, Optional, Tuple, IO, Any

import curses

DEFAULT_REVIEW_TEMPLATE = """You are reviewing output from a sub-agent.

Sub-agent prompt:
{prompt}

Sub-agent output:
{output}

Return:
1) a short verdict (correct/incorrect/uncertain),
2) any issues or missing steps,
3) concrete fixes if needed.
"""

SKILL_MARKER_RE = re.compile(r"\$([A-Za-z0-9_-]+)")


class AppServer:
    def __init__(self, cmd):
        self._proc = subprocess.Popen(
            cmd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            bufsize=1,
        )
        if self._proc.stdin is None or self._proc.stdout is None:
            raise RuntimeError("Failed to open stdin/stdout for app-server")
        self._stdin: IO[str] = self._proc.stdin
        self._stdout: IO[str] = self._proc.stdout
        self._next_id = 1
        self._lock = threading.Lock()
        self._responses = {}
        self._responses_cv = threading.Condition()
        self._events = queue.Queue()
        self._requests = queue.Queue()
        self._reader = threading.Thread(target=self._read_loop, daemon=True)
        self._reader.start()

    def _read_loop(self):
        for line in self._stdout:
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue
            if "method" in msg and "id" in msg and "result" not in msg and "error" not in msg:
                self._requests.put(msg)
            elif "id" in msg:
                with self._responses_cv:
                    self._responses[msg["id"]] = msg
                    self._responses_cv.notify_all()
            elif "method" in msg:
                self._events.put(msg)

    def notify(self, method, params=None):
        payload = {"method": method}
        if params is not None:
            payload["params"] = params
        with self._lock:
            self._stdin.write(json.dumps(payload) + "\n")
            self._stdin.flush()

    def request(self, method, params=None, timeout=30):
        with self._lock:
            req_id = self._next_id
            self._next_id += 1
            payload = {"id": req_id, "method": method}
            if params is not None:
                payload["params"] = params
            self._stdin.write(json.dumps(payload) + "\n")
            self._stdin.flush()
        return self._wait_response(req_id, timeout)

    def _wait_response(self, req_id, timeout):
        deadline = time.time() + timeout
        with self._responses_cv:
            while req_id not in self._responses:
                remaining = deadline - time.time()
                if remaining <= 0:
                    raise TimeoutError(f"request {req_id} timed out")
                self._responses_cv.wait(timeout=remaining)
            return self._responses.pop(req_id)

    def next_event(self, timeout=None):
        return self._events.get(timeout=timeout)

    def next_request(self, timeout=None):
        return self._requests.get(timeout=timeout)

    def respond(self, req_id, result):
        payload = {"id": req_id, "result": result}
        with self._lock:
            self._stdin.write(json.dumps(payload) + "\n")
            self._stdin.flush()

    def close(self):
        if self._proc.poll() is None:
            self._proc.terminate()
            try:
                self._proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self._proc.kill()


def parse_args():
    parser = argparse.ArgumentParser(
        description="Spawn multiple Codex threads and supervise their outputs."
    )
    parser.add_argument(
        "--server-cmd",
        default="codex app-server",
        help="Command to launch the app-server.",
    )
    parser.add_argument("--cwd", help="Working directory for new threads.")
    parser.add_argument(
        "--agent",
        action="append",
        default=[],
        help="Prompt for one agent. Repeat for multiple agents.",
    )
    parser.add_argument(
        "--review",
        action="store_true",
        help="Spawn reviewer threads to validate each agent output.",
    )
    parser.add_argument(
        "--review-template",
        default=DEFAULT_REVIEW_TEMPLATE,
        help="Template used for reviewer prompts.",
    )
    parser.add_argument(
        "--timeout",
        type=int,
        default=600,
        help="Overall timeout in seconds (0 disables timeout).",
    )
    parser.add_argument(
        "--max-parallel",
        type=int,
        default=None,
        help="Maximum number of agents to run at once.",
    )
    return parser.parse_args()


def resolve_skill_path(skill_name, cwd):
    candidates = []
    if cwd:
        candidates.append(os.path.join(cwd, "skills", skill_name, "SKILL.md"))
    candidates.append(
        os.path.join(os.path.expanduser("~/.codex/skills"), skill_name, "SKILL.md")
    )
    for path in candidates:
        if os.path.exists(path):
            return path
    return None


def build_turn_input(prompt, cwd):
    items = [{"type": "text", "text": prompt}]
    match = SKILL_MARKER_RE.search(prompt)
    if not match:
        return items
    skill_name = match.group(1)
    path = resolve_skill_path(skill_name, cwd)
    if not path:
        return items
    items.append({"type": "skill", "name": skill_name, "path": path})
    return items


def thread_id_from_params(params):
    return params.get("threadId") or params.get("thread_id")


def extract_directives(prompt):
    wait = None
    deps = []
    parts = prompt.split("||")
    body_parts = []
    i = 0
    while i < len(parts):
        token = parts[i].strip()
        if token.startswith("WAIT_FOR_STATUS:"):
            directive = token.replace("WAIT_FOR_STATUS:", "", 1).strip()
            if "|" in directive:
                path, status = directive.split("|", 1)
                wait = (path.strip(), status.strip())
                i += 1
                continue
        if token.startswith("WAIT_FOR_AGENT_DONE:"):
            directive = token.replace("WAIT_FOR_AGENT_DONE:", "", 1).strip()
            deps.extend([d.strip() for d in directive.split(",") if d.strip()])
            i += 1
            continue
        if token.startswith("WAIT_FOR_AGENT:"):
            directive = token.replace("WAIT_FOR_AGENT:", "", 1).strip()
            deps.extend([d.strip() for d in directive.split(",") if d.strip()])
            i += 1
            continue
        if token.startswith("WAIT_FOR_AGENTS:"):
            directive = token.replace("WAIT_FOR_AGENTS:", "", 1).strip()
            deps.extend([d.strip() for d in directive.split(",") if d.strip()])
            i += 1
            continue
        body_parts = parts[i:]
        break
    if not body_parts:
        body_parts = parts[i:]
    body = "||".join(body_parts).strip()
    return body, wait, deps


def deps_satisfied(deps, agents):
    if not deps:
        return True
    done_indices = {
        agent.get("index") for agent in agents.values() if agent.get("done")
    }
    done_names = {
        agent.get("name") for agent in agents.values() if agent.get("done") and agent.get("name")
    }
    for dep in deps:
        if dep.isdigit():
            if int(dep) not in done_indices:
                return False
        elif dep not in done_names:
            return False
    return True


def safe_addnstr(screen, row, col, text, max_width):
    if max_width <= 0:
        return
    try:
        screen.addnstr(row, col, text, max_width)
    except curses.error:
        # Ignore render failures when the terminal is too small.
        pass


def wait_for_status(path, expected, deadline, output=None):
    announced = False

    while True:
        if deadline is not None and time.time() > deadline:
            raise TimeoutError(f"waited for status '{expected}' in {path}")
        try:
            with open(path, "r", encoding="utf-8") as handle:
                first_line = handle.readline().strip()
            if first_line == expected:
                if announced and output is not None:
                    output(f"[supervisor] status ready in {path}")
                return
        except FileNotFoundError:
            pass
        if not announced and output is not None:
            output(f"[supervisor] waiting for status '{expected}' in {path}")
            announced = True
        time.sleep(5)


def summarize_item(item):
    if not isinstance(item, dict):
        return "item"
    item_type = item.get("type")
    if item_type == "commandExecution":
        cmd = item.get("command") or ""
        return f"command: {cmd}"
    if item_type == "fileChange":
        changes = item.get("changes") or []
        paths = []
        for change in changes:
            path = change.get("path")
            if path:
                paths.append(path)
        if paths:
            sample = ", ".join(paths[:3])
            suffix = "" if len(paths) <= 3 else "..."
            return f"file change: {sample}{suffix}"
        return "file change"
    if item_type == "agentMessage":
        return "agent message"
    if item_type == "reasoning":
        return "reasoning"
    if item_type == "toolCall":
        tool = item.get("toolName") or "tool"
        return f"tool: {tool}"
    return f"item: {item_type}"


def record_recent(agent, text):
    agent["recent"].append(text)
    agent["recent_stream"].append(text)
    agent["history"].append(text)
    log_line(agent, text)


def record_summary_delta(agent, delta):
    buffer = agent.get("summary_buffer", "")
    buffer += delta
    agent["summary_buffer"] = buffer


def normalize_summary_text(text):
    out = []
    i = 0
    length = len(text)
    while i < length:
        ch = text[i]
        if ch == "\r":
            i += 1
            continue
        if ch == "\n":
            if i + 1 < length and text[i + 1] == "\n":
                out.append("\n")
                i += 2
                continue
            j = i + 1
            while j < length and text[j] in (" ", "\t"):
                j += 1
            if j < length:
                next_char = text[j]
                if next_char in ("-", "*", "+", "#"):
                    out.append("\n")
                    i += 1
                    continue
                if next_char.isdigit() and j + 1 < length and text[j + 1] == ".":
                    out.append("\n")
                    i += 1
                    continue
            out.append(" ")
            i += 1
            continue
        out.append(ch)
        i += 1
    return "".join(out)


def record_agent_delta(agent, delta):
    buffer = agent.get("agent_buffer", "")
    buffer += delta
    agent["agent_buffer"] = buffer


def normalize_agent_text(text):
    return normalize_summary_text(text)


def flush_summary_buffer(agent):
    buffer = agent.get("summary_buffer", "")
    if buffer:
        normalized = normalize_summary_text(buffer)
        lines = [line.strip() for line in normalized.split("\n") if line.strip()]
        for line in lines:
            record_recent(agent, f"summary: {line}")
        agent["summary_buffer"] = ""


def flush_agent_buffer(agent):
    buffer = agent.get("agent_buffer", "")
    if buffer:
        normalized = normalize_agent_text(buffer)
        lines = [line.strip() for line in normalized.split("\n") if line.strip()]
        for line in lines:
            record_recent(agent, f"agent: {line}")
        agent["agent_buffer"] = ""


def colorize(text, code, enable):
    if not enable:
        return text
    return f"\033[{code}m{text}\033[0m"


def render_status(agents, spinner_frame, use_color, approvals_by_thread=None):
    lines = []
    for agent in agents.values():
        status = "done" if agent["done"] else "running"
        label = f"Agent {agent['index']}"
        if agent.get("name"):
            label = f"{label} ({agent['name']})"
        header = f"{label} [{status}] {spinner_frame}"
        lines.append(colorize(header, "36", use_color))
        if approvals_by_thread:
            thread_id = agent.get("thread_id")
            pending = approvals_by_thread.get(thread_id) if thread_id else None
            if pending:
                approval_text = describe_approval(agent, pending[0])
                lines.append(colorize(f"  approval: {approval_text}", "33", use_color))
        commands = agent["recent_commands"]
        if commands:
            for cmd in list(commands)[-3:]:
                lines.append(f"  {cmd}")
        else:
            lines.append("  -")
        if agent["last_agent_message"]:
            lines.append(colorize("  last message:", "90", use_color))
            for line in agent["last_agent_message"].splitlines()[:3]:
                lines.append(f"  {line}")
    return "\n".join(lines)


def print_status_block(agents, spinner_frame, use_color, approvals_by_thread=None):
    if use_color:
        print("\033[2J\033[H", end="")
    lines = []
    status_block = render_status(agents, spinner_frame, use_color, approvals_by_thread)
    if status_block:
        lines.append(status_block)
    for agent in agents.values():
        stream = agent["recent_stream"]
        if not stream:
            continue
        lines.append("")
        lines.append(colorize(f"Agent {agent['index']} stream:", "35", use_color))
        for entry in list(stream)[-3:]:
            lines.append(f"  {entry}")
    if lines:
        print("\n".join(lines))


def ensure_log_dir():
    path = os.path.expanduser("~/.codex/supervisor_logs")
    os.makedirs(path, exist_ok=True)
    return path


def log_line(agent, text):
    path = agent.get("log_path")
    if not path:
        return
    try:
        with open(path, "a", encoding="utf-8") as handle:
            handle.write(text.replace("\n", "\\n") + "\n")
    except OSError:
        pass


def sanitize_filename_component(value):
    return re.sub(r"[^A-Za-z0-9._-]+", "_", value).strip("_")


def write_review_output(thread_id, review_id, review_label, review_output, log_dir):
    timestamp = time.strftime("%Y%m%d-%H%M%S")
    safe_thread = sanitize_filename_component(thread_id or "thread")
    safe_review = sanitize_filename_component(review_id or "review")
    filename = f"review-{safe_thread}-{safe_review}-{timestamp}.md"
    path = os.path.join(log_dir, filename)
    header_lines = ["# Review Output", f"Thread: {thread_id}"]
    if review_id:
        header_lines.append(f"Review ID: {review_id}")
    if review_label:
        header_lines.append(f"Label: {review_label}")
    header_lines.append(f"Timestamp: {time.strftime('%Y-%m-%d %H:%M:%S %z')}")
    with open(path, "w", encoding="utf-8") as handle:
        handle.write("\n".join(header_lines))
        handle.write("\n\n")
        handle.write(review_output)
        if not review_output.endswith("\n"):
            handle.write("\n")
    return path


def build_display_lines(agents, spinner_frame, recent_logs, approvals_by_thread=None):
    lines = []
    for agent in agents.values():
        status = "done" if agent["done"] else "running"
        label = f"Agent {agent['index']}"
        if agent.get("name"):
            label = f"{label} ({agent['name']})"
        lines.append(f"{label} [{status}] {spinner_frame}")
        if approvals_by_thread:
            thread_id = agent.get("thread_id")
            pending = approvals_by_thread.get(thread_id) if thread_id else None
            if pending:
                approval_text = describe_approval(agent, pending[0])
                lines.append(f"  approval: {approval_text}")
        commands = agent["recent_commands"]
        if commands:
            for cmd in list(commands)[-3:]:
                lines.append(f"  {cmd}")
        else:
            lines.append("  -")
        if agent["last_agent_message"]:
            lines.append("  last message:")
            for line in agent["last_agent_message"].splitlines()[:3]:
                lines.append(f"  {line}")
        stream = agent["recent_stream"]
        if stream:
            lines.append("  stream:")
            for entry in list(stream)[-3:]:
                lines.append(f"  {entry}")
        lines.append("")
    if recent_logs:
        lines.append("Logs:")
        for entry in list(recent_logs)[-5:]:
            lines.append(f"  {entry}")
    return lines


def get_decision_payload(method, choice, amendment):
    is_legacy = method in ("applyPatchApproval", "execCommandApproval")
    if choice == "a":
        return {"decision": "approved"} if is_legacy else {"decision": "accept"}
    if choice == "s":
        return (
            {"decision": "approved_for_session"}
            if is_legacy
            else {"decision": "acceptForSession"}
        )
    if choice == "p" and amendment:
        if is_legacy:
            return {
                "decision": {
                    "approved_execpolicy_amendment": {
                        "proposed_execpolicy_amendment": amendment
                    }
                }
            }
        return {
            "decision": {
                "acceptWithExecpolicyAmendment": {"execpolicyAmendment": amendment}
            }
        }
    if choice == "d":
        return {"decision": "denied"} if is_legacy else {"decision": "decline"}
    if choice == "c":
        return {"decision": "abort"} if is_legacy else {"decision": "cancel"}
    return None


def describe_approval(agent, entry):
    params = entry.get("params") or {}
    item_id = params.get("itemId") or params.get("item_id")
    if item_id:
        item = (agent.get("items") or {}).get(item_id)
        if item:
            return summarize_item(item)
    reason = params.get("reason")
    if reason:
        return reason
    return entry.get("method") or "approval"


def parse_agent_name(prompt):
    marker = "(name:"
    if marker not in prompt:
        return None
    start = prompt.find(marker)
    if start == -1:
        return None
    end = prompt.find(")", start)
    if end == -1:
        return None
    name = prompt[start + len(marker) : end].strip()
    return name or None


def build_status_strip(agents, approvals_by_thread):
    parts = []
    for agent in agents.values():
        has_pending = bool(approvals_by_thread.get(agent.get("thread_id")))
        if has_pending:
            status = "!"
        elif agent.get("done"):
            status = "✓"
        else:
            status = "."
        queue_len = len(agent.get("queued_prompts") or [])
        suffix = f"+{queue_len}" if queue_len else ""
        label = f"{agent['index']}{status}{suffix}"
        if agent.get("name"):
            label = f"{label}:{agent['name']}"
        parts.append(label)
    return "  ".join(parts)


def start_input_reader(queue_out):
    def run():
        while True:
            try:
                line = sys.stdin.readline()
            except Exception:
                break
            if not line:
                break
            queue_out.append(line.strip())

    thread = threading.Thread(target=run, daemon=True)
    thread.start()
    return thread


def normalize_agent_key(key):
    return key.strip().lower()


def resolve_agent(agents, key):
    key = normalize_agent_key(key)
    if key.isdigit():
        idx = int(key)
        for thread_id, agent in agents.items():
            if agent["index"] == idx:
                return thread_id, agent
    for thread_id, agent in agents.items():
        name = agent.get("name")
        if name and normalize_agent_key(name) == key:
            return thread_id, agent
    return None, None


def queue_agent_prompt(agent, prompt):
    if "queued_prompts" not in agent:
        agent["queued_prompts"] = deque()
    agent["queued_prompts"].append(prompt)
    record_recent(agent, f"queued: {prompt}")


def start_agent_prompt(server, agent, prompt):
    server.request(
        "turn/start",
        {
            "threadId": agent["thread_id"],
            "input": build_turn_input(prompt, agent.get("cwd")),
        },
    )
    agent["done"] = False
    agent["last_delta"] = None
    agent["last_agent_message"] = None
    record_recent(agent, f"user: {prompt}")


def try_cancel_turn(server, agent):
    try:
        resp = server.request("turn/cancel", {"threadId": agent["thread_id"]})
        if resp.get("error"):
            return False
        return True
    except Exception:
        return False


def approve_request(server, entry, choice):
    decision = get_decision_payload(entry["method"], choice, entry.get("amendment"))
    if decision is None:
        return False
    server.respond(entry["req_id"], decision)
    return True


def handle_user_command(
    line,
    agents,
    server,
    approval_queue=None,
    approvals_by_thread=None,
    recent_logs=None,
    output=None,
):
    if output is None:
        output = print
    line = line.strip()
    if not line:
        return
    if line in ("help", "?"):
        output(
            "Commands: <id> <prompt> | <name> <prompt> | <id> <a|s|p|d|c> | approve [id] [a|s|p|d|c] | stop <id> <reason> | show <id|name> | dump <id|name> | list | review <agent|thread> [target] [--detached|--inline|delivery <mode>] | threads [loaded|list] [limit|cursor] | help | quit"
        )
        return
    if line in ("list", "ls"):
        for agent in agents.values():
            label = f"{agent['index']}"
            if agent.get("name"):
                label = f"{label} ({agent['name']})"
            status = "done" if agent["done"] else "running"
            output(f"{label}: {status}")
        return
    if line.startswith("review"):
        try:
            parts = shlex.split(line)
        except ValueError as err:
            output(f"review: {err}")
            return
        if len(parts) < 2:
            output(
                "Usage: review <agent|thread> [uncommitted|base <branch>|commit <sha> [title]|custom <instructions>] [--detached|--inline|delivery <mode>]"
            )
            return
        target_key = parts[1]
        rest = parts[2:]
        thread_id, _agent = resolve_agent(agents, target_key)
        if not thread_id:
            thread_id = target_key

        delivery = None
        remaining = []
        i = 0
        while i < len(rest):
            token = rest[i]
            if token in ("--detached", "--inline"):
                delivery = token.lstrip("--")
                i += 1
                continue
            if token == "delivery" and i + 1 < len(rest) and rest[i + 1] in (
                "inline",
                "detached",
            ):
                delivery = rest[i + 1]
                i += 2
                continue
            remaining.append(token)
            i += 1

        if not remaining:
            target = {"type": "uncommittedChanges"}
        elif remaining[0] in ("uncommitted", "uncommittedChanges", "changes", "current"):
            target = {"type": "uncommittedChanges"}
        elif remaining[0] == "base":
            if len(remaining) < 2:
                output("review: base requires a branch name")
                return
            target = {"type": "baseBranch", "branch": remaining[1]}
        elif remaining[0] == "commit":
            if len(remaining) < 2:
                output("review: commit requires a sha")
                return
            target = {"type": "commit", "sha": remaining[1]}
            title = " ".join(remaining[2:]).strip()
            if title:
                target["title"] = title
        elif remaining[0] == "custom":
            instructions = " ".join(remaining[1:]).strip()
            if not instructions:
                output("review: custom requires instructions")
                return
            target = {"type": "custom", "instructions": instructions}
        else:
            instructions = " ".join(remaining).strip()
            target = {"type": "custom", "instructions": instructions}

        params = {"threadId": thread_id, "target": target}
        if delivery:
            params["delivery"] = delivery
        try:
            resp = server.request("review/start", params)
        except Exception as err:
            output(f"review: request failed: {err}")
            return
        result = resp.get("result") or {}
        review_thread_id = result.get("reviewThreadId") or result.get("review_thread_id")
        output(
            f"review started: thread {review_thread_id or thread_id} ({target.get('type')})"
        )
        return
    if line.startswith("threads"):
        parts = line.split()
        mode = "loaded"
        cursor = None
        limit = None
        if len(parts) >= 2:
            if parts[1] in ("loaded", "list"):
                mode = parts[1]
                if len(parts) >= 3:
                    if parts[2].isdigit():
                        limit = int(parts[2])
                    else:
                        cursor = parts[2]
            elif parts[1].isdigit():
                limit = int(parts[1])
            else:
                cursor = parts[1]
        params = {}
        if cursor:
            params["cursor"] = cursor
        if limit is not None:
            params["limit"] = limit
        method = "thread/loaded/list" if mode == "loaded" else "thread/list"
        try:
            resp = server.request(method, params or None)
        except Exception as err:
            output(f"threads: request failed: {err}")
            return
        result = resp.get("result") or {}
        data = result.get("data") or []
        if mode == "list":
            thread_ids = []
            for entry in data:
                if isinstance(entry, dict):
                    thread_id = entry.get("id")
                    if thread_id:
                        thread_ids.append(thread_id)
            output(f"threads ({mode}): {len(thread_ids)}")
            for thread_id in thread_ids:
                output(f"  {thread_id}")
        else:
            output(f"threads ({mode}): {len(data)}")
            for thread_id in data:
                output(f"  {thread_id}")
        next_cursor = result.get("nextCursor") or result.get("next_cursor")
        if next_cursor:
            output(f"next_cursor: {next_cursor}")
        return
    if line in ("quit", "exit"):
        raise SystemExit(0)

    if line.startswith("show "):
        _, key = line.split(maxsplit=1)
        thread_id, agent = resolve_agent(agents, key)
        if not thread_id or agent is None:
            output(f"Unknown agent '{key}'. Use 'list' to see agents.")
            return
        if agent is None:
            return
        history = agent["history"][-20:]
        output(f"Agent {agent['index']} history (last 20):")
        for entry in history:
            output(f"  {entry}")
        return

    if line.startswith("dump "):
        _, key = line.split(maxsplit=1)
        thread_id, agent = resolve_agent(agents, key)
        if not thread_id or agent is None:
            output(f"Unknown agent '{key}'. Use 'list' to see agents.")
            return
        if agent is None:
            return
        path = agent.get("log_path")
        if path:
            output(f"Agent {agent['index']} log: {path}")
        else:
            output(f"Agent {agent['index']} has no log path.")
        return

    if line.startswith("approve"):
        if approval_queue is None or approvals_by_thread is None:
            output("No approval queue available.")
            return
        parts = line.split()
        target = None
        choice = None
        if len(parts) == 2:
            if parts[1] in ("a", "s", "p", "d", "c"):
                choice = parts[1]
            else:
                target = parts[1]
        elif len(parts) >= 3:
            target = parts[1]
            choice = parts[2]
        if not choice:
            output("Approval requires a choice: a/s/p/d/c.")
            return
        entry = None
        if target:
            thread_id, agent = resolve_agent(agents, target)
            if not thread_id or agent is None:
                output(f"Unknown agent '{target}'. Use 'list' to see agents.")
                return
            thread_queue = approvals_by_thread.get(thread_id) or deque()
            if not thread_queue:
                output(f"No pending approvals for agent {agent['index']}.")
                return
            entry = thread_queue.popleft()
            approvals_by_thread[thread_id] = thread_queue
        else:
            if not approval_queue:
                output("No pending approvals.")
                return
            entry = approval_queue.popleft()
            thread_queue = approvals_by_thread.get(entry["thread_id"]) or deque()
            if entry in thread_queue:
                thread_queue.remove(entry)
                approvals_by_thread[entry["thread_id"]] = thread_queue
        if entry and not approve_request(server, entry, choice):
            output("Invalid approval choice.")
        elif recent_logs is not None:
            recent_logs.append(f"approval {entry['thread_id']}: {choice}")
        return

    if ":" in line:
        head, prompt = line.split(":", 1)
        head = head.strip()
        prompt = prompt.strip()
    else:
        parts = line.split(maxsplit=1)
        if len(parts) < 2:
            output("Invalid command. Use: <id> <prompt> or <name> <prompt>")
            return
        head, prompt = parts[0], parts[1]

    thread_id, agent = resolve_agent(agents, head)
    if not thread_id or agent is None:
        output(f"Unknown agent '{head}'. Use 'list' to see agents.")
        return
    if agent is None:
        return

    if prompt in ("a", "s", "p", "d", "c"):
        if approval_queue is None or approvals_by_thread is None:
            output("No approval queue available.")
            return
        thread_queue = approvals_by_thread.get(thread_id) or deque()
        if not thread_queue:
            output(f"No pending approvals for agent {agent['index']}.")
            return
        entry = thread_queue.popleft()
        approvals_by_thread[thread_id] = thread_queue
        if entry in approval_queue:
            approval_queue.remove(entry)
        if not approve_request(server, entry, prompt):
            output("Invalid approval choice.")
        elif recent_logs is not None:
            recent_logs.append(f"approval {agent['index']}: {prompt}")
        return

    if prompt.startswith("stop"):
        reason = prompt[len("stop"):].strip()
        if not reason:
            reason = "stop current task and report status"
        if agent.get("done"):
            start_agent_prompt(server, agent, f"Stop: {reason}")
            return
        if try_cancel_turn(server, agent):
            record_recent(agent, "turn cancelled")
        else:
            queue_agent_prompt(agent, f"Stop: {reason}")
        return

    if agent.get("done"):
        start_agent_prompt(server, agent, prompt)
    else:
        queue_agent_prompt(agent, prompt)


def prompt_approval(server, agents, req, use_color):
    method = req.get("method")
    params = req.get("params") or {}
    req_id = req.get("id")
    thread_id = params.get("threadId") or params.get("thread_id") or "unknown"
    item_id = params.get("itemId") or params.get("item_id")
    reason = params.get("reason")
    print("\n" + colorize("[approval] request", "33", use_color))
    print(f"method: {method}")
    print(f"thread: {thread_id}")
    if item_id:
        print(f"item: {item_id}")
    if reason:
        print(f"reason: {reason}")
    if thread_id in agents and item_id:
        item = agents[thread_id]["items"].get(item_id, {})
        if item:
            print(f"details: {summarize_item(item)}")

    decision = None
    amendment = params.get("proposedExecpolicyAmendment") or params.get(
        "proposed_execpolicy_amendment"
    )
    while decision is None:
        if method == "item/commandExecution/requestApproval":
            prompt = "approve? [a]ccept, [s]ession, [p]execpolicy, [d]ecline, [c]ancel: "
        elif method == "item/fileChange/requestApproval":
            prompt = "approve? [a]ccept, [s]ession, [d]ecline, [c]cancel: "
        else:
            prompt = "approve? [a]ccept, [d]ecline, [c]cancel: "
        choice = input(prompt).strip().lower()
        decision = get_decision_payload(method, choice, amendment)
        if decision is None:
            print("invalid choice")
    server.respond(req_id, decision)


def process_event(
    event,
    agents,
    pending_reviews,
    args,
    server,
    recent_logs,
    log_dir,
    review_labels,
    review_written,
):
    method = event.get("method")
    params = event.get("params") or {}
    thread_id = thread_id_from_params(params)
    if method == "turn/started" and thread_id in agents:
        record_recent(agents[thread_id], "turn started")
    elif method == "item/started" and thread_id in agents:
        item = params.get("item") or {}
        item_id = item.get("id")
        if item_id:
            agents[thread_id]["items"][item_id] = item
        if item.get("type") == "commandExecution":
            cmd = item.get("command") or ""
            if cmd:
                agents[thread_id]["recent_commands"].append(cmd)
        if item.get("type") == "fileChange":
            record_recent(agents[thread_id], summarize_item(item))
    elif method == "item/completed" and thread_id in agents:
        item = params.get("item") or {}
        item_id = item.get("id")
        if item_id:
            agents[thread_id]["items"][item_id] = item
        if item.get("type") == "fileChange":
            record_recent(agents[thread_id], summarize_item(item))
        if item.get("type") == "enteredReviewMode":
            review_id = item.get("id")
            if review_id:
                review_labels[review_id] = item.get("review")
        if item.get("type") == "exitedReviewMode":
            review_output = item.get("review") or ""
            review_id = item.get("id")
            if review_output and review_id and review_id not in review_written:
                review_label = review_labels.pop(review_id, None)
                path = write_review_output(
                    thread_id,
                    review_id,
                    review_label,
                    review_output,
                    log_dir,
                )
                review_written.add(review_id)
                record_recent(agents[thread_id], f"review output saved: {path}")
                recent_logs.append(f"review output saved: {path}")
        if item.get("type") == "agentMessage":
            flush_agent_buffer(agents[thread_id])
            agents[thread_id]["last_message"] = item.get("text")
            agents[thread_id]["last_delta"] = None
            agents[thread_id]["last_agent_message"] = item.get("text")
    elif method == "item/agentMessage/delta" and thread_id in agents:
        delta = params.get("delta") or ""
        if delta:
            agents[thread_id]["last_delta"] = delta.strip()
            record_agent_delta(agents[thread_id], delta)
    elif method == "item/reasoning/textDelta" and thread_id in agents:
        delta = params.get("delta") or ""
        if delta:
            record_recent(agents[thread_id], f"reasoning: {delta.strip()}")
    elif method == "item/reasoning/summaryTextDelta" and thread_id in agents:
        delta = params.get("delta") or ""
        if delta:
            record_summary_delta(agents[thread_id], delta)
    elif method == "item/commandExecution/outputDelta" and thread_id in agents:
        delta = params.get("delta") or ""
        if delta:
            record_recent(agents[thread_id], f"cmd out: {delta.strip()}")
    elif method == "item/mcpToolCall/progress" and thread_id in agents:
        message = params.get("message") or ""
        if message:
            record_recent(agents[thread_id], f"mcp: {message}")
    elif method == "item/fileChange/outputDelta" and thread_id in agents:
        delta = params.get("delta") or ""
        if delta:
            record_recent(agents[thread_id], "file change output")
    elif method == "turn/completed" and thread_id in agents:
        status = params.get("turn", {}).get("status")
        agent = agents[thread_id]
        agent["done"] = True
        flush_agent_buffer(agent)
        flush_summary_buffer(agent)
        record_recent(agent, f"turn {status}")
        recent_logs.append(f"agent {agent['index']} completed status={status}")
        queued = agent.get("queued_prompts") or deque()
        if queued:
            next_prompt = queued.popleft()
            agent["queued_prompts"] = queued
            start_agent_prompt(server, agent, next_prompt)
            recent_logs.append(f"agent {agent['index']} dequeued prompt")
        if args.review and agent["last_message"]:
            review_prompt = args.review_template.format(
                prompt=agent["prompt"],
                output=agent["last_message"],
            )
            review_params = {"cwd": args.cwd} if args.cwd else {}
            resp = server.request("thread/start", review_params)
            review_thread = resp["result"]["thread"]["id"]
            server.request(
                "turn/start",
                {
                    "threadId": review_thread,
                    "input": [{"type": "text", "text": review_prompt}],
                },
            )
            pending_reviews[review_thread] = agent["index"]
            agent["review_thread"] = review_thread
            recent_logs.append(f"review {agent['index']} started")
    elif method == "item/completed" and thread_id in pending_reviews:
        item = params.get("item") or {}
        if item.get("type") == "agentMessage":
            index = pending_reviews[thread_id]
            text = item.get("text", "").strip()
            recent_logs.append(f"review {index}: {text[:120]}")
    elif method == "turn/completed" and thread_id in pending_reviews:
        index = pending_reviews.pop(thread_id)
        recent_logs.append(f"review {index} completed")


def run_plain(server, args):
    deadline = None if args.timeout <= 0 else time.time() + args.timeout
    log_dir = ensure_log_dir()
    review_labels = {}
    review_written = set()
    agents = {}
    pending_prompts = deque()
    for index, prompt in enumerate(args.agent, start=1):
        body, wait, deps = extract_directives(prompt)
        pending_prompts.append(
            {"index": index, "prompt": body, "wait": wait, "deps": deps}
        )
    max_parallel = args.max_parallel or len(args.agent)

    def start_agent(item):
        index = item["index"]
        prompt = item["prompt"]
        wait = item["wait"]
        if wait:
            wait_for_status(wait[0], wait[1], deadline, output=print)
        params = {"cwd": args.cwd} if args.cwd else {}
        resp = server.request("thread/start", params)
        thread_id = resp["result"]["thread"]["id"]
        agent_name = parse_agent_name(prompt)
        server.request(
            "turn/start",
            {
                "threadId": thread_id,
                "input": build_turn_input(prompt, args.cwd),
            },
        )
        agents[thread_id] = {
            "index": index,
            "name": agent_name,
            "prompt": prompt,
            "cwd": args.cwd,
            "thread_id": thread_id,
            "last_message": None,
            "last_delta": None,
            "last_agent_message": None,
            "done": False,
            "recent": deque(maxlen=3),
            "recent_commands": deque(maxlen=3),
            "recent_stream": deque(maxlen=3),
            "history": [],
            "agent_buffer": "",
            "summary_buffer": "",
            "log_path": os.path.join(log_dir, f"agent-{index}.log"),
            "items": {},
            "queued_prompts": deque(),
            "review_thread": None,
        }
        print(f"[agent {index}] started thread {thread_id}")

    def start_ready_agents():
        started_any = False
        remaining = deque()
        while pending_prompts and len([a for a in agents.values() if not a["done"]]) < max_parallel:
            item = pending_prompts.popleft()
            if deps_satisfied(item["deps"], agents):
                start_agent(item)
                started_any = True
            else:
                remaining.append(item)
            if not pending_prompts:
                break
        while pending_prompts:
            remaining.append(pending_prompts.popleft())
        pending_prompts.extend(remaining)
        return started_any

    start_ready_agents()

    pending_reviews = {}
    spinner = ["|", "/", "-", "\\"]
    spinner_idx = 0
    last_status = 0.0
    suppress_ui_until = 0.0
    awaiting_approval = False
    approval_queue = deque()
    use_color = sys.stdout.isatty()
    input_queue = deque()
    start_input_reader(input_queue)
    recent_logs = deque(maxlen=10)

    while True:
        if deadline is not None and time.time() > deadline:
            raise TimeoutError("supervisor timed out")
        if (not pending_prompts) and all(a["done"] for a in agents.values()) and not pending_reviews:
            break
        start_ready_agents()
        while True:
            try:
                req = server.next_request(timeout=0)
            except queue.Empty:
                break
            approval_queue.append(req)
        if approval_queue and not awaiting_approval:
            awaiting_approval = True
            req = approval_queue.popleft()
            prompt_approval(server, agents, req, use_color)
            awaiting_approval = False
        if input_queue and not awaiting_approval:
            line = input_queue.popleft()
            handle_user_command(line, agents, server)
        try:
            event = server.next_event(timeout=1)
        except queue.Empty:
            event = None
        if event is None:
            if (
                not awaiting_approval
                and time.time() >= suppress_ui_until
                and time.time() - last_status >= 2
            ):
                spinner_idx = (spinner_idx + 1) % len(spinner)
                print_status_block(agents, spinner[spinner_idx], use_color)
                last_status = time.time()
            continue
        process_event(
            event,
            agents,
            pending_reviews,
            args,
            server,
            recent_logs,
            log_dir,
            review_labels,
            review_written,
        )
        if (
            not awaiting_approval
            and time.time() >= suppress_ui_until
            and time.time() - last_status >= 2
        ):
            spinner_idx = (spinner_idx + 1) % len(spinner)
            print_status_block(agents, spinner[spinner_idx], use_color)
            last_status = time.time()


def run_curses(screen, server, args):
    screen.nodelay(True)
    curses.curs_set(1)
    input_buffer = ""
    approval_queue = deque()
    approvals_by_thread = {}
    recent_logs = deque(maxlen=10)
    log_dir = ensure_log_dir()
    review_labels = {}
    review_written = set()
    view_mode = "main"
    inspect_agent_id = None
    inspect_offset = 0
    main_offset = 0

    deadline = None if args.timeout <= 0 else time.time() + args.timeout
    agents = {}
    pending_prompts = deque()
    for index, prompt in enumerate(args.agent, start=1):
        body, wait, deps = extract_directives(prompt)
        pending_prompts.append(
            {"index": index, "prompt": body, "wait": wait, "deps": deps}
        )
    max_parallel = args.max_parallel or len(args.agent)

    def start_agent(item):
        index = item["index"]
        prompt = item["prompt"]
        wait = item["wait"]
        if wait:
            wait_for_status(wait[0], wait[1], deadline, output=recent_logs.append)
        params = {"cwd": args.cwd} if args.cwd else {}
        resp = server.request("thread/start", params)
        thread_id = resp["result"]["thread"]["id"]
        agent_name = parse_agent_name(prompt)
        server.request(
            "turn/start",
            {
                "threadId": thread_id,
                "input": build_turn_input(prompt, args.cwd),
            },
        )
        agents[thread_id] = {
            "index": index,
            "name": agent_name,
            "prompt": prompt,
            "cwd": args.cwd,
            "thread_id": thread_id,
            "last_message": None,
            "last_delta": None,
            "last_agent_message": None,
            "done": False,
            "recent": deque(maxlen=3),
            "recent_commands": deque(maxlen=3),
            "recent_stream": deque(maxlen=3),
            "history": [],
            "agent_buffer": "",
            "summary_buffer": "",
            "log_path": os.path.join(log_dir, f"agent-{index}.log"),
            "items": {},
            "queued_prompts": deque(),
            "review_thread": None,
        }
        recent_logs.append(f"agent {index} started thread {thread_id}")

    def start_ready_agents():
        started_any = False
        remaining = deque()
        while pending_prompts and len([a for a in agents.values() if not a["done"]]) < max_parallel:
            item = pending_prompts.popleft()
            if deps_satisfied(item["deps"], agents):
                start_agent(item)
                started_any = True
            else:
                remaining.append(item)
            if not pending_prompts:
                break
        while pending_prompts:
            remaining.append(pending_prompts.popleft())
        pending_prompts.extend(remaining)
        return started_any

    start_ready_agents()

    pending_reviews = {}
    spinner = ["|", "/", "-", "\\"]
    spinner_idx = 0
    last_render = 0.0

    while True:
        if deadline is not None and time.time() > deadline:
            raise TimeoutError("supervisor timed out")
        if (not pending_prompts) and all(a["done"] for a in agents.values()) and not pending_reviews:
            break
        start_ready_agents()
        while True:
            try:
                req = server.next_request(timeout=0)
            except queue.Empty:
                break
            params = req.get("params") or {}
            thread_id = thread_id_from_params(params) or "unknown"
            entry = {
                "req_id": req.get("id"),
                "method": req.get("method"),
                "params": params,
                "thread_id": thread_id,
                "amendment": params.get("proposedExecpolicyAmendment")
                or params.get("proposed_execpolicy_amendment"),
            }
            approval_queue.append(entry)
            approvals_by_thread.setdefault(thread_id, deque()).append(entry)
            recent_logs.append(f"approval request: {thread_id}")

        try:
            event = server.next_event(timeout=0)
        except queue.Empty:
            event = None
        if event is not None:
            process_event(
                event,
                agents,
                pending_reviews,
                args,
                server,
                recent_logs,
                log_dir,
                review_labels,
                review_written,
            )

        now = time.time()
        if now - last_render >= 0.2:
            try:
                spinner_idx = (spinner_idx + 1) % len(spinner)
                height, width = screen.getmaxyx()
                reserved_lines = 3
                body_height = max(0, height - reserved_lines)
                if view_mode == "inspect" and inspect_agent_id in agents:
                    agent = agents[inspect_agent_id]
                    lines = [
                        f"Inspect Agent {agent['index']} ({agent.get('name') or ''})",
                        "",
                    ]
                    history = agent["history"]
                    if not history:
                        lines.append("No history yet.")
                    else:
                        start = max(0, len(history) - inspect_offset - max(0, body_height - 2))
                        end = len(history) - inspect_offset
                        for entry in history[start:end]:
                            lines.append(entry)
                else:
                    lines = build_display_lines(
                        agents, spinner[spinner_idx], recent_logs, approvals_by_thread
                    )
                screen.erase()
                max_width = max(0, width - 1)
                max_offset = max(0, len(lines) - body_height)
                if main_offset > max_offset:
                    main_offset = max_offset
                start = main_offset if view_mode == "main" else 0
                end = start + body_height
                for i, line in enumerate(lines[start:end]):
                    safe_addnstr(screen, i, 0, line, max_width)
                status_line = build_status_strip(agents, approvals_by_thread)
                safe_addnstr(screen, height - 2, 0, " " * max_width, max_width)
                safe_addnstr(screen, height - 2, 0, status_line, max_width)
                safe_addnstr(screen, height - 1, 0, " " * max_width, max_width)
                safe_addnstr(screen, height - 1, 0, "cmd> " + input_buffer, max_width)
                screen.refresh()
                last_render = now
            except curses.error:
                # Terminal too small; skip this render tick.
                last_render = now

        try:
            ch = screen.getch()
        except Exception:
            ch = -1
        if ch == -1:
            time.sleep(0.05)
            continue
        if ch in (curses.KEY_BACKSPACE, 127, 8):
            input_buffer = input_buffer[:-1]
            continue
        if ch in (10, 13):
            line = input_buffer.strip()
            input_buffer = ""
            if line:
                if " " not in line and ":" not in line:
                    thread_id, _agent = resolve_agent(agents, line)
                    if thread_id:
                        view_mode = "inspect"
                        inspect_agent_id = thread_id
                        inspect_offset = 0
                        continue
                if line.startswith("show "):
                    _, key = line.split(maxsplit=1)
                    thread_id, _agent = resolve_agent(agents, key)
                    if thread_id:
                        view_mode = "inspect"
                        inspect_agent_id = thread_id
                        inspect_offset = 0
                    else:
                        recent_logs.append(f"Unknown agent '{key}'")
                elif line in ("back", "exit", "quit"):
                    view_mode = "main"
                    inspect_agent_id = None
                elif line.startswith("dump "):
                    _, key = line.split(maxsplit=1)
                    thread_id, _agent = resolve_agent(agents, key)
                    if thread_id and _agent:
                        recent_logs.append(
                            f"agent {_agent['index']} log: {_agent['log_path']}"
                        )
                else:
                    handle_user_command(
                        line,
                        agents,
                        server,
                        approval_queue=approval_queue,
                        approvals_by_thread=approvals_by_thread,
                        recent_logs=recent_logs,
                        output=recent_logs.append,
                    )
            continue
        if ch in (27,):
            input_buffer = ""
            view_mode = "main"
            inspect_agent_id = None
            continue
        if view_mode == "inspect":
            if ch in (curses.KEY_UP, ord("k")):
                inspect_offset = min(len(agents[inspect_agent_id]["history"]), inspect_offset + 1)
            elif ch in (curses.KEY_DOWN, ord("j")):
                inspect_offset = max(0, inspect_offset - 1)
            elif ch == curses.KEY_PPAGE:
                inspect_offset = min(
                    len(agents[inspect_agent_id]["history"]), inspect_offset + 10
                )
            elif ch == curses.KEY_NPAGE:
                inspect_offset = max(0, inspect_offset - 10)
            elif ch in (ord("b"),):
                view_mode = "main"
                inspect_agent_id = None
        elif view_mode == "main" and not input_buffer:
            if ch in (curses.KEY_UP, ord("k")):
                main_offset = max(0, main_offset - 1)
            elif ch in (curses.KEY_DOWN, ord("j")):
                main_offset = main_offset + 1
            elif ch == curses.KEY_PPAGE:
                main_offset = max(0, main_offset - 10)
            elif ch == curses.KEY_NPAGE:
                main_offset = main_offset + 10
        if 32 <= ch <= 126:
            input_buffer += chr(ch)


def main():
    args = parse_args()
    if not args.agent:
        raise SystemExit("Provide at least one --agent prompt.")

    server = AppServer(shlex.split(args.server_cmd))
    try:
        server.request(
            "initialize",
            {"clientInfo": {"name": "supervisor", "version": "0.1.0"}},
        )
        server.notify("initialized", {})
        if sys.stdout.isatty():
            curses.wrapper(run_curses, server, args)
        else:
            run_plain(server, args)
    except KeyboardInterrupt:
        print("\n[supervisor] interrupted, shutting down...")
    finally:
        server.close()


if __name__ == "__main__":
    main()
