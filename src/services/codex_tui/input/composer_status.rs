pub(super) fn line_is_codex_fast_context_status(line: &str) -> bool {
    let parts = status_parts(line);
    if parts.len() == 3 && matches!(parts[0], "Fast on" | "Fast off") {
        return !parts[1].is_empty() && context_percent(parts[2]).is_some();
    }
    model_first_status(&parts)
}

pub(super) fn line_is_codex_model_first_status(line: &str) -> bool {
    model_first_status(&status_parts(line))
}

fn status_parts(line: &str) -> Vec<&str> {
    let status = strip_warning_notice(line);
    // An invalid right-aligned notice cannot stand in for an opaque clipped slot.
    if status == line && line.contains("  ⚠ ") {
        return Vec::new();
    }
    status.split('·').map(str::trim).collect()
}

/// Codex right-aligns `⚠ N warning(s) · f2 to view` on the status row and cuts the row to fit.
fn strip_warning_notice(line: &str) -> &str {
    let Some((status, notice)) = line.trim_end().rsplit_once('⚠') else {
        return line;
    };
    let (notice, hint) = match notice.rsplit_once('·') {
        Some((notice, hint)) if matches!(hint.trim(), "f2 to view" | "f2 to…" | "f2 t…" | "f2") => {
            (notice, Some(hint.trim()))
        }
        Some(_) => return line,
        None => (notice, None),
    };
    let notice: Vec<&str> = notice.split_ascii_whitespace().collect();
    let counted = notice
        .first()
        .is_some_and(|count| !count.is_empty() && count.bytes().all(|ch| ch.is_ascii_digit()))
        && ((notice.len() == 2 && matches!(notice[1], "warning" | "warnings"))
            || (notice.len() == 1 && hint == Some("f2")));
    if counted { status } else { line }
}

fn model_first_status(parts: &[&str]) -> bool {
    // A row cut with `…` keeps the intact head; the cut part must still be a prefix of its slot.
    let (parts, cut) = match parts.split_last() {
        Some((last, head)) if last.ends_with('…') => (head, last.strip_suffix('…')),
        _ => (parts, None),
    };
    let Some(fast) = parts
        .iter()
        .skip(1)
        .position(|part| matches!(*part, "Fast on" | "Fast off"))
        .map(|index| index + 1)
    else {
        return false;
    };
    // Model and optional effort are opaque identifiers; the footer structure is the evidence.
    let model: Vec<&str> = parts[0].split_ascii_whitespace().collect();
    let head = (1..=2).contains(&model.len())
        && model.iter().all(|token| identifier(token))
        && parts[1..fast].iter().all(|part| quota(part));
    let context = |part: &&str| {
        context_percent(part)
            .and_then(|n| n.parse::<u8>().ok())
            .is_some_and(|n| n <= 100)
    };
    let Some(cut) = cut else {
        return head
            && parts.get(fast + 1).is_some_and(context)
            && parts.len() <= fast + 3
            && parts[fast + 2..].iter().all(|part| context_window(part));
    };
    head && match parts.len() - fast {
        1 => context_prefix(cut),
        2 => parts.get(fast + 1).is_some_and(context) && context_window_prefix(cut),
        3 => parts.get(fast + 1).is_some_and(context) && context_window(parts[fast + 2]),
        _ => false,
    }
}

fn context_prefix(cut: &str) -> bool {
    let Some(rest) = cut.strip_prefix("Context ") else {
        return "Context ".starts_with(cut);
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    rest.is_empty() || (digits > 0 && "% left".starts_with(&rest[digits..]))
}

fn context_window_prefix(cut: &str) -> bool {
    let digits = cut
        .bytes()
        .take_while(|ch| ch.is_ascii_digit() || *ch == b'.')
        .count();
    let rest = &cut[digits..];
    let rest = rest.strip_prefix(['K', 'M']).unwrap_or(rest);
    cut.is_empty() || (digits > 0 && " window".starts_with(rest))
}

fn identifier(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || b"-_.:/".contains(&ch))
}

fn context_percent(value: &str) -> Option<&str> {
    let percent = value.strip_prefix("Context ")?.strip_suffix("% left")?;
    (!percent.is_empty() && percent.bytes().all(|ch| ch.is_ascii_digit())).then_some(percent)
}

