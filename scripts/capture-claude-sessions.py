#!/usr/bin/env python3
"""Record which Claude Code session runs in which Kova pane, for the next start.

Kova records that mapping itself from now on, but a build that predates the
feature cannot: upgrading would still cost one round of lost conversations, the
very thing the feature prevents. This script closes that gap from the outside.
It reads the live panes through the IPC socket and writes
~/.config/kova/claude-sessions.json, which the new build folds into the restored
session on its first start and then retires.

Run it shortly before quitting Kova: it is a snapshot, so panes opened or moved
afterwards are not in it. Re-running simply overwrites the file.

Delete this script once Kova has been quit at least once from a build that
records sessions on its own.
"""

import json
import os
import socket
import subprocess
import sys

BOOTSTRAP_VERSION = 1
OUTPUT = os.path.expanduser("~/.config/kova/claude-sessions.json")
CLAUDE_SESSIONS = os.path.expanduser("~/.claude/sessions")
# claude normally sits directly under the pane's shell; the extra levels cover
# a wrapper process in between.
MAX_ANCESTRY_DEPTH = 3


def kova_socket_path():
    """The socket of the Kova instance to talk to."""
    if os.environ.get("KOVA_SOCKET"):
        return os.environ["KOVA_SOCKET"]
    candidates = [f"/tmp/{n}" for n in os.listdir("/tmp") if n.startswith("kova-") and n.endswith(".sock")]
    if len(candidates) == 1:
        return candidates[0]
    if not candidates:
        sys.exit("No Kova socket found: is Kova running?")
    sys.exit(f"Several Kova sockets found, set KOVA_SOCKET: {candidates}")


def ipc(request):
    """One JSON request per line; the reply comes back on the same line."""
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(kova_socket_path())
    # The trailing newline matters: the server reads line by line and would
    # otherwise wait forever for the end of the request.
    sock.sendall((json.dumps(request) + "\n").encode())
    chunks = []
    while not b"\n" in b"".join(chunks):
        chunk = sock.recv(65536)
        if not chunk:
            break
        chunks.append(chunk)
    sock.close()
    reply = json.loads(b"".join(chunks).decode())
    if not reply.get("ok", True):
        sys.exit(f"Kova refused the request: {reply.get('error')}")
    return reply["data"]


def parent_map():
    """pid -> (ppid, command name) for every live process."""
    out = subprocess.run(["ps", "-eo", "pid,ppid,comm"], capture_output=True, text=True).stdout
    parents = {}
    for line in out.splitlines()[1:]:
        fields = line.split(None, 2)
        if len(fields) == 3:
            parents[int(fields[0])] = (int(fields[1]), fields[2])
    return parents


def sessions_by_ancestor(parents):
    """ancestor pid -> Claude session id, walking up from each live session."""
    found = {}
    if not os.path.isdir(CLAUDE_SESSIONS):
        return found
    for name in os.listdir(CLAUDE_SESSIONS):
        if not name.endswith(".json"):
            continue
        with open(os.path.join(CLAUDE_SESSIONS, name)) as handle:
            try:
                data = json.load(handle)
            except json.JSONDecodeError:
                continue
        pid, session_id = data.get("pid"), data.get("sessionId")
        if not pid or not session_id:
            continue
        # A stale file left by a killed process would otherwise point at
        # whatever later recycled that pid.
        if parents.get(pid, (None, ""))[1] != "claude":
            continue
        current = pid
        for _ in range(MAX_ANCESTRY_DEPTH):
            parent = parents.get(current, (0, ""))[0]
            if parent <= 1:
                break
            found[parent] = session_id
            current = parent
    return found


def main():
    parents = parent_map()
    by_ancestor = sessions_by_ancestor(parents)

    captured = []
    # Panes come back in the order Kova saves them: windows, then tabs, then
    # columns left to right, panes top to bottom. The index within a tab is
    # what the session file uses to place a pane.
    index_in_tab = {}
    for pane in ipc({"cmd": "list-panes"}):
        key = (pane["window"], pane["tab"])
        index = index_in_tab.get(key, 0)
        index_in_tab[key] = index + 1
        session_id = by_ancestor.get(pane["pid"])
        if not session_id:
            continue
        captured.append({
            "window": pane["window"],
            "tab": pane["tab"],
            "index": index,
            "cwd": pane["cwd"],
            "session_id": session_id,
            # Restored as the pane title, so a pane whose conversation has not
            # been resumed yet still says what it was instead of showing its
            # directory name.
            "title": pane.get("title", ""),
        })

    os.makedirs(os.path.dirname(OUTPUT), exist_ok=True)
    with open(OUTPUT, "w") as handle:
        json.dump({"version": BOOTSTRAP_VERSION, "panes": captured}, handle, indent=2)
        handle.write("\n")

    total_panes = sum(index_in_tab.values())
    print(f"{len(captured)} Claude session(s) captured out of {total_panes} pane(s) -> {OUTPUT}")


if __name__ == "__main__":
    main()
