//! #3405: terminal-usage adoption + analytics re-parser cluster, split verbatim
//! out of `session_backend.rs`. Owns the #3344 provenance/magnitude gate
//! (`adopt_terminal_result_usage`) shared by the watcher and the analytics
//! re-parser, plus the offset-range re-parse entry points
//! (`extract_turn_analytics_from_output*`). The stream-line processor it drives
//! (`process_stream_line`) and the `StreamLineState` it accumulates into live in
//! the sibling `stream_line` child and resolve through the parent glob below.

use super::*;

/// #3344: backstop ceiling for impossible-magnitude session-cumulative terminal
/// usage from NON-codex providers (codex-legacy is gated by provenance, not
/// magnitude — see [`adopt_terminal_result_usage`]). MAINTENANCE RULE: keep
/// strictly above the largest registry context window (1_000_000) so it never
/// rejects honest per-call usage.
const MAX_PLAUSIBLE_PER_CALL_CONTEXT_TOKENS: u64 = 2_000_000;

/// #3344: adopt a terminal `result` frame's nested `usage` into `state`'s
/// per-call accumulators with provenance + magnitude gating. Shared by the
/// watcher (`process_watcher_lines`) and the analytics re-parser
/// (`process_stream_line`) so both consumers stay in sync.
///
/// Provenance gate (primary): the codex legacy wrapper's `emit_success_result`
/// ALWAYS stamps top-level `input_tokens`/`output_tokens` (Qwen emits a nested
/// `usage` only), so that marker identifies codex-legacy, whose terminal
/// accounting is session-cumulative. Its nested `usage` is NEVER adopted — even
/// small/early values — because honest-unknown CTW beats a sometimes-right
/// number that drifts into a misleading clamped 100%; codex per-call occupancy
/// flows from session `token_count` records (#3331). Magnitude gate (backstop):
/// non-codex providers adopt unchanged unless input+cache clears
/// [`MAX_PLAUSIBLE_PER_CALL_CONTEXT_TOKENS`]. Suppression leaves accumulators
/// untouched → honest "unknown". Claude never reaches this path
/// (`saw_per_message_usage` is set on its per-message `usage` frames).
pub fn adopt_terminal_result_usage(frame: &Value, state: &mut StreamLineState) {
    // Provenance: top-level token fields mark the codex legacy wrapper, whose
    // terminal usage is known-cumulative — suppress always.
    let is_codex_legacy_frame =
        frame.get("input_tokens").is_some() || frame.get("output_tokens").is_some();
    if state.saw_per_message_usage || is_codex_legacy_frame {
        return;
    }
    let Some(usage) = frame.get("usage") else {
        return;
    };
    let read = |key: &str| usage.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
    let (input, cache_read, cache_create, output) = (
        read("input_tokens"),
        read("cache_read_input_tokens"),
        read("cache_creation_input_tokens"),
        read("output_tokens"),
    );
    let occupancy = input
        .saturating_add(cache_read)
        .saturating_add(cache_create);
    if occupancy <= MAX_PLAUSIBLE_PER_CALL_CONTEXT_TOKENS {
        state.accum_input_tokens = input;
        state.accum_cache_read_tokens = cache_read;
        state.accum_cache_create_tokens = cache_create;
        state.accum_output_tokens = output;
    }
}

pub fn extract_turn_analytics_from_output(
    output_path: &str,
    start_offset: u64,
) -> (Option<String>, Option<TurnTokenUsage>) {
    extract_turn_analytics_from_output_range(output_path, start_offset, None)
}

