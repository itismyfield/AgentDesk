/// The measured bypass-mode status row below the Claude composer.
pub(crate) const BYPASS_FOOTER: &str = "  ⏵⏵ bypass permissions on";

/// None is unrecognised chrome; a recognised row reports whether interruption is offered.
pub(crate) fn bypass_interrupt(row: &str) -> Option<bool> {
    let rest = status_rest(row)?;
    match rest.strip_prefix(" · ") {
        None => rest.is_empty().then_some(false),
        Some("esc to interrupt") => Some(true),
        Some(pill) => {
            let active = pill
                .strip_suffix(" · ↓ to manage")
                .unwrap_or(pill)
                .strip_suffix(" · esc to interrupt");
            let (counts, interrupt) = active.map_or((pill, false), |counts| (counts, true));
            counts.split(", ").all(counted).then_some(interrupt)
        }
    }
}

/// Whether the capture's recent rows carry Claude's own `esc to interrupt` chrome: the footer
/// under the composer or a spinner's status group. Prose quoting the phrase does not count.
pub(crate) fn interrupt_offered(capture: &str) -> bool {
    let rows: Vec<&str> = capture
        .lines()
        .filter(|row| !row.trim().is_empty())
        .collect();
    let recent = rows
        .len()
        .saturating_sub(super::CLAUDE_TUI_ACTIVE_SCAN_LINES);
    super::tmux_recent_lines_show_claude_tui_interrupt_chrome(&rows[recent..])
}

/// The status text after the bypass prefix, mode hint and update notice; None is unrecognised.
fn status_rest(row: &str) -> Option<&str> {
    let rest = row.trim_end().strip_prefix(BYPASS_FOOTER)?;
    let rest = rest.strip_prefix(" (shift+tab to cycle)").unwrap_or(rest);
    match rest.split_once("  ") {
        Some((rest, update)) if update.trim_start() == "✔ Update installed · Restart to update" => {
            Some(rest)
        }
        Some(_) => None,
        None => Some(rest),
    }
}

/// A positive whole count with a matching singular/plural background-task noun.
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
