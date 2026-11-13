use super::{DISCORD_MSG_LIMIT, byte_index_at_discord_message_units, discord_message_units};

/// Render literal text with valid triple blocks and a bounded omission notice.
pub(crate) fn bounded_raw_code_block_notice(header: &str, text: &str) -> String {
    let total_chars = text.chars().count();
    let max_suffix = format!("\n-# … (이하 {total_chars}자)");
    let header_end = byte_index_at_discord_message_units(
        header,
        DISCORD_MSG_LIMIT.saturating_sub(9 + discord_message_units(&max_suffix)),
    );
    let ends: Vec<_> = std::iter::once(0)
        .chain(
            text.char_indices()
                .take(DISCORD_MSG_LIMIT)
                .map(|(i, ch)| i + ch.len_utf8()),
        )
        .collect();
    let max = ends.len() - 1;
    let collision = text.contains("```");
    let pieces = if collision {
        literal_pieces(text, ends[max])
    } else {
        vec![(0, ends[max], false)]
    };
    let render = |shown: usize| {
        render_prefix(
            &header[..header_end],
            text,
            &pieces,
            ends[shown],
            !collision,
        )
    };
    let complete = render(max);
    if max == total_chars && discord_message_units(&complete) <= DISCORD_MSG_LIMIT {
        return complete;
    }
    let bounded = |shown: usize| {
        let mut output = render(shown);
        let omitted = total_chars - shown;
        if omitted > 0 {
            output.push_str(&format!("\n-# … (이하 {omitted}자)"));
        }
        output
    };
    let (mut low, mut high) = (0, max);
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if discord_message_units(&bounded(mid)) <= DISCORD_MSG_LIMIT {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    bounded(low)
}

fn literal_pieces(text: &str, cap: usize) -> Vec<(usize, usize, bool)> {
    let (mut cursor, mut plain) = (0, 0);
    let mut pieces = Vec::new();
    while cursor < cap {
        // Inspect the original run even if the display cap lands within its first two ticks.
        if text[cursor..].starts_with("```") {
            if plain < cursor {
                pieces.push((plain, cursor, false));
            }
            let start = cursor;
            while cursor < cap && text.as_bytes()[cursor] == b'`' {
                cursor += 1;
            }
            pieces.push((start, cursor, true));
            plain = cursor;
        } else if let Some(ch) = text[cursor..].chars().next() {
            cursor += ch.len_utf8();
        } else {
            break;
        }
    }
    if plain < cap {
        pieces.push((plain, cap, false));
    }
    pieces
}

fn render_prefix(
    header: &str,
    text: &str,
    pieces: &[(usize, usize, bool)],
    end: usize,
    ordinary: bool,
) -> String {
    let mut output = format!("{header}\n");
    if ordinary && end == 0 {
        output.push_str("```\n\n```");
    }
    for &(start, stop, escaped) in pieces {
        if start >= end {
            break;
        }
        let piece = &text[start..stop.min(end)];
        if escaped {
            if !output.ends_with('\n') {
                output.push('\n');
            }
            for _ in 0..piece.len() {
                output.push_str("\\`");
            }
        } else if ordinary || piece.chars().any(|ch| ch != '\n') {
            if !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str("```\n");
            output.push_str(piece);
            output.push_str("\n```");
        } else {
            output.push_str(piece);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collision_fragments_omit_empty_and_newline_only_blocks() {
        for (text, expected) in [
            ("```", "H\n\\`\\`\\`"),
            ("```\n\n```", "H\n\\`\\`\\`\n\n\\`\\`\\`"),
            ("```**bold**", "H\n\\`\\`\\`\n```\n**bold**\n```"),
            ("end ```", "H\n```\nend \n```\n\\`\\`\\`"),
        ] {
            assert_eq!(bounded_raw_code_block_notice("H", text), expected);
        }
    }

    #[test]
    fn ordinary_fragments_keep_the_previous_bytes() {
        for text in ["", "\n\n", "first\nsecond", " \t\n", "😀_raw_*"] {
            assert_eq!(
                bounded_raw_code_block_notice("H", text),
                format!("H\n```\n{text}\n```")
            );
        }
    }
}
