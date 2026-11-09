import argparse
import collections
import errno
import fcntl
import http.server
import json
import os
import pathlib
import pty
import re
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
import uuid

BINARY = pathlib.Path(__file__).resolve().parents[2] / "target/debug/aisession-backup"
CLAUDE_VERSION = os.environ.get("CLAUDE_VERSION", "2.1.319")
CODEX_VERSION = os.environ.get("CODEX_VERSION", "0.167.0")
ANTHROPIC_KEY = "sk-ant-resume-check-dummy-not-a-real-key"
OPENAI_KEY = "sk-resume-check-dummy-not-a-real-key"
STAMP = "2026-11-01T10:00:00.000Z"
ANSI = re.compile(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b\[[0-?]*[ -/]*[@-~]|\x1b[()][0-2A-Z]|\x1b[=>78]")


def text_screen(raw):
    text = ANSI.sub("", raw.decode("utf-8", errors="replace"))
    return "".join(c for c in text if c in "\n\t" or c.isprintable())


class NetworkGuard:
    def __init__(self):
        self.requests = []
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def blocked(self):
                owner.requests.append((self.command, self.path))
                body = b'{"error":{"message":"Network disabled by resume check","type":"offline_check"}}'
                self.send_response(503)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                try:
                    self.wfile.write(body)
                except (BrokenPipeError, ConnectionResetError):
                    pass

            do_GET = blocked
            do_POST = blocked
            do_CONNECT = blocked
            do_PUT = blocked
            do_DELETE = blocked

            def log_message(self, *_args):
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = f"http://127.0.0.1:{self.server.server_port}"

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()


def write(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(value, encoding="utf-8")


def isolated_env(home, guard):
    home.mkdir(parents=True, exist_ok=True)
    env = {key: os.environ[key] for key in ("PATH", "LANG", "LC_ALL", "LD_LIBRARY_PATH") if key in os.environ}
    env.update(
        HOME=str(home), USERPROFILE=str(home),
        CLAUDE_CONFIG_DIR=str(home / ".claude"), CODEX_HOME=str(home / ".codex"),
        AISESSION_BACKUP_DATA_DIR=str(home / "data"),
        XDG_CONFIG_HOME=str(home / ".config"), XDG_DATA_HOME=str(home / ".local/share"),
        XDG_CACHE_HOME=str(home / ".cache"), XDG_STATE_HOME=str(home / ".local/state"),
        TMPDIR=str(home / "tmp"), TERM="xterm-256color", COLORTERM="truecolor",
        ANTHROPIC_API_KEY=ANTHROPIC_KEY, OPENAI_API_KEY=OPENAI_KEY,
        ANTHROPIC_BASE_URL=guard.url, OPENAI_BASE_URL=guard.url + "/v1",
        HTTP_PROXY=guard.url, HTTPS_PROXY=guard.url, ALL_PROXY=guard.url,
        http_proxy=guard.url, https_proxy=guard.url, all_proxy=guard.url,
        NO_PROXY="localhost,127.0.0.1", no_proxy="localhost,127.0.0.1",
        CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1", DISABLE_AUTOUPDATER="1",
        DISABLE_TELEMETRY="1", DISABLE_ERROR_REPORTING="1",
    )
    (home / "tmp").mkdir(exist_ok=True)
    return env


def seed(home, cwd):
    cwd.mkdir(parents=True, exist_ok=True)
    state = {
        "hasCompletedOnboarding": True, "theme": "dark", "numStartups": 1,
        "lastOnboardingVersion": CLAUDE_VERSION, "autoUpdates": False,
        "customApiKeyResponses": {"approved": [ANTHROPIC_KEY[-20:]], "rejected": []},
        "projects": {str(cwd): {"hasTrustDialogAccepted": True, "hasCompletedProjectOnboarding": True}},
    }
    for path in (home / ".claude.json", home / ".claude/.claude.json"):
        write(path, json.dumps(state))
    write(home / ".codex/auth.json", json.dumps({"auth_mode": "apikey", "OPENAI_API_KEY": OPENAI_KEY}))
    write(home / ".codex/config.toml", f'check_for_update_on_startup = false\n[projects.{json.dumps(str(cwd))}]\ntrust_level = "trusted"\n')


def fixture(home, cwd, provider, marker):
    sid = str(uuid.uuid4())
    user = "Remember this synthetic conversation for the resume check."
    if provider == "claude":
        rows = []
        parent = None
        for role, content in (("user", user), ("assistant", marker)):
            item_id = str(uuid.uuid4())
            message = {"role": role, "content": content}
            if role == "assistant":
                message.update(id="msg_synthetic", type="message", model="claude-sonnet-4-6", content=[{"type": "text", "text": content}], stop_reason="end_turn", stop_sequence=None, usage={"input_tokens": 1, "output_tokens": 1})
            rows.append({"type": role, "sessionId": sid, "cwd": str(cwd), "uuid": item_id, "parentUuid": parent, "isSidechain": False, "userType": "external", "version": CLAUDE_VERSION, "gitBranch": "", "timestamp": STAMP, "message": message})
            parent = item_id
        path = home / ".claude/projects" / re.sub(r"[^A-Za-z0-9]", "-", str(cwd)) / f"{sid}.jsonl"
    else:
        rows = [{"timestamp": STAMP, "type": "session_meta", "payload": {"id": sid, "session_id": sid, "timestamp": STAMP, "cwd": str(cwd), "originator": "codex_cli_rs", "cli_version": CODEX_VERSION, "source": "cli", "model_provider": "openai"}}]
        for role, content in (("user", user), ("assistant", marker)):
            rows.append({"timestamp": STAMP, "type": "response_item", "payload": {"type": "message", "role": role, "content": [{"type": "input_text" if role == "user" else "output_text", "text": content}]}})
            rows.append({"timestamp": STAMP, "type": "event_msg", "payload": {"type": "user_message" if role == "user" else "agent_message", "message": content}})
        path = home / ".codex/sessions/2026/11/01" / f"rollout-2026-11-01T10-00-00-{sid}.jsonl"
        write(home / ".codex/session_index.jsonl", json.dumps({"id": sid, "thread_name": "Synthetic resume history", "updated_at": STAMP}) + "\n")
    write(path, "".join(json.dumps(row) + "\n" for row in rows))
    return sid


def checked(env, cwd, *args):
    print("$", " ".join(map(str, args)), flush=True)
    result = subprocess.run(list(map(str, args)), cwd=cwd, env=env, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=120)
    if result.returncode:
        raise RuntimeError(f"exit {result.returncode}\n{result.stdout}\n{result.stderr}")
    return result.stdout


def resume(env, cwd, provider, sid, marker, timeout):
    command = [provider, "--resume" if provider == "claude" else "resume", sid]
    if provider == "codex":
        command.append("--no-daemon")
    print("$", " ".join(command), "[cwd:", cwd, "]", flush=True)
    pid, master = pty.fork()
    if pid == 0:
        fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 140, 0, 0))
        os.chdir(cwd)
        os.execvpe(command[0], command, env)
    fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 140, 0, 0))
    raw = bytearray()
    found = False
    status = None
    forced = False
    queries = {
        b"\x1b[6n": b"\x1b[1;1R",
        b"\x1b[c": b"\x1b[?1;2c",
        b"\x1b[0c": b"\x1b[?1;2c",
        b"\x1b]10;?\x07": b"\x1b]10;rgb:ffff/ffff/ffff\x07",
        b"\x1b]11;?\x07": b"\x1b]11;rgb:0000/0000/0000\x07",
    }

    def pump(wait):
        if not select.select([master], [], [], wait)[0]:
            return
        try:
            chunk = os.read(master, 65536)
        except OSError as error:
            if error.errno == errno.EIO:
                return
            raise
        tail = bytes(raw[-32:])
        probe = tail + chunk
        raw.extend(chunk)
        for query, answer in queries.items():
            start = 0
            while True:
                index = probe.find(query, start)
                if index < 0:
                    break
                if index + len(query) > len(tail):
                    os.write(master, answer)
                start = index + len(query)

    def wait_exit(seconds):
        nonlocal status
        end = time.monotonic() + seconds
        while status is None and time.monotonic() < end:
            pump(0.1)
            ended, code = os.waitpid(pid, os.WNOHANG)
            if ended:
                status = os.waitstatus_to_exitcode(code)

    try:
        started = time.monotonic()
        expanded = False
        while time.monotonic() < started + timeout:
            pump(0.15)
            if marker in text_screen(raw):
                found = True
                break
            ended, code = os.waitpid(pid, os.WNOHANG)
            if ended:
                status = os.waitstatus_to_exitcode(code)
                break
            if provider == "codex" and not expanded and time.monotonic() - started > 5:
                os.write(master, b"\x14")
                expanded = True
    finally:
        try:
            if status is None:
                try:
                    os.write(master, b"\x15/exit\r")
                except OSError:
                    pass
                wait_exit(3)
            if status is None:
                try:
                    os.write(master, b"\x03")
                    time.sleep(0.15)
                    os.write(master, b"\x03")
                except OSError:
                    pass
                wait_exit(3)
            if status is None:
                forced = True
                os.killpg(pid, signal.SIGTERM)
                wait_exit(2)
            if status is None:
                os.killpg(pid, signal.SIGKILL)
                _, code = os.waitpid(pid, 0)
                status = os.waitstatus_to_exitcode(code)
        finally:
            os.close(master)
    screen = text_screen(raw)
    if not found or forced or status not in (0, 130):
        raise RuntimeError(f"{provider}: marker displayed={found}, exit={status}, forced shutdown={forced}\nExpected marker: {marker}\nCaptured screen:\n{screen}")
    index = screen.index(marker)
    print("SCREEN:", screen[max(0, index - 100):index + len(marker)], flush=True)
    print(f"PASS: {marker}; clean exit={status}", flush=True)


