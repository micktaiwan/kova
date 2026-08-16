//! Which rows of a pane hold a block meant to be copied out, not read.
//!
//! An answer often carries two things at once: a message to paste somewhere — a Slack
//! reply, a mail — and the remarks around it, addressed to the reader. Nothing in the
//! bytes tells them apart. Claude Code renders a fenced block by dropping the fences, and
//! its renderer only ever gives a block one of two things: the language printed dim above
//! it when highlight.js does not know that language, or colour inside the block when it
//! does. A sentence in French gets neither — no grammar has anything to say about it — so
//! what arrives here is prose, in the same colour as the prose around it.
//!
//! The dim language line is the one thing that does arrive. So a block meant to be pasted
//! is opened with a tag highlight.js cannot know, and closed with the same tag behind a
//! slash — an empty fence, whose only output is that second dim line:
//!
//! ```text
//! slack        <- dim, printed by Claude Code for ```slack
//! the message to paste,
//!
//! blank lines and all
//! /slack       <- printed for an empty ```/slack fence, and never drawn
//! ```
//!
//! The closing line is where the block ends, and it is [RowPaint::Hidden]: a delimiter is
//! how this file knows, not something the reader should have to see. What is left on
//! screen is the distinction that was asked for — the message in one colour, the remarks
//! around it in another — so a block may hold blank lines like any ordinary message.
//!
//! Claude Code does not always render the markdown, though, and the rule is exact: before
//! lexing anything it sniffs the text with
//! `/[#*`|[>\-_~]|\n\n|(?:^|\n) {0,3}\d+\. |https?:\/\/|www\./` — and, past 500 characters,
//! only the first 500. No match means the whole message is emitted as one plain paragraph,
//! fences and all. A message that opens with a paragraph of French prose longer than 500
//! characters therefore reaches the terminal unrendered (read off the binary and checked
//! against a live pane, 14/08/2026). So the four raw lines are read too, and taken away
//! rather than shown:
//!
//! ```text
//! ```slack
//! the message to paste
//! ```
//! ```/slack
//! ```
//! ```
//!
//! A bare fence outside a block is left strictly alone: on its own it is far more likely
//! to be a file someone printed than anything to do with this.
//!
//! Nothing is painted until the closing line has arrived. A tag left open would otherwise
//! colour whatever followed it, which is exactly the confusion this is here to remove.

use super::Cell;

/// The bullet Claude Code prints at the head of a message. A block that opens a message
/// therefore arrives as `⏺ slack` rather than `slack`: the bullet is drawn in the message's
/// own colour, belongs to the renderer and not to the tag, and must not disqualify the line.
/// Read off a live pane on 14/08/2026 — the closing line, further down the message, comes
/// indented instead (`  /slack`), which the blank-cell handling below already covers.
const BULLETS: [char; 2] = ['⏺', '●'];

/// The tags that open a block. Deliberately a short closed list: any dim word alone on its
/// line would otherwise start painting — build logs are full of them — and a language
/// highlight.js does know (`rust`, `sh`) never prints this line in the first place.
const TAGS: [&str; 5] = ["slack", "mail", "linkedin", "sms", "paste"];

/// What the renderer does with a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowPaint {
    /// Draw it as the terminal wrote it.
    Normal,
    /// Draw it in the paste colour, except where Claude coloured it itself.
    Body,
    /// Draw nothing: a delimiter that has done its job.
    Hidden,
}

/// The foreground SGR 2 leaves behind: `effective_colors` halves each component.
fn dim(fg: [u8; 3]) -> [u8; 3] {
    [fg[0] / 2, fg[1] / 2, fg[2] / 2]
}

