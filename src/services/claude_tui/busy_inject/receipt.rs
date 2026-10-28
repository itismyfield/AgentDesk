//! Whether a transcript body is exactly the pasted frame, and which attempt took a mixed body.
//! Claude Code may record a paste inside one `<pasted_content id="hhhh">` block, read off its transcripts.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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

/// Mixed rows (our frame plus other composer text) an attempt already took, per transcript, so a
/// later drain of the same text confirms no other attempt. Process memory, oldest dropped first.
static MIXED: Mutex<VecDeque<(PathBuf, String, String)>> = Mutex::new(VecDeque::new());
const MIXED_CAP: usize = 256;

/// Whether `nonce` may take `text` in `path` as its mixed receipt; the first taker keeps it.
pub(super) fn take_mixed(path: &Path, nonce: &str, text: &str) -> bool {
    use sha2::Digest;
    let digest = format!("{:x}", sha2::Sha256::digest(text.as_bytes()));
    let mut taken = MIXED.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some((_, owner, _)) = taken.iter().find(|(p, _, d)| p == path && *d == digest) {
        return owner == nonce;
    }
    if taken.len() >= MIXED_CAP {
        taken.pop_front();
    }
    taken.push_back((path.to_path_buf(), nonce.to_string(), digest));
    true
}
