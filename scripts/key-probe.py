#!/usr/bin/env python3
"""Key-input probe: show, as `od -c` does, exactly what a terminal sends per key.

Puts the tty in raw mode and prints every read() it gets, one line per read, so
one key press (ESC [ A, a composed é, a pasted word) stays on one line.
With --kitty it also pushes the kitty keyboard protocol (disambiguate, flags 1)
the way Claude Code does, which sends modified keys down another path in Kova.

Usage:  python3 key-probe.py <label> [--kitty]
        press keys, then Ctrl+D alone to stop (auto-stops after 10 min).

Writes the trace to ~/Downloads/kova-embedding/key-probe-<label>.log so a run
before and after a refactor can be compared byte for byte (diff the two logs).
"""

import os
import select
import subprocess
import sys
import termios
import time
import tty

TIMEOUT_S = 600.0
KITTY_PUSH = b"\x1b[>1u"
KITTY_POP = b"\x1b[<u"


def od_c(chunk: bytes) -> str:
    out = subprocess.run(["od", "-An", "-c"], input=chunk, capture_output=True).stdout
    return " ".join(out.decode(errors="replace").split())


def main() -> int:
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    kitty = "--kitty" in sys.argv
    label = args[0] if args else "run"
    log_dir = os.path.expanduser("~/Downloads/kova-embedding")
    os.makedirs(log_dir, exist_ok=True)
    log_path = os.path.join(log_dir, f"key-probe-{label}.log")
    fd = sys.stdin.fileno()
    if not os.isatty(fd):
        print("not a tty", file=sys.stderr)
        return 1

    out = sys.stdout.buffer
    saved = termios.tcgetattr(fd)
    log = open(log_path, "w")
    reads = 0

    def emit(line: str) -> None:
        out.write((line + "\r\n").encode())
        out.flush()
        log.write(line + "\n")

    emit(f"key-probe [{label}]{' kitty' if kitty else ''} - press keys, Ctrl+D alone to stop")
    emit(f"log: {log_path}")
    emit("-" * 60)
    if kitty:
        out.write(KITTY_PUSH)
        out.flush()
    start = time.monotonic()
    try:
        tty.setraw(fd)
        while time.monotonic() - start < TIMEOUT_S:
            if not select.select([fd], [], [], 0.2)[0]:
                continue
            chunk = os.read(fd, 4096)
            if not chunk or chunk == b"\x04":
                break
            reads += 1
            emit(f"{reads:4d}  {od_c(chunk)}")
    finally:
        if kitty:
            out.write(KITTY_POP)
            out.flush()
        termios.tcsetattr(fd, termios.TCSADRAIN, saved)
        emit("-" * 60)
        emit(f"total: {reads} reads")
        log.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
