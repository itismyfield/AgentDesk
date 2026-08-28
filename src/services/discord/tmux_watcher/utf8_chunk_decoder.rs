//! #3479 Phase-1 rank-2 extraction: the tmux watcher's streaming UTF-8 chunk
//! decoder — `Utf8ChunkDecoder` + its `DecodedUtf8Chunk` result, which buffer a
//! partial trailing multibyte scalar across read boundaries so a code point
//! split between two `read()` chunks is never emitted as `U+FFFD`. PURE MOVE from
//! `tmux_watcher.rs` (zero logic change) to shrink the frozen root file below its
//! maintainability baseline.
//!
use crate::services::cluster::stream_relay::{SourceFileIdentity, SourceWitness};
use std::io::{Read, Seek, SeekFrom};

type SourceChunk = Result<(Vec<u8>, u64, SourceFileIdentity), String>;

pub(super) fn read_watcher_source_chunk(path: &str, offset: u64) -> SourceChunk {
    read_watcher_source_chunk_from_file(
        std::fs::File::open(path).map_err(|error| format!("open: {error}"))?,
        offset,
    )
}

fn read_watcher_source_chunk_from_file(mut file: std::fs::File, offset: u64) -> SourceChunk {
    let identity = SourceFileIdentity::from_open_file(&file);
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| format!("seek: {error}"))?;
    let mut bytes = vec![0_u8; 16_384];
    let read = file
        .read(&mut bytes)
        .map_err(|error| format!("read: {error}"))?;
    bytes.truncate(read);
    Ok((bytes, offset + read as u64, identity))
}

pub(super) fn authority_for_decoded_text(
    authority: super::loop_poll_prologue::WatcherSourceAuthority,
    mixed: bool,
) -> super::loop_poll_prologue::WatcherSourceAuthority {
    super::loop_poll_prologue::WatcherSourceAuthority {
        source_stamp: (!mixed).then_some(authority.source_stamp).flatten(),
        ..authority
    }
}

pub(super) fn source_authority_for_read(
    base: super::loop_poll_prologue::WatcherSourceAuthority,
    session: &str,
    witness: Option<SourceWitness>,
    file: SourceFileIdentity,
) -> super::loop_poll_prologue::WatcherSourceAuthority {
    super::loop_poll_prologue::WatcherSourceAuthority {
        source_stamp: witness.and_then(|witness| {
            crate::services::discord::delivery_lease_cell::source_epoch_observer::source_stamp(
                session, witness, file,
            )
        }),
        ..base
    }
}

#[cfg(all(test, unix))]
mod source_epoch_read_tests {
    use super::*;
    use crate::services::cluster::stream_relay::GenerationSourceIdentity;

    #[test]
    #[rustfmt::skip]
    fn same_fd_identity_mode_resample_and_mixed_utf8_policy() {
        let session = format!("watcher-source-{}", uuid::Uuid::new_v4().simple()); let base = super::super::loop_poll_prologue::WatcherSourceAuthority { generation_mtime_ns: 77, reset_incarnation: 9, source_stamp: None };
        let witness = SourceWitness { generation: Some(GenerationSourceIdentity::Unix { mtime_ns: 88, dev: 1, ino: 2 }), spawn_nonce_hash: Some([3; 32]) };
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("source.jsonl"); let replacement = dir.path().join("replacement.jsonl");
        std::fs::write(&path, b"old-bytes").unwrap(); let old_file = std::fs::File::open(&path).unwrap();
        std::fs::write(&replacement, b"new-bytes").unwrap(); std::fs::rename(&replacement, &path).unwrap();
        let (old_bytes, _, old_id) = read_watcher_source_chunk_from_file(old_file, 0).unwrap();
        let (new_bytes, _, new_id) = read_watcher_source_chunk(path.to_str().unwrap(), 0).unwrap();
        assert_eq!((old_bytes.as_slice(), new_bytes.as_slice()), (b"old-bytes".as_slice(), b"new-bytes".as_slice())); assert_ne!(old_id, new_id);
        let first = source_authority_for_read(base, &session, Some(witness), old_id); let known = first.source_stamp;
        let legacy = source_authority_for_read(first, &session, None, new_id); assert_eq!((legacy.source_stamp, legacy.generation_mtime_ns, legacy.reset_incarnation), (None, 77, 9));
        let next = source_authority_for_read(legacy, &session, Some(witness), new_id); assert_ne!(known, next.source_stamp);
        let mixed = authority_for_decoded_text(next, true); assert_eq!((mixed.source_stamp, mixed.generation_mtime_ns, mixed.reset_incarnation), (None, 77, 9));
        let mut decoder = Utf8ChunkDecoder::default(); let bytes = "안".as_bytes(); assert!(!decoder.decode(&bytes[..1], 0).mixed_read_provenance); assert!(decoder.decode(&bytes[1..], 1).mixed_read_provenance);
    }
}

#[derive(Debug, Default)]
pub(super) struct Utf8ChunkDecoder {
    pending: Vec<u8>,
    pending_start_offset: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct DecodedUtf8Chunk {
    pub(super) start_offset: Option<u64>,
    pub(super) text: String,
    pub(super) mixed_read_provenance: bool,
}

impl Utf8ChunkDecoder {
    pub(super) fn decode(&mut self, chunk: &[u8], chunk_start_offset: u64) -> DecodedUtf8Chunk {
        if chunk.is_empty() {
            return DecodedUtf8Chunk {
                start_offset: None,
                text: String::new(),
                mixed_read_provenance: false,
            };
        }
        let had_pending = !self.pending.is_empty();
        if self.pending.is_empty() {
            self.pending_start_offset = Some(chunk_start_offset);
        }
        self.pending.extend_from_slice(chunk);

        let start_offset = self.pending_start_offset.unwrap_or(chunk_start_offset);
        match std::str::from_utf8(&self.pending) {
            Ok(text) => {
                let text = text.to_string();
                self.pending.clear();
                self.pending_start_offset = None;
                DecodedUtf8Chunk {
                    start_offset: Some(start_offset),
                    mixed_read_provenance: had_pending && !text.is_empty(),
                    text,
                }
            }
            Err(err) if err.error_len().is_none() => {
                let valid_up_to = err.valid_up_to();
                if valid_up_to == 0 {
                    return DecodedUtf8Chunk {
                        start_offset: None,
                        text: String::new(),
                        mixed_read_provenance: false,
                    };
                }
                let text = std::str::from_utf8(&self.pending[..valid_up_to])
                    .expect("valid UTF-8 prefix")
                    .to_string();
                self.pending.drain(..valid_up_to);
                self.pending_start_offset = Some(start_offset.saturating_add(valid_up_to as u64));
                DecodedUtf8Chunk {
                    start_offset: Some(start_offset),
                    mixed_read_provenance: had_pending && !text.is_empty(),
                    text,
                }
            }
            Err(_) => {
                let text = String::from_utf8_lossy(&self.pending).into_owned();
                self.pending.clear();
                self.pending_start_offset = None;
                DecodedUtf8Chunk {
                    start_offset: Some(start_offset),
                    mixed_read_provenance: had_pending && !text.is_empty(),
                    text,
                }
            }
        }
    }

    pub(super) fn clear_pending(&mut self) {
        self.pending.clear();
        self.pending_start_offset = None;
    }
}

#[cfg(test)]
#[path = "utf8_chunk_decoder_tests.rs"]
mod tests;
