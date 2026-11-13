//! Notice attribution is historical and source-bound; it grants no input or stop authority.
use super::*;
use crate::services::tui_o::shadow::capture::Fingerprint;
use crate::services::tui_o::store::ChannelStore;
use crate::services::tui_o::writer::input_notices::NoticeOrigin;
use std::collections::{HashMap, HashSet};

mod census;
pub(crate) mod generation;
#[cfg(all(test, unix))]
pub(crate) use census::tests::{Stage as CensusStage, fail_once as fail_census_once};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Owned,
    Unowned,
    Pending(&'static str),
}

type Stamp = Fingerprint;
struct ContextObservation {
    stamp: Option<Stamp>,
    unavailable: bool,
}
type Contexts = HashMap<PathBuf, ContextObservation>;
type Answers = HashMap<String, (u64, Vec<String>, Decision)>;

struct Scope {
    origin: NoticeOrigin,
    executions: Contexts,
    history_stamp: Option<Stamp>,
    binding_stamp: Option<Fingerprint>,
    observed_native: Option<Fingerprint>,
    prefix: String,
    owners: HashMap<String, String>,
    retry_at: Option<tokio::time::Instant>,
    through: u64,
    native_stamp: Option<Fingerprint>,
    exact: Answers,
    census: Answers,
}

impl Scope {
    fn current(&self, store: &ChannelStore, history: &Option<Stamp>) -> bool {
        let cursor = store.cursor(&self.origin.record.source);
        self.history_stamp == *history
            && self.binding_stamp
                == crate::services::tui_prompt_dedupe::binding_events::log_path(
                    store.init().channel,
                )
                .ok()
                .flatten()
                .as_deref()
                .and_then(generation::fingerprint)
            && self.observed_native == generation::fingerprint(&self.origin.record.source.path)
            && self.through == cursor.map_or(0, |c| c.captured_through)
            && self.prefix == cursor.map_or("", |c| c.prefix_hash.as_str())
            && self
                .executions
                .iter()
                .all(|(p, observed)| file_stamp(p) == observed.stamp)
            && self
                .retry_at
                .is_none_or(|at| tokio::time::Instant::now() < at)
    }
}

#[derive(Default)]
pub(crate) struct Batch {
    stamp: Option<Stamp>,
    scopes: Vec<Scope>,
    native: census::Cache,
}

fn file_stamp(path: &Path) -> Option<Stamp> {
    generation::fingerprint(path)
}

fn stamp() -> Option<Stamp> {
    file_stamp(&path().ok()?)
}

