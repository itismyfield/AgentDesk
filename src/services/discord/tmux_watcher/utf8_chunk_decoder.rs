//! Streaming UTF-8 decoder and opened-source provenance carried by the watcher.
//! Split scalars retain every original byte; source continuity is tracked apart
//! from decoding so a mixed or non-contiguous carry never gains a clean stamp.
use crate::services::cluster::stream_relay::{SourceFileIdentity, SourceWitness};
use crate::services::discord::tmux::tmux_output_stream::{
    read_native_codex_state, watcher_source_witness,
};
use std::io::{Read, Seek, SeekFrom};

type SourceChunk = Result<WatcherReadBatch, String>;

pub(super) struct WatcherReadBatch {
    file: std::fs::File,
    bytes: Vec<u8>,
    end: u64,
    origin: SourceFileIdentity,
}

impl WatcherReadBatch {
    fn read(mut self) -> SourceChunk {
        self.origin = SourceFileIdentity::from_open_file(&self.file);
        self.file
            .seek(SeekFrom::Start(self.end))
            .map_err(|error| format!("seek: {error}"))?;
        self.bytes = vec![0_u8; 16_384];
        let read = self
            .file
            .read(&mut self.bytes)
            .map_err(|error| format!("read: {error}"))?;
        self.bytes.truncate(read);
        self.end += read as u64;
        Ok(self)
    }

    pub(super) fn into_retained_parts(
        self,
    ) -> (
        Vec<u8>,
        u64,
        SourceFileIdentity,
        std::sync::Arc<std::fs::File>,
    ) {
        (
            self.bytes,
            self.end,
            self.origin,
            std::sync::Arc::new(self.file),
        )
    }

    pub(super) fn into_parts(self) -> (Vec<u8>, u64, SourceFileIdentity) {
        (self.bytes, self.end, self.origin)
    }
}

pub(super) fn read_watcher_source_chunk(path: &str, offset: u64) -> SourceChunk {
    read_watcher_source_chunk_from_file(
        std::fs::File::open(path).map_err(|error| format!("open: {error}"))?,
        offset,
    )
}

pub(super) async fn read_watcher_source_chunk_with_witness(
    ctx: &super::TurnStreamCollectorContext,
    offset: u64,
) -> (
    Result<Result<SourceChunk, tokio::task::JoinError>, tokio::time::error::Elapsed>,
    Option<SourceWitness>,
) {
    let witness = watcher_source_witness(
        &ctx.watcher_provider,
        &ctx.tmux_session_name,
        &ctx.output_path,
    );
    let read = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking({
            let path = ctx.output_path.clone();
            move || read_watcher_source_chunk(&path, offset)
        }),
    )
    .await;
    (read, witness)
}

pub(super) async fn read_retained_watcher_source_chunk(
    source: std::sync::Arc<std::fs::File>,
    offset: u64,
) -> Result<Result<SourceChunk, tokio::task::JoinError>, tokio::time::error::Elapsed> {
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(move || {
            let file = source
                .try_clone()
                .map_err(|error| format!("clone retained source: {error}"))?;
            read_watcher_source_chunk_from_file(file, offset)
        }),
    )
    .await
}

fn read_watcher_source_chunk_from_file(file: std::fs::File, offset: u64) -> SourceChunk {
    WatcherReadBatch {
        file,
        bytes: Vec::new(),
        end: offset,
        origin: SourceFileIdentity::Unavailable,
    }
    .read()
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
        source_file: file,
        source_stamp: witness.and_then(|witness| {
            crate::services::discord::delivery_lease_cell::source_epoch_observer::source_stamp(
                session, witness, file,
            )
        }),
        ..base
    }
}

impl super::loop_poll_prologue::WatcherSourceAuthority {
    pub(super) fn restore_stream_decoder(
        &self,
        ctx: &super::TurnStreamCollectorContext,
        cursor: u64,
        response: &mut String,
    ) -> Result<(super::WatcherToolState, Option<super::InflightTurnState>), ()> {
        let mut tool_state = super::WatcherToolState::new();
        tool_state.set_provider(&ctx.watcher_provider);
        let row = crate::services::discord::inflight::load_inflight_state(
            &ctx.watcher_provider,
            ctx.channel_id.get(),
        );
        if let Some(decoder) = self.restore_native_prefix(ctx, row.as_ref(), cursor)? {
            tool_state.restore_native_codex(decoder, response);
        }
        Ok((tool_state, row))
    }

    pub(super) fn restore_native_prefix(
        &self,
        ctx: &super::TurnStreamCollectorContext,
        row: Option<&super::InflightTurnState>,
        cursor: u64,
    ) -> Result<Option<crate::services::codex_tui::rollout_tail::RolloutRecordDecoder>, ()> {
        if ctx.watcher_provider == super::ProviderKind::Codex
            && let Some(row) = row
            && row.runtime_kind
                == Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui)
            && row.tmux_session_name.as_deref() == Some(ctx.tmux_session_name.as_str())
            && row.output_path.as_deref() == Some(ctx.output_path.as_str())
            && let Some(start) = row.turn_start_offset
            && start <= cursor
        {
            let token = ctx.shared.relay_frontier_token(ctx.channel_id);
            let _mutation = (token.reset_incarnation == self.reset_incarnation)
                .then(|| {
                    ctx.shared
                        .acquire_relay_frontier_mutation(ctx.channel_id, token)
                })
                .flatten()
                .ok_or(())?;
            return read_native_codex_state(
                &ctx.output_path, start, cursor, self.source_file,
                &ctx.tmux_session_name, self.generation_mtime_ns, self.source_stamp,
            ).map(Some).map_err(|error| {
                tracing::warn!(channel_id = ctx.channel_id.get(), %error, "native Codex restart prefix unavailable; retaining turn for retry");
            });
        }
        Ok(None)
    }
}

