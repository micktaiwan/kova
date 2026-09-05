//! Index of past Claude Code conversations, so the search palette can find a
//! session that is no longer open in any pane and bring it back.
//!
//! Claude Code appends one JSON record per event to
//! `~/.claude/projects/<slug>/<session-id>.jsonl`. Those transcripts are big
//! (1.4 GB on this machine) and almost entirely tool output, so the index keeps
//! only what a search needs: the prompts the user typed, the project directory,
//! and when the file last moved. A transcript is append-only, so re-indexing
//! reads just the bytes added since last time — the first pass is the only one
//! that walks everything.
//!
//! The index is rebuilt from the search worker thread (never on the main
//! thread) and cached in `INDEX` for the rest of the process' life.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// Cap on the searchable text kept per session. A long session's prompts fit in
/// far less; the cap only bounds the pathological case so the index file cannot
/// grow without limit.
const MAX_TEXT_PER_SESSION: usize = 64 * 1024;

/// Chars of the first prompt kept as the row's title.
const MAX_TITLE_CHARS: usize = 100;

/// Archived rows shown for one query. Deliberately small: a common word matches
/// hundreds of sessions here ("oui" matches 379 of 1365), and a section that
/// long buries the open panes above it. The list is sorted by `score`, so the
/// cut falls on the sessions least likely to be the one wanted; the count of
/// what was cut is shown instead, as an invitation to type one more word.
const MAX_RESULTS: usize = 8;

/// How the five ranking signals weigh against each other. They are summed, so
/// the numbers compare directly: recency alone (max 1.0) cannot outrank a
/// session that is older but was reopened, matches in its title, and sits in
/// the project the focused pane is in.
const W_RECENCY: f64 = 1.0;
const W_INVESTMENT: f64 = 0.8;
const W_RESUMED: f64 = 0.6;
const W_SAME_PROJECT: f64 = 0.5;
const W_MATCH: f64 = 0.9;

/// Days after which recency counts half as much. Short enough that this
/// morning's session leads, long enough that a fortnight-old conversation with
/// every other signal in its favour still comes back.
const RECENCY_HALF_LIFE_DAYS: f64 = 10.0;

/// Prompt count at which a session counts as fully invested in; likewise for
/// its transcript size in MB, the number of reopenings, and the number of term
/// occurrences in its text. Saturating rather than linear: the difference
/// between 3 and 40 prompts says something, the one between 200 and 400 does
/// not.
const FULL_PROMPTS: f64 = 40.0;
const FULL_MEGABYTES: f64 = 20.0;
const FULL_RESUMES: f64 = 3.0;
const FULL_OCCURRENCES: f64 = 8.0;

/// What the index remembers about one transcript file.
#[derive(Clone, Serialize, Deserialize)]
pub struct IndexedSession {
    /// Conversation id — the argument `claude --resume` expects. Same as the
    /// file stem.
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
    /// Number of prompts the user typed in this session — the cheapest proxy
    /// for how much of the conversation is his rather than tool output.
    #[serde(default)]
    pub prompts: u32,
    /// How many times this session was reopened from the palette. The only
    /// vote the user casts on a conversation, and it costs him nothing.
    #[serde(default)]
    pub resumes: u32,
}

/// Schema of the on-disk index. Bumped whenever a field the ranking needs is
/// added: an entry already at its file's length is never re-read, so a new
/// counter would otherwise stay at zero for every session already indexed.
/// A bump costs one full pass (8 s here for 1.4 GB), once.
const INDEX_VERSION: u32 = 2;

/// The on-disk index, keyed by transcript path.
#[derive(Default, Serialize, Deserialize)]
pub struct Index {
    #[serde(default)]
    pub version: u32,
    pub sessions: HashMap<String, IndexedSession>,
}

/// What a query found in the archive: the rows to show, and how many sessions
/// matched in total — the two differ as soon as the query is a common word.
pub struct Results {
    pub hits: Vec<Hit>,
    pub total: usize,
}

/// One archived session matching a query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
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
}

/// Pull a typed prompt out of one transcript line, or `None` if the record is
/// anything else.
///
/// Most `type: "user"` records are not the user talking: tool results, skill
/// bodies and system reminders all come back under the same type. Recent Claude
/// Code versions settle it with `promptSource: "typed"`; older transcripts
/// (56 files out of 1364 here) have no such field, and there the tell is a
/// string `content` that is not a machine-injected block — those all open with
/// a `<` tag.
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
    let content = v.get("message")?.get("content")?.as_str()?;
    let accepted = match v.get("promptSource").and_then(|p| p.as_str()) {
        Some("typed") => true,
        Some(_) => false,
        None => !content.trim_start().starts_with('<'),
    };
    if !accepted {
        return None;
    }
    let text = content.trim();
    if text.is_empty() {
        return None;
    }
    Some(Prompt {
        text: text.to_string(),
        cwd: v.get("cwd").and_then(|c| c.as_str()).map(str::to_string),
    })
}

