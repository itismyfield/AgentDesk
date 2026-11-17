//! Reads the bottom of a Codex TUI capture: the composer above the status row, the queue Codex
//! shows for inputs it holds until the turn ends, and the screens that hide the composer.

use crate::services::codex_tui::input::{
    pane_shows_codex_interactive_modal, strip_ansi_escape_sequences,
};

/// Words that only Codex's status row carries below the composer.
const STATUS_MARKERS: &[&str] = &[
    "context left",
    "% left",
    "tab to queue message",
    " · ← for agents",
];
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
    "Messages to be submitted after next tool call",
    "• Messages to be submitted after next tool call",
];
/// Bound the pre-paste composer scan; an unread layout falls back before mutation.
const MAX_COMPOSER_ROWS: usize = 40;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Composer {
    /// Only the dimmed placeholder, or nothing, after the prompt mark.
    Empty,
    /// The composer's text, rows joined by newlines.
    Text(String),
    /// No prompt row whose layout reads as Codex's composer.
    Unread,
}

pub(crate) struct Screen {
    /// A Codex modal or a screen that hides the composer.
    pub modal: bool,
    pub queued: bool,
    pub composer: Composer,
    /// Image rows above the textarea, which an Enter submits along with the text.
    pub attachments: bool,
}

pub(crate) fn read(capture: &str) -> Screen {
    let raw: Vec<&str> = capture.lines().collect();
    let plain: Vec<String> = raw
        .iter()
        .map(|line| strip_ansi_escape_sequences(line))
        .collect();
    let footer = plain.iter().rev().find(|line| !line.trim().is_empty());
    let overlay = footer.is_some_and(|line| MODAL_FOOTERS.iter().any(|mark| line.contains(mark)));
    let modal = overlay || pane_shows_codex_interactive_modal(&plain.join("\n"));
    let (composer, attachments) = composer(&raw, &plain);
    let queued = plain.iter().any(|line| {
        QUEUE_HEADERS
            .iter()
            .any(|header| line.trim().starts_with(header))
    });
    Screen {
        modal,
        queued,
        composer,
        attachments,
    }
}

/// Owned delivery ignores editor contents, but Enter must still mean prompt submission.
pub(crate) fn delivery_veto(capture: &str) -> Option<super::Veto> {
    let plain = strip_ansi_escape_sequences(capture);
    let footer = plain.lines().rev().find(|row| !row.trim().is_empty());
    (crate::services::codex_tui::input::submission_modal_in_pane(&plain)
        || crate::services::codex_tui::input::pane_has_blocking_action_menu(&plain)
        || completion_popup(&plain)
        || footer.is_some_and(|row| MODAL_FOOTERS.iter().any(|hint| row.contains(hint))))
    .then_some(super::Veto::Modal)
}

/// Completion choices occupy their own block above the current composer, outside its draft.
fn completion_popup(plain: &str) -> bool {
    let rows: Vec<_> = plain.lines().collect();
    let Some(composer) = rows.iter().rposition(|row| row.starts_with('›')) else {
        return false;
    };
    rows[..composer]
        .iter()
        .rev()
        .skip_while(|row| row.trim().is_empty())
        .take_while(|row| !row.trim().is_empty())
        .any(|row| {
            row.starts_with("› /") || row.trim_start().starts_with("enter/tab insert · esc close")
        })
}

/// A notice needs visible text evidence; an unread screen must not invent a person's draft.
pub(crate) fn draft_may_be_present(capture: &str) -> bool {
    use crate::services::codex_tui::input::submission_draft_in_pane;

    let body = submission_draft_in_pane(capture).or_else(|| match read(capture).composer {
        Composer::Empty => Some(String::new()),
        Composer::Text(body) => Some(body),
        Composer::Unread => None,
    });
    let (draft_branch, notify) = match body {
        Some(body) if body.trim().is_empty() => ("empty", false),
        Some(body) if current_body_is_dim(capture, &body) => ("dim_placeholder", false),
        Some(_) => ("readable_text", true),
        None => ("unread", false),
    };
    if notify || draft_branch == "unread" {
        tracing::info!(draft_branch, notify, "Codex draft notice decision");
    } else {
        tracing::debug!(draft_branch, notify, "Codex draft notice decision");
    }
    notify
}

/// Keep SGR state across wrapped suggestion rows, without treating color channels as dim codes.
fn current_body_is_dim(capture: &str, body: &str) -> bool {
    let raw: Vec<_> = capture.lines().collect();
    let plain: Vec<_> = raw
        .iter()
        .map(|line| strip_ansi_escape_sequences(line))
        .collect();
    let Some(start) = plain.iter().rposition(|line| line.starts_with('›')) else {
        return false;
    };
    let end = start + body.split('\n').count();
    let Some(rows) = plain.get(start..end) else {
        return false;
    };
    // A historical prompt above a boxed composer is not the current body's style evidence.
    if plain[end..]
        .iter()
        .filter(|line| !line.trim().is_empty())
        .count()
        > 1
    {
        return false;
    }
    let first = rows[0].strip_prefix('›').unwrap_or(&rows[0]);
    let observed = std::iter::once(first.strip_prefix(' ').unwrap_or(first))
        .chain(
            rows[1..]
                .iter()
                .map(|row| row.strip_prefix("  ").unwrap_or(row)),
        )
        .collect::<Vec<_>>()
        .join("\n");
    if observed != body {
        return false;
    }
    let styled = raw[start..end].join("\n");
    let (mut dim, mut marker, mut visible) = (false, false, false);
    let mut chars = styled.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.next() != Some('[') {
                return false;
            }
            let mut params = String::new();
            loop {
                match chars.next() {
                    Some('m') => {
                        dim = sgr_dim(&params, dim);
                        break;
                    }
                    Some(c) if c.is_ascii_digit() || c == ';' => params.push(c),
                    _ => return false,
                }
            }
        } else if !marker {
            if c == '›' {
                marker = true;
            } else if !c.is_whitespace() {
                return false;
            }
        } else if !c.is_whitespace() {
            if !dim {
                return false;
            }
            visible = true;
        }
    }
    marker && visible
}

