//! #3479 Phase-1 rank-2 extraction: the tmux watcher's streaming UTF-8 chunk
//! decoder — `Utf8ChunkDecoder` + its `DecodedUtf8Chunk` result, which buffer a
//! partial trailing multibyte scalar across read boundaries so a code point
//! split between two `read()` chunks is never emitted as `U+FFFD`. PURE MOVE from
//! `tmux_watcher.rs` (zero logic change) to shrink the frozen root file below its
//! maintainability baseline.
//!
//! Fully self-contained: depends only on `std` (`std::str`, `Vec`), with ZERO
//! coupling to `shared`/`http`/`InflightTurnState`. Items are `pub(super)` so the
//! parent watcher loop keeps constructing/driving the decoder by its original
//! name. This module references nothing from `super`, so it intentionally omits
//! the `use super::*;` glob the sibling modules carry.

#[derive(Debug, Default)]
pub(super) struct Utf8ChunkDecoder {
    pending: Vec<u8>,
    pending_start_offset: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct DecodedUtf8Chunk {
    pub(super) start_offset: Option<u64>,
    pub(super) text: String,
}

impl Utf8ChunkDecoder {
    pub(super) fn decode(&mut self, chunk: &[u8], chunk_start_offset: u64) -> DecodedUtf8Chunk {
        if chunk.is_empty() {
            return DecodedUtf8Chunk {
                start_offset: None,
                text: String::new(),
            };
        }
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
                    text,
                }
            }
            Err(err) if err.error_len().is_none() => {
                let valid_up_to = err.valid_up_to();
                if valid_up_to == 0 {
                    return DecodedUtf8Chunk {
                        start_offset: None,
                        text: String::new(),
                    };
                }
                let text = std::str::from_utf8(&self.pending[..valid_up_to])
                    .expect("valid UTF-8 prefix")
                    .to_string();
                self.pending.drain(..valid_up_to);
                self.pending_start_offset = Some(start_offset.saturating_add(valid_up_to as u64));
                DecodedUtf8Chunk {
                    start_offset: Some(start_offset),
                    text,
                }
            }
            Err(_) => {
                let text = String::from_utf8_lossy(&self.pending).into_owned();
                self.pending.clear();
                self.pending_start_offset = None;
                DecodedUtf8Chunk {
                    start_offset: Some(start_offset),
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
