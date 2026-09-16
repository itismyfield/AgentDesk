//! #3608 청크 경계 빈 줄 정규화 composition primitives.

// #5938 body-mutation telemetry. Declared here rather than in
// `turn_bridge/mod.rs` because that file sits exactly at its 968-line
// `scripts/hotfile_ratchet.toml` ceiling, so a `mod` line there would fail the
// ratchet, and this PR must not raise a cap. The `#[path]` spelling mirrors the
// `chunk_compose_tests.rs` declaration at the bottom of this file.
#[path = "body_mutation_telemetry.rs"]
pub(in crate::services::discord::turn_bridge) mod body_mutation_telemetry;

/// #3608: true when the accumulated `full_response` currently ends *inside* an
/// open ``` code fence. Mirrors the fence toggle used by `format_for_discord`
/// (`trim_start().starts_with("```")`), so blank-line runs the model placed
/// inside a fence are treated as intentional and left untouched.
pub(super) fn streamed_text_inside_open_code_fence(full_response: &str) -> bool {
    let mut in_code_block = false;
    for line in full_response.lines() {
        if line.trim_start().starts_with("```") {
            in_code_block = !in_code_block;
        }
    }
    in_code_block
}

/// #3608: append a streamed `StreamMessage::Text` chunk to `full_response`,
/// normalizing only the *chunk boundary* so a tool-use paragraph separator
/// (`\n\n`, appended at the ToolUse branch) followed by a chunk that itself
/// begins with blank lines does not accumulate into `\n\n\n\n`.
///
/// Narrow by construction (issue option 1): when `full_response` already ends
/// with `\n\n` and we are NOT inside an open code fence, the chunk's leading
/// `\n` run is trimmed before appending so the boundary collapses to a single
/// `\n\n`. Intentional larger gaps *within* a single chunk are preserved, and
/// blank lines inside an open code fence are never touched.
///
/// #5938: every branch below is a `push_str`, so the accumulated body is only
/// ever extended, never rewritten. The post-append observation records that
/// mutation and CANNOT suppress it — a gate here would swallow legitimately
/// repeated model text and manufacture a fresh #5941-class silent loss.
pub(super) fn append_streamed_text_chunk(full_response: &mut String, content: &str) {
    // Captured before the append: because the branches only push, this length
    // IS the retained prefix, so the record needs no clone of the body on this
    // per-streaming-tick hot path.
    let before_len = full_response.len();
    if full_response.ends_with("\n\n") && !streamed_text_inside_open_code_fence(full_response) {
        full_response.push_str(content.trim_start_matches('\n'));
    } else if !streamed_text_inside_open_code_fence(full_response)
        && super::super::semantic_boundaries::semantic_chunk_separator_needed(
            full_response,
            content,
        )
    {
        full_response.push_str("\n\n");
        full_response.push_str(content);
    } else {
        full_response.push_str(content);
    }
    body_mutation_telemetry::observe_body_append(
        body_mutation_telemetry::BodyMutationSite::AppendStreamedTextChunk,
        before_len,
        full_response,
    );
}

/// #3608: append the tool-use paragraph separator to `full_response`.
///
/// When a `StreamMessage::ToolUse` arrives mid-turn we trim trailing
/// whitespace off the accumulated body and append exactly one `\n\n` so the
/// post-tool prose starts on its own paragraph. This is the *only* boundary
/// `append_streamed_text_chunk` keys off (a `\n\n` suffix), so the two helpers
/// are the matched pair that composes `text → tool → text` into a single
/// separator. Extracting the ToolUse side into its own primitive lets the
/// regression test drive the *real* boundary instead of hand-rolling it, so a
/// logic regression in either primitive is caught.
///
/// Caller keeps the surrounding `inflight_state` / `state_dirty` side effects
/// inline — this helper is pure string composition only (no relay/watcher/
/// ownership state, per the #3016 hot-file constraint). No-op on an empty body.
pub(super) fn append_tool_boundary_separator(full_response: &mut String) {
    if full_response.is_empty() {
        return;
    }
    let trimmed = full_response.trim_end();
    full_response.truncate(trimmed.len());
    full_response.push_str("\n\n");
}

#[cfg(test)]
#[path = "chunk_compose_tests.rs"]
mod chunk_boundary_blank_line_tests;
