//! Reads the active Claude composer and the status row right above its top border from one
//! `capture-pane -e` capture. Anything outside the recognised layout reads as Unknown.

mod paste_footer;

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
pub(in crate::services::claude_tui) enum Composer {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Footer {
    Normal,
    PasteExpand,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Screen {
    pub(super) composer: Composer,
    pub(super) stash: Stash,
    pub(super) footer: Footer,
}

const UNKNOWN: Screen = Screen {
    composer: Composer::Unknown,
    stash: Stash::Unknown,
    footer: Footer::Unknown,
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

/// Equal box edges and a single prompt delimit the input independently of footer chrome.
fn box_bounds(rows: &[Row]) -> Option<(usize, usize)> {
    let prompt = rows.iter().rposition(|row| row.plain.starts_with('❯'))?;
    let border = |index: usize| -> Option<usize> {
        let text = rows.get(index)?.plain.trim_end();
        let width = text.chars().count();
        (width >= MIN_BORDER && text.chars().all(|c| c == '─')).then_some(width)
    };
    let width = border(prompt.checked_sub(1)?)?;
    let bottom = (prompt + 1..rows.len()).find(|&index| border(index).is_some())?;
    (border(bottom) == Some(width)).then_some((prompt, bottom))
}

/// A typed draft C-s may park: not while `› stashed` above the box shows a stash C-s would replace.
pub(in crate::services::claude_tui) fn parkable(capture: &str) -> bool {
    let rows: Vec<Row> = capture.lines().map(row).collect();
    let Some((prompt, bottom)) = box_bounds(&rows) else {
        return false;
    };
    let width = rows[prompt - 1].plain.trim_end().chars().count();
    let stashed = prompt
        .checked_sub(2)
        .is_some_and(|status| stash(&rows[status].plain, width) == Stash::Present);
    !stashed && matches!(composer(&rows[prompt..bottom]), Composer::Text(_))
}

/// Unknown never triggers C-s: a faint or incomplete field may hide an old stash toggle.
pub(in crate::services::claude_tui) fn input_box(capture: &str) -> Composer {
    let rows: Vec<Row> = capture.lines().map(row).collect();
    box_bounds(&rows).map_or(Composer::Unknown, |(prompt, bottom)| {
        composer(&rows[prompt..bottom])
    })
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
        footer: if paste_footer::recognized(rows[foot].plain.trim_end()) {
            Footer::PasteExpand
        } else {
            Footer::Normal
        },
    }
}

pub(super) fn modal(capture: &str) -> bool {
    use crate::services::tui_input::actor::gate::{PaneVerdict, judge_pane};
    use crate::services::tui_o::shadow::ShadowProvider;
    if judge_pane(ShadowProvider::Claude, capture) == PaneVerdict::Modal {
        return true;
    }
    let plain = crate::services::codex_tui::input::strip_ansi_escape_sequences(capture);
    let rows: Vec<&str> = plain.lines().collect();
    let chrome = if input_box(capture) != Composer::Unknown {
        rows.iter()
            .rposition(|row| row.starts_with('❯'))
            .and_then(|prompt| {
                let bottom = (prompt + 1..rows.len()).find(|&index| {
                    let row = rows[index].trim_end();
                    row.chars().count() >= MIN_BORDER && row.chars().all(|c| c == '─')
                })?;
                Some(
                    rows[..prompt]
                        .iter()
                        .chain(rows[bottom..].iter())
                        .copied()
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            })
    } else {
        None
    };
    // Retain the older permission-card veto outside a proven composer, including above blanks.
    crate::services::tmux_common::tmux_capture_indicates_claude_tui_interactive_modal(
        chrome.as_deref().unwrap_or(&plain),
    )
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
    // Measured on Claude Code 2.1.309: this effort row uses a one-cell `◉`.
    let measured_effort = drawn.trim() == "◉ xhigh · /effort";
    let whole = drawn.starts_with(' ')
        && (measured_effort || drawn.chars().all(|c| c.is_ascii() || c == '\u{b7}'))
        && drawn.chars().count() + 2 == width;
    if whole {
        Stash::AbsentInRecognizedLayout
    } else {
        Stash::Unknown
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