#[cfg(all(test, unix))]
mod source_epoch_read_tests {
    use super::*;
    use crate::services::cluster::stream_relay::GenerationSourceIdentity;

    #[test]
    #[rustfmt::skip]
    fn same_fd_identity_mode_resample_and_mixed_utf8_policy() {
        let session = format!("watcher-source-{}", uuid::Uuid::new_v4().simple()); let base = super::super::loop_poll_prologue::WatcherSourceAuthority { source_file: SourceFileIdentity::Unavailable, generation_mtime_ns: 77, reset_incarnation: 9, source_stamp: None };
        let witness = SourceWitness { generation: Some(GenerationSourceIdentity::Unix { mtime_ns: 88, dev: 1, ino: 2 }), spawn_nonce_hash: Some([3; 32]) };
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("source.jsonl"); let replacement = dir.path().join("replacement.jsonl");
        std::fs::write(&path, b"old-bytes").unwrap(); let old_file = std::fs::File::open(&path).unwrap();
        std::fs::write(&replacement, b"new-bytes").unwrap(); std::fs::rename(&replacement, &path).unwrap();
        let (old_bytes, _, old_id) = read_watcher_source_chunk_from_file(old_file, 0).unwrap().into_parts();
        let (new_bytes, _, new_id) = read_watcher_source_chunk(path.to_str().unwrap(), 0).unwrap().into_parts();
        assert_eq!((old_bytes.as_slice(), new_bytes.as_slice()), (b"old-bytes".as_slice(), b"new-bytes".as_slice())); assert_ne!(old_id, new_id);
        let first = source_authority_for_read(base, &session, Some(witness), old_id); let known = first.source_stamp;
        let legacy = source_authority_for_read(first, &session, None, new_id); assert_eq!((legacy.source_stamp, legacy.generation_mtime_ns, legacy.reset_incarnation), (None, 77, 9));
        let next = source_authority_for_read(legacy, &session, Some(witness), new_id); assert_ne!(known, next.source_stamp);
        let mixed = authority_for_decoded_text(next, true); assert_eq!((mixed.source_stamp, mixed.generation_mtime_ns, mixed.reset_incarnation), (None, 77, 9));
        let mut decoder = Utf8ChunkDecoder::default(); let bytes = "안".as_bytes(); assert!(!decoder.decode(&bytes[..1], 0).mixed_read_provenance); assert!(decoder.decode(&bytes[1..], 1).mixed_read_provenance);
    }
}

#[derive(Debug, Default, Clone)]
pub(super) struct Utf8ChunkDecoder {
    pending: Vec<u8>,
    pending_start_offset: Option<u64>,
    pending_source: Option<super::loop_poll_prologue::WatcherSourceAuthority>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct DecodedUtf8Chunk {
    pub(super) start_offset: Option<u64>,
    pub(super) text: String,
    pub(super) mixed_read_provenance: bool,
}

impl Utf8ChunkDecoder {
    pub(super) fn decode_source(
        &mut self,
        chunk: &[u8],
        offset: u64,
        source: super::loop_poll_prologue::WatcherSourceAuthority,
    ) -> DecodedUtf8Chunk {
        let had_pending = !self.pending.is_empty();
        let contiguous = self.pending_end_offset() == Some(offset);
        let same_source = contiguous
            && source.generation_mtime_ns != 0
            && source.source_file != SourceFileIdentity::Unavailable
            && self.pending_source == Some(source);
        let mut decoded = self.decode(chunk, offset);
        decoded.mixed_read_provenance &= !same_source;
        if !chunk.is_empty() {
            self.pending_source =
                (!self.pending.is_empty() && (!had_pending || same_source)).then_some(source);
        }
        decoded
    }

    /// #5979 (I18): the read that refills an EMPTY `all_data`. A watcher rewind
    /// empties the buffer and re-anchors it at the rewound cursor without reaching
    /// this decoder, so gluing the abandoned read's split-scalar tail onto the
    /// replay would re-anchor `all_data` at the tail's offset and shift every
    /// forwarded `source_span` off the file. A read that does not continue the
    /// tail drops it; a continuing read (#5833 cancelled-turn resume), an empty
    /// read, and any read into a non-empty buffer keep the ordinary carry.
    pub(super) fn decode_source_for_buffer(
        &mut self,
        chunk: &[u8],
        offset: u64,
        source: super::loop_poll_prologue::WatcherSourceAuthority,
        buffer: &str,
    ) -> DecodedUtf8Chunk {
        if buffer.is_empty() && !chunk.is_empty() && self.pending_end_offset() != Some(offset) {
            self.clear_pending();
        }
        self.decode_source(chunk, offset, source)
    }

    /// The file offset one past the buffered tail, when there is one.
    fn pending_end_offset(&self) -> Option<u64> {
        self.pending_start_offset
            .and_then(|start| start.checked_add(self.pending.len() as u64))
    }

    fn decode(&mut self, chunk: &[u8], chunk_start_offset: u64) -> DecodedUtf8Chunk {
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

    pub(super) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub(super) fn clear_pending(&mut self) {
        self.pending_source = None;
        self.pending.clear();
        self.pending_start_offset = None;
    }
}

#[cfg(test)]
#[path = "utf8_chunk_decoder_tests.rs"]
mod tests;