/// The composer's text and whether image rows sit above it.
fn composer(raw: &[&str], plain: &[String]) -> (Composer, bool) {
    let blank = |at: usize| plain[at].trim().is_empty();
    let Some(status) = (0..plain.len()).rev().find(|at| !blank(*at)) else {
        return (Composer::Unread, false);
    };
    if !STATUS_MARKERS
        .iter()
        .any(|mark| plain[status].contains(mark))
    {
        return (Composer::Unread, false);
    }
    // A blank row parts the composer from the status row.
    let Some(last) = status.checked_sub(2).filter(|_| blank(status - 1)) else {
        return (Composer::Unread, false);
    };
    // Continuation rows are indented two columns; the first row carries the prompt mark.
    let mut top = last;
    loop {
        let row = &plain[top];
        if row.starts_with('›') {
            break;
        }
        if !row.starts_with("  ") || row.trim().is_empty() || last - top >= MAX_COMPOSER_ROWS {
            return (Composer::Unread, false);
        }
        let Some(above) = top.checked_sub(1) else {
            return (Composer::Unread, false);
        };
        top = above;
    }
    // A row cut off at the top of the capture may hide the composer's first line.
    if top == 0 || !blank(top - 1) {
        return (Composer::Unread, false);
    }
    // Remote images take rows of their own two columns in, a blank row above the prompt, under
    // the composer's blank top row; a row naming an image in any other shape is not read.
    let mut above = top - 1;
    while above > 0 && image_row(&plain[above - 1]) {
        above -= 1;
    }
    let attachments = above < top - 1;
    let unshaped = above > 0 && !attachments && plain[above - 1].contains("[Image #");
    if (attachments && (above == 0 || !blank(above - 1))) || unshaped {
        return (Composer::Unread, false);
    }
    let first = plain[top].trim_start_matches('›');
    let first = first.strip_prefix(' ').unwrap_or(first);
    if top == last
        && (first.is_empty() || (!first.trim().is_empty() && only_dim_after_mark(raw[top])))
    {
        return (Composer::Empty, attachments);
    }
    let rest = plain[top + 1..=last]
        .iter()
        .map(|row| row.strip_prefix("  ").unwrap_or(row));
    let rows: Vec<&str> = std::iter::once(first).chain(rest).collect();
    (Composer::Text(rows.join("\n")), attachments)
}

/// A remote image row as Codex draws it: `[Image #N]` alone, two columns in.
fn image_row(row: &str) -> bool {
    let label = row
        .strip_prefix("  ")
        .map(str::trim_end)
        .unwrap_or_default();
    let number = label
        .strip_prefix("[Image #")
        .and_then(|rest| rest.strip_suffix(']'));
    number.is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
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

/// Unknown editor semantics never turn a displayed draft into a proven append position.
pub(crate) fn append_veto(capture: &str, shown: &Screen) -> Option<super::Veto> {
    use super::Veto;
    let plain = strip_ansi_escape_sequences(capture);
    let rows: Vec<_> = plain.lines().collect();
    let top = rows.iter().rposition(|row| row.starts_with('›'));
    let draft = top.map(|top| rows[top..].join("\n")).unwrap_or_default();
    if draft.trim_start_matches('›').trim_start().starts_with('/') {
        return Some(Veto::DraftCommandUnproven);
    }
    if draft
        .lines()
        .any(|row| row.trim_start_matches('›').trim_start().starts_with('>'))
    {
        return Some(Veto::DraftPasteTransformUnproven);
    }
    if top.is_some_and(|top| {
        rows[top..]
            .iter()
            .take_while(|row| !row.trim().is_empty())
            .count()
            > MAX_COMPOSER_ROWS
    }) {
        return Some(Veto::DraftNotFullyVisible);
    }
    if draft.contains("[Pasted") || draft.contains("<pasted_content") {
        return Some(Veto::DraftPlaceholderUnproven);
    }
    // Selection absence and cursor coordinates alone do not establish default editor semantics.
    if capture
        .lines()
        .skip(top.unwrap_or(0))
        .any(|row| row.contains("[7m") || row.contains(";7m"))
    {
        return Some(Veto::DraftSelectionUnproven);
    }
    match &shown.composer {
        Composer::Unread => Some(Veto::ComposerUnread),
        Composer::Empty if shown.attachments => Some(Veto::DraftImagePositionUnproven),
        Composer::Text(_) => Some(Veto::DraftNavigationUnproven),
        Composer::Empty => None,
    }
}

#[cfg(test)]
#[path = "draft_notice_tests.rs"]
mod draft_notice_tests;
