//! An agent asks the user something (`kova ask`), and the answer travels back.
//!
//! The agent runs `kova ask "<message>"`, which sends one `ask` request on Kova's
//! socket and blocks on that connection. Kova shows the question on a card
//! (`crate::ask_card`); the user answers with a button and, optionally, some
//! text. While the agent still waits, the answer goes back on the same
//! connection, and counts as handed over only once `kova ask` writes `ack`.
//!
//! When it has stopped waiting (Claude Code's Bash tool gives up after 10
//! minutes at most, a `claude -p` routine has ended), the answer is delivered
//! to the conversation itself, in this order:
//! 1. typed into the Kova pane that still runs the conversation;
//! 2. the conversation is open in another process (a routine still finishing
//!    its turn, a session in another terminal): wait for that process to end,
//!    since resuming it now would put two writers on its transcript;
//! 3. reopened in a new, visible Kova tab with `claude --resume <id> "<answer>"`.
//!
//! Questions not yet answered and answers not yet delivered are kept on disk
//! (`AskStore`) and picked up at the next launch; an answer leaves the disk only
//! once it is delivered.
//!
//! Every id that reaches a command line or a file name comes from the asker's
//! environment, which any process can set: session ids are refused outside
//! `[A-Za-z0-9_-]`, question ids outside the UUID shape.
//!
//! Ported from an earlier Swift prototype,
//! where every delivery path was measured first.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Who asked: what `kova ask` finds in its own environment, enough to reach
/// the conversation later.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asker {
    /// Shown on the card. Defaults to the working directory's last component.
    pub name: String,
    /// Which agent ran `kova ask`, so a late answer resumes with the right
    /// command. Absent on files written before omp existed: Claude then.
    #[serde(default)]
    pub agent: crate::agent_session::Agent,
    /// `CLAUDE_CODE_SESSION_ID`, or the id in omp/Pi's `PI_SESSION_FILE`, of the
    /// conversation that ran `kova ask`.
    #[serde(default)]
    pub session_id: Option<String>,
    /// `KOVA_PANE_ID` and `KOVA_SOCKET` of the pane it runs in, when that terminal is Kova.
    #[serde(default)]
    pub pane_id: Option<u32>,
    #[serde(default)]
    pub kova_socket: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

impl Asker {
    /// From `kova ask`'s environment. A session id Claude Code would not write
    /// is dropped, not escaped: it ends up on a command line.
    pub fn from_env(env: impl Fn(&str) -> Option<String>, name: Option<String>, cwd: &str) -> Asker {
        let default_name = Path::new(cwd)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| cwd.to_string());
        let (agent, session_id) = agent_and_session(&env);
        Asker {
            name: name.unwrap_or(default_name),
            agent,
            session_id,
            pane_id: env("KOVA_PANE_ID").and_then(|p| p.parse().ok()),
            kova_socket: env("KOVA_SOCKET").filter(|s| !s.is_empty()),
            cwd: Some(cwd.to_string()),
        }
    }
}

/// The agent that ran `kova ask`, and its conversation id, from the environment
/// its children inherit. Claude is tried first; omp and Pi both hand their
/// commands `PI_SESSION_FILE` / `PI_SESSION_ID`, and `OMPCODE` (or a path under
/// `~/.omp`) tells the two apart.
fn agent_and_session(
    env: &impl Fn(&str) -> Option<String>,
) -> (crate::agent_session::Agent, Option<String>) {
    if let Some(id) = env("CLAUDE_CODE_SESSION_ID").filter(|id| is_safe_session_id(id)) {
        return (crate::agent_session::Agent::Claude, Some(id));
    }
    if let Some(id) = env(crate::tazu_session::SESSION_ENV).filter(|id| is_safe_session_id(id)) {
        return (crate::agent_session::Agent::Tazu, Some(id));
    }
    let omps = env("OMPCODE").as_deref() == Some("1")
        || env("PI_SESSION_FILE").is_some_and(|path| path.contains("/.omp/"));
    if let Some(path) = env("PI_SESSION_FILE") {
        let id = Path::new(&path)
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(crate::omp_session::session_id_from_file_name)
            .filter(|id| is_safe_session_id(id));
        if id.is_some() {
            let agent = if omps {
                crate::agent_session::Agent::Omp
            } else {
                crate::agent_session::Agent::Pi
            };
            return (agent, id);
        }
    }
    if let Some(id) = env("PI_SESSION_ID").filter(|id| is_safe_session_id(id)) {
        let agent = if omps {
            crate::agent_session::Agent::Omp
        } else {
            crate::agent_session::Agent::Pi
        };
        return (agent, Some(id));
    }
    (crate::agent_session::Agent::Claude, None)
}

/// One question, as it travels on the socket and sits on disk.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    pub message: String,
    /// Buttons, in order; empty means a single "Reply" button.
    #[serde(default)]
    pub actions: Vec<String>,
    /// Whether the card shows a text field.
    #[serde(default = "default_true")]
    pub input: bool,
    pub from: Asker,
    /// Epoch seconds, shown on the card.
    #[serde(default)]
    pub asked_at: u64,
    /// The user wrote in the conversation after the question was asked (see
    /// [`Board::withdraw`]): the card keeps it, says so, and adds [`DISMISS`].
    #[serde(default)]
    pub superseded: bool,
    /// A decision card (`crate::decision_card`): when present, the card draws it
    /// and `message`, `actions`, `input` are only its fallback for an older Kova.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card: Option<crate::decision_card::Card>,
    /// 1 at creation, bumped by each `update-ask`. Left out of the JSON at 1, so
    /// a plain question serializes exactly as before; `list-asks` adds it.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub card_version: u32,
}

fn default_true() -> bool {
    true
}

fn one() -> u32 {
    1
}

fn is_one(n: &u32) -> bool {
    *n == 1
}

/// The button always added after the asker's actions when the card has a text field: a typed
/// answer that is none of the proposed choices must not ride on one of them ("A: stop everything").
pub const OTHER: &str = "Other";

/// The extra button of a superseded question: closes the card, delivers nothing.
pub const DISMISS: &str = "Dismiss";

/// The line a superseded card shows above the question.
pub const SUPERSEDED_NOTE: &str = "You wrote in the conversation since. Answer here anyway, or dismiss.";

/// The pauses the card's "Not now" offers, in seconds with their label; the
/// first is the default.
pub const DND_CHOICES: [(u64, &str); 3] = [(20 * 60, "20 min"), (60 * 60, "1 h"), (3 * 60 * 60, "3 h")];

/// The error a `kova ask` receives while the user paused the cards: followed by
/// the epoch second the pause ends at. Its question is kept and shows then.
pub const PAUSED_PREFIX: &str = "paused until ";

/// The error line for a pause ending at `until`.
pub fn paused_error(until: u64) -> String {
    format!("{}{}", PAUSED_PREFIX, until)
}

/// The end of the pause an error line announces, if it is one.
pub fn paused_until_of(error: &str) -> Option<u64> {
    error.strip_prefix(PAUSED_PREFIX)?.trim().parse().ok()
}

/// `kova ask --dnd <value>`: `off` (or 0) ends the pause, `<n>m`, `<n>h` or a
/// bare number of minutes sets one. Returns minutes, 0 for off; at most a day.
pub fn parse_dnd_minutes(raw: &str) -> Result<u64, String> {
    let raw = raw.trim().to_ascii_lowercase();
    if raw == "off" {
        return Ok(0);
    }
    let (digits, factor) = match raw.strip_suffix('h') {
        Some(d) => (d, 60),
        None => (raw.strip_suffix("min").or_else(|| raw.strip_suffix('m')).unwrap_or(&raw), 1),
    };
    let minutes = digits
        .trim()
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(factor))
        .ok_or_else(|| format!("kova ask: --dnd takes off, minutes (20, 20m) or hours (1h), got '{}'", raw))?;
    if minutes > 24 * 60 {
        return Err("kova ask: --dnd pauses for a day at most".to_string());
    }
    Ok(minutes)
}

/// `until` (epoch seconds) as the local `HH:MM`.
pub fn local_hhmm(until: u64) -> String {
    let t = until as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

impl Question {
    /// The buttons the card shows.
    pub fn buttons(&self) -> Vec<String> {
        // A card lists its own actions, and gets no "Other".
        let mut buttons = if let Some(card) = &self.card {
            card.actions.iter().map(|a| a.id.clone()).collect()
        } else if self.actions.is_empty() {
            vec!["Reply".to_string()]
        } else {
            let mut buttons = self.actions.clone();
            if self.input && !buttons.iter().any(|b| b == OTHER) {
                buttons.push(OTHER.to_string());
            }
            buttons
        };
        if self.dismissable() {
            buttons.push(DISMISS.to_string());
        }
        buttons
    }

    /// Whether [`DISMISS`] closes this question without an answer: superseded,
    /// and the asker did not propose a "Dismiss" of its own, which stays an answer.
    pub fn dismissable(&self) -> bool {
        let own = match &self.card {
            Some(card) => card.actions.iter().any(|a| a.id == DISMISS),
            None => self.actions.iter().any(|a| a == DISMISS),
        };
        self.superseded && !own
    }

    /// Whether the card shows a text field.
    pub fn has_input(&self) -> bool {
        match &self.card {
            Some(card) => card.input.is_some(),
            None => self.input,
        }
    }

    /// Put `card` on this question: its fallback fields follow it.
    pub fn set_card(&mut self, card: crate::decision_card::Card, message: Option<String>) {
        self.message = message.filter(|m| !m.trim().is_empty()).unwrap_or_else(|| {
            crate::decision_card::derive_message(&serde_json::to_value(&card).unwrap_or_default())
        });
        self.actions = card.legacy_actions();
        self.input = card.input.is_some();
        self.card = Some(card);
    }
}

/// The user's answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    pub id: String,
    pub action: String,
    pub text: String,
    /// The version of the question shown when the user answered (§ 7 of the
    /// decision cards spec). 1 for an answer kept on disk by an older Kova.
    #[serde(default = "one")]
    pub card_version: u32,
    /// Every `code` block of a decision card, as the user left it: the text
    /// to send is in `file`, edited or not, never the asker's own copy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<AnswerBlock>,
}

/// A `code` block's final text, written by Kova when the user answered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnswerBlock {
    /// Index in the card's `blocks`.
    pub block: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Absolute path of the file holding the text, byte for byte.
    pub file: String,
    pub sha256: String,
    /// The user changed the text the card showed.
    pub edited: bool,
}

impl Answer {
    /// One line for the asker: "OK", or "OK: fix that part", then where each
    /// code block's final text is.
    pub fn line(&self) -> String {
        let line = if self.text.is_empty() { self.action.clone() } else { format!("{}: {}", self.action, self.text) };
        line + &block_reports(&self.blocks)
    }
}

/// Where each code block's final text is, as appended to an answer line (a
/// `kova ask` answer or a pane view click): ` [block <i> "<label>" edited by
/// the user|unchanged: <file> sha256 <hex>]` per block.
pub fn block_reports(blocks: &[AnswerBlock]) -> String {
    blocks
        .iter()
        .map(|b| {
            format!(
                " [block {}{} {}: {} sha256 {}]",
                b.block,
                b.label.as_deref().map(|l| format!(" \"{}\"", l)).unwrap_or_default(),
                if b.edited { "edited by the user" } else { "unchanged" },
                b.file,
                b.sha256
            )
        })
        .collect()
}

/// How long an answer's block files stay on disk: a late delivery can wake
/// the asking conversation days later.
pub const ANSWER_FILES_TTL_SECS: u64 = 7 * 24 * 3600;

/// `<ask dir>/answers/<question id>/block-<i>.txt`.
pub fn answer_block_path(root: &Path, question_id: &str, block: usize) -> Option<PathBuf> {
    let stem = crate::decision_card::grant_file_stem(question_id)?;
    Some(root.join("answers").join(stem).join(format!("block-{}.txt", block)))
}

/// Write each code block's final text and fill the answer's `blocks`.
/// Directory 0700, files 0600, each written atomically.
pub fn write_answer_blocks(root: &Path, answer: &mut Answer, texts: &[FinalBlock]) -> Result<(), String> {
    let blocks = write_block_files(root, &answer.id, texts)?;
    answer.blocks.extend(blocks);
    Ok(())
}

/// Write each code block's final text under `<ask dir>/answers/<id>/`, the
/// files a click reports. `id` is a question id or a pane view's grant id
/// (`view:card`). Directory 0700, files 0600, each written atomically.
pub fn write_block_files(root: &Path, id: &str, texts: &[FinalBlock]) -> Result<Vec<AnswerBlock>, String> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let mut out = Vec::new();
    for t in texts {
        let path = answer_block_path(root, id, t.block).ok_or("bad question id")?;
        let dir = path.parent().ok_or("no parent")?;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(|e| e.to_string())?;
        let tmp = dir.join(format!(".block-{}.tmp", t.block));
        let _ = std::fs::remove_file(&tmp);
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp).map_err(|e| e.to_string())?;
        file.write_all(t.text.as_bytes()).and_then(|_| file.sync_all()).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
        out.push(AnswerBlock {
            block: t.block,
            label: t.label.clone(),
            file: path.to_string_lossy().into_owned(),
            sha256: crate::decision_card::sha256_hex(t.text.as_bytes()),
            edited: t.edited,
        });
    }
    Ok(out)
}

/// Delete answer folders older than [`ANSWER_FILES_TTL_SECS`] (by mtime).
pub fn prune_answer_files(root: &Path, now: u64) -> usize {
    let Ok(entries) = std::fs::read_dir(root.join("answers")) else { return 0 };
    let mut removed = 0;
    for entry in entries.flatten() {
        let modified = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if now.saturating_sub(modified) >= ANSWER_FILES_TTL_SECS && std::fs::remove_dir_all(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// A `code` block's text when the user answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinalBlock {
    pub block: usize,
    pub label: Option<String>,
    pub text: String,
    pub edited: bool,
}

/// The final text of every `code` block of `card`: the user's version from
/// `edits` (block index, text) when given, else the card's.
pub fn final_blocks(card: &crate::decision_card::Card, edits: &[(usize, String)]) -> Vec<FinalBlock> {
    card.blocks
        .iter()
        .enumerate()
        .filter_map(|(i, b)| match b {
            crate::decision_card::Block::Code { text, label } => {
                let final_text = edits.iter().rev().find(|(e, _)| *e == i).map(|(_, t)| t.clone()).unwrap_or_else(|| text.clone());
                let edited = final_text != *text;
                Some(FinalBlock { block: i, label: label.clone(), text: final_text, edited })
            }
            _ => None,
        })
        .collect()
}

/// An answer given but not yet delivered, kept on disk until it is: a delivery
/// can wait hours for a conversation to close, and Kova may quit meanwhile.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Undelivered {
    pub question: Question,
    pub answer: Answer,
}

/// How an answer reaches an asker that no longer waits on the socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// Type it into this pane of this Kova socket: the conversation is still open there.
    Pane { socket: String, pane_id: u32 },
    /// Reopen the conversation in a new Kova tab, in its directory.
    Resume {
        agent: crate::agent_session::Agent,
        session_id: String,
        cwd: Option<String>,
    },
    /// The conversation is still open in a process Kova cannot type into:
    /// wait for it to end, then resume.
    OpenElsewhere { pid: i32 },
    /// Nothing to reach: the answer stays in Kova's log.
    None,
}

/// Picks the delivery for an answer nobody waits for. `pane_holds_session`
/// says whether the asker's pane still runs the asker's conversation;
/// `live_pid` is the process that still runs it, if any.
/// Typing into a pane needs a session id to compare: without one, the pane may
/// run anything by now.
pub fn delivery(asker: &Asker, pane_holds_session: bool, live_pid: Option<i32>) -> Delivery {
    let Some(session) = asker.session_id.clone() else { return Delivery::None };
    if pane_holds_session {
        if let (Some(socket), Some(pane_id)) = (asker.kova_socket.clone(), asker.pane_id) {
            return Delivery::Pane { socket, pane_id };
        }
    }
    if let Some(pid) = live_pid {
        return Delivery::OpenElsewhere { pid };
    }
    Delivery::Resume {
        agent: asker.agent,
        session_id: session,
        cwd: asker.cwd.clone(),
    }
}

