//! What a Claude conversation still has to do ("reste à faire"): the todo and
//! idea list a Claude Code plugin keeps on disk, one file per session, at
//! `~/.claude/raf/<session-id>.json`.
//!
//! Kova only reads it, and only at two moments — when the pane switcher opens
//! and when a pane or tab is about to close — never on the per-frame path. The
//! file is written by something Kova does not control, so nothing in it is
//! trusted: the session id is checked before it becomes a path, a file that is
//! missing or does not parse means "no list", and the texts are only ever
//! displayed (cleaned and shortened first), never typed into a PTY.

use serde::Deserialize;

/// Longest text shown for one item before it is cut with an ellipsis.
pub(crate) const MAX_ITEM_CHARS: usize = 80;
/// How many todo texts the close confirmation lists before "… and N more".
pub(crate) const MAX_LISTED_TODOS: usize = 5;

/// A session's open items, split by kind. Texts are already cleaned for display.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Raf {
    pub todos: Vec<String>,
    pub ideas: Vec<String>,
}

#[derive(Deserialize)]
struct RafFile {
    #[serde(default)]
    items: Vec<RafItem>,
}

#[derive(Deserialize)]
struct RafItem {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    text: String,
}

/// Parse the plugin's JSON. `None` when it is not a JSON object of the expected
/// shape; items of an unknown kind and items with an empty text are skipped.
pub(crate) fn parse(json: &str) -> Option<Raf> {
    let file: RafFile = serde_json::from_str(json).ok()?;
    let mut raf = Raf::default();
    for item in file.items {
        let text = display_text(&item.text);
        if text.is_empty() {
            continue;
        }
        match item.kind.as_str() {
            "todo" => raf.todos.push(text),
            "idea" => raf.ideas.push(text),
            _ => {}
        }
    }
    Some(raf)
}

/// Read the list of `session_id`. `None` for an id that is not a plain
/// `[A-Za-z0-9_-]` token (it would otherwise steer the path), a missing file,
/// or one that does not parse.
pub(crate) fn load(session_id: &str) -> Option<Raf> {
    if !crate::claude_session::is_safe_session_id(session_id) {
        return None;
    }
    let home = std::env::var_os("HOME")?;
    let path = std::path::Path::new(&home).join(".claude/raf").join(format!("{session_id}.json"));
    let json = std::fs::read_to_string(path).ok()?;
    parse(&json)
}

/// Bidirectional formatting characters: embeddings and overrides
/// (U+202A–U+202E), isolates (U+2066–U+2069) and the LRM/RLM marks
/// (U+200E/U+200F). They are not `is_control`, yet one in a todo can flip the
/// rest of the alert's text so it reads differently from what it says.
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}')
}

/// One line of display text: control and bidi formatting characters
/// (newlines included) become spaces, runs of whitespace collapse, and anything past `MAX_ITEM_CHARS`
/// is cut with an ellipsis. Counted in chars, never bytes.
fn display_text(raw: &str) -> String {
    let cleaned: String = raw.chars().map(|c| if c.is_control() || is_bidi_control(c) { ' ' } else { c }).collect();
    let one_line = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= MAX_ITEM_CHARS {
        one_line
    } else {
        let mut cut: String = one_line.chars().take(MAX_ITEM_CHARS - 1).collect();
        cut.push('\u{2026}');
        cut
    }
}

/// The two halves of a pane switcher marker: `"3 todo"` and `"1 idea"` /
/// `"2 ideas"`, each `None` when its count is zero. The todo half is drawn in
/// colour and the idea half dim, so they stay separate rather than one string.
pub(crate) fn marker_parts(todos: usize, ideas: usize) -> (Option<String>, Option<String>) {
    let todo = (todos > 0).then(|| format!("{todos} todo"));
    let idea = match ideas {
        0 => None,
        1 => Some("1 idea".to_string()),
        n => Some(format!("{n} ideas")),
    };
    (todo, idea)
}

