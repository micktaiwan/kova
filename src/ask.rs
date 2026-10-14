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
//! Ported from Kova Float (`~/projects/perso/kova-float`, `kfloat ask`),
//! 14/10/2026, where every delivery path was measured first.

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
    /// `CLAUDE_CODE_SESSION_ID` of the Claude Code conversation that ran `kova ask`.
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
        Asker {
            name: name.unwrap_or(default_name),
            session_id: env("CLAUDE_CODE_SESSION_ID").filter(|id| is_safe_session_id(id)),
            pane_id: env("KOVA_PANE_ID").and_then(|p| p.parse().ok()),
            kova_socket: env("KOVA_SOCKET").filter(|s| !s.is_empty()),
            cwd: Some(cwd.to_string()),
        }
    }
}

/// One question, as it travels on the socket and sits on disk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
}

fn default_true() -> bool {
    true
}

impl Question {
    /// The buttons the card shows.
    pub fn buttons(&self) -> Vec<String> {
        if self.actions.is_empty() { vec!["Reply".to_string()] } else { self.actions.clone() }
    }
}

/// The user's answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    pub id: String,
    pub action: String,
    pub text: String,
}

impl Answer {
    /// One line for the asker: "OK", or "OK: fix that part".
    pub fn line(&self) -> String {
        if self.text.is_empty() { self.action.clone() } else { format!("{}: {}", self.action, self.text) }
    }
}

/// An answer given but not yet delivered, kept on disk until it is: a delivery
/// can wait hours for a conversation to close, and Kova may quit meanwhile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    Resume { session_id: String, cwd: Option<String> },
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
    Delivery::Resume { session_id: session, cwd: asker.cwd.clone() }
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
    format!(
        "[Kova] The user answered your question \"{}\": {}",
        one_line(&question.message),
        one_line(&answer.line())
    )
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
/// before still showed no session (14/10/2026, pane 205 in Kova Float's tests).
pub fn pane_holds_session(
    panes: &[serde_json::Value],
    pane_id: u32,
    session_id: &str,
    live_pid: Option<i32>,
    descends: impl Fn(i32, i32) -> bool,
) -> Option<String> {
    let pane = panes.iter().find(|p| p.get("id").and_then(|v| v.as_u64()) == Some(pane_id as u64))?;
    let reported = pane.get("claude_session_id").and_then(|v| v.as_str());
    if reported == Some(session_id) {
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
    let asker = &question.from;
    let id = &question.id;
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let registry = PathBuf::from(home).join(".claude/sessions");
    let live = asker.session_id.as_deref().and_then(|s| live_session_pid(s, &registry));
    let holds = holding_pane(asker, live);
    match delivery(asker, holds, live) {
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
                store.remove_delivery(id);
            }
        }
        Delivery::Resume { session_id, cwd } => resume(store, id, &session_id, cwd.as_deref(), &message, own_socket),
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
                Some(session) if !alive() => resume(store, id, session, asker.cwd.as_deref(), &message, own_socket),
                _ => log::warn!(
                    "ask {}: NOT delivered, process {} still alive after 24 h ({}): {}",
                    id,
                    pid,
                    kept(),
                    answer.line()
                ),
            }
        }
        Delivery::None => {
            log::info!("ask {}: answer has nowhere to go (no Claude session to reach): {}", id, answer.line());
            store.remove_delivery(id);
        }
    }
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
    match pane_holds_session(panes, pane_id, session, live, descends) {
        Some(proof) => {
            log::info!("ask: pane {} holds session {}: {}", pane_id, session, proof);
            true
        }
        None => false,
    }
}

