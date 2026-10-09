//! Reads the active Claude composer and the status row right above its top border from one
//! `capture-pane -e` capture. Anything outside the recognised layout reads as Unknown.

use unicode_width::UnicodeWidthChar;

use crate::services::tui_input::actor::gate::{claude_border_row, claude_prompt_row};

/// Draft chips whose payload is not measured to survive a stash round trip.
const UNMEASURED_CHIPS: [&str; 3] = [
    "[Image #",
    "[...Truncated text #",
    "\u{2726} Team setup guide #",
];
const STASHED: &str = "\u{203a} stashed";
const MIN_BORDER: usize = 10;
/// The footer measured under the box with `--dangerously-skip-permissions`.
const FOOTER: &str = "  \u{23f5}\u{23f5} bypass permissions on";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Composer {
    Empty,
    /// Rows as typed: the prompt and the two-column continuation indent removed.
    Text(Vec<String>),
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stash {
    Present,
    AbsentInRecognizedLayout,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Screen {
    pub(super) composer: Composer,
    pub(super) stash: Stash,
}

const UNKNOWN: Screen = Screen {
    composer: Composer::Unknown,
    stash: Stash::Unknown,
};

/// One captured row: all visible text, the part not drawn faint, and the row as captured.
struct Row {
    plain: String,
    solid: String,
    raw: String,
}

fn row(raw: &str) -> Row {
    let (mut plain, mut solid, mut faint) = (String::new(), String::new(), false);
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            plain.push(c);
            if !faint {
                solid.push(c);
            }
            continue;
        }
        match chars.next() {
            Some('[') => {
                let mut params = String::new();
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        if c == 'm' {
                            faint = sgr_faint(&params, faint);
                        }
                        break;
                    }
                    params.push(c);
                }
            }
            // OSC runs to BEL or ST.
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\x07' || (c == '\x1b' && chars.next().is_some()) {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    let raw = raw.to_string();
    Row { plain, solid, raw }
}

/// SGR 2 turns faint on; 0, 22 and an empty list turn it off. Colour arguments are skipped.
fn sgr_faint(params: &str, mut faint: bool) -> bool {
    let mut parts = params.split(';');
    while let Some(part) = parts.next() {
        match part {
            "" | "0" | "22" => faint = false,
            "2" => faint = true,
            "38" | "48" | "58" => match parts.next() {
                Some("5") => {
                    parts.next();
                }
                Some("2") => {
                    parts.nth(2);
                }
                _ => {}
            },
            _ => {}
        }
    }
    faint
}

/// The bottom-most `❯` row between two equal full borders, with only a measured footer below.
pub(super) fn read(capture: &str) -> Screen {
    let rows: Vec<Row> = capture.lines().map(row).collect();
    let border = |index: usize| -> Option<usize> {
        let text = rows.get(index)?.plain.trim_end();
        let width = text.chars().count();
        (width >= MIN_BORDER && text.chars().all(|c| c == '─')).then_some(width)
    };
    let Some(prompt) = rows.iter().rposition(|row| row.plain.starts_with('❯')) else {
        return UNKNOWN;
    };
    let (Some(top), Some(status)) = (prompt.checked_sub(1), prompt.checked_sub(2)) else {
        return UNKNOWN;
    };
    let Some(width) = border(top) else {
        return UNKNOWN;
    };
    let Some(bottom) = (prompt + 1..rows.len()).find(|&index| border(index).is_some()) else {
        return UNKNOWN;
    };
    // A box that closes at another width, or whose footer is cut, unmeasured or followed by more
    // rows (another box, a `!` bash-mode prompt), is stale or not a layout read here.
    let mut below = rows[bottom + 1..].iter().map(|row| row.plain.trim_end());
    if border(bottom) != Some(width)
        || !below.next().is_some_and(footer)
        || below.any(|row| !row.is_empty())
    {
        return UNKNOWN;
    }
    Screen {
        composer: composer(&rows[prompt..bottom]),
        stash: stash(&rows[status].plain, width),
    }
}

/// The bypass-mode footer, its cycle hint and the busy hint; a footer cut at the edge is not.
fn footer(row: &str) -> bool {
    let Some(rest) = row.strip_prefix(FOOTER) else {
        return false;
    };
    let rest = rest.strip_prefix(" (shift+tab to cycle)").unwrap_or(rest);
    matches!(rest, "" | " \u{b7} esc to interrupt")
}