/// Whether an answer given after the asker stopped waiting can still reach it.
pub fn can_deliver_late(asker: &Asker) -> bool {
    asker.session_id.is_some()
}

/// The message the conversation receives when the answer comes late: it says
/// what it answers, since the question is far up in the conversation by then.
/// One line: typed into a terminal, a newline would submit early.
pub fn late_answer_message(question: &Question, answer: &Answer) -> String {
    // Both texts come from outside: the question from whatever process reached
    // the socket, the answer from the user. Any control character (a newline,
    // but also ^U that erases the line, ESC…) would act on the terminal it is
    // typed into, so each becomes a space.
    let one_line = |t: &str| {
        let flat = t.replace("\r\n", " ");
        flat.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>()
    };
    match &question.card {
        Some(card) => format!(
            "[Kova] The user answered your card \"{}\" (v{}): {}",
            one_line(&card.title),
            answer.card_version,
            one_line(&answer.line())
        ),
        None => format!(
            "[Kova] The user answered your question \"{}\": {}",
            one_line(&question.message),
            one_line(&answer.line())
        ),
    }
}

/// A Claude session id as Claude Code writes it.
pub fn is_safe_session_id(id: &str) -> bool {
    id.len() <= 128 && crate::claude_session::is_safe_session_id(id)
}

/// A question id as `kova ask` makes it (a UUID): it names a file.
pub fn is_safe_question_id(id: &str) -> bool {
    let groups: Vec<&str> = id.split('-').collect();
    groups.len() == 5
        && groups.iter().map(|g| g.len()).eq([8, 4, 4, 4, 12])
        && groups.iter().all(|g| g.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A fresh random v4 UUID, lowercase.
pub fn new_question_id() -> String {
    let mut b = [0u8; 16];
    let read = std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b));
    if read.is_err() {
        // Not secret, only unique: time and pid are enough when urandom fails.
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        b[..8].copy_from_slice(&(now.as_nanos() as u64).to_le_bytes());
        b[8..12].copy_from_slice(&std::process::id().to_le_bytes());
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{:02x}", x)).collect();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

/// The live process running conversation `session_id`, from Claude Code's
/// registry (one JSON per process in `dir`, normally `~/.claude/sessions`).
pub fn live_session_pid(session_id: &str, dir: &Path) -> Option<i32> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(data) = std::fs::read_to_string(&path) else { continue };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&data) else { continue };
        if json.get("sessionId").and_then(|v| v.as_str()) != Some(session_id) {
            continue;
        }
        let Some(pid) = json.get("pid").and_then(|v| v.as_i64()).and_then(|p| i32::try_from(p).ok()) else {
            continue;
        };
        if pid > 0 && unsafe { libc::kill(pid, 0) } == 0 {
            return Some(pid);
        }
    }
    None
}

/// The parent of `pid`, `None` once the process is gone.
pub fn parent_pid(pid: i32) -> Option<i32> {
    unsafe {
        let mut info: libc::proc_bsdinfo = std::mem::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        let ret = libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, &mut info as *mut _ as *mut libc::c_void, size);
        (ret > 0).then_some(info.pbi_ppid as i32)
    }
}

/// Whether `pid` descends from `ancestor` (a pane's shell), walking at most 16 parents.
pub fn descends(pid: i32, ancestor: i32) -> bool {
    // A shell that ran `exec claude` is the conversation itself.
    if pid == ancestor {
        return true;
    }
    let mut current = pid;
    for _ in 0..16 {
        match parent_pid(current) {
            Some(parent) if parent > 1 => {
                if parent == ancestor {
                    return true;
                }
                current = parent;
            }
            _ => return false,
        }
    }
    false
}

/// Whether pane `pane_id` of a `list-panes` reply still runs `session_id`, and
/// the proof. Two proofs, either is enough: Kova reports the session in that
/// pane, or the process running the conversation descends from the pane's
/// shell. The second exists because the report lags: a pane opened minutes
/// before still showed no session.
pub fn pane_holds_session(
    panes: &[serde_json::Value],
    pane_id: u32,
    agent: crate::agent_session::Agent,
    session_id: &str,
    live_pid: Option<i32>,
    descends: impl Fn(i32, i32) -> bool,
) -> Option<String> {
    let pane = panes
        .iter()
        .find(|p| p.get("id").and_then(|v| v.as_u64()) == Some(pane_id as u64))?;
    // An older Kova published only the Claude field.
    let reported_agent = pane
        .get("agent")
        .and_then(|v| v.as_str())
        .unwrap_or("claude");
    let reported = pane
        .get("agent_session_id")
        .or_else(|| pane.get("claude_session_id"))
        .and_then(|v| v.as_str());
    if reported == Some(session_id) && reported_agent == agent.as_str() {
        return Some("reported by Kova".to_string());
    }
    let live = live_pid?;
    let shell = pane.get("pid").and_then(|v| v.as_i64()).and_then(|p| i32::try_from(p).ok())?;
    descends(live, shell).then(|| {
        format!(
            "process {} descends from its shell {} (Kova reports {})",
            live,
            shell,
            reported.unwrap_or("no session")
        )
    })
}

/// Where Kova keeps what must survive a restart: `questions/<id>.json` (asked,
/// not answered) and `deliveries/<id>.json` (answered, not delivered). One file
/// per item, deleted once done.
#[derive(Clone, Debug)]
pub struct AskStore {
    pub root: PathBuf,
}

impl AskStore {
    /// `~/.config/kova/ask`, or `KOVA_ASK_DIR`: a test instance of Kova points
    /// it elsewhere so it never replays the real one's questions.
    pub fn default_root() -> PathBuf {
        if let Some(dir) = std::env::var_os("KOVA_ASK_DIR").filter(|d| !d.is_empty()) {
            return PathBuf::from(dir);
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join(".config/kova/ask")
    }

    pub fn new(root: PathBuf) -> AskStore {
        AskStore { root }
    }

    /// Take this store's lock, `None` when another Kova holds it. Only the
    /// holder replays the questions and deliveries left on disk: two Kova
    /// replaying them would show each question twice and deliver each answer
    /// twice (two `claude --resume` on one transcript). Released by the kernel
    /// when the process dies.
    pub fn try_own(&self) -> Option<std::fs::File> {
        std::fs::create_dir_all(&self.root).ok()?;
        let file = std::fs::OpenOptions::new().write(true).create(true).truncate(false).open(self.root.join("owner.lock")).ok()?;
        file.try_lock().ok()?;
        Some(file)
    }

    fn questions(&self) -> PathBuf {
        self.root.join("questions")
    }

    fn deliveries(&self) -> PathBuf {
        self.root.join("deliveries")
    }

    pub fn save_question(&self, question: &Question) {
        write(&self.questions(), &question.id, question);
    }

    pub fn remove_question(&self, id: &str) {
        remove(&self.questions(), id);
    }

    pub fn save_delivery(&self, item: &Undelivered) {
        write(&self.deliveries(), &item.question.id, item);
    }

    pub fn remove_delivery(&self, id: &str) {
        remove(&self.deliveries(), id);
    }

    /// Oldest first, the order the card shows them in.
    pub fn pending_questions(&self) -> Vec<Question> {
        let mut questions: Vec<Question> = read(&self.questions());
        questions.sort_by(|a, b| (a.asked_at, &a.id).cmp(&(b.asked_at, &b.id)));
        questions
    }

    pub fn undelivered(&self) -> Vec<Undelivered> {
        read(&self.deliveries())
    }

    /// The pause of the cards (`Board::set_pause`), kept across a restart: the
    /// epoch second it ends at, in a file of its own. `None` removes it.
    pub fn save_pause(&self, until: Option<u64>) {
        let path = self.root.join("paused-until");
        match until {
            Some(until) => {
                let _ = std::fs::create_dir_all(&self.root);
                if let Err(e) = std::fs::write(&path, until.to_string()) {
                    log::warn!("ask: cannot save the pause in {}: {}", path.display(), e);
                }
            }
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    pub fn load_pause(&self) -> Option<u64> {
        std::fs::read_to_string(self.root.join("paused-until")).ok()?.trim().parse().ok()
    }
}

fn write<T: Serialize>(dir: &Path, id: &str, value: &T) {
    if !is_safe_question_id(id) {
        return;
    }
    let Ok(data) = serde_json::to_vec(value) else { return };
    if let Err(e) = std::fs::create_dir_all(dir) {
        log::warn!("ask: cannot create {}: {}", dir.display(), e);
        return;
    }
    // Atomic: a crash mid-write must not leave half a question to replay.
    let tmp = dir.join(format!(".{}.tmp", id));
    let result = std::fs::write(&tmp, data).and_then(|_| std::fs::rename(&tmp, dir.join(format!("{}.json", id))));
    if let Err(e) = result {
        log::warn!("ask: cannot save {} in {}: {}", id, dir.display(), e);
    }
}

fn remove(dir: &Path, id: &str) {
    if is_safe_question_id(id) {
        let _ = std::fs::remove_file(dir.join(format!("{}.json", id)));
    }
}

fn read<T: for<'de> Deserialize<'de>>(dir: &Path) -> Vec<T> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .filter_map(|p| std::fs::read(&p).ok())
        .filter_map(|data| serde_json::from_slice(&data).ok())
        .collect()
}

/// Whether this process holds the ask directory's lock (`Board`), read by the
/// delivery threads to say truthfully what became of an undelivered answer.
static OWNS_STORE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What happens to an answer that could not be delivered, in a log line.
fn kept() -> &'static str {
    if OWNS_STORE.load(std::sync::atomic::Ordering::Relaxed) {
        "kept on disk, retried at next launch"
    } else {
        "LOST: this Kova does not hold the ask directory, so nothing was kept on disk"
    }
}

/// `'…'` for zsh: the answer is the user's free text.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// One request on a Kova socket, its reply as JSON. `None` when the socket
/// does not answer within `timeout`.
fn request(socket: &str, body: serde_json::Value, timeout: Duration) -> Option<serde_json::Value> {
    let stream = UnixStream::connect(socket).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    let mut writer = &stream;
    writeln!(writer, "{}", body).ok()?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

fn reply_ok(reply: &Option<serde_json::Value>) -> bool {
    reply.as_ref().and_then(|r| r.get("ok")).and_then(|v| v.as_bool()) == Some(true)
}

/// The asker stopped waiting: reach its conversation instead. Blocking (it may
/// wait hours for a process to end), so it runs on its own thread, and it talks
/// to Kova sockets only — this Kova's own included, answered by the main thread.
/// The answer leaves `store` only once delivered.
pub fn deliver_late(store: &AskStore, question: &Question, answer: &Answer, own_socket: &str) {
    let message = late_answer_message(question, answer);
    let id = question.id.clone();
    deliver_line(&question.id, &question.from, &message, own_socket, &|| store.remove_delivery(&id));
}

/// Type `message` into the asker's conversation (its pane, a sleeping pane
/// woken, or a resume in a new tab), then call `done` once it is delivered.
/// Shared by late `kova ask` answers and pane UI events. Blocking.
pub fn deliver_line(id: &str, asker: &Asker, message: &str, own_socket: &str, done: &dyn Fn()) {
    let message = message.to_string();
    let target = match wake_sleeping_asker(asker, own_socket) {
        Some(Woken::Ready { socket, pane_id }) => Delivery::Pane { socket, pane_id },
        Some(Woken::NotYet { pane_id }) => {
            log::warn!("ask {}: pane {} holds the conversation asleep, its agent did not come up ({})", id, pane_id, kept());
            return;
        }
        None => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            let registry = PathBuf::from(home).join(".claude/sessions");
            // Only Claude registers a per-process session file; for the other
            // agents the pane report is the whole evidence.
            let live = (asker.agent == crate::agent_session::Agent::Claude)
                .then(|| {
                    asker
                        .session_id
                        .as_deref()
                        .and_then(|s| live_session_pid(s, &registry))
                })
                .flatten();
            let holds = holding_pane(asker, live);
            delivery(asker, holds, live)
        }
    };
    match target {
        Delivery::Pane { socket, pane_id } => {
            // The text, then Enter on its own: Claude Code takes a pasted
            // newline as part of the message.
            let typed = request(&socket, serde_json::json!({"cmd": "send-keys", "pane_id": pane_id, "text": message}), Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(300));
            let entered = request(&socket, serde_json::json!({"cmd": "send-keys", "pane_id": pane_id, "text": "\r"}), Duration::from_secs(5));
            let delivered = reply_ok(&typed) && reply_ok(&entered);
            log::info!(
                "ask {}: typed into pane {} of {}: ok={}/{}{}",
                id,
                pane_id,
                socket,
                reply_ok(&typed),
                reply_ok(&entered),
                if delivered { String::new() } else { format!(" — {}", kept()) }
            );
            if delivered {
                done();
            }
        }
        Delivery::Resume {
            agent,
            session_id,
            cwd,
        } => resume(
            done,
            id,
            agent,
            &session_id,
            cwd.as_deref(),
            &message,
            own_socket,
        ),
        Delivery::OpenElsewhere { pid } => {
            log::info!(
                "ask {}: conversation still open in process {}, outside a pane Kova can type into; resuming once it exits",
                id,
                pid
            );
            let deadline = std::time::Instant::now() + Duration::from_secs(24 * 3600);
            let alive = || unsafe { libc::kill(pid, 0) } == 0;
            while alive() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_secs(2));
            }
            match asker.session_id.as_deref() {
                Some(session) if !alive() => resume(
                    done,
                    id,
                    asker.agent,
                    session,
                    asker.cwd.as_deref(),
                    &message,
                    own_socket,
                ),
                _ => log::warn!(
                    "ask {}: NOT delivered, process {} still alive after 24 h ({}): {}",
                    id,
                    pid,
                    kept(),
                    message
                ),
            }
        }
        Delivery::None => {
            log::info!("ask {}: answer has nowhere to go (no Claude session to reach): {}", id, message);
            done();
        }
    }
}

/// What waking a sleeping asker's pane gave (`wake_sleeping_asker`).
enum Woken {
    /// Its agent runs the conversation again: type the answer there.
    Ready { socket: String, pane_id: u32 },
    /// Woken, but its agent did not report the conversation in time.
    NotYet { pane_id: u32 },
}

/// How long a woken agent gets to report its conversation in `list-panes`.
const WAKE_WAIT: Duration = Duration::from_secs(60);

/// The pane of `panes` where conversation `session_id` sleeps since a Kova
/// relaunch (`dormant_session_id`), resume line not run yet.
pub fn sleeping_pane(panes: &[serde_json::Value], session_id: &str) -> Option<u32> {
    panes
        .iter()
        .find(|p| p.get("dormant_session_id").and_then(|v| v.as_str()) == Some(session_id))
        .and_then(|p| p.get("id")?.as_u64()?.try_into().ok())
}

/// The pane `pane_id` of `panes` reports that it runs `session_id`.
fn reports_session(
    panes: &[serde_json::Value],
    pane_id: u32,
    agent: crate::agent_session::Agent,
    session_id: &str,
) -> bool {
    panes.iter().any(|p| {
        p.get("id").and_then(|v| v.as_u64()) == Some(pane_id as u64)
            && p.get("agent_session_id").and_then(|v| v.as_str()) == Some(session_id)
            && p.get("agent").and_then(|v| v.as_str()) == Some(agent.as_str())
    })
}

fn list_panes(socket: &str) -> Option<Vec<serde_json::Value>> {
    let reply = request(socket, serde_json::json!({"cmd": "list-panes"}), Duration::from_secs(3))?;
    reply.get("data")?.as_array().cloned()
}

