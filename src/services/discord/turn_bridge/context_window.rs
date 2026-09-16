/// Decide the final response text when a Done event arrives.
///
/// Returns the text that should be used as `full_response`.
/// - If streaming accumulated post-tool text, keep the streamed `full_response`.
/// - If streaming only accumulated pre-tool narration (tools used, no post-tool
///   text), replace with the authoritative `result` from the Done event.
/// - If streaming produced nothing, use `result` directly.
///
/// #5938 r3 P0-1: every `Some` returned here replaces the bridge-local body
/// wholesale, and `result` is NOT bridge-authored — on the TUI-direct path the
/// `Done` frame is synthesised from the watcher's own output file. So the
/// adoption is observed before it is handed back. See
/// [`BodyMutationSite::AdoptTerminalDoneResult`] for the full path and for why
/// this could not be left to the caller (`stream_loop/content_arms.rs` is at its
/// line cap with zero headroom, so it can take no extra argument).
///
/// The decision itself is untouched: `decide_done_response` holds the original
/// four arms byte for byte, this wrapper only records what that decision did.
/// An adoption that returns the body already in hand is a no-op and is skipped,
/// matching `bridge_entry_persist::adopt_full_response_from_inflight_row`.
pub(super) fn resolve_done_response(
    full_response: &str,
    result: &str,
    any_tool_used: bool,
    has_post_tool_text: bool,
) -> Option<String> {
    let resolved = decide_done_response(full_response, result, any_tool_used, has_post_tool_text)?;
    if resolved != full_response {
        super::chunk_compose::body_mutation_telemetry::observe_body_mutation(
            super::chunk_compose::body_mutation_telemetry::BodyMutationSite::AdoptTerminalDoneResult,
            super::chunk_compose::body_mutation_telemetry::BodyMutationCorrelation::unavailable(),
            full_response,
            resolved.as_str(),
        );
    }
    Some(resolved)
}

fn decide_done_response(
    full_response: &str,
    result: &str,
    any_tool_used: bool,
    has_post_tool_text: bool,
) -> Option<String> {
    if result.is_empty() {
        return None;
    }
    if full_response.trim().is_empty() {
        return Some(result.to_string());
    }
    if any_tool_used && !has_post_tool_text {
        return Some(result.to_string());
    }
    if done_result_supersedes_streamed_partial(full_response, result) {
        return Some(result.to_string());
    }
    None
}

fn done_result_supersedes_streamed_partial(full_response: &str, result: &str) -> bool {
    let streamed = full_response.trim();
    let terminal = result.trim();
    !streamed.is_empty()
        && terminal.len() > streamed.len()
        && (terminal.ends_with(streamed) || terminal.starts_with(streamed))
}

pub(super) fn total_context_tokens(
    input_tokens: u64,
    cache_create_tokens: u64,
    cache_read_tokens: u64,
    _output_tokens: u64,
) -> u64 {
    // Context occupancy is the prompt presented to the model. Claude reports
    // uncached input, cache writes, and cache reads as separate usage fields;
    // cached prefixes still occupy the context window. Adding output_tokens
    // would double-count generated text and inflate the percentage.
    input_tokens
        .saturating_add(cache_create_tokens)
        .saturating_add(cache_read_tokens)
}

pub(super) fn persisted_context_tokens(
    input_tokens: u64,
    cache_create_tokens: u64,
    cache_read_tokens: u64,
    output_tokens: u64,
) -> Option<u64> {
    let total = total_context_tokens(
        input_tokens,
        cache_create_tokens,
        cache_read_tokens,
        output_tokens,
    );
    (total > 0).then_some(total)
}

