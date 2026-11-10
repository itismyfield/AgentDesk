pub(crate) mod semantic_boundaries;

pub(crate) const DISCORD_MSG_LIMIT: usize = 2000;

/// Conservative unit policy for Discord's documented 2000-character limit.
///
/// Discord does not document a Unicode counting model, and this repository has
/// no observed-delivery evidence that establishes one. The production-call
/// inventory is: `formatting::delivery::{split_message, needs_multiple_messages}`;
/// the streaming split, snapshot, and status truncation helpers in this module;
/// `outbound::decision::decide_length`; `outbound::manual_delivery`'s oversize routing;
/// `outbound::delivery::{truncate_with_marker, split_content}`;
/// `single_message_panel::{compose_completion_footer_text,
/// clamp_footer_status_block}`; `placeholder_live_events::status_panel`;
/// `turn_end_wip_warning`'s bounded merge helpers;
/// `tui_task_card::truncate_preview_at_boundary`; `tmux_placeholder_suppression`; and
/// `http::discord_content_or_zwsp`. This is a caller index, not a claim about a
/// Discord guarantee or a repository-wide invariant: `markdown_preview`
/// uses scalar count while collecting candidate lines before its final
/// unit-based clamp.
pub(crate) fn discord_message_units(s: &str) -> usize {
    s.encode_utf16().count()
}

pub(crate) fn byte_index_at_discord_message_units(s: &str, max_units: usize) -> usize {
    let mut units = 0;
    for (index, character) in s.char_indices() {
        let next_units = units + character.len_utf16();
        if next_units > max_units {
            return index;
        }
        units = next_units;
    }
    s.len()
}

