//! `kova ui …`: the client of pane UI views (`docs/pane-ui-spec.md` § 5).
//! It speaks the `ui-*` socket commands, filling the asker and the pane from
//! its environment like `kova ask`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub const USAGE: &str = "\
usage: kova ui show [--placement dock-bottom|dock-right|pane-right|pane-down] [--size 0.4] --view <file|->
       kova ui update <view_id> --view <file|->
       kova ui card <view_id> <card_id> (--card <file|-> | --remove)
       kova ui wait <view_id> [--timeout SECONDS]
       kova ui close <view_id>
       (each takes --socket <path>; default $KOVA_SOCKET, else the running Kova.app)

Shows a native view in this agent's Kova pane: a list of decision cards (docs/pane-ui-spec.md,
docs/decision-cards-spec.md). `show` prints {\"view_id\",\"pane_id\"}. A click reaches you through
`kova ui wait` (one event as JSON on stdout, exit 0; exit 2 on timeout, default 600 s) or, when
nothing waits, as a line typed into this conversation:
  [Kova] UI \"<view>\", card \"<card>\" (v<n>): <action>[: <text>] [block <i> … : <file> sha256 <hex>]
The user can edit a card's `code` blocks: send the block file a click names, never your own copy.
After acting on a click, update or remove that card (`kova ui card`): its buttons stay locked
until then (10 minutes at most).";

struct Args {
    words: Vec<String>,
    placement: Option<String>,
    size: Option<f64>,
    view: Option<String>,
    card: Option<String>,
    remove: bool,
    timeout: Option<u64>,
    socket: Option<String>,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut out = Args { words: Vec::new(), placement: None, size: None, view: None, card: None, remove: false, timeout: None, socket: None };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().cloned().ok_or_else(|| format!("kova ui: {} needs a value", a));
        match a.as_str() {
            "--placement" => out.placement = Some(value()?),
            "--size" => {
                let raw = value()?;
                out.size = Some(raw.parse().map_err(|_| format!("kova ui: --size takes a number, got '{}'", raw))?);
            }
            "--view" => out.view = Some(value()?),
            "--card" => out.card = Some(value()?),
            "--remove" => out.remove = true,
            "--timeout" => {
                let raw = value()?;
                out.timeout = Some(raw.parse().map_err(|_| format!("kova ui: --timeout takes seconds, got '{}'", raw))?);
            }
            "--socket" => out.socket = Some(value()?),
            flag if flag.starts_with("--") => return Err(format!("kova ui: unknown flag {}\n\n{}", flag, USAGE)),
            word => out.words.push(word.to_string()),
        }
    }
    Ok(out)
}

/// The socket body of a `kova ui …` call, from its arguments, the asker and
/// the documents read (`view`, `card`). Pure, so it is tested.
fn body(a: &Args, from: &crate::ask::Asker, view: Option<serde_json::Value>, card: Option<serde_json::Value>) -> Result<serde_json::Value, String> {
    let from_json = serde_json::to_value(from).unwrap_or_default();
    let word = |i: usize, what: &str| a.words.get(i).cloned().ok_or_else(|| format!("kova ui: missing {}\n\n{}", what, USAGE));
    let mut v = match a.words.first().map(String::as_str) {
        Some("show") => {
            let pane_id = from.pane_id.ok_or("kova ui: not in a Kova pane (no KOVA_PANE_ID)")?;
            serde_json::json!({"cmd": "ui-show", "pane_id": pane_id, "view": view.ok_or("kova ui: show needs --view <file|->")?})
        }
        Some("update") => serde_json::json!({"cmd": "ui-update", "view_id": word(1, "view id")?, "view": view.ok_or("kova ui: update needs --view <file|->")?}),
        Some("card") => {
            if card.is_none() && !a.remove {
                return Err("kova ui: card needs --card <file|-> or --remove".into());
            }
            serde_json::json!({"cmd": "ui-update-card", "view_id": word(1, "view id")?, "card_id": word(2, "card id")?, "card": card})
        }
        Some("wait") => serde_json::json!({"cmd": "ui-wait", "view_id": word(1, "view id")?, "timeout_ms": a.timeout.unwrap_or(600) * 1000}),
        Some("close") => serde_json::json!({"cmd": "ui-close", "view_id": word(1, "view id")?}),
        _ => return Err(USAGE.to_string()),
    };
    if let Some(p) = &a.placement {
        v["placement"] = serde_json::json!(p);
    }
    if let Some(s) = a.size {
        v["size"] = serde_json::json!(s);
    }
    v["from"] = from_json;
    Ok(v)
}

