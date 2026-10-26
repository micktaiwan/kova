#!/usr/bin/env python3
"""Search the conversations of the Claude Code sessions open in Kova panes.

Usage: grep-sessions.py PATTERN [--all] [--json]

PATTERN is a case-insensitive Python regex. Only what a person or Claude actually wrote is
searched: user prompts and assistant text blocks. Task notifications, messages from other
sessions, compaction summaries, skill bodies and tool results are skipped, since they repeat
the same words in every session and drown the signal.

Per matching pane: the count of user and assistant hits, every user hit, and the last 3
assistant hits (--all prints every one). Exit 1 if nothing matched, 2 on error.
"""
import glob
import json
import os
import re
import socket
import sys

NOISE = ("<task-notification>", "<teammate-message", "<cross-session-message",
         "Another Claude session sent a message", "This session is being continued",
         "<command-message>", "Base directory for this skill", "Stop hook feedback",
         "<system-reminder>")


def list_panes():
    path = os.environ.get("KOVA_SOCKET")
    if not path:
        sys.exit("KOVA_SOCKET is not set: run this from a Kova pane.")
    s = socket.socket(socket.AF_UNIX)
    s.connect(path)
    s.sendall(b'{"cmd":"list-panes"}')
    s.shutdown(socket.SHUT_WR)
    buf = b""
    while chunk := s.recv(65536):
        buf += chunk
    return json.loads(buf)["data"]


def transcript(session_id, cwd, config_dir=None):
    # A session started under its own CLAUDE_CONFIG_DIR (a launcher like deepseek-cc) keeps
    # its transcripts under that directory, not under ~/.claude.
    root = config_dir or os.path.expanduser("~/.claude")
    d = re.sub(r"[^A-Za-z0-9]", "-", cwd or "")
    p = os.path.join(root, "projects", d, f"{session_id}.jsonl")
    if os.path.exists(p):
        return p
    found = glob.glob(os.path.join(root, "projects", "*", f"{session_id}.jsonl"))
    return found[0] if found else None


def texts(path):
    """Yield (role, timestamp, text) for what was actually written."""
    for line in open(path, errors="ignore"):
        try:
            o = json.loads(line)
        except ValueError:
            continue
        role, content = o.get("type"), o.get("message", {}).get("content")
        if role == "user":
            if isinstance(content, str) and not any(n in content for n in NOISE):
                yield role, o.get("timestamp", ""), content
        elif role == "assistant" and isinstance(content, list):
            for c in content:
                if isinstance(c, dict) and c.get("type") == "text":
                    yield role, o.get("timestamp", ""), c["text"]


def snippet(text, m):
    s = text[max(0, m.start() - 100):m.end() + 140]
    return " ".join(s.split())


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    if len(args) != 1:
        print(__doc__, file=sys.stderr)
        sys.exit(2)
    pat = re.compile(args[0], re.I)
    show_all, as_json = "--all" in sys.argv, "--json" in sys.argv
    me = os.environ.get("CLAUDE_SESSION_ID") or os.environ.get("CLAUDE_CODE_SESSION_ID")
    results, missing, searched = [], [], 0
    for p in list_panes():
        sid = p.get("agent_session_id") or p.get("claude_session_id")
        if not sid or sid == me:
            continue
        path = transcript(sid, p.get("cwd"), p.get("claude_config_dir"))
        if not path:
            missing.append(p["id"])
            continue
        searched += 1
        hits = {"user": [], "assistant": []}
        for role, ts, text in texts(path):
            m = pat.search(text)
            if m:
                hits[role].append({"time": ts[:16], "text": snippet(text, m)})
        if hits["user"] or hits["assistant"]:
            results.append({"pane": p["id"], "name": p.get("agent_session_name") or p.get("title"),
                            "session_id": sid, **hits})
    if as_json:
        print(json.dumps(results, ensure_ascii=False, indent=1))
    else:
        for r in results:
            print(f"#{r['pane']} {r['name']}: {len(r['user'])} user, {len(r['assistant'])} assistant")
            for h in r["user"]:
                print(f"  user {h['time']}  {h['text']}")
            shown = r["assistant"] if show_all else r["assistant"][-3:]
            if len(shown) < len(r["assistant"]):
                print(f"  (last {len(shown)} of {len(r['assistant'])} assistant hits, --all for every one)")
            for h in shown:
                print(f"  claude {h['time']}  {h['text']}")
        print(f"{len(results)} of {searched} session(s) matched"
              + (f"; no transcript for pane(s) {missing}" if missing else ""))
    sys.exit(0 if results else 1)


if __name__ == "__main__":
    main()
