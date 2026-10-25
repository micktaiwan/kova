//! OSC 7501 — the Program Status Protocol: a program tells the terminal what it
//! is doing (idle, working, done, blocked, error) and why.
//!
//! Spec: <https://www.superlogical.com/rex/docs/build/program-status>. Pi
//! reports its state this way (and Claude Code does not), so a Kova pane can say
//! "the agent behind this pane is working / waiting for you / finished" without
//! scraping the screen.
//!
//! Only the **root record** is kept: the spec's per-`id` records (a build with
//! several workers) are ignored whole, which is what the spec allows — a report
//! whose id is not understood must not fall back to the root record, and a
//! terminal may support a single record. Everything Pi sends is a root record.
//!
//! Untrusted input: every value comes from a program that already owns the
//! screen. Nothing is shown as markup, `msg`/`title` are rejected whole when
//! they decode to a control character, and every spec limit is enforced before
//! any stored record is touched.

/// The `state` key. `clear` is not a state: it removes the record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProgramState {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
}

impl ProgramState {
    fn parse(value: &str) -> Option<ProgramState> {
        match value {
            "idle" => Some(ProgramState::Idle),
            "working" => Some(ProgramState::Working),
            "done" => Some(ProgramState::Done),
            "blocked" => Some(ProgramState::Blocked),
            "error" => Some(ProgramState::Error),
            _ => None,
        }
    }

    /// One-word label for the status bar.
    pub fn label(self) -> &'static str {
        match self {
            ProgramState::Idle => "idle",
            ProgramState::Working => "working",
            ProgramState::Done => "done",
            ProgramState::Blocked => "waiting",
            ProgramState::Error => "failed",
        }
    }
}

/// `kind`, meaningful with `blocked` only: what the program is waiting for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockedKind {
    /// Approval to do something.
    Permission,
    /// The user must type an answer.
    Question,
    /// A login, token or credential.
    Auth,
}

impl BlockedKind {
    fn parse(value: &str) -> Option<BlockedKind> {
        match value {
            "permission" => Some(BlockedKind::Permission),
            "question" => Some(BlockedKind::Question),
            "auth" => Some(BlockedKind::Auth),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            BlockedKind::Permission => "permission",
            BlockedKind::Question => "question",
            BlockedKind::Auth => "auth",
        }
    }
}

/// What one report left in the terminal's root record.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ProgramStatus {
    /// `None` only for a report without a usable `state` — which never gets
    /// stored (see `parse_report`), so a stored record always has one.
    pub state: Option<ProgramState>,
    pub kind: Option<BlockedKind>,
    /// 0..=100, `working`/`blocked` only.
    pub progress: Option<u8>,
    /// Stable program id (`pi`, `cargo`, `terraform`…), not a label.
    pub app: Option<String>,
    /// Short human label, base64 in the report.
    pub title: Option<String>,
    /// One human line: what it is doing, waiting for, or finished.
    pub msg: Option<String>,
}

// Spec limits (bytes). A report breaking one is discarded whole.
const MAX_SEQUENCE: usize = 4096;
const MAX_KEY: usize = 16;
const MAX_MSG_ENCODED: usize = 2732;
const MAX_MSG_DECODED: usize = 2048;
const MAX_TITLE_ENCODED: usize = 256;
const MAX_TITLE_DECODED: usize = 192;
const MAX_APP: usize = 32;

/// The reply to `OSC 7501 ; ? ST`: a supporting terminal MUST answer `?` and MAY
/// list the states and kinds it accepts. Ours are the full fixed sets, so a
/// program never has to fall back.
pub const QUERY_REPLY: &[u8] =
    b"\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x1b\\";

/// What a report asks for: store a record, or remove the root record.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ProgramReport {
    /// `state=clear` — the record is gone.
    Clear,
    Record(ProgramStatus),
}

/// The value characters of the grammar: `[A-Za-z0-9_.,+/=-]*`.
fn valid_value(value: &str) -> bool {
    value.bytes().all(|b| {
        b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b',' | b'+' | b'/' | b'=' | b'-')
    })
}

/// `[a-z]+`, at most 16 bytes.
fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= MAX_KEY && key.bytes().all(|b| b.is_ascii_lowercase())
}

/// Base64 of UTF-8 text, with the spec's decoded-size cap and its refusal of
/// control characters. `None` when the value is unusable — which discards the
/// whole report, per spec.
fn free_text(value: &str, max_encoded: usize, max_decoded: usize) -> Option<Option<String>> {
    if value.is_empty() {
        return Some(None);
    }
    if value.len() > max_encoded {
        return None;
    }
    let bytes = super::parser::decode_base64(value.as_bytes())?;
    if bytes.len() > max_decoded {
        return None;
    }
    // Decoded text must not contain a control character (C0, DEL, C1): these
    // are single-line values, and a `\n` in the status bar is not a message.
    if bytes
        .iter()
        .any(|&b| b < 0x20 || b == 0x7F || (0x80..=0x9F).contains(&b))
    {
        return None;
    }
    Some(String::from_utf8(bytes).ok())
}

