//! Decision cards: the structured document an agent can put on the question
//! card instead of one line of text (`docs/decision-cards-spec.md`).
//!
//! Everything here is pure logic, tested without AppKit: the card document and
//! its validation, the plain-text fallback derived from it, the inline markup
//! parser, and the grant a click on a grant action writes for the outbound
//! gate. The card is drawn by `crate::ask_card`; questions live in `crate::ask`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The one format version this Kova knows.
pub const VERSION: u64 = 1;
/// Whole document, measured on its compact JSON.
pub const MAX_BYTES: usize = 64 * 1024;
/// A `message` given by the caller alongside a card.
pub const MAX_MESSAGE: usize = 1000;
/// A grant older than this is dead: the gate refuses it and Kova deletes it at launch.
pub const GRANT_TTL_SECS: u64 = 24 * 3600;

// ─── The document ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Card {
    pub version: u64,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    pub blocks: Vec<Block>,
    pub actions: Vec<CardAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<CardInput>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Block {
    Text {
        text: String,
    },
    Quote {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        author: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        time: Option<String>,
    },
    Code {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    Fields {
        items: Vec<FieldItem>,
    },
    Divider,
    Context {
        text: String,
    },
    /// A plain table; cells take the inline markup of `text`.
    Table {
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    /// A horizontal bar chart, scaled to the largest value.
    Bars {
        items: Vec<BarItem>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unit: Option<String>,
    },
    /// A one-line chart, its last value printed after it.
    Sparkline {
        values: Vec<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    /// A thin progress bar, `value` from 0 to 1.
    Progress {
        value: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BarItem {
    pub label: String,
    pub value: f64,
}

/// A number as the charts print it: at most 2 decimals, trailing zeros
/// removed, no thousands separator.
pub fn format_number(n: f64) -> String {
    let s = format!("{:.2}", n);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".to_string() } else { s.to_string() }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldItem {
    pub key: String,
    pub value: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Style {
    Primary,
    #[default]
    Default,
    Destructive,
}

impl Style {
    fn is_default(&self) -> bool {
        *self == Style::Default
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardAction {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Style::is_default")]
    pub style: Style,
    #[serde(default, skip_serializing_if = "is_false")]
    pub needs_input: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<GrantDecl>,
}

/// What a grant action declares (§ 8.2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantDecl {
    pub kind: String,
    pub target: String,
    pub payload_sha256: String,
    pub payload_block: usize,
}

impl Card {
    pub fn action(&self, id: &str) -> Option<&CardAction> {
        self.actions.iter().find(|a| a.id == id)
    }

    pub fn primary(&self) -> Option<&CardAction> {
        self.actions.iter().find(|a| a.style == Style::Primary)
    }

    /// What an older Kova shows as buttons: every action but the grant one.
    pub fn legacy_actions(&self) -> Vec<String> {
        self.actions.iter().filter(|a| a.grant.is_none()).map(|a| a.id.clone()).collect()
    }

    pub fn grant_action(&self) -> Option<&CardAction> {
        self.actions.iter().find(|a| a.grant.is_some())
    }
}

/// The plain-text fallback for a card, from its raw JSON so the CLI can build
/// it for an older Kova before anything is validated: the title, then ` — `
/// and the first 200 chars of the first `text` block, newlines as spaces.
pub fn derive_message(card: &Value) -> String {
    let flat = |s: &str| s.chars().map(|c| if c == '\n' || c == '\t' { ' ' } else { c }).collect::<String>();
    let title = card.get("title").and_then(Value::as_str).map(flat).unwrap_or_default();
    let first_text = card
        .get("blocks")
        .and_then(Value::as_array)
        .and_then(|blocks| blocks.iter().find(|b| b.get("type").and_then(Value::as_str) == Some("text")))
        .and_then(|b| b.get("text"))
        .and_then(Value::as_str)
        .map(|t| flat(&t.chars().take(200).collect::<String>()));
    match first_text {
        Some(text) if !text.trim().is_empty() => format!("{} — {}", title, text),
        _ => title,
    }
}

/// The legacy `actions` / `input` of a question, from the card's raw JSON
/// (best effort: a malformed card is refused later, by the Kova that knows cards).
pub fn legacy_fields(card: &Value) -> (Vec<String>, bool) {
    let actions = card
        .get("actions")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter(|a| a.get("grant").is_none())
                .filter_map(|a| a.get("id").and_then(Value::as_str).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let input = card.get("input").is_some_and(Value::is_object);
    (actions, input)
}

// ─── Validation (§ 9) ──────────────────────────────────────────────────────────

/// What validation needs to know of the asker: a grant is bound to a Claude
/// Code session.
pub struct AskerFacts<'a> {
    pub session_id: Option<&'a str>,
    pub claude: bool,
}

/// Validate a card as received, in the order of the spec (§ 9), and return
/// the first error only. `message` is the caller's own `message`, `None` when
/// it gave none (Kova derives it then).
pub fn validate(card: &Value, message: Option<&str>, asker: &AskerFacts) -> Result<Card, String> {
    let size = serde_json::to_string(card).map(|s| s.len()).unwrap_or(usize::MAX);
    if size > MAX_BYTES {
        return Err("card too large".into());
    }
    check_controls(card, "")?;
    if message.is_some_and(|m| m.chars().count() > MAX_MESSAGE) {
        return Err("message too long".into());
    }
    let obj = card.as_object().ok_or("card must be an object")?;
    for key in obj.keys() {
        if !["version", "title", "subtitle", "blocks", "actions", "input"].contains(&key.as_str()) {
            return Err(format!("unknown field {}", key));
        }
    }

    // § 4 document fields.
    match obj.get("version") {
        None => return Err("missing version".into()),
        Some(v) => match v.as_u64() {
            Some(VERSION) => {}
            Some(n) => return Err(format!("unknown card version {}", n)),
            None => return Err(format!("unknown card version {}", v)),
        },
    }
    let title = required_str(obj, "title", "title")?;
    if title.trim().is_empty() {
        return Err("empty title".into());
    }
    limit(title, 120, "title")?;
    if let Some(sub) = optional_str(obj, "subtitle", "subtitle")? {
        limit(sub, 200, "subtitle")?;
    }
    let has_input = match obj.get("input") {
        None => false,
        Some(Value::Object(input)) => {
            for key in input.keys() {
                if key != "placeholder" {
                    return Err(format!("unknown field input.{}", key));
                }
            }
            if let Some(p) = optional_str(input, "placeholder", "input.placeholder")? {
                limit(p, 200, "input.placeholder")?;
            }
            true
        }
        Some(_) => return Err("input must be an object".into()),
    };

    // § 4.1 blocks.
    let blocks = obj.get("blocks").ok_or("missing blocks")?.as_array().ok_or("blocks must be an array")?;
    if blocks.is_empty() {
        return Err("a card needs a block".into());
    }
    if blocks.len() > 20 {
        return Err("too many blocks".into());
    }
    for (i, block) in blocks.iter().enumerate() {
        validate_block(block, i)?;
    }

    // § 5 actions, § 8 grant.
    let actions = obj.get("actions").ok_or("missing actions")?.as_array().ok_or("actions must be an array")?;
    if actions.is_empty() {
        return Err("a card needs an action".into());
    }
    if actions.len() > 6 {
        return Err("too many actions".into());
    }
    let mut ids: Vec<&str> = Vec::new();
    let mut primaries = 0;
    let mut grants = 0;
    for (i, action) in actions.iter().enumerate() {
        let path = format!("actions[{}]", i);
        let a = action.as_object().ok_or_else(|| format!("{} must be an object", path))?;
        for key in a.keys() {
            if !["id", "label", "style", "needs_input", "grant"].contains(&key.as_str()) {
                return Err(format!("unknown field {}.{}", path, key));
            }
        }
        let id = required_str(a, "id", &format!("{}.id", path))?;
        if !is_action_id(id) {
            return Err(format!("bad action id {}", id));
        }
        if ids.contains(&id) {
            return Err(format!("duplicate action id {}", id));
        }
        ids.push(id);
        let label = required_str(a, "label", &format!("{}.label", path))?;
        if label.trim().is_empty() {
            return Err(format!("empty {}.label", path));
        }
        limit(label, 24, &format!("{}.label", path))?;
        match a.get("style") {
            None => {}
            Some(Value::String(s)) if s == "primary" => primaries += 1,
            Some(Value::String(s)) if s == "default" || s == "destructive" => {}
            Some(other) => return Err(format!("unknown style {} in {}.style", other, path)),
        }
        if primaries > 1 {
            return Err("more than one primary action".into());
        }
        let needs_input = match a.get("needs_input") {
            None => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err(format!("{}.needs_input must be a boolean", path)),
        };
        if needs_input && !has_input {
            return Err("needs_input without input".into());
        }
        if let Some(grant) = a.get("grant") {
            grants += 1;
            if grants > 1 {
                return Err("more than one grant action".into());
            }
            if needs_input {
                return Err("grant action cannot need input".into());
            }
            validate_grant(grant, &format!("{}.grant", path), blocks)?;
        }
    }
    if grants > 0 {
        if grants == actions.len() {
            return Err("a card needs a non-grant action".into());
        }
        if asker.session_id.is_none() {
            return Err("grant requires an asker session id".into());
        }
        if !asker.claude {
            return Err("grant requires a Claude Code asker".into());
        }
    }
    serde_json::from_value::<Card>(card.clone()).map_err(|e| format!("invalid card: {}", e))
}

fn validate_block(block: &Value, i: usize) -> Result<(), String> {
    let path = format!("blocks[{}]", i);
    let b = block.as_object().ok_or_else(|| format!("{} must be an object", path))?;
    let kind = match b.get("type") {
        None => return Err(format!("missing {}.type", path)),
        Some(Value::String(t)) => t.as_str(),
        Some(other) => return Err(format!("unknown block type {}", other)),
    };
    let allowed: &[&str] = match kind {
        "text" | "context" => &["type", "text"],
        "quote" => &["type", "text", "author", "time"],
        "code" => &["type", "text", "label"],
        "fields" => &["type", "items"],
        "divider" => &["type"],
        "table" => &["type", "columns", "rows"],
        "bars" => &["type", "items", "unit"],
        "sparkline" => &["type", "values", "label"],
        "progress" => &["type", "value", "label"],
        other => return Err(format!("unknown block type {}", other)),
    };
    for key in b.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!(
                "unknown field {}.{} (a \"{}\" block accepts: {})",
                path,
                key,
                kind,
                allowed.join(", ")
            ));
        }
    }
    let text = |max: usize| -> Result<(), String> {
        let field = format!("{}.text", path);
        limit(required_str(b, "text", &field)?, max, &field)
    };
    let opt = |key: &str, max: usize| -> Result<(), String> {
        let field = format!("{}.{}", path, key);
        match optional_str(b, key, &field)? {
            Some(s) => limit(s, max, &field),
            None => Ok(()),
        }
    };
    match kind {
        "text" => text(4000),
        "context" => text(300),
        "quote" => {
            text(4000)?;
            opt("author", 80)?;
            opt("time", 40)
        }
        "code" => {
            text(8000)?;
            opt("label", 40)
        }
        "fields" => {
            let items = b
                .get("items")
                .ok_or_else(|| format!("missing {}.items", path))?
                .as_array()
                .ok_or_else(|| format!("{}.items must be an array", path))?;
            if items.is_empty() {
                return Err(format!("{}.items needs an item", path));
            }
            if items.len() > 12 {
                return Err(format!("too many {}.items", path));
            }
            for (j, item) in items.iter().enumerate() {
                let ipath = format!("{}.items[{}]", path, j);
                let it = item.as_object().ok_or_else(|| format!("{} must be an object", ipath))?;
                for key in it.keys() {
                    if key != "key" && key != "value" {
                        return Err(format!("unknown field {}.{}", ipath, key));
                    }
                }
                limit(required_str(it, "key", &format!("{}.key", ipath))?, 40, &format!("{}.key", ipath))?;
                limit(required_str(it, "value", &format!("{}.value", ipath))?, 300, &format!("{}.value", ipath))?;
            }
            Ok(())
        }
        "table" => {
            let columns = array(b, "columns", &path)?;
            if columns.is_empty() || columns.len() > 8 {
                return Err(format!("{}.columns needs 1 to 8 columns", path));
            }
            for (j, c) in columns.iter().enumerate() {
                let field = format!("{}.columns[{}]", path, j);
                limit(c.as_str().ok_or_else(|| format!("{} must be a string", field))?, 40, &field)?;
            }
            let rows = array(b, "rows", &path)?;
            if rows.len() > 100 {
                return Err(format!("too many {}.rows", path));
            }
            for (j, row) in rows.iter().enumerate() {
                let rpath = format!("{}.rows[{}]", path, j);
                let cells = row.as_array().ok_or_else(|| format!("{} must be an array", rpath))?;
                if cells.len() != columns.len() {
                    return Err(format!("{} has the wrong number of cells", rpath));
                }
                for (k, cell) in cells.iter().enumerate() {
                    let field = format!("{}[{}]", rpath, k);
                    limit(cell.as_str().ok_or_else(|| format!("{} must be a string", field))?, 200, &field)?;
                }
            }
            Ok(())
        }
        "bars" => {
            let items = array(b, "items", &path)?;
            if items.is_empty() || items.len() > 20 {
                return Err(format!("{}.items needs 1 to 20 items", path));
            }
            for (j, item) in items.iter().enumerate() {
                let ipath = format!("{}.items[{}]", path, j);
                let it = item.as_object().ok_or_else(|| format!("{} must be an object", ipath))?;
                for key in it.keys() {
                    if key != "label" && key != "value" {
                        return Err(format!("unknown field {}.{}", ipath, key));
                    }
                }
                limit(required_str(it, "label", &format!("{}.label", ipath))?, 40, &format!("{}.label", ipath))?;
                let value = number(it, "value", &format!("{}.value", ipath))?;
                if value < 0.0 {
                    return Err(format!("{}.value out of range", ipath));
                }
            }
            opt("unit", 10)
        }
        "sparkline" => {
            let values = array(b, "values", &path)?;
            if values.len() < 2 || values.len() > 200 {
                return Err(format!("{}.values needs 2 to 200 values", path));
            }
            for (j, v) in values.iter().enumerate() {
                if !v.as_f64().is_some_and(f64::is_finite) {
                    return Err(format!("{}.values[{}] is not a finite number", path, j));
                }
            }
            opt("label", 40)
        }
        "progress" => {
            let value = number(b, "value", &format!("{}.value", path))?;
            if !(0.0..=1.0).contains(&value) {
                return Err(format!("{}.value out of range", path));
            }
            opt("label", 80)
        }
        _ => Ok(()),
    }
}

fn array<'a>(obj: &'a serde_json::Map<String, Value>, key: &str, path: &str) -> Result<&'a Vec<Value>, String> {
    obj.get(key)
        .ok_or_else(|| format!("missing {}.{}", path, key))?
        .as_array()
        .ok_or_else(|| format!("{}.{} must be an array", path, key))
}

fn number(obj: &serde_json::Map<String, Value>, key: &str, field: &str) -> Result<f64, String> {
    match obj.get(key) {
        None => Err(format!("missing {}", field)),
        Some(v) => v.as_f64().filter(|n| n.is_finite()).ok_or_else(|| format!("{} is not a finite number", field)),
    }
}

fn validate_grant(grant: &Value, path: &str, blocks: &[Value]) -> Result<(), String> {
    let g = grant.as_object().ok_or_else(|| format!("{} must be an object", path))?;
    for key in g.keys() {
        if !["kind", "target", "payload_sha256", "payload_block"].contains(&key.as_str()) {
            return Err(format!("unknown field {}.{}", path, key));
        }
    }
    let kind = required_str(g, "kind", &format!("{}.kind", path))?;
    if kind != "outbound" {
        return Err(format!("unknown grant kind {}", kind));
    }
    let target = required_str(g, "target", &format!("{}.target", path))?;
    if target.is_empty() {
        return Err(format!("empty {}.target", path));
    }
    limit(target, 200, &format!("{}.target", path))?;
    let sha = required_str(g, "payload_sha256", &format!("{}.payload_sha256", path))?;
    if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return Err("payload_sha256 is not 64 hex chars".into());
    }
    let index = match g.get("payload_block") {
        None => return Err(format!("missing {}.payload_block", path)),
        Some(v) => v.as_u64().ok_or_else(|| format!("{}.payload_block must be an index", path))? as usize,
    };
    let block = blocks.get(index).ok_or_else(|| {
        format!("payload_block out of range: it is a 0-based index and the card has {} blocks (the first block is 0)", blocks.len())
    })?;
    if block.get("type").and_then(Value::as_str) != Some("code") {
        return Err("payload_block is not a code block".into());
    }
    let payload = block.get("text").and_then(Value::as_str).unwrap_or_default();
    if sha256_hex(payload.as_bytes()) != sha {
        return Err(format!("payload_sha256 does not match block {}", index));
    }
    Ok(())
}

/// `[a-z0-9_-]{1,32}`.
pub fn is_action_id(id: &str) -> bool {
    (1..=32).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn required_str<'a>(obj: &'a serde_json::Map<String, Value>, key: &str, field: &str) -> Result<&'a str, String> {
    match obj.get(key) {
        None => Err(format!("missing {}", field)),
        Some(Value::String(s)) => Ok(s),
        Some(_) => Err(format!("{} must be a string", field)),
    }
}

