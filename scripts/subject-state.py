#!/usr/bin/env python3
"""Where every other conversation of this pane's Kova subject stands.

Usage: subject-state.py [--pane N] [--json]
       subject-state.py --clear-raf SESSION_ID... [--into SESSION_ID]

The subject is the one the calling pane ($KOVA_PANE_ID, or --pane N) belongs to. For every
other pane of that subject: its conversation (agent, session id, name, directory), whether it
is working right now, its last activity, its last user prompt, the last <recap> its answers
ended with, its <raf> list (~/.claude/raf/<session>.json, under its CLAUDE_CONFIG_DIR when it
has one), and the transcript path and size for a full read.

A pane with no conversation name is named after the first prompt of its conversation, so the
name still says which pane an item came from once that pane is closed. Times are local.

--clear-raf empties the <raf> list of the given sessions once their items have been taken over
by another pane, so that Kova no longer warns about them when their panes are closed. Kova reads
only the `items` field (src/raf.rs); the file keeps a `merged_into` note of where they went.

Exit 1 if the pane belongs to no subject or is alone in it, 2 on error.
"""
import datetime
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
NAME_MAX = 60


def local_time(stamp):
    """Transcript timestamps are UTC ISO strings; show them in local time."""
    try:
        t = datetime.datetime.fromisoformat(stamp.replace("Z", "+00:00"))
    except (AttributeError, ValueError):
        return stamp
    return t.astimezone().strftime("%Y-%m-%d %H:%M")


def prompt_text(content):
    """A user prompt is a string, or a list of blocks when it carries an image or a paste.
    Tool results come back as user messages too and are not prompts."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        if any(isinstance(c, dict) and c.get("type") == "tool_result" for c in content):
            return None
        texts = [c.get("text", "") for c in content if isinstance(c, dict) and c.get("type") == "text"]
        return "\n".join(texts) if texts else None
    return None


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
    """First and last real user prompt, last recap and last timestamp of a transcript."""
    first_prompt, last_prompt, recap, last_time = None, None, None, None
    for line in open(path, errors="ignore"):
        try:
            o = json.loads(line)
        except ValueError:
            continue
        last_time = o.get("timestamp") or last_time
        role, content = o.get("type"), o.get("message", {}).get("content")
        text = prompt_text(content) if role == "user" and not o.get("isSidechain") else None
        if text and text.strip() and not any(n in text for n in NOISE):
            last_prompt = text.strip()
            first_prompt = first_prompt or last_prompt
        elif role == "assistant" and isinstance(content, list):
            for c in content:
                if isinstance(c, dict) and c.get("type") == "text":
                    m = RECAP.findall(c["text"])
                    if m:
                        recap = " ".join(m[-1].split())
    return first_prompt, last_prompt, recap, last_time


def clear_raf(sessions, into):
    root = os.path.expanduser("~/.claude/raf")
    for sid in sessions:
        if not re.fullmatch(r"[A-Za-z0-9_-]+", sid):
            print(f"{sid}: not a session id, skipped.", file=sys.stderr)
            continue
        path = os.path.join(root, f"{sid}.json")
        try:
            with open(path) as f:
                data = json.load(f)
        except (OSError, ValueError) as e:
            print(f"{sid}: no readable raf file ({e}), skipped.", file=sys.stderr)
            continue
        n = len(data.get("items", []))
        data["items"] = []
        data["merged_into"] = into
        data["updated_at"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        with open(path, "w") as f:
            json.dump(data, f, ensure_ascii=False, indent=1)
        print(f"{sid}: {n} item(s) cleared" + (f", merged into {into}" if into else ""))


def main():
    if "--clear-raf" in sys.argv:
        args = sys.argv[sys.argv.index("--clear-raf") + 1:]
        into = None
        if "--into" in args:
            i = args.index("--into")
            into = args[i + 1] if i + 1 < len(args) else None
            args = args[:i] + args[i + 2:]
        if not args:
            print("--clear-raf needs at least one session id.", file=sys.stderr)
            sys.exit(2)
        clear_raf(args, into)
        return
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
        r = {"pane": pid, "name": p.get("agent_session_name"),
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
                first, prompt, recap, last = scan(path)
                if not r["name"] and first:
                    first = " ".join(first.split())
                    r["name"] = first[:NAME_MAX] + ("…" if len(first) > NAME_MAX else "")
                cut = prompt is not None and len(prompt) > PROMPT_MAX
                r.update(transcript=path, transcript_bytes=os.path.getsize(path), last_activity=local_time(last),
                         last_prompt=prompt[:PROMPT_MAX] + " (cut)" if cut else prompt,
                         recap=recap, raf=read_raf(sid, cfg))
        out.append(r)
    for r in out:
        r["name"] = r["name"] or os.path.basename((r["cwd"] or "").rstrip("/")) or "(unnamed)"
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