/// What a line is, as far as this file is concerned.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Marker {
    /// `slack`, dim: the label Claude Code prints for a fence it could not highlight.
    DimOpen(&'static str),
    /// `/slack`, dim: same, for the empty fence that closes the block.
    DimClose(&'static str),
    /// ```` ```slack ````: the fence itself, left on screen unrendered.
    RawOpen(&'static str),
    /// ```` ```/slack ````, the raw form of the closing fence.
    RawClose(&'static str),
    /// A bare ```` ``` ````. Only meaningful once a raw block has been opened — anywhere
    /// else it is somebody's file being printed, and none of this file's business.
    Fence,
    Ordinary,
}

/// The text of a line, with the message bullet dropped, or `None` if some of it is written
/// in a colour of its own — which a tag line never is.
fn plain_text(line: &[Cell]) -> String {
    let mut text = String::new();
    for cell in line {
        if cell.is_blank() {
            text.push(' ');
        } else if BULLETS.contains(&cell.c) && text.trim().is_empty() {
            text.push(' ');
        } else {
            text.push(cell.c);
        }
    }
    text.trim().to_string()
}

/// Whether every character of the line that isn't blank or a bullet is dim.
fn all_dim(line: &[Cell], dim_fg: [u8; 3]) -> bool {
    let mut seen = false;
    for cell in line {
        if cell.is_blank() {
            continue;
        }
        if cell.fg != dim_fg {
            if !seen && BULLETS.contains(&cell.c) {
                continue;
            }
            return false;
        }
        seen = true;
    }
    seen
}

fn tag_of(name: &str) -> Option<&'static str> {
    TAGS.iter().find(|t| **t == name).copied()
}

fn classify(line: &[Cell], dim_fg: [u8; 3]) -> Marker {
    let text = plain_text(line);
    if text == "```" {
        return Marker::Fence;
    }
    if let Some(rest) = text.strip_prefix("```") {
        return match rest.strip_prefix('/') {
            Some(name) => tag_of(name).map_or(Marker::Ordinary, Marker::RawClose),
            None => tag_of(rest).map_or(Marker::Ordinary, Marker::RawOpen),
        };
    }
    if !all_dim(line, dim_fg) {
        return Marker::Ordinary;
    }
    match text.strip_prefix('/') {
        Some(name) => tag_of(name).map_or(Marker::Ordinary, Marker::DimClose),
        None => tag_of(&text).map_or(Marker::Ordinary, Marker::DimOpen),
    }
}

/// True when the line holds nothing but a tag, in either form.
///
/// A copy is the point of the whole thing, so a selection that swept over one of these
/// lines must not paste the word `slack` into the message it labelled. A bare ```` ``` ````
/// is left alone: out of context it is as likely to be someone's file on screen.
pub fn is_marker_line(line: &[Cell], default_fg: [u8; 3]) -> bool {
    !matches!(
        classify(line, dim(default_fg)),
        Marker::Ordinary | Marker::Fence
    )
}

/// One verdict per line, in the order the rows are drawn.
///
/// The dim opening label stays [RowPaint::Normal]: Claude Code already prints it in grey,
/// and grey above a coloured block reads as its label. Everything else that delimits — the
/// dim closing line, and all four lines of the unrendered form — is taken away.
pub fn paste_block_rows(lines: &[&[Cell]], default_fg: [u8; 3]) -> Vec<RowPaint> {
    let dim_fg = dim(default_fg);
    let mut paint = vec![RowPaint::Normal; lines.len()];

    /// Where the scan is between two tag lines.
    enum State {
        Idle,
        /// Inside a block, holding the row its label was on and the tag to match.
        Open(usize, &'static str),
        /// The body is closed; the lines that close the raw form may still follow.
        Closing(&'static str),
        /// One last ```` ``` ```` belongs to the block rather than to the message.
        Trailing,
    }
    let mut state = State::Idle;
    let fill = |paint: &mut Vec<RowPaint>, from: usize, to: usize| {
        for row in &mut paint[from..to] {
            *row = RowPaint::Body;
        }
    };

    for (row, line) in lines.iter().enumerate() {
        let marker = classify(line, dim_fg);
        state = match (state, marker) {
            // A block opens. The raw fence is noise, the dim label is a label.
            (_, Marker::DimOpen(tag)) => State::Open(row, tag),
            (_, Marker::RawOpen(tag)) => {
                paint[row] = RowPaint::Hidden;
                State::Open(row, tag)
            }
            // It closes. Painting happens here, never before: a block left open would
            // otherwise colour whatever followed it.
            (State::Open(start, tag), Marker::DimClose(closed)) if closed == tag => {
                fill(&mut paint, start + 1, row);
                paint[row] = RowPaint::Hidden;
                State::Idle
            }
            (State::Open(start, tag), Marker::Fence) => {
                fill(&mut paint, start + 1, row);
                paint[row] = RowPaint::Hidden;
                State::Closing(tag)
            }
            (State::Open(start, tag), Marker::RawClose(closed)) if closed == tag => {
                fill(&mut paint, start + 1, row);
                paint[row] = RowPaint::Hidden;
                State::Trailing
            }
            (State::Closing(tag), Marker::RawClose(closed)) if closed == tag => {
                paint[row] = RowPaint::Hidden;
                State::Trailing
            }
            (State::Trailing, Marker::Fence) => {
                paint[row] = RowPaint::Hidden;
                State::Idle
            }
            // A closing line with no opening in view: the block started above the window,
            // and a block is one unbroken run, so everything above it belongs to it.
            (State::Idle, Marker::DimClose(_)) | (State::Idle, Marker::RawClose(_)) => {
                fill(&mut paint, 0, row);
                paint[row] = RowPaint::Hidden;
                State::Idle
            }
            (State::Open(start, tag), _) => State::Open(start, tag),
            (_, _) => State::Idle,
        };
    }

    paint
}

#[cfg(test)]
mod tests {
    use super::RowPaint::{Body, Hidden, Normal};
    use super::*;
    use crate::terminal::{CellAttrs, DEFAULT_FG};

    fn line(text: &str, fg: [u8; 3]) -> Vec<Cell> {
        text.chars()
            .map(|c| Cell {
                c,
                cluster: None,
                fg,
                bg: [0, 0, 0],
                hyperlink_id: 0,
                attrs: CellAttrs::empty(),
            })
            .collect()
    }

    fn dimmed(text: &str) -> Vec<Cell> {
        line(text, dim(DEFAULT_FG))
    }

    fn plain(text: &str) -> Vec<Cell> {
        line(text, DEFAULT_FG)
    }

    fn rows(lines: &[Vec<Cell>]) -> Vec<RowPaint> {
        let refs: Vec<&[Cell]> = lines.iter().map(|l| l.as_slice()).collect();
        paste_block_rows(&refs, DEFAULT_FG)
    }

    #[test]
    fn the_body_is_painted_the_label_stays_and_the_closing_line_goes() {
        let paint = rows(&[
            plain("here it is:"),
            dimmed("slack"),
            plain("Salut, je regarde ça"),
            dimmed("/slack"),
            plain("et une remarque"),
        ]);
        assert_eq!(paint, vec![Normal, Normal, Body, Hidden, Normal]);
    }

    #[test]
    fn a_blank_line_inside_a_block_is_part_of_it() {
        // What broke the previous reading: a message of two paragraphs is still one
        // message, and its empty line must not end the colour.
        let paint = rows(&[
            dimmed("mail"),
            plain("Bonjour,"),
            plain(""),
            plain("à demain."),
            dimmed("/mail"),
        ]);
        assert_eq!(paint, vec![Normal, Body, Body, Body, Hidden]);
    }

    #[test]
    fn a_tag_left_open_paints_nothing() {
        // The block is still streaming, or I forgot to close it. Either way, colouring
        // what follows would be the very confusion this exists to remove.
        let paint = rows(&[dimmed("slack"), plain("a"), plain("b")]);
        assert_eq!(paint, vec![Normal, Normal, Normal]);
    }

    #[test]
    fn a_closing_line_alone_paints_everything_above_it() {
        // A block taller than the window, scrolled so its opening line is off the top.
        let paint = rows(&[plain("a"), plain("b"), dimmed("/slack"), plain("after")]);
        assert_eq!(paint, vec![Body, Body, Hidden, Normal]);
    }

    #[test]
    fn a_block_opening_a_message_carries_the_bullet_and_still_counts() {
        // Exactly what a live pane held on 14/08/2026: "⏺ slack" to open, "  /slack" to
        // close, the whole message body indented by two columns.
        let mut opening = plain("⏺ ");
        opening.extend(dimmed("slack"));
        let mut closing = plain("  ");
        closing.extend(dimmed("/slack"));
        let paint = rows(&[opening, plain("  Salut,"), closing]);
        assert_eq!(paint, vec![Normal, Body, Hidden]);
    }

    #[test]
    fn a_bullet_after_the_tag_is_not_a_marker() {
        let mut line = dimmed("slack");
        line.extend(plain("⏺"));
        let paint = rows(&[line, plain("a"), dimmed("/slack")]);
        assert_eq!(paint, vec![Body, Body, Hidden]);
    }

    #[test]
    fn the_unrendered_form_is_painted_and_its_four_fences_are_taken_away() {
        // Copied off a live pane on 14/08/2026, indentation included.
        let paint = rows(&[
            plain("  ```slack"),
            plain("  the reason I ask: telling our users it's AI"),
            plain("  ```"),
            plain("  ```/slack"),
            plain("  ```"),
            plain("  Sources : ..."),
        ]);
        assert_eq!(
            paint,
            vec![Hidden, Body, Hidden, Hidden, Hidden, Normal]
        );
    }

    #[test]
    fn a_bare_fence_outside_a_block_is_left_alone() {
        // Someone printing a markdown file must not have lines disappear from under them.
        let paint = rows(&[plain("```"), plain("du code"), plain("```")]);
        assert_eq!(paint, vec![Normal, Normal, Normal]);
    }

    #[test]
    fn an_unrendered_block_left_open_paints_nothing() {
        let paint = rows(&[plain("```slack"), plain("a"), plain("b")]);
        assert_eq!(paint, vec![Hidden, Normal, Normal]);
    }

    #[test]
    fn a_fence_naming_a_language_is_not_a_tag() {
        let paint = rows(&[plain("```rust"), plain("fn main() {}"), plain("```")]);
        assert_eq!(paint, vec![Normal, Normal, Normal]);
    }

    #[test]
    fn a_tag_in_ordinary_colour_is_just_a_word() {
        let paint = rows(&[plain("slack"), plain("a"), plain("/slack")]);
        assert_eq!(paint, vec![Normal, Normal, Normal]);
    }

    #[test]
    fn an_unknown_dim_word_starts_nothing() {
        let paint = rows(&[dimmed("thinking"), plain("a"), dimmed("/thinking")]);
        assert_eq!(paint, vec![Normal, Normal, Normal]);
    }

    #[test]
    fn a_dim_line_carrying_more_than_the_tag_is_not_a_marker() {
        let paint = rows(&[dimmed("slack now"), plain("a"), dimmed("/slack")]);
        assert_eq!(paint, vec![Body, Body, Hidden]);
    }

    #[test]
    fn two_blocks_in_view_are_each_painted() {
        let paint = rows(&[
            dimmed("slack"),
            plain("one"),
            dimmed("/slack"),
            plain("between"),
            dimmed("mail"),
            plain("two"),
            dimmed("/mail"),
        ]);
        assert_eq!(
            paint,
            vec![Normal, Body, Hidden, Normal, Normal, Body, Hidden]
        );
    }

    #[test]
    fn a_closing_tag_that_does_not_match_its_opening_paints_nothing() {
        let paint = rows(&[dimmed("slack"), plain("one"), dimmed("/mail")]);
        assert_eq!(paint, vec![Normal, Normal, Normal]);
    }

    #[test]
    fn both_tag_lines_are_recognised_and_nothing_else_is() {
        assert!(is_marker_line(&dimmed("slack"), DEFAULT_FG));
        assert!(is_marker_line(&dimmed("/slack"), DEFAULT_FG));
        assert!(!is_marker_line(&plain("slack"), DEFAULT_FG));
        assert!(!is_marker_line(&dimmed("Salut, je regarde ça"), DEFAULT_FG));
    }

    #[test]
    fn trailing_blanks_do_not_disqualify_a_tag() {
        let mut tag = dimmed("slack");
        tag.extend(plain("   "));
        let mut closing = dimmed("/slack");
        closing.extend(plain("  "));
        let paint = rows(&[tag, plain("one"), closing]);
        assert_eq!(paint, vec![Normal, Body, Hidden]);
    }
}
