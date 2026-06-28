fn line_starts_code_fence(line: &str) -> bool {
    line.trim_start().starts_with("```")
}

fn has_hangul(text: &str) -> bool {
    text.chars()
        .any(|ch| ('\u{ac00}'..='\u{d7a3}').contains(&ch))
}

fn semantic_terminal_char(ch: char) -> bool {
    matches!(ch, '.' | '!' | '?' | '…' | '。' | '！' | '？')
}

fn semantic_terminal_boundary_allowed(line: &str, idx: usize, ch: char) -> bool {
    if ch != '.' {
        return true;
    }

    let before = line[..idx].chars().rev().find(|ch| !ch.is_whitespace());
    let after = line[idx + ch.len_utf8()..].chars().next();
    if before.is_some_and(|ch| ch.is_ascii_digit()) && after.is_some_and(|ch| ch.is_ascii_digit()) {
        return false;
    }

    let token_before_dot = line[..idx].rsplit(char::is_whitespace).next().unwrap_or("");
    if before.is_some_and(|ch| ch.is_ascii_alphanumeric())
        && after.is_some_and(|ch| ch.is_ascii_lowercase())
        && !has_hangul(token_before_dot)
    {
        return false;
    }

    true
}

fn markdown_continuation_head(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("|")
        || trimmed.starts_with("- ")
        || trimmed.starts_with("* ")
        || trimmed.starts_with("+ ")
        || trimmed.starts_with("> ")
        || trimmed
            .chars()
            .next()
            .is_some_and(|ch| matches!(ch, ')' | ']' | '}' | ',' | ';' | ':'))
}

fn markdown_continuation_tail(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("|")
        || trimmed.starts_with("- ")
        || trimmed.starts_with("* ")
        || trimmed.starts_with("+ ")
        || trimmed.starts_with("> ")
}

pub(in crate::services::discord) fn semantic_sentence_split_boundary(text: &str) -> Option<usize> {
    let mut in_code_block = false;
    let mut offset = 0;
    let mut boundary = None;

    for segment in text.split_inclusive('\n') {
        let line = segment.strip_suffix('\n').unwrap_or(segment);
        let fence_line = line_starts_code_fence(line);
        if !in_code_block && !fence_line && !markdown_continuation_tail(line) {
            for (idx, ch) in line.char_indices() {
                if semantic_terminal_char(ch) && semantic_terminal_boundary_allowed(line, idx, ch) {
                    boundary = Some(offset + idx + ch.len_utf8());
                }
            }
        }
        if fence_line {
            in_code_block = !in_code_block;
        }
        offset += segment.len();
    }

    boundary
}

pub(in crate::services::discord) fn message_split_boundary(
    remaining: &str,
    safe_end: usize,
) -> (usize, &'static str) {
    let window = &remaining[..safe_end];
    if let Some(idx) = window.rfind('\n') {
        (idx, "newline")
    } else if let Some(idx) = semantic_sentence_split_boundary(window) {
        (idx, "semantic")
    } else {
        (safe_end, "hard")
    }
}

pub(in crate::services::discord) fn semantic_chunk_separator_needed(
    prefix: &str,
    incoming: &str,
) -> bool {
    if prefix.is_empty()
        || incoming.is_empty()
        || prefix.chars().last().is_some_and(char::is_whitespace)
        || incoming.chars().next().is_some_and(char::is_whitespace)
    {
        return false;
    }
    if markdown_continuation_head(incoming) {
        return false;
    }

    let tail_line = prefix.rsplit('\n').next().unwrap_or(prefix).trim_end();
    if markdown_continuation_tail(tail_line) {
        return false;
    }

    let Some((last_idx, last)) = tail_line.char_indices().next_back() else {
        return false;
    };
    if !semantic_terminal_char(last) {
        return false;
    }

    let next = incoming.chars().next();
    if last == '.' {
        let before = tail_line[..last_idx]
            .chars()
            .rev()
            .find(|ch| !ch.is_whitespace());
        if before.is_some_and(|ch| ch.is_ascii_digit())
            && next.is_some_and(|ch| ch.is_ascii_digit())
        {
            return false;
        }
        if before.is_some_and(|ch| ch.is_ascii_alphanumeric())
            && next.is_some_and(|ch| ch.is_ascii_lowercase())
            && !has_hangul(tail_line)
            && !tail_line.chars().any(char::is_whitespace)
        {
            return false;
        }
    }

    true
}