/// Cut a query into the terms a session must ALL carry.
///
/// One word is rarely enough here: "oui" matches 379 sessions of 1365. Matching
/// the query as a single string would make "dust mcp" find nothing at all (the
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

/// First line of a prompt, trimmed to a row-sized title.
pub fn title_from_prompt(prompt: &str) -> String {
    let first_line = prompt.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
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
        entry.title.clear();
        entry.prompts = 0;
        0
    } else {
        entry.indexed_len
    };

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
        let prompt = match typed_prompt(&line) {
            Some(p) => p,
            None => continue,
        };
        if entry.title.is_empty() {
            entry.title = title_from_prompt(&prompt.text);
        }
        if let Some(cwd) = prompt.cwd {
            entry.cwd = cwd;
        }
        entry.prompts = entry.prompts.saturating_add(1);
        if entry.text.len() < MAX_TEXT_PER_SESSION {
            entry.text.push_str(&prompt.text.to_lowercase());
            entry.text.push('\n');
        }
    }

    entry.indexed_len = consumed;
    entry.last_active = mtime;
    true
}

/// Bring the index in line with what is on disk. Returns the number of
/// transcripts that were read.
fn refresh(index: &mut Index) -> usize {
    let root = transcripts_root();
    let project_dirs = match std::fs::read_dir(&root) {
        Ok(d) => d,
        Err(e) => {
            log::debug!("Claude history: no transcripts at {} ({})", root.display(), e);
            return 0;
        }
    };

    let mut seen: Vec<String> = Vec::new();
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
            if crate::claude_session::resume_command(None, &id).is_none() {
                continue;
            }
            let meta = match file.metadata() {
                Ok(m) => m,
                Err(_) => continue,
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
                id: id.clone(),
                cwd: String::new(),
                title: String::new(),
                last_active: 0,
                indexed_len: 0,
                text: String::new(),
                prompts: 0,
                resumes: 0,
            });
            if index_file(&path, len, mtime, entry) {
                changed += 1;
            }
        }
    }

    // Drop entries whose transcript is gone, so a deleted conversation stops
    // showing up as something to resume.
    if seen.len() != index.sessions.len() {
        let alive: std::collections::HashSet<&String> = seen.iter().collect();
        index.sessions.retain(|k, _| alive.contains(k));
        changed += 1;
    }
    changed
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

/// The five signals a session is ranked on, pulled out of the index so the
/// arithmetic can be tested without a filesystem.
#[derive(Debug, Clone, Copy, Default)]
pub struct Signals {
    /// Seconds since the transcript was last written.
    pub age_secs: u64,
    /// Typed prompts in the session.
    pub prompts: u32,
    /// Transcript size in bytes.
    pub bytes: u64,
    /// Times the session was reopened from the palette.
    pub resumes: u32,
    /// The focused pane sits in the same directory as the session.
    pub same_project: bool,
    /// Share of the query terms the title or the project path carries, 0..1.
    pub title_share: f64,
    /// Total occurrences of the query terms in the typed prompts.
    pub occurrences: usize,
}

/// A saturating 0..1 curve: `full` maps to 1, and going further adds almost
/// nothing. Logarithmic so the low end — where the real difference sits — is
/// where the slope is.
fn saturate(value: f64, full: f64) -> f64 {
    if value <= 0.0 {
        return 0.0;
    }
    ((1.0 + value).ln() / (1.0 + full).ln()).min(1.0)
}

/// Rank one session against a query. Higher is better; the palette shows the
/// top `MAX_RESULTS`.
///
/// Date is one term among five, decayed rather than sorted on: sorting by date
/// alone means the conversation of the day always buries the one actually
/// wanted, which is the whole reason a bookmark feels needed in the first
/// place.
pub fn score(sig: &Signals) -> f64 {
    let age_days = sig.age_secs as f64 / 86_400.0;
    let recency = 0.5f64.powf(age_days / RECENCY_HALF_LIFE_DAYS);

    // What the user put into the session. Prompts carry most of it; the
    // transcript size only breaks ties, since it is mostly tool output.
    let megabytes = sig.bytes as f64 / (1024.0 * 1024.0);
    let investment =
        0.7 * saturate(sig.prompts as f64, FULL_PROMPTS) + 0.3 * saturate(megabytes, FULL_MEGABYTES);

    let resumed = saturate(sig.resumes as f64, FULL_RESUMES);
    let project = if sig.same_project { 1.0 } else { 0.0 };

    // A term in the title says the session is about it; a term buried in the
    // prompts may be an aside. Repetition is the tiebreaker between asides.
    let matched = 0.5 * sig.title_share.clamp(0.0, 1.0)
        + 0.5 * saturate(sig.occurrences as f64, FULL_OCCURRENCES);

    W_RECENCY * recency
        + W_INVESTMENT * investment
        + W_RESUMED * resumed
        + W_SAME_PROJECT * project
        + W_MATCH * matched
}