/// `kova ui wait`: one event on stdout, acknowledged so Kova drops it.
fn wait(socket: &str, body: serde_json::Value, timeout_s: u64) -> i32 {
    let Ok(stream) = UnixStream::connect(socket) else {
        eprintln!("kova ui: Kova is not answering on {}", socket);
        return 1;
    };
    let mut writer = &stream;
    if writeln!(writer, "{}", body).is_err() {
        eprintln!("kova ui: could not send to Kova");
        return 1;
    }
    let _ = stream.set_read_timeout(Some(Duration::from_secs(timeout_s + 10)));
    let mut line = String::new();
    if BufReader::new(&stream).read_line(&mut line).unwrap_or(0) == 0 {
        eprintln!("kova ui: Kova closed before an event came");
        return 1;
    }
    let reply: serde_json::Value = match serde_json::from_str(line.trim()) {
        Ok(v) => v,
        Err(_) => {
            eprintln!("kova ui: unreadable reply: {}", line.trim());
            return 1;
        }
    };
    if reply.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        eprintln!("kova ui: {}", reply.get("error").and_then(|e| e.as_str()).unwrap_or("unknown error"));
        return 1;
    }
    match reply.get("data") {
        Some(data) if !data.is_null() => {
            println!("{}", data);
            let _ = writeln!(writer, "ack");
            let _ = writer.flush();
            0
        }
        _ => {
            eprintln!("kova ui: no event within {} s", timeout_s);
            2
        }
    }
}

pub fn run(args: &[String]) -> i32 {
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{}", USAGE);
        return if args.is_empty() { 1 } else { 0 };
    }
    let a = match parse(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{}", e);
            return 1;
        }
    };
    let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "/".into());
    let from = crate::ask::Asker::from_env(|k| std::env::var(k).ok(), None, &cwd);
    let read = |p: &Option<String>| -> Result<Option<serde_json::Value>, String> {
        p.as_deref().map(crate::ask::read_card).transpose().map_err(|e| e.replace("kova ask", "kova ui"))
    };
    let (view, card) = match (read(&a.view), read(&a.card)) {
        (Ok(v), Ok(c)) => (v, c),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("{}", e);
            return 1;
        }
    };
    let body = match body(&a, &from, view, card) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{}", e);
            return 1;
        }
    };
    if a.words.first().map(String::as_str) == Some("wait") {
        let socket = a.socket.clone().or_else(|| from.kova_socket.clone()).or_else(crate::ask::discover_socket);
        let Some(socket) = socket else {
            eprintln!("kova ui: no running Kova found");
            return 1;
        };
        return wait(&socket, body, a.timeout.unwrap_or(600));
    }
    match crate::ask::one_shot(a.socket.clone(), from.kova_socket.clone(), body) {
        Ok(reply) => {
            if let Some(data) = reply.get("data").filter(|d| !d.is_null()) {
                println!("{}", data);
            }
            0
        }
        Err(e) => {
            eprintln!("{}", e.replace("kova ask", "kova ui"));
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(v: &[&str]) -> Args {
        parse(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    fn asker() -> crate::ask::Asker {
        crate::ask::Asker { pane_id: Some(7), session_id: Some("s1".into()), ..Default::default() }
    }

    #[test]
    fn bodies() {
        let view = json!({"version": 1, "title": "T", "cards": []});
        let b = body(&args(&["show", "--placement", "pane-right", "--size", "0.5"]), &asker(), Some(view.clone()), None).unwrap();
        assert_eq!((b["cmd"].as_str(), b["pane_id"].as_u64(), b["placement"].as_str()), (Some("ui-show"), Some(7), Some("pane-right")));
        assert_eq!(b["from"]["session_id"], "s1");
        let no_pane = crate::ask::Asker::default();
        assert!(body(&args(&["show"]), &no_pane, Some(view.clone()), None).unwrap_err().contains("not in a Kova pane"));
        let c = body(&args(&["card", "v1", "a", "--remove"]), &asker(), None, None).unwrap();
        assert_eq!((c["cmd"].as_str(), c["card"].is_null()), (Some("ui-update-card"), true));
        assert!(body(&args(&["card", "v1", "a"]), &asker(), None, None).is_err());
        let w = body(&args(&["wait", "v1", "--timeout", "30"]), &asker(), None, None).unwrap();
        assert_eq!(w["timeout_ms"], 30_000);
        assert_eq!(body(&args(&["close", "v1"]), &asker(), None, None).unwrap()["cmd"], "ui-close");
        assert!(body(&args(&["nope"]), &asker(), None, None).is_err());
    }
}