fn composer(rows: &[Row]) -> Composer {
    let line = |index: usize, text: &str| -> Option<String> {
        if index == 0 {
            let rest = text.strip_prefix('❯')?;
            if rest.is_empty() {
                return Some(String::new());
            }
            return rest.strip_prefix(['\u{00a0}', ' ']).map(str::to_string);
        }
        if text.is_empty() {
            return Some(String::new());
        }
        text.strip_prefix("  ").map(str::to_string)
    };
    let mut lines = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        let (Some(plain), Some(solid)) = (line(index, &row.plain), line(index, &row.solid)) else {
            return Composer::Unknown;
        };
        // Only the measured placeholder shape proves an empty composer; other faint rows stay unread.
        if plain != solid {
            let blank = rows.len() == 1 && solid.trim().is_empty();
            return if blank && measured_placeholder(&row.raw) {
                Composer::Empty
            } else {
                Composer::Unknown
            };
        }
        // An attachment chip stands for a payload the rows do not show.
        if UNMEASURED_CHIPS.iter().any(|chip| plain.contains(chip)) {
            return Composer::Unknown;
        }
        lines.push(plain);
    }
    match (lines.len(), lines.iter().all(|line| line.trim().is_empty())) {
        (1, true) => Composer::Empty,
        (_, true) => Composer::Unknown,
        _ => Composer::Text(lines),
    }
}

/// The idle placeholder as Claude Code 2.1.292 draws it: the prompt and a no-break space, then one
/// faint run of plain text closed by a full reset at the row's end. The words are never checked.
fn measured_placeholder(raw: &str) -> bool {
    let Some((_, rest)) = raw.split_once("\u{276f}\u{00a0}") else {
        return false;
    };
    let run = rest.trim_end().strip_prefix("\x1b[2m");
    let text = run.and_then(|run| run.strip_suffix("\x1b[0m"));
    text.is_some_and(|text| !text.trim().is_empty() && !text.contains('\x1b'))
}

/// Only the row right above the top border counts; scrollback never does.
fn stash(status: &str, width: usize) -> Stash {
    let text = status.trim();
    if text == STASHED || text.ends_with(&format!(" \u{b7} {STASHED}")) {
        return Stash::Present;
    }
    if text.contains('\u{203a}') || text.contains("stash") {
        return Stash::Unknown;
    }
    if text.is_empty() {
        return Stash::AbsentInRecognizedLayout;
    }
    // A right-aligned row drawn whole: padding on the left, its last cell two columns from the edge.
    let drawn = status.trim_end();
    let whole = drawn.starts_with(' ')
        && drawn.chars().all(|c| c.is_ascii() || c == '\u{b7}')
        && drawn.chars().count() + 2 == width;
    if whole {
        Stash::AbsentInRecognizedLayout
    } else {
        Stash::Unknown
    }
}

/// The person's draft rows when the composer holds plain text, Claude shows no stash and the
/// frame will render flat; anything else leaves the draft untouched.
pub(super) fn stashable(
    capture: &str,
    frame: &str,
    size: Option<(usize, usize)>,
) -> Option<Vec<String>> {
    let (width, height) = size?;
    if !renders_flat(frame, width, height) {
        return None;
    }
    let screen = read(capture);
    let Composer::Text(rows) = screen.composer else {
        return None;
    };
    (screen.stash == Stash::AbsentInRecognizedLayout).then_some(rows)
}

/// Claude folds a paste over 800 UTF-16 units or with more newlines than clamp(rows-10, 0, 2),
/// and wraps rows wider than width-4. Wide characters count two columns; tabs and CR get rewritten.
fn renders_flat(frame: &str, width: usize, height: usize) -> bool {
    let newlines = frame.matches('\n').count();
    frame.encode_utf16().count() <= 800
        && newlines <= height.saturating_sub(10).min(2)
        && frame.split('\n').all(|line| {
            let columns: usize = line.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum();
            line.trim_end() == line
                && !line.chars().any(char::is_control)
                && columns < width.saturating_sub(4)
        })
}

/// How Claude shows a paste in an empty composer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Drawn {
    /// A `[Pasted text #N …]` placeholder.
    Folded,
    /// Every row, the prompt and continuation indent removed.
    Rows(Vec<String>),
}

/// Measured on Claude Code 2.1.293: a paste folds as in [`renders_flat`]; otherwise each line
/// wraps at width-4 and the composer shows at most (rows-10)/2 rows. `None` when not predictable.
pub(super) fn drawn(frame: &str, size: Option<(usize, usize)>) -> Option<Drawn> {
    let (width, height) = size?;
    // Claude may rewrite a tab or CR, changing even a folded paste's line count.
    if frame
        .split('\n')
        .any(|line| line.chars().any(char::is_control))
    {
        return None;
    }
    let newlines = frame.matches('\n').count();
    if frame.encode_utf16().count() > 800 || newlines > height.saturating_sub(10).min(2) {
        return Some(Drawn::Folded);
    }
    let mut rows = Vec::new();
    for line in frame.split('\n') {
        if line.trim_end() != line {
            return None;
        }
        rows.extend(wrap(line, width.checked_sub(4).filter(|&c| c >= 2)?)?);
    }
    // A row the composer reader would take for the prompt or a border could not prove the paste.
    let misread = |row: &String| {
        let shown = format!("  {row}");
        claude_prompt_row(&shown) || claude_border_row(&shown)
    };
    if rows[1..].iter().any(misread) {
        return None;
    }
    (rows.len() <= height.saturating_sub(10) / 2).then_some(Drawn::Rows(rows))
}

