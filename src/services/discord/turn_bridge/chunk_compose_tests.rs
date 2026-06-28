use super::{
    append_streamed_text_chunk, append_tool_boundary_separator,
    streamed_text_inside_open_code_fence,
};

// #3608 regression: Text("first") → ToolUse(...) → Text("\n\nsecond") must
// compose `first\n\nsecond`, NOT `first\n\n\n\nsecond`.
//
// codex review on PR #3609: the boundary is now driven through the *real*
// production composition primitives — `append_tool_boundary_separator` for
// the ToolUse arm and `append_streamed_text_chunk` for the Text arm —
// instead of hand-rolling the trim+push_str. This ties the test to the
// same helpers the production loop calls, so a logic regression in either
// primitive (e.g. the separator no longer collapsing, or the chunk
// trimming dropped) fails here.
#[test]
fn tool_boundary_then_blank_leading_chunk_collapses_to_single_separator() {
    let mut full_response = String::new();
    append_streamed_text_chunk(&mut full_response, "first");
    // Real ToolUse boundary primitive (production path).
    append_tool_boundary_separator(&mut full_response);
    // Next text chunk that itself starts with a blank line.
    append_streamed_text_chunk(&mut full_response, "\n\nsecond");

    assert_eq!(full_response, "first\n\nsecond");
    assert!(!full_response.contains("\n\n\n"));
}

// #3608 call-site wiring: the full Text → ToolUse → Text composition
// sequence, driven end-to-end through BOTH production primitives, must
// yield a single paragraph separator. This is the matched-pair contract —
// `append_tool_boundary_separator` only ever leaves a `\n\n` suffix, which
// is exactly the suffix `append_streamed_text_chunk` keys off to trim the
// next chunk's leading blank run. If the separator primitive stops
// emitting `\n\n`, or the text primitive stops collapsing the boundary,
// the two no longer compose and this assertion fails.
#[test]
fn text_tooluse_text_sequence_composes_single_separator_via_real_primitives() {
    let mut full_response = String::new();
    // Text arm chunk 1.
    append_streamed_text_chunk(&mut full_response, "first");
    // ToolUse arm separator.
    append_tool_boundary_separator(&mut full_response);
    assert_eq!(full_response, "first\n\n");
    // Text arm chunk 2 (provider re-emits its own leading blank lines).
    append_streamed_text_chunk(&mut full_response, "\n\nsecond");

    assert_eq!(full_response, "first\n\nsecond");
    assert!(!full_response.contains("\n\n\n"));
}

// `append_tool_boundary_separator` is a no-op on an empty body (mirrors the
// production `if !full_response.is_empty()` guard around the ToolUse arm):
// a tool call before any assistant text must not seed a leading separator.
#[test]
fn tool_boundary_separator_is_noop_on_empty_body() {
    let mut full_response = String::new();
    append_tool_boundary_separator(&mut full_response);
    assert_eq!(full_response, "");
}

// `append_tool_boundary_separator` collapses pre-existing trailing
// whitespace to exactly one `\n\n` separator (idempotent boundary): a body
// that already ends in blank lines must not stack a second separator.
#[test]
fn tool_boundary_separator_collapses_trailing_whitespace() {
    let mut full_response = String::from("first\n\n");
    append_tool_boundary_separator(&mut full_response);
    assert_eq!(full_response, "first\n\n");

    let mut trailing_spaces = String::from("first  \n  \n");
    append_tool_boundary_separator(&mut trailing_spaces);
    assert_eq!(trailing_spaces, "first\n\n");
}

// A single intentional `\n\n` separator with a non-blank-leading follow-up
// chunk must pass through untouched (no over-trimming).
#[test]
fn tool_boundary_then_plain_chunk_is_unchanged() {
    let mut full_response = String::from("first\n\n");
    append_streamed_text_chunk(&mut full_response, "second");
    assert_eq!(full_response, "first\n\nsecond");
}