/// The paragraph the close confirmation adds when the closing pane(s) hold
/// open todos. `todos` are every open todo across the closing conversations,
/// `conversations` how many conversations they come from. Empty when there are
/// no todos: ideas never warn.
pub(crate) fn close_warning(todos: &[String], conversations: usize) -> String {
    if todos.is_empty() {
        return String::new();
    }
    let things = if todos.len() == 1 { "1 thing".to_string() } else { format!("{} things", todos.len()) };
    let place = if conversations > 1 {
        format!("these {conversations} conversations")
    } else {
        "this conversation".to_string()
    };
    let mut out = format!("{things} left to do in {place}:");
    for todo in todos.iter().take(MAX_LISTED_TODOS) {
        out.push_str("\n\u{2022} ");
        out.push_str(todo);
    }
    if todos.len() > MAX_LISTED_TODOS {
        out.push_str(&format!("\n\u{2026} and {} more", todos.len() - MAX_LISTED_TODOS));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_splits_todos_and_ideas() {
        let json = r#"{"session_id":"abc","cwd":"/tmp","updated_at":1760000000000,
            "items":[{"kind":"todo","text":"ship it"},{"kind":"idea","text":"maybe later"},
                     {"kind":"todo","text":"write tests"}]}"#;
        let raf = parse(json).unwrap();
        assert_eq!(raf.todos, vec!["ship it", "write tests"]);
        assert_eq!(raf.ideas, vec!["maybe later"]);
    }

    #[test]
    fn parse_empty_items_is_an_empty_list() {
        let raf = parse(r#"{"session_id":"abc","cwd":"/","updated_at":1,"items":[]}"#).unwrap();
        assert_eq!(raf, Raf::default());
        // A file without the key at all reads the same.
        assert_eq!(parse(r#"{"session_id":"abc"}"#).unwrap(), Raf::default());
    }

    #[test]
    fn parse_refuses_garbage() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("not json"), None);
        assert_eq!(parse("[1,2,3]"), None);
        assert_eq!(parse(r#"{"items":"nope"}"#), None);
    }

    #[test]
    fn parse_ignores_unknown_kinds_and_empty_texts() {
        let json = r#"{"items":[{"kind":"note","text":"x"},{"text":"no kind"},
            {"kind":"todo","text":"   "},{"kind":"todo","text":"real"}]}"#;
        let raf = parse(json).unwrap();
        assert_eq!(raf.todos, vec!["real"]);
        assert!(raf.ideas.is_empty());
    }

    #[test]
    fn texts_are_flattened_to_one_line() {
        let json = r#"{"items":[{"kind":"todo","text":"line one\nline two\r\n\tend\u001b[31m"}]}"#;
        assert_eq!(parse(json).unwrap().todos, vec!["line one line two end [31m"]);
    }

    #[test]
    fn bidi_formatting_characters_are_neutralized() {
        let raw = "a\u{202E}b\u{202A}c\u{202D}d\u{2066}e\u{2069}f\u{200E}g\u{200F}h\u{2067}i\u{2068}j\u{202B}k\u{202C}l";
        assert_eq!(display_text(raw), "a b c d e f g h i j k l");
        // Through the JSON path too, and ordinary non-Latin text is untouched.
        let raw_json = r#"{"items":[{"kind":"todo","text":"pay \u202Etxt.exe\u202C עברית"}]}"#;
        assert_eq!(parse(raw_json).unwrap().todos, vec!["pay txt.exe עברית"]);
    }

    #[test]
    fn long_texts_are_cut_on_chars_with_an_ellipsis() {
        let long = "é".repeat(200);
        let json = format!(r#"{{"items":[{{"kind":"idea","text":"{long}"}}]}}"#);
        let idea = &parse(&json).unwrap().ideas[0];
        assert_eq!(idea.chars().count(), MAX_ITEM_CHARS);
        assert!(idea.ends_with('\u{2026}'));
    }

    #[test]
    fn load_refuses_an_id_that_could_steer_the_path() {
        assert_eq!(load("../../etc/passwd"), None);
        assert_eq!(load("a/b"), None);
        assert_eq!(load(""), None);
    }

    #[test]
    fn marker_parts_name_each_kind_on_its_own() {
        assert_eq!(marker_parts(0, 0), (None, None));
        assert_eq!(marker_parts(3, 0), (Some("3 todo".into()), None));
        assert_eq!(marker_parts(3, 1), (Some("3 todo".into()), Some("1 idea".into())));
        assert_eq!(marker_parts(0, 2), (None, Some("2 ideas".into())));
    }

    #[test]
    fn close_warning_is_empty_without_todos() {
        assert_eq!(close_warning(&[], 1), "");
    }

    #[test]
    fn close_warning_lists_the_todos() {
        let todos = vec!["ship it".to_string(), "write tests".to_string()];
        assert_eq!(
            close_warning(&todos, 1),
            "2 things left to do in this conversation:\n\u{2022} ship it\n\u{2022} write tests"
        );
        assert_eq!(
            close_warning(&todos[..1], 1),
            "1 thing left to do in this conversation:\n\u{2022} ship it"
        );
    }

    #[test]
    fn close_warning_caps_the_list_at_five() {
        let todos: Vec<String> = (1..=7).map(|i| format!("t{i}")).collect();
        let w = close_warning(&todos, 2);
        assert!(w.starts_with("7 things left to do in these 2 conversations:"));
        assert_eq!(w.matches('\u{2022}').count(), 5);
        assert!(w.ends_with("\u{2026} and 2 more"));
        assert!(!w.contains("t6"));
    }
}
