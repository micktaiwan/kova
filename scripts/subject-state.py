#!/usr/bin/env python3
"""Where every other conversation of this pane's Kova subject stands.

Usage: subject-state.py [--pane N] [--json]

The subject is the one the calling pane ($KOVA_PANE_ID, or --pane N) belongs to. For every
other pane of that subject: its conversation (agent, session id, name, directory), whether it
is working right now, its last activity, its last user prompt, the last <recap> its answers
ended with, its <raf> list (~/.claude/raf/<session>.json, under its CLAUDE_CONFIG_DIR when it
has one), and the transcript path and size for a full read.

Exit 1 if the pane belongs to no subject or is alone in it, 2 on error.
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
RECAP = re.compile(r"<recap>(.*?)(?:</recap>|$)", re.S)
PROMPT_MAX = 400


def ask(cmd):
    path = os.environ.get("KOVA_SOCKET")
    if not path:
        print("KOVA_SOCKET is not set: run this from a Kova pane.", file=sys.stderr)
        sys.exit(2)
    s = socket.socket(socket.AF_UNIX)
    s.connect(path)
    s.sendall(json.dumps({"cmd": cmd}).encode())
    s.shutdown(socket.SHUT_WR)
    buf = b""
    while chunk := s.recv(65536):
        buf += chunk
    return json.loads(buf)["data"]


def transcript(session_id, cwd, config_dir):
    root = config_dir or os.path.expanduser("~/.claude")
    d = re.sub(r"[^A-Za-z0-9]", "-", cwd or "")
    p = os.path.join(root, "projects", d, f"{session_id}.jsonl")
    if os.path.exists(p):
        return p
    found = glob.glob(os.path.join(root, "projects", "*", f"{session_id}.jsonl"))
    return found[0] if found else None


def read_raf(session_id, config_dir):
    root = config_dir or os.path.expanduser("~/.claude")
    try:
        with open(os.path.join(root, "raf", f"{session_id}.json")) as f:
            return [{"kind": i.get("kind"), "text": i.get("text")} for i in json.load(f).get("items", [])]
    except (OSError, ValueError):
        return None


def scan(path):
    """Last real user prompt, last recap and last timestamp of a transcript."""
    last_prompt, recap, last_time = None, None, None
    for line in open(path, errors="ignore"):
        try:
            o = json.loads(line)
        except ValueError:
            continue
        last_time = o.get("timestamp") or last_time
        role, content = o.get("type"), o.get("message", {}).get("content")
        if role == "user" and isinstance(content, str) and not any(n in content for n in NOISE):
            last_prompt = content.strip()
        elif role == "assistant" and isinstance(content, list):
            for c in content:
                if isinstance(c, dict) and c.get("type") == "text":
                    m = RECAP.findall(c["text"])
                    if m:
                        recap = " ".join(m[-1].split())
    return last_prompt, recap, last_time


def main():
    as_json = "--json" in sys.argv
    me = os.environ.get("KOVA_PANE_ID")
    if "--pane" in sys.argv:
        me = sys.argv[sys.argv.index("--pane") + 1]
    if not me or not me.isdigit():
        print("No pane id: set KOVA_PANE_ID or pass --pane N.", file=sys.stderr)
        sys.exit(2)
    me = int(me)
    subject = next((s for s in ask("list-subjects") if me in s["pane_ids"]), None)
    if not subject:
        print(f"Pane {me} belongs to no subject.", file=sys.stderr)
        sys.exit(1)
    others = [i for i in subject["pane_ids"] if i != me]
    if not others:
        print(f"Pane {me} is alone in subject {subject['name']!r}.", file=sys.stderr)
        sys.exit(1)
    panes = {p["id"]: p for p in ask("list-panes")}
    out = []
    for pid in others:
        p = panes.get(pid, {})
        sid = p.get("agent_session_id") or p.get("claude_session_id") or p.get("dormant_session_id")
        cfg = p.get("claude_config_dir")
        r = {"pane": pid, "name": p.get("agent_session_name") or p.get("title"),
             "agent": p.get("agent"), "config_dir": cfg, "cwd": p.get("cwd"), "session_id": sid,
             "state": "asleep" if p.get("dormant_session_id") else
                      "working" if p.get("working") else "idle"}
        if not sid:
            r["error"] = "no conversation in this pane"
        elif r["agent"] == "codex":
            r["error"] = "Codex session: transcript not read by this script"
        else:
            path = transcript(sid, p.get("cwd"), cfg)
            if not path:
                r["error"] = "transcript not found"
            else:
                prompt, recap, last = scan(path)
                cut = prompt is not None and len(prompt) > PROMPT_MAX
                r.update(transcript=path, transcript_bytes=os.path.getsize(path), last_activity=last,
                         last_prompt=prompt[:PROMPT_MAX] + " (cut)" if cut else prompt,
                         recap=recap, raf=read_raf(sid, cfg))
        out.append(r)
    if as_json:
        print(json.dumps({"subject": subject["name"], "me": me, "panes": out}, ensure_ascii=False, indent=1))
        return
    print(f"Subject {subject['name']!r}: pane {me} + {len(others)} other(s)")
    for r in out:
        flavor = f", {os.path.basename(r['config_dir'].rstrip('/'))}" if r["config_dir"] else ""
        print(f"\n#{r['pane']} {r['name']} ({r['agent'] or 'no agent'}{flavor}, {r['state']})")
        print(f"  cwd: {r['cwd']}  session: {r['session_id']}")
        if "error" in r:
            print(f"  ! {r['error']}")
            continue
        print(f"  transcript: {r['transcript']} ({r['transcript_bytes']} bytes), last activity {r['last_activity']}")
        print(f"  last prompt: {r['last_prompt']}")
        print(f"  recap: {r['recap'] or '(none)'}")
        if r["raf"] is None:
            print("  raf: (no file)")
        elif not r["raf"]:
            print("  raf: (empty)")
        else:
            print("  raf:")
            for i in r["raf"]:
                print(f"    - {i['kind']}: {i['text']}")


if __name__ == "__main__":
    main()
