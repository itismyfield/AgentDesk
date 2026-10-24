//! Reads the active Claude composer and the status row right above its top border from one
//! `capture-pane -e` capture. Anything outside the recognised layout reads as Unknown.

mod paste_footer;

use unicode_width::UnicodeWidthChar;

use crate::services::tui_input::actor::gate::{
    claude_border_row, claude_prompt_row, own_wrapped_draft,
};

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

/// The bottom-most `❯` row between two equal full borders; below, only status-line rows and then
/// a measured footer as the last row drawn.
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
    // A box that closes at another width, a footer cut, unmeasured or not last, or a row between
    // that is no status line (another box, a `!` bash-mode prompt, a blank) is stale or unread.
    let last = rows
        .iter()
        .rposition(|row| !row.plain.trim_end().is_empty());
    let Some(foot) = last.filter(|&last| last > bottom && footer(rows[last].plain.trim_end()))
    else {
        return UNKNOWN;
    };
    let statusline = &rows[bottom + 1..foot];
    if border(bottom) != Some(width) || !statusline.iter().all(|row| status_line(&row.plain)) {
        return UNKNOWN;
    }
    // A stash marker under the box is in no measured layout, so the row above cannot rule it out.
    let marked = |row: &Row| row.plain.contains('\u{203a}') || row.plain.contains("stash");
    Screen {
        composer: composer(&rows[prompt..bottom]),
        stash: if statusline.iter().any(marked) {
            Stash::Unknown
        } else {
            stash(&rows[status].plain, width)
        },
    }
}

/// A row of the user's status line, drawn two columns in like the footer; never a box edge, a
/// prompt or a second footer.
fn status_line(row: &str) -> bool {
    let first = row
        .strip_prefix("  ")
        .and_then(|rest| rest.trim_start().chars().next());
    !row.starts_with(FOOTER)
        && first.is_some_and(|c| c != '❯' && !('\u{2500}'..='\u{257f}').contains(&c))
}

/// The bypass-mode footer with a measured suffix, or the folded-paste hint; a footer cut at
/// the edge or carrying an unmeasured suffix is not.
fn footer(row: &str) -> bool {
    let Some(rest) = row.strip_prefix(FOOTER) else {
        return paste_footer::recognized(row);
    };
    let rest = rest.strip_prefix(" (shift+tab to cycle)").unwrap_or(rest);
    match rest.strip_prefix(" \u{b7} ") {
        None => rest.is_empty(),
        Some("esc to interrupt") => true,
        Some(pill) => pill.split(", ").all(counted),
    }
}

/// `1 monitor` or `4 monitors`: a whole count whose noun agrees with it, as Claude Code 2.1.299
/// names background shells, monitors, local agents and mixed tasks.
fn counted(phrase: &str) -> bool {
    let Some((count, noun)) = phrase.split_once(' ') else {
        return false;
    };
    let whole =
        !count.is_empty() && !count.starts_with('0') && count.bytes().all(|b| b.is_ascii_digit());
    let noun = if count == "1" {
        Some(noun)
    } else {
        noun.strip_suffix('s')
    };
    let stems = ["shell", "monitor", "local agent", "background task"];
    whole && noun.is_some_and(|noun| stems.contains(&noun))
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
pub(in crate::services::claude_tui) fn stashable(
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
    /// Wrapping is unknown; only the full captured body can prove this paste.
    Body,
    /// A `[Pasted text #N …]` placeholder.
    Folded,
    /// Every row, the prompt and continuation indent removed.
    Rows(Vec<String>),
}

/// Wrap predictions are optional for ownership; an unfolded body must remain fully visible.
pub(super) fn direct(frame: &str, size: Option<(usize, usize)>) -> Option<Drawn> {
    let (width, height) = size?;
    if width < 6
        || height <= 10
        || frame
            .split('\n')
            .any(|line| line.chars().any(char::is_control))
    {
        return None;
    }
    if let Some(drawn) = drawn(frame, size) {
        return Some(drawn);
    }
    let columns = width - 4;
    let mut rows = 0;
    let mut shown = Vec::new();
    for line in frame.split('\n') {
        match wrap(line, columns) {
            Some(wrapped) => {
                rows += wrapped.len();
                shown.extend(wrapped);
            }
            None => {
                // Word wrapping may use half a row; only unknown lines need this upper bound.
                let bound: usize = line.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum();
                rows += if bound <= columns {
                    1
                } else {
                    2 * bound.div_ceil(columns) + 1
                };
                shown.push(line.to_owned());
            }
        }
    }
    if shown[1..].iter().any(|row| chrome_row(row)) {
        return None;
    }
    (rows <= height.saturating_sub(10) / 2).then_some(Drawn::Body)
}

/// Proves the complete body or an exact placeholder; multiline pastes may fold before predicted.
pub(super) fn owns_direct(capture: &str, frame: &str, drawn: &Drawn) -> bool {
    let Composer::Text(rows) = read(capture).composer else {
        return false;
    };
    if matches!(drawn, Drawn::Rows(expected) if own_wrapped_draft(capture, expected))
        || rows
            .iter()
            .flat_map(|row| row.chars())
            .filter(|c| !c.is_whitespace())
            .eq(frame.chars().filter(|c| !c.is_whitespace()))
    {
        return true;
    }
    let [row] = rows.as_slice() else {
        return false;
    };
    let lines = frame.matches('\n').count();
    if !matches!(drawn, Drawn::Folded) && lines == 0 {
        return false;
    }
    let Some(rest) = row.strip_prefix("[Pasted text #") else {
        return false;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0
        && if lines == 0 {
            &rest[digits..] == "]"
        } else {
            rest[digits..] == format!(" +{lines} lines]")
        }
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
    if rows[1..].iter().any(|row| chrome_row(row)) {
        return None;
    }
    (rows.len() <= height.saturating_sub(10) / 2).then_some(Drawn::Rows(rows))
}

// A row the composer reader takes for a prompt or border cannot prove the paste.
fn chrome_row(row: &str) -> bool {
    let shown = format!("  {row}");
    claude_prompt_row(&shown) || claude_border_row(&shown)
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
/// The input header's `·` and `📱` take one and two, as string-width and tmux both count them; a
/// paste whose rows differ is still proven by its whitespace-free body.
fn measured_width(c: char) -> Option<usize> {
    match c {
        ' '..='~' | '\u{b7}' | '\u{2192}' => Some(1),
        '\u{ac00}'..='\u{d7a3}' | '\u{3131}'..='\u{318e}' | '\u{1f4f1}' => Some(2),
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
pub(in crate::services::claude_tui) fn owns(capture: &str, frame: &str) -> bool {
    let screen = read(capture);
    screen.stash == Stash::Present
        && matches!(&screen.composer, Composer::Text(rows) if rows.iter().map(String::as_str).eq(frame.split('\n')))
}

/// The C-s took: an empty composer under the stash marker.
pub(in crate::services::claude_tui) fn stashed(capture: &str) -> bool {
    let screen = read(capture);
    screen.stash == Stash::Present && screen.composer == Composer::Empty
}

/// Claude handed the draft back: the same rows and no stash.
pub(super) fn restored(capture: &str, draft: &[String]) -> bool {
    let screen = read(capture);
    screen.stash == Stash::AbsentInRecognizedLayout
        && matches!(&screen.composer, Composer::Text(rows) if rows == draft)
}
