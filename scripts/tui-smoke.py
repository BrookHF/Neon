#!/usr/bin/env python3
"""Drive the Neon TUI on a pty and check what it renders. Needs no API key.

  python3 scripts/tui-smoke.py

Enter arrives as carriage return, like a real terminal. Ctrl+C aborts a run, or quits when idle.
"""
import json
import os
import pty
import re
import select
import struct
import subprocess
import sys
import termios
import time
import fcntl

ESC = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]")
REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(REPO, "target", "debug", "neon")
SESSIONS = os.path.join(os.path.expanduser("~"), ".local", "state", "neon", "sessions")


def session_files():
    if not os.path.isdir(SESSIONS):
        return set()
    return {f for f in os.listdir(SESSIONS) if f.endswith(".jsonl")}


def chunk(parts, finish="STOP"):
    return {"candidates": [{"content": {"parts": parts}, "finish_reason": finish}]}


def call(cmd):
    return {"functionCall": {"name": "bash", "args": {"command": cmd}}}


def write_fixture(path, rows):
    with open(path, "w") as f:
        for row in rows:
            f.write(json.dumps(row) + "\n")


def open_latest_session():
    """The newest session log neon wrote, for checks that read the file."""
    files = sorted(session_files())
    if not files:
        return ""
    return open(os.path.join(SESSIONS, files[-1])).read()


def screen(args, keys, wait, env=None):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
    child_env = os.environ.copy()
    # The wasmi debug interpreter needs a deep host stack. cargo sets RUST_MIN_STACK
    # from .cargo/config.toml only for processes it launches, so a CI step that runs
    # this script directly would not pass it to neon. Set it here.
    child_env.setdefault("RUST_MIN_STACK", "8000000")
    # GitHub Actions runners report TERM=unknown; give neon a real terminal type.
    child_env.setdefault("TERM", "xterm-256color")
    if env:
        child_env.update(env)
    proc = subprocess.Popen([BIN] + args, stdin=slave, stdout=slave, stderr=slave, cwd=REPO, env=child_env)
    out = bytearray()

    def drain(seconds):
        end = time.time() + seconds
        while time.time() < end:
            readable, _, _ = select.select([master], [], [], 0.2)
            if master in readable:
                try:
                    out.extend(os.read(master, 4096))
                except OSError:
                    break

    drain(0.6)
    for key, delay in keys:
        time.sleep(delay)
        os.write(master, key)
        drain(0.3)
    drain(wait)
    os.write(master, b"\x03")
    try:
        proc.wait(timeout=6)
    except subprocess.TimeoutExpired:
        proc.kill()
    os.close(slave)
    os.close(master)
    return ESC.sub("", out.decode("utf-8", "replace")), proc.returncode


def norm(text):
    """Drop ANSI sequences and all whitespace, so substring checks survive diff frames."""
    return re.sub(r"\s+", "", ESC.sub("", text))


