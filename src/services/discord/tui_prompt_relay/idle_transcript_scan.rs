//! #3479 rank-10: the PURE transcript/rollout scanners that the Claude/Codex
//! idle relay loops use to find the next (or newest) operator-typed prompt.
//!
//! These functions are byte-stream parsers with no Discord-IO or `SharedData`
//! coupling — they only read a transcript/rollout file and delegate prompt
//! extraction to `crate::services::tui_prompt_dedupe`. They live in a capped
//! sibling module; the scan-result enums and the helpers are re-imported by the
//! parent so the stateful idle-relay call sites stay byte-identical.

use super::*;

pub(super) fn codex_idle_prompt_observation_should_tail_response(
    observation: crate::services::tui_prompt_dedupe::PromptObservation,
) -> bool {
    // The turn bridge owns Discord-originated Codex prompts. The idle rollout
    // relay is only for text typed directly into the Codex TUI; tailing
    // suppressed Discord/recent duplicates can replay stale prior-turn output
    // after a newer Discord message has already started.
    matches!(
        observation,
        crate::services::tui_prompt_dedupe::PromptObservation::PublishedSshDirect
    )
}

pub(super) fn claude_idle_prompt_observation_should_tail_response(
    observation: crate::services::tui_prompt_dedupe::PromptObservation,
) -> bool {
    // The turn bridge owns Discord-originated prompts. Claude's idle tail is
    // only a recovery path for operator text typed directly into the TUI; if
    // we tail suppressed Discord/recent duplicates here, the bridge-delivered
    // answer is posted a second time after inflight clears.
    matches!(
        observation,
        crate::services::tui_prompt_dedupe::PromptObservation::PublishedSshDirect
    )
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum CodexIdleRolloutScan {
    NoPrompt {
        offset: u64,
    },
    Prompt {
        prompt: String,
        line_end_offset: u64,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ClaudeIdleTranscriptScan {
    NoPrompt {
        offset: u64,
    },
    Prompt {
        prompt: String,
        prompt_start_offset: u64,
        line_end_offset: u64,
    },
}

pub(super) fn scan_claude_idle_transcript_for_prompt(
    transcript_path: &Path,
    start_offset: u64,
) -> Result<ClaudeIdleTranscriptScan, String> {
    let mut file = std::fs::File::open(transcript_path).map_err(|error| {
        format!(
            "open Claude transcript {}: {error}",
            transcript_path.display()
        )
    })?;
    let file_len = file
        .metadata()
        .map_err(|error| {
            format!(
                "stat Claude transcript {}: {error}",
                transcript_path.display()
            )
        })?
        .len();
    let mut offset = if start_offset > file_len {
        0
    } else {
        start_offset
    };
    file.seek(SeekFrom::Start(offset)).map_err(|error| {
        format!(
            "seek Claude transcript {}: {error}",
            transcript_path.display()
        )
    })?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();

    loop {
        line.clear();
        let line_start_offset = offset;
        let bytes_read = reader.read_line(&mut line).map_err(|error| {
            format!(
                "read Claude transcript {}: {error}",
                transcript_path.display()
            )
        })?;
        if bytes_read == 0 {
            return Ok(ClaudeIdleTranscriptScan::NoPrompt { offset });
        }
        offset = offset.saturating_add(bytes_read as u64);
        let Ok(json) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            if !line.ends_with('\n') {
                return Ok(ClaudeIdleTranscriptScan::NoPrompt {
                    offset: line_start_offset,
                });
            }
            continue;
        };
        if let Some(prompt) =
            crate::services::tui_prompt_dedupe::extract_claude_transcript_user_prompt(&json)
        {
            return Ok(ClaudeIdleTranscriptScan::Prompt {
                prompt,
                prompt_start_offset: line_start_offset,
                line_end_offset: offset,
            });
        }
    }
}

/// #2843 (codex round-2 P1): scan `[start_offset, EOF)` and return the LAST
/// (newest, closest to EOF) user prompt rather than the first.
///
/// The path-change lookback reads a bounded byte window that can contain
/// several already-finished turns. Selecting the first prompt would re-relay an
/// old turn (`observe_prompt_by_tmux` only suppresses pending Discord prompts or
/// recent duplicates, so an older prompt inside the window is misclassified as
/// SSH-direct and tailed again). The just-typed prompt is always the newest
/// entry in the window, so returning the last prompt catches the current turn
/// without replaying stale backlog. Incremental tailing on an unchanged path
/// keeps first-prompt semantics via [`scan_claude_idle_transcript_for_prompt`].
pub(super) fn scan_claude_idle_transcript_for_last_prompt(
    transcript_path: &Path,
    start_offset: u64,
) -> Result<ClaudeIdleTranscriptScan, String> {
    let mut file = std::fs::File::open(transcript_path).map_err(|error| {
        format!(
            "open Claude transcript {}: {error}",
            transcript_path.display()
        )
    })?;
    let file_len = file
        .metadata()
        .map_err(|error| {
            format!(
                "stat Claude transcript {}: {error}",
                transcript_path.display()
            )
        })?
        .len();
    let mut offset = if start_offset > file_len {
        0
    } else {
        start_offset
    };
    file.seek(SeekFrom::Start(offset)).map_err(|error| {
        format!(
            "seek Claude transcript {}: {error}",
            transcript_path.display()
        )
    })?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    let mut last_prompt: Option<ClaudeIdleTranscriptScan> = None;

    loop {
        line.clear();
        let line_start_offset = offset;
        let bytes_read = reader.read_line(&mut line).map_err(|error| {
            format!(
                "read Claude transcript {}: {error}",
                transcript_path.display()
            )
        })?;
        if bytes_read == 0 {
            return Ok(last_prompt.unwrap_or(ClaudeIdleTranscriptScan::NoPrompt { offset }));
        }
        offset = offset.saturating_add(bytes_read as u64);
        let Ok(json) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            if !line.ends_with('\n') {
                // Partial trailing line: stop before consuming it. Return the
                // newest COMPLETE prompt found so far; otherwise leave the cursor
                // at the partial line so the next tick re-reads it once complete.
                //
                // #2843 (codex round-3/round-4): deferring here — returning the
                // scan start so a later tick re-picks the newest prompt once the
                // partial completes — is NOT viable: `resolve_idle_relay_transcript`
                // re-registers the binding to the fresh path with `last_offset`
                // pinned at EOF BEFORE this scan runs, so the next tick has
                // `path_changed == false` and the first-prompt scanner starts at
                // that pinned EOF, dropping the deferred (current) turn entirely.
                // Returning the last complete prompt instead never drops the
                // current turn: the relayed prompt advances the cursor to its
                // own line end, and any prompt written after it (e.g. one that
                // was mid-write this tick) is caught on the next tick by the
                // unchanged-path first-prompt scanner.
                //
                // Residual: if the freshly-resolved transcript is one we already
                // relayed earlier and then returned to (multi-session mtime
                // flip-back) AND its just-typed prompt is mid-write at scan time,
                // the last complete prompt can be an already-relayed older turn,
                // re-surfaced once (bounded by the 30s recent-duplicate dedup in
                // observe_prompt_by_tmux). Distinguishing that from the dominant
                // single-session case ([prompt][its streaming response]) needs
                // per-transcript relayed-offset memory, which is the relay
                // delivery-lease / cursor-unification consolidation, not #2843.
                return Ok(last_prompt.unwrap_or(ClaudeIdleTranscriptScan::NoPrompt {
                    offset: line_start_offset,
                }));
            }
            continue;
        };
        if let Some(prompt) =
            crate::services::tui_prompt_dedupe::extract_claude_transcript_user_prompt(&json)
        {
            last_prompt = Some(ClaudeIdleTranscriptScan::Prompt {
                prompt,
                prompt_start_offset: line_start_offset,
                line_end_offset: offset,
            });
        }
    }
}

pub(super) fn scan_codex_idle_rollout_for_prompt(
    rollout_path: &Path,
    start_offset: u64,
) -> Result<CodexIdleRolloutScan, String> {
    let mut file = std::fs::File::open(rollout_path)
        .map_err(|error| format!("open Codex rollout {}: {error}", rollout_path.display()))?;
    let file_len = file
        .metadata()
        .map_err(|error| format!("stat Codex rollout {}: {error}", rollout_path.display()))?
        .len();
    let mut offset = if start_offset > file_len {
        0
    } else {
        start_offset
    };
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| format!("seek Codex rollout {}: {error}", rollout_path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();

    loop {
        line.clear();
        let line_start_offset = offset;
        let bytes_read = reader
            .read_line(&mut line)
            .map_err(|error| format!("read Codex rollout {}: {error}", rollout_path.display()))?;
        if bytes_read == 0 {
            return Ok(CodexIdleRolloutScan::NoPrompt { offset });
        }
        offset = offset.saturating_add(bytes_read as u64);
        let Ok(json) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            if !line.ends_with('\n') {
                return Ok(CodexIdleRolloutScan::NoPrompt {
                    offset: line_start_offset,
                });
            }
            continue;
        };
        if let Some(prompt) =
            crate::services::tui_prompt_dedupe::extract_codex_rollout_user_prompt(&json)
        {
            return Ok(CodexIdleRolloutScan::Prompt {
                prompt,
                line_end_offset: offset,
            });
        }
    }
}
