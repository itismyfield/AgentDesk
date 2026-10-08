//! Reads the bottom of a Codex TUI capture: the composer above the status row, the queue Codex
//! shows for inputs it holds until the turn ends, and the screens that hide the composer.

use crate::services::codex_tui::input::{
    pane_shows_codex_interactive_modal, strip_ansi_escape_sequences,
};

/// Words that only Codex's status row carries below the composer.
const STATUS_MARKERS: &[&str] = &["context left", "% left", "tab to queue message"];
/// Footers of the transcript overlay and the review pickers, which hide the composer.
const MODAL_FOOTERS: &[&str] = &[
    "q close",
    "esc browse prompts",
    "enter select · esc back",
    "enter submit · esc back",
];
/// Headers Codex puts over inputs it holds until the running turn ends.
const QUEUE_HEADERS: &[&str] = &[
    "• Queued follow-up inputs",
    "• Messages to be submitted at end of turn",
];
/// More composer rows than this are not a composer the size check let through.
const MAX_COMPOSER_ROWS: usize = 40;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Composer {
    /// Only the dimmed placeholder, or nothing, after the prompt mark.
    Empty,
    /// The composer's text, rows joined by newlines.
    Text(String),
    /// No prompt row whose layout reads as Codex's composer.
    Unread,
}

pub(super) struct Screen {
    /// A Codex modal or a screen that hides the composer.
    pub modal: bool,
    /// The bottom row is an overlay or picker footer.
    pub overlay: bool,
    pub queued: bool,
    pub composer: Composer,
}

pub(super) fn read(capture: &str) -> Screen {
    let raw: Vec<&str> = capture.lines().collect();
    let plain: Vec<String> = raw
        .iter()
        .map(|line| strip_ansi_escape_sequences(line))
        .collect();
    let footer = plain.iter().rev().find(|line| !line.trim().is_empty());
    let overlay = footer.is_some_and(|line| MODAL_FOOTERS.iter().any(|mark| line.contains(mark)));
    let modal = overlay || pane_shows_codex_interactive_modal(&plain.join("\n"));
    let queued = plain
        .iter()
        .any(|line| QUEUE_HEADERS.contains(&line.trim()));
    Screen {
        modal,
        overlay,
        queued,
        composer: composer(&raw, &plain),
    }
}

/// Whether the composer holds exactly `text`, line breaks and wrapping aside. Modal words in the
/// pasted text itself must not fail the check, so only the footers count here.
pub(super) fn owns(capture: &str, text: &str) -> bool {
    let shown = read(capture);
    let squeeze = |value: &str| -> String { value.split_whitespace().collect() };
    !shown.overlay
        && !shown.queued
        && matches!(shown.composer, Composer::Text(held) if squeeze(&held) == squeeze(text))
}

fn composer(raw: &[&str], plain: &[String]) -> Composer {
    let blank = |at: usize| plain[at].trim().is_empty();
    let Some(status) = (0..plain.len()).rev().find(|at| !blank(*at)) else {
        return Composer::Unread;
    };
    if !STATUS_MARKERS
        .iter()
        .any(|mark| plain[status].contains(mark))
    {
        return Composer::Unread;
    }
    // A blank row parts the composer from the status row.
    let Some(last) = status.checked_sub(2).filter(|_| blank(status - 1)) else {
        return Composer::Unread;
    };
    // Continuation rows are indented two columns; the first row carries the prompt mark.
    let mut top = last;
    loop {
        let row = &plain[top];
        if row.starts_with('›') {
            break;
        }
        if !row.starts_with("  ") || row.trim().is_empty() || last - top >= MAX_COMPOSER_ROWS {
            return Composer::Unread;
        }
        let Some(above) = top.checked_sub(1) else {
            return Composer::Unread;
        };
        top = above;
    }
    // A row cut off at the top of the capture may hide the composer's first line.
    if top == 0 || !blank(top - 1) {
        return Composer::Unread;
    }
    let first = plain[top].trim_start_matches('›');
    let first = first.strip_prefix(' ').unwrap_or(first);
    if top == last && (first.trim().is_empty() || only_dim_after_mark(raw[top])) {
        return Composer::Empty;
    }
    let rest = plain[top + 1..=last]
        .iter()
        .map(|row| row.strip_prefix("  ").unwrap_or(row));
    let rows: Vec<&str> = std::iter::once(first).chain(rest).collect();
    Composer::Text(rows.join("\n"))
}

/// Whether every visible character after the prompt mark is drawn dim, as Codex draws its
/// placeholder; typed or pasted text is drawn plain.
fn only_dim_after_mark(row: &str) -> bool {
    let Some(mark) = row.find('›') else {
        return false;
    };
    let mut dim = false;
    let mut chars = row[mark + '›'.len_utf8()..].chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.next() != Some('[') {
                return false;
            }
            let mut params = String::new();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    if c == 'm' {
                        dim = sgr_dim(&params, dim);
                    }
                    break;
                }
                params.push(c);
            }
        } else if !c.is_whitespace() && !dim {
            return false;
        }
    }
    true
}

/// The dim state after one SGR sequence: 2 sets it, 0, 22 or an empty list clear it. The
/// arguments of an extended colour (38, 48, 58) are skipped, so their 2 never reads as dim.
fn sgr_dim(params: &str, mut dim: bool) -> bool {
    let mut params = params.split(';');
    while let Some(param) = params.next() {
        match param {
            "" | "0" | "22" => dim = false,
            "2" => dim = true,
            "38" | "48" | "58" => {
                let skip = match params.next() {
                    Some("5") => 1,
                    Some("2") => 3,
                    _ => 0,
                };
                for _ in 0..skip {
                    params.next();
                }
            }
            _ => {}
        }
    }
    dim
}