/// A Kova relaunch leaves restored agents asleep until their pane is shown. If
/// the asker's conversation sleeps in a pane, wake that pane and wait for the
/// agent: resuming it in a new tab would run the conversation twice once the
/// sleeping pane is shown. Looked up in this Kova too, not only the asker's: a
/// question asked before the relaunch names a socket that died with the old
/// Kova. `None` when it sleeps nowhere.
fn wake_sleeping_asker(asker: &Asker, own_socket: &str) -> Option<Woken> {
    let session = asker.session_id.as_deref()?;
    let (socket, pane_id) = asker
        .kova_socket
        .as_deref()
        .into_iter()
        .chain(std::iter::once(own_socket))
        .find_map(|socket| Some((socket, sleeping_pane(&list_panes(socket)?, session)?)))?;
    let woke = request(socket, serde_json::json!({"cmd": "wake-pane", "pane_id": pane_id}), Duration::from_secs(5));
    if !reply_ok(&woke) {
        // Still asleep there: a resume elsewhere would be the second one.
        return Some(Woken::NotYet { pane_id });
    }
    log::info!("ask: session {} was asleep in pane {}, woke it", session, pane_id);
    let deadline = std::time::Instant::now() + WAKE_WAIT;
    while std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(1));
        if list_panes(socket)
            .is_some_and(|panes| reports_session(&panes, pane_id, asker.agent, session))
        {
            // Registered is not drawn yet: give the agent's input a moment.
            std::thread::sleep(Duration::from_secs(2));
            return Some(Woken::Ready { socket: socket.to_string(), pane_id });
        }
    }
    Some(Woken::NotYet { pane_id })
}

/// Whether the asker's Kova pane still runs the asker's conversation, read now
/// from that Kova's `list-panes`. Logs the proof it used.
fn holding_pane(asker: &Asker, live: Option<i32>) -> bool {
    let (Some(session), Some(socket), Some(pane_id)) = (asker.session_id.as_deref(), asker.kova_socket.as_deref(), asker.pane_id) else {
        return false;
    };
    let reply = request(socket, serde_json::json!({"cmd": "list-panes"}), Duration::from_secs(3));
    let Some(panes) = reply.as_ref().and_then(|r| r.get("data")).and_then(|d| d.as_array()) else {
        return false;
    };
    match pane_holds_session(panes, pane_id, asker.agent, session, live, descends) {
        Some(proof) => {
            log::info!("ask: pane {} holds session {}: {}", pane_id, session, proof);
            true
        }
        None => false,
    }
}

/// The line typed into a new tab to reopen the conversation with the answer as
/// its next message. Claude keeps the same permission mode as the user's
/// sessions: a reopened agent that cannot run its tools would read the answer
/// and do nothing with it. The other agents reopen with their own resume line.
pub fn resume_line(
    agent: crate::agent_session::Agent,
    session_id: &str,
    message: &str,
) -> Option<String> {
    if !is_safe_session_id(session_id) {
        return None;
    }
    let base = if agent == crate::agent_session::Agent::Claude {
        format!(
            "claude --dangerously-skip-permissions --resume {}",
            session_id
        )
    } else {
        crate::agent_session::resume_command(agent, session_id, None, None)?
    };
    Some(format!("{} {}", base, shell_quote(message)))
}

/// The directory conversation `session_id` ran in, read from its transcript
/// (`<projects>/<dir>/<id>.jsonl`, whose lines carry a `cwd`). `None` when
/// Claude Code has no transcript for it.
pub fn transcript_cwd(projects: &Path, session_id: &str) -> Option<String> {
    if !is_safe_session_id(session_id) {
        return None;
    }
    let file = std::fs::read_dir(projects)
        .ok()?
        .flatten()
        .map(|d| d.path().join(format!("{}.jsonl", session_id)))
        .find(|p| p.is_file())?;
    let reader = BufReader::new(std::fs::File::open(file).ok()?);
    reader
        .lines()
        .take(200)
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(&line).ok())
        .find_map(|v| v.get("cwd").and_then(|c| c.as_str()).map(String::from))
}

/// How long after Enter a reopened conversation must still be running.
const RESUME_CHECK: Duration = Duration::from_secs(10);

/// Reopen the conversation in a new tab of this Kova, in its directory, with
/// the answer as its next prompt: visible, so the user sees the agent go on.
fn resume(
    done: &dyn Fn(),
    id: &str,
    agent: crate::agent_session::Agent,
    session_id: &str,
    cwd: Option<&str>,
    message: &str,
    own_socket: &str,
) {
    let Some(line) = resume_line(agent, session_id, message) else {
        log::warn!("ask {}: refusing to resume a non-plain session id", id);
        return;
    };
    // `claude --resume <id>` only finds a conversation from the directory it
    // ran in (`claude_session::resume_command`), which is not always the
    // asker's cwd (a `cd` before `kova ask`). The transcript records it: no
    // transcript, or a directory gone since, and the answer stays on disk.
    // omp and Pi carry their whole path in `PI_SESSION_FILE`, and their resume
    // command does not need the original directory; the asker's cwd is a fine
    // place to put the tab back.
    let dir = if agent == crate::agent_session::Agent::Claude {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        transcript_cwd(&PathBuf::from(home).join(".claude/projects"), session_id)
    } else {
        cwd.map(str::to_string)
    };
    let Some(dir) = dir.filter(|d| Path::new(d).is_dir()) else {
        log::warn!(
            "ask {}: no transcript (or its directory is gone) for session {}, NOT delivered ({})",
            id,
            session_id,
            kept()
        );
        return;
    };
    if cwd != Some(dir.as_str()) {
        log::info!("ask {}: session {} ran in {}, not in the asker's cwd {}", id, session_id, dir, cwd.unwrap_or("-"));
    }
    let cwd = Some(dir.as_str());
    let body = serde_json::json!({"cmd": "new-tab", "command": line, "cwd": cwd});
    let reply = request(own_socket, body, Duration::from_secs(10));
    let Some(pane) = reply.as_ref().and_then(|r| r.pointer("/data/pane_id")).and_then(|v| v.as_u64()) else {
        log::warn!("ask {}: could not reopen session {} (reply {:?}), {}", id, session_id, reply, kept());
        return;
    };
    // `new-tab` types its command once the shell is ready and leaves it for
    // review, without Enter. Press Enter once the line shows in the pane: an
    // Enter sent earlier would reach the shell before the line.
    let marker = format!("--resume {}", session_id);
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut shown = false;
    while std::time::Instant::now() < deadline {
        let content = request(own_socket, serde_json::json!({"cmd": "get-pane-content", "panes": [pane]}), Duration::from_secs(3));
        let text = content.as_ref().and_then(|c| c.pointer("/data/panes/0/text")).and_then(|t| t.as_str()).unwrap_or("");
        if text.contains(&marker) {
            shown = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let entered = shown
        && reply_ok(&request(own_socket, serde_json::json!({"cmd": "send-keys", "pane_id": pane, "text": "\r"}), Duration::from_secs(5)));
    if !entered {
        log::warn!(
            "ask {}: session {} typed into new pane {} but not started (line shown: {}), {}",
            id,
            session_id,
            pane,
            shown,
            kept()
        );
        return;
    }
    // When Claude Code cannot open the conversation it says "No conversation
    // found with session ID" and exits at once (measured 14/10/2026), so the
    // pane is back at its shell. Read the process, not the screen: the screen
    // also shows the typed line, question and answer included, which may say
    // anything. Still back at the shell after the wait: the answer stays on disk.
    std::thread::sleep(RESUME_CHECK);
    let panes = request(own_socket, serde_json::json!({"cmd": "list-panes"}), Duration::from_secs(3));
    let idle = panes
        .as_ref()
        .and_then(|r| r.get("data"))
        .and_then(|d| d.as_array())
        .and_then(|list| list.iter().find(|p| p.get("id").and_then(|v| v.as_u64()) == Some(pane)))
        .and_then(|p| p.get("is_idle"))
        .and_then(|v| v.as_bool());
    if idle != Some(false) {
        log::warn!(
            "ask {}: claude is not running in pane {} {:?} after reopening session {}, {}",
            id,
            pane,
            RESUME_CHECK,
            session_id,
            kept()
        );
        return;
    }
    log::info!("ask {}: reopened session {} in a new tab (pane {}) in {}", id, session_id, pane, cwd.unwrap_or("-"));
    done();
}

// ─── Kova.app's side: the questions on screen ─────────────────────────────────

/// The questions waiting for the user, oldest first, each with the channel to
/// the connection still waiting for it (`None` for a question reloaded after a
/// restart: its asker is gone, the answer is delivered late). Main thread only.
pub struct Board {
    store: AskStore,
    pending: Vec<(Question, Option<std::sync::mpsc::Sender<crate::ipc::IpcResponse>>)>,
    /// The store's lock, held for the life of the process once taken.
    _owner: Option<std::fs::File>,
    /// Last attempt at the lock while another Kova held it (see `retry_owner`).
    owner_tried: std::time::Instant,
    /// The card must be redrawn on the next tick.
    dirty: bool,
    /// The question the card shows, and its rank when last drawn: the card
    /// follows the id, and falls back on the rank when that question leaves.
    shown: Option<(String, usize)>,
    /// A red line on a question's card: its grant could not be written.
    error: Option<(String, String)>,
    /// The user said "Not now": until this epoch second no card shows, the
    /// questions wait in the queue, and a `kova ask` gets [`PAUSED_PREFIX`].
    paused_until: Option<u64>,
}

/// What set off an answer: grant actions only answer a real click or ⌘Return.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    Click,
    CmdReturn,
    /// Plain Return: never a grant action.
    Return,
    /// `answer-ask`, from any local process.
    Remote,
}

/// Why an answer was not taken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClickRefusal {
    /// The card shown was not the current version (a redraw is pending).
    Stale,
    /// Nothing to do, silently (plain Return on a grant action).
    Ignored,
    Refused(String),
}

/// The answer a click (or `answer-ask`) gives on `question`, and the grant it
/// writes, decided without touching anything: the AppKit handler and
/// `answer-ask` both go through it. `shown_version` is the version the user
/// saw; anything but the current one is refused and the card redrawn.
/// `edits` are the user's versions of the card's `code` blocks (block index,
/// text); the answer carries every code block's final text, and a grant
/// authorizes the payload block as the user left it.
pub fn plan_click(
    question: &Question,
    action: &str,
    text: &str,
    edits: &[(usize, String)],
    shown_version: u32,
    trigger: Trigger,
    now: u64,
) -> Result<Planned, ClickRefusal> {
    if shown_version != question.card_version {
        return Err(ClickRefusal::Stale);
    }
    let text = text.trim().to_string();
    let answer = Answer { id: question.id.clone(), action: action.to_string(), text: text.clone(), card_version: question.card_version, blocks: Vec::new() };
    if question.dismissable() && action == DISMISS {
        return Ok((answer, None, Vec::new()));
    }
    let Some(card) = &question.card else {
        if trigger == Trigger::Remote {
            check_answer(Some(question), &question.id, action, &text).map_err(ClickRefusal::Refused)?;
        }
        return Ok((answer, None, Vec::new()));
    };
    let finals = final_blocks(card, edits);
    let Some(chosen) = card.action(action) else {
        return Err(ClickRefusal::Refused(format!("no action {} in this card", action)));
    };
    if chosen.needs_input && text.is_empty() {
        return Err(ClickRefusal::Refused(format!("{} needs text", action)));
    }
    if card.input.is_none() && !text.is_empty() {
        return Err(ClickRefusal::Refused("\"text\" must be empty: this card has no text field".to_string()));
    }
    let Some(decl) = &chosen.grant else { return Ok((answer, None, finals)) };
    match trigger {
        Trigger::Remote => Err(ClickRefusal::Refused("a grant action can only be clicked".to_string())),
        Trigger::Return => Err(ClickRefusal::Ignored),
        Trigger::Click | Trigger::CmdReturn => {
            let Some(session_id) = question.from.session_id.clone() else {
                return Err(ClickRefusal::Refused("grant requires an asker session id".to_string()));
            };
            let grant = grant_for(&question.id, session_id, decl, &finals, question.card_version, now);
            Ok((answer, Some(grant), finals))
        }
    }
}

/// The grant a click on a grant action writes, for a `kova ask` card or a
/// pane view's card (`id` is then `view:card`): it authorizes the bytes the
/// user left in the payload block, edited or not.
pub fn grant_for(
    id: &str,
    session_id: String,
    decl: &crate::decision_card::GrantDecl,
    finals: &[FinalBlock],
    card_version: u32,
    now: u64,
) -> crate::decision_card::Grant {
    crate::decision_card::Grant {
        question_id: id.to_string(),
        session_id,
        kind: decl.kind.clone(),
        target: decl.target.clone(),
        payload_sha256: finals
            .iter()
            .find(|f| f.block == decl.payload_block)
            .map(|f| crate::decision_card::sha256_hex(f.text.as_bytes()))
            .unwrap_or_else(|| decl.payload_sha256.clone()),
        card_version,
        clicked_at: now,
    }
}

/// What a click gives: the answer, the grant it writes, and the final text of
/// each code block (written to disk with the answer).
pub type Planned = (Answer, Option<crate::decision_card::Grant>, Vec<FinalBlock>);