def main():
    results = []
    before = session_files()

    s, rc = screen(["--fixture", "fixtures/demo.jsonl"], [(b"say hello\r", 0.2)], 2.5)
    n = norm(s)
    results.append(("turn renders cards", "echohellofromneon" in n and "->bash" in n))
    results.append(("reply shown", "Itprintedhellofromneon" in n))
    results.append(("status bar", "tok" in n and "2steps" in n))

    # The turn appended a JSONL session log: meta, user, assistant, tool_result.
    logged = False
    for f in session_files() - before:
        text = open(os.path.join(SESSIONS, f)).read()
        logged = logged or (
            '"k":"meta"' in text
            and '"k":"user"' in text
            and '"k":"assistant"' in text
            and '"k":"tool_result"' in text
        )
    results.append(("session logged", logged))

    # --resume replays the log before the new turn starts.
    s, rc = screen(
        ["--fixture", "fixtures/demo.jsonl", "--resume", "last"],
        [(b"say hello\r", 0.2)],
        2.5,
    )
    n = norm(s)
    results.append(
        ("resume replays history", "resumed4messages" in n and "Itprintedhellofromneon" in n)
    )

    # Profile rows load at the turn boundary; /plugin list shows them after the turn.
    profiles = os.path.join(os.path.expanduser("~"), ".config", "neon", "profiles")
    os.makedirs(profiles, exist_ok=True)
    smoke = os.path.join(profiles, "smoke.yaml")
    with open(smoke, "w") as f:
        f.write(
            "rows:\n  - id: echo\n"
            f"    plugin: {os.path.join(REPO, 'fixtures', 'plugins', 'echo')}\n"
            "    grants: [log, clock.now]\n"
        )
    s, rc = screen(
        ["--fixture", "fixtures/demo.jsonl", "--profile", "smoke"],
        [(b"say hello\r", 0.2), (b"/plugin list\r", 2.2)],
        1.5,
    )
    n = norm(s)
    results.append(("profile row loaded", "loaded:echo" in n))
    results.append(("plugin count in status", "1plugins" in n))
    os.remove(smoke)

    # Long replies must wrap inside the window, not be cut at the first line.
    long = os.path.join(REPO, "fixtures", "long-tmp.jsonl")
    write_fixture(long, [chunk([{"text": "word " * 40 + "ENDWORD"}])])
    s, rc = screen(["--fixture", long], [(b"go\r", 0.2)], 2.5)
    n = norm(s)
    results.append(("long reply wraps in window", "ENDWORD" in n))
    os.remove(long)

    # The cursor moves with arrow keys and edits mid-line.
    s, rc = screen(
        ["--fixture", "fixtures/demo.jsonl"],
        [(b"ab cd", 0.2), (b"\x1b[D", 0.15), (b"\x1b[D", 0.15), (b"x", 0.15), (b"\r", 0.3)],
        2.5,
    )
    n = norm(s)
    results.append(("cursor edit mid-line", ">abxcd" in n))

    slow = os.path.join(REPO, "fixtures", "slow-tmp.jsonl")
    write_fixture(
        slow,
        [
            chunk([call("sleep 2; echo one"), call("sleep 2; echo two"), call("sleep 2; echo three")]),
            chunk([{"text": "stopped early"}]),
        ],
    )
    s, rc = screen(
        ["--fixture", slow],
        [(b"run all\r", 0.2), (b"halt\r", 1.2)],
        6.0,
    )
    n = norm(s)
    results.append(("steering ran the first call", "echoone" in n))
    results.append(("steering dropped the rest", "echotwo" not in n and "echothree" not in n))
    results.append(("steering shown in transcript", ">halt" in n))
    results.append(("next step answered", "stoppedearly" in n))
    os.remove(slow)

    write_fixture(
        slow,
        [chunk([call("sleep 3; echo a"), call("sleep 3; echo b")]), chunk([{"text": "never reached"}])],
    )
    s, rc = screen(["--fixture", slow], [(b"go\r", 0.2), (b"\x03", 1.0)], 3.0)
    n = norm(s)
    results.append(("abort note shown", "#aborted" in n))
    results.append(("abort killed the in-flight tool", "exit:-1" in n and "[error]" in n))
    results.append(("abort dropped later calls", "echob" not in n))
    os.remove(slow)

    # /help lists the keys and every command; PgUp scrolls the transcript back.
    write_fixture(long, [chunk([{"text": "word " * 60 + "ENDWORD"}])])
    s, rc = screen(
        ["--fixture", long],
        [(b"go\r", 0.2), (b"/help\r", 1.5), (b"\x1b[5~", 0.3)],
        2.0,
    )
    n = norm(s)
    results.append(("help shows keys and commands", "/help" in n and "/compact" in n and "PgUp" in n))
    results.append(("scrollback indicator shows", "scrolled" in n and "\u2191" in n))
    os.remove(long)

    # /tools names the tools and where each one came from; Tab completes a command.
    s, rc = screen(
        ["--fixture", "fixtures/demo.jsonl"],
        [(b"/tools\r", 0.3), (b"/pl", 0.2), (b"\t", 0.3), (b"\r", 0.5)],
        2.0,
    )
    n = norm(s)
    results.append(("tools lists bash with its source", "bash[core]" in n))
    results.append(("tab completes the command", "> /plugin".replace(" ", "") in n and "nopluginsloaded" in n))

    # /model changes the model at once, and the status bar shows the new one.
    s, rc = screen(
        ["--fixture", "fixtures/demo.jsonl"],
        [(b"/model gemini-3.5-flash\r", 0.4), (b"go\r", 0.2)],
        2.5,
    )
    n = norm(s)
    results.append(("model change shows in status", "->gemini-3.5-flash" in n and "gemini-3.5-flash\u00b7" in n))
    results.append(("model change logged", "model_change" in open_latest_session()))

    # /hist reports the size and the budget share; /sessions lists saved sessions.
    s, rc = screen(
        ["--fixture", "fixtures/demo.jsonl"],
        [(b"say hello\r", 0.2), (b"/hist\r", 2.2), (b"/sessions\r", 0.5)],
        2.0,
    )
    n = norm(s)
    results.append(("hist shows tokens and budget", "messages" in n and "budget" in n and "%" in n))
    results.append(("sessions lists saved sessions", "sessionsin" in n and "/resume" in n))

    # A mistyped command says what was meant, in red rather than as a silent failure.
    s, rc = screen(
        ["--fixture", "fixtures/demo.jsonl"],
        [(b"/hepl\r", 0.4)],
        1.5,
    )
    n = norm(s)
    results.append(("unknown command suggests one", "!unknowncommand:hepl" in n and "didyoumean/help" in n))

    # /quit leaves on its own, so a person does not need Ctrl+C.
    s, rc = screen(["--fixture", "fixtures/demo.jsonl"], [(b"/quit\r", 0.5)], 1.5)
    results.append(("quit command exits clean", rc == 0))

    # First run with no config file: neon writes a starter file and says where it is.
    home = os.path.join(REPO, "tmp-first-home")
    import shutil
    shutil.rmtree(home, ignore_errors=True)
    os.makedirs(home)
    s, rc = screen(["--plain"], [(b":help\r", 0.5), (b":quit\r", 0.5)], 1.5, env={"HOME": home})
    cfg = os.path.join(home, ".config", "neon", "config.yaml")
    results.append(("first run writes the starter config", os.path.isfile(cfg) and "created" in norm(s)))
    if os.path.isfile(cfg):
        results.append(("starter config is mode 0600", (os.stat(cfg).st_mode & 0o777) == 0o600))
        text = open(cfg).read()
        results.append(("starter config names api_key", "api_key" in text and "apikey" in text))
    results.append(("plain help lists commands", ":help" in norm(s) and ":compact" in norm(s)))
    shutil.rmtree(home, ignore_errors=True)

    failed = 0
    for name, ok in results:
        print(f"{name}: {'PASS' if ok else 'FAIL'}")
        if not ok:
            failed += 1
            print(f"  screen tail: {norm(s)[-300:]}")
    for f in session_files() - before:
        os.remove(os.path.join(SESSIONS, f))
    print("all checks passed" if failed == 0 else f"{failed} check(s) failed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
