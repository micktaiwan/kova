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
/// long buries the open panes above it. The list is sorted most-recent-first, so
/// the cut falls on the oldest; the count of what was cut is shown instead, as
/// an invitation to type one more word.
const MAX_RESULTS: usize = 8;

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
}

/// The on-disk index, keyed by transcript path.
#[derive(Default, Serialize, Deserialize)]
pub struct Index {
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
    match serde_json::from_str(&data) {
        Ok(i) => i,
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

fn save_index(index: &Index) {
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

/// Refresh the index, then return the archived sessions matching `query`,
/// most recent first. Sessions listed in `live_ids` are left out: they are
/// already open in a pane, and the palette lists those in its own section.
///
/// Called from the search worker thread — the first call after a cold start
/// reads every transcript, which takes a moment.
pub fn search(query: &str, live_ids: &[String]) -> Results {
    let terms = split_terms(query);
    if terms.is_empty() {
        return Results { hits: Vec::new(), total: 0 };
    }

    let mut guard = INDEX.lock();
    let index = guard.get_or_insert_with(load_index);
    if refresh(index) > 0 {
        save_index(index);
    }

    let mut hits: Vec<Hit> = index
        .sessions
        .values()
        .filter(|s| !s.title.is_empty())
        .filter(|s| !live_ids.iter().any(|id| id == &s.id))
        .filter(|s| {
            // Per field rather than one concatenated haystack: `text` alone can
            // reach 64 KB, and copying it for every session of every keystroke
            // is the one thing that would make a live search feel slow.
            let title = s.title.to_lowercase();
            let cwd = s.cwd.to_lowercase();
            terms
                .iter()
                .all(|t| s.text.contains(t) || title.contains(t) || cwd.contains(t))
        })
        .map(|s| Hit {
            id: s.id.clone(),
            cwd: s.cwd.clone(),
            title: s.title.clone(),
            last_active: s.last_active,
        })
        .collect();

    hits.sort_by(|a, b| b.last_active.cmp(&a.last_active));
    let total = hits.len();
    hits.truncate(MAX_RESULTS);
    Results { hits, total }
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
        let found = search("dust", &[]);
        let cold = t0.elapsed();
        let t1 = std::time::Instant::now();
        let again = search("dust", &[]);
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
        let narrowed = search("dust mcp", &[]);
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