/// Which question the card shows: the one it showed (`prev`, by id) while it
/// is on the board; else the one now at its old rank — the next in order, or
/// the previous when it was the last; else the first live question, a
/// superseded one only when nothing else waits. `pending` is (id, superseded).
pub fn pick_shown(pending: &[(&str, bool)], prev: Option<(&str, usize)>) -> Option<usize> {
    if pending.is_empty() {
        return None;
    }
    if let Some((id, rank)) = prev {
        if let Some(i) = pending.iter().position(|(p, _)| *p == id) {
            return Some(i);
        }
        return Some(rank.min(pending.len() - 1));
    }
    Some(pending.iter().position(|(_, superseded)| !superseded).unwrap_or(0))
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// What a waiting `kova ask` receives when its asker takes the question back.
pub const WITHDRAWN_BY_ASKER: &str = "withdrawn by the asker";

impl Board {
    /// Takes the store's lock now, before the socket serves anything: a Kova
    /// without it keeps its questions in memory only, so that nothing it writes
    /// can be replayed — shown and delivered a second time — by the one that has it.
    pub fn new() -> Board {
        let store = AskStore::new(AskStore::default_root());
        let owner = store.try_own();
        if owner.is_none() {
            log::warn!(
                "ask: another Kova holds {}; this instance keeps its questions in memory only and replays nothing",
                store.root.display()
            );
        }
        OWNS_STORE.store(owner.is_some(), std::sync::atomic::Ordering::Relaxed);
        // A grant whose session died must not stay forever.
        let pruned = crate::decision_card::prune_grants(&store.root, now_secs());
        if pruned > 0 {
            log::info!("ask: deleted {} expired grant(s)", pruned);
        }
        let answers = prune_answer_files(&store.root, now_secs());
        if answers > 0 {
            log::info!("ask: deleted {} old answer folder(s)", answers);
        }
        Board::with_store(store, owner)
    }

    fn with_store(store: AskStore, owner: Option<std::fs::File>) -> Board {
        Board {
            store,
            pending: Vec::new(),
            _owner: owner,
            owner_tried: std::time::Instant::now(),
            dirty: false,
            shown: None,
            error: None,
            paused_until: None,
        }
    }

    /// The end of the pause, while one runs at `now`.
    pub fn paused_at(&self, now: u64) -> Option<u64> {
        self.paused_until.filter(|until| *until > now)
    }

    /// Pause the cards until `until` (epoch seconds), or end the pause (`None`).
    /// Every `kova ask` still waiting whose answer can reach it later is let go
    /// with [`PAUSED_PREFIX`]: an agent must not sit on a card nobody will read.
    /// One that cannot receive a late answer keeps waiting, or it would lose it.
    pub fn set_pause(&mut self, until: Option<u64>) {
        match until {
            Some(until) => log::info!("ask: cards paused until {} ({} waiting)", local_hhmm(until), self.pending.len()),
            None => log::info!("ask: pause ended ({} waiting)", self.pending.len()),
        }
        self.paused_until = until;
        if self.owns_store() {
            self.store.save_pause(until);
        }
        if let Some(until) = until {
            for (question, responder) in self.pending.iter_mut() {
                if can_deliver_late(&question.from) {
                    if let Some(tx) = responder.take() {
                        let _ = tx.send(crate::ipc::IpcResponse::Error { message: paused_error(until) });
                    }
                }
            }
        }
        self.dirty = true;
    }

    /// A pause whose end has passed is over: the card comes back.
    fn expire_pause(&mut self, now: u64) {
        if self.paused_until.is_some_and(|until| until <= now) {
            self.set_pause(None);
        }
    }

    /// A Kova that started while another held the lock takes it over once that
    /// one is gone (every `OWNER_RETRY` at most, from the tick), then replays
    /// what the other left on disk, and persists from then on. Without this,
    /// Kova.app would stay memory-only for its whole run after a test instance
    /// started first.
    fn retry_owner(&mut self) {
        if self.owns_store() || self.owner_tried.elapsed() < OWNER_RETRY {
            return;
        }
        self.owner_tried = std::time::Instant::now();
        let Some(lock) = self.store.try_own() else { return };
        log::info!("ask: now holding {}, replaying what was kept there", self.store.root.display());
        self._owner = Some(lock);
        OWNS_STORE.store(true, std::sync::atomic::Ordering::Relaxed);
        // What this instance asked while memory-only goes to disk now.
        for (question, _) in &self.pending {
            self.store.save_question(question);
        }
        self.replay();
    }

    fn owns_store(&self) -> bool {
        self._owner.is_some()
    }

    /// At launch: questions asked before a restart come back on the card, and
    /// answers still waiting to be delivered resume their delivery.
    pub fn restore(&mut self) {
        if !self.owns_store() {
            return;
        }
        self.replay();
    }

    /// Put the questions kept on disk on the card, and resume the deliveries.
    fn replay(&mut self) {
        if let Some(until) = self.store.load_pause().filter(|until| *until > now_secs()) {
            log::info!("ask: cards paused until {}, restored from disk", local_hhmm(until));
            self.paused_until = Some(until);
        }
        let undelivered = self.store.undelivered();
        for question in self.store.pending_questions() {
            // Answered just before a quit: the answer is on disk, the question must not come back.
            if undelivered.iter().any(|u| u.question.id == question.id) {
                self.store.remove_question(&question.id);
                continue;
            }
            if self.pending.iter().any(|(q, _)| q.id == question.id) {
                continue;
            }
            log::info!("ask {}: restored from disk ({})", question.id, question.from.name);
            self.pending.push((question, None));
            self.dirty = true;
        }
        for item in undelivered {
            log::info!("ask {}: answer restored from disk, delivering", item.question.id);
            spawn_late(self.store.clone(), item.question, item.answer);
        }
    }

    /// A question arrived on the socket; `responder` reaches its waiting connection.
    pub fn add(&mut self, question: Question, responder: std::sync::mpsc::Sender<crate::ipc::IpcResponse>) {
        log::info!(
            "ask {} from {} session={} pane={}{}: {}",
            question.id,
            question.from.name,
            question.from.session_id.as_deref().unwrap_or("-"),
            question.from.pane_id.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
            if question.card.is_some() { " (card)" } else { "" },
            question.message
        );
        let question = Question { superseded: false, card_version: 1, ..question };
        if self.owns_store() {
            self.store.save_question(&question);
        }
        // Paused: the question waits in the queue, and its asker is told at once
        // rather than after its timeout; the answer reaches it late, as usual.
        let responder = match self.paused_at(now_secs()) {
            Some(until) if can_deliver_late(&question.from) => {
                log::info!("ask {}: queued, the cards are paused until {}", question.id, local_hhmm(until));
                let _ = responder.send(crate::ipc::IpcResponse::Error { message: paused_error(until) });
                None
            }
            _ => Some(responder),
        };
        self.pending.push((question, responder));
        self.dirty = true;
    }

    /// The questions on the card, oldest first (`list-asks`).
    pub fn questions(&self) -> Vec<&Question> {
        self.pending.iter().map(|(q, _)| q).collect()
    }

    /// `answer-ask`: the answer is checked against the question as the card
    /// would have constrained it, then takes the card's own path.
    pub fn answer_checked(&mut self, id: &str, action: String, text: String) -> Result<Answer, String> {
        let Some(version) = self.pending.iter().find(|(q, _)| q.id == id).map(|(q, _)| q.card_version) else {
            return Err(format!("no pending question {}", id));
        };
        match self.submit(id, &action, &text, &[], version, Trigger::Remote) {
            Ok(answer) => Ok(answer),
            Err(ClickRefusal::Refused(message)) => Err(message),
            Err(other) => Err(format!("answer not taken: {:?}", other)),
        }
    }

    /// An answer from the card or from `answer-ask`: planned by [`plan_click`],
    /// its grant written first (nothing is delivered when that fails), then
    /// handed over. A click on a card not yet redrawn is refused and redrawn.
    pub fn submit(
        &mut self,
        id: &str,
        action: &str,
        text: &str,
        edits: &[(usize, String)],
        shown_version: u32,
        trigger: Trigger,
    ) -> Result<Answer, ClickRefusal> {
        let Some(question) = self.pending.iter().map(|(q, _)| q).find(|q| q.id == id) else {
            return Err(ClickRefusal::Ignored);
        };
        let planned = plan_click(question, action, text, edits, shown_version, trigger, now_secs());
        let (mut answer, grant, finals) = match planned {
            Ok(plan) => plan,
            Err(ClickRefusal::Stale) => {
                log::info!("ask {}: click on v{} while v{} is current, redrawing", id, shown_version, question.card_version);
                self.dirty = true;
                return Err(ClickRefusal::Stale);
            }
            Err(other) => return Err(other),
        };
        // The texts first: a grant must never exist for a payload whose file is missing.
        if let Err(e) = write_answer_blocks(&self.store.root, &mut answer, &finals) {
            log::warn!("ask {}: cannot write the answer's blocks: {}", id, e);
            let message = format!("Could not save the edited text: {}", e);
            self.error = Some((id.to_string(), message.clone()));
            self.dirty = true;
            return Err(ClickRefusal::Refused(message));
        }
        if let Some(grant) = grant {
            if let Err(e) = crate::decision_card::write_grant(&self.store.root, &grant) {
                log::warn!("ask {}: cannot write the grant: {}", id, e);
                self.error = Some((id.to_string(), format!("Could not record the authorization: {}", e)));
                self.dirty = true;
                return Err(ClickRefusal::Refused(format!("Could not record the authorization: {}", e)));
            }
            log::info!("ask {}: grant written for {} (v{})", id, grant.target, grant.card_version);
        }
        self.deliver(answer.clone());
        Ok(answer)
    }

    /// `update-ask`: the asker replaces its card. Checks in the spec's order:
    /// the question is on the board, the caller asked it, then the card.
    /// Returns the new `card_version`.
    pub fn update(&mut self, id: &str, session_id: &str, card: &serde_json::Value, message: Option<String>) -> Result<u32, String> {
        let Some(index) = self.pending.iter().position(|(q, _)| q.id == id) else {
            return Err("no such question".to_string());
        };
        let owns = self.owns_store();
        let question = &mut self.pending[index].0;
        if !asked_from(question, session_id) {
            return Err("not the asker of this question".to_string());
        }
        let facts = crate::decision_card::AskerFacts {
            session_id: question.from.session_id.as_deref(),
            claude: question.from.agent == crate::agent_session::Agent::Claude,
        };
        let card = crate::decision_card::validate(card, message.as_deref(), &facts)?;
        question.set_card(card, message);
        question.card_version += 1;
        log::info!("ask {}: card updated to v{}", id, question.card_version);
        if owns {
            self.store.save_question(question);
        }
        let version = question.card_version;
        if self.error.as_ref().is_some_and(|(e, _)| e == id) {
            self.error = None;
        }
        self.dirty = true;
        Ok(version)
    }

    /// `withdraw-ask`: the asker takes one question back. Nothing is delivered.
    pub fn withdraw_one(&mut self, id: &str, session_id: &str) -> Result<(), String> {
        let Some(index) = self.pending.iter().position(|(q, _)| q.id == id) else {
            return Err("no such question".to_string());
        };
        if !asked_from(&self.pending[index].0, session_id) {
            return Err("not the asker of this question".to_string());
        }
        let (_, responder) = self.pending.remove(index);
        log::info!("ask {}: withdrawn by its asker", id);
        if self.owns_store() {
            self.store.remove_question(id);
        }
        if let Some(tx) = responder {
            let _ = tx.send(crate::ipc::IpcResponse::Error { message: WITHDRAWN_BY_ASKER.to_string() });
        }
        self.dirty = true;
        Ok(())
    }

    /// The user wrote in `session_id`'s conversation while its questions were
    /// on the card. That message may answer them or be about something else,
    /// so they stay on the card, marked superseded, with a [`DISMISS`] button;
    /// a click still answers, delivered late like any answer nobody waits for.
    /// A `kova ask` still waiting gets [`WITHDRAWN`] and exits 3, since the
    /// conversation has moved on. Returns how many were newly superseded.
    pub fn withdraw(&mut self, session_id: &str) -> usize {
        let owns = self.owns_store();
        let mut count = 0;
        for (question, responder) in self.pending.iter_mut() {
            if !asked_from(question, session_id) || question.superseded {
                continue;
            }
            log::info!("ask {}: superseded, the user wrote in the conversation; the card keeps it", question.id);
            question.superseded = true;
            if owns {
                self.store.save_question(question);
            }
            if let Some(tx) = responder.take() {
                let _ = tx.send(crate::ipc::IpcResponse::Error { message: WITHDRAWN.to_string() });
            }
            count += 1;
        }
        if count > 0 {
            self.dirty = true;
        }
        count
    }

    /// Hand an answer over: back on the waiting connection, else delivered late.
    /// Nothing happens when no such question waits (already answered elsewhere).
    fn deliver(&mut self, answer: Answer) -> bool {
        let id = answer.id.clone();
        let Some(index) = self.pending.iter().position(|(q, _)| q.id == id) else { return false };
        let (question, responder) = self.pending.remove(index);
        self.dirty = true;
        if self.error.as_ref().is_some_and(|(e, _)| *e == id) {
            self.error = None;
        }
        if question.dismissable() && answer.action == DISMISS {
            log::info!("ask {}: dismissed, nothing delivered", id);
            if self.owns_store() {
                self.store.remove_question(&id);
            }
            return true;
        }
        log::info!("ask {}: answered (v{}): {}", id, answer.card_version, answer.line());
        // On disk before the question leaves it: a quit from here on still delivers it.
        if self.owns_store() {
            self.store.save_delivery(&Undelivered { question: question.clone(), answer: answer.clone() });
            self.store.remove_question(&id);
        }
        let data = serde_json::to_value(&answer).ok();
        let handed = match responder {
            // The connection thread writes it, waits for the ack, and delivers late without one.
            Some(tx) => tx.send(crate::ipc::IpcResponse::Ok { data }).is_ok(),
            None => false,
        };
        if !handed {
            spawn_late(self.store.clone(), question, answer);
        }
        true
    }

    /// The card's ‹ / › (or ⌘[ / ⌘]): show another question. Answers nothing.
    pub fn browse(&mut self, delta: i32) {
        let Some((_, rank)) = self.current_shown() else { return };
        let target = (rank as i64 + delta as i64).clamp(0, self.pending.len() as i64 - 1) as usize;
        if target != rank {
            self.shown = Some((self.pending[target].0.id.clone(), target));
            self.dirty = true;
        }
    }

    /// The question the card shows now, and its rank.
    fn current_shown(&self) -> Option<(String, usize)> {
        let ids: Vec<(&str, bool)> = self.pending.iter().map(|(q, _)| (q.id.as_str(), q.superseded)).collect();
        let prev = self.shown.as_ref().map(|(id, rank)| (id.as_str(), *rank));
        pick_shown(&ids, prev).map(|i| (self.pending[i].0.id.clone(), i))
    }

    /// Show the question the card is on (or hide the card), when something changed.
    pub fn refresh_card(&mut self, mtm: objc2_foundation::MainThreadMarker) {
        self.retry_owner();
        self.expire_pause(now_secs());
        if !self.dirty {
            return;
        }
        self.dirty = false;
        if self.paused_at(now_secs()).is_some() {
            crate::ask_card::pause(mtm, self.pending.len());
            return;
        }
        self.shown = self.current_shown();
        let total = self.pending.len();
        let head = self.shown.as_ref().map(|(_, rank)| (&self.pending[*rank].0, *rank));
        let buttons = head.map(|(q, _)| q.buttons()).unwrap_or_default();
        let on_board: Vec<&str> = self.pending.iter().map(|(q, _)| q.id.as_str()).collect();
        let error = self.error.as_ref().filter(|(id, _)| head.is_some_and(|(q, _)| q.id == *id)).map(|(_, e)| e.as_str());
        let card = head.map(|(q, rank)| crate::ask_card::CardQuestion {
            id: &q.id,
            from: &q.from.name,
            message: &q.message,
            buttons: &buttons,
            input: q.has_input(),
            asked_at: q.asked_at,
            note: q.superseded.then_some(SUPERSEDED_NOTE),
            card: q.card.as_ref(),
            card_version: q.card_version,
            dismissable: q.dismissable(),
            position: rank + 1,
            error,
        });
        crate::ask_card::show(mtm, card.as_ref(), total, &on_board);
    }

    /// The card's answers and arrows since the last tick.
    pub fn drain_card(&mut self) {
        if let Some(seconds) = crate::ask_card::take_snooze() {
            self.set_pause(Some(now_secs() + seconds));
        }
        for nav in crate::ask_card::take_browse() {
            self.browse(nav);
        }
        for a in crate::ask_card::take_answers() {
            match self.submit(&a.id, &a.action, &a.text, &a.edits, a.card_version, a.trigger) {
                Ok(_) | Err(ClickRefusal::Ignored) | Err(ClickRefusal::Stale) => {}
                Err(ClickRefusal::Refused(message)) => {
                    log::info!("ask {}: click refused: {}", a.id, message);
                    // Shown in red on the card unless it is already (a grant that failed).
                    if self.error.is_none() {
                        self.error = Some((a.id.clone(), message));
                    }
                    self.dirty = true;
                }
            }
        }
    }
}

/// What a `kova ask` still waiting receives when its question is withdrawn.
pub const WITHDRAWN: &str = "withdrawn: the user answered in the conversation instead of on the card";

/// Whether `question` was asked from the conversation `session_id`. A question
/// whose asker had no session id never matches.
pub fn asked_from(question: &Question, session_id: &str) -> bool {
    question.from.session_id.as_deref() == Some(session_id)
}

/// Whether an answer coming from elsewhere than the card (`answer-ask`) is one
/// the card could have given for a plain `question` (`None`: no question `id` waits).
pub fn check_answer(question: Option<&Question>, id: &str, action: &str, text: &str) -> Result<(), String> {
    let Some(question) = question else { return Err(format!("no pending question {}", id)) };
    let buttons = question.buttons();
    if !buttons.iter().any(|b| b == action) {
        return Err(format!("\"action\" must be one of: {}", buttons.join(", ")));
    }
    if !question.input && !text.is_empty() {
        return Err("\"text\" must be empty: this question has no text field".to_string());
    }
    Ok(())
}

/// How often a Kova without the ask directory's lock tries to take it.
const OWNER_RETRY: Duration = Duration::from_secs(10);

/// `deliver_late` on its own thread: it may wait hours for a process to end.
fn spawn_late(store: AskStore, question: Question, answer: Answer) {
    let own = crate::ipc::socket_path().to_string_lossy().into_owned();
    let spawned = std::thread::Builder::new()
        .name("ask-delivery".into())
        .spawn(move || deliver_late(&store, &question, &answer, &own));
    if let Err(e) = spawned {
        log::error!("ask: cannot start a delivery thread ({}); the answer stays on disk", e);
    }
}

// ─── The client: `kova ask` ────────────────────────────────────────────────────

pub const USAGE: &str = "\
usage: kova ask \"<message>\" [--actions OK,KO] [--no-input] [--from <name>] [--timeout <seconds>] [--json] [--socket <path>]
       kova ask --card <file|-> [--from <name>] [--timeout <seconds>] [--json] [--socket <path>]
       kova ask --update <question id> --card <file|->
       kova ask --withdraw-id <question id>
       kova ask --dnd [off | <minutes> | <n>h]   read, set or end the pause of the cards (--dnd --help)

Shows a card in Kova with the message, one button per action and a text field, then waits for
the user and prints the answer on stdout: the action, or \"action: text\" when they typed something.

  --actions     buttons, comma-separated (default: a single \"Reply\" button). With a text field, the
                card adds an \"Other\" button after them, and text sent with Enter answers \"Other: text\"
  --no-input    no text field, buttons only
  --from        the name shown on the card (default: the working directory's name)
  --timeout     seconds to wait. Default: 540 inside a Claude Code conversation (under the Bash tool's
                600 s limit; run kova ask with the Bash timeout set to 600000), no limit anywhere else,
                since only a Claude conversation can receive a late answer
  --card        a decision card (JSON, docs/decision-cards-spec.md): title, blocks, named actions,
                optional text field and grant action. Excludes a message, --actions and --no-input.
                The answer is the action id, or \"id: text\"
  --update      replace the card of a question this conversation asked (its id is in list-asks or in
                the timeout message); the card's version goes up and the click carries it
  --withdraw-id take back one question this conversation asked. --update and --withdraw-id return at
                once: exit 0 on success, the error on stderr and exit 1 otherwise
  --json        print the answer as JSON (with card_version)
  --socket      the Kova to ask (default: $KOVA_SOCKET, else the running Kova.app)

Exit codes: 0 answered · 2 no answer within the timeout · 3 the user wrote in the conversation
instead (their message is the answer; the card stays, marked, and a click there still arrives later
as a message) · 4 the user paused the cards (\"Not now\", see kova ask --dnd): the question waits in
the queue and shows when the pause ends, its answer arrives later as a message · 1 error, or Kova
quit while you waited.
On a timeout the question stays on the user's screen. When they answer, Kova delivers the answer to
this conversation itself: typed into its Kova pane if it is still open there, else by reopening it
in a new Kova tab with `claude --resume`. So after exit 2, end your turn: the answer will come as a message.";

struct CliArgs {
    message: String,
    actions: Vec<String>,
    input: bool,
    /// `--card <file|->`: a decision card instead of a message.
    card: Option<String>,
    /// `--update <id>` / `--withdraw-id <id>`: act on a question already asked.
    update: Option<String>,
    withdraw_id: Option<String>,
    from: Option<String>,
    timeout: Option<u64>,
    json: bool,
    socket: Option<String>,
}

fn parse_cli(args: &[String]) -> Result<CliArgs, String> {
    let mut out = CliArgs {
        message: String::new(),
        actions: Vec::new(),
        input: true,
        card: None,
        update: None,
        withdraw_id: None,
        from: None,
        timeout: None,
        json: false,
        socket: None,
    };
    let mut message: Option<String> = None;
    let mut plain_flags = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().cloned().ok_or_else(|| format!("kova ask: {} needs a value\n\n{}", a, USAGE));
        match a.as_str() {
            "--card" => out.card = Some(value()?),
            "--update" => out.update = Some(value()?),
            "--withdraw-id" => out.withdraw_id = Some(value()?),
            "--actions" => {
                plain_flags = true;
                out.actions = value()?.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
            }
            "--no-input" => {
                plain_flags = true;
                out.input = false
            }
            "--from" => out.from = Some(value()?),
            "--timeout" => {
                let raw = value()?;
                match raw.parse::<u64>() {
                    Ok(t) if t > 0 => out.timeout = Some(t),
                    _ => return Err(format!("kova ask: --timeout takes seconds, got '{}'", raw)),
                }
            }
            "--json" => out.json = true,
            "--socket" => out.socket = Some(value()?),
            flag if flag.starts_with("--") => return Err(format!("kova ask: unknown flag {}\n\n{}", flag, USAGE)),
            _ => {
                if message.is_some() {
                    return Err(format!(
                        "kova ask: unexpected extra argument '{}': the message is one quoted string, and so is an --actions list whose labels contain spaces (--actions \"Yes please,No\")\n\n{}",
                        a, USAGE
                    ));
                }
                message = Some(a.clone());
            }
        }
    }
    if let Some(id) = out.update.as_ref().or(out.withdraw_id.as_ref()) {
        if out.update.is_some() && out.withdraw_id.is_some() {
            return Err("kova ask: --update and --withdraw-id exclude each other".to_string());
        }
        if out.timeout.is_some() || out.from.is_some() {
            return Err("kova ask: --timeout and --from do not apply to --update or --withdraw-id".to_string());
        }
        if !is_safe_question_id(id) {
            return Err(format!("kova ask: '{}' is not a question id (a UUID)", id));
        }
        if message.is_some() || plain_flags {
            return Err("kova ask: --update and --withdraw-id take no message, --actions or --no-input".to_string());
        }
        if out.update.is_some() && out.card.is_none() {
            return Err("kova ask: --update needs --card <file|->".to_string());
        }
        if out.withdraw_id.is_some() && out.card.is_some() {
            return Err("kova ask: --withdraw-id takes no --card".to_string());
        }
        return Ok(out);
    }
    if out.card.is_some() {
        if message.is_some() || plain_flags {
            return Err("kova ask: --card excludes a message, --actions and --no-input".to_string());
        }
        return Ok(out);
    }
    out.message = message.filter(|m| !m.trim().is_empty()).ok_or_else(|| USAGE.to_string())?;
    Ok(out)
}

/// The executable path of `pid`.
fn exe_path(pid: i32) -> Option<String> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32) };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

/// The Kova to ask when the caller names none and runs outside a Kova pane (a
/// routine started by launchd): the live `/tmp/kova-<pid>.sock` served by
/// Kova.app itself. A library host serves the same path shape, hence the executable check.
pub(crate) fn discover_socket() -> Option<String> {
    let mut found: Vec<(bool, i32, String)> = Vec::new();
    for entry in std::fs::read_dir("/tmp").ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(pid) = name.strip_prefix("kova-").and_then(|r| r.strip_suffix(".sock")).and_then(|p| p.parse::<i32>().ok()) else {
            continue;
        };
        let Some(exe) = exe_path(pid) else { continue };
        if !exe.ends_with("/kova") {
            continue;
        }
        let bundled = exe.contains("/Kova.app/");
        found.push((bundled, pid, entry.path().to_string_lossy().into_owned()));
    }
    // Kova.app first, then the oldest process: the user's long-running Kova
    // rather than a test instance started from the build directory.
    found.sort_by_key(|(bundled, pid, _)| (!*bundled, *pid));
    found.into_iter().map(|(_, _, path)| path).next()
}