/// Parse the body of `OSC 7501 ; <body> ST`.
///
/// `None` for a report that must be ignored whole: no `state`, an unknown
/// state, an `id` (we keep the root record only), a malformed base64, a decoded
/// control character, or a value outside the grammar.
pub fn parse_report(body: &[u8]) -> Option<ProgramReport> {
    if body.len() > MAX_SEQUENCE {
        return None;
    }
    let body = std::str::from_utf8(body).ok()?;

    let mut status = ProgramStatus::default();
    let mut state_seen = false;
    let mut clear = false;

    for pair in body.split(':') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let Some((key, value)) = pair.split_once('=') else {
            // A malformed pair is skipped; the rest of the report still counts.
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if !valid_key(key) || !valid_value(value) {
            continue;
        }

        match key {
            "state" => {
                if value == "clear" {
                    clear = true;
                    state_seen = true;
                } else if let Some(state) = ProgramState::parse(value) {
                    status.state = Some(state);
                    state_seen = true;
                } else {
                    // An unrecognized state ignores the whole report, so a
                    // state added later never degrades into `idle`.
                    return None;
                }
            }
            // A child record: we keep the root record only, so the whole report
            // is dropped rather than overwriting the root with a child's state.
            "id" => return None,
            "kind" => status.kind = BlockedKind::parse(value),
            "progress" => status.progress = value.parse::<u8>().ok().filter(|p| *p <= 100),
            "app" => {
                let ok = !value.is_empty()
                    && value.len() <= MAX_APP
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-'));
                status.app = if ok { Some(value.to_string()) } else { None };
            }
            "title" => status.title = free_text(value, MAX_TITLE_ENCODED, MAX_TITLE_DECODED)?,
            "msg" => status.msg = free_text(value, MAX_MSG_ENCODED, MAX_MSG_DECODED)?,
            // Unknown keys are ignored: that is how the protocol grows.
            _ => {}
        }
    }

    if !state_seen {
        return None;
    }
    if clear {
        return Some(ProgramReport::Clear);
    }
    // `kind` and `progress` only mean something with their state.
    if status.state != Some(ProgramState::Blocked) {
        status.kind = None;
    }
    if !matches!(status.state, Some(ProgramState::Working | ProgramState::Blocked)) {
        status.progress = None;
    }
    Some(ProgramReport::Record(status))
}