fn optional_str<'a>(obj: &'a serde_json::Map<String, Value>, key: &str, field: &str) -> Result<Option<&'a str>, String> {
    match obj.get(key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(format!("{} must be a string", field)),
    }
}

fn limit(s: &str, max: usize, field: &str) -> Result<(), String> {
    if s.chars().count() > max {
        Err(format!("{} too long", field))
    } else {
        Ok(())
    }
}

/// Every string of the card, keys included: no control character but `\n` and `\t`.
fn check_controls(v: &Value, path: &str) -> Result<(), String> {
    let bad = |s: &str| s.chars().any(|c| c.is_control() && c != '\n' && c != '\t');
    let name = |p: &str| if p.is_empty() { "card".to_string() } else { p.to_string() };
    match v {
        Value::String(s) if bad(s) => Err(format!("control character in {}", name(path))),
        Value::Array(list) => {
            for (i, item) in list.iter().enumerate() {
                check_controls(item, &format!("{}[{}]", path, i))?;
            }
            Ok(())
        }
        Value::Object(map) => {
            for (key, item) in map {
                let sub = if path.is_empty() { key.clone() } else { format!("{}.{}", path, key) };
                if bad(key) {
                    return Err(format!("control character in {}", name(path)));
                }
                check_controls(item, &sub)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

// ─── The view document (`docs/pane-ui-spec.md` § 4) ────────────────────────────

/// A whole view, measured on its compact JSON.
pub const MAX_VIEW_BYTES: usize = 512 * 1024;
pub const MAX_CARDS: usize = 50;

/// A pane view: a header and a list of cards.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewDoc {
    pub version: u64,
    pub title: String,
    pub cards: Vec<ViewCard>,
    /// Close the view once its last card is removed (pane-ui-spec § 2).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub close_when_empty: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewCard {
    pub id: String,
    pub card: Card,
}

/// Validate a view document: its own fields, then each card exactly as a
/// `kova ask` card, its errors prefixed with `cards[i].card: `.
pub fn validate_view(view: &Value, asker: &AskerFacts) -> Result<ViewDoc, String> {
    let size = serde_json::to_string(view).map(|s| s.len()).unwrap_or(usize::MAX);
    if size > MAX_VIEW_BYTES {
        return Err("view too large".into());
    }
    check_controls(view, "")?;
    let obj = view.as_object().ok_or("view must be an object")?;
    for key in obj.keys() {
        if !["version", "title", "cards", "close_when_empty"].contains(&key.as_str()) {
            return Err(format!("unknown field {}", key));
        }
    }
    if let Some(v) = obj.get("close_when_empty") {
        if !v.is_boolean() {
            return Err("close_when_empty must be a boolean".into());
        }
    }
    match obj.get("version") {
        None => return Err("missing version".into()),
        Some(v) => match v.as_u64() {
            Some(VERSION) => {}
            _ => return Err(format!("unknown view version {}", v)),
        },
    }
    let title = required_str(obj, "title", "title")?;
    if title.trim().is_empty() {
        return Err("empty title".into());
    }
    limit(title, 120, "title")?;
    let cards = obj.get("cards").ok_or("missing cards")?.as_array().ok_or("cards must be an array")?;
    if cards.len() > MAX_CARDS {
        return Err("too many cards".into());
    }
    let mut ids: Vec<&str> = Vec::new();
    for (i, item) in cards.iter().enumerate() {
        let path = format!("cards[{}]", i);
        let it = item.as_object().ok_or_else(|| format!("{} must be an object", path))?;
        for key in it.keys() {
            if key != "id" && key != "card" {
                return Err(format!("unknown field {}.{}", path, key));
            }
        }
        let id = required_str(it, "id", &format!("{}.id", path))?;
        if !is_action_id(id) {
            return Err(format!("bad card id {}", id));
        }
        if ids.contains(&id) {
            return Err(format!("duplicate card id {}", id));
        }
        ids.push(id);
        let card = it.get("card").ok_or_else(|| format!("missing {}.card", path))?;
        validate(card, None, asker).map_err(|e| format!("{}.card: {}", path, e))?;
    }
    serde_json::from_value::<ViewDoc>(view.clone()).map_err(|e| format!("invalid view: {}", e))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).iter().map(|b| format!("{:02x}", b)).collect()
}

// ─── Inline markup (§ 4.1) ─────────────────────────────────────────────────────

/// A run of text with one set of attributes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    /// Only `https:` and `slack:` links are links; the rest is plain text.
    pub link: Option<String>,
}

impl Span {
    fn plain(text: impl Into<String>) -> Span {
        Span { text: text.into(), bold: false, italic: false, code: false, link: None }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    /// A character, `escaped` when it came from `\x` (never a marker then).
    Ch(char, bool),
    /// A code span's literal content.
    Code(String),
}

fn tokenize(text: &str) -> Vec<Tok> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() && matches!(chars[i + 1], '*' | '_' | '`' | '[' | '\\') {
            out.push(Tok::Ch(chars[i + 1], true));
            i += 2;
            continue;
        }
        if c == '`' {
            if let Some(end) = chars[i + 1..].iter().position(|&x| x == '`') {
                let content: String = chars[i + 1..i + 1 + end].iter().collect();
                if !content.is_empty() {
                    out.push(Tok::Code(content));
                    i += end + 2;
                    continue;
                }
            }
        }
        out.push(Tok::Ch(c, false));
        i += 1;
    }
    out
}

