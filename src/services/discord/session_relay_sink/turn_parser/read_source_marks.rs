//! Which read file the sink parser is folding, and what each other file left
//! behind in the open turn: its resend watermark and its incomplete trailing
//! JSONL record. Records from different files are never joined into one line.

use std::collections::VecDeque;

use crate::services::cluster::stream_relay::{ReadSource, SourceFileIdentity, SourceStamp};

/// Files whose watermark and partial record survive while another file is active.
const PARKED_FILE_CAP: usize = 3;

pub(super) type BufferSegments = VecDeque<(usize, Option<SourceStamp>)>;

/// The fold decision for one frame's read source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Admission {
    /// Fold under the active file's watermark; frames without a source land here.
    Fold,
    /// A proven different file read by the same or a newer watcher.
    Switch(ReadSource),
    /// A different file read by a watcher older than the active one.
    DropStale,
}

struct ParkedFile {
    file: SourceFileIdentity,
    watermark: Option<u64>,
    tail: String,
    tail_segments: BufferSegments,
}

/// The parser's `buffer`, `buffer_source_segments` and `turn_source_end` always
/// describe the active file; `parked` holds the others for the open turn.
#[derive(Default)]
pub(super) struct ReadSourceMarks {
    active: Option<ReadSource>,
    parked: VecDeque<ParkedFile>,
}

impl ReadSourceMarks {
    pub(super) fn classify(&self, read_source: Option<ReadSource>) -> Admission {
        let source = read_source.filter(|source| source.file != SourceFileIdentity::Unavailable);
        match (source, self.active) {
            (Some(source), Some(active)) if source.file != active.file => {
                if source.producer < active.producer {
                    Admission::DropStale
                } else {
                    Admission::Switch(source)
                }
            }
            _ => Admission::Fold,
        }
    }

    /// Remember the source of a frame folded under the active file.
    pub(super) fn note_folded(&mut self, read_source: Option<ReadSource>) {
        let Some(source) =
            read_source.filter(|source| source.file != SourceFileIdentity::Unavailable)
        else {
            return;
        };
        match self.active.as_mut() {
            Some(active) if active.file == source.file => {
                active.producer = active.producer.max(source.producer);
            }
            _ => self.active = Some(source),
        }
    }

    /// Park the active file's incomplete trailing record and watermark, then make
    /// `to` active with whatever it parked earlier in this turn. Complete records
    /// already in the buffer stay in the shared stream.
    pub(super) fn switch(
        &mut self,
        to: ReadSource,
        buffer: &mut String,
        segments: &mut BufferSegments,
        watermark: &mut Option<u64>,
    ) {
        let tail_start = buffer.rfind('\n').map_or(0, |newline| newline + 1);
        let tail = buffer.split_off(tail_start);
        let tail_segments = split_off_back(segments, tail.len());
        let parked_watermark = watermark.take();
        if let Some(active) = self.active {
            self.parked.retain(|parked| parked.file != active.file);
            self.parked.push_back(ParkedFile {
                file: active.file,
                watermark: parked_watermark,
                tail,
                tail_segments,
            });
            if self.parked.len() > PARKED_FILE_CAP {
                self.parked.pop_front();
            }
        }
        if let Some(index) = self.parked.iter().position(|parked| parked.file == to.file)
            && let Some(parked) = self.parked.remove(index)
        {
            *watermark = parked.watermark;
            buffer.push_str(&parked.tail);
            segments.extend(parked.tail_segments);
        }
        self.active = Some(to);
    }

    /// Parked watermarks and partial records belong to the open turn only.
    pub(super) fn reset_turn(&mut self) {
        self.parked.clear();
    }
}

/// Split the last `len` bytes' worth of segments off the back of `segments`.
fn split_off_back(segments: &mut BufferSegments, mut len: usize) -> BufferSegments {
    let mut back = VecDeque::new();
    while len > 0 {
        let Some((segment_len, stamp)) = segments.pop_back() else {
            break;
        };
        let taken = len.min(segment_len);
        back.push_front((taken, stamp));
        if taken < segment_len {
            segments.push_back((segment_len - taken, stamp));
        }
        len -= taken;
    }
    back
}