const WITHDRAW_USAGE: &str = "kova ask --withdraw [--session <id> | --session -] [--socket <path>]

Take back the questions a Claude Code conversation still has on the card, because the user answered
in the conversation instead. Meant for a UserPromptSubmit hook: `--session -` reads the hook's JSON
input on stdin and takes its `session_id`; without --session, $CLAUDE_CODE_SESSION_ID. Prints
nothing and exits 0 whatever happens (no Kova, an older Kova, nothing to withdraw), so a prompt is
never held up by it.";

/// The session `kova ask --withdraw` names: `-` is a hook's JSON input.
fn withdraw_session(flag: Option<&str>, stdin: impl FnOnce() -> String, env: Option<String>) -> Option<String> {
    let id = match flag {
        Some("-") => serde_json::from_str::<serde_json::Value>(&stdin())
            .ok()?
            .get("session_id")?
            .as_str()?
            .to_string(),
        Some(id) => id.to_string(),
        None => env?,
    };
    is_safe_session_id(&id).then_some(id)
}

/// `kova ask --withdraw …`: always 0, see [`WITHDRAW_USAGE`].
const DND_USAGE: &str = "kova ask --dnd [off | <minutes> | <n>m | <n>h] [--json] [--socket <path>]

Reads, sets or ends the pause of the cards, the one the card's \"Not now\" button starts (20 min,
1 h or 3 h). While it runs no card shows: the questions wait in the queue and all come back when it
ends, and a `kova ask` returns at once with exit 4. With no value it only reads.

Exit codes: 0 not paused · 4 paused (stdout says until when and how many questions wait) · 1 error.
An agent checks it before work that would end on a card: exit 4 means not now, keep it for later.";

/// `kova ask --dnd …`: returns the process exit code.
fn run_dnd(args: &[String]) -> i32 {
    let mut minutes: Option<u64> = None;
    let mut socket: Option<String> = None;
    let mut json = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--socket" => socket = it.next().cloned(),
            "--json" => json = true,
            "-h" | "--help" => {
                println!("{}", DND_USAGE);
                return 0;
            }
            value if minutes.is_none() => match parse_dnd_minutes(value) {
                Ok(m) => minutes = Some(m),
                Err(e) => {
                    eprintln!("{}", e);
                    return 1;
                }
            },
            other => {
                eprintln!("kova ask --dnd: unexpected '{}'\n{}", other, DND_USAGE);
                return 1;
            }
        }
    }
    let mut body = serde_json::json!({"cmd": "ask-dnd"});
    if let Some(m) = minutes {
        body["minutes"] = serde_json::json!(m);
    }
    let first = socket.clone().or_else(|| std::env::var("KOVA_SOCKET").ok().filter(|s| !s.is_empty())).or_else(discover_socket);
    let Some(first) = first else {
        eprintln!("kova ask --dnd: no running Kova found (no $KOVA_SOCKET, no /tmp/kova-<pid>.sock served by Kova.app)");
        return 1;
    };
    let mut reply = request(&first, body.clone(), Duration::from_secs(2));
    // A library host's socket has no card: the pause lives on Kova.app.
    let error = |r: &Option<serde_json::Value>| {
        r.as_ref().and_then(|r| r.get("error")).and_then(|e| e.as_str()).map(String::from)
    };
    let unsupported = error(&reply).is_some_and(|e| e == "command not supported by this host" || e.starts_with("unknown command"));
    if unsupported && socket.is_none() {
        if let Some(app) = discover_socket().filter(|s| *s != first) {
            reply = request(&app, body, Duration::from_secs(2));
        }
    }
    let Some(reply) = reply else {
        eprintln!("kova ask --dnd: Kova is not answering on {}", first);
        return 1;
    };
    if let Some(e) = error(&Some(reply.clone())) {
        if e.starts_with("unknown command") {
            eprintln!("kova ask --dnd: the running Kova predates the pause ({}). It learns it at its next launch.", e);
        } else {
            eprintln!("kova ask --dnd: {}", e);
        }
        return 1;
    }
    let data = reply.get("data").cloned().unwrap_or_default();
    let until = data.get("paused_until").and_then(|u| u.as_u64());
    let waiting = data.get("waiting").and_then(|w| w.as_u64()).unwrap_or(0);
    if json {
        println!("{}", data);
    } else {
        match until {
            Some(until) => println!("paused until {} ({} question(s) waiting)", local_hhmm(until), waiting),
            None => println!("not paused ({} question(s) on the card)", waiting),
        }
    }
    if until.is_some() { 4 } else { 0 }
}

fn run_withdraw(args: &[String]) -> i32 {
    let mut session: Option<String> = None;
    let mut socket: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--session" => session = it.next().cloned(),
            "--socket" => socket = it.next().cloned(),
            _ => {
                eprintln!("{}", WITHDRAW_USAGE);
                return 0;
            }
        }
    }
    let read_stdin = || {
        let mut input = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut input);
        input
    };
    let Some(session_id) = withdraw_session(session.as_deref(), read_stdin, std::env::var("CLAUDE_CODE_SESSION_ID").ok()) else {
        return 0;
    };
    let body = serde_json::json!({"cmd": "withdraw-asks", "session_id": session_id});
    let first = socket.or_else(|| std::env::var("KOVA_SOCKET").ok().filter(|s| !s.is_empty())).or_else(discover_socket);
    let Some(first) = first else { return 0 };
    let reply = request(&first, body.clone(), Duration::from_secs(1));
    // A library host's socket has no card: the questions are on Kova.app.
    let refused = reply.as_ref().and_then(|r| r.get("error")).and_then(|e| e.as_str())
        .is_some_and(|e| e == "command not supported by this host");
    if refused {
        if let Some(app) = discover_socket().filter(|s| *s != first) {
            let _ = request(&app, body, Duration::from_secs(1));
        }
    }
    0
}

/// The card file (`-` = stdin), parsed: it is sent re-serialized compact.
pub(crate) fn read_card(path: &str) -> Result<serde_json::Value, String> {
    let raw = if path == "-" {
        let mut input = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).map_err(|e| format!("kova ask: cannot read the card on stdin: {}", e))?;
        input
    } else {
        std::fs::read_to_string(path).map_err(|e| format!("kova ask: cannot read {}: {}", path, e))?
    };
    serde_json::from_str(&raw).map_err(|e| format!("kova ask: the card is not JSON: {}", e))
}

/// One request that returns at once, sent to the Kova that holds the card:
/// the caller's socket, else Kova.app when that one is a library host.
pub(crate) fn one_shot(cli_socket: Option<String>, asker_socket: Option<String>, body: serde_json::Value) -> Result<serde_json::Value, String> {
    let first = cli_socket.clone().or(asker_socket).or_else(discover_socket).ok_or_else(|| {
        "kova ask: no running Kova found (no $KOVA_SOCKET, no /tmp/kova-<pid>.sock served by Kova.app)".to_string()
    })?;
    let mut reply = request(&first, body.clone(), Duration::from_secs(5));
    let error = reply.as_ref().and_then(|r| r.get("error")).and_then(|e| e.as_str()).unwrap_or_default().to_string();
    if cli_socket.is_none() && (error == "command not supported by this host" || error.starts_with("unknown command")) {
        if let Some(app) = discover_socket().filter(|s| *s != first) {
            reply = request(&app, body, Duration::from_secs(5));
        }
    }
    let reply = reply.ok_or_else(|| format!("kova ask: Kova is not answering on {}", first))?;
    if reply_ok(&Some(reply.clone())) {
        Ok(reply)
    } else {
        Err(format!("kova ask: {}", reply.get("error").and_then(|e| e.as_str()).unwrap_or("unknown error")))
    }
}

/// `kova ask --update` / `--withdraw-id`: act on a question this conversation asked.
fn run_on_question(cli: &CliArgs, asker: &Asker, card: Option<serde_json::Value>) -> i32 {
    let Some(session_id) = asker.session_id.clone() else {
        eprintln!("kova ask: no session id in the environment");
        return 1;
    };
    let body = match (&cli.update, &cli.withdraw_id) {
        (Some(id), _) => serde_json::json!({"cmd": "update-ask", "id": id, "session_id": session_id, "card": card}),
        (_, Some(id)) => serde_json::json!({"cmd": "withdraw-ask", "id": id, "session_id": session_id}),
        _ => return 1,
    };
    match one_shot(cli.socket.clone(), asker.kova_socket.clone(), body) {
        Ok(_) => 0,
        Err(msg) => {
            eprintln!("{}", msg);
            1
        }
    }
}

