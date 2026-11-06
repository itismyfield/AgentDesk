//! Notice attribution is historical and source-bound; it grants no input or stop authority.
use super::*;
use crate::services::tui_o::store::ChannelStore;
use crate::services::tui_o::writer::input_notices::NoticeOrigin;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Owned,
    Unowned,
    Pending(&'static str),
}

type Stamp = ([u64; 2], u64, std::time::SystemTime);
type Answers = HashMap<String, (u64, Vec<String>, Decision)>;

struct Scope {
    origin: NoticeOrigin,
    through: u64,
    native_stamp: Option<Stamp>,
    exact: Answers,
    census: Answers,
}

#[derive(Default)]
pub(crate) struct Batch {
    stamp: Option<Stamp>,
    scopes: Vec<Scope>,
}

fn file_stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((file_id(&meta), meta.len(), meta.modified().ok()?))
}

fn stamp() -> Option<Stamp> {
    file_stamp(&path().ok()?)
}

#[cfg(test)]
thread_local! {
    pub(crate) static CENSUS_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn same_scope(a: &NoticeOrigin, b: &NoticeOrigin) -> bool {
    a.record.source == b.record.source
        && a.logical_session == b.logical_session
        && a.execution_nonce == b.execution_nonce
        && a.binding_seq == b.binding_seq
}

impl Batch {
    pub(crate) fn prepare(store: &ChannelStore) -> Self {
        let before = stamp();
        let snapshot = path().and_then(|p| match std::fs::read(p) {
            Ok(bytes) => compat::parse(&bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(serde_json::json!({"entries":[]}))
            }
            Err(e) => Err(e.to_string()),
        });
        let mut batch = Self {
            stamp: before,
            scopes: Vec::new(),
        };
        let mut native = HashMap::new();
        for evidence in store.ledger().input_notices().evidence.values() {
            if evidence.unit_key.as_ref().is_some_and(|key| {
                store.ledger().excluded(key).is_some()
                    || store.ledger().latest_piece(key, 0).is_some_and(|(_, p)| {
                        matches!(
                            p.outcome,
                            Some(crate::services::tui_o::store::ledger::PieceOutcome::Posted(
                                _
                            ))
                        )
                    })
            }) {
                continue;
            }
            let Some(origin) = evidence.origin.as_deref() else {
                continue;
            };
            if batch.scopes.iter().any(|s| same_scope(&s.origin, origin)) {
                continue;
            }
            let (through, native_stamp, rows) = native
                .entry(origin.record.source.clone())
                .or_insert_with(|| {
                    let cursor = store.cursor(&origin.record.source);
                    let through = cursor.map_or(0, |c| c.captured_through);
                    let prefix = cursor.map_or("", |c| c.prefix_hash.as_str());
                    let before = file_stamp(&origin.record.source.path);
                    let rows = native_rows(origin, 0, through, prefix);
                    let after = file_stamp(&origin.record.source.path);
                    (through, after, if before == after { rows } else { None })
                });
            let (exact, census) = snapshot
                .as_ref()
                .ok()
                .and_then(|v| candidates(v, origin).ok())
                .map(|(entries, blocked)| attribute(&entries, &blocked, rows.as_deref()))
                .unwrap_or_default();
            batch.scopes.push(Scope {
                origin: origin.clone(),
                through: *through,
                native_stamp: *native_stamp,
                exact,
                census,
            });
        }
        if before != stamp() {
            batch.scopes.clear();
        }
        batch
    }

    pub(crate) fn decision(&self, origin: &NoticeOrigin, row: &str, prompt: &str) -> Decision {
        if self.stamp != stamp() {
            return Decision::Pending("history_snapshot_changed");
        }
        let Some(scope) = self.scopes.iter().find(|s| same_scope(&s.origin, origin)) else {
            return Decision::Pending("original_binding_missing");
        };
        let hashes = class_hashes(prompt);
        if let Some((offset, frames, answer)) = scope.exact.get(row)
            && *offset <= origin.record.start
            && frames.iter().any(|h| hashes.contains(h))
        {
            return *answer;
        }
        let source = &origin.record.source;
        let valid = scope.native_stamp.is_some() && file_stamp(&source.path) == scope.native_stamp;
        if !valid || origin.record.end > scope.through {
            return Decision::Pending("native_census_unavailable");
        }
        scope
            .census
            .get(row)
            .filter(|(offset, frames, _)| {
                *offset == origin.record.start && frames.iter().any(|h| hashes.contains(h))
            })
            .map_or(
                Decision::Pending("native_census_unavailable"),
                |(_, _, answer)| *answer,
            )
    }
}

type Blocked = Vec<(String, &'static str)>;

fn candidates(
    value: &serde_json::Value,
    origin: &NoticeOrigin,
) -> Result<(Vec<Entry>, Blocked), String> {
    let source = &origin.record.source;
    let same = |e: &Entry| {
        e.provider == "claude"
            && e.session == origin.logical_session
            && e.path == source.path
            && e.file == [source.dev, source.ino]
    };
    if !compat::envelope(value)? {
        let store: Store = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
        let blocked = store
            .entries
            .iter()
            .filter(|e| e.provider == "claude" && e.session == origin.logical_session && !same(e))
            .map(|e| (e.class_hash.clone(), "registration_source_mismatch"))
            .collect();
        return Ok((store.entries.into_iter().filter(same).collect(), blocked));
    }
    let mut entries = Vec::new();
    let mut blocked = Vec::new();
    let bound = value["live"]
        .as_array()
        .ok_or("history live missing")?
        .iter()
        .map(|v| (v, &v["registration"]))
        .chain(
            value["passive"]
                .as_array()
                .ok_or("history passive missing")?
                .iter()
                .filter(|v| v["state"] != "abandoned")
                .map(|v| (&v["entry"], &v["registration"])),
        );
    for (raw, registration) in bound {
        if raw.is_null() {
            if relevant(registration, origin) {
                blocked.push((frame_hash(registration)?, "unbound_pre_source"));
            }
            continue;
        }
        let entry: Entry = serde_json::from_value(raw.clone()).map_err(|e| e.to_string())?;
        if entry.provider != "claude" || entry.session != origin.logical_session {
            continue;
        }
        if !same(&entry) || (!registration.is_null() && !relevant(registration, origin)) {
            blocked.push((entry.class_hash.clone(), "registration_source_mismatch"));
            continue;
        }
        // Migrated V1 evidence has no execution nonce or native-session field to invent.
        if registration.is_null() || historical_match(registration, &entry, origin)? {
            entries.push(entry);
        } else {
            blocked.push((entry.class_hash, "registration_source_mismatch"));
        }
    }
    let planned = match value.get("planned") {
        None => &[][..],
        Some(serde_json::Value::Array(values)) => values.as_slice(),
        _ => return Err("history planned invalid".into()),
    };
    for registration in planned {
        if relevant(registration, origin) {
            blocked.push((frame_hash(registration)?, "unbound_pre_source"));
        }
    }
    Ok((entries, blocked))
}

fn frame_hash(registration: &serde_json::Value) -> Result<String, String> {
    registration["frame"]["class_hash"]
        .as_str()
        .map(str::to_owned)
        .ok_or("frame hash missing".into())
}

fn relevant(registration: &serde_json::Value, origin: &NoticeOrigin) -> bool {
    let attempt = &registration["key"]["attempt"];
    attempt["provider"] == "claude"
        && attempt["target"]["owner"] == origin.logical_session
        && attempt["target"]["execution_nonce"] == origin.execution_nonce
}

fn historical_match(
    registration: &serde_json::Value,
    entry: &Entry,
    origin: &NoticeOrigin,
) -> Result<bool, String> {
    let source = &origin.record.source;
    let bound: v2::Source =
        serde_json::from_value(registration["source"].clone()).map_err(|e| e.to_string())?;
    let key = &registration["key"];
    let expected = &key["attempt"]["source"];
    let expectation = match expected["kind"].as_str() {
        Some("existing") => {
            expected["session_id"] == bound.session_id
                && expected["transcript"] == serde_json::json!(bound.transcript)
                && expected["file"] == serde_json::json!(bound.file)
                && expected["offset"] == bound.offset
        }
        Some("pre_source") => [
            (
                &expected["expected_session_id"],
                serde_json::json!(bound.session_id),
            ),
            (
                &expected["expected_transcript"],
                serde_json::json!(bound.transcript),
            ),
            (
                &expected["binding_watermark"],
                serde_json::json!(bound.binding_watermark),
            ),
        ]
        .iter()
        .all(|(wanted, actual)| wanted.is_null() || *wanted == actual),
        _ => false,
    };
    Ok(bound.provider == "claude"
        && bound.target == key["attempt"]["target"]
        && bound.session_id == source.session_id
        && bound.transcript == source.path
        && bound.file == [source.dev, source.ino]
        && bound.offset == entry.offset
        && key["attempt"]["nonce"].as_str() == Some(entry.id.as_str())
        && registration["frame"]["class_hash"].as_str() == Some(entry.class_hash.as_str())
        && registration["frame"]["hash"].as_str() == Some(entry.hash.as_str())
        && registration["namespace"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
        && key["identity"]["sources"]
            .as_array()
            .is_some_and(|s| !s.is_empty())
        && expectation)
}

fn native_rows(origin: &NoticeOrigin, from: u64, through: u64, prefix: &str) -> Option<Vec<Taken>> {
    use std::io::{BufRead, Read};
    #[cfg(test)]
    CENSUS_READS.with(|count| count.set(count.get() + 1));
    let source = &origin.record.source;
    let file = std::fs::File::open(&source.path).ok()?;
    let meta = file.metadata().ok()?;
    (file_id(&meta) == [source.dev, source.ino] && meta.len() >= through && from <= through)
        .then_some(())?;
    let mut reader = std::io::BufReader::new(file.take(through));
    let mut digest = Sha256::new();
    let (mut rows, mut line, mut at) = (Vec::new(), Vec::new(), 0);
    while reader.read_until(b'\n', &mut line).ok()? > 0 {
        let offset = at;
        at += line.len() as u64;
        line.ends_with(b"\n").then_some(())?;
        digest.update(&line);
        if offset < from {
            (at <= from).then_some(())?;
            line.clear();
            continue;
        }
        let value: serde_json::Value = serde_json::from_slice(&line).ok()?;
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
            let uuid = value["uuid"].as_str()?;
            rows.push(Taken {
                offset,
                uuid: uuid.into(),
                hashes: class_hashes(&text),
                user: value["type"] == "user",
            });
        }
    }
    (at == through && hex::encode(digest.finalize()) == prefix).then_some(rows)
}

fn attribute(entries: &[Entry], blocked: &Blocked, rows: Option<&[Taken]>) -> (Answers, Answers) {
    let mut exact: Answers = HashMap::new();
    let mut occupied = HashSet::new();
    for entry in entries {
        let receipts: HashSet<_> = entry
            .events
            .iter()
            .filter(|e| matches!(e.kind.as_str(), "user" | "command"))
            .filter_map(|e| e.row.as_deref())
            .collect();
        if !receipts.is_empty() {
            occupied.insert(entry.id.as_str());
        }
        for uuid in &receipts {
            let decision = if receipts.len() == 1 && entry.append.is_none() {
                Decision::Owned
            } else {
                Decision::Pending("ambiguous_registration")
            };
            exact
                .entry((*uuid).into())
                .and_modify(|(_, _, answer)| *answer = Decision::Pending("ambiguous_registration"))
                .or_insert((entry.offset, vec![entry.class_hash.clone()], decision));
        }
    }
    let from = entries.iter().map(|e| e.offset).min().unwrap_or(0);
    let Some(rows) = rows else {
        return (exact, HashMap::new());
    };
    let mut free: Vec<_> = entries
        .iter()
        .filter(|e| !occupied.contains(e.id.as_str()))
        .collect();
    free.sort_by_key(|e| e.offset);
    let mut census = HashMap::new();
    for row in rows.iter().filter(|r| r.offset >= from) {
        if exact.contains_key(&row.uuid) {
            continue;
        }
        let matching: Vec<_> = free
            .iter()
            .copied()
            .filter(|e| e.offset <= row.offset && row.hashes.contains(&e.class_hash))
            .collect();
        let decision =
            if let Some((_, reason)) = blocked.iter().find(|(h, _)| row.hashes.contains(h)) {
                Decision::Pending(reason)
            } else if matching.iter().any(|e| e.append.is_some())
                || matching.windows(2).any(|p| p[0].offset == p[1].offset)
            {
                Decision::Pending("ambiguous_registration")
            } else if let Some(first) = matching.first() {
                let id = first.id.clone();
                free.retain(|e| e.id != id);
                Decision::Owned
            } else {
                Decision::Unowned
            };
        census.insert(row.uuid.clone(), (row.offset, row.hashes.clone(), decision));
    }
    (exact, census)
}
