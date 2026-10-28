//! Index of past Claude Code and omp conversations, so the search palette can
//! find a session that is no longer open in any pane and bring it back.
//!
//! Claude Code appends one JSON record per event to
//! `~/.claude/projects/<slug>/<session-id>.jsonl`; omp writes its own journal
//! under `~/.omp/agent/sessions/<bucket>/<created>_<id>.jsonl`. Those
//! transcripts are big (1.4 GB of Claude alone on this machine) and almost
//! entirely tool output, so the index keeps only what a search needs: the
//! prompts the user typed, the text of the agent's own replies, the project
//! directory, the title, and when the file last moved. Tool calls and thinking
//! blocks stay out: they are most of the bytes and never what someone searches
//! for. A transcript is append-only, so re-indexing reads just the bytes added
//! since last time — the first pass is the only one that walks everything.
//!
//! The index is rebuilt from the search worker thread (never on the main
//! thread) and cached in `INDEX` for the rest of the process' life.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::agent_session::Agent;

/// Cap on the searchable text kept per session. A long session's prompts fit in
/// far less; the cap only bounds the pathological case so the index file cannot
/// grow without limit. Past it the oldest prompts go, not the newest: the
/// conversation just had is the one most likely to be searched for.
const MAX_TEXT_PER_SESSION: usize = 256 * 1024;

/// Cap on the agent's own text kept per session, counted apart from the
/// prompts: a long answer must not push the questions out of the index, and the
/// two are searched for different reasons. Measured on the 1.4 GB corpus here,
/// every transcript's replies fit under 32 KB and the whole set costs 20 MB at
/// this cap (the prompts cost 8.5 MB), so the ceiling only bounds a runaway
/// session.
const MAX_REPLY_PER_SESSION: usize = 256 * 1024;

/// Chars of the first prompt kept as the row's title.
const MAX_TITLE_CHARS: usize = 100;

/// Archived rows shown per section for one query. Deliberately small: a common
/// word matches hundreds of sessions here ("oui" matches 379 of 1365), and a
/// section that long buries the open panes above it. The list is sorted by
/// date, so the cut falls on the oldest sessions; the count of what was cut is
/// shown instead, as an invitation to type one more word.
const MAX_RESULTS: usize = 8;

/// What the index remembers about one transcript file.
#[derive(Clone, Serialize, Deserialize)]
pub struct IndexedSession {
    /// Which agent wrote the transcript, and therefore which `resume` line
    /// reopens it.
    #[serde(default = "claude_agent")]
    pub agent: Agent,
    /// Conversation id — the argument the agent's `resume` expects. Same as
    /// the file stem for Claude, and the suffix after the timestamp for omp.
    pub id: String,
    /// Directory the session ran in. `claude --resume` only finds a session
    /// from its own project directory, so this is what a reopen must `cd` to.
    pub cwd: String,
    /// First typed prompt, trimmed to one line — what the row shows.
    pub title: String,
    /// Transcript mtime, epoch seconds.
    pub last_active: u64,
    /// Bytes already folded into `text`. Always a line boundary, so the next
    /// pass can start reading there.
    pub indexed_len: u64,
    /// Lowercased typed prompts, newline-separated: what a query matches on.
    pub text: String,
    /// Lowercased text of the agent's own replies, newline-separated: the other
    /// half of what a query matches on. Kept apart from `text` so each side has
    /// its own cap and a long answer cannot evict the questions. Empty for an
    /// index written before version 5 until its session is re-read.
    #[serde(default)]
    pub reply: String,
    /// Number of prompts the user typed in this session.
    #[serde(default)]
    pub prompts: u32,
    /// First typed prompt, kept even when a real title shadows it. omp can
    /// clear its title slot later, and the row then falls back to this without
    /// re-reading the transcript.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fallback_title: String,
    /// True when `title` came from an explicit title (Claude's row title
    /// always does; omp's title slot or `/rename`). A first prompt must not
    /// overwrite it.
    #[serde(default)]
    pub titled: bool,
}

fn claude_agent() -> Agent {
    Agent::Claude
}

/// Schema of the on-disk index. Bumped whenever what goes into an entry
/// changes: an entry already at its file's length is never re-read, so the
/// sessions indexed under the old rules would keep them. A bump costs one full
/// pass (8 s here for 1.4 GB), once. Version 3 started indexing slash commands
/// with their arguments and prompts sent with an image; version 5 indexes the
/// agent's own replies too (`reply`), not only what the user typed.
const INDEX_VERSION: u32 = 5;

/// The on-disk index, keyed by transcript path.
#[derive(Default, Serialize, Deserialize)]
pub struct Index {
    #[serde(default)]
    pub version: u32,
    pub sessions: HashMap<String, IndexedSession>,
}

/// What a query found in the archive, most recent first. `hits` carry every
/// term as a whole word; `inside` are the sessions where at least one term only
/// shows up inside a longer word ("pitr" in "chapitre"). Each list is capped,
/// and its total says how many matched before the cut.
pub struct Results {
    pub hits: Vec<Hit>,
    pub total: usize,
    pub inside: Vec<Hit>,
    pub inside_total: usize,
}

/// One archived session matching a query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub agent: Agent,
    pub id: String,
    pub cwd: String,
    pub title: String,
    pub last_active: u64,
}

static INDEX: Mutex<Option<Index>> = Mutex::new(None);

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

fn index_path() -> PathBuf {
    home().join(".config/kova/claude_history.json")
}

fn transcripts_root() -> PathBuf {
    home().join(".claude/projects")
}