fn is_punct(c: char) -> bool {
    c.is_ascii_punctuation() && c != '_' && c != '*'
}

fn marker(t: Option<&Tok>, c: char) -> bool {
    matches!(t, Some(Tok::Ch(x, false)) if *x == c)
}

/// The spans of `text` under the inline markup rules of § 4.1.
pub fn parse_markup(text: &str) -> Vec<Span> {
    let toks = tokenize(text);
    let mut spans: Vec<Span> = Vec::new();
    let push = |spans: &mut Vec<Span>, span: Span| {
        if span.text.is_empty() {
            return;
        }
        if let Some(last) = spans.last_mut() {
            if last.bold == span.bold && last.italic == span.italic && last.code == span.code && last.link == span.link {
                last.text.push_str(&span.text);
                return;
            }
        }
        spans.push(span);
    };
    // Literal tokens, with `bold`/`italic` applied; code tokens stay code.
    let emit = |spans: &mut Vec<Span>, toks: &[Tok], bold: bool, italic: bool| {
        for t in toks {
            let span = match t {
                Tok::Ch(c, _) => Span { text: c.to_string(), bold, italic, code: false, link: None },
                Tok::Code(s) => Span { text: s.clone(), bold, italic, code: true, link: None },
            };
            push(spans, span);
        }
    };
    let mut i = 0;
    while i < toks.len() {
        // **bold**
        if marker(toks.get(i), '*') && marker(toks.get(i + 1), '*') {
            let close = (i + 2..toks.len().saturating_sub(1)).find(|&j| marker(toks.get(j), '*') && marker(toks.get(j + 1), '*'));
            if let Some(j) = close.filter(|&j| j > i + 2) {
                emit(&mut spans, &toks[i + 2..j], true, false);
                i = j + 2;
                continue;
            }
            emit(&mut spans, &toks[i..i + 2], false, false);
            i += 2;
            continue;
        }
        // _italic_
        if marker(toks.get(i), '_') {
            let opens = i == 0
                || matches!(toks.get(i - 1), Some(Tok::Ch(c, _)) if c.is_whitespace() || is_punct(*c));
            if opens {
                let close = (i + 2..toks.len()).find(|&j| {
                    marker(toks.get(j), '_')
                        && match toks.get(j + 1) {
                            None => true,
                            Some(Tok::Ch(c, _)) => c.is_whitespace() || is_punct(*c),
                            Some(Tok::Code(_)) => false,
                        }
                });
                if let Some(j) = close {
                    emit(&mut spans, &toks[i + 1..j], false, true);
                    i = j + 1;
                    continue;
                }
            }
        }
        // [label](url)
        if marker(toks.get(i), '[') {
            if let Some(link) = parse_link(&toks, i) {
                let (label, url, next) = link;
                if url.starts_with("https:") || url.starts_with("slack:") {
                    push(&mut spans, Span { text: label, bold: false, italic: false, code: false, link: Some(url) });
                } else {
                    push(&mut spans, Span::plain(format!("{} ({})", label, url)));
                }
                i = next;
                continue;
            }
        }
        emit(&mut spans, &toks[i..i + 1], false, false);
        i += 1;
    }
    spans
}