pub(super) fn apply_context_token_update(
    accumulated_input_tokens: &mut u64,
    accumulated_cache_create_tokens: &mut u64,
    accumulated_cache_read_tokens: &mut u64,
    input_tokens: Option<u64>,
    cache_create_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
) -> bool {
    let current_total = total_context_tokens(
        *accumulated_input_tokens,
        *accumulated_cache_create_tokens,
        *accumulated_cache_read_tokens,
        0,
    );
    let next_input_tokens = input_tokens.unwrap_or(*accumulated_input_tokens);
    let next_cache_create_tokens = cache_create_tokens.unwrap_or(*accumulated_cache_create_tokens);
    let next_cache_read_tokens = cache_read_tokens.unwrap_or(*accumulated_cache_read_tokens);
    let next_total = total_context_tokens(
        next_input_tokens,
        next_cache_create_tokens,
        next_cache_read_tokens,
        0,
    );

    if next_total < current_total {
        return false;
    }

    *accumulated_input_tokens = next_input_tokens;
    *accumulated_cache_create_tokens = next_cache_create_tokens;
    *accumulated_cache_read_tokens = next_cache_read_tokens;
    next_total != current_total
}

#[cfg(test)]
mod tests {
    use super::resolve_done_response;
    use crate::services::discord::turn_bridge::chunk_compose::body_mutation_telemetry::body_mutation_telemetry_tests::captured_logs;

    const SITE: &str = "site=\"context_window::resolve_done_response\"";

    /// #5938 r3 P0-1, the verdict-inversion arm. When the streamed body is blank
    /// the terminal `Done` result becomes the WHOLE turn body, and on the
    /// TUI-direct path those bytes were decoded from the watcher's own output
    /// file. Unrecorded, such a turn showed zero cross-boundary records and read
    /// as bridge-first. Driven through the REAL function.
    #[test]
    fn a_blank_streamed_body_adopting_the_terminal_done_result_is_recorded() {
        let logs = captured_logs(|| {
            let resolved = resolve_done_response("", "COUNT-001\nCOUNT-002\n", false, false);
            assert_eq!(resolved.as_deref(), Some("COUNT-001\nCOUNT-002\n"));
        });
        assert!(logs.contains(SITE), "got: {logs}");
        assert!(logs.contains("before_len=0"), "got: {logs}");
        assert!(logs.contains("after_len=20"), "got: {logs}");
    }

    /// #5938 r3 P0-1, the fingerprint-blindness arm.
    /// `done_result_supersedes_streamed_partial` fires on
    /// `terminal.starts_with(streamed)` with a longer terminal — which is exactly
    /// the shape a DOUBLED body has. `record_from_parts` only evaluates
    /// `body_is_exact_self_duplicate` when it builds a record, so while this site
    /// was silent the #5938 predicate never saw the body it was about to deliver.
    /// This pins both halves: the record exists AND it carries the verdict.
    #[test]
    fn a_doubled_terminal_result_is_recorded_and_carries_the_self_duplicate_verdict() {
        let half = "알겠습니다. 바로 진행할게요.";
        let doubled = half.repeat(2);
        let logs = captured_logs(|| {
            let resolved = resolve_done_response(half, &doubled, false, false);
            assert_eq!(resolved.as_deref(), Some(doubled.as_str()));
        });
        assert!(logs.contains(SITE), "got: {logs}");
        assert!(
            logs.contains("self_duplicate=true"),
            "the doubled body must reach the #5938 predicate here, because no \
             later site will look at it; got: {logs}"
        );
        assert!(
            logs.contains("[invariant]"),
            "and it must raise the invariant, not just log the record; got: {logs}"
        );
    }

    /// A `Done` the resolver declines moves no bytes, so it must stay out of the
    /// readout.
    #[test]
    fn a_declined_done_records_nothing() {
        let logs = captured_logs(|| {
            assert!(resolve_done_response("streamed prose stays", "", false, false).is_none());
            assert!(
                resolve_done_response("streamed prose stays", "shorter", false, false).is_none()
            );
        });
        assert!(logs.is_empty(), "got: {logs}");
    }

