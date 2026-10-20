//! Whether a transcript body carrying our nonce is exactly the frame that was pasted.
//! Claude Code may record a paste inside one `<pasted_content id="hhhh">` block, read off its transcripts.

const OPEN: &str = "<pasted_content id=\"";
const CLOSE: &str = "</pasted_content id=\"";
/// Ids Claude Code writes are four lowercase hex digits; any other id stays contaminated.
const TOKEN_LEN: usize = 4;

/// The frame verbatim, or in one whole paste block with Claude's measured outer separators.
pub(crate) fn exact(body: &str, expected: &str) -> bool {
    body == expected
        || unwrap_paste(body) == Some(expected)
        // Claude writes exactly two leading LFs and one trailing LF around a folded paste.
        || body
            .strip_prefix("\n\n")
            .and_then(|inner| inner.strip_suffix('\n'))
            .and_then(unwrap_paste)
            == Some(expected)
}

/// The inside of `<pasted_content id="T">\n…\n</pasted_content id="T">` when that is the whole body.
fn unwrap_paste(body: &str) -> Option<&str> {
    let (token, rest) = body.strip_prefix(OPEN)?.split_at_checked(TOKEN_LEN)?;
    if !token
        .bytes()
        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    let inner = rest.strip_prefix("\">\n")?;
    inner.strip_suffix(&format!("\n{CLOSE}{token}\">"))
}