fn quota(value: &str) -> bool {
    let parts: Vec<&str> = value.split_ascii_whitespace().collect();
    parts.len() == 3
        && identifier(parts[0])
        && parts[1]
            .strip_suffix('%')
            .filter(|n| !n.is_empty() && n.bytes().all(|ch| ch.is_ascii_digit()))
            .and_then(|n| n.parse::<u8>().ok())
            .is_some_and(|n| n <= 100)
        && parts[2] == "left"
}

fn context_window(value: &str) -> bool {
    let Some(size) = value.strip_suffix(" window") else {
        return false;
    };
    let number = size
        .strip_suffix('K')
        .or_else(|| size.strip_suffix('M'))
        .unwrap_or(size);
    !number.is_empty()
        && number.bytes().all(|ch| ch.is_ascii_digit() || ch == b'.')
        && number
            .parse::<f64>()
            .is_ok_and(|n| n.is_finite() && n > 0.0)
}

// A truncated footer corroborates a complete composer without classifying arbitrary prose as status.
fn short_composer_footer(line: &str) -> bool {
    let parts = status_parts(line);
    let Some((cut, head)) = parts.split_last() else {
        return false;
    };
    let Some(cut) = cut.strip_suffix('…') else {
        return false;
    };
    let Some((model, quotas)) = head.split_first() else {
        return false;
    };
    let model: Vec<&str> = model.split_ascii_whitespace().collect();
    if !(1..=2).contains(&model.len())
        || !model.iter().all(|part| identifier(part))
        || !quotas.iter().all(|part| quota(part))
    {
        return false;
    }
    if !quotas.is_empty()
        && ["Fast on", "Fast off"]
            .iter()
            .any(|slot| slot.starts_with(cut))
    {
        return true;
    }
    let fields: Vec<&str> = cut.split_ascii_whitespace().collect();
    let Some(label) = fields.first() else {
        return !quotas.is_empty();
    };
    if !identifier(label) || fields.len() > 3 {
        return false;
    }
    let Some(percent) = fields.get(1) else {
        return true;
    };
    let digits = percent.strip_suffix('%').unwrap_or(percent);
    !digits.is_empty()
        && digits.bytes().all(|ch| ch.is_ascii_digit())
        && digits.parse::<u8>().is_ok_and(|value| value <= 100)
        && (fields.len() == 2 || (percent.ends_with('%') && "left".starts_with(fields[2])))
}

pub(super) fn pane_has_dim_composer(pane: &str) -> bool {
    let Some((at, body)) = composer_draft_with_start_in_pane(pane) else {
        return false;
    };
    let plain = super::strip_ansi_escape_sequences(pane);
    plain
        .lines()
        .nth(at)
        .is_some_and(|line| line.starts_with('›'))
        && (body.is_empty()
            || (!body.contains('\n')
                && pane.lines().nth(at).is_some_and(compact_body_is_fully_dim)))
}

pub(super) fn recent_has_compact_composer(recent: &[&str]) -> bool {
    recent
        .get(1)
        .is_some_and(|line| super::line_is_codex_compact_prompt_marker(line))
        && recent.first().is_some_and(|line| {
            super::line_is_codex_compact_status_line(line)
                || line_is_codex_fast_context_status(line)
                || (short_composer_footer(line) && !super::line_is_legacy_codex_prompt(recent[1]))
        })
}

pub(super) fn pane_has_interactive_modal(pane: &str) -> bool {
    let recent: Vec<&str> = pane
        .lines()
        .rev()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(6)
        .collect();
    if !recent
        .iter()
        .take(3)
        .any(|line| line_is_codex_model_first_status(line) || short_composer_footer(line))
    {
        return false;
    }
    let header = recent
        .iter()
        .position(|line| line.starts_with('›'))
        .and_then(|prompt| recent.get(prompt + 1));
    header.is_some_and(|line| {
        let lower = line.to_ascii_lowercase();
        super::CODEX_INTERACTIVE_MODAL_MARKERS
            .iter()
            .any(|marker| lower.starts_with(marker))
    })
}

fn action_banner_above(lines: &[&str], at: usize) -> bool {
    lines[..at]
        .iter()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(2)
        .any(|line| {
            line.contains("esc to dismiss")
                || line.trim_start().starts_with("Press a number to choose")
        })
}

