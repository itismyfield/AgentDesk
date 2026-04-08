/// Decide the final response text when a Done event arrives.
///
/// Returns the text that should be used as `full_response`.
/// - If streaming accumulated post-tool text, keep the streamed `full_response`.
/// - If streaming only accumulated pre-tool narration (tools used, no post-tool
///   text), replace with the authoritative `result` from the Done event.
/// - If streaming produced nothing, use `result` directly.
pub(super) fn resolve_done_response(
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
    None
}

pub(super) fn total_context_tokens(input_tokens: u64, _output_tokens: u64) -> u64 {
    // input_tokens already represents cumulative context window occupancy
    // (system prompt + conversation history + new message + cache tokens).
    // Adding output_tokens would double-count and inflate the percentage.
    input_tokens
}

pub(super) fn persisted_context_tokens(input_tokens: u64, output_tokens: u64) -> Option<u64> {
    let total = total_context_tokens(input_tokens, output_tokens);
    (total > 0).then_some(total)
}