/// The status-bar badge for a stored record, or `None` when the bar has nothing
/// to add: `idle` is the resting state the bar already shows by being quiet.
///
/// Returns the text and a hue key the renderer maps to a colour:
/// `0` working, `1` waiting for the user, `2` finished, `3` failed.
pub fn badge(status: &ProgramStatus) -> Option<(String, u8)> {
    let state = status.state?;
    let mut text = match state {
        ProgramState::Idle => return None,
        ProgramState::Working => "working".to_string(),
        ProgramState::Blocked => match status.kind {
            Some(kind) => format!("waiting: {}", kind.label()),
            None => "waiting".to_string(),
        },
        ProgramState::Done => "done".to_string(),
        ProgramState::Error => "failed".to_string(),
    };
    if let Some(p) = status.progress {
        text.push_str(&format!(" {}%", p));
    }
    if let Some(app) = &status.app {
        text.push_str(&format!(" · {}", app));
    }
    let hue = match state {
        ProgramState::Idle => 0,
        ProgramState::Working => 0,
        ProgramState::Blocked => 1,
        ProgramState::Done => 2,
        ProgramState::Error => 3,
    };
    Some((text, hue))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64_encode_for_test as b64;

    /// Small helper so the tests read as the spec's own examples.
    fn base64_encode_for_test(text: &str) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let bytes = text.as_bytes();
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
            let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
            out.push(ALPHABET[(n >> 18) as usize & 63] as char);
            out.push(ALPHABET[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 { ALPHABET[(n >> 6) as usize & 63] as char } else { '=' });
            out.push(if chunk.len() > 2 { ALPHABET[n as usize & 63] as char } else { '=' });
        }
        out
    }

    fn report(body: &str) -> Option<ProgramReport> {
        parse_report(body.as_bytes())
    }

    #[test]
    fn spec_example_working_with_message() {
        let msg = b64("Installing updates");
        let parsed = report(&format!("state=working:app=brew:msg={msg}")).expect("report");
        let ProgramReport::Record(status) = parsed else { panic!("a record, not a clear") };
        assert_eq!(status.state, Some(ProgramState::Working));
        assert_eq!(status.app.as_deref(), Some("brew"));
        assert_eq!(status.msg.as_deref(), Some("Installing updates"));
        assert_eq!(badge(&status), Some(("working · brew".to_string(), 0)));
    }

    #[test]
    fn blocked_keeps_its_kind_and_progress_only_where_the_spec_allows_it() {
        let parsed = report("state=blocked:kind=permission:progress=40").expect("report");
        let ProgramReport::Record(status) = parsed else { panic!("a record") };
        assert_eq!(status.kind, Some(BlockedKind::Permission));
        assert_eq!(status.progress, Some(40));
        assert_eq!(badge(&status), Some(("waiting: permission 40%".to_string(), 1)));

        // `kind` with `working` is ignored, and so is `progress` with `done`.
        let parsed = report("state=working:kind=auth:progress=200").expect("report");
        let ProgramReport::Record(status) = parsed else { panic!("a record") };
        assert_eq!(status.kind, None, "kind means something with blocked only");
        assert_eq!(status.progress, None, "200 is out of range, treated as absent");
        assert_eq!(badge(&status), Some(("working".to_string(), 0)));

        let parsed = report("state=done:progress=100").expect("report");
        let ProgramReport::Record(status) = parsed else { panic!("a record") };
        assert_eq!(status.progress, None);
        assert_eq!(badge(&status), Some(("done".to_string(), 2)));
    }

    #[test]
    fn idle_shows_nothing_and_error_says_failed() {
        let ProgramReport::Record(idle) = report("state=idle").expect("report") else { panic!() };
        assert_eq!(badge(&idle), None, "at rest the bar stays quiet");
        let ProgramReport::Record(err) = report("state=error:app=pi").expect("report") else { panic!() };
        assert_eq!(badge(&err), Some(("failed · pi".to_string(), 3)));
    }

    #[test]
    fn clear_removes_the_record() {
        assert_eq!(report("state=clear"), Some(ProgramReport::Clear));
    }

    #[test]
    fn a_report_without_or_with_an_unknown_state_is_ignored_whole() {
        assert_eq!(report("app=pi:msg=dGVzdA=="), None);
        assert_eq!(report("state=deploying"), None, "a state added later must not become idle");
    }

    #[test]
    fn child_records_never_overwrite_the_root_one() {
        assert_eq!(report("state=working:id=us-east:title=VVMgRWFzdA=="), None);
        assert_eq!(report("state=working:id=../evil"), None);
    }

    #[test]
    fn a_message_that_decodes_to_a_control_character_discards_the_whole_report() {
        // "a\nb" — the spec refuses the whole report, not just the value.
        assert_eq!(report(&format!("state=working:msg={}", b64("a\nb"))), None);
        assert_eq!(report(&format!("state=working:title={}", b64("tab\there"))), None);
    }

    #[test]
    fn a_malformed_message_discards_only_that_pair() {
        // A byte outside the value grammar skips THAT pair only, per spec: the
        // record is still stored, without the message.
        let ProgramReport::Record(status) = report("state=working:msg=not!base64").expect("report") else {
            panic!("a record")
        };
        assert_eq!(status.msg, None);
        assert_eq!(status.state, Some(ProgramState::Working));

        // Truncated base64 decodes to nothing rather than erroring — an empty
        // message, which shows nothing. Harmless, and documented here so that no
        // one mistakes it for a strict decoder.
        let ProgramReport::Record(status) = report("state=working:msg=a").expect("report") else {
            panic!("a record")
        };
        assert_eq!(status.msg.as_deref(), Some(""));
    }

    #[test]
    fn malformed_pairs_are_skipped_and_unknown_keys_ignored() {
        let ProgramReport::Record(status) =
            report("nonsense:state=working:future=yes:app=pi").expect("report")
        else {
            panic!("a record")
        };
        assert_eq!(status.state, Some(ProgramState::Working));
        assert_eq!(status.app.as_deref(), Some("pi"));

        // An invalid `app` is treated as absent, an over-long one too.
        let ProgramReport::Record(status) = report("state=working:app=..\\x00").expect("report") else {
            panic!()
        };
        assert_eq!(status.app, None);
        let long = format!("state=working:app={}", "a".repeat(33));
        let ProgramReport::Record(status) = report(&long).expect("report") else { panic!() };
        assert_eq!(status.app, None, "app is capped at 32 bytes");
    }

    #[test]
    fn msg_is_capped_at_2048_decoded_bytes() {
        let big = "x".repeat(2000);
        let ok = format!("state=working:msg={}", b64(&big));
        let ProgramReport::Record(status) = report(&ok).expect("2000 bytes fit") else { panic!() };
        assert_eq!(status.msg.as_deref().map(str::len), Some(2000));

        let too_big = format!("state=working:msg={}", b64(&"y".repeat(2100)));
        assert_eq!(report(&too_big), None);
    }

    #[test]
    fn the_query_reply_names_every_state_and_kind() {
        let reply = std::str::from_utf8(QUERY_REPLY).expect("ascii");
        assert!(reply.starts_with("\x1b]7501;?:"), "a supporting terminal answers '?'");
        assert!(reply.ends_with("\x1b\\"), "ST-terminated, never BEL");
        for state in ["idle", "working", "done", "blocked", "error"] {
            assert!(reply.contains(state), "states list misses {state}");
        }
        for kind in ["permission", "question", "auth"] {
            assert!(reply.contains(kind), "kinds list misses {kind}");
        }
    }
}
