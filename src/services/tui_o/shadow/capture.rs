//! Read-only transcript capture: complete lines only, identity and prefix checked each poll.

use std::fs::File;

use super::{CaptureOutcome, CaptureSource, SourceId};

pub struct SourceCapture {
    source: SourceId,
    file: File,
    captured_through: u64,
}

impl SourceCapture {
    /// Opens `source.path` read-only and starts capturing at `start_offset`.
    pub fn open(source: SourceId, start_offset: u64) -> std::io::Result<Self> {
        let _ = (source, start_offset);
        todo!()
    }

    pub fn captured_through(&self) -> u64 {
        self.captured_through
    }

    /// Hex sha256 of bytes `0..captured_through`.
    pub fn prefix_hash(&self) -> String {
        todo!()
    }
}

impl CaptureSource for SourceCapture {
    fn source(&self) -> &SourceId {
        &self.source
    }

    fn poll(&mut self, max_bytes: u64) -> CaptureOutcome {
        let _ = (max_bytes, &self.file);
        todo!()
    }
}
