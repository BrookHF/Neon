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


def chunk(parts, finish="STOP"):
    return {"candidates": [{"content": {"parts": parts}, "finish_reason": finish}]}


def call(cmd):
    return {"functionCall": {"name": "bash", "args": {"command": cmd}}}


def write_fixture(path, rows):
    with open(path, "w") as f:
        for row in rows:
            f.write(json.dumps(row) + "\n")


def screen(args, keys, wait):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
    proc = subprocess.Popen([BIN] + args, stdin=slave, stdout=slave, stderr=slave, cwd=REPO)
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

    s, rc = screen(["--fixture", "fixtures/demo.jsonl"], [(b"say hello\r", 0.2)], 2.5)
    n = norm(s)
    results.append(("turn renders cards", "echohellofromneon" in n and "->bash" in n))
    results.append(("reply shown", "Itprintedhellofromneon" in n))
    results.append(("status bar", "tokens" in n and "2steps" in n))

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
    results.append(("abort dropped later calls", "echob" not in n))
    os.remove(slow)

    failed = 0
    for name, ok in results:
        print(f"{name}: {'PASS' if ok else 'FAIL'}")
        if not ok:
            failed += 1
    print("all checks passed" if failed == 0 else f"{failed} check(s) failed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