pub(super) fn composer_draft_with_start_in_pane(pane: &str) -> Option<(usize, String)> {
    let raw: Vec<&str> = pane.lines().collect();
    let plain = super::strip_ansi_escape_sequences(pane);
    let lines: Vec<&str> = plain.lines().collect();
    if lines
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .is_some_and(|at| lines[at].starts_with('›') && compact_body_is_fully_dim(raw[at]))
    {
        let at = lines.iter().rposition(|line| !line.trim().is_empty())?;
        let body = lines[at].strip_prefix('›')?.trim();
        let header = lines[..at]
            .iter()
            .rev()
            .find(|line| !line.trim().is_empty());
        let modal = header.is_some_and(|line| {
            let lower = line.trim().to_ascii_lowercase();
            super::CODEX_INTERACTIVE_MODAL_MARKERS
                .iter()
                .any(|marker| lower.starts_with(marker))
        });
        return (matches!(
            body,
            "Ask Codex to do anything" | "Use /skills to list available skills"
        ) && compact_body_is_fully_dim(raw.get(at)?)
            && !modal
            && !action_banner_above(&lines, at)
            && !super::pane_has_codex_active_turn_in_pane(&plain))
        .then(|| (at, body.to_string()));
    }
    let footer = lines.iter().rposition(|line| {
        super::line_is_codex_footer_hint(line)
            || line_is_codex_model_first_status(line)
            || line_is_codex_fast_context_status(line)
            || super::line_is_codex_compact_status_line(line)
            || short_composer_footer(line)
    })?;
    if lines[footer + 1..]
        .iter()
        .any(|line| !line.trim().is_empty())
    {
        return None;
    }
    let end = (0..footer).rfind(|&at| !lines[at].trim().is_empty())?;
    let border = |line: &str, left: char, right: char| {
        line.trim()
            .strip_prefix(left)
            .and_then(|body| body.strip_suffix(right))
            .is_some_and(|body| {
                body.chars().count() + 2 >= super::COMPOSER_EDGE_MIN_GLYPHS
                    && body.chars().all(|ch| ch == '─')
            })
    };
    if border(lines[end], '╰', '╯') {
        let start = (0..end).rfind(|&at| border(lines[at], '╭', '╮'))?;
        let mut rows = Vec::new();
        let mut cursors = 0;
        for line in &lines[start + 1..end] {
            let body = line
                .trim_start()
                .strip_prefix("│ ")?
                .strip_suffix('│')?
                .trim_end();
            let body = if let Some(body) = body.strip_suffix('▌') {
                cursors += 1;
                body.strip_suffix(' ').unwrap_or(body)
            } else {
                body
            };
            if body.contains('▌') {
                return None;
            }
            rows.push(body);
        }
        return (cursors == 1).then(|| (start, rows.join("\n")));
    }
    let start = (0..=end).rfind(|&at| lines[at].starts_with('›'))?;
    let first = lines[start].strip_prefix('›')?;
    let mut rows = vec![first.strip_prefix(' ').unwrap_or(first)];
    for line in &lines[start + 1..=end] {
        rows.push(line.strip_prefix("  ").or(line.is_empty().then_some(""))?);
    }
    if short_composer_footer(lines[footer])
        && (action_banner_above(&lines, start) || rows.iter().all(|row| row.trim().is_empty()))
    {
        return None;
    }
    Some((start, rows.join("\n")))
}

