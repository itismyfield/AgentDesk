//! Reuses a source census only while its cursor and full file fingerprint still match.
use super::*;
use crate::services::tui_o::shadow::{SourceId, capture::Fingerprint};
#[cfg(unix)]
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, Read};
use std::sync::Arc;

#[cfg(test)]
#[path = "census/census_tests.rs"]
pub(super) mod tests;

#[cfg(unix)]
const MAX_SOURCES: usize = 128;
#[cfg(unix)]
const MAX_BYTES: usize = 8 * 1024 * 1024;

#[cfg(unix)]
struct Kept {
    source: SourceId,
    through: u64,
    prefix: String,
    fingerprint: Fingerprint,
    rows: Option<Arc<Vec<Taken>>>,
    bytes: usize,
}

#[derive(Default)]
pub(super) struct Cache {
    #[cfg(unix)]
    entries: VecDeque<Kept>,
    #[cfg(unix)]
    bytes: usize,
}

struct Transient;

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
        {
            if let Some(index) = self.entries.iter().position(|e| e.source == *source) {
                let kept = &self.entries[index];
                if kept.through == through
                    && kept.prefix == prefix
                    && kept.fingerprint == before
                    && fingerprint(source).as_ref() == Some(&before)
                {
                    return (Some(before), kept.rows.clone());
                }
                if let Some(removed) = self.entries.remove(index) {
                    self.bytes -= removed.bytes;
                }
            }
        }
        let Ok(rows) = scan(source, through, prefix, &before) else {
            return (None, None);
        };
        #[cfg(unix)]
        self.keep(source, through, prefix, &before, &rows);
        (Some(before), rows)
    }

    #[cfg(unix)]
    fn keep(
        &mut self,
        source: &SourceId,
        through: u64,
        prefix: &str,
        fingerprint: &Fingerprint,
        rows: &Option<Arc<Vec<Taken>>>,
    ) {
        let bytes = rows.as_ref().map_or(0, |rows| {
            rows.capacity() * std::mem::size_of::<Taken>()
                + rows
                    .iter()
                    .map(|row| {
                        row.uuid.capacity()
                            + row.hashes.capacity() * std::mem::size_of::<String>()
                            + row.hashes.iter().map(String::capacity).sum::<usize>()
                    })
                    .sum::<usize>()
        });
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
            fingerprint: fingerprint.clone(),
            rows: rows.clone(),
            bytes,
        });
    }
}

fn scan(
    source: &SourceId,
    through: u64,
    prefix: &str,
    before: &Fingerprint,
) -> Result<Option<Arc<Vec<Taken>>>, Transient> {
    #[cfg(test)]
    CENSUS_READS.with(|count| count.set(count.get() + 1));
    #[cfg(test)]
    tests::point(tests::Stage::Open, &source.path).map_err(|_| Transient)?;
    let file = File::open(&source.path).map_err(|_| Transient)?;
    #[cfg(test)]
    tests::point(tests::Stage::Metadata, &source.path).map_err(|_| Transient)?;
    let opened =
        Fingerprint::of(&file.metadata().map_err(|_| Transient)?).map_err(|_| Transient)?;
    if opened != *before || opened.len < through {
        return Err(Transient);
    }
    #[cfg(test)]
    tests::point(tests::Stage::Read, &source.path).map_err(|_| Transient)?;
    let rows = rows(&file, through, prefix)?;
    #[cfg(test)]
    tests::point(tests::Stage::AfterScan, &source.path).map_err(|_| Transient)?;
    let after = Fingerprint::of(&file.metadata().map_err(|_| Transient)?).map_err(|_| Transient)?;
    if after != *before || fingerprint(source).as_ref() != Some(before) {
        return Err(Transient);
    }
    Ok(rows.map(Arc::new))
}

fn rows(file: &File, through: u64, prefix: &str) -> Result<Option<Vec<Taken>>, Transient> {
    let mut reader = std::io::BufReader::new(file.take(through));
    let mut digest = Sha256::new();
    let (mut rows, mut line, mut at) = (Vec::new(), Vec::new(), 0);
    loop {
        if reader.read_until(b'\n', &mut line).map_err(|_| Transient)? == 0 {
            break;
        }
        let offset = at;
        at += line.len() as u64;
        if !line.ends_with(b"\n") {
            return Ok(None);
        }
        digest.update(&line);
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&line) else {
            return Ok(None);
        };
        line.clear();
        let text = if value["type"] == "user"
            && value["isMeta"] != true
            && !crate::services::tui_o::shadow::identity::record_is_compact_summary(&value)
        {
            match &value["message"]["content"] {
                serde_json::Value::String(text) => Some(text.clone()),
                serde_json::Value::Array(blocks) => blocks
                    .iter()
                    .map(|b| (b["type"] == "text").then(|| b["text"].as_str()))
                    .collect::<Option<Option<Vec<_>>>>()
                    .flatten()
                    .map(|v| v.join("\n")),
                _ => None,
            }
        } else if value["type"] == "attachment" && value["attachment"]["type"] == "queued_command" {
            value["attachment"]["prompt"].as_str().map(str::to_owned)
        } else {
            None
        };
        if let Some(text) = text {
            let Some(uuid) = value["uuid"].as_str() else {
                return Ok(None);
            };
            rows.push(Taken {
                offset,
                uuid: uuid.into(),
                hashes: class_hashes(&text),
                user: value["type"] == "user",
            });
        }
    }
    if at != through {
        return Err(Transient);
    }
    Ok((hex::encode(digest.finalize()) == prefix).then_some(rows))
}
