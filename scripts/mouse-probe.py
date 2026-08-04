#!/usr/bin/env python3
"""Mouse-input probe: show exactly what a terminal sends on scroll.

Enables the same mouse modes Claude Code turns on (1000 button, 1002 drag,
1003 any-motion, 1006 SGR encoding), puts the tty in raw mode, and prints every
byte it receives, decoding SGR mouse reports into a readable form.

Usage:  python3 mouse-probe.py <label>
        scroll inside the window, then press q to stop (auto-stops after 60s).

Writes the full trace to ~/Downloads/mouse-probe-<label>.log so two terminals
can be compared byte for byte.
"""

import os
import re
import select
import sys
import termios
import time
import tty

MODES = ("1000", "1002", "1003", "1006")
SGR_RE = re.compile(rb"\x1b\[<(\d+);(\d+);(\d+)([Mm])")
TIMEOUT_S = 60.0

BUTTONS = {0: "left", 1: "middle", 2: "right", 3: "release", 64: "wheel-up", 65: "wheel-down"}


def describe(code: int) -> str:
    base = code & ~(4 | 8 | 16 | 32)
    name = BUTTONS.get(base, f"btn{base}")
    if code & 32 and base not in (64, 65):
        name += "+motion"
    mods = [m for bit, m in ((4, "shift"), (8, "alt"), (16, "ctrl")) if code & bit]
    return name + ("[" + "+".join(mods) + "]" if mods else "")


def main() -> int:
    label = sys.argv[1] if len(sys.argv) > 1 else "run"
    log_path = os.path.expanduser(f"~/Downloads/mouse-probe-{label}.log")
    fd = sys.stdin.fileno()
    if not os.isatty(fd):
        print("not a tty", file=sys.stderr)
        return 1

    out = sys.stdout.buffer
    saved = termios.tcgetattr(fd)
    log = open(log_path, "w")
    events = 0
    raw_bytes = 0
    start = time.monotonic()
    last = start

    def emit(line: str) -> None:
        out.write((line + "\r\n").encode())
        out.flush()
        log.write(line + "\n")

    emit(f"mouse-probe [{label}] — scroll here, press q to stop (auto-stop 60s)")
    emit(f"log: {log_path}")
    emit("-" * 70)
    out.write(b"".join(f"\x1b[?{m}h".encode() for m in MODES))
    out.flush()
    try:
        tty.setraw(fd)
        pending = b""
        while time.monotonic() - start < TIMEOUT_S:
            if not select.select([fd], [], [], 0.2)[0]:
                continue
            chunk = os.read(fd, 4096)
            if not chunk:
                break
            if b"q" in chunk:
                break
            raw_bytes += len(chunk)
            now = time.monotonic()
            gap_ms = (now - last) * 1000
            last = now
            pending += chunk
            reports = list(SGR_RE.finditer(pending))
            if reports:
                for m in reports:
                    events += 1
                    code, col, row, kind = int(m[1]), int(m[2]), int(m[3]), m[4].decode()
                    emit(
                        f"+{now - start:7.3f}s  gap {gap_ms:7.1f}ms  chunk {len(chunk):4d}B  "
                        f"SGR <{code};{col};{row}{kind}  {describe(code)}"
                        f"{' press' if kind == 'M' else ' release'}"
                    )
                pending = pending[reports[-1].end():]
            else:
                emit(f"+{now - start:7.3f}s  gap {gap_ms:7.1f}ms  raw {chunk!r}")
                pending = pending[-32:]
    finally:
        termios.tcsetattr(fd, termios.TCSADRAIN, saved)
        out.write(b"".join(f"\x1b[?{m}l".encode() for m in MODES))
        out.flush()
        emit("-" * 70)
        emit(f"total: {events} mouse reports, {raw_bytes} bytes in {time.monotonic() - start:.1f}s")
        log.close()
        print(f"\ntrace written to {log_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