// Track raw current-row SGR state so a dim span cannot hide ordinary typed characters.
pub(super) fn compact_body_is_fully_dim(line: &str) -> bool {
    let mut remaining = line;
    let (mut dim, mut marker, mut body) = (false, false, false);
    while !remaining.is_empty() {
        if let Some(sequence) = remaining.strip_prefix("\x1b[") {
            let Some((parameters, after)) = sequence.split_once('m') else {
                return false;
            };
            if !parameters
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b';')
            {
                return false;
            }
            let Some(codes) = parameters
                .split(';')
                .map(|code| {
                    if code.is_empty() {
                        Some(0)
                    } else {
                        code.parse::<u16>().ok()
                    }
                })
                .collect::<Option<Vec<_>>>()
            else {
                return false;
            };
            let mut at = 0;
            while at < codes.len() {
                match codes[at] {
                    0 | 22 => dim = false,
                    2 => dim = true,
                    38 | 48 | 58 => {
                        let channels = match codes.get(at + 1) {
                            Some(2) => 3,
                            Some(5) => 1,
                            _ => return false,
                        };
                        if !codes
                            .get(at + 2..at + 2 + channels)
                            .is_some_and(|values| values.iter().all(|value| *value <= 255))
                        {
                            return false;
                        }
                        at += channels + 2;
                        continue;
                    }
                    1 | 3..=9 | 23..=37 | 39..=47 | 49 | 53 | 55 | 59 | 90..=97 | 100..=107 => {}
                    _ => return false,
                }
                at += 1;
            }
            remaining = after;
            continue;
        }
        let mut characters = remaining.chars();
        let Some(character) = characters.next() else {
            return false;
        };
        remaining = characters.as_str();
        if character.is_control() {
            return false;
        }
        if !marker {
            if character != '›' {
                return false;
            }
            marker = true;
        } else if !character.is_whitespace() {
            if !dim {
                return false;
            }
            body = true;
        }
    }
    marker && body
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::line_is_codex_model_first_status;

    const CUT_IDLE: &str =
        include_str!("../../../../tests/fixtures/tui_input/codex-status-astra-113-idle.ansi");
    const IDLE_46: &str =
        include_str!("../../../../tests/fixtures/tui_input/codex-status-sol-46-idle.ansi");
    const IDLE_80: &str =
        include_str!("../../../../tests/fixtures/tui_input/codex-status-sol-80-idle.ansi");
    const IDLE_121: &str =
        include_str!("../../../../tests/fixtures/tui_input/codex-status-sol-121-idle.ansi");
    const IDLE_WIDTHS: [(usize, &str); 4] = [
        (46, IDLE_46),
        (80, IDLE_80),
        (113, CUT_IDLE),
        (121, IDLE_121),
    ];
    const ASTRA_STATUS: &str = "GPT-6-Astra xhigh · weekly 62% left · Fast off · Context 77% left · 258K window · M…  ⚠ 1 warning · f2";
    const SOL_STATUS: &str = "GPT-6.1-Sol xhigh · weekly 73% left · Fast off · Context 92% left · 475K window · Main [def…  ⚠ 1 warning · f2 to view";

    #[test]
    fn cut_astra_status_row_is_recognized() {
        assert!(
            line_is_codex_model_first_status(ASTRA_STATUS),
            "{ASTRA_STATUS}"
        );
        assert!(line_is_codex_fast_context_status(ASTRA_STATUS));
    }

    #[test]
    fn cut_sol_status_row_is_recognized() {
        let capture = include_str!(
            "../../../../tests/fixtures/tui_input/codex-status-sol-before-kill-busy.plain"
        );
        assert_eq!(capture.lines().last().unwrap().trim(), SOL_STATUS);
        assert!(line_is_codex_model_first_status(SOL_STATUS), "{SOL_STATUS}");
        assert!(line_is_codex_fast_context_status(SOL_STATUS));
    }

    #[test]
    fn cut_113_column_idle_ansi_is_ready() {
        for pane in [CUT_IDLE.to_string(), CUT_IDLE.replace(" to view", "")] {
            assert!(pane_looks_ready_for_codex_prompt_with_ansi(&pane), "{pane}");
        }
    }

    #[test]
    fn cut_status_keeps_busy_modal_and_non_dim_draft_not_ready() {
        for (width, pane) in IDLE_WIDTHS {
            for state in [
                "• Working (2s • esc to interrupt)",
                "Approval required",
                "Allow command",
                "Do you trust this folder?",
                "Sign in to continue",
                "Update available",
            ] {
                let blocked = format!("{state}\n{pane}");
                let ready = pane_looks_ready_for_codex_prompt_with_ansi(&blocked);
                eprintln!("blocked width={width} state={state:?} ready={ready}");
                assert!(!ready, "width={width}: {state}");
            }
            let draft = pane.replace(
                "\x1b[2mAsk Codex to do anything\x1b[0m",
                "user draft ⚠ 1 warning · f2…",
            );
            assert!(
                !pane_looks_ready_for_codex_prompt_with_ansi(&draft),
                "width={width}"
            );
            assert!(prompt_readiness_from_ansi_pane(&draft).1);
        }
        let sol_busy = format!("• Working (2s • esc to interrupt)\n\n›\n\n{SOL_STATUS}");
        assert!(!pane_looks_ready_for_codex_prompt_with_ansi(&sol_busy));
    }

    #[test]
    fn cut_46_column_dim_composer_is_ready_without_full_status() {
        let plain = strip_ansi_escape_sequences(IDLE_46);
        assert!(!line_is_codex_model_first_status(
            plain.lines().last().unwrap()
        ));
        assert!(pane_looks_ready_for_codex_prompt_with_ansi(IDLE_46));
        assert!(!pane_looks_ready_for_codex_prompt_with_ansi(&plain));
        assert!(!prompt_readiness_from_ansi_pane(IDLE_46).1);
    }

    #[test]
    fn cut_idle_ansi_is_ready_at_all_measured_widths() {
        for (width, pane) in IDLE_WIDTHS {
            let ready = pane_looks_ready_for_codex_prompt_with_ansi(pane);
            eprintln!("idle width={width} ready={ready}");
            assert!(ready, "width={width}");
            assert!(!prompt_readiness_from_ansi_pane(pane).1, "width={width}");
        }
    }

    #[test]
    fn cut_idle_rejects_prose_bad_notices_and_partial_dim_drafts_at_all_widths() {
        for (width, pane) in IDLE_WIDTHS {
            let (body, status) = pane.trim_end().rsplit_once('\n').unwrap();
            for prose in [
                "see ⚠ 1 warning · f2",
                "see … ⚠ 1 warning · f2",
                "Context 77% left",
            ] {
                let pane = format!("{body}\n{prose}");
                assert!(
                    !pane_looks_ready_for_codex_prompt_with_ansi(&pane),
                    "width={width}: {prose}"
                );
            }
            let plain_status = strip_ansi_escape_sequences(status);
            let (head, _) = plain_status.rsplit_once('⚠').unwrap();
            for notice in [
                "one warning · f2",
                "one · f2",
                "1 extra · f2",
                "1",
                "1 · f2 to view",
            ] {
                let pane = format!("{body}\n{head}⚠ {notice}");
                assert!(
                    !pane_looks_ready_for_codex_prompt_with_ansi(&pane),
                    "width={width}: {notice}"
                );
            }
            for draft in [
                "user draft ⚠ 1 warning · f2…",
                "typed \x1b[2mhint\x1b[0m",
                "\x1b[2mAsk Codex to do anything\x1b[0m typed",
                "\x1b[38;2;2;2;2mAsk Codex to do anything\x1b[0m",
                "\x1b[38;5;2mAsk Codex to do anything\x1b[0m",
            ] {
                let pane = pane.replace("\x1b[2mAsk Codex to do anything\x1b[0m", draft);
                assert!(
                    !pane_looks_ready_for_codex_prompt_with_ansi(&pane),
                    "width={width}: {draft}"
                );
            }
            eprintln!("negative prose/count/SGR width={width} ready=false");
        }
    }

    #[test]
    fn cut_status_accepts_counted_warning_suffix_variants() {
        let status =
            "codex-next balanced · weekly 62% left · Fast off · Context 77% left · 258K window";
        for suffix in [" · f2 to view", " · f2 to…", " · f2 t…", " · f2", ""] {
            for count in ["1 warning", "12 warnings"] {
                let line = format!("{status}  ⚠ {count}{suffix}");
                assert!(line_is_codex_model_first_status(&line), "{line}");
                assert!(line_is_codex_fast_context_status(&line), "{line}");
            }
        }
    }

    #[test]
    fn cut_status_rejects_prose_bad_counts_and_out_of_order_slots() {
        let head = "codex-next balanced · weekly 62% left · Fast off";
        for line in [
            "see ⚠ 1 warning · f2".to_string(),
            "Context 77% left".to_string(),
            "see … ⚠ 1 warning · f2".to_string(),
            format!("{head} · Context 77% left · 258K window · Main"),
            format!("{head} · Context 77% left · prose · Main…"),
            format!("{head} · 258K window · Context 77% left · Main…"),
            format!("{head} · Context 77% left · 258K window · Main · extra…"),
        ] {
            assert!(!line_is_codex_model_first_status(&line), "{line}");
            assert!(!line_is_codex_fast_context_status(&line), "{line}");
        }
        let status = format!("{head} · Context 77% left · 258K window");
        for notice in [
            "one warning · f2",
            "one warning…",
            "-1 warning · f2",
            "1.0 warnings · f2",
            "1 warnings extra · f2",
            "1 warning · f2 prose",
            "1 warning · f2 to… prose",
        ] {
            let extra = if notice.ends_with('…') {
                " · Main…"
            } else {
                ""
            };
            let line = format!("{status}{extra}  ⚠ {notice}");
            assert_eq!(super::strip_warning_notice(&line), line);
            assert!(!line_is_codex_model_first_status(&line), "{line}");
        }
    }

    #[test]
    fn cut_status_supports_structural_slots_at_different_widths() {
        let full = "codex-next balanced · weekly 62% left · Fast off · Context 77% left · 475K window · Main [default workspace with a long name extending past a wide terminal status row]";
        for width in [80, 100, 121, 160] {
            let notice = "  ⚠ 1 warning · f2";
            let budget = width - unicode_width::UnicodeWidthStr::width(notice) - 1;
            let prefix: String = full.chars().take(budget).collect();
            let row = format!("{prefix}…{notice}");
            assert_eq!(unicode_width::UnicodeWidthStr::width(row.as_str()), width);
            assert!(
                line_is_codex_model_first_status(&row),
                "width={width}: {row}"
            );
            let pane = format!("\n›\n\n{row}");
            assert!(pane_looks_ready_for_codex_prompt_with_ansi(&pane), "{row}");
        }
    }

    #[test]
    fn canonical_cut_composer_proves_idle_and_human_draft_at_each_width() {
        for (width, pane) in IDLE_WIDTHS {
            let (ready, draft, _) = submission_readiness_from_ansi_pane(pane);
            eprintln!("canonical idle width={width} ready={ready} draft={draft}");
            assert!(ready && !draft, "width={width}");
            let pane = own_draft(pane);
            let (ready, draft, _) = submission_readiness_from_ansi_pane(&pane);
            eprintln!("canonical human draft width={width} ready={ready} draft={draft}");
            assert!(!ready && draft, "width={width}");
            assert_eq!(
                submission_draft_in_pane(&strip_ansi_escape_sequences(&pane)),
                Some("fixture follow-up".to_string())
            );
        }
    }

    #[test]
    fn canonical_cut_composer_preserves_busy_modal_and_complete_body_guards() {
        for (width, pane) in IDLE_WIDTHS {
            for state in [
                "• Working (2s • esc to interrupt)",
                "Approval required",
                "Allow command",
                "Sign in to continue",
                "Update available",
            ] {
                let blocked = pane.replace("\x1b[1m›", &format!("{state}\n\x1b[1m›"));
                let (ready, _, _) = submission_readiness_from_ansi_pane(&blocked);
                eprintln!("canonical blocked width={width} state={state:?} ready={ready}");
                assert!(!ready, "width={width}: {state}");
            }
            for body in [
                "typed \x1b[2mhint\x1b[0m",
                "\x1b[2mhint\x1b[22m typed",
                "\x1b[38;2;2;2;2mhint\x1b[0m",
                "\x1b[38;5;2mhint\x1b[0m",
            ] {
                let draft = pane.replace("\x1b[2mAsk Codex to do anything\x1b[0m", body);
                let (ready, has_draft, _) = submission_readiness_from_ansi_pane(&draft);
                assert!(!ready && has_draft, "width={width}: {body}");
            }
            let extra = format!("{pane}\nnew output");
            assert!(
                !submission_readiness_from_ansi_pane(&extra).0,
                "width={width}"
            );
        }
    }

    #[test]
    fn complete_dim_composer_uses_early_footer_prefix_as_auxiliary_only() {
        for prefix in [
            "w",
            "weekly 5",
            "weekly 50%",
            "weekly 50% l",
            "weekly 50% left · F",
            "weekly 50% left · Fast o",
        ] {
            let row = format!("codex-next balanced · {prefix}…  ⚠ 1 · f2");
            assert!(!line_is_codex_model_first_status(&row), "{row}");
            let pane = format!("\n\x1b[1m›\x1b[0m \x1b[2mA native suggestion\x1b[0m\n\n{row}");
            assert!(pane_looks_ready_for_codex_prompt_with_ansi(&pane), "{row}");
            assert!(submission_readiness_from_ansi_pane(&pane).0, "{row}");
            assert!(
                !pane_looks_ready_for_codex_prompt_with_ansi(&strip_ansi_escape_sequences(&pane)),
                "{row}"
            );
        }
    }

    #[test]
    fn short_auxiliary_footer_does_not_prove_bare_marker_ready() {
        let plain = strip_ansi_escape_sequences(IDLE_46);
        let pane = plain.replace("› Ask Codex to do anything", "›");
        assert!(!pane_looks_ready_for_codex_prompt_with_ansi(&pane));
        assert!(!submission_readiness_from_ansi_pane(&pane).0);
        for legend in [
            "Press a number to choose · esc to dismiss · type to continue",
            "Press a number to choose · esc to\ndismiss · type to continue",
        ] {
            let pane = IDLE_46.replace("\x1b[1m›", &format!("{legend}\n\x1b[1m›"));
            assert!(!pane_looks_ready_for_codex_prompt_with_ansi(&pane));
            assert!(!submission_readiness_from_ansi_pane(&pane).0);
        }
    }

    #[test]
    fn complete_native_dim_placeholder_can_prove_composer_without_footer() {
        let (body, _) = IDLE_46.trim_end().rsplit_once('\n').unwrap();
        assert!(pane_looks_ready_for_codex_prompt_with_ansi(body));
        assert!(submission_readiness_from_ansi_pane(body).0);
        assert!(!pane_looks_ready_for_codex_prompt_with_ansi(
            &strip_ansi_escape_sequences(body)
        ));
        for state in [
            "• Working (2s • esc to interrupt)",
            "Approval required",
            "Sign in to continue",
            "Press a number to choose · esc to dismiss · type to continue",
        ] {
            let blocked = format!("{state}\n{body}");
            assert!(!pane_looks_ready_for_codex_prompt_with_ansi(&blocked));
            assert!(!submission_readiness_from_ansi_pane(&blocked).0);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn canonical_cut_composer_submits_followup_without_kill() {
        use super::super::super::host_input::spy::{SpyGuard, SpyState};
        for (width, pane) in [(46, IDLE_46), (113, CUT_IDLE)] {
            let draft = own_draft(pane);
            let guard = SpyGuard::install(SpyState {
                captures: [pane, &draft, pane, pane]
                    .into_iter()
                    .map(|capture| Some(capture.to_string()))
                    .collect(),
                ..SpyState::default()
            });
            let outcome =
                submit_codex_followup_prompt("cut-composer-fixture", "fixture follow-up", None);
            assert!(
                matches!(outcome, CodexFollowupPromptSubmitOutcome::Submitted),
                "width={width}: {outcome:?}"
            );
            let calls = guard.calls();
            assert_eq!(
                calls
                    .iter()
                    .filter(|call| call.as_str() == "keys:Enter")
                    .count(),
                1
            );
            assert!(!calls.iter().any(|call| call.starts_with("kill")));
        }
    }

    fn own_draft(pane: &str) -> String {
        pane.replace(
            "\x1b[2mAsk Codex to do anything\x1b[0m",
            "fixture follow-up",
        )
        .replace(
            "\x1b[2mUse /skills to list available skills\x1b[0m",
            "fixture follow-up",
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn current_codex_status_fixture_allows_followup_without_kill() {
        use super::super::super::host_input::spy::{SpyGuard, SpyState};
        let pane = include_str!("../../../../tests/fixtures/tui_input/codex-0.160.0-idle.ansi");
        let draft = own_draft(pane);
        let guard = SpyGuard::install(SpyState {
            captures: [pane, pane, pane, &draft, pane]
                .into_iter()
                .map(|p| Some(p.to_string()))
                .collect(),
            ..SpyState::default()
        });
        let session = "composer-current-fixture";
        let snapshot = prompt_readiness_snapshot(session);
        assert!(
            snapshot.composer_marker_detected,
            "actual composer must be ready: {snapshot:?}"
        );
        let ready =
            wait_until_codex_tui_input_ready(session, PromptReadinessKind::PostTurnHandoff, None);
        assert!(ready.is_ok(), "{ready:?}");
        assert!(matches!(
            submit_codex_followup_prompt(session, "fixture follow-up", None),
            CodexFollowupPromptSubmitOutcome::Submitted
        ));
        let calls = guard.calls();
        assert_eq!(
            calls.iter().filter(|c| c.as_str() == "keys:Enter").count(),
            1
        );
        assert_eq!(calls.iter().filter(|c| c.starts_with("kill")).count(), 0);
    }

    #[test]
    fn current_codex_status_fixture_keeps_busy_draft_and_history_guards() {
        let idle = include_str!("../../../../tests/fixtures/tui_input/codex-0.160.0-idle.ansi");
        let draft = include_str!("../../../../tests/fixtures/tui_input/codex-0.160.0-draft.ansi");
        let (marker, has_draft, _) = prompt_readiness_from_ansi_pane(draft);
        assert!(!marker);
        assert!(has_draft);
        for pane in [
            idle.replace("Context 97% left", "Context unknown"),
            idle.replace("Fast off", "slow prose"),
            idle.replace("weekly 99% left", "assistant quota prose"),
            idle.replace("weekly 99% left", "weekly 101% left"),
            idle.replace("weekly 99% left", "weekly +99% left"),
            idle.replace("258K window", "assistant prose"),
            idle.replace("Context 97% left", "Context +97% left"),
            idle.replace("GPT-6.1-Sol xhigh", "assistant prose here"),
            idle.replace("Context 97% left · 258K window", "Context unknown…"),
            idle.replace(
                "weekly 99% left · Fast off · Context 97% left",
                "weekly 99% l…",
            ),
            idle.replace(
                "Worked for 2s • 7:26 AM",
                "• Working (2s • esc to interrupt)",
            ),
            format!(
                "{idle}
new output
new output
new output
new output"
            ),
        ] {
            assert!(!prompt_readiness_from_ansi_pane(&pane).0, "{pane}");
        }
        assert!(!prompt_readiness_from_ansi_pane("Worked for 6s • 6:26 AM").0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn generalized_codex_footer_submits_followup_for_model_and_effort_variants() {
        use super::super::super::host_input::spy::{SpyGuard, SpyState};
        let idle = include_str!("../../../../tests/fixtures/tui_input/codex-0.160.0-idle.ansi");
        for model in [
            "o3 none",
            "codex-next balanced",
            "local/model",
            "custom:tag auto",
            "approval required",
            "Fast off",
            "legacy-fast-first",
        ] {
            let pane = if model == "legacy-fast-first" {
                include_str!("../../../../tests/fixtures/tui_input/codex-idle-dim.ansi").replace(
                    "\x1b[0;1m›",
                    "Approval required: this is answer prose.\n\n\x1b[0;1m›",
                )
            } else {
                idle.replace("GPT-6.1-Sol xhigh", model).replace(
                    "FIXTURE_READY",
                    "Fixture answer\nApproval required: this is answer prose.",
                )
            };
            let plain = strip_ansi_escape_sequences(&pane);
            let separator = if model == "legacy-fast-first" {
                "Approval required: this is answer prose.\n\n›"
            } else {
                "Approval required: this is answer prose.\n\n  Worked for"
            };
            assert!(
                plain.contains(separator),
                "historical prose lost its separator: {model}"
            );
            let draft = own_draft(&pane);
            let guard = SpyGuard::install(SpyState {
                captures: vec![
                    Some(pane.clone()),
                    Some(draft),
                    Some(pane.clone()),
                    Some(pane),
                ]
                .into(),
                ..SpyState::default()
            });
            let session = "composer-generalized-fixture";
            assert!(
                matches!(
                    submit_codex_followup_prompt(session, "fixture follow-up", None),
                    CodexFollowupPromptSubmitOutcome::Submitted
                ),
                "warm follow-up must submit for {model}"
            );
            assert_eq!(
                guard
                    .calls()
                    .iter()
                    .filter(|c| c.as_str() == "keys:Enter")
                    .count(),
                1
            );
            assert_eq!(
                guard
                    .calls()
                    .iter()
                    .filter(|c| c.starts_with("kill"))
                    .count(),
                0
            );
            assert!(
                wait_until_codex_tui_input_ready(
                    session,
                    PromptReadinessKind::PostTurnHandoff,
                    None
                )
                .is_ok()
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn generalized_codex_footer_blocks_submit_during_work_and_approval() {
        use super::super::super::host_input::spy::{SpyGuard, SpyState};
        let idle = include_str!("../../../../tests/fixtures/tui_input/codex-0.160.0-idle.ansi");
        for state in [
            "• Working (2s • esc to interrupt)",
            "Approval required",
            "Allow command",
            "Allow this action",
            "Confirm to continue",
            "Do you trust this folder?",
            "Sign in to continue",
            "Authentication required",
        ] {
            let pane = idle.replace("GPT-6.1-Sol xhigh", "codex-next none");
            let pane = if state == "• Working (2s • esc to interrupt)" {
                pane.replace("Worked for 2s • 7:26 AM", state)
            } else {
                pane.replace("\x1b[1m›\x1b[0m", &format!("{state}\n\x1b[1m›\x1b[0m"))
            };
            for pane in [
                pane.clone(),
                pane.replace(
                    "\x1b[1m›\x1b[0m \x1b[2mAsk Codex to do anything\x1b[0m",
                    "› 1. Yes, proceed (y)",
                ),
            ] {
                let guard = SpyGuard::install(SpyState {
                    captures: vec![Some(pane); 2].into(),
                    ..SpyState::default()
                });
                let outcome = submit_codex_followup_prompt(
                    "composer-blocked-fixture",
                    "fixture follow-up",
                    None,
                );
                assert!(
                    !guard
                        .calls()
                        .iter()
                        .any(|c| c == "keys:Enter" || c.starts_with("literal:")),
                    "blocked composer must receive no input: {state}"
                );
                assert!(
                    matches!(
                        outcome,
                        CodexFollowupPromptSubmitOutcome::NotSubmitted { .. }
                    ),
                    "blocked composer must not submit: {state}"
                );
            }
        }
    }
}