#[cfg(test)]
thread_local! {
    pub(crate) static SNAPSHOT_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static CENSUS_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn same_scope(a: &NoticeOrigin, b: &NoticeOrigin) -> bool {
    a.record.source == b.record.source
        && a.logical_session == b.logical_session
        && a.execution_nonce == b.execution_nonce
        && a.binding_seq == b.binding_seq
}

impl Batch {
    #[cfg(test)]
    pub(crate) fn prepare(store: &ChannelStore) -> Self {
        let mut batch = Self::default();
        batch.refresh(store);
        batch
    }

    pub(crate) fn refresh(&mut self, store: &ChannelStore) {
        let before = stamp();
        self.stamp = before.clone();
        let mut previous = std::mem::take(&mut self.scopes);
        let mut snapshot = None;
        let mut native = HashMap::new();
        for evidence in store.ledger().input_notices().evidence.values() {
            if store.ledger().input_notices().settled.contains_key(&(
                evidence.record.source.clone(),
                evidence.record.start,
                evidence.record.end,
            )) {
                continue;
            }
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
            if self.scopes.iter().any(|s| same_scope(&s.origin, origin)) {
                continue;
            }
            if let Some(at) = previous
                .iter()
                .position(|s| same_scope(&s.origin, origin) && s.current(store, &before))
            {
                self.scopes.push(previous.swap_remove(at));
                continue;
            }
            let snapshot = snapshot.get_or_insert_with(|| {
                #[cfg(test)]
                SNAPSHOT_READS.with(|c| c.set(c.get() + 1));
                path().and_then(|p| match std::fs::read(&p) {
                    Ok(bytes) if bytes.len() <= 256 * 1024 => {
                        compat::reuse(&p, bytes, compat::parse)
                    }
                    Ok(bytes) => compat::parse(&bytes).map(std::sync::Arc::new),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        Ok(std::sync::Arc::new(serde_json::json!({"entries":[]})))
                    }
                    Err(e) => Err(e.to_string()),
                })
            });
            let (through, native_stamp, rows) = native
                .entry(origin.record.source.clone())
                .or_insert_with(|| {
                    let cursor = store.cursor(&origin.record.source);
                    let through = cursor.map_or(0, |c| c.captured_through);
                    let prefix = cursor.map_or("", |c| c.prefix_hash.as_str());
                    let (stamp, rows) = self.native.read(&origin.record.source, through, prefix);
                    (through, stamp, rows)
                });
            let mut executions = HashMap::new();
            let attributed = snapshot
                .as_ref()
                .ok()
                .and_then(|v| candidates(v, origin, store.init().channel, &mut executions).ok())
                .map(|(entries, blocked)| {
                    attribute(&entries, &blocked, rows.as_deref().map(Vec::as_slice))
                });
            let transient = native_stamp.is_none()
                && generation::fingerprint(&origin.record.source.path).is_some_and(|fp| {
                    fp.dev == origin.record.source.dev && fp.ino == origin.record.source.ino
                });
            let unstable = attributed.is_none()
                || rows.is_none()
                || executions.values().any(|e| e.unavailable);
            let (exact, census, owners) = attributed.unwrap_or_default();
            self.scopes.push(Scope {
                origin: origin.clone(),
                executions,
                history_stamp: before.clone(),
                binding_stamp: crate::services::tui_prompt_dedupe::binding_events::log_path(
                    store.init().channel,
                )
                .ok()
                .flatten()
                .as_deref()
                .and_then(generation::fingerprint),
                observed_native: generation::fingerprint(&origin.record.source.path),
                prefix: store
                    .cursor(&origin.record.source)
                    .map_or(String::new(), |c| c.prefix_hash.clone()),
                owners,
                retry_at: unstable.then(|| {
                    tokio::time::Instant::now()
                        + tokio::time::Duration::from_secs(if transient { 0 } else { 5 })
                }),
                through: *through,
                native_stamp: native_stamp.clone(),
                exact,
                census,
            });
        }
        if before != stamp() {
            self.scopes.clear();
        }
    }

    pub(crate) fn transient(&self, source: &crate::services::tui_o::shadow::SourceId) -> bool {
        self.scopes.iter().any(|s| {
            s.origin.record.source == *source
                && s.native_stamp.is_none()
                && s.observed_native
                    .as_ref()
                    .is_some_and(|fp| fp.dev == source.dev && fp.ino == source.ino)
        })
    }

    pub(crate) fn owned_proof(
        &self,
        origin: &NoticeOrigin,
        row: &str,
        prompt: &str,
    ) -> Option<String> {
        (self.decision(origin, row, prompt) == Decision::Owned)
            .then(|| {
                self.scopes
                    .iter()
                    .find(|s| same_scope(&s.origin, origin))?
                    .owners
                    .get(row)
                    .cloned()
            })
            .flatten()
    }

    pub(crate) fn decision(&self, origin: &NoticeOrigin, row: &str, prompt: &str) -> Decision {
        if self.stamp != stamp() {
            return Decision::Pending("history_snapshot_changed");
        }
        let Some(scope) = self.scopes.iter().find(|s| same_scope(&s.origin, origin)) else {
            return Decision::Pending("original_binding_missing");
        };
        if scope
            .executions
            .iter()
            .any(|(path, stamp)| file_stamp(path).as_ref() != stamp.stamp.as_ref())
        {
            return Decision::Pending("execution_context_changed");
        }
        if scope.executions.values().any(|e| e.unavailable) {
            return Decision::Pending("execution_context_unavailable");
        }
        let hashes = class_hashes(prompt);
        if let Some((offset, frames, answer)) = scope.exact.get(row)
            && *offset <= origin.record.start
            && frames.iter().any(|h| hashes.contains(h))
        {
            return *answer;
        }
        let source = &origin.record.source;
        let valid = scope
            .native_stamp
            .as_ref()
            .is_some_and(|held| census::fingerprint(source).as_ref() == Some(held));
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
    channel: u64,
    executions: &mut Contexts,
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
            if relevant(registration, origin, channel, executions) {
                blocked.push((frame_hash(registration)?, "unbound_pre_source"));
            }
            continue;
        }
        let mut entry: Entry = serde_json::from_value(raw.clone()).map_err(|e| e.to_string())?;
        if entry.provider != "claude" || entry.session != origin.logical_session {
            continue;
        }
        if !same(&entry)
            || (!registration.is_null() && !relevant(registration, origin, channel, executions))
        {
            blocked.push((entry.class_hash.clone(), "registration_source_mismatch"));
            continue;
        }
        // Migrated V1 evidence has no execution nonce or native-session field to invent.
        if registration.is_null() || historical_match(registration, &entry, origin)? {
            // Pair by full registration identity; a nonce can recur in another key or namespace.
            entry.id = if registration.is_null() {
                format!("legacy:{}", entry.id)
            } else {
                serde_json::json!([registration["namespace"], registration["key"]]).to_string()
            };
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
        if relevant(registration, origin, channel, executions) {
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

fn relevant(
    registration: &serde_json::Value,
    origin: &NoticeOrigin,
    channel: u64,
    executions: &mut Contexts,
) -> bool {
    let attempt = &registration["key"]["attempt"];
    attempt["provider"] == "claude"
        && attempt["target"]["owner"] == origin.logical_session
        && (attempt["target"]["execution_nonce"] == origin.execution_nonce
            || resumed_execution(&attempt["target"], origin, channel, executions))
}

fn resumed_execution(
    target: &serde_json::Value,
    origin: &NoticeOrigin,
    channel: u64,
    executions: &mut Contexts,
) -> bool {
    use crate::services::tui_prompt_dedupe::binding_context;
    let Some(nonce) = target["execution_nonce"].as_str() else {
        return false;
    };
    let Ok(path) = binding_context::context_path("claude", nonce) else {
        return false;
    };
    let before = file_stamp(&path);
    let context = binding_context::execution_context("claude", nonce);
    executions.insert(
        path.clone(),
        ContextObservation {
            stamp: before.clone(),
            unavailable: context.is_err(),
        },
    );
    let Ok(context) = context else {
        return false;
    };
    // The attachment's nonce stays historical; the immutable launch must name this source.
    before.is_some()
        && target["host"] == "tmux"
        && context.tmux_session == origin.logical_session
        && context.channel_id == Some(channel)
        && target["root"] == context.owner_runtime_root
        && matches!(context.launch_mode.as_str(), "resume" | "fresh")
        && context.expected_native_session_id.as_deref()
            == Some(origin.record.source.session_id.as_str())
        && file_stamp(&path) == before
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

fn attribute(
    entries: &[Entry],
    blocked: &Blocked,
    rows: Option<&[Taken]>,
) -> (Answers, Answers, HashMap<String, String>) {
    let mut owners = HashMap::new();
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
                owners.insert((*uuid).to_owned(), entry.id.clone());
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
        return (exact, HashMap::new(), owners);
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
                owners.insert(row.uuid.clone(), id.clone());
                free.retain(|e| e.id != id);
                Decision::Owned
            } else {
                Decision::Unowned
            };
        census.insert(row.uuid.clone(), (row.offset, row.hashes.clone(), decision));
    }
    (exact, census, owners)
}
