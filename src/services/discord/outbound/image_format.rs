//! Image checks shared by scheduled-message uploads and tool-result images: MIME, size and magic.

/// Largest decoded image either path accepts; the scheduled-message store checks the same bound.
pub const MAX_IMAGE_BYTES: usize = crate::db::scheduled_messages::MAX_IMAGE_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejected {
    /// Empty, or larger than `MAX_IMAGE_BYTES` once decoded.
    Size,
    /// The bytes do not open as the MIME says, or the MIME is not PNG, JPEG, GIF or WebP.
    Magic,
}

/// Whether `filename`'s extension names the lowercase `content_type`.
pub fn extension_matches(filename: &str, content_type: &str) -> bool {
    let extension = filename
        .rsplit_once('.')
        .filter(|(stem, _)| !stem.is_empty())
        .map(|(_, extension)| extension)
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        (extension.as_str(), content_type),
        ("png", "image/png")
            | ("jpg" | "jpeg", "image/jpeg")
            | ("gif", "image/gif")
            | ("webp", "image/webp")
    )
}

/// Whether base64 text of this length can decode within `MAX_IMAGE_BYTES`, checked before decoding.
pub fn encoded_fits(encoded_len: usize) -> bool {
    encoded_len <= MAX_IMAGE_BYTES.div_ceil(3) * 4
}

/// Decoded bytes against the lowercase `content_type`; tool images may carry no filename.
pub fn check_bytes(content_type: &str, data: &[u8]) -> Result<(), Rejected> {
    if !(1..=MAX_IMAGE_BYTES).contains(&data.len()) {
        return Err(Rejected::Size);
    }
    let matches_magic = match content_type {
        "image/png" => data.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => data.starts_with(&[0xff, 0xd8, 0xff]),
        "image/gif" => data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a"),
        "image/webp" => data.starts_with(b"RIFF") && data.get(8..12) == Some(&b"WEBP"[..]),
        _ => false,
    };
    matches_magic.then_some(()).ok_or(Rejected::Magic)
}