/// Non-overlapping occurrences of `needle` in `haystack`, both lowercased.
fn count_occurrences(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    haystack.matches(needle).count()
}

/// Record that a session was reopened from the palette. Runs off the caller's
/// thread: it takes the index lock, which the search worker may be holding.
pub fn record_resume(session_id: &str) {
    let id = session_id.to_string();
    std::thread::spawn(move || {
        let mut guard = INDEX.lock();
        let index = guard.get_or_insert_with(load_index);
        let mut touched = false;
        for entry in index.sessions.values_mut() {
            if entry.id == id {
                entry.resumes = entry.resumes.saturating_add(1);
                touched = true;
            }
        }
        if touched {
            save_index(index);
        }
    });
}

/// Refresh the index, then return the archived sessions matching `query`, best
/// first (see `score`). Sessions listed in `live_ids` are left out: they are
/// already open in a pane, and the palette lists those in its own section.
/// `focus_cwd` is the directory of the focused pane, so a session from the
/// project being worked in ranks above one from elsewhere.
///
/// Called from the search worker thread — the first call after a cold start
/// reads every transcript, which takes a moment.
pub fn search(query: &str, live_ids: &[String], focus_cwd: &str) -> Results {
    let terms = split_terms(query);
    if terms.is_empty() {
        return Results { hits: Vec::new(), total: 0 };
    }

    let mut guard = INDEX.lock();
    let index = guard.get_or_insert_with(load_index);
    if refresh(index) > 0 {
        save_index(index);
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let focus = focus_cwd.to_lowercase();

    let mut hits: Vec<(f64, Hit)> = index
        .sessions
        .values()
        .filter(|s| !s.title.is_empty())
        .filter(|s| !live_ids.iter().any(|id| id == &s.id))
        .filter_map(|s| {
            // Per field rather than one concatenated haystack: `text` alone can
            // reach 64 KB, and copying it for every session of every keystroke
            // is the one thing that would make a live search feel slow.
            let title = s.title.to_lowercase();
            let cwd = s.cwd.to_lowercase();
            let mut in_title = 0usize;
            let mut occurrences = 0usize;
            for t in &terms {
                let named = title.contains(t) || cwd.contains(t);
                let body = count_occurrences(&s.text, t);
                if !named && body == 0 {
                    return None;
                }
                if named {
                    in_title += 1;
                }
                occurrences += body;
            }
            let sig = Signals {
                age_secs: now.saturating_sub(s.last_active),
                prompts: s.prompts,
                bytes: s.indexed_len,
                resumes: s.resumes,
                same_project: !focus.is_empty() && cwd == focus,
                title_share: in_title as f64 / terms.len() as f64,
                occurrences,
            };
            Some((
                score(&sig),
                Hit {
                    id: s.id.clone(),
                    cwd: s.cwd.clone(),
                    title: s.title.clone(),
                    last_active: s.last_active,
                },
            ))
        })
        .collect();

    // Ties (two sessions of the same project scored alike) fall back to date.
    hits.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.1.last_active.cmp(&a.1.last_active))
    });
    let total = hits.len();
    hits.truncate(MAX_RESULTS);
    Results { hits: hits.into_iter().map(|(_, h)| h).collect(), total }
}