// Without a preceding `\n\n` boundary, a chunk's own leading newlines are
// preserved verbatim — the normalization is boundary-only.
#[test]
fn no_boundary_separator_preserves_chunk_leading_newlines() {
    let mut full_response = String::from("first");
    append_streamed_text_chunk(&mut full_response, "\nsecond");
    assert_eq!(full_response, "first\nsecond");

    let mut single_nl = String::from("first\n");
    append_streamed_text_chunk(&mut single_nl, "\nsecond");
    // ends with only one `\n`, so not a `\n\n` boundary → no trim.
    assert_eq!(single_nl, "first\n\nsecond");
}

// Intentional larger gaps INSIDE a single chunk are preserved; only the
// cross-chunk boundary is normalized.
#[test]
fn intentional_gap_within_single_chunk_is_preserved() {
    let mut full_response = String::new();
    append_streamed_text_chunk(&mut full_response, "a\n\n\n\nb");
    assert_eq!(full_response, "a\n\n\n\nb");
}

// Blank lines inside an OPEN code fence must not be touched: when the
// accumulated body ends inside a fence, the incoming chunk's leading
// newlines are intentional fenced content and are preserved.
#[test]
fn open_code_fence_preserves_chunk_leading_newlines() {
    let mut full_response = String::from("```\ncode line\n\n");
    assert!(streamed_text_inside_open_code_fence(&full_response));
    append_streamed_text_chunk(&mut full_response, "\n\nmore code");
    // Inside the fence: leading blank lines kept verbatim.
    assert_eq!(full_response, "```\ncode line\n\n\n\nmore code");
}

// A closed fence followed by prose normalizes normally (we are no longer
// inside the fence at the boundary).
#[test]
fn closed_code_fence_then_boundary_normalizes() {
    let mut full_response = String::from("```\ncode\n```\n\n");
    assert!(!streamed_text_inside_open_code_fence(&full_response));
    append_streamed_text_chunk(&mut full_response, "\n\nafter");
    assert_eq!(full_response, "```\ncode\n```\n\nafter");
}

#[test]
fn semantic_sentence_boundary_separates_glued_korean_progress_chunks() {
    let mut inline_code = String::from("확인하겠습니다.");
    append_streamed_text_chunk(&mut inline_code, "`NullRHI`");
    assert_eq!(inline_code, "확인하겠습니다.\n\n`NullRHI`");

    let mut identifier = String::from("보겠습니다.");
    append_streamed_text_chunk(&mut identifier, "Monolith는");
    assert_eq!(identifier, "보겠습니다.\n\nMonolith는");

    let mut lower = String::from("완료.");
    append_streamed_text_chunk(&mut lower, "local");
    assert_eq!(lower, "완료.\n\nlocal");
}

#[test]
fn semantic_sentence_boundary_separates_ascii_status_chunks() {
    let mut full_response = String::from("Review complete.");
    append_streamed_text_chunk(&mut full_response, "VERDICT: CLEAN");
    assert_eq!(full_response, "Review complete.\n\nVERDICT: CLEAN");
}

#[test]
fn semantic_sentence_boundary_avoids_decimal_and_single_token_extension_splits() {
    let mut decimal = String::from("version 1.");
    append_streamed_text_chunk(&mut decimal, "2");
    assert_eq!(decimal, "version 1.2");

    let mut file = String::from("config.");
    append_streamed_text_chunk(&mut file, "yaml");
    assert_eq!(file, "config.yaml");
}

#[test]
fn semantic_sentence_boundary_does_not_touch_lists_tables_or_open_fences() {
    let mut list = String::from("- item.");
    append_streamed_text_chunk(&mut list, "- next");
    assert_eq!(list, "- item.- next");

    let mut table = String::from("| Col |");
    append_streamed_text_chunk(&mut table, "| Next |");
    assert_eq!(table, "| Col || Next |");

    let mut fence = String::from("```text\ncomplete.\n");
    append_streamed_text_chunk(&mut fence, "VERDICT");
    assert_eq!(fence, "```text\ncomplete.\nVERDICT");
}
