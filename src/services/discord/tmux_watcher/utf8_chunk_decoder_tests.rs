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

#[test]
fn same_opened_source_keeps_split_utf8_provenance_across_decoder_calls() {
    use super::super::loop_poll_prologue::WatcherSourceAuthority;
    use crate::services::cluster::stream_relay::SourceFileIdentity;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.jsonl");
    let original = "안".as_bytes();
    std::fs::write(&path, original).unwrap();
    let file = std::fs::File::open(&path).unwrap();
    let identity = SourceFileIdentity::from_open_file(&file);
    let source = WatcherSourceAuthority {
        source_file: identity,
        generation_mtime_ns: 77,
        reset_incarnation: 4,
        source_stamp: None,
    };
    let mut decoder = Utf8ChunkDecoder::default();
    assert!(
        decoder
            .decode_source(&original[..1], 20, source)
            .text
            .is_empty()
    );
    // Decoder is owned by the watcher, so this same value survives collector calls.
    let decoded = decoder.decode_source(&original[1..], 21, source);
    assert_eq!(
        (decoded.start_offset, decoded.text.as_str()),
        (Some(20), "안")
    );
    assert!(
        !decoded.mixed_read_provenance,
        "same opened source is not mixed"
    );

    for changed in [
        WatcherSourceAuthority {
            generation_mtime_ns: 78,
            ..source
        },
        WatcherSourceAuthority {
            reset_incarnation: 5,
            ..source
        },
        WatcherSourceAuthority {
            source_file: SourceFileIdentity::Unavailable,
            ..source
        },
        WatcherSourceAuthority {
            generation_mtime_ns: 0,
            ..source
        },
    ] {
        let mut decoder = Utf8ChunkDecoder::default();
        decoder.decode_source(&original[..1], 20, source);
        // An empty read must not relabel the pending original byte.
        decoder.decode_source(&[], 21, changed);
        let decoded = decoder.decode_source(&original[1..], 21, changed);
        assert_eq!(
            decoded.text, "안",
            "do not reset or discard original raw carry"
        );
        assert!(decoded.mixed_read_provenance);
    }

    for unknown in [
        WatcherSourceAuthority {
            source_file: SourceFileIdentity::Unavailable,
            ..source
        },
        WatcherSourceAuthority {
            generation_mtime_ns: 0,
            ..source
        },
    ] {
        let mut decoder = Utf8ChunkDecoder::default();
        decoder.decode_source(&original[..1], 20, unknown);
        let decoded = decoder.decode_source(&original[1..], 21, unknown);
        assert_eq!(decoded.text, "안");
        assert!(
            decoded.mixed_read_provenance,
            "matching unknowns are not origin proof"
        );
    }

    #[cfg(unix)]
    {
        let replacement = dir.path().join("replacement.jsonl");
        std::fs::write(&replacement, original).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let (_, _, new_identity) = read_watcher_source_chunk(path.to_str().unwrap(), 0)
            .unwrap()
            .into_parts();
        assert_ne!(identity, new_identity);
        let next = source_authority_for_read(source, "decoder-local-proof", None, new_identity);
        assert_eq!(
            next.source_file, new_identity,
            "FD identity survives observer-off"
        );
        let mut decoder = Utf8ChunkDecoder::default();
        decoder.decode_source(&original[..1], 20, source);
        let decoded = decoder.decode_source(&original[1..], 21, next);
        assert_eq!(decoded.text, "안");
        assert!(
            decoded.mixed_read_provenance,
            "same path is not the same opened file"
        );
    }
    decoder.decode_source(&original[..1], 30, source);
    decoder.clear_pending();
    assert!(decoder.pending_source.is_none());
    let decoded = decoder.decode_source(original, 40, source);
    assert_eq!(
        (decoded.start_offset, decoded.text.as_str()),
        (Some(40), "안")
    );
}