/// `kova ask …`: returns the process exit code.
pub fn run_cli(args: &[String]) -> i32 {
    if args.first().map(String::as_str) == Some("--withdraw") {
        if args.iter().any(|a| a == "-h" || a == "--help") {
            println!("{}", WITHDRAW_USAGE);
            return 0;
        }
        return run_withdraw(&args[1..]);
    }
    if args.first().map(String::as_str) == Some("--dnd") {
        return run_dnd(&args[1..]);
    }
    let fail = |msg: &str, code: i32| -> i32 {
        eprintln!("{}", msg);
        code
    };
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{}", USAGE);
        return 0;
    }
    let cli = match parse_cli(args) {
        Ok(c) => c,
        Err(msg) => return fail(&msg, 1),
    };
    let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "/".into());
    let asker = Asker::from_env(|k| std::env::var(k).ok(), cli.from.clone(), &cwd);
    let card = match cli.card.as_deref().map(read_card) {
        None => None,
        Some(Ok(card)) => Some(card),
        Some(Err(msg)) => return fail(&msg, 1),
    };
    if cli.update.is_some() || cli.withdraw_id.is_some() {
        return run_on_question(&cli, &asker, card);
    }
    let mut socket = match cli.socket.clone().or_else(|| asker.kova_socket.clone()).or_else(discover_socket) {
        Some(s) => s,
        None => return fail("kova ask: no running Kova found (no $KOVA_SOCKET, no /tmp/kova-<pid>.sock served by Kova.app)", 1),
    };
    let late = can_deliver_late(&asker);
    let wait = cli.timeout.or(if late { Some(540) } else { None });
    // A card travels as the raw JSON the agent wrote (Kova validates it), with
    // the fallback fields filled from it for a Kova that predates cards.
    let (message, actions, input) = match &card {
        Some(card) => {
            let (actions, input) = crate::decision_card::legacy_fields(card);
            (crate::decision_card::derive_message(card), actions, input)
        }
        None => (cli.message.clone(), cli.actions.clone(), cli.input),
    };
    let question = Question {
        id: new_question_id(),
        message,
        actions,
        input,
        from: asker,
        asked_at: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        superseded: false,
        card: None,
        card_version: 1,
    };

    let mut question_json = serde_json::to_value(&question).unwrap_or_default();
    if let Some(card) = &card {
        question_json["card"] = card.clone();
    }
    let body = serde_json::json!({"cmd": "ask", "question": question_json});
    let mut fallback_tried = false;
    let (stream, line, read) = loop {
        let stream = match UnixStream::connect(&socket) {
            Ok(s) => s,
            Err(e) => return fail(&format!("kova ask: Kova is not answering on {}: {}", socket, e), 1),
        };
        let mut writer = &stream;
        if writeln!(writer, "{}", body).is_err() {
            return fail("kova ask: could not send the question to Kova", 1);
        }
        let _ = stream.set_read_timeout(wait.map(Duration::from_secs));
        let mut line = String::new();
        let read = BufReader::new(&stream).read_line(&mut line);
        // A pane of a library host names the host's socket, which serves
        // panes but no card: ask Kova.app instead. The asker keeps the host's socket
        // and pane, which is where a late answer is typed.
        // Same for a socket that predates `ask` altogether (an older host build).
        let error = serde_json::from_str::<serde_json::Value>(line.trim())
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
            .unwrap_or_default();
        let unsupported = error == "command not supported by this host" || error == "unknown command: ask";
        if unsupported && cli.socket.is_none() && !fallback_tried {
            fallback_tried = true;
            if let Some(app) = discover_socket().filter(|s| *s != socket) {
                socket = app;
                continue;
            }
        }
        break (stream, line, read);
    };
    let mut writer = &stream;
    let waited = wait.unwrap_or(0);
    match read {
        Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
            return if late {
                fail(&format!(
                    "kova ask: no answer within {} s. The question stays on the user's screen (id {}); \
                     their answer will reach this conversation as a message. End your turn now.",
                    waited, question.id
                ), 2)
            } else {
                fail(&format!(
                    "kova ask: no answer within {} s, and the answer cannot reach you later (not a Claude Code \
                     conversation). The question stays on screen; its answer will only be in Kova's log.",
                    waited
                ), 2)
            };
        }
        Err(e) => return fail(&format!("kova ask: reading Kova's answer failed: {}", e), 1),
        Ok(0) => {
            return fail(&format!(
                "kova ask: Kova closed before the user answered. It keeps the question and shows it again when it restarts; {}",
                if late { "the answer will reach this conversation as a message. End your turn now." } else { "but its answer cannot reach you later. Ask again." }
            ), 1);
        }
        Ok(_) => {}
    }
    let reply: serde_json::Value = match serde_json::from_str(line.trim()) {
        Ok(v) => v,
        Err(_) => return fail(&format!("kova ask: unreadable answer from Kova: {}", line.trim()), 1),
    };
    if reply.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        let err = reply.get("error").and_then(|v| v.as_str()).unwrap_or("unknown error");
        if err.starts_with("unknown command") {
            return fail(&format!(
                "kova ask: the Kova on {} predates `ask` ({}). It answers from its next launch.",
                socket, err
            ), 1);
        }
        if let Some(until) = paused_until_of(err) {
            return if late {
                fail(&format!(
                    "kova ask: the user paused the cards until {} (\"Not now\"): do not disturb them. The question \
                     is kept (id {}) and shows when the pause ends; the answer will reach this conversation as a \
                     message. Do not ask again. End your turn now.",
                    local_hhmm(until), question.id
                ), 4)
            } else {
                fail(&format!("kova ask: the user paused the cards until {}. Ask again after that.", local_hhmm(until)), 4)
            };
        }
        if err == WITHDRAWN {
            return fail(
                "kova ask: the user wrote in the conversation instead of on the card. \
                 End your turn now: their message is the answer. The card stays, marked; if they still \
                 answer there, it arrives later as a message.",
                3,
            );
        }
        return fail(&format!("kova ask: Kova refused the question: {}", err), 1);
    }
    let Some(answer) = reply.get("data").and_then(|d| serde_json::from_value::<Answer>(d.clone()).ok()) else {
        return fail(&format!("kova ask: unreadable answer from Kova: {}", line.trim()), 1);
    };
    // Handed over only once acknowledged: Kova delivers the answer late otherwise.
    let _ = writeln!(writer, "ack");
    let _ = writer.flush();
    if cli.json {
        println!("{}", serde_json::to_string(&answer).unwrap_or_default());
    } else {
        println!("{}", answer.line());
    }
    0
}

