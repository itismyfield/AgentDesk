//! Read-only bounded observation with no durable prefix proof or checkpoint API.
use super::{
    CaptureBatch, CaptureOutcome, CaptureSource, CapturedRecord, Kind, SourceAnomaly, SourceId,
};
use super::{
    Fingerprint, MAX_PARTIAL_BYTES, TAIL_GUARD_BYTES, file_identity, read_at, update_guard,
};
use std::fs::File;
use std::io;

pub struct ObservationCapture {
    source: SourceId,
    file: File,
    opened: (u64, u64),
    captured_through: u64,
    cursor: u64,
    partial: Vec<u8>,
    guard: Vec<u8>,
    aligning: bool,
    forward_bytes: u64,
    halted: Option<SourceAnomaly>,
}

impl ObservationCapture {
    pub fn open(source: SourceId, offset: u64) -> io::Result<Self> {
        let file = File::open(&source.path)?;
        let meta = file.metadata()?;
        let opened = file_identity(&meta);
        let path = std::fs::metadata(&source.path)?;
        if opened != (source.dev, source.ino)
            || file_identity(&path) != opened
            || offset > meta.len()
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let guard_len = offset.min(TAIL_GUARD_BYTES);
        let guard = read_at(&file, offset - guard_len, guard_len)?;
        Ok(Self {
            aligning: guard.last().is_some_and(|b| *b != b'\n'),
            source,
            file,
            opened,
            captured_through: offset,
            cursor: offset,
            partial: Vec::new(),
            guard,
            forward_bytes: 0,
            halted: None,
        })
    }
    pub fn captured_through(&self) -> u64 {
        self.captured_through
    }
    pub fn read_through(&self) -> u64 {
        self.cursor
    }
    pub fn forward_bytes(&self) -> u64 {
        self.forward_bytes
    }
    pub fn file_len(&self) -> io::Result<u64> {
        self.validate(self.cursor)
            .map_err(|(_, detail)| io::Error::other(detail))
    }
    pub fn caught_up(&self) -> io::Result<bool> {
        Ok(!self.aligning && self.captured_through == self.file_len()?)
    }
    fn validate(&self, cursor: u64) -> Result<u64, (Kind, String)> {
        let len = self
            .file
            .metadata()
            .map_err(|e| (Kind::Unreadable, e.to_string()))?
            .len();
        let path = Fingerprint::path(&self.source)?;
        if (path.dev, path.ino) != self.opened {
            return Err((Kind::Replaced, "path no longer names the open file".into()));
        }
        if len < cursor {
            return Err((Kind::Shrunk, format!("len {len} < cursor {cursor}")));
        }
        Ok(len)
    }
    fn poll_inner(
        &mut self,
        max: u64,
        end: Option<(u64, u64)>,
    ) -> Result<CaptureBatch, (Kind, String)> {
        let io_err = |e: io::Error| (Kind::Unreadable, e.to_string());
        let len = self.validate(self.cursor)?;
        let guard_len = self.guard.len() as u64;
        if read_at(&self.file, self.cursor - guard_len, guard_len).map_err(io_err)? != self.guard {
            return Err((
                Kind::PrefixMismatch,
                "bytes behind the cursor changed".into(),
            ));
        }
        // Bootstrap reserves partial revalidation reads; installed polls limit only new bytes.
        let reserved = end.map_or(0, |_| self.partial.len() as u64);
        let budget = end.map_or(max, |(_, total)| total.saturating_sub(reserved).min(max));
        let limit = len
            .min(end.map_or(len, |(end, _)| end))
            .saturating_sub(self.cursor)
            .min(budget);
        let limit = if self.aligning {
            limit.min(8192)
        } else {
            limit
        };
        let chunk = read_at(&self.file, self.cursor, limit).map_err(io_err)?;
        self.forward_bytes += chunk.len() as u64;
        self.validate(self.cursor + chunk.len() as u64)?;
        let skip = if self.aligning {
            match chunk.iter().position(|b| *b == b'\n') {
                Some(at) => {
                    self.aligning = false;
                    at + 1
                }
                None => chunk.len(),
            }
        } else {
            0
        };
        let incoming = &chunk[skip..];
        let last_newline = incoming.iter().rposition(|b| *b == b'\n');
        let rest = last_newline.map_or(self.partial.len() + incoming.len(), |at| {
            incoming.len() - at - 1
        });
        if rest > MAX_PARTIAL_BYTES {
            return Err((
                Kind::Oversized,
                format!("record without newline passed {rest} bytes"),
            ));
        }
        if last_newline.is_some() && !self.partial.is_empty() {
            let disk = read_at(&self.file, self.captured_through, self.partial.len() as u64)
                .map_err(io_err)?;
            self.forward_bytes += disk.len() as u64;
            if disk != self.partial {
                return Err((Kind::PrefixMismatch, "buffered partial line changed".into()));
            }
        }
        update_guard(&mut self.guard, &chunk);
        self.cursor += chunk.len() as u64;
        self.captured_through += skip as u64;
        let mut search = self.partial.len();
        self.partial.extend_from_slice(incoming);
        let (mut records, mut consumed) = (Vec::new(), 0);
        while let Some(pos) = self.partial[search..].iter().position(|b| *b == b'\n') {
            let end = search + pos + 1;
            records.push(CapturedRecord {
                start: self.captured_through + consumed as u64,
                end: self.captured_through + end as u64,
                line: self.partial[consumed..end - 1].to_vec(),
            });
            (consumed, search) = (end, end);
        }
        self.partial.drain(..consumed);
        self.captured_through += consumed as u64;
        Ok(CaptureBatch {
            source: self.source.clone(),
            records,
            captured_through: self.captured_through,
        })
    }
    fn capture(&mut self, max: u64, end: Option<(u64, u64)>) -> CaptureOutcome {
        if let Some(anomaly) = &self.halted {
            return CaptureOutcome::Anomaly(anomaly.clone());
        }
        let (mut remaining, mut end) = (max, end);
        let (kind, detail) = loop {
            let before = (self.cursor, self.forward_bytes);
            match self.poll_inner(remaining, end) {
                Ok(batch) => {
                    let advanced = self.cursor - before.0;
                    remaining = remaining.saturating_sub(advanced);
                    if let Some((_, budget)) = &mut end {
                        *budget = budget.saturating_sub(self.forward_bytes - before.1);
                    }
                    // Discard with fixed buffers while using this poll's available byte budget.
                    if self.aligning
                        && advanced > 0
                        && remaining > 0
                        && end.is_none_or(|(_, budget)| budget > 0)
                    {
                        continue;
                    }
                    return CaptureOutcome::Batch(batch);
                }
                Err(error) => break error,
            }
        };
        let anomaly = SourceAnomaly {
            source: self.source.clone(),
            kind,
            captured_through: self.captured_through,
            detail,
        };
        self.halted = Some(anomaly.clone());
        CaptureOutcome::Anomaly(anomaly)
    }
    /// Includes forward reads and partial rereads, and never reads beyond the initial snapshot.
    pub fn poll_until(&mut self, max: u64, end: u64, budget: u64) -> CaptureOutcome {
        self.capture(max, Some((end, budget)))
    }
}
impl CaptureSource for ObservationCapture {
    fn source(&self) -> &SourceId {
        &self.source
    }
    fn poll(&mut self, max: u64) -> CaptureOutcome {
        self.capture(max, None)
    }
}
#[cfg(test)]
mod observation_tests;