/// `[label](url)` starting at `i`: the label, the URL, and the index after `)`.
fn parse_link(toks: &[Tok], i: usize) -> Option<(String, String, usize)> {
    let close = (i + 1..toks.len()).find(|&j| marker(toks.get(j), ']'))?;
    if !marker(toks.get(close + 1), '(') {
        return None;
    }
    let mut label = String::new();
    for t in &toks[i + 1..close] {
        match t {
            Tok::Ch(c, _) => label.push(*c),
            Tok::Code(s) => label.push_str(s),
        }
    }
    let mut url = String::new();
    let mut j = close + 2;
    loop {
        match toks.get(j)? {
            Tok::Ch(')', _) => break,
            Tok::Ch(c, _) => url.push(*c),
            Tok::Code(_) => return None,
        }
        j += 1;
    }
    if label.is_empty() || url.is_empty() {
        return None;
    }
    Some((label, url, j + 1))
}

// ─── Grants (§ 8.3) ────────────────────────────────────────────────────────────

/// What a click on a grant action authorizes, written for the outbound gate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub question_id: String,
    pub session_id: String,
    pub kind: String,
    pub target: String,
    pub payload_sha256: String,
    pub card_version: u32,
    pub clicked_at: u64,
}

/// The file name (without `.json`) of a grant: a `kova ask` question id (a
/// UUID) as is, or a pane view's `<view_id>:<card_id>` with `:` as `_`.
/// `None` for anything else: it names a file.
pub fn grant_file_stem(id: &str) -> Option<String> {
    match id.split_once(':') {
        None => crate::ask::is_safe_question_id(id).then(|| id.to_string()),
        Some((view, card)) => {
            (crate::ask::is_safe_question_id(view) && is_action_id(card)).then(|| format!("{}_{}", view, card))
        }
    }
}

