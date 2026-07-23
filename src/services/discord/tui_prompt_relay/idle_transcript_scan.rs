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
        entry_id: Option<String>,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ClaudeIdleTranscriptScan {
    NoPrompt {
        offset: u64,
    },
    CompactionReanchor {
        offset: u64,
    },
    Prompt {
        prompt: String,
        prompt_start_offset: u64,
        line_end_offset: u64,
        /// #3540: the scanned `user` entry's STABLE identity (`uuid`), when the
        /// transcript provides one. Threaded into the dedupe layer so an
        /// already-relayed entry re-encountered after a watermark reset / jsonl
        /// head rotation is suppressed by identity (not by the 30s content
        /// window), preventing a phantom synthetic inflight. `None` falls back to
        /// the content-keyed recent-observed dedup (pre-#3540 behavior).
        entry_id: Option<String>,
    },
}

pub(super) fn claude_idle_compaction_reanchor(
    path_changed: bool,
    binding_offset: u64,
    current_eof: u64,
    durable_frontier_exceeds_eof: bool,
) -> Option<ClaudeIdleTranscriptScan> {
    // A real Claude session rotation changes the UUID/path and is handled by the
    // bounded newest-prompt lookback. Only an unchanged-path, same-generation
    // durable frontier beyond EOF proves an in-place `/compact` rewrite whose
    // surviving bytes are historical and must be treated as already delivered.
    (!path_changed && binding_offset > current_eof && durable_frontier_exceeds_eof).then_some(
        ClaudeIdleTranscriptScan::CompactionReanchor {
            offset: current_eof,
        },
    )
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
        if let Some((prompt, entry_id)) =
            crate::services::tui_prompt_dedupe::extract_claude_transcript_user_prompt_with_entry_id(
                &json,
            )
        {
            return Ok(ClaudeIdleTranscriptScan::Prompt {
                prompt,
                prompt_start_offset: line_start_offset,
                line_end_offset: offset,
                entry_id,
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
        if let Some((prompt, entry_id)) =
            crate::services::tui_prompt_dedupe::extract_claude_transcript_user_prompt_with_entry_id(
                &json,
            )
        {
            last_prompt = Some(ClaudeIdleTranscriptScan::Prompt {
                prompt,
                prompt_start_offset: line_start_offset,
                line_end_offset: offset,
                entry_id,
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
        if let Some((prompt, entry_id)) =
            crate::services::tui_prompt_dedupe::extract_codex_rollout_user_prompt_with_entry_id(
                &json,
            )
        {
            return Ok(CodexIdleRolloutScan::Prompt {
                prompt,
                line_end_offset: offset,
                entry_id,
            });
        }
    }
}

pub(super) fn scan_codex_idle_rollout_for_latest_prompt_matching(
    rollout_path: &Path,
    prompt_text: &str,
) -> Result<Option<CodexIdleRolloutScan>, String> {
    let target = prompt_text.trim();
    if target.is_empty() {
        return Ok(None);
    }
    let file = std::fs::File::open(rollout_path)
        .map_err(|error| format!("open Codex rollout {}: {error}", rollout_path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let mut offset = 0_u64;
    let mut line = String::new();
    let mut latest = None;

    loop {
        line.clear();
        let line_start_offset = offset;
        let bytes_read = reader
            .read_line(&mut line)
            .map_err(|error| format!("read Codex rollout {}: {error}", rollout_path.display()))?;
        if bytes_read == 0 {
            return Ok(latest);
        }
        offset = offset.saturating_add(bytes_read as u64);
        let Ok(json) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            if !line.ends_with('\n') {
                return Ok(latest);
            }
            continue;
        };
        if let Some((prompt, entry_id)) =
            crate::services::tui_prompt_dedupe::extract_codex_rollout_user_prompt_with_entry_id(
                &json,
            )
            && prompt.trim() == target
        {
            latest = Some(CodexIdleRolloutScan::Prompt {
                prompt,
                line_end_offset: offset,
                entry_id,
            });
        } else if !line.ends_with('\n') {
            return Ok(latest);
        }
        if bytes_read == 0 || line_start_offset == offset {
            return Ok(latest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_idle_rollout_scan_finds_user_prompt_and_stops_at_prompt_end() {
        let dir = tempfile::tempdir().expect("temp dir");
        let rollout = dir.path().join("rollout.jsonl");
        let before = "{\"type\":\"session_meta\",\"payload\":{\"id\":\"s1\"}}\n";
        let prompt = "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"direct prompt\"}]}}\n";
        let after = "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"answer\"}]}}\n";
        std::fs::write(&rollout, format!("{before}{prompt}{after}")).expect("write rollout");

        assert_eq!(
            scan_codex_idle_rollout_for_prompt(&rollout, 0).expect("scan"),
            CodexIdleRolloutScan::Prompt {
                prompt: "direct prompt".to_string(),
                line_end_offset: (before.len() + prompt.len()) as u64,
                entry_id: None,
            }
        );
        assert_eq!(
            scan_codex_idle_rollout_for_prompt(&rollout, (before.len() + prompt.len()) as u64)
                .expect("scan after prompt"),
            CodexIdleRolloutScan::NoPrompt {
                offset: (before.len() + prompt.len() + after.len()) as u64,
            }
        );
    }

    #[test]
    fn codex_idle_rollout_scan_preserves_partial_trailing_jsonl() {
        let dir = tempfile::tempdir().expect("temp dir");
        let rollout = dir.path().join("rollout.jsonl");
        let complete = "{\"type\":\"session_meta\",\"payload\":{\"id\":\"s1\"}}\n";
        let partial =
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\"";
        std::fs::write(&rollout, format!("{complete}{partial}")).expect("write rollout");

        assert_eq!(
            scan_codex_idle_rollout_for_prompt(&rollout, 0).expect("scan partial"),
            CodexIdleRolloutScan::NoPrompt {
                offset: complete.len() as u64,
            }
        );
    }

    #[test]
    fn codex_idle_rollout_scan_restarts_when_file_shrinks() {
        let dir = tempfile::tempdir().expect("temp dir");
        let rollout = dir.path().join("rollout.jsonl");
        let prompt = "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"after shrink\"}]}}\n";
        std::fs::write(&rollout, prompt).expect("write rollout");

        assert_eq!(
            scan_codex_idle_rollout_for_prompt(&rollout, 99_999).expect("scan shrunken"),
            CodexIdleRolloutScan::Prompt {
                prompt: "after shrink".to_string(),
                line_end_offset: prompt.len() as u64,
                entry_id: None,
            }
        );
    }

    #[test]
    fn codex_idle_rollout_scan_threads_entry_id_into_replay_dedupe() {
        let _guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap();
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
        let dir = tempfile::tempdir().expect("temp dir");
        let rollout = dir.path().join("rollout.jsonl");
        let entry_id = "codex-user-entry-3676";
        let prompt = format!(
            "{{\"type\":\"response_item\",\"payload\":{{\"id\":\"{entry_id}\",\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"direct codex prompt\"}}]}}}}\n"
        );
        std::fs::write(&rollout, &prompt).expect("write rollout");

        let (text, line_end_offset, scanned_entry_id) =
            match scan_codex_idle_rollout_for_prompt(&rollout, 0).expect("scan") {
                CodexIdleRolloutScan::Prompt {
                    prompt,
                    line_end_offset,
                    entry_id,
                } => (prompt, line_end_offset, entry_id),
                other => panic!("expected Codex prompt, got {other:?}"),
            };
        assert_eq!(text, "direct codex prompt");
        assert_eq!(line_end_offset, prompt.len() as u64);
        assert_eq!(scanned_entry_id.as_deref(), Some(entry_id));

        let first = crate::services::tui_prompt_dedupe::observe_prompt_by_tmux_with_entry_id_at(
            crate::services::provider::ProviderKind::Codex.as_str(),
            "AgentDesk-codex-entry-id",
            &text,
            scanned_entry_id.as_deref(),
            chrono::Utc::now(),
        );
        assert_eq!(
            first,
            crate::services::tui_prompt_dedupe::PromptObservation::PublishedSshDirect
        );

        let (re_prompt, re_entry_id) =
            match scan_codex_idle_rollout_for_prompt(&rollout, 0).expect("rescan") {
                CodexIdleRolloutScan::Prompt {
                    prompt, entry_id, ..
                } => (prompt, entry_id),
                other => panic!("expected rescan prompt, got {other:?}"),
            };
        assert_eq!(re_prompt, text);
        assert_eq!(re_entry_id.as_deref(), Some(entry_id));
        let second = crate::services::tui_prompt_dedupe::observe_prompt_by_tmux_with_entry_id_at(
            crate::services::provider::ProviderKind::Codex.as_str(),
            "AgentDesk-codex-entry-id",
            &re_prompt,
            re_entry_id.as_deref(),
            chrono::Utc::now(),
        );
        assert_eq!(
            second,
            crate::services::tui_prompt_dedupe::PromptObservation::SuppressedReplayedEntry
        );
    }

    #[test]
    fn codex_idle_rollout_latest_matching_prompt_uses_last_complete_match() {
        let dir = tempfile::tempdir().expect("temp dir");
        let rollout = dir.path().join("rollout.jsonl");
        let first = "{\"type\":\"response_item\",\"payload\":{\"id\":\"first\",\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"same prompt\"}]}}\n";
        let middle = "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"old answer\"}]}}\n";
        let second = "{\"type\":\"response_item\",\"payload\":{\"id\":\"second\",\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"same prompt\"}]}}\n";
        let partial =
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\"";
        std::fs::write(&rollout, format!("{first}{middle}{second}{partial}"))
            .expect("write rollout");

        assert_eq!(
            scan_codex_idle_rollout_for_latest_prompt_matching(&rollout, "same prompt")
                .expect("scan latest"),
            Some(CodexIdleRolloutScan::Prompt {
                prompt: "same prompt".to_string(),
                line_end_offset: (first.len() + middle.len() + second.len()) as u64,
                entry_id: Some("second".to_string()),
            })
        );
    }
}
