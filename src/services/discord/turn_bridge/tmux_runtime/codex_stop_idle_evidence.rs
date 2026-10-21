//! What a Herdr Claude stop read and judged when it refused as idle, kept for one log line that is
//! written only after the composer lock is released.

use sha2::{Digest, Sha256};

use super::super::claude_stop_pane as pane;
use super::classify_tui_interrupt_phase;
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::tmux_common as screen_state;
use crate::services::tui_prompt_dedupe::TuiRuntimeBinding;
use crate::services::tui_turn_state::TuiTurnState;

const TAIL_ABOVE: usize = 8;
const TAIL_BELOW: usize = 3;
const TAIL_WITHOUT_COMPOSER: usize = 12;
const LINE_CHARS: usize = 160;
const TAIL_BYTES: usize = 2048;
const RUN_COLLAPSE: usize = 8;

/// The refusing stop's own `(structured, ready, active, draft)`, never re-read.
pub(super) type Judged = (Option<TuiTurnState>, bool, bool, bool);

pub(super) struct IdleEvidence {
    channel: u64,
    provider: String,
    logical: String,
    turn_nonce: Option<String>,
    execution_nonce: String,
    runtime_generation: u64,
    runtime_kind: &'static str,
    judged: Judged,
    modal: bool,
    veto: bool,
    legacy_active: bool,
    foreground_tool_running: bool,
    exact_empty_composer: bool,
    phase: &'static str,
    screen_bytes: usize,
    screen_lines: usize,
    screen_sha: String,
    sources_same: bool,
    tail: Vec<String>,
    below_composer_lines: Option<usize>,
}

impl IdleEvidence {
    /// Pure work on values the stop already holds: `at` is `(channel, logical, nonce)` and
    /// `sources` the identity binding and the binding re-read under the lock.
    pub(super) fn new(
        token: &CancelToken,
        provider: &ProviderKind,
        at: (u64, &str, &str),
        sources: (&TuiRuntimeBinding, &TuiRuntimeBinding),
        judged: Judged,
        screen: &str,
    ) -> Self {
        let (structured, ready, active, draft) = judged;
        let (tail, below_composer_lines) = tail(screen);
        Self {
            channel: at.0,
            provider: provider.as_str().to_owned(),
            logical: at.1.to_owned(),
            turn_nonce: token.turn_nonce().map(str::to_owned),
            execution_nonce: at.2.to_owned(),
            runtime_generation: token.claude_interrupt_generation(),
            runtime_kind: sources.1.runtime_kind.as_str(),
            judged,
            modal: screen_state::tmux_capture_indicates_claude_tui_interactive_modal(screen),
            veto: pane::veto(screen),
            legacy_active: pane::legacy_evidence(screen).1,
            foreground_tool_running: pane::foreground_tool_running(screen),
            exact_empty_composer:
                screen_state::tmux_capture_indicates_claude_tui_exact_empty_composer(screen),
            phase: classify_tui_interrupt_phase(structured, ready || draft, active).as_str(),
            screen_bytes: screen.len(),
            screen_lines: screen.lines().count(),
            screen_sha: sha_hex(screen)[..12].to_owned(),
            sources_same: sources.0.output_path == sources.1.relay_output_path(),
            tail,
            below_composer_lines,
        }
    }

    pub(super) fn emit(&self) {
        let (structured, ready, active, draft) = self.judged;
        tracing::info!(
            event = "herdr_stop_idle_evidence",
            channel_id = self.channel,
            provider = self.provider.as_str(),
            logical = self.logical.as_str(),
            turn_nonce = self.turn_nonce.as_deref(),
            execution_nonce = self.execution_nonce.as_str(),
            runtime_generation = self.runtime_generation,
            runtime_kind = self.runtime_kind,
            structured = ?structured,
            ready,
            active,
            draft,
            modal = self.modal,
            veto = self.veto,
            legacy_active = self.legacy_active,
            foreground_tool_running = self.foreground_tool_running,
            exact_empty_composer = self.exact_empty_composer,
            phase = self.phase,
            screen_bytes = self.screen_bytes,
            screen_lines = self.screen_lines,
            screen_sha = self.screen_sha.as_str(),
            truncated = false,
            sources_same = self.sources_same,
            tail = ?self.tail,
            tail_lines = self.tail.len(),
            below_composer_lines = self.below_composer_lines,
            "herdr user stop idle evidence"
        );
    }
}