/// Delete a grant not consumed yet (its card changed or left the view).
pub fn remove_grant(ask_root: &Path, id: &str) {
    if let Some(stem) = grant_file_stem(id) {
        let _ = std::fs::remove_file(grants_dir(ask_root).join(format!("{}.json", stem)));
    }
}

pub fn grants_dir(ask_root: &Path) -> PathBuf {
    ask_root.join("grants")
}

/// `<ask dir>/grants/<question id>.json`, directory 0700, file 0600. Atomic.
pub fn write_grant(ask_root: &Path, grant: &Grant) -> Result<PathBuf, String> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    let Some(stem) = grant_file_stem(&grant.question_id) else {
        return Err("bad question id".into());
    };
    let dir = grants_dir(ask_root);
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).map_err(|e| e.to_string())?;
    // An existing directory keeps its mode through `create`: enforce it.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    let data = serde_json::to_vec(grant).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!(".{}.tmp", stem));
    let path = dir.join(format!("{}.json", stem));
    let _ = std::fs::remove_file(&tmp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| e.to_string())?;
    file.write_all(&data).and_then(|_| file.sync_all()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(path)
}

/// Delete grants older than [`GRANT_TTL_SECS`] (by `clicked_at`, else the file's
/// age when it cannot be read). Returns how many went.
pub fn prune_grants(ask_root: &Path, now: u64) -> usize {
    let Ok(entries) = std::fs::read_dir(grants_dir(ask_root)) else { return 0 };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let clicked = std::fs::read(&path)
            .ok()
            .and_then(|d| serde_json::from_slice::<Grant>(&d).ok())
            .map(|g| g.clicked_at)
            .or_else(|| {
                entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
            })
            .unwrap_or(0);
        if now.saturating_sub(clicked) >= GRANT_TTL_SECS && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CLAUDE: AskerFacts = AskerFacts { session_id: Some("sess-1"), claude: true };

    fn base() -> Value {
        json!({
            "version": 1,
            "title": "Reply to Alex",
            "blocks": [{ "type": "text", "text": "Hello" }],
            "actions": [{ "id": "ok", "label": "OK" }]
        })
    }

    fn err(card: Value) -> String {
        validate(&card, None, &CLAUDE).unwrap_err()
    }

    fn with(mut card: Value, key: &str, value: Value) -> Value {
        card[key] = value;
        card
    }

    fn draft_card(payload: &str) -> Value {
        json!({
            "version": 1,
            "title": "Reply to Alex in #infra",
            "subtitle": "Slack drafts",
            "blocks": [
                { "type": "fields", "items": [{ "key": "Asked by", "value": "Alex" }] },
                { "type": "quote", "author": "Alex", "time": "18/11 16:02", "text": "Can you review?" },
                { "type": "code", "label": "Draft", "text": payload },
                { "type": "divider" },
                { "type": "context", "text": "Source: slack" }
            ],
            "actions": [
                { "id": "post", "label": "Post", "style": "primary",
                  "grant": { "kind": "outbound", "target": "slack:C01/179.1", "payload_sha256": sha256_hex(payload.as_bytes()), "payload_block": 2 } },
                { "id": "edit", "label": "Rewrite", "needs_input": true },
                { "id": "clear", "label": "Clear draft", "style": "destructive" }
            ],
            "input": { "placeholder": "What to change" }
        })
    }

    #[test]
    fn a_valid_card_round_trips_unchanged() {
        let raw = draft_card("Yes, I read it.\n");
        let card = validate(&raw, None, &CLAUDE).unwrap();
        assert_eq!(serde_json::to_value(&card).unwrap(), raw);
        let back: Card = serde_json::from_value(serde_json::to_value(&card).unwrap()).unwrap();
        assert_eq!(back, card);
        assert_eq!(card.legacy_actions(), vec!["edit", "clear"]);
        assert_eq!(card.primary().unwrap().id, "post");
    }

    #[test]
    fn every_error_of_section_9() {
        assert_eq!(err(json!({"version": 1, "title": "x".repeat(70_000)})), "card too large");
        assert_eq!(err(with(base(), "title", json!("a\rb"))), "control character in title");
        let mut c = base();
        c["blocks"][0]["text"] = json!("x\u{1b}y");
        assert_eq!(err(c), "control character in blocks[0].text");
        assert_eq!(validate(&base(), Some(&"m".repeat(1001)), &CLAUDE).unwrap_err(), "message too long");
        assert!(validate(&base(), Some(&"m".repeat(1000)), &CLAUDE).is_ok());
        let mut c = base();
        c.as_object_mut().unwrap().remove("version");
        assert_eq!(err(c), "missing version");
        assert_eq!(err(with(base(), "version", json!(2))), "unknown card version 2");
        assert_eq!(err(with(base(), "title", json!("t".repeat(121)))), "title too long");
        assert!(validate(&with(base(), "title", json!("é".repeat(120))), None, &CLAUDE).is_ok(), "chars, not bytes");
        assert_eq!(err(with(base(), "subtitle", json!("s".repeat(201)))), "subtitle too long");
        assert_eq!(err(with(base(), "blocks", json!([{"type": "image"}]))), "unknown block type image");
        assert_eq!(err(with(base(), "blocks", json!([{"type": "text", "text": "t".repeat(4001)}]))), "blocks[0].text too long");
        assert_eq!(
            err(with(base(), "blocks", json!([{"type": "divider"}, {"type": "fields", "items": [{"key": "k".repeat(41), "value": "v"}]}]))),
            "blocks[1].items[0].key too long"
        );
        assert_eq!(err(with(base(), "blocks", json!(vec![json!({"type": "divider"}); 21]))), "too many blocks");
        assert_eq!(err(with(base(), "actions", json!(vec![json!({"id": "a", "label": "A"}); 7]))), "too many actions");
        assert_eq!(
            err(with(base(), "actions", json!([{"id": "a", "label": "A"}, {"id": "a", "label": "B"}]))),
            "duplicate action id a"
        );
        assert_eq!(err(with(base(), "actions", json!([{"id": "A b", "label": "A"}]))), "bad action id A b");
        assert_eq!(
            err(with(base(), "actions", json!([{"id": "a", "label": "A", "style": "primary"}, {"id": "b", "label": "B", "style": "primary"}]))),
            "more than one primary action"
        );
        assert_eq!(err(with(base(), "actions", json!([{"id": "a", "label": "A", "needs_input": true}]))), "needs_input without input");
        assert_eq!(err(with(base(), "actions", json!([{"id": "a", "label": "L".repeat(25)}]))), "actions[0].label too long");
    }

    #[test]
    fn grant_errors() {
        let payload = "Draft body\n";
        let good = draft_card(payload);
        assert!(validate(&good, None, &CLAUDE).is_ok());

        let mut c = good.clone();
        c["actions"][2]["grant"] = c["actions"][0]["grant"].clone();
        assert_eq!(err(c), "more than one grant action");

        let mut c = good.clone();
        c["actions"][0]["grant"]["payload_block"] = json!(1);
        assert_eq!(err(c), "payload_block is not a code block");
        let mut c = good.clone();
        c["actions"][0]["grant"]["payload_block"] = json!(9);
        assert!(err(c).starts_with("payload_block out of range: it is a 0-based index"));
        let mut c = good.clone();
        c["blocks"][2]["text"] = json!("Draft body");
        assert_eq!(err(c), "payload_sha256 does not match block 2", "a trailing newline is part of the payload");
        let mut c = good.clone();
        c["actions"][0]["grant"]["payload_sha256"] = json!("ABC");
        assert_eq!(err(c), "payload_sha256 is not 64 hex chars");
        let mut c = good.clone();
        c["actions"][0]["grant"]["kind"] = json!("delete");
        assert_eq!(err(c), "unknown grant kind delete");
        let mut c = good.clone();
        c["actions"][0]["needs_input"] = json!(true);
        assert_eq!(err(c), "grant action cannot need input");
        let mut c = good.clone();
        c["actions"] = json!([c["actions"][0].clone()]);
        assert_eq!(err(c), "a card needs a non-grant action");

        let none = AskerFacts { session_id: None, claude: true };
        assert_eq!(validate(&good, None, &none).unwrap_err(), "grant requires an asker session id");
        let omp = AskerFacts { session_id: Some("s"), claude: false };
        assert_eq!(validate(&good, None, &omp).unwrap_err(), "grant requires a Claude Code asker");
    }

    #[test]
    fn the_payload_hash_is_taken_on_the_raw_bytes() {
        // CRLF is a control character: refused before the hash is even looked at.
        let c = draft_card("line\r\n");
        assert_eq!(err(c), "control character in blocks[2].text");
        let c = draft_card("  spaced  \n\n");
        assert!(validate(&c, None, &CLAUDE).is_ok(), "no trimming");
    }

    #[test]
    fn message_and_legacy_fields_from_the_card() {
        let c = draft_card("x");
        assert_eq!(derive_message(&c), "Reply to Alex in #infra");
        let mut c = base();
        c["blocks"] = json!([{"type": "divider"}, {"type": "text", "text": format!("line\nnext {}", "y".repeat(300))}]);
        let m = derive_message(&c);
        assert!(m.starts_with("Reply to Alex — line next yyy"));
        assert_eq!(m.chars().count(), "Reply to Alex — ".chars().count() + 200);
        assert_eq!(legacy_fields(&draft_card("x")), (vec!["edit".to_string(), "clear".to_string()], true));
        assert_eq!(legacy_fields(&base()), (vec!["ok".to_string()], false));
    }

    fn texts(spans: &[Span]) -> Vec<(&str, bool, bool, bool, Option<&str>)> {
        spans.iter().map(|s| (s.text.as_str(), s.bold, s.italic, s.code, s.link.as_deref())).collect()
    }

    #[test]
    fn markup_rules() {
        assert_eq!(texts(&parse_markup("a **b** c")), vec![("a ", false, false, false, None), ("b", true, false, false, None), (" c", false, false, false, None)]);
        assert_eq!(texts(&parse_markup("_it_.")), vec![("it", false, true, false, None), (".", false, false, false, None)]);
        assert_eq!(texts(&parse_markup("snake_case_name")), vec![("snake_case_name", false, false, false, None)]);
        assert_eq!(texts(&parse_markup("x `a**b**` y")), vec![("x ", false, false, false, None), ("a**b**", false, false, true, None), (" y", false, false, false, None)]);
        // Code inside bold: monospace and bold, the one exception to "no nesting".
        assert_eq!(texts(&parse_markup("**a `c` b**")), vec![("a ", true, false, false, None), ("c", true, false, true, None), (" b", true, false, false, None)]);
        // No nesting: markers inside bold print as is.
        assert_eq!(texts(&parse_markup("**a _b_ [l](https://x)**")), vec![("a _b_ [l](https://x)", true, false, false, None)]);
        assert_eq!(texts(&parse_markup("see [docs](https://x.y/a%28b%29) now")), vec![
            ("see ", false, false, false, None),
            ("docs", false, false, false, Some("https://x.y/a%28b%29")),
            (" now", false, false, false, None),
        ]);
        assert_eq!(texts(&parse_markup("[c](slack://channel?id=C1)"))[0].4, Some("slack://channel?id=C1"));
        assert_eq!(texts(&parse_markup("[x](http://evil)")), vec![("x (http://evil)", false, false, false, None)]);
        assert_eq!(texts(&parse_markup("[x](javascript:alert)")), vec![("x (javascript:alert)", false, false, false, None)]);
        // Label is plain: no markup inside.
        assert_eq!(texts(&parse_markup("[**b**](https://x)")), vec![("**b**", false, false, false, Some("https://x"))]);
        assert_eq!(texts(&parse_markup(r"\*\*a\*\* \_b\_ \` \[c\\")), vec![(r"**a** _b_ ` [c\", false, false, false, None)]);
        assert_eq!(texts(&parse_markup("**open and `tick")), vec![("**open and `tick", false, false, false, None)]);
        assert_eq!(texts(&parse_markup("(_a_)")), vec![("(", false, false, false, None), ("a", false, true, false, None), (")", false, false, false, None)]);
        assert_eq!(texts(&parse_markup("é_a_")), vec![("é_a_", false, false, false, None)], "non-ASCII is not punctuation");
    }

    #[test]
    fn new_blocks_validate() {
        let ok = |blocks: Value| validate(&with(base(), "blocks", blocks), None, &CLAUDE);
        assert!(ok(json!([
            {"type": "table", "columns": ["a", "b"], "rows": [["1", "**2**"], ["3", "4"]]},
            {"type": "bars", "items": [{"label": "x", "value": 0}, {"label": "y", "value": 3.5}], "unit": "h"},
            {"type": "sparkline", "values": [1, -2, 3.25], "label": "trend"},
            {"type": "progress", "value": 0.5, "label": "half"}
        ])).is_ok());
        let e = |blocks: Value| ok(blocks).unwrap_err();
        assert_eq!(e(json!([{"type": "table", "columns": ["a", "b"], "rows": [["1"]]}])), "blocks[0].rows[0] has the wrong number of cells");
        assert_eq!(e(json!([{"type": "table", "columns": [], "rows": []}])), "blocks[0].columns needs 1 to 8 columns");
        assert_eq!(e(json!([{"type": "table", "columns": ["a"], "rows": vec![json!(["x"]); 101]}])), "too many blocks[0].rows");
        assert_eq!(e(json!([{"type": "bars", "items": [{"label": "x", "value": -1}]}])), "blocks[0].items[0].value out of range");
        assert_eq!(e(json!([{"type": "sparkline", "values": [1]}])), "blocks[0].values needs 2 to 200 values");
        assert_eq!(e(json!([{"type": "sparkline", "values": [1, "x"]}])), "blocks[0].values[1] is not a finite number");
        assert_eq!(e(json!([{"type": "progress", "value": 1.5}])), "blocks[0].value out of range");
        assert_eq!(e(json!([{"type": "progress", "value": 0.1, "label": "l".repeat(81)}])), "blocks[0].label too long");
    }

    #[test]
    fn numbers_print_with_two_decimals_at_most() {
        assert_eq!(format_number(3.0), "3");
        assert_eq!(format_number(3.5), "3.5");
        assert_eq!(format_number(3.14159), "3.14");
        assert_eq!(format_number(12345.678), "12345.68");
        assert_eq!(format_number(-0.001), "0");
        assert_eq!(format_number(0.0), "0");
    }

    #[test]
    fn view_documents_validate() {
        let card = base();
        let view = |cards: Value| json!({"version": 1, "title": "Drafts", "cards": cards});
        let v = validate_view(&view(json!([{"id": "a", "card": card}, {"id": "b", "card": card}])), &CLAUDE).unwrap();
        assert_eq!(v.cards.len(), 2);
        assert!(validate_view(&view(json!([])), &CLAUDE).unwrap().cards.is_empty(), "0 cards is fine");
        let fifty: Vec<Value> = (0..50).map(|i| json!({"id": format!("c{}", i), "card": card})).collect();
        assert!(validate_view(&view(json!(fifty)), &CLAUDE).is_ok());
        let fifty_one: Vec<Value> = (0..51).map(|i| json!({"id": format!("c{}", i), "card": card})).collect();
        assert_eq!(validate_view(&view(json!(fifty_one)), &CLAUDE).unwrap_err(), "too many cards");
        assert_eq!(validate_view(&view(json!([{"id": "a", "card": card}, {"id": "a", "card": card}])), &CLAUDE).unwrap_err(), "duplicate card id a");
        let mut bad = card.clone();
        bad["version"] = json!(3);
        assert_eq!(validate_view(&view(json!([{"id": "a", "card": card}, {"id": "b", "card": bad}])), &CLAUDE).unwrap_err(), "cards[1].card: unknown card version 3");
        let big = json!({"version": 1, "title": "t", "cards": [], "x": "y".repeat(600_000)});
        assert_eq!(validate_view(&big, &CLAUDE).unwrap_err(), "view too large");
        assert_eq!(validate_view(&json!({"version": 2, "title": "t", "cards": []}), &CLAUDE).unwrap_err(), "unknown view version 2");
    }

    #[test]
    fn grant_file_is_private_and_pruned() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("kova-grant-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let grant = Grant {
            question_id: "0f8fad5b-d9cb-469f-a165-70867728950e".into(),
            session_id: "s1".into(),
            kind: "outbound".into(),
            target: "slack:C1/-".into(),
            payload_sha256: sha256_hex(b"x"),
            card_version: 3,
            clicked_at: 1_000_000,
        };
        let path = write_grant(&root, &grant).unwrap();
        assert_eq!(path, root.join("grants/0f8fad5b-d9cb-469f-a165-70867728950e.json"));
        assert_eq!(std::fs::metadata(root.join("grants")).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let back: Grant = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(back, grant);
        let json: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let keys: Vec<&str> = json.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys.len(), 7);
        let pane = Grant { question_id: format!("{}:draft-a", grant.question_id), ..grant.clone() };
        let pane_path = write_grant(&root, &pane).unwrap();
        assert_eq!(pane_path.file_name().unwrap().to_str().unwrap(), "0f8fad5b-d9cb-469f-a165-70867728950e_draft-a.json");
        remove_grant(&root, &pane.question_id);
        assert!(!pane_path.exists());
        assert!(write_grant(&root, &Grant { question_id: "../x:y".into(), ..grant.clone() }).is_err());
        assert!(write_grant(&root, &Grant { question_id: format!("{}:../y", grant.question_id), ..grant.clone() }).is_err());
        assert_eq!(prune_grants(&root, 1_000_000 + GRANT_TTL_SECS - 1), 0);
        assert!(path.exists());
        assert_eq!(prune_grants(&root, 1_000_000 + GRANT_TTL_SECS), 1);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
