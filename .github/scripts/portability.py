import json
import os
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile

SOURCES = {
    "linux": "/home/u/app",
    "macos": "/Users/u/app",
    "windows": "C:\\Users\\u\\app",
    "wsl": "/mnt/c/Users/u/app",
}
TARGETS = ("linux", "macos", "windows")
SESSION = "11111111-1111-4111-8111-111111111111"
ROLLOUT = "sessions/2026/01/01/rollout-2026-01-01T00-00-00-22222222-2222-4222-8222-222222222222.jsonl"
PASSPHRASE = "portability-check"
BINARY = pathlib.Path("target/debug/aisession-backup" + (".exe" if os.name == "nt" else "")).resolve()


def encode(cwd):
    return re.sub(r"[^A-Za-z0-9]", "-", cwd)


def child(cwd, name):
    return f"{cwd}{chr(92) if chr(92) in cwd else '/'}{name}"


def run(root, *args):
    env = dict(os.environ)
    env.update(
        HOME=str(root),
        USERPROFILE=str(root),
        CLAUDE_CONFIG_DIR=str(root / ".claude"),
        CODEX_HOME=str(root / ".codex"),
        MAESTRI_HOME=str(root / ".maestri"),
        AISESSION_BACKUP_DATA_DIR=str(root / "data"),
        AISESSION_BACKUP_PASSPHRASE=PASSPHRASE,
        NO_COLOR="1",
    )
    return subprocess.run(
        [str(BINARY), *args],
        env=env,
        cwd=root,
        stdin=subprocess.DEVNULL,
        capture_output=True,
        text=True,
        encoding="utf-8",
    )


def checked(root, *args):
    result = run(root, *args)
    if result.returncode != 0:
        raise SystemExit(f"{args[0]} exit {result.returncode}\n{result.stdout}\n{result.stderr}")
    return result


def write(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8", newline="\n")


def session_line(cwd, content):
    return json.dumps(
        {
            "type": "user",
            "cwd": cwd,
            "sessionId": SESSION,
            "timestamp": "2026-01-01T00:00:00Z",
            "message": {"role": "user", "content": content},
        }
    )


def fixture(root, cwd):
    project = root / ".claude" / "projects" / encode(cwd)
    write(
        project / f"{SESSION}.jsonl",
        session_line(cwd, "first") + "\n" + session_line(child(cwd, "sub"), "second") + "\n",
    )
    write(project / SESSION / "tool-results" / "result.txt", "tool output")
    meta = {"type": "session_meta", "payload": {"id": "22222222", "cwd": cwd}}
    write(root / ".codex" / ROLLOUT, json.dumps(meta) + "\n")


def create(out_dir, runner):
    out_dir.mkdir(parents=True, exist_ok=True)
    names = [runner] + (["wsl"] if runner == "linux" else [])
    for name in names:
        root = pathlib.Path(tempfile.mkdtemp())
        fixture(root, SOURCES[name])
        archive = out_dir / f"{name}.tar.gz.age"
        checked(root, "backup", "--provider", "claude,codex", "--encrypt", "-o", str(archive), "--yes")
        shutil.rmtree(root, ignore_errors=True)
        print(f"created {archive.name}")


def field_cwds(path, pick):
    return [pick(json.loads(line)) for line in path.read_text(encoding="utf-8").splitlines()]


def restore_one(archive, source_name, target_name):
    source = SOURCES[source_name]
    target = SOURCES[target_name]
    root = pathlib.Path(tempfile.mkdtemp())
    arguments = [
        "restore",
        str(archive),
        "--dest",
        str(root / ".claude"),
        "--provider-home",
        f"codex={root / '.codex'}",
        "--allow-running",
    ]
    if source != target:
        arguments += ["-m", f"{source}:{target}"]

    checked(root, "verify", str(archive))
    checked(root, *arguments, "--dry-run")
    if (root / ".claude").exists():
        raise SystemExit("dry-run wrote files")

    report = json.loads(checked(root, *arguments, "--yes", "--json").stdout)
    if report["failed"] or not report["written"]:
        raise SystemExit(f"restore report {report}")

    project = root / ".claude" / "projects" / encode(target)
    session = project / f"{SESSION}.jsonl"
    expected = [target, child(target, "sub")]
    found = field_cwds(session, lambda value: value["cwd"])
    if found != expected:
        raise SystemExit(f"session cwd {found} != {expected}")
    if (project / SESSION / "tool-results" / "result.txt").read_text(encoding="utf-8") != "tool output":
        raise SystemExit("tool result mismatch")
    found = field_cwds(root / ".codex" / ROLLOUT, lambda value: value["payload"]["cwd"])
    if found != [target]:
        raise SystemExit(f"codex cwd {found} != {target}")

    checked(root, "rollback", report["journal_id"], "--yes")
    if session.exists():
        raise SystemExit("rollback left the session")
    shutil.rmtree(root, ignore_errors=True)


def restore(in_dir, runner):
    archives = sorted(in_dir.glob("*.tar.gz.age"))
    if {path.name.split(".")[0] for path in archives} != set(SOURCES):
        raise SystemExit(f"missing archives: {[path.name for path in archives]}")
    for archive in archives:
        source_name = archive.name.split(".")[0]
        restore_one(archive, source_name, runner)
        print(f"ok {source_name} -> {runner}")


def main():
    command, directory, runner = sys.argv[1:4]
    if runner not in TARGETS:
        raise SystemExit(f"unknown runner {runner}")
    {"create": create, "restore": restore}[command](pathlib.Path(directory).resolve(), runner)


if __name__ == "__main__":
    main()
