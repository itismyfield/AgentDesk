/// A folded paste identifies its shape and LF count, not the payload's contents.
pub(in crate::services::claude_tui) fn matches_prompt(row: &str, prompt: &str) -> bool {
    let Some(rest) = row.strip_prefix("[Pasted text #") else {
        return false;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return false;
    }
    let suffix = &rest[digits..];
    let lines = prompt.bytes().filter(|byte| *byte == b'\n').count();
    if lines == 0 {
        suffix == "]"
    } else {
        suffix == format!(" +{lines} lines]")
    }
}

#[cfg(test)]
mod tests {
    use super::matches_prompt;

    #[test]
    fn folded_placeholder_requires_exact_shape_and_payload_lf_count() {
        for (row, prompt) in [
            ("[Pasted text #1]", "single line"),
            ("[Pasted text #27]", "한글"),
            ("[Pasted text #2 +1 lines]", "first\nsecond"),
            ("[Pasted text #9 +2 lines]", "first\nsecond\n"),
        ] {
            assert!(matches_prompt(row, prompt), "{row:?} / {prompt:?}");
        }
    }

    #[test]
    fn folded_placeholder_rejects_foreign_counts_and_malformed_markers() {
        for (row, prompt) in [
            ("[Pasted text #1]", "first\nsecond"),
            ("[Pasted text #1 +1 lines]", "single line"),
            ("[Pasted text #1 +0 lines]", "single line"),
            ("[Pasted text # +1 lines]", "first\nsecond"),
            ("[Pasted text #x +1 lines]", "first\nsecond"),
            ("[Pasted text #１ +1 lines]", "first\nsecond"),
            ("[Pasted text #1 +01 lines]", "first\nsecond"),
            ("[Pasted text #1  +1 lines]", "first\nsecond"),
            ("[Pasted text #1 +1 line]", "first\nsecond"),
            ("[Pasted text #1 +1 lines] foreign", "first\nsecond"),
            ("foreign [Pasted text #1 +1 lines]", "first\nsecond"),
        ] {
            assert!(!matches_prompt(row, prompt), "{row:?} / {prompt:?}");
        }
    }
}