/// Split a message into chunks that fit within the shared Discord unit limit.
/// Handles code block boundaries correctly. Used by stream/slash-command/recovery
/// paths where overflow is delivered as additional inline messages. The manual
/// `/api/discord/send` route uses the shared outbound API length policy instead.
///
/// Emits structured `tracing::debug!` logs at `target: "discord::chunker"` for
/// every chunk produced (chunk_index, byte_len, boundary_kind, in_code_block).
/// This is the tracepoint referenced by issue #1043 to diagnose any case where
/// the tail of a long agent message fails to reach Discord (e.g. option block
/// A/B/C sections disappearing near the 2000-char boundary).
pub(crate) fn split_message(text: &str) -> Vec<String> {
    let total_bytes = text.len();
    let mut chunks = Vec::new();
    let mut remaining = text;
    let mut remaining_units = discord_message_units(text);
    let mut in_code_block = false;
    let mut code_block_lang = String::new();

    while !remaining.is_empty() {
        // Reserve space for code block tags we may need to add
        let tag_overhead = if in_code_block {
            // closing ``` + opening ```lang\n
            discord_message_units("```")
                + discord_message_units("```")
                + discord_message_units(&code_block_lang)
                + discord_message_units("\n")
        } else {
            0
        };
        let effective_limit = DISCORD_MSG_LIMIT
            .saturating_sub(tag_overhead)
            .saturating_sub(10);
        // `code_block_lang` is capped when the opener is observed, so the
        // synthetic reopen/close tags can never consume the body budget. Every
        // Unicode scalar needs at most two UTF-16 units, hence a non-empty
        // `remaining` always has a non-zero hard-split boundary here.
        debug_assert!(effective_limit >= 2);

        if remaining_units <= effective_limit {
            let mut chunk = String::new();
            if in_code_block {
                chunk.push_str("```");
                chunk.push_str(&code_block_lang);
                chunk.push('\n');
            }
            chunk.push_str(remaining);
            let byte_len = chunk.len();
            let was_in_code_block = in_code_block;
            chunks.push(chunk);
            tracing::debug!(
                target: "discord::chunker",
                chunk_index = chunks.len() - 1,
                byte_len,
                boundary_kind = "final",
                in_code_block = was_in_code_block,
                total_bytes,
                "split_message emit"
            );
            break;
        }

        // Find a safe split point.
        //
        // Issue #1043 root cause #1: when the input begins with a leading `\n`
        // and the next ~2000 chars contain no other newline, `rfind('\n')`
        // returns `Some(0)`. That made `raw_chunk` empty, the chunker emitted a
        // zero-byte chunk, and Discord's REST API rejected the send with HTTP
        // 400 ("Cannot send an empty message"). The error short-circuited
        // `send_long_message_raw` / `replace_long_message_raw`, so every later
        // chunk — including the trailing A/B/C option block users were
        // reporting missing — never reached the channel.
        //
        // Fix: if a newline split would yield a zero-byte `raw_chunk`, fall
        // back to a hard split at `safe_end` (or skip the orphan newline when
        // `safe_end` is also 0 due to a multi-byte char on the boundary).
        let safe_end = byte_index_at_discord_message_units(remaining, effective_limit);
        let (mut split_at, mut boundary_kind) =
            semantic_boundaries::message_split_boundary(remaining, safe_end, in_code_block);
        if split_at == 0 {
            split_at = safe_end;
            boundary_kind = "hard_after_leading_newline";
        }

        let (raw_chunk, rest) = remaining.split_at(split_at);
        let raw_chunk_units = discord_message_units(raw_chunk);
        let stripped_boundary_units = usize::from(rest.starts_with('\n'));

        let mut chunk = String::new();
        if in_code_block {
            chunk.push_str("```");
            chunk.push_str(&code_block_lang);
            chunk.push('\n');
        }
        chunk.push_str(raw_chunk);

        // Track code blocks across chunk boundaries
        for line in raw_chunk.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") {
                if in_code_block {
                    in_code_block = false;
                    code_block_lang.clear();
                } else {
                    in_code_block = true;
                    let full_lang = trimmed.strip_prefix("```").unwrap_or("");
                    // Only synthetic continuation fences use this copy. Cap it
                    // to a fraction of the message budget so continuation body
                    // text always makes progress. An unusually long info string
                    // can therefore lose syntax-highlighting fidelity after the
                    // first chunk, while the original opener and body survive.
                    let lang_end =
                        byte_index_at_discord_message_units(full_lang, DISCORD_MSG_LIMIT / 4);
                    code_block_lang = full_lang[..lang_end].to_string();
                }
            }
        }

        // Close unclosed code block at end of chunk
        if in_code_block {
            chunk.push_str("\n```");
        }

        let byte_len = chunk.len();
        let fence_was_open_at_emit = in_code_block;
        // Defensive: never emit an empty chunk to the Discord send path.
        // (split_at == 0 is handled above; this guard catches any future
        // rewrite that could regress.)
        if chunk.is_empty() {
            tracing::warn!(
                target: "discord::chunker",
                boundary_kind,
                total_bytes,
                "split_message would have emitted an empty chunk; skipping (issue #1043 guard)"
            );
            remaining_units = remaining_units
                .saturating_sub(raw_chunk_units)
                .saturating_sub(stripped_boundary_units);
            remaining = rest.strip_prefix('\n').unwrap_or(rest);
            continue;
        }
        chunks.push(chunk);
        tracing::debug!(
            target: "discord::chunker",
            chunk_index = chunks.len() - 1,
            byte_len,
            boundary_kind,
            in_code_block = fence_was_open_at_emit,
            total_bytes,
            "split_message emit"
        );
        remaining_units = remaining_units
            .saturating_sub(raw_chunk_units)
            .saturating_sub(stripped_boundary_units);
        remaining = rest.strip_prefix('\n').unwrap_or(rest);
    }

    tracing::debug!(
        target: "discord::chunker",
        total_chunks = chunks.len(),
        total_bytes,
        "split_message done"
    );

    chunks
}

/// Whether `text` exceeds what one Discord message can carry.
///
/// Discord documents the limit as characters without defining a Unicode
/// counting model. [`discord_message_units`] records this repository's
/// conservative UTF-16 policy and its evidentiary limitation.
///
/// The predicate uses the hard limit. [`split_message`] retains its smaller
/// effective limit so code-fence repair has room; callers that invoke the
/// chunker unconditionally can therefore split a hard-limit-fitting message.
pub(crate) fn needs_multiple_messages(text: &str) -> bool {
    discord_message_units(text) > DISCORD_MSG_LIMIT
}

/// Legacy split pieces and their UTF-16 unit counts, reused unchanged by the output shadow.
pub(crate) fn split_for_shadow(text: &str) -> Vec<(String, usize)> {
    let with_units = |piece: String| {
        let units = discord_message_units(&piece);
        (piece, units)
    };
    split_message(text).into_iter().map(with_units).collect()
}