#[test]
fn decoder_mixed_carry_stays_poisoned_until_consumed_and_offset_jump_is_mixed() {
    use super::super::loop_poll_prologue::WatcherSourceAuthority;
    use crate::services::cluster::stream_relay::SourceFileIdentity;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.jsonl");
    std::fs::write(&path, []).unwrap();
    let file = std::fs::File::open(path).unwrap();
    let source = WatcherSourceAuthority {
        source_file: SourceFileIdentity::from_open_file(&file),
        generation_mtime_ns: 77,
        reset_incarnation: 4,
        source_stamp: None,
    };
    let changed = WatcherSourceAuthority {
        generation_mtime_ns: 78,
        ..source
    };
    let bytes = "💙".as_bytes();
    let mut decoder = Utf8ChunkDecoder::default();
    assert!(
        decoder
            .decode_source(&bytes[..1], 0, source)
            .text
            .is_empty()
    );
    assert!(
        decoder
            .decode_source(&bytes[1..2], 1, changed)
            .text
            .is_empty()
    );
    assert_eq!(decoder.pending.as_slice(), &bytes[..2]);
    assert!(
        decoder.pending_source.is_none(),
        "A/B carry must remain mixed"
    );
    let decoded = decoder.decode_source(&bytes[2..], 2, changed);
    assert_eq!(decoded.text, "💙");
    assert!(
        decoded.mixed_read_provenance,
        "third read cannot relabel A as B"
    );
    let clean = decoder.decode_source(bytes, 4, changed);
    assert_eq!(clean.text, "💙");
    assert!(
        !clean.mixed_read_provenance,
        "a fully consumed carry permits recovery"
    );

    for next_offset in [0, 2, 100] {
        let mut decoder = Utf8ChunkDecoder::default();
        decoder.decode_source(&bytes[..1], 0, source);
        let decoded = decoder.decode_source(&bytes[1..], next_offset, source);
        assert_eq!(
            decoded.text, "💙",
            "jump does not discard the retained byte"
        );
        assert!(
            decoded.mixed_read_provenance,
            "same source cannot hide a gap or rewind"
        );
    }
    let mut decoder = Utf8ChunkDecoder::default();
    decoder.decode_source(&bytes[..1], 0, source);
    decoder.decode_source(&bytes[1..2], 1, source);
    let decoded = decoder.decode_source(&bytes[2..], 2, source);
    assert_eq!(decoded.text, "💙");
    assert!(
        !decoded.mixed_read_provenance,
        "three contiguous reads from one source remain clean"
    );
}

/// #5979 (I18): a watcher rewind moves the read cursor back and empties
/// `all_data`, but never reaches the decoder. The read that refills the EMPTY
/// buffer must drop the tail buffered from the abandoned read instead of gluing
/// it onto the replay — otherwise the replay reports the abandoned read's offset
/// and every forwarded `source_span` shifts off the file. A continuing read, an
/// empty read, and a read into a non-empty buffer keep the ordinary carry.
#[test]
fn utf8_decoder_drops_the_abandoned_tail_only_when_refilling_an_empty_buffer() {
    use super::super::loop_poll_prologue::WatcherSourceAuthority;
    use crate::services::cluster::stream_relay::SourceFileIdentity;
    let source = WatcherSourceAuthority {
        source_file: SourceFileIdentity::Unavailable,
        generation_mtime_ns: 0,
        reset_incarnation: 0,
        source_stamp: None,
    };
    let line = "{\"type\":\"assistant\"}\n안\n";
    let bytes = line.as_bytes();
    let split = line.find('안').expect("fixture contains korean text") + 1;

    // Rewind: the buffer was emptied and the next read starts at 100 again.
    let mut decoder = Utf8ChunkDecoder::default();
    let first = decoder.decode_source_for_buffer(&bytes[..split], 100, source, "");
    assert_eq!(first.start_offset, Some(100));
    assert!(decoder.has_pending());
    let replay = decoder.decode_source_for_buffer(bytes, 100, source, "");
    assert_eq!(
        replay.start_offset,
        Some(100),
        "replay anchored at the rewind"
    );
    assert_eq!(replay.text, line);
    assert!(!replay.mixed_read_provenance);
    assert!(!decoder.has_pending());

    // An empty read is not a read: the tail survives it (#5833 resume shape).
    let mut decoder = Utf8ChunkDecoder::default();
    decoder.decode_source_for_buffer(&bytes[..split], 100, source, "");
    assert!(
        decoder
            .decode_source_for_buffer(&[], 0, source, "")
            .text
            .is_empty()
    );
    assert!(decoder.has_pending(), "empty read must not drop the tail");
    let rest = decoder.decode_source_for_buffer(&bytes[split..], 100 + split as u64, source, "");
    assert_eq!(rest.start_offset, Some(100 + split as u64 - 1));
    assert_eq!(rest.text, &line[split - 1..]);

    // A jump into a NON-empty buffer keeps the decoder's carry contract.
    let mut decoder = Utf8ChunkDecoder::default();
    let head = decoder.decode_source_for_buffer(&bytes[..split], 100, source, "");
    let jumped = decoder.decode_source_for_buffer(&bytes[split..], 900, source, &head.text);
    assert_eq!(jumped.text, &line[split - 1..]);
    assert!(jumped.mixed_read_provenance);
}
