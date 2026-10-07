//! Reads the active Claude composer and the status row right above its top border from one
//! `capture-pane -e` capture. Anything outside the recognised layout reads as Unknown.

/// Draft chips whose payload is not measured to survive a stash round trip.
const UNMEASURED_CHIPS: [&str; 3] = [
    "[Image #",
    "[...Truncated text #",
    "\u{2726} Team setup guide #",
];
const STASHED: &str = "\u{203a} stashed";
const MIN_BORDER: usize = 10;

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

/// One captured row: all visible text, and the part not drawn faint.
struct Row {
    plain: String,
    solid: String,
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
    Row { plain, solid }
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

/// The bottom-most `❯` row between two equal full borders, with footer chrome below the box.
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
    // A box that closes at another width, has nothing under it, or has another box below it (a
    // `!` bash-mode prompt) is cut, stale or not Claude's.
    let below = bottom + 1..rows.len();
    if border(bottom) != Some(width)
        || rows[below.clone()]
            .iter()
            .all(|r| r.plain.trim().is_empty())
        || below.into_iter().any(|index| border(index).is_some())
    {
        return UNKNOWN;
    }
    Screen {
        composer: composer(&rows[prompt..bottom]),
        stash: stash(&rows[status].plain, width),
    }
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
        // A faint placeholder or ghost suggestion only ever stands in for an empty composer.
        if plain != solid {
            let blank = rows.len() == 1 && solid.trim().is_empty();
            return if blank {
                Composer::Empty
            } else {
                Composer::Unknown
            };
        }
        lines.push(plain);
    }
    match (lines.len(), lines.iter().all(|line| line.trim().is_empty())) {
        (1, true) => Composer::Empty,
        (_, true) => Composer::Unknown,
        _ => Composer::Text(lines),
    }
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
    let chips = rows
        .iter()
        .any(|row| UNMEASURED_CHIPS.iter().any(|chip| row.contains(chip)));
    (!chips && screen.stash == Stash::AbsentInRecognizedLayout).then_some(rows)
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