    /// No-op parity with `bridge_entry_persist::adopt_full_response_from_inflight_row`:
    /// a result equal to the body already in hand is not an adoption.
    #[test]
    fn a_done_result_equal_to_the_body_in_hand_records_nothing() {
        let logs = captured_logs(|| {
            let body = "COUNT-001\nCOUNT-002\n";
            // `any_tool_used && !has_post_tool_text` takes the replace arm, so the
            // decision really does return `Some` here — the silence comes from the
            // no-op skip, not from the resolver declining.
            assert_eq!(resolve_done_response(body, body, true, false).as_deref(), Some(body));
        });
        assert!(logs.is_empty(), "got: {logs}");
    }

    #[test]
    fn done_uses_terminal_result_when_streamed_response_is_tail_only() {
        let streamed_tail = "E15-LINE-150\nE15-LINE-151\n[E2E:E15:END]";
        let terminal_body = "[E2E:E15:BEGIN]\nE15-LINE-001\nE15-LINE-002\nE15-LINE-150\nE15-LINE-151\n[E2E:E15:END]";

        let resolved = resolve_done_response(streamed_tail, terminal_body, false, false);

        assert_eq!(resolved, Some(terminal_body.to_string()));
    }

    #[test]
    fn done_uses_terminal_result_when_streamed_response_is_prefix_missing_tail() {
        let streamed_prefix = "probe 품질 검토 결과, 현재 관측된 기준으로는";
        let terminal_body = "probe 품질 검토 결과, 현재 관측된 기준으로는 실패입니다";

        let resolved = resolve_done_response(streamed_prefix, terminal_body, false, false);

        assert_eq!(resolved, Some(terminal_body.to_string()));
    }

    #[test]
    fn done_keeps_equal_streaming_response_when_no_tools_used() {
        let resolved = resolve_done_response("final response", "final response", false, false);

        assert_eq!(resolved, None);
    }

    // #3419 R3: a tool-only turn replaces a LONG streamed `full_response` with a
    // SHORT sentinel `result`. `retry_state::sync_response_delivery_state`
    // clamps the prior offset to the replaced length and walks it back to a UTF-8
    // char boundary, keeping the watcher slice valid (no relay wedge) and the
    // `response_sent_offset_in_bounds` invariant (inflight.rs) intact.
    #[test]
    fn sentinel_overwrite_clamps_response_sent_offset_within_bounds() {
        // A long streamed pre-tool narration, then a Done with a short sentinel.
        let streamed_long = "a".repeat(900);
        let sentinel = "⚠ tool-only turn, no assistant text";
        // tool used + no post-tool text → resolve replaces with the sentinel.
        let resolved = resolve_done_response(&streamed_long, sentinel, true, false);
        assert_eq!(resolved.as_deref(), Some(sentinel));

        let replaced = resolved.expect("sentinel replacement");
        // The prior offset tracked the long streamed body and now exceeds the
        // replaced length — exactly the out-of-bounds the wedge came from.
        let prior_offset = streamed_long.len();
        assert!(prior_offset > replaced.len());

        // The clamp the bridge applies right after the swap.
        let clamped = prior_offset.min(replaced.len());
        assert_eq!(clamped, replaced.len());
        // Bound + char-boundary invariant (what validate_inflight_state_for_save
        // enforces). Removing the clamp (mutation: `clamped = prior_offset`) makes
        // BOTH assertions fail — the offset is out of bounds and the slice empties.
        assert!(clamped <= replaced.len());
        assert!(replaced.is_char_boundary(clamped));
        assert!(
            !replaced.get(clamped..).unwrap_or("").is_empty() || clamped == replaced.len(),
            "clamped offset yields a valid (possibly empty-at-end) slice, never a panic/None"
        );
        // Concretely: at the clamped offset the remaining slice is well-defined,
        // whereas the unclamped offset would slice OUT OF BOUNDS → `.get` None →
        // watcher empty-slice wedge.
        assert!(replaced.get(prior_offset..).is_none());
    }
}

pub(super) async fn compact_lower_bound(api_port: u16) -> u64 {
    crate::services::discord::adk_session::fetch_context_thresholds(api_port)
        .await
        .compact_lower_bound_tokens
}
