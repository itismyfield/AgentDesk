//! Reuses a source census only while its cursor and full file fingerprint still match; an
//! appended source resumes from its last prefix-proven cursor instead of rescanning from byte 0.
use super::*;
use crate::services::tui_o::shadow::{SourceId, capture::Fingerprint};
#[cfg(unix)]
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::sync::Arc;

#[cfg(any(test, feature = "test-support"))]
#[path = "census/census_tests.rs"]
pub(super) mod tests;

#[cfg(unix)]
const MAX_SOURCES: usize = 128;
#[cfg(unix)]
const MAX_BYTES: usize = 8 * 1024 * 1024;
/// Bytes re-read behind a resume point, as O capture re-reads behind its cursor each poll.
const GUARD_BYTES: usize = 4096;

#[cfg(test)]
thread_local! {
    /// Source bytes the census consumed on this thread, for the resume cost tests.
    pub(super) static SCANNED_BYTES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A structurally complete scan of `0..at` whose digest matched that cursor's prefix hash.
struct Resume {
    at: u64,
    /// The file the bytes came from and its length then: only a strictly longer same file resumes.
    file: (u64, u64),
    len: u64,
    digest: Sha256,
    guard: Vec<u8>,
    rows: Arc<Vec<Taken>>,
}

impl Resume {
    fn extends(&self, now: &Fingerprint, through: u64) -> bool {
        (now.dev, now.ino) == self.file && now.len > self.len && through >= self.at
    }

    /// The bytes just behind the resume point are still the ones hashed, as O capture checks.
    fn guard_holds(&self, mut file: &File) -> bool {
        let start = self.at - self.guard.len() as u64;
        let mut now = vec![0; self.guard.len()];
        file.seek(SeekFrom::Start(start)).is_ok()
            && file.read_exact(&mut now).is_ok()
            && now == self.guard
    }
}

#[cfg(unix)]
struct Kept {
    source: SourceId,
    through: u64,
    prefix: String,
    /// The snapshot `rows` answers for; `None` keeps only a resume point.
    fingerprint: Option<Fingerprint>,
    rows: Option<Arc<Vec<Taken>>>,
    resume: Option<Resume>,
    bytes: usize,
}

#[derive(Default)]
pub(super) struct Cache {
    #[cfg(unix)]
    entries: VecDeque<Kept>,
    #[cfg(unix)]
    bytes: usize,
}

/// The scan cannot answer now; a prefix-proven read still leaves its resume point.
struct Transient(Option<Resume>);

struct Scanned {
    rows: Option<Arc<Vec<Taken>>>,
    resume: Option<Resume>,
}

pub(super) fn fingerprint(source: &SourceId) -> Option<Fingerprint> {
    let fp = Fingerprint::of(&std::fs::metadata(&source.path).ok()?).ok()?;
    (fp.dev == source.dev && fp.ino == source.ino).then_some(fp)
}

impl Cache {
    pub(super) fn read(
        &mut self,
        source: &SourceId,
        through: u64,
        prefix: &str,
    ) -> (Option<Fingerprint>, Option<Arc<Vec<Taken>>>) {
        let Some(before) = fingerprint(source).filter(|fp| fp.len >= through) else {
            return (None, None);
        };
        #[cfg(unix)]
        let mut resume = None;
        #[cfg(not(unix))]
        let resume = None;
        #[cfg(unix)]
        {
            if let Some(index) = self.entries.iter().position(|e| e.source == *source) {
                let kept = &self.entries[index];
                if kept.through == through
                    && kept.prefix == prefix
                    && kept.fingerprint.as_ref() == Some(&before)
                    && fingerprint(source).as_ref() == Some(&before)
                {
                    return (Some(before), kept.rows.clone());
                }
                if let Some(removed) = self.entries.remove(index) {
                    self.bytes -= removed.bytes;
                    resume = removed.resume;
                }
            }
        }
        match scan(source, through, prefix, &before, resume) {
            Ok(scanned) => {
                #[cfg(unix)]
                self.keep(
                    source,
                    (through, prefix),
                    Some(&before),
                    &scanned.rows,
                    scanned.resume,
                );
                (Some(before), scanned.rows)
            }
            Err(Transient(proven)) => {
                #[cfg(unix)]
                if let Some(point) = proven {
                    self.keep(source, (point.at, ""), None, &None, Some(point));
                }
                #[cfg(not(unix))]
                drop(proven);
                (None, None)
            }
        }
    }

    #[cfg(unix)]
    fn keep(
        &mut self,
        source: &SourceId,
        (through, prefix): (u64, &str),
        fingerprint: Option<&Fingerprint>,
        rows: &Option<Arc<Vec<Taken>>>,
        resume: Option<Resume>,
    ) {
        let counted = rows.as_ref().or(resume.as_ref().map(|point| &point.rows));
        let bytes = counted.map_or(0, |rows| {
            rows.capacity() * std::mem::size_of::<Taken>()
                + rows
                    .iter()
                    .map(|row| {
                        row.uuid.capacity()
                            + row.hashes.capacity() * std::mem::size_of::<String>()
                            + row.hashes.iter().map(String::capacity).sum::<usize>()
                    })
                    .sum::<usize>()
        }) + resume.as_ref().map_or(0, |point| point.guard.capacity());
        if bytes > MAX_BYTES {
            return;
        }
        while self.entries.len() >= MAX_SOURCES || self.bytes + bytes > MAX_BYTES {
            let Some(removed) = self.entries.pop_front() else {
                break;
            };
            self.bytes -= removed.bytes;
        }
        self.bytes += bytes;
        self.entries.push_back(Kept {
            source: source.clone(),
            through,
            prefix: prefix.into(),
            fingerprint: fingerprint.cloned(),
            rows: rows.clone(),
            resume,
            bytes,
        });
    }
}

fn scan(
    source: &SourceId,
    through: u64,
    prefix: &str,
    before: &Fingerprint,
    resume: Option<Resume>,
) -> Result<Scanned, Transient> {
    #[cfg(any(test, feature = "test-support"))]
    CENSUS_READS.with(|count| count.set(count.get() + 1));
    #[cfg(any(test, feature = "test-support"))]
    tests::point(tests::Stage::Open, &source.path).map_err(|_| Transient(None))?;
    let file = File::open(&source.path).map_err(|_| Transient(None))?;
    #[cfg(any(test, feature = "test-support"))]
    tests::point(tests::Stage::Metadata, &source.path).map_err(|_| Transient(None))?;
    let opened = Fingerprint::of(&file.metadata().map_err(|_| Transient(None))?)
        .map_err(|_| Transient(None))?;
    if opened != *before || opened.len < through {
        return Err(Transient(None));
    }
    #[cfg(any(test, feature = "test-support"))]
    tests::point(tests::Stage::Read, &source.path).map_err(|_| Transient(None))?;
    let start = resume
        .filter(|point| point.extends(before, through) && point.guard_holds(&file))
        .map_or_else(Progress::default, Progress::from);
    let scanned = match rows(&file, start, through)? {
        Some(progress) if hex::encode(progress.digest.clone().finalize()) == prefix => {
            let rows = Arc::new(progress.rows);
            Scanned {
                rows: Some(rows.clone()),
                resume: Some(Resume {
                    at: through,
                    file: (before.dev, before.ino),
                    len: before.len,
                    digest: progress.digest,
                    guard: progress.guard,
                    rows,
                }),
            }
        }
        _ => Scanned {
            rows: None,
            resume: None,
        },
    };
    #[cfg(any(test, feature = "test-support"))]
    tests::point(tests::Stage::AfterScan, &source.path).map_err(|_| Transient(None))?;
    let after = Fingerprint::of(&file.metadata().map_err(|_| Transient(None))?)
        .map_err(|_| Transient(None))?;
    if after != *before || fingerprint(source).as_ref() != Some(before) {
        return Err(Transient(scanned.resume));
    }
    Ok(scanned)
}

/// Rows, digest and trailing guard bytes of `0..at`.
#[derive(Default)]
struct Progress {
    at: u64,
    digest: Sha256,
    guard: Vec<u8>,
    rows: Vec<Taken>,
}

impl From<Resume> for Progress {
    fn from(point: Resume) -> Self {
        Self {
            at: point.at,
            digest: point.digest,
            guard: point.guard,
            rows: Arc::try_unwrap(point.rows).unwrap_or_else(|rows| rows.as_ref().clone()),
        }
    }
}

fn keep_guard(guard: &mut Vec<u8>, line: &[u8]) {
    if line.len() >= GUARD_BYTES {
        guard.clear();
        guard.extend_from_slice(&line[line.len() - GUARD_BYTES..]);
        return;
    }
    guard.extend_from_slice(line);
    let excess = guard.len().saturating_sub(GUARD_BYTES);
    guard.drain(..excess);
}

/// Extends `progress` to `through`; `None` once a line is torn, unparsable or has no uuid.
fn rows(
    mut file: &File,
    mut progress: Progress,
    through: u64,
) -> Result<Option<Progress>, Transient> {
    file.seek(SeekFrom::Start(progress.at))
        .map_err(|_| Transient(None))?;
    let mut reader = std::io::BufReader::new(file.take(through - progress.at));
    let mut line = Vec::new();
    loop {
        if reader
            .read_until(b'\n', &mut line)
            .map_err(|_| Transient(None))?
            == 0
        {
            break;
        }
        #[cfg(test)]
        SCANNED_BYTES.with(|count| count.set(count.get() + line.len() as u64));
        let offset = progress.at;
        progress.at += line.len() as u64;
        if !line.ends_with(b"\n") {
            return Ok(None);
        }
        progress.digest.update(&line);
        keep_guard(&mut progress.guard, &line);
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&line) else {
            return Ok(None);
        };
        line.clear();
        let text = if value["type"] == "user"
            && value["isMeta"] != true
            && !crate::services::tui_o::shadow::identity::record_is_compact_summary(&value)
        {
            exact_text(&value["message"]["content"])
        } else if value["type"] == "attachment" && value["attachment"]["type"] == "queued_command" {
            exact_text(&value["attachment"]["prompt"])
        } else {
            None
        };
        if let Some(text) = text {
            let Some(uuid) = value["uuid"].as_str() else {
                return Ok(None);
            };
            progress.rows.push(Taken {
                offset,
                uuid: uuid.into(),
                hashes: class_hashes(&text),
                user: value["type"] == "user",
            });
        }
    }
    if progress.at != through {
        return Err(Transient(None));
    }
    Ok(Some(progress))
}

fn exact_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .map(|b| (b["type"] == "text").then_some(b["text"].as_str()?))
            .collect::<Option<Vec<_>>>()
            .map(|texts| texts.join("\n")),
        _ => None,
    }
}