/// The colour of an agent on the card, as RGB in 0…1: one of a few vivid hues,
/// always the same for the same name (FNV-1a over its UTF-8, so stable across
/// launches). Tells at a glance who asks.
pub fn agent_rgb(name: &str) -> (f64, f64, f64) {
    const PALETTE: [(f64, f64, f64); 8] = [
        (0.38, 0.40, 0.98), // indigo
        (0.16, 0.73, 0.55), // green
        (0.98, 0.45, 0.25), // orange
        (0.90, 0.30, 0.60), // pink
        (0.20, 0.62, 0.95), // blue
        (0.66, 0.40, 0.95), // violet
        (0.95, 0.70, 0.15), // amber
        (0.15, 0.75, 0.80), // teal
    ];
    let mut hash: u32 = 2_166_136_261;
    for byte in name.bytes() {
        hash = (hash ^ byte as u32).wrapping_mul(16_777_619);
    }
    PALETTE[(hash % PALETTE.len() as u32) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asker() -> Asker {
        Asker {
            name: "routine".into(),
            agent: crate::agent_session::Agent::Claude,
            session_id: Some("abc-123".into()),
            pane_id: Some(7),
            kova_socket: Some("/tmp/kova-1.sock".into()),
            cwd: Some("/tmp/w".into()),
        }
    }

    fn question(id: &str) -> Question {
        Question { id: id.into(), message: "Send it?".into(), actions: vec!["OK".into(), "KO".into()], input: true, from: asker(), asked_at: 5, superseded: false, card: None, card_version: 1 }
    }

    #[test]
    fn withdraw_takes_the_session_from_the_flag_the_hook_input_or_the_environment() {
        let none = || -> String { panic!("stdin read without --session -") };
        assert_eq!(withdraw_session(Some("abc-1"), none, None), Some("abc-1".into()));
        assert_eq!(withdraw_session(None, none, Some("env-2".into())), Some("env-2".into()));
        assert_eq!(withdraw_session(None, none, None), None);
        let hook = || r#"{"session_id":"hook-3","prompt":"deploy"}"#.to_string();
        assert_eq!(withdraw_session(Some("-"), hook, None), Some("hook-3".into()));
        assert_eq!(withdraw_session(Some("-"), || "not json".into(), None), None);
        assert_eq!(withdraw_session(Some("a;b"), none, None), None, "an odd id is dropped, not sent");
    }

    #[test]
    fn a_question_belongs_to_the_conversation_that_asked_it() {
        let q = question("0f8fad5b-d9cb-469f-a165-70867728950e");
        let own = q.from.session_id.clone().expect("the test asker has a session");
        assert!(asked_from(&q, &own));
        assert!(!asked_from(&q, "someone-else"));
        let mut anonymous = q.clone();
        anonymous.from.session_id = None;
        assert!(!asked_from(&anonymous, &own));
    }

    #[test]
    fn delivery_prefers_the_live_pane() {
        assert_eq!(delivery(&asker(), true, None), Delivery::Pane { socket: "/tmp/kova-1.sock".into(), pane_id: 7 });
        // Open in the pane wins over "open in some process": it is that process.
        assert_eq!(delivery(&asker(), true, Some(42)), Delivery::Pane { socket: "/tmp/kova-1.sock".into(), pane_id: 7 });
    }

    #[test]
    fn delivery_resumes_when_the_pane_no_longer_holds_the_conversation() {
        assert_eq!(
            delivery(&asker(), false, None),
            Delivery::Resume {
                agent: crate::agent_session::Agent::Claude,
                session_id: "abc-123".into(),
                cwd: Some("/tmp/w".into()),
            }
        );
        let headless = Asker {
            pane_id: None,
            kova_socket: None,
            ..asker()
        };
        assert_eq!(
            delivery(&headless, true, None),
            Delivery::Resume {
                agent: crate::agent_session::Agent::Claude,
                session_id: "abc-123".into(),
                cwd: Some("/tmp/w".into()),
            }
        );
    }

    #[test]
    fn delivery_never_forks_a_conversation_open_elsewhere() {
        assert_eq!(delivery(&asker(), false, Some(42)), Delivery::OpenElsewhere { pid: 42 });
    }

    #[test]
    fn delivery_none_without_any_way_back() {
        let script = Asker { name: "script".into(), ..Default::default() };
        assert_eq!(delivery(&script, false, None), Delivery::None);
        // A pane without a session to compare is not enough: it may run anything by now.
        let pane_only = Asker { name: "script".into(), pane_id: Some(3), kova_socket: Some("/tmp/k.sock".into()), ..Default::default() };
        assert_eq!(delivery(&pane_only, true, None), Delivery::None);
        assert!(!can_deliver_late(&pane_only));
    }

    #[test]
    fn asker_from_env_refuses_unsafe_session_ids() {
        let env = |k: &str| match k {
            "CLAUDE_CODE_SESSION_ID" => Some("e2e1d3cf-7cc0-48a2".to_string()),
            "KOVA_PANE_ID" => Some("3".to_string()),
            "KOVA_SOCKET" => Some("/tmp/k.sock".to_string()),
            _ => None,
        };
        let a = Asker::from_env(env, None, "/Users/x/projects/app");
        assert_eq!(
            a,
            Asker {
                name: "app".into(),
                agent: crate::agent_session::Agent::Claude,
                session_id: Some("e2e1d3cf-7cc0-48a2".into()),
                pane_id: Some(3),
                kova_socket: Some("/tmp/k.sock".into()),
                cwd: Some("/Users/x/projects/app".into()),
            }
        );
        let evil = Asker::from_env(
            |k| (k == "CLAUDE_CODE_SESSION_ID").then(|| "x; rm -rf ~".to_string()),
            Some("n".into()),
            "/",
        );
        assert_eq!(evil.session_id, None);
        assert!(!is_safe_session_id("a\nb"));
        assert!(!is_safe_session_id(""));
    }

    #[test]
    fn a_tazu_child_names_its_agent_and_conversation_from_tazu_session_id() {
        let tazu = |k: &str| match k {
            "TAZU_SESSION_ID" => Some("20261101-132200-ab12".to_string()),
            _ => None,
        };
        let asker = Asker::from_env(tazu, None, "/w");
        assert_eq!(asker.agent, crate::agent_session::Agent::Tazu);
        assert_eq!(asker.session_id.as_deref(), Some("20261101-132200-ab12"));
        assert_eq!(
            resume_line(crate::agent_session::Agent::Tazu, "20261101-132200-ab12", "ok").as_deref(),
            Some("tazu --resume 20261101-132200-ab12 'ok'")
        );
        let odd = |k: &str| (k == "TAZU_SESSION_ID").then(|| "a;b c".to_string());
        assert_eq!(Asker::from_env(odd, None, "/w").session_id, None);
    }

    #[test]
    fn an_omp_child_names_its_agent_and_conversation_from_pi_session_file() {
        let path = "/Users/x/.omp/agent/sessions/-w/2026-10-27T15-54-09-646Z_01a17e11-ab6e-76b2-a4f3-e856a40686d7.jsonl";
        let omp = |k: &str| match k {
            "OMPCODE" => Some("1".to_string()),
            "PI_SESSION_FILE" => Some(path.to_string()),
            _ => None,
        };
        let asker = Asker::from_env(omp, None, "/w");
        assert_eq!(asker.agent, crate::agent_session::Agent::Omp);
        assert_eq!(
            asker.session_id.as_deref(),
            Some("01a17e11-ab6e-76b2-a4f3-e856a40686d7")
        );

        // A Pi child inherits the same variable; the tell is the omp path.
        let pi = |k: &str| match k {
            "PI_SESSION_FILE" => Some(
                "/Users/x/.pi/agent/sessions/--w--/t_01a17e11-ab6e-76b2-a4f3-e856a40686d7.jsonl"
                    .to_string(),
            ),
            _ => None,
        };
        assert_eq!(
            Asker::from_env(pi, None, "/w").agent,
            crate::agent_session::Agent::Pi
        );

        // Claude's own id wins when both are present.
        let both = |k: &str| match k {
            "CLAUDE_CODE_SESSION_ID" => Some("claude-id".to_string()),
            "OMPCODE" => Some("1".to_string()),
            "PI_SESSION_FILE" => Some(path.to_string()),
            _ => None,
        };
        assert_eq!(
            Asker::from_env(both, None, "/w").agent,
            crate::agent_session::Agent::Claude
        );
    }

    #[test]
    fn answers_from_elsewhere_are_held_to_the_card() {
        let id = "0f8fad5b-d9cb-469f-a165-70867728950e";
        assert_eq!(check_answer(None, id, "OK", ""), Err(format!("no pending question {}", id)));
        let mut q = question(id);
        q.actions = vec!["OK".into(), "KO".into()];
        assert_eq!(check_answer(Some(&q), id, "OK", "fine"), Ok(()));
        assert_eq!(
            check_answer(Some(&q), id, "Other", "my own answer"),
            Ok(()),
            "a text field brings Other"
        );
        assert_eq!(
            check_answer(Some(&q), id, "Reply", ""),
            Err("\"action\" must be one of: OK, KO, Other".into())
        );
        assert_eq!(
            check_answer(Some(&q), id, "ok", ""),
            Err("\"action\" must be one of: OK, KO, Other".into())
        );
        q.actions.clear();
        assert_eq!(check_answer(Some(&q), id, "Reply", "x"), Ok(()), "no actions: the single Reply button");
        q.input = false;
        assert_eq!(check_answer(Some(&q), id, "Reply", ""), Ok(()));
        assert!(check_answer(Some(&q), id, "Reply", "x").unwrap_err().starts_with("\"text\" must be empty"));
    }

    #[test]
    fn question_ids_are_uuids_only() {
        let id = new_question_id();
        assert!(is_safe_question_id(&id), "{id}");
        assert_ne!(id, new_question_id());
        assert!(!is_safe_question_id("../x"));
        assert!(!is_safe_question_id("q1"));
        assert!(!is_safe_question_id("0000000g-0000-4000-8000-000000000000"));
    }

    #[test]
    fn answer_line_and_late_message_stay_on_one_line() {
        assert_eq!(Answer { id: "1".into(), action: "OK".into(), text: String::new(), card_version: 1, blocks: vec![] }.line(), "OK");
        let answer = Answer { id: "1".into(), action: "OK".into(), text: "but fix\r\nthat part".into(), card_version: 1, blocks: vec![] };
        let q = Question { message: "I drafted\nthis reply".into(), ..question("1") };
        let msg = late_answer_message(&q, &answer);
        assert!(!msg.contains('\n') && !msg.contains('\r'));
        assert_eq!(msg, "[Kova] The user answered your question \"I drafted this reply\": OK: but fix that part");
    }

    #[test]
    fn question_round_trips_as_json_with_defaults() {
        let q = question("q1");
        let json = serde_json::to_value(&q).unwrap();
        assert_eq!(serde_json::from_value::<Question>(json).unwrap(), q);
        let bare: Question = serde_json::from_str(r#"{"id":"q","message":"m","from":{"name":"n"}}"#).unwrap();
        assert!(bare.input);
        assert_eq!(bare.buttons(), vec!["Reply".to_string()]);
    }

    #[test]
    fn a_card_with_a_text_field_always_offers_other() {
        let mut q = question("q1");
        q.actions = vec!["A".into(), "B".into()];
        assert_eq!(
            q.buttons(),
            vec!["A", "B", OTHER],
            "typed text must not ride on A"
        );
        q.actions = vec!["Retry".into(), OTHER.into()];
        assert_eq!(q.buttons(), vec!["Retry", OTHER], "never twice");
        q.actions = vec!["A".into(), "B".into()];
        q.input = false;
        assert_eq!(
            q.buttons(),
            vec!["A", "B"],
            "no field, nothing to type, no Other"
        );
    }

    #[test]
    fn descends_walks_the_parent_chain() {
        let me = std::process::id() as i32;
        let parent = parent_pid(me).expect("this process has a parent");
        assert!(descends(me, parent));
        assert!(descends(me, me), "exec claude: the shell is the conversation");
        assert_eq!(parent_pid(999_999), None);
    }

    #[test]
    fn a_sleeping_conversation_is_found_and_then_seen_running() {
        let asleep = vec![
            serde_json::json!({"id": 3, "agent_session_id": null, "dormant_session_id": "other"}),
            serde_json::json!({"id": 7, "agent_session_id": null, "dormant_session_id": "abc-123"}),
        ];
        assert_eq!(sleeping_pane(&asleep, "abc-123"), Some(7));
        assert_eq!(sleeping_pane(&asleep, "nowhere"), None);
        assert!(
            !reports_session(&asleep, 7, crate::agent_session::Agent::Claude, "abc-123"),
            "asleep is not running"
        );

        let woken = vec![
            serde_json::json!({"id": 7, "agent": "omp", "agent_session_id": "abc-123", "dormant_session_id": null}),
        ];
        assert!(reports_session(
            &woken,
            7,
            crate::agent_session::Agent::Omp,
            "abc-123"
        ));
        assert!(
            !reports_session(&woken, 7, crate::agent_session::Agent::Claude, "abc-123"),
            "another agent's id is not it"
        );
        assert!(!reports_session(
            &woken,
            8,
            crate::agent_session::Agent::Omp,
            "abc-123"
        ));
        assert_eq!(sleeping_pane(&woken, "abc-123"), None);
    }

    #[test]
    fn pane_holds_session_takes_either_proof() {
        let panes = vec![
            serde_json::json!({"id": 7, "pid": 100, "agent": "claude", "agent_session_id": "abc-123"}),
            serde_json::json!({"id": 8, "pid": 200, "agent": "omp", "agent_session_id": "def-456"}),
        ];
        let never = |_: i32, _: i32| false;
        assert_eq!(
            pane_holds_session(
                &panes,
                7,
                crate::agent_session::Agent::Claude,
                "abc-123",
                None,
                never
            )
            .as_deref(),
            Some("reported by Kova")
        );
        assert_eq!(
            pane_holds_session(
                &panes,
                7,
                crate::agent_session::Agent::Omp,
                "abc-123",
                None,
                never
            ),
            None
        );
        assert_eq!(
            pane_holds_session(
                &panes,
                8,
                crate::agent_session::Agent::Omp,
                "def-456",
                None,
                never
            )
            .as_deref(),
            Some("reported by Kova")
        );
        let from_200 = |pid: i32, shell: i32| pid == 555 && shell == 200;
        assert!(
            pane_holds_session(
                &panes,
                8,
                crate::agent_session::Agent::Claude,
                "abc-123",
                Some(555),
                from_200
            )
            .unwrap()
            .contains("descends from its shell 200")
        );
        assert_eq!(
            pane_holds_session(
                &panes,
                9,
                crate::agent_session::Agent::Claude,
                "abc-123",
                Some(555),
                from_200
            ),
            None
        );
    }

    #[test]
    fn live_session_pid_reads_the_registry() {
        let dir = std::env::temp_dir().join(format!("kova-ask-reg-{}", new_question_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let me = std::process::id();
        std::fs::write(dir.join("a.json"), format!(r#"{{"pid":{},"sessionId":"live-1"}}"#, me)).unwrap();
        std::fs::write(dir.join("b.json"), r#"{"pid":999999,"sessionId":"dead-1"}"#).unwrap();
        assert_eq!(live_session_pid("live-1", &dir), Some(me as i32));
        assert_eq!(live_session_pid("dead-1", &dir), None);
        assert_eq!(live_session_pid("other", &dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_keeps_questions_and_deliveries_across_restarts() {
        let root = std::env::temp_dir().join(format!("kova-ask-store-{}", new_question_id()));
        let store = AskStore::new(root.clone());
        let id = new_question_id();
        let q = question(&id);
        store.save_question(&q);
        assert_eq!(AskStore::new(root.clone()).pending_questions(), vec![q.clone()]);
        store.remove_question(&id);
        assert!(AskStore::new(root.clone()).pending_questions().is_empty());
        let item = Undelivered { question: q.clone(), answer: Answer { id: id.clone(), action: "OK".into(), text: "fix it".into(), card_version: 1, blocks: vec![] } };
        store.save_delivery(&item);
        assert_eq!(AskStore::new(root.clone()).undelivered(), vec![item]);
        store.remove_delivery(&id);
        assert!(AskStore::new(root.clone()).undelivered().is_empty());
        // A malformed id never names a file.
        store.save_question(&question("../x"));
        assert!(store.pending_questions().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn transcript_gives_the_directory_the_conversation_ran_in() {
        let root = std::env::temp_dir().join(format!("kova-ask-proj-{}", new_question_id()));
        std::fs::create_dir_all(root.join("-Users-x-w")).unwrap();
        std::fs::write(root.join("-Users-x-w/abc-1.jsonl"), "{\"type\":\"summary\"}\n{\"cwd\":\"/Users/x/w\",\"type\":\"user\"}\n").unwrap();
        assert_eq!(transcript_cwd(&root, "abc-1").as_deref(), Some("/Users/x/w"));
        assert_eq!(transcript_cwd(&root, "abc-2"), None);
        assert_eq!(transcript_cwd(&root, "../abc-1"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn only_one_kova_owns_the_store() {
        let root = std::env::temp_dir().join(format!("kova-ask-lock-{}", new_question_id()));
        let store = AskStore::new(root.clone());
        let held = store.try_own();
        assert!(held.is_some());
        // flock is per open file description, so a second open in the same process conflicts too.
        assert!(store.try_own().is_none());
        drop(held);
        assert!(store.try_own().is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn late_message_drops_every_control_character() {
        let q = Question { message: "erase\u{15}claude --resume x; rm\u{1b}[A".into(), ..question("1") };
        let msg = late_answer_message(&q, &Answer { id: "1".into(), action: "OK".into(), text: "a\tb".into(), card_version: 1, blocks: vec![] });
        assert!(!msg.chars().any(|c| c.is_control()), "{msg:?}");
    }

    #[test]
    fn resume_line_quotes_the_answer_and_refuses_odd_ids() {
        assert_eq!(
            resume_line(
                crate::agent_session::Agent::Claude,
                "abc-1",
                "[Kova] it's \"ok\""
            )
            .as_deref(),
            Some("claude --dangerously-skip-permissions --resume abc-1 '[Kova] it'\\''s \"ok\"'")
        );
        assert_eq!(
            resume_line(
                crate::agent_session::Agent::Omp,
                "abc-1",
                "[Kova] it's \"ok\""
            )
            .as_deref(),
            Some("omp --resume abc-1 '[Kova] it'\\''s \"ok\"'")
        );
        assert_eq!(
            resume_line(crate::agent_session::Agent::Claude, "abc; rm", "m"),
            None
        );
        assert_eq!(
            resume_line(crate::agent_session::Agent::Omp, "abc; rm", "m"),
            None
        );
    }

    #[test]
    fn cli_parses_flags_and_refuses_mistakes() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let c = parse_cli(&args(&["Ship it?", "--actions", "OK, KO", "--no-input", "--timeout", "30", "--json"])).unwrap();
        assert_eq!(c.message, "Ship it?");
        assert_eq!(c.actions, vec!["OK", "KO"]);
        assert!(!c.input && c.json);
        assert_eq!(c.timeout, Some(30));
        assert!(parse_cli(&args(&[])).is_err());
        assert!(parse_cli(&args(&["a", "b"])).is_err());
        // An unquoted --actions list with spaces splits into stray arguments: the error names one and says to quote.
        let e = parse_cli(&args(&["a", "--actions", "Yes", "please,No"])).err().expect("stray argument must be refused");
        assert!(e.contains("'please,No'") && e.contains("--actions \"Yes please,No\""), "{}", e);
        assert!(parse_cli(&args(&["a", "--timeout", "0"])).is_err());
        assert!(parse_cli(&args(&["a", "--bogus"])).is_err());
        assert!(parse_cli(&args(&["a", "--from"])).is_err());
    }

    #[test]
    fn cli_card_update_and_withdraw_flags() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let id = "0f8fad5b-d9cb-469f-a165-70867728950e";
        let c = parse_cli(&args(&["--card", "-", "--from", "cos", "--timeout", "5"])).unwrap();
        assert_eq!(c.card.as_deref(), Some("-"));
        assert!(parse_cli(&args(&["msg", "--card", "c.json"])).is_err());
        assert!(parse_cli(&args(&["--card", "c.json", "--actions", "OK"])).is_err());
        assert!(parse_cli(&args(&["--card", "c.json", "--no-input"])).is_err());
        let u = parse_cli(&args(&["--update", id, "--card", "c.json"])).unwrap();
        assert_eq!(u.update.as_deref(), Some(id));
        assert!(parse_cli(&args(&["--update", id])).is_err(), "update needs a card");
        assert!(parse_cli(&args(&["--update", id, "--card", "c", "--timeout", "3"])).is_err());
        assert!(parse_cli(&args(&["--update", id, "--card", "c", "--from", "x"])).is_err());
        assert!(parse_cli(&args(&["--update", "../x", "--card", "c"])).is_err());
        let w = parse_cli(&args(&["--withdraw-id", id])).unwrap();
        assert_eq!(w.withdraw_id.as_deref(), Some(id));
        assert!(parse_cli(&args(&["--withdraw-id", id, "--timeout", "3"])).is_err());
    }

    #[test]
    fn agent_colour_is_stable_and_varies() {
        assert_eq!(agent_rgb("Claude · FLoat"), agent_rgb("Claude · FLoat"));
        let names = ["routine-a", "routine-b", "app", "kova", "mira", "docs", "api", "web"];
        let distinct: std::collections::HashSet<String> = names.iter().map(|n| format!("{:?}", agent_rgb(n))).collect();
        assert!(distinct.len() > 3);
    }

    /// A board that owns no store: nothing touches the disk.
    fn memory_board() -> Board {
        Board::with_store(AskStore::new(std::env::temp_dir().join("kova-ask-test-unowned")), None)
    }

    #[test]
    fn writing_in_the_conversation_keeps_the_question_on_the_card_marked() {
        let mut board = memory_board();
        let (tx, rx) = std::sync::mpsc::channel();
        board.add(question("q1"), tx);
        assert_eq!(board.withdraw("abc-123"), 1);
        match rx.try_recv() {
            Ok(crate::ipc::IpcResponse::Error { message }) => assert_eq!(message, WITHDRAWN),
            _ => panic!("the waiting asker must be released"),
        }
        let kept = board.questions();
        assert_eq!(kept.len(), 1, "the card keeps it");
        assert!(kept[0].superseded);
        assert_eq!(kept[0].buttons(), vec!["OK", "KO", OTHER, DISMISS]);
        assert_eq!(board.withdraw("abc-123"), 0, "a second prompt changes nothing");
        assert_eq!(board.withdraw("other-session"), 0);
    }

    #[test]
    fn dismiss_closes_a_superseded_question_without_delivering() {
        let mut board = memory_board();
        let (tx, _rx) = std::sync::mpsc::channel();
        board.add(question("q1"), tx);
        board.withdraw("abc-123");
        assert!(board.submit("q1", DISMISS, "", &[], 1, Trigger::Click).is_ok());
        assert!(board.questions().is_empty());
    }

    #[test]
    fn an_askers_own_dismiss_stays_an_answer() {
        let q = Question { actions: vec![DISMISS.into(), "Keep".into()], superseded: true, ..question("1") };
        assert!(!q.dismissable());
        assert_eq!(q.buttons(), vec![DISMISS, "Keep", OTHER], "no second Dismiss");
        assert!(!question("2").dismissable(), "a live question has none");
    }

    #[test]
    fn not_now_lets_a_waiting_asker_go_and_keeps_its_question() {
        let mut board = memory_board();
        let (tx, rx) = std::sync::mpsc::channel();
        board.add(question("q1"), tx);
        let until = now_secs() + 1200;
        board.set_pause(Some(until));
        match rx.try_recv() {
            Ok(crate::ipc::IpcResponse::Error { message }) => assert_eq!(paused_until_of(&message), Some(until)),
            _ => panic!("the waiting asker must be told at once"),
        }
        assert_eq!(board.questions().len(), 1, "the question stays in the queue");
        assert_eq!(board.paused_at(now_secs()), Some(until));
    }

    #[test]
    fn a_question_asked_during_the_pause_is_queued_and_its_asker_told() {
        let mut board = memory_board();
        let until = now_secs() + 600;
        board.set_pause(Some(until));
        let (tx, rx) = std::sync::mpsc::channel();
        board.add(question("q1"), tx);
        assert!(matches!(rx.try_recv(), Ok(crate::ipc::IpcResponse::Error { message }) if message == paused_error(until)));
        assert_eq!(board.questions().len(), 1);
    }

    #[test]
    fn an_asker_that_cannot_get_a_late_answer_keeps_waiting_through_the_pause() {
        let mut board = memory_board();
        let mut q = question("q1");
        q.from.session_id = None;
        let (tx, rx) = std::sync::mpsc::channel();
        board.add(q.clone(), tx);
        board.set_pause(Some(now_secs() + 600));
        assert!(rx.try_recv().is_err(), "released, it would lose the answer");
        let (tx2, rx2) = std::sync::mpsc::channel();
        board.add(Question { id: "q2".into(), ..q }, tx2);
        assert!(rx2.try_recv().is_err());
    }

    #[test]
    fn the_pause_ends_by_itself_or_on_demand() {
        let mut board = memory_board();
        let now = now_secs();
        board.set_pause(Some(now + 60));
        board.expire_pause(now + 59);
        assert!(board.paused_at(now + 59).is_some());
        board.expire_pause(now + 60);
        assert_eq!(board.paused_until, None);
        board.set_pause(Some(now + 600));
        board.set_pause(None);
        assert_eq!(board.paused_at(now), None);
    }

    #[test]
    fn the_pause_is_kept_on_disk() {
        let root = std::env::temp_dir().join(format!("kova-ask-pause-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = AskStore::new(root.clone());
        assert_eq!(store.load_pause(), None);
        store.save_pause(Some(1_900_000_000));
        assert_eq!(AskStore::new(root.clone()).load_pause(), Some(1_900_000_000));
        store.save_pause(None);
        assert_eq!(store.load_pause(), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dnd_values_read_as_minutes() {
        assert_eq!(parse_dnd_minutes("off"), Ok(0));
        assert_eq!(parse_dnd_minutes("0"), Ok(0));
        assert_eq!(parse_dnd_minutes("20"), Ok(20));
        assert_eq!(parse_dnd_minutes("20m"), Ok(20));
        assert_eq!(parse_dnd_minutes("20min"), Ok(20));
        assert_eq!(parse_dnd_minutes("1h"), Ok(60));
        assert_eq!(parse_dnd_minutes("3H"), Ok(180));
        assert!(parse_dnd_minutes("2d").is_err());
        assert!(parse_dnd_minutes("25h").is_err());
        assert!(parse_dnd_minutes("-5").is_err());
        assert_eq!(paused_until_of("paused until 1900000000"), Some(1_900_000_000));
        assert_eq!(paused_until_of(WITHDRAWN), None);
        assert_eq!(DND_CHOICES[0].0, 20 * 60, "the default is 20 min");
    }

    #[test]
    fn a_new_question_never_arrives_superseded() {
        let mut board = memory_board();
        let (tx, _rx) = std::sync::mpsc::channel();
        board.add(Question { superseded: true, ..question("q1") }, tx);
        assert!(!board.questions()[0].superseded);
    }

    const QID: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";
    const QID2: &str = "1f8fad5b-d9cb-469f-a165-70867728950e";

    fn draft_json(payload: &str) -> serde_json::Value {
        serde_json::json!({
            "version": 1,
            "title": "Reply to Alex",
            "blocks": [
                { "type": "text", "text": "Alex asks for a review." },
                { "type": "code", "label": "Draft", "text": payload }
            ],
            "actions": [
                { "id": "post", "label": "Post", "style": "primary",
                  "grant": { "kind": "outbound", "target": "slack:C1/-",
                             "payload_sha256": crate::decision_card::sha256_hex(payload.as_bytes()), "payload_block": 1 } },
                { "id": "edit", "label": "Rewrite", "needs_input": true },
                { "id": "later", "label": "Later" }
            ],
            "input": { "placeholder": "What to change" }
        })
    }

    fn card_question(id: &str, payload: &str) -> Question {
        let facts = crate::decision_card::AskerFacts { session_id: Some("abc-123"), claude: true };
        let card = crate::decision_card::validate(&draft_json(payload), None, &facts).unwrap();
        let mut q = question(id);
        q.set_card(card, None);
        q
    }

    fn board_in(dir: &str) -> Board {
        let root = std::env::temp_dir().join(format!("kova-ask-{}-{}", dir, std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        Board::with_store(AskStore::new(root), None)
    }

    #[test]
    fn a_card_fills_the_fallback_fields_for_an_older_kova() {
        let q = card_question(QID, "Hi\n");
        assert_eq!(q.message, "Reply to Alex — Alex asks for a review.");
        assert_eq!(q.actions, vec!["edit", "later"], "never the grant action");
        assert!(q.input);
        assert_eq!(q.buttons(), vec!["post", "edit", "later"], "no Other on a card");
    }

    #[test]
    fn a_plain_question_serializes_as_before() {
        let q = question("1");
        let json = serde_json::to_value(&q).unwrap();
        assert!(json.get("card").is_none() && json.get("card_version").is_none());
        // A question an older Kova wrote loads at version 1, and so does its answer.
        let old = r#"{"id":"1","message":"m","actions":[],"input":true,"from":{"name":"x"},"asked_at":1,"superseded":false}"#;
        assert_eq!(serde_json::from_str::<Question>(old).unwrap().card_version, 1);
        let answer: Answer = serde_json::from_str(r#"{"id":"1","action":"OK","text":""}"#).unwrap();
        assert_eq!(answer.card_version, 1);
        // A card question round-trips through disk.
        let mut c = card_question(QID, "x");
        c.card_version = 3;
        let back: Question = serde_json::from_value(serde_json::to_value(&c).unwrap()).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn late_line_for_a_card_names_its_title_and_version() {
        let mut q = card_question(QID, "x");
        q.card_version = 2;
        let a = Answer { id: QID.into(), action: "edit".into(), text: "shorter\nplease".into(), card_version: 2, blocks: vec![] };
        assert_eq!(late_answer_message(&q, &a), "[Kova] The user answered your card \"Reply to Alex\" (v2): edit: shorter please");
    }

    #[test]
    fn plan_click_gives_a_grant_only_to_a_real_click_on_the_shown_version() {
        let q = card_question(QID, "Draft\n");
        let (answer, grant, _) = plan_click(&q, "post", "  ", &[], 1, Trigger::Click, 42).unwrap();
        assert_eq!(answer, Answer { id: QID.into(), action: "post".into(), text: String::new(), card_version: 1, blocks: vec![] });
        let grant = grant.expect("a click writes a grant");
        assert_eq!((grant.session_id.as_str(), grant.target.as_str(), grant.card_version, grant.clicked_at), ("abc-123", "slack:C1/-", 1, 42));
        assert_eq!(grant.payload_sha256, crate::decision_card::sha256_hex(b"Draft\n"));
        assert!(plan_click(&q, "post", "", &[], 1, Trigger::CmdReturn, 0).unwrap().1.is_some());
        assert_eq!(plan_click(&q, "post", "", &[], 1, Trigger::Return, 0), Err(ClickRefusal::Ignored), "Return never grants");
        assert_eq!(plan_click(&q, "post", "", &[], 1, Trigger::Remote, 0), Err(ClickRefusal::Refused("a grant action can only be clicked".into())));
        assert_eq!(plan_click(&q, "post", "", &[], 2, Trigger::Click, 0), Err(ClickRefusal::Stale), "a redraw is pending");
        assert_eq!(plan_click(&q, "edit", " ", &[], 1, Trigger::Click, 0), Err(ClickRefusal::Refused("edit needs text".into())));
        assert_eq!(plan_click(&q, "nope", "", &[], 1, Trigger::Remote, 0), Err(ClickRefusal::Refused("no action nope in this card".into())));
        let (a, g, _) = plan_click(&q, "later", "fyi", &[], 1, Trigger::Click, 0).unwrap();
        assert!(g.is_none());
        assert_eq!(a.text, "fyi", "every answer carries the field");
    }

    #[test]
    fn answer_ask_on_a_grant_action_is_refused_and_writes_nothing() {
        let mut board = board_in("remote");
        let (tx, rx) = std::sync::mpsc::channel();
        board.add(card_question(QID, "x"), tx);
        assert_eq!(board.answer_checked(QID, "post".into(), String::new()), Err("a grant action can only be clicked".into()));
        assert!(!crate::decision_card::grants_dir(&board.store.root).exists());
        assert_eq!(board.questions().len(), 1, "the card stays");
        assert!(rx.try_recv().is_err());
        assert_eq!(board.answer_checked(QID, "edit".into(), String::new()), Err("edit needs text".into()));
        let answer = board.answer_checked(QID, "edit".into(), " shorter ".into()).unwrap();
        assert_eq!((answer.text.as_str(), answer.card_version), ("shorter", 1));
        assert!(matches!(rx.try_recv(), Ok(crate::ipc::IpcResponse::Ok { .. })));
        let _ = std::fs::remove_dir_all(&board.store.root);
    }

    #[test]
    fn a_click_on_a_grant_action_writes_the_grant_before_answering() {
        let mut board = board_in("click");
        let (tx, rx) = std::sync::mpsc::channel();
        board.add(card_question(QID, "x"), tx);
        board.update(QID, "abc-123", &draft_json("second\n"), None).unwrap();
        assert_eq!(board.submit(QID, "post", "", &[], 1, Trigger::Click), Err(ClickRefusal::Stale), "the old version never grants");
        assert!(!crate::decision_card::grants_dir(&board.store.root).exists());
        board.submit(QID, "post", "", &[], 2, Trigger::Click).unwrap();
        let path = crate::decision_card::grants_dir(&board.store.root).join(format!("{}.json", QID));
        let grant: crate::decision_card::Grant = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(grant.card_version, 2);
        assert_eq!(grant.payload_sha256, crate::decision_card::sha256_hex(b"second\n"));
        match rx.try_recv() {
            Ok(crate::ipc::IpcResponse::Ok { data: Some(d) }) => assert_eq!(d["card_version"], 2),
            _ => panic!("answered"),
        }
        let _ = std::fs::remove_dir_all(&board.store.root);
    }

    #[test]
    fn an_edited_code_block_is_what_the_answer_and_the_grant_carry() {
        let q = card_question(QID, "Draft\n");
        // Unchanged: the card's text, not marked edited.
        let (_, grant, finals) = plan_click(&q, "post", "", &[(1, "Draft\n".into())], 1, Trigger::Click, 0).unwrap();
        assert_eq!(finals, vec![FinalBlock { block: 1, label: Some("Draft".into()), text: "Draft\n".into(), edited: false }]);
        assert_eq!(grant.unwrap().payload_sha256, crate::decision_card::sha256_hex(b"Draft\n"));
        // Edited: the grant authorizes the user's bytes, not the asker's.
        let (_, grant, finals) = plan_click(&q, "post", "", &[(1, "My draft".into())], 1, Trigger::Click, 0).unwrap();
        assert_eq!((finals[0].text.as_str(), finals[0].edited), ("My draft", true));
        assert_eq!(grant.unwrap().payload_sha256, crate::decision_card::sha256_hex(b"My draft"));
        // Any action carries the blocks; an index that is not a code block is ignored.
        let (_, _, finals) = plan_click(&q, "later", "", &[(0, "nope".into())], 1, Trigger::Click, 0).unwrap();
        assert_eq!((finals.len(), finals[0].text.as_str()), (1, "Draft\n"));
        // A remote answer has no edits: the card's text.
        assert!(final_blocks(q.card.as_ref().unwrap(), &[]).iter().all(|f| !f.edited));
    }

    #[test]
    fn a_click_writes_each_code_block_to_a_file_named_in_the_answer() {
        let mut board = board_in("blocks");
        let (tx, rx) = std::sync::mpsc::channel();
        board.add(card_question(QID, "Draft\n"), tx);
        let answer = board.submit(QID, "post", "", &[(1, "Édité\r\n ok".into())], 1, Trigger::Click).unwrap();
        let b = &answer.blocks[0];
        assert_eq!((b.block, b.edited), (1, true));
        assert_eq!(std::path::PathBuf::from(&b.file), answer_block_path(&board.store.root, QID, 1).unwrap());
        assert_eq!(std::fs::read(&b.file).unwrap(), "Édité\r\n ok".as_bytes(), "byte for byte");
        assert_eq!(b.sha256, crate::decision_card::sha256_hex("Édité\r\n ok".as_bytes()));
        let grant_path = crate::decision_card::grants_dir(&board.store.root).join(format!("{}.json", QID));
        let grant: crate::decision_card::Grant = serde_json::from_slice(&std::fs::read(&grant_path).unwrap()).unwrap();
        assert_eq!(grant.payload_sha256, b.sha256, "the gate checks the file Kova wrote");
        match rx.try_recv() {
            Ok(crate::ipc::IpcResponse::Ok { data: Some(d) }) => assert_eq!(d["blocks"][0]["edited"], true),
            _ => panic!("answered"),
        }
        assert!(answer.line().starts_with(&format!("post [block 1 \"Draft\" edited by the user: {} sha256 ", b.file)));
        let _ = std::fs::remove_dir_all(&board.store.root);
    }

    #[test]
    fn old_answer_folders_are_pruned() {
        let root = std::env::temp_dir().join(format!("kova-ask-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("answers").join("q1")).unwrap();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(prune_answer_files(&root, now), 0);
        assert_eq!(prune_answer_files(&root, now + ANSWER_FILES_TTL_SECS + 5), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_grant_that_cannot_be_written_delivers_nothing() {
        let mut board = board_in("nogrant");
        // A file where the grants directory should be.
        std::fs::create_dir_all(&board.store.root).unwrap();
        std::fs::write(crate::decision_card::grants_dir(&board.store.root), b"x").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        board.add(card_question(QID, "x"), tx);
        match board.submit(QID, "post", "", &[], 1, Trigger::Click) {
            Err(ClickRefusal::Refused(m)) => assert!(m.starts_with("Could not record the authorization:"), "{}", m),
            other => panic!("{:?}", other),
        }
        assert_eq!(board.questions().len(), 1);
        assert!(board.error.is_some());
        assert!(rx.try_recv().is_err());
        let _ = std::fs::remove_dir_all(&board.store.root);
    }

    #[test]
    fn only_the_asker_updates_or_withdraws_and_the_queue_keeps_its_order() {
        let mut board = memory_board();
        let (tx, _rx) = std::sync::mpsc::channel();
        board.add(question(QID), tx.clone());
        board.add(question(QID2), tx);
        let card = draft_json("x");
        assert_eq!(board.update(QID, "someone-else", &card, None), Err("not the asker of this question".into()));
        assert_eq!(board.update("2f8fad5b-d9cb-469f-a165-70867728950e", "abc-123", &card, None), Err("no such question".into()));
        let mut bad = card.clone();
        bad["version"] = serde_json::json!(9);
        assert_eq!(board.update(QID, "abc-123", &bad, None), Err("unknown card version 9".into()));
        // A plain question becomes a card.
        assert_eq!(board.update(QID, "abc-123", &card, None), Ok(2));
        assert_eq!(board.update(QID, "abc-123", &card, Some("custom".into())), Ok(3));
        let ids: Vec<&str> = board.questions().iter().map(|q| q.id.as_str()).collect();
        assert_eq!(ids, vec![QID, QID2]);
        assert_eq!(board.questions()[0].message, "custom");
        assert_eq!(board.withdraw_one(QID2, "nope"), Err("not the asker of this question".into()));
        assert_eq!(board.withdraw_one(QID2, "abc-123"), Ok(()));
        assert_eq!(board.withdraw_one(QID2, "abc-123"), Err("no such question".into()));
        // An asker without a session id never matches.
        let mut anon = question("3f8fad5b-d9cb-469f-a165-70867728950e");
        anon.from.session_id = None;
        let (tx, _rx) = std::sync::mpsc::channel();
        board.add(anon, tx);
        assert_eq!(board.update("3f8fad5b-d9cb-469f-a165-70867728950e", "abc-123", &card, None), Err("not the asker of this question".into()));
    }

    #[test]
    fn the_shown_card_is_followed_by_id_and_browsing_answers_nothing() {
        // Nothing shown yet: the first live question.
        assert_eq!(pick_shown(&[("a", true), ("b", false)], None), Some(1));
        assert_eq!(pick_shown(&[("a", true)], None), Some(0));
        assert_eq!(pick_shown(&[], Some(("a", 0))), None);
        // An older question leaves: the shown one stays, its rank moves.
        assert_eq!(pick_shown(&[("b", false), ("c", false)], Some(("c", 2))), Some(1));
        // The shown one leaves: the next in order, or the previous when it was last.
        assert_eq!(pick_shown(&[("a", false), ("c", false)], Some(("b", 1))), Some(1));
        assert_eq!(pick_shown(&[("a", false), ("b", false)], Some(("c", 2))), Some(1));

        let mut board = memory_board();
        let (tx, rx) = std::sync::mpsc::channel();
        for id in [QID, QID2] {
            board.add(question(id), tx.clone());
        }
        board.shown = board.current_shown();
        board.browse(1);
        assert_eq!(board.shown.as_ref().map(|(id, _)| id.as_str()), Some(QID2));
        board.browse(5);
        assert_eq!(board.shown.as_ref().map(|(_, r)| *r), Some(1), "clamped");
        board.browse(-1);
        assert_eq!(board.shown.as_ref().map(|(id, _)| id.as_str()), Some(QID));
        assert_eq!(board.questions().len(), 2);
        assert!(rx.try_recv().is_err(), "browsing answers nothing");
    }
}
