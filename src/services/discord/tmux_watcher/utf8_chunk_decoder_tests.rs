//! #3479 Phase-1 rank-2: tests for the streaming UTF-8 chunk decoder. PURE MOVE
//! from `tmux_watcher.rs`'s `#[cfg(test)] mod tests` (zero logic change). Kept in
//! a sibling `*_tests.rs` so the production module stays within the
//! `src/services/discord/tmux_watcher/**` namespace LoC cap (test files are
//! excluded from the cap by the audit's `production_rust_files()` filter).

use super::*;

#[test]
fn utf8_decoder_buffers_split_multibyte_scalar_at_chunk_start() {
    let mut decoder = Utf8ChunkDecoder::default();
    let payload = "안녕\n";
    let bytes = payload.as_bytes();

    let first = decoder.decode(&bytes[..1], 20);
    assert_eq!(first.start_offset, None);
    assert!(first.text.is_empty());

    let second = decoder.decode(&bytes[1..], 21);
    assert_eq!(second.start_offset, Some(20));
    assert_eq!(second.text, payload);
    assert!(!second.text.contains('\u{FFFD}'));
}

#[test]
fn utf8_decoder_preserves_jsonl_when_multibyte_scalar_splits_after_prefix() {
    let mut decoder = Utf8ChunkDecoder::default();
    let payload = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"안녕하세요 😀\"}]}}\n";
    let korean_start = payload.find('안').expect("fixture contains korean text");
    let split = korean_start + 1;
    let bytes = payload.as_bytes();

    let first = decoder.decode(&bytes[..split], 100);
    let second = decoder.decode(&bytes[split..], 100 + split as u64);

    assert_eq!(first.start_offset, Some(100));
    assert_eq!(second.start_offset, Some(100 + korean_start as u64));
    assert_eq!(format!("{}{}", first.text, second.text), payload);
    assert!(!first.text.contains('\u{FFFD}'));
    assert!(!second.text.contains('\u{FFFD}'));
}