/// One line as wrap-ansi `hard` without trim lays it out; Claude drops the leading spaces of each
/// wrapped row and the capture drops trailing ones. `None` for a wrap past measured characters.
fn wrap(line: &str, columns: usize) -> Option<Vec<String>> {
    // Zero-width joiners, selectors, combining marks and modifiers may merge into one glyph.
    if line.chars().any(may_join) {
        return None;
    }
    // No character is wider than two columns, so this bound proves a line stays on one row.
    let bound: usize = line.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum();
    if bound <= columns {
        return Some(vec![line.to_string()]);
    }
    let mut rows = vec![String::new()];
    let mut used = 0;
    for (index, word) in line.split(' ').enumerate() {
        if index > 0 {
            if used >= columns {
                rows.push(String::new());
                used = 0;
            }
            rows.last_mut()?.push(' ');
            used += 1;
        }
        let length = word.chars().map(measured_width).sum::<Option<usize>>()?;
        if length > columns {
            // A word wider than the row breaks where it starts unless that costs a row.
            if (length - 1) / columns < 1 + (length - (columns - used) - 1) / columns {
                rows.push(String::new());
                used = 0;
            }
            let chars: Vec<char> = word.chars().collect();
            for (at, &c) in chars.iter().enumerate() {
                let cells = measured_width(c)?;
                if used + cells <= columns {
                    rows.last_mut()?.push(c);
                    used += cells;
                } else {
                    rows.push(c.to_string());
                    used = cells;
                }
                if used == columns && at + 1 < chars.len() {
                    rows.push(String::new());
                    used = 0;
                }
            }
            continue;
        }
        if used + length > columns && used > 0 && length > 0 {
            rows.push(String::new());
            used = 0;
        }
        rows.last_mut()?.push_str(word);
        used += length;
    }
    let shown = |(index, row): (usize, String)| {
        let row = row.trim_end_matches(' ');
        if index == 0 {
            row
        } else {
            row.trim_start_matches(' ')
        }
        .to_string()
    };
    Some(rows.into_iter().enumerate().map(shown).collect())
}

/// A character with no cell of its own, or a flag or skin-tone half that joins its neighbour.
fn may_join(c: char) -> bool {
    !matches!(c.width(), Some(1 | 2))
        || ('\u{1f1e6}'..='\u{1f1ff}').contains(&c)
        || ('\u{1f3fb}'..='\u{1f3ff}').contains(&c)
}

/// Columns measured in Claude's composer: printable ASCII one, Hangul syllables and jamo two.
fn measured_width(c: char) -> Option<usize> {
    match c {
        ' '..='~' => Some(1),
        '\u{ac00}'..='\u{d7a3}' | '\u{3131}'..='\u{318e}' => Some(2),
        _ => None,
    }
}

/// Whether the bottom-most `❯` row, and the rows under it down to a full border, hold text not
/// drawn faint; `None` without a `❯` row or its closing border. It needs no measured layout.
pub(super) fn typed(capture: &str) -> Option<bool> {
    let rows: Vec<Row> = capture.lines().map(row).collect();
    let prompt_row = |row: &Row| row.plain.trim_start().starts_with('❯');
    let prompt = rows.iter().rposition(prompt_row)?;
    let border = |row: &Row| {
        let text = row.plain.trim();
        text.chars().count() >= MIN_BORDER && text.chars().all(|c| c == '─')
    };
    let end = prompt + 1 + rows[prompt + 1..].iter().position(border)?;
    let solid = rows[prompt..end].iter().map(|row| row.solid.as_str());
    let text = solid.collect::<String>().replacen('❯', "", 1);
    Some(text.chars().any(|c| !c.is_whitespace()))
}

/// The composer shows exactly the frame, row for row, with the draft still stashed.
pub(super) fn owns(capture: &str, frame: &str) -> bool {
    let screen = read(capture);
    screen.stash == Stash::Present
        && matches!(&screen.composer, Composer::Text(rows) if rows.iter().map(String::as_str).eq(frame.split('\n')))
}

/// The C-s took: an empty composer under the stash marker.
pub(super) fn stashed(capture: &str) -> bool {
    let screen = read(capture);
    screen.stash == Stash::Present && screen.composer == Composer::Empty
}

/// Claude handed the draft back: the same rows and no stash.
pub(super) fn restored(capture: &str, draft: &[String]) -> bool {
    let screen = read(capture);
    screen.stash == Stash::AbsentInRecognizedLayout
        && matches!(&screen.composer, Composer::Text(rows) if rows == draft)
}