fn sha_hex(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// Trimmed as `tmux_common` reads a Claude prompt line: whitespace and NBSP at both ends.
fn trim(line: &str) -> &str {
    line.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{00a0}')
}

/// A prompt line keeps its marker and only the length and hash of what follows it.
fn masked(marker: Option<char>, content: &str) -> String {
    let lead = marker
        .map(|marker| format!("{marker} "))
        .unwrap_or_default();
    match content.is_empty() {
        true => lead.trim_end().to_owned(),
        false => format!(
            "{lead}<masked chars={} sha={}>",
            content.chars().count(),
            &sha_hex(content)[..8]
        ),
    }
}

/// Non-empty rows without escapes or control characters, each with whether it is a `❯` row.
/// `❯`, `>` and bash-mode `!` rows and their indented continuation are user input, masked.
fn rows(screen: &str) -> Vec<(bool, String)> {
    let mut in_prompt = false;
    screen
        .lines()
        .filter_map(|line| {
            let line: String = crate::services::codex_tui::input::strip_ansi_escape_sequences(line)
                .chars()
                .filter(|ch| !ch.is_control())
                .collect();
            let text = trim(&line);
            let marker = text
                .chars()
                .next()
                .filter(|ch| matches!(ch, '❯' | '>' | '!'));
            let continued = in_prompt
                && line.starts_with(char::is_whitespace)
                && !text.chars().all(|ch| ch == '─')
                && !text.starts_with(['⎿', '⏺']);
            in_prompt = !text.is_empty() && (marker.is_some() || continued);
            let row = match marker {
                _ if text.is_empty() => return None,
                Some(marker) => masked(Some(marker), trim(&text[marker.len_utf8()..])),
                None if continued => masked(None, text),
                None => text.to_owned(),
            };
            Some((marker == Some('❯'), row))
        })
        .collect()
}

/// Redacted, runs of one character folded, at most `LINE_CHARS` characters.
fn shape(row: &str) -> String {
    let row = crate::logging::redact_log_text(row);
    let mut folded = String::with_capacity(row.len());
    let mut chars = row.chars().peekable();
    while let Some(ch) = chars.next() {
        let mut run = 1;
        while chars.next_if_eq(&ch).is_some() {
            run += 1;
        }
        match run >= RUN_COLLAPSE {
            true => folded.push_str(&format!("{ch}×{run}")),
            false => folded.extend(std::iter::repeat_n(ch, run)),
        }
    }
    match folded.chars().count() > LINE_CHARS {
        true => folded.chars().take(LINE_CHARS - 1).chain(['…']).collect(),
        false => folded,
    }
}

/// The bottom composer, the rows above and below it nearest first, within `TAIL_BYTES`; with no
/// composer the last rows. Also the count of non-empty rows below the composer.
pub(super) fn tail(screen: &str) -> (Vec<String>, Option<usize>) {
    let rows = rows(screen);
    let composer = rows.iter().rposition(|(composer, _)| *composer);
    let order: Vec<usize> = match composer {
        Some(at) => std::iter::once(at)
            .chain((at.saturating_sub(TAIL_ABOVE)..at).rev())
            .chain(at + 1..rows.len().min(at + 1 + TAIL_BELOW))
            .collect(),
        None => (rows.len().saturating_sub(TAIL_WITHOUT_COMPOSER)..rows.len())
            .rev()
            .collect(),
    };
    let (mut budget, mut kept) = (TAIL_BYTES, Vec::new());
    for index in order {
        let line = shape(&rows[index].1);
        if line.len() > budget {
            break;
        }
        budget -= line.len();
        kept.push((index, line));
    }
    kept.sort_by_key(|(index, _)| *index);
    let below = composer.map(|at| rows.len() - at - 1);
    (kept.into_iter().map(|(_, line)| line).collect(), below)
}

#[cfg(test)]
#[path = "codex_stop_idle_evidence_tests.rs"]
mod tests;