fn load_index() -> Index {
    let path = index_path();
    let data = match std::fs::read_to_string(&path) {
        Ok(d) => d,
        Err(_) => return Index::default(),
    };
    match serde_json::from_str::<Index>(&data) {
        Ok(i) if i.version == INDEX_VERSION => i,
        Ok(_) => {
            log::info!("Claude history: index schema changed, rebuilding it from scratch");
            Index::default()
        }
        Err(e) => {
            log::warn!(
                "Failed to parse {} ({}); rebuilding the Claude history index from scratch",
                path.display(),
                e
            );
            Index::default()
        }
    }
}

fn save_index(index: &mut Index) {
    index.version = INDEX_VERSION;
    let path = index_path();
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            log::warn!("Failed to create {}: {}", parent.display(), e);
            return;
        }
    }
    match serde_json::to_string(index) {
        Ok(data) => {
            if let Err(e) = std::fs::write(&path, data) {
                log::warn!("Failed to write {}: {}", path.display(), e);
            }
        }
        Err(e) => log::warn!("Failed to serialize the Claude history index: {}", e),
    }
}

/// A prompt the user typed, as pulled out of one transcript record.
#[derive(Debug, PartialEq, Eq)]
pub struct Prompt {
    pub text: String,
    pub cwd: Option<String>,
    /// Whether the prompt can name the session in a row. A bare `/clear` or a
    /// compaction summary is searchable but says nothing about the session.
    pub titled: bool,
}