/// Row label for a hit: project, age, then the prompt that opened the session.
pub fn hit_label(hit: &Hit) -> String {
    let project = std::path::Path::new(&hit.cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?");
    format!(
        "{} · {} · {}",
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
        let line = r#"{"type":"user","promptSource":"typed","cwd":"/Users/x/projects/cto","message":{"role":"user","content":"  je trouve plus la session avec Dust  "}}"#;
        let p = typed_prompt(line).expect("a typed prompt");
        assert_eq!(p.text, "je trouve plus la session avec Dust");
        assert_eq!(p.cwd.as_deref(), Some("/Users/x/projects/cto"));
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

    /// A session with nothing going for it but its date, `days` old.
    fn plain(days: f64) -> Signals {
        Signals {
            age_secs: (days * 86_400.0) as u64,
            prompts: 10,
            bytes: 1024 * 1024,
            resumes: 0,
            same_project: false,
            title_share: 0.0,
            occurrences: 1,
        }
    }

    #[test]
    fn recency_decays_by_half_every_ten_days() {
        // Only the recency term moves, so the gap between the two is exactly
        // half of its weight.
        let fresh = score(&plain(0.0));
        let old = score(&plain(RECENCY_HALF_LIFE_DAYS));
        assert!((fresh - old - W_RECENCY * 0.5).abs() < 1e-9, "{} vs {}", fresh, old);
        // And it keeps halving rather than falling off a cliff.
        let older = score(&plain(2.0 * RECENCY_HALF_LIFE_DAYS));
        assert!((old - older - W_RECENCY * 0.25).abs() < 1e-9);
    }

    #[test]
    fn a_worked_session_outranks_a_three_turn_one_from_the_same_day() {
        let mut short = plain(3.0);
        short.prompts = 3;
        short.bytes = 200 * 1024;
        let mut long = plain(3.0);
        long.prompts = 60;
        long.bytes = 30 * 1024 * 1024;
        assert!(score(&long) > score(&short));
    }

    #[test]
    fn reopening_a_session_is_worth_more_than_a_few_days_of_freshness() {
        let mut reopened = plain(6.0);
        reopened.resumes = 2;
        // Never reopened, but three days newer.
        let fresher = plain(3.0);
        assert!(score(&reopened) > score(&fresher));
    }

    #[test]
    fn the_project_in_front_of_the_user_wins_a_tie() {
        let elsewhere = plain(4.0);
        let mut here = plain(4.0);
        here.same_project = true;
        assert!((score(&here) - score(&elsewhere) - W_SAME_PROJECT).abs() < 1e-9);
    }

    #[test]
    fn a_term_in_the_title_beats_the_same_term_buried_in_the_prompts() {
        let mut named = plain(4.0);
        named.title_share = 1.0;
        named.occurrences = 1;
        let mut aside = plain(4.0);
        aside.title_share = 0.0;
        aside.occurrences = 3;
        assert!(score(&named) > score(&aside));
        // Repetition still separates two asides.
        let mut repeated = aside;
        repeated.occurrences = 8;
        assert!(score(&repeated) > score(&aside));
    }

    #[test]
    fn saturating_curves_stay_between_zero_and_one() {
        assert_eq!(saturate(0.0, 10.0), 0.0);
        assert_eq!(saturate(-5.0, 10.0), 0.0);
        assert!((saturate(10.0, 10.0) - 1.0).abs() < 1e-9);
        assert_eq!(saturate(1_000.0, 10.0), 1.0, "past `full` it is capped");
        assert!(saturate(2.0, 10.0) < saturate(5.0, 10.0));
    }

    #[test]
    fn occurrences_are_counted_per_term() {
        assert_eq!(count_occurrences("dust dust mcp", "dust"), 2);
        assert_eq!(count_occurrences("dust", "mcp"), 0);
        assert_eq!(count_occurrences("dust", ""), 0);
    }

    #[test]
    fn a_query_is_cut_into_terms_that_must_all_match() {
        assert_eq!(split_terms("  Dust   MCP "), vec!["dust", "mcp"]);
        assert_eq!(split_terms("dust, mcp"), vec!["dust", "mcp"]);
        assert_eq!(split_terms("dust"), vec!["dust"]);
        assert!(split_terms("   ").is_empty());
    }

    #[test]
    fn a_title_is_the_first_line_capped() {
        assert_eq!(title_from_prompt("\n\nfirst line\nsecond line"), "first line");
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
            id: "s".into(),
            cwd: String::new(),
            title: String::new(),
            last_active: 0,
            indexed_len: 0,
            text: String::new(),
            prompts: 0,
            resumes: 0,
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

    /// Not part of the suite: it reads the real transcripts of this machine and
    /// writes the real index. Run it by hand with
    /// `cargo test claude_history -- --ignored --nocapture` to check the index
    /// against actual data and see what a cold pass costs.
    #[test]
    #[ignore]
    fn indexes_the_real_transcripts() {
        let t0 = std::time::Instant::now();
        let found = search("dust", &[], "");
        let cold = t0.elapsed();
        let t1 = std::time::Instant::now();
        let again = search("dust", &[], "");
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
        let common = search("oui", &[], "");
        println!("\"oui\": {} shown of {} matches in {:?}", common.hits.len(), common.total, t2.elapsed());
        assert!(common.hits.len() <= MAX_RESULTS);
        let t3 = std::time::Instant::now();
        let narrowed = search("dust mcp", &[], "");
        println!("\"dust mcp\": {} matches in {:?}", narrowed.total, t3.elapsed());
        assert!(narrowed.total <= found.total);
        assert_eq!(found.total, again.total);
    }

    #[test]
    fn a_label_names_the_project_then_the_prompt() {
        let hit = Hit {
            id: "abc".into(),
            cwd: "/Users/x/projects/cto".into(),
            title: "relis le thread".into(),
            last_active: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        };
        let label = hit_label(&hit);
        assert!(label.starts_with("cto · "), "got {}", label);
        assert!(label.ends_with("· relis le thread"), "got {}", label);
    }
}