/// The line typed into a new tab to reopen the conversation with the answer as
/// its next message. Same permission mode as the user's sessions: a reopened
/// agent that cannot run its tools would read the answer and do nothing with it.
pub fn resume_line(session_id: &str, message: &str) -> Option<String> {
    is_safe_session_id(session_id).then(|| {
        format!("claude --dangerously-skip-permissions --resume {} {}", session_id, shell_quote(message))
    })
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
fn resume(store: &AskStore, id: &str, session_id: &str, cwd: Option<&str>, message: &str, own_socket: &str) {
    let Some(line) = resume_line(session_id, message) else {
        log::warn!("ask {}: refusing to resume a non-plain session id", id);
        return;
    };
    // `claude --resume <id>` only finds a conversation from the directory it
    // ran in (`claude_session::resume_command`), which is not always the
    // asker's cwd (a `cd` before `kova ask`). The transcript records it: no
    // transcript, or a directory gone since, and the answer stays on disk.
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let Some(dir) = transcript_cwd(&PathBuf::from(home).join(".claude/projects"), session_id).filter(|d| Path::new(d).is_dir()) else {
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
    store.remove_delivery(id);
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
}

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
        Board { store, pending: Vec::new(), _owner: owner, owner_tried: std::time::Instant::now(), dirty: false }
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
            "ask {} from {} session={} pane={}: {}",
            question.id,
            question.from.name,
            question.from.session_id.as_deref().unwrap_or("-"),
            question.from.pane_id.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
            question.message
        );
        if self.owns_store() {
            self.store.save_question(&question);
        }
        self.pending.push((question, Some(responder)));
        self.dirty = true;
    }

    /// The user answered `id` on the card.
    pub fn answer(&mut self, id: &str, action: String, text: String) {
        let Some(index) = self.pending.iter().position(|(q, _)| q.id == id) else { return };
        let (question, responder) = self.pending.remove(index);
        self.dirty = true;
        let answer = Answer { id: id.to_string(), action, text };
        log::info!("ask {}: answered: {}", id, answer.line());
        // On disk before the question leaves it: a quit from here on still delivers it.
        if self.owns_store() {
            self.store.save_delivery(&Undelivered { question: question.clone(), answer: answer.clone() });
            self.store.remove_question(id);
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
    }

    /// Show the oldest question (or hide the card), when something changed.
    pub fn refresh_card(&mut self, mtm: objc2_foundation::MainThreadMarker) {
        self.retry_owner();
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let head = self.pending.first().map(|(q, _)| q);
        let buttons = head.map(|q| q.buttons()).unwrap_or_default();
        let card = head.map(|q| crate::ask_card::CardQuestion {
            id: &q.id,
            from: &q.from.name,
            message: &q.message,
            buttons: &buttons,
            input: q.input,
            asked_at: q.asked_at,
        });
        crate::ask_card::show(mtm, card.as_ref(), self.pending.len().saturating_sub(1));
    }
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

Shows a card in Kova with the message, one button per action and a text field, then waits for
the user and prints the answer on stdout: the action, or \"action: text\" when they typed something.

  --actions     buttons, comma-separated (default: a single \"Reply\" button)
  --no-input    no text field, buttons only
  --from        the name shown on the card (default: the working directory's name)
  --timeout     seconds to wait. Default: 540 inside a Claude Code conversation (under the Bash tool's
                600 s limit; run kova ask with the Bash timeout set to 600000), no limit anywhere else,
                since only a Claude conversation can receive a late answer
  --json        print the answer as JSON
  --socket      the Kova to ask (default: $KOVA_SOCKET, else the running Kova.app)

Exit codes: 0 answered · 2 no answer within the timeout · 1 error, or Kova quit while you waited.
On a timeout the question stays on the user's screen. When they answer, Kova delivers the answer to
this conversation itself: typed into its Kova pane if it is still open there, else by reopening it
in a new Kova tab with `claude --resume`. So after exit 2, end your turn: the answer will come as a message.";

struct CliArgs {
    message: String,
    actions: Vec<String>,
    input: bool,
    from: Option<String>,
    timeout: Option<u64>,
    json: bool,
    socket: Option<String>,
}

fn parse_cli(args: &[String]) -> Result<CliArgs, String> {
    let mut out = CliArgs { message: String::new(), actions: Vec::new(), input: true, from: None, timeout: None, json: false, socket: None };
    let mut message: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().cloned().ok_or_else(|| format!("kova ask: {} needs a value\n\n{}", a, USAGE));
        match a.as_str() {
            "--actions" => {
                out.actions = value()?.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
            }
            "--no-input" => out.input = false,
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
                    return Err(format!("kova ask: one message only (quote it)\n\n{}", USAGE));
                }
                message = Some(a.clone());
            }
        }
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
/// Kova.app itself. Kana serves the same path shape, hence the executable check.
fn discover_socket() -> Option<String> {
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

/// `kova ask …`: returns the process exit code.
pub fn run_cli(args: &[String]) -> i32 {
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
    let mut socket = match cli.socket.clone().or_else(|| asker.kova_socket.clone()).or_else(discover_socket) {
        Some(s) => s,
        None => return fail("kova ask: no running Kova found (no $KOVA_SOCKET, no /tmp/kova-<pid>.sock served by Kova.app)", 1),
    };
    let late = can_deliver_late(&asker);
    let wait = cli.timeout.or(if late { Some(540) } else { None });
    let question = Question {
        id: new_question_id(),
        message: cli.message.clone(),
        actions: cli.actions.clone(),
        input: cli.input,
        from: asker,
        asked_at: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
    };

    let body = serde_json::json!({"cmd": "ask", "question": question});
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
        // A pane of Kana (the library host) names Kana's socket, which serves
        // panes but no card: ask Kova.app instead. The asker keeps Kana's socket
        // and pane, which is where a late answer is typed.
        // Same for a socket that predates `ask` altogether (an older Kana build).
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
            session_id: Some("abc-123".into()),
            pane_id: Some(7),
            kova_socket: Some("/tmp/kova-1.sock".into()),
            cwd: Some("/tmp/w".into()),
        }
    }

    fn question(id: &str) -> Question {
        Question { id: id.into(), message: "Send it?".into(), actions: vec!["OK".into(), "KO".into()], input: true, from: asker(), asked_at: 5 }
    }

    #[test]
    fn delivery_prefers_the_live_pane() {
        assert_eq!(delivery(&asker(), true, None), Delivery::Pane { socket: "/tmp/kova-1.sock".into(), pane_id: 7 });
        // Open in the pane wins over "open in some process": it is that process.
        assert_eq!(delivery(&asker(), true, Some(42)), Delivery::Pane { socket: "/tmp/kova-1.sock".into(), pane_id: 7 });
    }

    #[test]
    fn delivery_resumes_when_the_pane_no_longer_holds_the_conversation() {
        assert_eq!(delivery(&asker(), false, None), Delivery::Resume { session_id: "abc-123".into(), cwd: Some("/tmp/w".into()) });
        let headless = Asker { pane_id: None, kova_socket: None, ..asker() };
        assert_eq!(delivery(&headless, true, None), Delivery::Resume { session_id: "abc-123".into(), cwd: Some("/tmp/w".into()) });
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
        let a = Asker::from_env(env, None, "/Users/x/projects/lemlist");
        assert_eq!(a, Asker {
            name: "lemlist".into(),
            session_id: Some("e2e1d3cf-7cc0-48a2".into()),
            pane_id: Some(3),
            kova_socket: Some("/tmp/k.sock".into()),
            cwd: Some("/Users/x/projects/lemlist".into()),
        });
        let evil = Asker::from_env(|k| (k == "CLAUDE_CODE_SESSION_ID").then(|| "x; rm -rf ~".to_string()), Some("n".into()), "/");
        assert_eq!(evil.session_id, None);
        assert!(!is_safe_session_id("a\nb"));
        assert!(!is_safe_session_id(""));
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
        assert_eq!(Answer { id: "1".into(), action: "OK".into(), text: String::new() }.line(), "OK");
        let answer = Answer { id: "1".into(), action: "OK".into(), text: "but fix\r\nthat part".into() };
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
    fn descends_walks_the_parent_chain() {
        let me = std::process::id() as i32;
        let parent = parent_pid(me).expect("this process has a parent");
        assert!(descends(me, parent));
        assert!(descends(me, me), "exec claude: the shell is the conversation");
        assert_eq!(parent_pid(999_999), None);
    }

    #[test]
    fn pane_holds_session_takes_either_proof() {
        let panes = vec![
            serde_json::json!({"id": 7, "pid": 100, "claude_session_id": "abc-123"}),
            serde_json::json!({"id": 8, "pid": 200, "claude_session_id": null}),
        ];
        let never = |_: i32, _: i32| false;
        assert_eq!(pane_holds_session(&panes, 7, "abc-123", None, never).as_deref(), Some("reported by Kova"));
        assert_eq!(pane_holds_session(&panes, 8, "abc-123", None, never), None);
        let from_200 = |pid: i32, shell: i32| pid == 555 && shell == 200;
        assert!(pane_holds_session(&panes, 8, "abc-123", Some(555), from_200).unwrap().contains("descends from its shell 200"));
        assert_eq!(pane_holds_session(&panes, 9, "abc-123", Some(555), from_200), None);
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
        let item = Undelivered { question: q.clone(), answer: Answer { id: id.clone(), action: "OK".into(), text: "fix it".into() } };
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
        let msg = late_answer_message(&q, &Answer { id: "1".into(), action: "OK".into(), text: "a\tb".into() });
        assert!(!msg.chars().any(|c| c.is_control()), "{msg:?}");
    }

    #[test]
    fn resume_line_quotes_the_answer_and_refuses_odd_ids() {
        assert_eq!(
            resume_line("abc-1", "[Kova] it's \"ok\"").as_deref(),
            Some("claude --dangerously-skip-permissions --resume abc-1 '[Kova] it'\\''s \"ok\"'")
        );
        assert_eq!(resume_line("abc; rm", "m"), None);
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
        assert!(parse_cli(&args(&["a", "--timeout", "0"])).is_err());
        assert!(parse_cli(&args(&["a", "--bogus"])).is_err());
        assert!(parse_cli(&args(&["a", "--from"])).is_err());
    }

    #[test]
    fn agent_colour_is_stable_and_varies() {
        assert_eq!(agent_rgb("Claude · FLoat"), agent_rgb("Claude · FLoat"));
        let names = ["routine-a", "routine-b", "lemlist", "kova", "mira", "vigie", "cto", "self"];
        let distinct: std::collections::HashSet<String> = names.iter().map(|n| format!("{:?}", agent_rgb(n))).collect();
        assert!(distinct.len() > 3);
    }
}