/// Pull a typed prompt out of one transcript line, or `None` if the record is
/// anything else.
///
/// Most `type: "user"` records are not the user talking: tool results, skill
/// bodies and system reminders all come back under the same type. Recent Claude
/// Code versions settle it with `promptSource: "typed"`; older transcripts
/// (a small minority of files) have no such field, and there the tell is a
/// string `content` that is not a machine-injected block — those all open with
/// a `<` tag.
///
/// Two kinds of typed prompts do not fit that mould and are read apart:
///   - a slash command (`/review the search results`) is stored without
///     `promptSource`, wrapped in `<command-name>`/`<command-args>` tags — they
///     are frequent, arguments often the whole request;
///   - a prompt sent with an image has an array `content`, its text in the
///     `text` parts.
pub fn typed_prompt(line: &str) -> Option<Prompt> {
    // Cheap gate first: parsing every record of a 50 MB transcript as JSON to
    // discard 99% of them is what makes a full index slow.
    if !line.contains("\"type\":\"user\"") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("type")?.as_str()? != "user" {
        return None;
    }
    if v.get("isMeta").and_then(|m| m.as_bool()).unwrap_or(false) {
        return None;
    }
    let source = v.get("promptSource").and_then(|p| p.as_str());
    let content = v.get("message")?.get("content")?;
    let cwd = v.get("cwd").and_then(|c| c.as_str()).map(str::to_string);

    let (text, titled) = if let Some(content) = content.as_str() {
        if let Some(command) = slash_command(content) {
            if !matches!(source, None | Some("typed")) {
                return None;
            }
            // A command alone names nothing: `/clear` is not what the session
            // was about.
            let titled = command.contains(' ');
            (command, titled)
        } else {
            let accepted = match source {
                Some("typed") => true,
                Some(_) => false,
                None => !content.trim_start().starts_with('<'),
            };
            if !accepted {
                return None;
            }
            let summary = v.get("isCompactSummary").and_then(|c| c.as_bool()).unwrap_or(false);
            (content.trim().to_string(), !summary)
        }
    } else {
        // Array content is a tool result unless the user typed it — only
        // `promptSource` tells the two apart.
        if source != Some("typed") {
            return None;
        }
        let parts: Vec<&str> = content
            .as_array()?
            .iter()
            .filter(|part| part.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
            .collect();
        (parts.join("\n").trim().to_string(), true)
    };
    if text.is_empty() {
        return None;
    }
    Some(Prompt { text, cwd, titled })
}

/// The text an assistant record carries: its `text` blocks only. Thinking
/// blocks and tool calls are skipped — they are the bulk of a transcript and
/// what a search for a phrase never means.
pub fn assistant_text(line: &str) -> Option<String> {
    // Same cheap gate as `typed_prompt`: an array of blocks with no `text` one
    // in it (a tool call, a thinking-only record) cannot yield a reply, so it is
    // skipped without a JSON parse.
    if !line.contains("\"type\":\"assistant\"") {
        return None;
    }
    if line.contains("\"content\":[") && !line.contains("\"text\":\"") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("type")?.as_str()? != "assistant" {
        return None;
    }
    let content = v.get("message")?.get("content")?;
    let text = match content {
        serde_json::Value::String(s) => s.clone(),
        array => array
            .as_array()?
            .iter()
            .filter(|block| block.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|block| block.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
    };
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// `/name args` out of a slash command record, or `None` when `content` is not
/// one. The record carries the name and the arguments in their own tags.
fn slash_command(content: &str) -> Option<String> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("<command-") {
        return None;
    }
    let tag = |name: &str| -> Option<&str> {
        let open = format!("<{}>", name);
        let close = format!("</{}>", name);
        let start = content.find(&open)? + open.len();
        let end = start + content[start..].find(&close)?;
        Some(content[start..end].trim())
    };
    let name = tag("command-name")?;
    let args = tag("command-args").unwrap_or("");
    Some(if args.is_empty() { name.to_string() } else { format!("{} {}", name, args) })
}

/// Cut a query into the terms a session must ALL carry.
///
/// One word is rarely enough here: a common word matches a large share of all sessions. Matching
/// the query as a single string would make "parser mcp" find nothing at all (the
/// two words never sit side by side), so the space — and the comma, which is how
/// one naturally lists keywords — separates terms instead, and a session has to
/// contain every one of them. Order does not matter, and the terms may come from
/// different prompts of the same session.
pub fn split_terms(query: &str) -> Vec<String> {
    query
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

/// How well a term sits in a text: not at all, only inside a longer word
/// ("pitr" in "chapitre"), or as a word of its own. Ordered, so the best of
/// several sources is their `max` and the weakest of several terms their `min`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Fit {
    Absent,
    Inside,
    Word,
}

/// Where `term` first sits in `hay` as a whole word, else where it first sits
/// at all, with how it fits. Both are expected lowercased.
///
/// A whole word means no letter or digit right before or after the match. A
/// term that itself starts or ends on punctuation (`/agent`, `rpo/rto`) is not
/// held to that on that side, so typing the slash does not lose the hit.
pub fn find_term(hay: &str, term: &str) -> Option<(usize, Fit)> {
    if term.is_empty() {
        return None;
    }
    let free_start = !term.chars().next().is_some_and(char::is_alphanumeric);
    let free_end = !term.chars().next_back().is_some_and(char::is_alphanumeric);
    let mut first = None;
    for (pos, _) in hay.match_indices(term) {
        let before = hay[..pos].chars().next_back();
        let after = hay[pos + term.len()..].chars().next();
        let left = free_start || !before.is_some_and(char::is_alphanumeric);
        let right = free_end || !after.is_some_and(char::is_alphanumeric);
        if left && right {
            return Some((pos, Fit::Word));
        }
        first.get_or_insert(pos);
    }
    first.map(|pos| (pos, Fit::Inside))
}

/// How `term` fits in `hay` (see `find_term`).
pub fn fit(hay: &str, term: &str) -> Fit {
    find_term(hay, term).map_or(Fit::Absent, |(_, f)| f)
}

/// First line of a prompt, trimmed to a row-sized title. A line that is only a
/// tag — the `<pasted_content id="…">` wrapping a paste — is skipped: the row
/// would show the wrapper instead of what was pasted.
pub fn title_from_prompt(prompt: &str) -> String {
    let first_line = prompt
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !(l.starts_with('<') && l.ends_with('>')))
        .unwrap_or("");
    let mut out: String = first_line.chars().take(MAX_TITLE_CHARS).collect();
    if first_line.chars().count() > MAX_TITLE_CHARS {
        out.push('…');
    }
    out
}

/// Fold the bytes appended to one transcript since `entry.indexed_len` into the
/// index entry. `len` is the file's current size, `mtime` its modification time.
///
/// A transcript only ever grows, so a smaller size means the file was replaced:
/// the entry is then rebuilt from byte 0 rather than resuming mid-file, which
/// would splice two unrelated halves together.
fn index_file(path: &std::path::Path, len: u64, mtime: u64, entry: &mut IndexedSession) -> bool {
    if entry.indexed_len == len {
        // Touched but not appended to (or already up to date): nothing to read.
        if entry.last_active != mtime {
            entry.last_active = mtime;
            return true;
        }
        return false;
    }
    let from = if len < entry.indexed_len {
        entry.text.clear();
        entry.reply.clear();
        entry.title.clear();
        entry.cwd.clear();
        entry.prompts = 0;
        entry.fallback_title.clear();
        entry.titled = false;
        0
    } else {
        entry.indexed_len
    };
    let omp = entry.agent == Agent::Omp;

    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            log::warn!("Claude history: cannot open {}: {}", path.display(), e);
            return false;
        }
    };
    let mut reader = BufReader::new(file);
    if from > 0 {
        use std::io::Seek;
        if let Err(e) = reader.seek(std::io::SeekFrom::Start(from)) {
            log::warn!("Claude history: cannot seek in {}: {}", path.display(), e);
            return false;
        }
    }

    let mut consumed = from;
    let mut buf = Vec::new();
    // Title of last resort, for a session with nothing but bare commands or a
    // compaction summary to show.
    let mut fallback_title = None;
    loop {
        buf.clear();
        let n = match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                log::warn!("Claude history: read error on {}: {}", path.display(), e);
                break;
            }
        };
        // A record still being written has no newline yet: stop before it so the
        // next pass reads it whole.
        if !buf.ends_with(b"\n") {
            break;
        }
        consumed += n as u64;
        let line = String::from_utf8_lossy(&buf);
        if omp && omp_title_line(&line, entry) {
            continue;
        }
        // A Claude line carries at most one of the two; an omp `message` record
        // is one or the other by its role. Both halves go in: the user's
        // questions under `text`, the agent's own words under `reply`.
        let (prompt, reply) = if omp {
            match omp_message(&line) {
                Some((true, text)) => (
                    Some(Prompt {
                        text,
                        cwd: None,
                        titled: true,
                    }),
                    None,
                ),
                Some((false, text)) => (None, Some(text)),
                None => (None, None),
            }
        } else {
            (typed_prompt(&line), assistant_text(&line))
        };
        if let Some(reply) = reply {
            entry.reply.push_str(&reply.to_lowercase());
            entry.reply.push('\n');
        }
        let Some(prompt) = prompt else {
            continue;
        };
        if omp && entry.fallback_title.is_empty() && prompt.titled {
            entry.fallback_title = title_from_prompt(&prompt.text);
        }
        if entry.title.is_empty() && !entry.titled {
            if prompt.titled {
                entry.title = title_from_prompt(&prompt.text);
            } else if fallback_title.is_none() {
                fallback_title = Some(title_from_prompt(&prompt.text));
            }
        }
        if let Some(cwd) = prompt.cwd {
            entry.cwd = cwd;
        }
        entry.prompts = entry.prompts.saturating_add(1);
        entry.text.push_str(&prompt.text.to_lowercase());
        entry.text.push('\n');
    }
    if omp && entry.title.is_empty() && !entry.fallback_title.is_empty() {
        entry.title = entry.fallback_title.clone();
        entry.titled = false;
    } else if entry.title.is_empty() {
        if let Some(title) = fallback_title {
            entry.title = title;
        }
    }
    trim_oldest(&mut entry.text, MAX_TEXT_PER_SESSION);
    trim_oldest(&mut entry.reply, MAX_REPLY_PER_SESSION);

    entry.indexed_len = consumed;
    entry.last_active = mtime;
    true
}