pub fn extract_turn_analytics_from_output_range(
    output_path: &str,
    start_offset: u64,
    end_offset: Option<u64>,
) -> (Option<String>, Option<TurnTokenUsage>) {
    let Ok(bytes) = std::fs::read(output_path) else {
        return (None, None);
    };
    let end = end_offset
        .and_then(|offset| usize::try_from(offset).ok())
        .map(|offset| offset.min(bytes.len()))
        .unwrap_or(bytes.len());
    let start = usize::try_from(start_offset)
        .ok()
        .map(|offset| offset.min(end))
        .unwrap_or(end);

    let (sender, _receiver) = std::sync::mpsc::channel::<StreamMessage>();
    let mut state = StreamLineState::new();
    for line in String::from_utf8_lossy(&bytes[start..end]).lines() {
        let _ = process_stream_line(line, &sender, &mut state);
    }

    let usage = TurnTokenUsage {
        input_tokens: state.accum_input_tokens,
        cache_create_tokens: state.accum_cache_create_tokens,
        cache_read_tokens: state.accum_cache_read_tokens,
        output_tokens: state.accum_output_tokens,
    };
    let has_usage = usage.input_tokens > 0
        || usage.cache_create_tokens > 0
        || usage.cache_read_tokens > 0
        || usage.output_tokens > 0;

    (state.last_session_id, has_usage.then_some(usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #3344 round 2 — analytics-path parity (Finding 2): the analytics
    /// re-parser (`extract_turn_analytics_from_output_range` →
    /// `process_stream_line`) is the SECOND consumer of terminal `result.usage`,
    /// and `turn_analytics::resolve_output_analytics_snapshot` PREFERS its
    /// output over the live snapshot. It must apply the same provenance gate:
    /// the codex legacy wrapper's terminal frame (top-level
    /// `input_tokens`/`output_tokens` marker) carries known-cumulative
    /// accounting, so its nested `usage` is NOT adopted. The re-parse returns
    /// the session id but `None` usage, so analytics never carries poisoned
    /// context numbers. (On base this nested usage was adopted unconditionally.)
    #[test]
    fn extract_turn_analytics_suppresses_codex_wrapper_result_usage() {
        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("codex-wrapper.jsonl");
        std::fs::write(
            &output_path,
            concat!(
                "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"sess-cdx\"}\n",
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"codex reply\"}]}}\n",
                "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"codex reply\",\"session_id\":\"sess-cdx\",\"duration_ms\":12,\"input_tokens\":8325687,\"output_tokens\":41600,\"usage\":{\"input_tokens\":500000,\"cache_read_input_tokens\":600,\"output_tokens\":50}}\n",
            ),
        )
        .unwrap();

        let (session_id, usage) = extract_turn_analytics_from_output_range(
            output_path.to_string_lossy().as_ref(),
            0,
            None,
        );

        assert_eq!(session_id.as_deref(), Some("sess-cdx"));
        // Codex-legacy provenance → nested usage suppressed → no poisoned
        // analytics. `has_usage` is false so the re-parser returns None.
        assert!(
            usage.is_none(),
            "codex-legacy terminal usage must be suppressed from analytics"
        );
    }

    /// #3344 round 2 — analytics-path other-provider regression (Finding 2 /
    /// Test 4): a terminal-usage provider WITHOUT the codex top-level marker
    /// (Qwen's tmux wrapper) keeps flowing through the analytics re-parse
    /// unchanged. The provenance gate does not fire; the magnitude backstop
    /// passes for sane per-call values, so the per-call occupancy is recovered.
    #[test]
    fn extract_turn_analytics_adopts_qwen_terminal_usage() {
        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("qwen-wrapper.jsonl");
        std::fs::write(
            &output_path,
            concat!(
                "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"sess-qwen\"}\n",
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"qwen reply\"}]}}\n",
                "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"qwen reply\",\"session_id\":\"sess-qwen\",\"duration_ms\":7,\"usage\":{\"input_tokens\":1200,\"cache_read_input_tokens\":300,\"cache_creation_input_tokens\":80,\"output_tokens\":256}}\n",
            ),
        )
        .unwrap();

        let (session_id, usage) = extract_turn_analytics_from_output_range(
            output_path.to_string_lossy().as_ref(),
            0,
            None,
        );

        assert_eq!(session_id.as_deref(), Some("sess-qwen"));
        let usage = usage.expect("qwen terminal usage must be recoverable");
        assert_eq!(usage.input_tokens, 1200);
        assert_eq!(usage.cache_read_tokens, 300);
        assert_eq!(usage.cache_create_tokens, 80);
        assert_eq!(usage.output_tokens, 256);
    }

    /// #3344 round 2 — analytics-path backstop: a non-codex provider whose
    /// nested occupancy clears the 2M ceiling is rejected by the magnitude
    /// backstop even with no codex marker, so impossible-magnitude garbage from
    /// any provider never poisons analytics.
    #[test]
    fn extract_turn_analytics_backstop_rejects_millions_usage() {
        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("other-wrapper.jsonl");
        std::fs::write(
            &output_path,
            concat!(
                "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"sess-x\"}\n",
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"reply\"}]}}\n",
                "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"reply\",\"session_id\":\"sess-x\",\"duration_ms\":7,\"usage\":{\"input_tokens\":3000000,\"cache_read_input_tokens\":1000000,\"output_tokens\":256}}\n",
            ),
        )
        .unwrap();

        let (_session_id, usage) = extract_turn_analytics_from_output_range(
            output_path.to_string_lossy().as_ref(),
            0,
            None,
        );

        assert!(
            usage.is_none(),
            "millions-scale terminal usage must be rejected by the backstop"
        );
    }
}
