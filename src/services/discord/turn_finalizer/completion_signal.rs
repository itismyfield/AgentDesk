//! #3479 r9 — structural completion-signal split out of `turn_finalizer.rs`.
//!
//! PURE MOVE (no logic change): the `CompletionSignal` enum and the pure
//! `completion_signal_from_transcript()` derivation, plus their unit tests.
//! The parent re-exports both (`use self::completion_signal::{...}`) so the
//! `completion_signal_state` method, the watcher-backstop re-check, and the
//! existing tests all reference them byte-identically.

use super::*;

/// #3016 S1: the structural completion signal from the provider's JSONL
/// transcript, independent of the ledger.
/// - `Done` — strict reverse-scan found a definitive terminator (Claude
///   `result`/`system{...}`, Codex `turn.completed`): structurally over.
/// - `PausedLive` — in-flight or inconclusive evidence; conservatively a live,
///   paused turn.
/// - `Unknown` — no structured on-disk JSONL turn state (LegacyTmuxWrapper /
///   ProcessBackend / ClaudeEAdapter, or a non-JSONL provider).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
// #3016 S3: wired into the watcher fresh-idle finalize decision.
pub(in crate::services::discord) enum CompletionSignal {
    Done,
    PausedLive,
    Unknown,
}

/// Acceptance mode for the shared JSONL completion scan.
///
/// `FinalizeAuthority` is the strict turn-finalizer authority and accepts only
/// definitive turn-end terminators as `Done`. `GateQuiescence` is a temporary
/// S2-a compatibility mode for the watcher completion gate; it preserves the
/// pre-S2-a observer-style gate acceptance semantics byte-for-byte, including
/// the source-agnostic matched-session JSONL shortcut. S2-b should delete
/// `GateQuiescence` together with the completion-gate `TimedOut` suppression.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::services::discord) enum CompletionSignalMode {
    FinalizeAuthority,
    GateQuiescence,
}

/// #3016 S1 (PURE) — the structural completion signal derived solely from the
/// provider's on-disk JSONL transcript. Shared by the public
/// `completion_signal_state` (see it for the Done/PausedLive/Unknown
/// rationale) and the reconciler's watcher-backstop liveness re-check.
pub(in crate::services::discord) fn completion_signal_from_transcript(
    provider: &ProviderKind,
    runtime_kind: Option<crate::services::agent_protocol::RuntimeHandoffKind>,
    transcript_path: &std::path::Path,
) -> CompletionSignal {
    completion_signal_from_transcript_with_mode(
        provider,
        runtime_kind,
        transcript_path,
        CompletionSignalMode::FinalizeAuthority,
    )
}

pub(in crate::services::discord) fn completion_signal_from_transcript_with_mode(
    provider: &ProviderKind,
    runtime_kind: Option<crate::services::agent_protocol::RuntimeHandoffKind>,
    transcript_path: &std::path::Path,
    mode: CompletionSignalMode,
) -> CompletionSignal {
    let structured_jsonl_available = match mode {
        CompletionSignalMode::FinalizeAuthority => {
            crate::services::tui_turn_state::provider_runtime_has_structured_jsonl_turn_state(
                provider,
                runtime_kind,
            )
        }
        CompletionSignalMode::GateQuiescence => {
            matches!(provider, ProviderKind::Claude | ProviderKind::Codex)
        }
    };
    if !structured_jsonl_available {
        return CompletionSignal::Unknown;
    }
    let Ok(metadata) = std::fs::metadata(transcript_path) else {
        return CompletionSignal::PausedLive;
    };
    if !metadata.is_file() || metadata.len() == 0 {
        return CompletionSignal::PausedLive;
    }
    let scan_mode = match mode {
        CompletionSignalMode::FinalizeAuthority => {
            crate::services::tui_turn_state::JsonlCompletionScanMode::FinalizeAuthority
        }
        CompletionSignalMode::GateQuiescence => {
            crate::services::tui_turn_state::JsonlCompletionScanMode::GateQuiescence
        }
    };
    if crate::services::tui_turn_state::jsonl_completion_scan_idle(
        provider,
        transcript_path,
        scan_mode,
    ) {
        CompletionSignal::Done
    } else {
        CompletionSignal::PausedLive
    }
}