/// The title an omp record carries, if it carries one: the fixed-width slot,
/// a `/rename`, or the logical header title. Returns true when the line was
/// consumed, so it is not parsed as a prompt too.
fn omp_title_line(line: &str, entry: &mut IndexedSession) -> bool {
    if !line.contains("\"type\":\"title\"")
        && !line.contains("\"title_change\"")
        && !line.contains("\"type\":\"session\"")
    {
        return false;
    }
    let Ok(entry_json) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    match entry_json["type"].as_str().unwrap_or_default() {
        "title" | "title_change" => {
            let title = entry_json["title"].as_str().unwrap_or("").trim();
            if title.is_empty() {
                // `/rename` with no title, or a slot cleared by an automatic
                // rename: the first prompt is the row's title again.
                entry.title.clear();
                entry.titled = false;
            } else {
                entry.title = title.to_string();
                entry.titled = true;
            }
            true
        }
        "session" => {
            if let Some(cwd) = entry_json["cwd"].as_str() {
                entry.cwd = cwd.to_string();
            }
            let title = entry_json["title"].as_str().unwrap_or("").trim();
            if !title.is_empty() && entry.title.is_empty() {
                entry.title = title.to_string();
                entry.titled = true;
            }
            true
        }
        _ => false,
    }
}

/// The text of an omp message record, and whether the user typed it. omp
/// persists ordinary turns as `type: "message"` with the role nested, like Pi,
/// for both sides of the conversation; tool results carry the `toolResult` role
/// and are skipped here.
fn omp_message(line: &str) -> Option<(bool, String)> {
    if !line.contains("\"type\":\"message\"") {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value["type"] != "message" {
        return None;
    }
    let user = match value["message"]["role"].as_str()? {
        "user" => true,
        "assistant" => false,
        _ => return None,
    };
    let content = &value["message"]["content"];
    let text = if let Some(text) = content.as_str() {
        text.to_string()
    } else {
        content
            .as_array()?
            .iter()
            .filter(|block| block["type"] == "text")
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let text = text.trim().to_string();
    (!text.is_empty()).then_some((user, text))
}

/// Cut `text` down to at most `max` bytes by dropping whole lines from the
/// front, so the newest prompts are the ones kept. A last line longer than
/// `max` on its own is cut through, keeping its end.
fn trim_oldest(text: &mut String, max: usize) {
    if text.len() <= max {
        return;
    }
    // Bytes, not chars: a '\n' byte is never inside a multibyte char, so the
    // cut always lands on a char boundary. Searching from one byte early keeps
    // a line that already starts exactly at the cut.
    let from = text.len() - max - 1;
    let mut cut = text.as_bytes()[from..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(text.len(), |i| from + i + 1);
    if cut == text.len() {
        // One prompt longer than the whole cap: keep its tail rather than
        // nothing at all.
        cut = from + 1;
        while !text.is_char_boundary(cut) {
            cut += 1;
        }
    }
    text.drain(..cut);
}

/// Bring the index in line with what is on disk. Returns the number of
/// transcripts that were read.
fn refresh(index: &mut Index) -> usize {
    let mut seen: Vec<String> = Vec::new();
    let mut changed = refresh_claude(index, &mut seen);
    changed += refresh_omp(index, &mut seen);

    // Drop entries whose transcript is gone, so a deleted conversation stops
    // showing up as something to resume.
    if seen.len() != index.sessions.len() {
        let alive: std::collections::HashSet<&String> = seen.iter().collect();
        index.sessions.retain(|k, _| alive.contains(k));
        changed += 1;
    }
    changed
}

/// Every Claude Code transcript under `~/.claude/projects/<slug>/`.
fn refresh_claude(index: &mut Index, seen: &mut Vec<String>) -> usize {
    let root = transcripts_root();
    let project_dirs = match std::fs::read_dir(&root) {
        Ok(d) => d,
        Err(e) => {
            log::debug!("Claude history: no transcripts at {} ({})", root.display(), e);
            return 0;
        }
    };

    let mut changed = 0usize;
    for project in project_dirs.flatten() {
        let files = match std::fs::read_dir(project.path()) {
            Ok(f) => f,
            Err(_) => continue,
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let id = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            // An id that cannot be handed to `claude --resume` is an entry that
            // could never be reopened — same guard as the session restore path.
            if crate::claude_session::resume_command(None, &id, None).is_none() {
                continue;
            }
            changed += index_one(index, seen, &file, &path, Agent::Claude, id);
        }
    }
    changed
}

/// Every top-level omp transcript under
/// `~/.omp/agent/sessions/<bucket>/<created>_<id>.jsonl`.
///
/// Only the bucket's direct files are read: a session's artifact directory
/// holds subagent transcripts, which are not conversations the palette should
/// offer to reopen.
fn refresh_omp(index: &mut Index, seen: &mut Vec<String>) -> usize {
    let root = home().join(".omp/agent/sessions");
    let buckets = match std::fs::read_dir(&root) {
        Ok(d) => d,
        Err(e) => {
            log::debug!("omp history: no transcripts at {} ({})", root.display(), e);
            return 0;
        }
    };

    let mut changed = 0usize;
    for bucket in buckets.flatten() {
        let files = match std::fs::read_dir(bucket.path()) {
            Ok(f) => f,
            Err(_) => continue,
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let id = match path.file_name().and_then(|s| s.to_str()) {
                Some(name) => crate::omp_session::session_id_from_file_name(name),
                None => None,
            };
            let Some(id) = id else { continue };
            // Same guard as the restore path: an id that cannot produce a safe
            // `omp --resume` line is not something to offer.
            if crate::omp_session::resume_command(None, &id).is_none() {
                continue;
            }
            changed += index_one(index, seen, &file, &path, Agent::Omp, id);
        }
    }
    changed
}

/// Fold one transcript file into `index`, and record its path as still alive.
fn index_one(
    index: &mut Index,
    seen: &mut Vec<String>,
    file: &std::fs::DirEntry,
    path: &std::path::Path,
    agent: Agent,
    id: String,
) -> usize {
    let Ok(meta) = file.metadata() else {
        return 0;
    };
    let len = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let key = path.to_string_lossy().to_string();
    seen.push(key.clone());
    let entry = index.sessions.entry(key).or_insert_with(|| IndexedSession {
        agent,
        id: id.clone(),
        cwd: String::new(),
        title: String::new(),
        last_active: 0,
        indexed_len: 0,
        text: String::new(),
        reply: String::new(),
        prompts: 0,
        fallback_title: String::new(),
        titled: false,
    });
    // A path is one conversation; an agent never changes hands at the same one.
    entry.agent = agent;
    entry.id = id;
    usize::from(index_file(path, len, mtime, entry))
}

/// Bring the index up to date without searching, off the calling thread.
///
/// Called when the palette opens so the work overlaps with typing instead of
/// landing on the first query. Only the very first run of a machine walks every
/// transcript (8 s here for 1.4 GB); after that it is a `stat` per file plus
/// whatever was appended.
pub fn warm() {
    std::thread::spawn(|| {
        let mut guard = INDEX.lock();
        let index = guard.get_or_insert_with(load_index);
        if refresh(index) > 0 {
            save_index(index);
        }
    });
}

/// Refresh the index, then return the archived sessions matching `query`,
/// most recent first. Sessions listed in `live` are left out: they are already
/// open in a pane, and the palette lists those in its own section. The pair
/// carries the agent, so an omp id never hides a Claude conversation with the
/// same one.
///
/// Sorted by date and nothing else: the session wanted is almost always one
/// from the last days, and a ranking that mixes in other signals moves it
/// around for reasons the row does not show.
///
/// Called from the search worker thread — the first call after a cold start
/// reads every transcript, which takes a moment.
pub fn search(query: &str, live: &[(Agent, String)]) -> Results {
    let terms = split_terms(query);
    let empty = Results { hits: Vec::new(), total: 0, inside: Vec::new(), inside_total: 0 };
    if terms.is_empty() {
        return empty;
    }

    let mut guard = INDEX.lock();
    let index = guard.get_or_insert_with(load_index);
    if refresh(index) > 0 {
        save_index(index);
    }

    let mut hits: Vec<Hit> = Vec::new();
    let mut inside: Vec<Hit> = Vec::new();
    for s in index.sessions.values() {
        if s.title.is_empty()
            || live
                .iter()
                .any(|(agent, id)| *agent == s.agent && id == &s.id)
        {
            continue;
        }
        // Per field rather than one concatenated haystack: `text` and `reply`
        // can reach 256 KB each, and copying them for every session of every
        // keystroke is the one thing that would make a live search feel slow.
        let title = s.title.to_lowercase();
        let cwd = s.cwd.to_lowercase();
        let worst = terms
            .iter()
            .map(|t| fit(&title, t).max(fit(&cwd, t)).max(fit(&s.text, t)).max(fit(&s.reply, t)))
            .min()
            .unwrap_or(Fit::Absent);
        let hit = Hit {
            agent: s.agent,
            id: s.id.clone(),
            cwd: s.cwd.clone(),
            title: s.title.clone(),
            last_active: s.last_active,
        };
        match worst {
            Fit::Word => hits.push(hit),
            Fit::Inside => inside.push(hit),
            Fit::Absent => {}
        }
    }

    let newest_first = |list: &mut Vec<Hit>| -> usize {
        list.sort_by(|a, b| b.last_active.cmp(&a.last_active).then_with(|| a.id.cmp(&b.id)));
        let total = list.len();
        list.truncate(MAX_RESULTS);
        total
    };
    let total = newest_first(&mut hits);
    let inside_total = newest_first(&mut inside);
    Results { hits, total, inside, inside_total }
}

/// The indexed prompts of the sessions in `ids`, keyed by `(agent, id)`. Lets
/// the palette search a live pane by what was said in it, not only by what its
/// screen shows: Claude Code draws on the alternate screen, which keeps no
/// scrollback, so a pane's terminal holds one screen of the conversation.
///
/// Reads the index as the last `search` left it, without refreshing it again.
pub fn texts_of(ids: &[(Agent, String)]) -> HashMap<(Agent, String), String> {
    let mut guard = INDEX.lock();
    let index = guard.get_or_insert_with(load_index);
    // One id can have a transcript in two project directories: keep both.
    let mut texts: HashMap<(Agent, String), String> = HashMap::new();
    for s in index.sessions.values().filter(|s| {
        ids.iter()
            .any(|(agent, id)| *agent == s.agent && id == &s.id)
    }) {
        let said = texts
            .entry((s.agent, s.id.clone()))
            .or_default();
        said.push_str(&s.text);
        said.push('\n');
        said.push_str(&s.reply);
    }
    texts
}

/// Row label for a hit: project, age, then the prompt that opened the session.
pub fn hit_label(hit: &Hit) -> String {
    let project = std::path::Path::new(&hit.cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?");
    format!(
        "{} · {} · {} · {}",
        hit.agent.label(),
        project,
        crate::recent_projects::time_ago(hit.last_active),
        hit.title
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_typed_prompt_is_kept_with_its_cwd() {
        let line = r#"{"type":"user","promptSource":"typed","cwd":"/Users/x/projects/app","message":{"role":"user","content":"  cannot find the session about the parser  "}}"#;
        let p = typed_prompt(line).expect("a typed prompt");
        assert_eq!(p.text, "cannot find the session about the parser");
        assert_eq!(p.cwd.as_deref(), Some("/Users/x/projects/app"));
    }

    #[test]
    fn tool_results_and_injected_blocks_are_not_prompts() {
        // A tool result: same type, content is an array.
        assert!(typed_prompt(r#"{"type":"user","message":{"content":[{"type":"tool_result"}]}}"#).is_none());
        // A skill body replayed as a user turn.
        assert!(typed_prompt(r#"{"type":"user","isMeta":true,"message":{"content":"hello"}}"#).is_none());
        // Anything the harness generated rather than the user typing it.
        assert!(
            typed_prompt(r#"{"type":"user","promptSource":"queued_command","message":{"content":"hi"}}"#)
                .is_none()
        );
        // Not a user record at all.
        assert!(typed_prompt(r#"{"type":"assistant","message":{"content":"hi"}}"#).is_none());
    }

    #[test]
    fn an_old_transcript_without_prompt_source_falls_back_to_the_content() {
        // These 56 files predate `promptSource`; a real prompt still reads as one.
        let typed = r#"{"type":"user","cwd":"/tmp","message":{"content":"commit ça"}}"#;
        assert_eq!(typed_prompt(typed).unwrap().text, "commit ça");
        // …while an injected block opens with a tag.
        let injected = r#"{"type":"user","message":{"content":"<system-reminder>be brief</system-reminder>"}}"#;
        assert!(typed_prompt(injected).is_none());
    }

    #[test]
    fn a_slash_command_is_kept_with_its_arguments() {
        let line = r#"{"type":"user","cwd":"/p","message":{"content":"<command-message>verify-claims</command-message>\n<command-name>/verify-claims</command-name>\n<command-args>on fait un PITR</command-args>"}}"#;
        let p = typed_prompt(line).expect("a slash command");
        assert_eq!(p.text, "/verify-claims on fait un PITR");
        assert!(p.titled);
        // A bare command is searchable but does not name the session.
        let bare = r#"{"type":"user","message":{"content":"<command-name>/clear</command-name>\n<command-message>clear</command-message>\n<command-args></command-args>"}}"#;
        let p = typed_prompt(bare).expect("a bare command");
        assert_eq!(p.text, "/clear");
        assert!(!p.titled);
        // The command's printed output is not a prompt.
        let output = r#"{"type":"user","message":{"content":"<local-command-stdout>done</local-command-stdout>"}}"#;
        assert!(typed_prompt(output).is_none());
    }

    #[test]
    fn a_prompt_sent_with_an_image_keeps_its_text() {
        let line = r#"{"type":"user","promptSource":"typed","message":{"content":[{"type":"text","text":"c'est faux [Image #1]"},{"type":"image","source":{"data":"AAAA"}}]}}"#;
        assert_eq!(typed_prompt(line).unwrap().text, "c'est faux [Image #1]");
        // The same shape without `promptSource` is a tool result.
        let tool = r#"{"type":"user","message":{"content":[{"type":"text","text":"output"}]}}"#;
        assert!(typed_prompt(tool).is_none());
    }

    #[test]
    fn a_compaction_summary_is_searchable_but_never_the_title() {
        let line = r#"{"type":"user","isCompactSummary":true,"message":{"content":"This session is being continued. PITR takes 35 h"}}"#;
        let p = typed_prompt(line).unwrap();
        assert!(p.text.contains("PITR"));
        assert!(!p.titled);
    }

    #[test]
    fn a_whole_word_beats_the_same_letters_inside_a_longer_one() {
        assert_eq!(find_term("lis le chapitre 3", "pitr"), Some((10, Fit::Inside)));
        // The whole word wins even when the longer one comes first.
        assert_eq!(find_term("chapitre, puis le pitr", "pitr"), Some((18, Fit::Word)));
        assert_eq!(fit("rto / rpo: pitr.", "pitr"), Fit::Word);
        assert_eq!(fit("pitr", "pitr"), Fit::Word);
        assert_eq!(fit("rien", "pitr"), Fit::Absent);
        // Accented letters count as letters.
        assert_eq!(fit("pitré", "pitr"), Fit::Inside);
        // A term with its own punctuation is not held to a boundary there.
        assert_eq!(fit("lance /agent sur ça", "/agent"), Fit::Word);
        assert_eq!(fit("x/agent", "/agent"), Fit::Word);
    }

    #[test]
    fn trimming_drops_the_oldest_lines() {
        let mut text = "old one\nmiddle\nnewest\n".to_string();
        trim_oldest(&mut text, 15);
        assert_eq!(text, "middle\nnewest\n");
        // A cut that falls right at the start of a line keeps that line.
        let mut aligned = "aaaa\nbbbb\n".to_string();
        trim_oldest(&mut aligned, 5);
        assert_eq!(aligned, "bbbb\n");
        let mut short = "kept\n".to_string();
        trim_oldest(&mut short, 15);
        assert_eq!(short, "kept\n");
        // A cut landing inside a multibyte char moves to the next line, not into it.
        let mut accented = "ééééé\nfin\n".to_string();
        trim_oldest(&mut accented, 7);
        assert_eq!(accented, "fin\n");
        // A last prompt bigger than the cap keeps its tail, cut on a char.
        let mut huge = format!("{}\n", "é".repeat(20));
        trim_oldest(&mut huge, 6);
        assert_eq!(huge, "éé\n");
    }

    #[test]
    fn a_query_is_cut_into_terms_that_must_all_match() {
        assert_eq!(split_terms("  Parser   MCP "), vec!["parser", "mcp"]);
        assert_eq!(split_terms("parser, mcp"), vec!["parser", "mcp"]);
        assert_eq!(split_terms("parser"), vec!["parser"]);
        assert!(split_terms("   ").is_empty());
    }

    #[test]
    fn a_title_is_the_first_line_capped() {
        assert_eq!(title_from_prompt("\n\nfirst line\nsecond line"), "first line");
        assert_eq!(
            title_from_prompt("<pasted_content id=\"9d2a\">\nWeekly notes\n</pasted_content>"),
            "Weekly notes"
        );
        let long = "a".repeat(MAX_TITLE_CHARS + 10);
        let title = title_from_prompt(&long);
        assert_eq!(title.chars().count(), MAX_TITLE_CHARS + 1); // + the ellipsis
        assert!(title.ends_with('…'));
    }

    #[test]
    fn indexing_reads_only_what_was_appended() {
        let dir = std::env::temp_dir().join(format!("kova-history-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let first = "{\"type\":\"user\",\"promptSource\":\"typed\",\"cwd\":\"/tmp/p\",\"message\":{\"content\":\"one\"}}\n";
        std::fs::write(&path, first).unwrap();

        let mut entry = IndexedSession {
            agent: Agent::Claude,
            id: "s".into(),
            cwd: String::new(),
            title: String::new(),
            last_active: 0,
            indexed_len: 0,
            text: String::new(),
            reply: String::new(),
            prompts: 0,
            fallback_title: String::new(),
            titled: false,
        };
        assert!(index_file(&path, first.len() as u64, 10, &mut entry));
        assert_eq!(entry.title, "one");
        assert_eq!(entry.indexed_len, first.len() as u64);

        // Append a second prompt plus a half-written record: the complete line
        // lands, the partial one is left for the next pass.
        let second = "{\"type\":\"user\",\"promptSource\":\"typed\",\"message\":{\"content\":\"TWO\"}}\n";
        let partial = "{\"type\":\"user\",\"promptSou";
        std::fs::write(&path, format!("{}{}{}", first, second, partial)).unwrap();
        let len = (first.len() + second.len() + partial.len()) as u64;
        assert!(index_file(&path, len, 20, &mut entry));
        assert_eq!(entry.text, "one\ntwo\n");
        assert_eq!(entry.indexed_len, (first.len() + second.len()) as u64);
        assert_eq!(entry.title, "one", "the title stays the first prompt");
        assert_eq!(entry.last_active, 20);

        // A shorter file means it was replaced: start over instead of splicing.
        std::fs::write(&path, second).unwrap();
        assert!(index_file(&path, second.len() as u64, 30, &mut entry));
        assert_eq!(entry.text, "two\n");
        assert_eq!(entry.title, "TWO");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_omp_transcript_keeps_its_slot_title_and_first_prompt() {
        let dir = std::env::temp_dir().join(format!("kova-omp-history-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("2026-10-27T15-54-09-646Z_abc.jsonl");
        let slot = "{\"type\":\"title\",\"v\":1,\"title\":\"Titre auto\",\"source\":\"auto\",\"pad\":\"\"}\n";
        let header = "{\"type\":\"session\",\"version\":3,\"id\":\"abc\",\"timestamp\":\"t\",\"cwd\":\"/Users/x/app\"}\n";
        let prompt = "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"t\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"lis le parser\"}]}}\n";
        std::fs::write(&path, format!("{slot}{header}{prompt}")).unwrap();

        let mut entry = IndexedSession {
            agent: Agent::Omp,
            id: "abc".into(),
            cwd: String::new(),
            title: String::new(),
            last_active: 0,
            indexed_len: 0,
            text: String::new(),
            reply: String::new(),
            prompts: 0,
            fallback_title: String::new(),
            titled: false,
        };
        let len = (slot.len() + header.len() + prompt.len()) as u64;
        assert!(index_file(&path, len, 10, &mut entry));
        assert_eq!(entry.cwd, "/Users/x/app");
        assert_eq!(entry.title, "Titre auto");
        assert_eq!(entry.fallback_title, "lis le parser");
        assert_eq!(entry.text, "lis le parser\n");

        // A rename replaces the automatic title; clearing it falls back to the
        // first prompt without re-reading the transcript.
        let rename = "{\"type\":\"title_change\",\"id\":\"t1\",\"parentId\":\"m1\",\"timestamp\":\"t\",\"title\":\"Renommé\",\"source\":\"user\"}\n";
        std::fs::write(&path, format!("{slot}{header}{prompt}{rename}")).unwrap();
        let len = (slot.len() + header.len() + prompt.len() + rename.len()) as u64;
        assert!(index_file(&path, len, 20, &mut entry));
        assert_eq!(entry.title, "Renommé");
        let clear = "{\"type\":\"title_change\",\"id\":\"t2\",\"parentId\":\"t1\",\"timestamp\":\"t\",\"title\":\"\",\"source\":\"user\"}\n";
        std::fs::write(&path, format!("{slot}{header}{prompt}{rename}{clear}")).unwrap();
        let len = (slot.len() + header.len() + prompt.len() + rename.len() + clear.len()) as u64;
        assert!(index_file(&path, len, 30, &mut entry));
        assert_eq!(entry.title, "lis le parser");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn only_the_text_blocks_of_an_assistant_record_are_kept() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"first"},{"type":"text","text":"second"}]}}"#;
        assert_eq!(assistant_text(line).as_deref(), Some("first\nsecond"));
        assert_eq!(
            assistant_text(r#"{"type":"assistant","message":{"content":"plain string"}}"#).as_deref(),
            Some("plain string")
        );
        // A tool call carries no answer, and a record that is not the agent's
        // own is not a reply.
        assert!(assistant_text(r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"read"}]}}"#).is_none());
        assert!(assistant_text(r#"{"type":"user","message":{"content":"hi"}}"#).is_none());
        assert!(assistant_text(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"   "}]}}"#).is_none());
    }

    #[test]
    fn a_claude_answer_is_indexed_apart_from_the_prompts() {
        let dir = std::env::temp_dir().join(format!("kova-reply-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("r.jsonl");
        let prompt = "{\"type\":\"user\",\"promptSource\":\"typed\",\"cwd\":\"/tmp/p\",\"message\":{\"content\":\"where is the parser\"}}\n";
        let thinking = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"thinking\",\"thinking\":\"secret reasoning\"}]}}\n";
        let answer = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"In `src/parser.rs`\"},{\"type\":\"tool_use\",\"name\":\"read\",\"input\":{}}]}}\n";
        std::fs::write(&path, format!("{prompt}{thinking}{answer}")).unwrap();

        let mut entry = IndexedSession {
            agent: Agent::Claude,
            id: "r".into(),
            cwd: String::new(),
            title: String::new(),
            last_active: 0,
            indexed_len: 0,
            text: String::new(),
            reply: String::new(),
            prompts: 0,
            fallback_title: String::new(),
            titled: false,
        };
        let len = (prompt.len() + thinking.len() + answer.len()) as u64;
        assert!(index_file(&path, len, 10, &mut entry));
        assert_eq!(entry.text, "where is the parser\n");
        assert_eq!(entry.reply, "in `src/parser.rs`\n", "the answer, lowercased");
        assert_eq!(entry.prompts, 1, "an answer is not a prompt");
        assert_eq!(entry.title, "where is the parser");

        // A replaced file rebuilds both halves, not one over the other.
        std::fs::write(&path, answer).unwrap();
        assert!(index_file(&path, answer.len() as u64, 20, &mut entry));
        assert_eq!(entry.text, "");
        assert_eq!(entry.reply, "in `src/parser.rs`\n");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_omp_answer_goes_to_the_reply_and_the_question_to_the_prompts() {
        let dir = std::env::temp_dir().join(format!("kova-omp-reply-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("2026-10-27T15-54-09-646Z_abc.jsonl");
        let header = "{\"type\":\"session\",\"version\":3,\"id\":\"abc\",\"timestamp\":\"t\",\"cwd\":\"/Users/x/app\"}\n";
        let question = "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"lis le parser\"}]}}\n";
        let answer = "{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"thinking\",\"thinking\":\"hmm\"},{\"type\":\"text\",\"text\":\"Il est dans parser.rs\"}]}}\n";
        let tool = "{\"type\":\"message\",\"message\":{\"role\":\"toolResult\",\"content\":[{\"type\":\"text\",\"text\":\"file contents\"}]}}\n";
        std::fs::write(&path, format!("{header}{question}{answer}{tool}")).unwrap();

        let mut entry = IndexedSession {
            agent: Agent::Omp,
            id: "abc".into(),
            cwd: String::new(),
            title: String::new(),
            last_active: 0,
            indexed_len: 0,
            text: String::new(),
            reply: String::new(),
            prompts: 0,
            fallback_title: String::new(),
            titled: false,
        };
        let len = (header.len() + question.len() + answer.len() + tool.len()) as u64;
        assert!(index_file(&path, len, 10, &mut entry));
        assert_eq!(entry.text, "lis le parser\n");
        assert_eq!(entry.reply, "il est dans parser.rs\n");
        assert_eq!(entry.prompts, 1);
        assert_eq!(entry.title, "lis le parser");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Manual check that the real omp transcripts are indexed: run it with
    /// `cargo test claude_history -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn indexes_the_real_omp_transcripts() {
        let found = search("omp support", &[]);
        for hit in &found.hits {
            println!("{}", hit_label(hit));
        }
        assert!(
            found.hits.iter().any(|hit| hit.agent == Agent::Omp),
            "no omp session in {} hits",
            found.total
        );
    }

    /// Not part of the suite: it reads the real transcripts of this machine and
    /// writes the real index. Run it by hand with
    /// `cargo test claude_history -- --ignored --nocapture` to check the index
    /// against actual data and see what a cold pass costs.
    #[test]
    #[ignore]
    fn indexes_the_real_transcripts() {
        let t0 = std::time::Instant::now();
        let found = search("parser", &[]);
        let cold = t0.elapsed();
        let t1 = std::time::Instant::now();
        let again = search("parser", &[]);
        println!(
            "cold pass {:?}, warm pass {:?}, {} shown of {} matches",
            cold, t1.elapsed(), found.hits.len(), found.total
        );
        for h in found.hits.iter().take(5) {
            println!("  {}", hit_label(h));
        }
        // A word that matches hundreds of sessions must still show a short list,
        // and a second word must narrow it down.
        let t2 = std::time::Instant::now();
        let common = search("oui", &[]);
        println!("\"oui\": {} shown of {} matches in {:?}", common.hits.len(), common.total, t2.elapsed());
        assert!(common.hits.len() <= MAX_RESULTS);
        let t3 = std::time::Instant::now();
        let narrowed = search("parser mcp", &[]);
        println!("\"parser mcp\": {} matches in {:?}", narrowed.total, t3.elapsed());
        assert!(narrowed.total <= found.total);
        assert_eq!(found.total, again.total);
    }

    #[test]
    fn a_label_names_the_project_then_the_prompt() {
        let hit = Hit {
            agent: Agent::Claude,
            id: "abc".into(),
            cwd: "/Users/x/projects/app".into(),
            title: "reread the thread".into(),
            last_active: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        };
        let label = hit_label(&hit);
        assert!(label.starts_with("Claude · app · "), "got {}", label);
        assert!(label.ends_with("· reread the thread"), "got {}", label);
    }
}
