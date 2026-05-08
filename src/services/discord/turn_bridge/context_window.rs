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