#[cfg(test)]
mod tests {
    // =======================================================================
    // #3016 S1 — read-only completion-signal probes (#3479 r9: moved verbatim
    // from the parent test module; they exercise the `completion_signal_state`
    // method on `super::TurnFinalizer`, which delegates to the pure
    // `completion_signal_from_transcript` above).
    // Additive, dead until S3/S4; these prove the read-only contract now.
    // =======================================================================
    use super::*;
    use crate::services::agent_protocol::RuntimeHandoffKind;

    fn write_transcript(lines: &[&str]) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), lines.join("\n")).unwrap();
        file
    }

    // (a) Claude transcript ending in a real terminator → Done.
    #[test]
    fn completion_signal_claude_terminator_is_done() {
        let fin = TurnFinalizer::spawn();
        let file = write_transcript(&[
            r#"{"type":"user","message":{"content":"hi"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}"#,
            r#"{"type":"result","result":"done","session_id":"s"}"#,
        ]);
        assert_eq!(
            fin.completion_signal_state(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                file.path(),
            ),
            CompletionSignal::Done,
        );
    }

    // (b) Claude transcript still streaming (no terminator) → PausedLive.
    #[test]
    fn completion_signal_claude_streaming_is_paused_live() {
        let fin = TurnFinalizer::spawn();
        let file = write_transcript(&[
            r#"{"type":"user","message":{"content":"hi"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working"}]}}"#,
        ]);
        assert_eq!(
            fin.completion_signal_state(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                file.path(),
            ),
            CompletionSignal::PausedLive,
        );
    }

    // (b) Claude transcript whose latest line is a partial selector fragment
    // after a terminator (a just-restarted turn) → PausedLive (the strict scan
    // refuses to fall through a partial new envelope).
    #[test]
    fn completion_signal_claude_partial_after_terminator_is_paused_live() {
        let fin = TurnFinalizer::spawn();
        let file = write_transcript(&[
            r#"{"type":"result","result":"done","session_id":"s"}"#,
            r#"{"ty"#,
        ]);
        assert_eq!(
            fin.completion_signal_state(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                file.path(),
            ),
            CompletionSignal::PausedLive,
        );
    }

    // (a) Codex transcript ending in `turn.completed` → Done.
    #[test]
    fn completion_signal_codex_terminator_is_done() {
        let fin = TurnFinalizer::spawn();
        let file = write_transcript(&[
            r#"{"type":"session_meta","payload":{"id":"s","cwd":"/repo"}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":5,"output_tokens":3}}"#,
        ]);
        assert_eq!(
            fin.completion_signal_state(
                &ProviderKind::Codex,
                Some(RuntimeHandoffKind::CodexTui),
                file.path(),
            ),
            CompletionSignal::Done,
        );
    }

    // (b) Codex transcript mid-tool-call (no terminator) → PausedLive.
    #[test]
    fn completion_signal_codex_inflight_is_paused_live() {
        let fin = TurnFinalizer::spawn();
        let file = write_transcript(&[
            r#"{"type":"session_meta","payload":{"id":"s","cwd":"/repo"}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call","name":"run_cmd","arguments":"{}","call_id":"c1"}}"#,
        ]);
        assert_eq!(
            fin.completion_signal_state(
                &ProviderKind::Codex,
                Some(RuntimeHandoffKind::CodexTui),
                file.path(),
            ),
            CompletionSignal::PausedLive,
        );
    }

    // #3016 S3 (Concern 1): a COMPLETED Codex `agent_message` written right
    // before a tool call is MID-TURN — the turn has not ended. The lenient drain
    // probe would call this Idle, but the finalize `Done` decision uses the
    // turn-END-only probe, so it must resolve to PausedLive (NOT Done) and the
    // watcher therefore CANNOT over-finalize the live turn.
    #[test]
    fn completion_signal_codex_completed_agent_message_is_paused_live_not_done() {
        let fin = TurnFinalizer::spawn();
        let file = write_transcript(&[
            r#"{"type":"session_meta","payload":{"id":"s","cwd":"/repo"}}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"on it, running a tool next"}}"#,
        ]);
        assert_eq!(
            fin.completion_signal_state(
                &ProviderKind::Codex,
                Some(RuntimeHandoffKind::CodexTui),
                file.path(),
            ),
            CompletionSignal::PausedLive,
            "a completed agent_message with no turn.completed is mid-turn → not Done",
        );
    }

    // #3016 S3 (Concern 1): a Codex `event_msg{task_complete}` (a task signal,
    // not the turn record) is likewise NOT the turn terminator → PausedLive.
    #[test]
    fn completion_signal_codex_task_complete_is_paused_live_not_done() {
        let fin = TurnFinalizer::spawn();
        let file = write_transcript(&[
            r#"{"type":"session_meta","payload":{"id":"s","cwd":"/repo"}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#,
        ]);
        assert_eq!(
            fin.completion_signal_state(
                &ProviderKind::Codex,
                Some(RuntimeHandoffKind::CodexTui),
                file.path(),
            ),
            CompletionSignal::PausedLive,
        );
    }

    // #3016 S3 (Concern 1): a Claude mid-turn assistant message (no terminator)
    // → PausedLive; and a Claude `system{init}` (session-start, not turn-end) is
    // at-rest to the drain probe but must NOT be Done for the finalize decision.
    #[test]
    fn completion_signal_claude_init_and_mid_turn_are_paused_live_not_done() {
        let fin = TurnFinalizer::spawn();
        let mid_turn = write_transcript(&[
            r#"{"type":"user","message":{"content":"hi"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"thinking"}]}}"#,
        ]);
        assert_eq!(
            fin.completion_signal_state(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                mid_turn.path(),
            ),
            CompletionSignal::PausedLive,
        );

        let init_only =
            write_transcript(&[r#"{"type":"system","subtype":"init","session_id":"s"}"#]);
        assert_eq!(
            fin.completion_signal_state(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                init_only.path(),
            ),
            CompletionSignal::PausedLive,
            "system{{init}} is a session-start marker, not a turn-end terminator → not Done",
        );
    }

    // #3016 S3 (Concern 1): a Claude `system{turn_duration}` IS a real turn-end
    // terminator → Done (the stricter probe still accepts the genuine
    // system-family turn boundary, not only `result`).
    #[test]
    fn completion_signal_claude_turn_duration_is_done() {
        let fin = TurnFinalizer::spawn();
        let file = write_transcript(&[
            r#"{"type":"user","message":{"content":"hi"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}"#,
            r#"{"type":"system","subtype":"turn_duration","session_id":"s"}"#,
        ]);
        assert_eq!(
            fin.completion_signal_state(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                file.path(),
            ),
            CompletionSignal::Done,
        );
    }

    // (c) Non-JSONL runtime (LegacyTmuxWrapper) → Unknown even with a terminator
    // on disk: the probe must not speak to completion for a runtime that has no
    // structured on-disk turn state.
    #[test]
    fn completion_signal_non_jsonl_runtime_is_unknown() {
        let fin = TurnFinalizer::spawn();
        let file = write_transcript(&[r#"{"type":"result","result":"done","session_id":"s"}"#]);
        assert_eq!(
            fin.completion_signal_state(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::LegacyTmuxWrapper),
                file.path(),
            ),
            CompletionSignal::Unknown,
        );
        // ProcessBackend and ClaudeEAdapter are also non-JSONL.
        assert_eq!(
            fin.completion_signal_state(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ProcessBackend),
                file.path(),
            ),
            CompletionSignal::Unknown,
        );
        assert_eq!(
            fin.completion_signal_state(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeEAdapter),
                file.path(),
            ),
            CompletionSignal::Unknown,
        );
    }

    // (c) Non-JSONL PROVIDER (Qwen) → Unknown regardless of runtime kind.
    #[test]
    fn completion_signal_non_jsonl_provider_is_unknown() {
        let fin = TurnFinalizer::spawn();
        let file = write_transcript(&[r#"{"type":"result","result":"done","session_id":"s"}"#]);
        assert_eq!(
            fin.completion_signal_state(
                &ProviderKind::Qwen,
                Some(RuntimeHandoffKind::ClaudeTui),
                file.path(),
            ),
            CompletionSignal::Unknown,
        );
    }
}