def scenario(number, source, target, conversion, root, guard, timeout):
    print(f"Scenario {number}: {source} -> {target}", flush=True)
    home = root / f"case-{number}/source"
    cwd = home / "work"
    env = isolated_env(home, guard)
    seed(home, cwd)
    marker = f"RESUME_CASE_{number}_{uuid.uuid4().hex[:12].upper()}"
    sid = fixture(home, cwd, source, marker)
    if conversion:
        report = json.loads(checked(env, cwd, BINARY, "convert", sid, "--to", target, "--allow-running", "--yes", "--json"))
        if not report.get("written") or report["session_id"] == sid:
            raise RuntimeError(f"Conversion did not create a new session: {report}")
        sid = report["session_id"]
    else:
        archive = root / f"case-{number}/backup.tar.gz"
        backup_report = json.loads(checked(env, cwd, BINARY, "backup", "--provider", source, "--output", archive, "--yes", "--json"))
        if not backup_report.get("files"):
            raise RuntimeError(f"Backup contained no files: {backup_report}")
        shutil.rmtree(home)
        destination = root / f"case-{number}/destination"
        new_cwd = destination / "work"
        env = isolated_env(destination, guard)
        seed(destination, new_cwd)
        report = json.loads(checked(env, new_cwd, BINARY, "restore", archive, "--provider", target, "--dest", destination / ".claude", "--provider-home", f"codex={destination / '.codex'}", "-m", f"{cwd}:{new_cwd}", "--allow-running", "--yes", "--json"))
        if report.get("failed") or not report.get("written"):
            raise RuntimeError(f"Restore failed: {report}")
        cwd = new_cwd
    resume(env, cwd, target, sid, marker, timeout)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--timeout", type=float, default=45)
    parser.add_argument("--scenario", type=int, choices=range(1, 5))
    args = parser.parse_args()
    if not 0 < args.timeout <= 300:
        parser.error("--timeout must be greater than zero and at most 300 seconds")
    if not BINARY.is_file():
        raise RuntimeError("Build target/debug/aisession-backup with cargo build first")
    guard = NetworkGuard()
    try:
        with tempfile.TemporaryDirectory(prefix="aisession-resume-") as temporary:
            root = pathlib.Path(temporary)
            env = isolated_env(root / "versions", guard)
            for tool, expected in (("claude", CLAUDE_VERSION), ("codex", CODEX_VERSION)):
                version = checked(env, root, tool, "--version").strip()
                print(version, flush=True)
                if not re.search(r"\b" + re.escape(expected) + r"\b", version):
                    raise RuntimeError(f"Expected {tool} {expected}, got {version}")
            failures = []
            for number, source, target, conversion in ((1, "codex", "claude", True), (2, "claude", "codex", True), (3, "claude", "claude", False), (4, "codex", "codex", False)):
                if args.scenario and args.scenario != number:
                    continue
                try:
                    scenario(number, source, target, conversion, root, guard, args.timeout)
                except Exception as error:
                    failures.append(number)
                    print(f"FAIL scenario {number}: {error}", file=sys.stderr, flush=True)
            if failures:
                raise RuntimeError(f"Resume scenarios failed: {failures}")
            print("All selected real-CLI resume checks passed", flush=True)
    finally:
        guard.close()
        counts = collections.Counter(f"{method} {path}" for method, path in guard.requests)
        print("Network attempts blocked locally (nothing forwarded):", json.dumps(dict(counts), sort_keys=True), flush=True)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"FAIL: {error}", file=sys.stderr)
        sys.exit(1)
