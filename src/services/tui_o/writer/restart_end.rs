//! Physical ends are scoped to the old binding and applied between captured records.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Read;

use sha2::{Digest, Sha256};

use super::binding::{BindingEvent, BindingEvents, BindingRecord, BindingTarget};
use super::deliver::ChannelWriter;
use super::pieces::UnitDeriver;
use super::rotation::Owe;
use super::{AlarmSink, DeliveryLease, DiscordPort, WriterAlarm};
use crate::services::tui_o::shadow::capture::{FileMatch, Fingerprint, file_identity, file_match};
use crate::services::tui_o::shadow::identity::{RecordFact, classify, row_key};
use crate::services::tui_o::shadow::seal::{TurnEvent, TurnTracker};
use crate::services::tui_o::shadow::{
    CapturedRecord, ShadowProvider, SourceBinding, SourceId, UnitKey,
};
use crate::services::tui_o::store::restart_end::{self, RestartEnd};
use crate::services::tui_o::store::spool::source_key;

pub(crate) fn same_source(left: &SourceId, right: &SourceId) -> bool {
    left.session_id == right.session_id
        && left.path == right.path
        && file_match(left, (right.dev, right.ino)) != FileMatch::Other
}

fn scope_matches(end: &RestartEnd, event: &BindingEvent, binding: &SourceBinding) -> bool {
    let source = match &event.record {
        BindingRecord::Bound {
            new: BindingTarget::Source(source),
            ..
        }
        | BindingRecord::Resolved { source, .. } => Some(source),
        _ => None,
    };
    end.channel == binding.channel_id
        && end.provider == binding.provider
        && same_source(&end.source, &binding.source)
        && event.channel_id == end.channel
        && event.provider == end.provider
        && event.seq == end.binding_seq
        && event.tmux_session == end.tmux_session
        && event.execution_nonce == end.execution_nonce
        && source == Some(&end.source)
}

#[derive(Clone, Debug)]
pub(crate) struct VerifiedEnd {
    end: RestartEnd,
    snapshot: Option<Fingerprint>,
    captured: bool,
}

impl std::ops::Deref for VerifiedEnd {
    type Target = RestartEnd;
    fn deref(&self) -> &RestartEnd {
        &self.end
    }
}

impl VerifiedEnd {
    pub(crate) fn renewed(&self) -> Option<Self> {
        let mut renewed = self.clone();
        renewed.snapshot = Some(checked_snapshot(self)?);
        Some(renewed)
    }

    pub(crate) fn native_snapshot(&self) -> Option<&Fingerprint> {
        self.snapshot.as_ref()
    }

    pub(crate) fn native_current(&self, source: &SourceId) -> bool {
        self.snapshot
            .as_ref()
            .is_some_and(|snapshot| Fingerprint::path(source).as_ref().ok() == Some(snapshot))
    }

    pub(crate) fn accepts_prefix(&self, prefix: Option<&Sha256>, source: &SourceId) -> bool {
        match prefix {
            Some(hash) => hex::encode(hash.clone().finalize()) == self.prefix_hash,
            None => {
                self.captured
                    || self.snapshot.as_ref().is_some_and(|snapshot| {
                        Fingerprint::path(source).as_ref().ok() == Some(snapshot)
                    })
            }
        }
    }
}

fn prefix_matches(end: &RestartEnd) -> Result<Option<Fingerprint>, String> {
    for _ in 0..2 {
        let file = match std::fs::File::open(&end.source.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.to_string()),
        };
        let before = file.metadata().map_err(|error| error.to_string())?;
        if file_match(&end.source, file_identity(&before)) == FileMatch::Other
            || before.len() < end.cutoff
        {
            return Ok(None);
        }
        let snapshot = Fingerprint::of(&before).map_err(|error| error.to_string())?;
        let mut hash = Sha256::new();
        let read = std::io::copy(&mut file.take(end.cutoff), &mut hash)
            .map_err(|error| error.to_string())?;
        #[cfg(test)]
        AFTER_HASH.with_borrow_mut(|hook| {
            if let Some(hook) = hook.take() {
                hook();
            }
        });
        let after = std::fs::metadata(&end.source.path).map_err(|error| error.to_string())?;
        let after = Fingerprint::of(&after).map_err(|error| error.to_string())?;
        if (after.dev, after.ino) != (snapshot.dev, snapshot.ino)
            || after.len < end.cutoff
            || (after.len == snapshot.len && after != snapshot)
        {
            return Err("restart prefix changed while being verified".into());
        }
        if read != end.cutoff || hex::encode(hash.finalize()) != end.prefix_hash {
            return Ok(None);
        }
        // A growth observation needs a fresh read before it can replace the verified snapshot.
        if after.len != snapshot.len {
            continue;
        }
        return Ok(Some(after));
    }
    Err("restart prefix changed while being verified".into())
}

fn rejected(end: &RestartEnd, reason: impl std::fmt::Display) {
    tracing::warn!(end_id = %end.end_id, reason = %reason, "restart end not applied");
}

fn checked_snapshot(end: &RestartEnd) -> Option<Fingerprint> {
    for _ in 0..2 {
        match prefix_matches(end) {
            Ok(snapshot) => {
                if snapshot.is_none() {
                    rejected(end, "prefix missing or mismatched");
                }
                return snapshot;
            }
            Err(reason) => {
                rejected(end, &reason);
                if reason != "restart prefix changed while being verified" {
                    break;
                }
            }
        }
    }
    None
}

#[cfg(test)]
thread_local! {
    static AFTER_HASH: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
#[path = "restart_end_repair_tests.rs"]
mod repair_tests;

/// Called on a worker before folding native records; the current resume nonce may be newer.
pub(crate) fn validated(
    ends: Vec<RestartEnd>,
    history: &[BindingEvent],
    binding: &SourceBinding,
) -> Result<Vec<VerifiedEnd>, String> {
    let mut checked = Vec::new();
    for mut end in ends {
        if !history
            .iter()
            .any(|event| scope_matches(&end, event, binding))
        {
            rejected(&end, "binding scope mismatch");
        } else if let Some(snapshot) = checked_snapshot(&end) {
            end.source = binding.source.clone();
            checked.push(VerifiedEnd {
                end,
                snapshot: Some(snapshot),
                captured: false,
            });
        }
    }
    checked.sort_by_key(|end| (end.cutoff, end.open_after));
    Ok(checked)
}

struct Window {
    tracker: TurnTracker,
    open_after: Option<u64>,
    native_id: Option<String>,
    keys: BTreeSet<UnitKey>,
    last: Option<CapturedRecord>,
    idle: bool,
    prefix: Option<Sha256>,
    through: u64,
}

impl Default for Window {
    fn default() -> Self {
        Self {
            tracker: TurnTracker::starting_at(0),
            open_after: None,
            native_id: None,
            keys: BTreeSet::new(),
            last: None,
            idle: true,
            prefix: Some(Sha256::new()),
            through: 0,
        }
    }
}

pub(super) struct Ended {
    pub source: SourceId,
    pub record: CapturedRecord,
    pub release: BTreeSet<UnitKey>,
}

#[derive(Default)]
pub(super) struct Projection {
    version: Option<(u64, u64)>,
    ends: Vec<VerifiedEnd>,
    applied: HashSet<String>,
    missed: HashSet<String>,
    windows: HashMap<String, Window>,
}

impl Projection {
    fn take(&mut self, source: &SourceId, through: u64) -> Option<Ended> {
        for end in &self.ends {
            if same_source(&end.source, source)
                && through > end.cutoff
                && !self.applied.contains(&end.end_id)
                && self.missed.insert(end.end_id.clone())
            {
                rejected(end, "capture passed the unapplied cutoff");
            }
        }
        let key = source_key(source);
        let window = self.windows.get_mut(&key)?;
        let end = self.ends.iter().find(|end| {
            !self.applied.contains(&end.end_id)
                && same_source(&end.source, source)
                && end.cutoff == through
                && end.accepts_prefix(window.prefix.as_ref(), source)
                && window
                    .last
                    .as_ref()
                    .is_some_and(|record| record.end == end.cutoff)
                && window.open_after == Some(end.open_after)
                && window.native_id == end.native_turn_id
        })?;
        let record = window.last.clone()?;
        self.applied.insert(end.end_id.clone());
        let keys = std::mem::take(&mut window.keys);
        window.tracker = TurnTracker::starting_at(0);
        window.open_after = None;
        window.native_id = None;
        window.idle = true;
        let release = keys
            .iter()
            .filter(|key| !self.windows.values().any(|other| other.keys.contains(*key)))
            .cloned()
            .collect();
        Some(Ended {
            source: source.clone(),
            record,
            release,
        })
    }

    pub(super) fn before(
        &mut self,
        binding: &SourceBinding,
        record: &CapturedRecord,
    ) -> Option<Ended> {
        let ended = self.take(&binding.source, record.start);
        let window = self
            .windows
            .entry(source_key(&binding.source))
            .or_insert_with(|| Window {
                tracker: TurnTracker::starting_at(record.start),
                idle: record.start == 0,
                prefix: (record.start == 0).then(Sha256::new),
                through: record.start,
                ..Window::default()
            });
        if window.through != record.start
            || record.end - record.start != record.line.len() as u64 + 1
        {
            window.prefix = None;
        }
        if let Some(hash) = &mut window.prefix {
            hash.update(&record.line);
            hash.update(b"\n");
        }
        window.through = record.end;
        if let Ok(value) = serde_json::from_slice(&record.line) {
            let row = row_key(&value);
            for fact in classify(binding.provider, &value) {
                if let RecordFact::Idle(Some(close)) = &fact
                    && window.native_id.as_ref().is_some_and(|open| open != close)
                {
                    continue;
                }
                match window.tracker.observe(&fact, row.as_ref()) {
                    TurnEvent::Opened(id) => {
                        window.open_after = Some(record.end);
                        window.native_id = Some(id);
                        window.keys.clear();
                        window.idle = false;
                    }
                    TurnEvent::Closed(_) | TurnEvent::StrayIdle | TurnEvent::EdgeTurn => {
                        window.open_after = None;
                        window.native_id = None;
                        window.keys.clear();
                        window.idle = true;
                    }
                    TurnEvent::None if matches!(fact, RecordFact::Idle(_)) => {
                        window.open_after = None;
                        window.native_id = None;
                        window.keys.clear();
                        window.idle = true;
                    }
                    TurnEvent::None
                        if matches!(fact, RecordFact::TurnStart(_))
                            || (row.is_none() && matches!(fact, RecordFact::Prompt(true, _)))
                            || (window.idle && matches!(fact, RecordFact::Assistant)) =>
                    {
                        window.open_after = Some(record.end);
                        window.native_id = None;
                        window.keys.clear();
                        window.idle = false;
                    }
                    _ => {}
                }
                if let RecordFact::Unit(native_key, kind, _)
                | RecordFact::Announced(native_key, kind) = fact
                {
                    window.keys.insert(UnitKey {
                        channel_id: binding.channel_id,
                        provider: binding.provider,
                        native_key,
                        kind,
                    });
                }
            }
        }
        window.last = Some(record.clone());
        ended
    }

    pub(super) fn through(&mut self, source: &SourceId, through: u64) -> Option<Ended> {
        self.take(source, through)
    }
}

fn version<B: BindingEvents>(channel: u64, bindings: &B) -> (u64, u64) {
    (
        *restart_end::subscribe(channel).borrow(),
        *bindings.subscribe(channel).borrow(),
    )
}

pub(super) fn capture_current<B: BindingEvents>(
    deriver: &mut UnitDeriver,
    channel: u64,
    bindings: &B,
) -> bool {
    deriver.restart_projection().version == Some(version(channel, bindings))
}

/// Loading precedes both boot replay and capture, so no unit crosses its end before sealing.
pub(super) async fn refresh<P, L, A, B>(
    writer: &mut ChannelWriter<P, L, A>,
    deriver: &mut UnitDeriver,
    bindings: &B,
    provider: ShadowProvider,
) -> Result<(), WriterAlarm>
where
    P: DiscordPort,
    L: DeliveryLease,
    A: AlarmSink,
    B: BindingEvents,
{
    let channel = writer.channel();
    let version = version(channel, bindings);
    if deriver.restart_projection().version == Some(version) {
        return Ok(());
    }
    let halted = |detail| WriterAlarm::Halted { detail };
    let ends: Vec<_> = writer
        .store()
        .durable_restart_ends()
        .map_err(halted)?
        .into_iter()
        .filter(|end| end.provider == provider)
        .collect();
    if ends.is_empty() {
        deriver.restart_projection().ends.clear();
        deriver.restart_projection().version = Some(version);
        return Ok(());
    }
    let history = bindings.binding_history(channel).map_err(halted)?;
    let cursors: Vec<_> = writer.store().cursors().cloned().collect();
    let checked = tokio::task::spawn_blocking(move || {
        let mut checked = Vec::new();
        for mut end in ends {
            let cursor = cursors
                .iter()
                .find(|cursor| same_source(&cursor.source, &end.source));
            let source = cursor
                .map(|cursor| cursor.source.clone())
                .unwrap_or_else(|| end.source.clone());
            let binding = SourceBinding {
                channel_id: channel,
                provider,
                source,
            };
            if !history
                .iter()
                .any(|event| scope_matches(&end, event, &binding))
            {
                rejected(&end, "binding scope mismatch");
                continue;
            }
            let captured = cursor.is_some_and(|cursor| {
                cursor.captured_through == end.cutoff && cursor.prefix_hash == end.prefix_hash
            });
            end.source = binding.source;
            let snapshot = checked_snapshot(&end);
            checked.push(VerifiedEnd {
                end,
                snapshot,
                captured,
            });
        }
        checked
    })
    .await
    .map_err(|error| halted(error.to_string()))?;
    let projection = deriver.restart_projection();
    projection.ends = checked;
    projection.version = Some(version);
    Ok(())
}

pub(super) fn finish(
    store: &crate::services::tui_o::store::ChannelStore,
    deriver: &mut UnitDeriver,
    owed: &mut impl Owe,
) {
    for cursor in store.cursors() {
        if let Some(ended) = deriver
            .restart_projection()
            .through(&cursor.source, cursor.captured_through)
        {
            let derived = deriver.end_projection(&ended);
            owed.owe(&ended.source, &ended.record, derived);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::binding::{BindingCause, BindingEvidence};
    use super::*;
    use crate::services::tui_o::shadow::UnitKind;
    use crate::services::tui_o::shadow::binding_reader::source_id_for;
    use crate::services::tui_o::store::spool::Cursor;
    use serde_json::json;

    pub(super) fn record(start: u64, value: serde_json::Value) -> CapturedRecord {
        let line = serde_json::to_vec(&value).unwrap();
        CapturedRecord {
            start,
            end: start + line.len() as u64 + 1,
            line,
        }
    }

    pub(super) fn prompt(start: u64, id: &str) -> CapturedRecord {
        record(
            start,
            json!({"type":"user","uuid":id,"message":{"role":"user","content":"work"}}),
        )
    }

    pub(super) fn tool(start: u64) -> CapturedRecord {
        record(
            start,
            json!({"type":"assistant","uuid":"tool-row","apiBlockIndex":0,
            "message":{"id":"shared-tool","role":"assistant","content":[
                {"type":"tool_use","id":"call-shared","name":"Read","input":{"file_path":"fixture"}}
            ]}}),
        )
    }

    pub(super) fn append_record(bytes: &mut Vec<u8>, record: &CapturedRecord) {
        bytes.extend_from_slice(&record.line);
        bytes.push(b'\n');
    }

    pub(super) fn end(
        binding: &SourceBinding,
        opener: &CapturedRecord,
        last: &CapturedRecord,
        bytes: &[u8],
    ) -> RestartEnd {
        RestartEnd {
            end_id: "physical-old-end".into(),
            channel: binding.channel_id,
            provider: binding.provider,
            tmux_session: "fixture-pane".into(),
            execution_nonce: "a".repeat(32),
            binding_seq: 1,
            source: binding.source.clone(),
            cutoff: last.end,
            prefix_hash: hex::encode(Sha256::digest(&bytes[..last.end as usize])),
            open_after: opener.end,
            native_turn_id: Some("old-open".into()),
        }
    }

    pub(super) fn history(end: &RestartEnd) -> Vec<BindingEvent> {
        let now = chrono::Utc::now();
        vec![BindingEvent {
            seq: end.binding_seq,
            channel_id: end.channel,
            provider: end.provider,
            tmux_session: end.tmux_session.clone(),
            execution_nonce: end.execution_nonce.clone(),
            record: BindingRecord::Bound {
                old: None,
                new: BindingTarget::Source(end.source.clone()),
                cause: BindingCause::Startup,
                parent_hint: None,
                evidence: BindingEvidence {
                    hook_event: "SessionStart".into(),
                    received_at: now,
                    reclaims: false,
                },
            },
            committed_at: now,
        }]
    }

    #[test]
    fn retained_prefix_snapshot_closes_old_open_after_capture_advances() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("retired.jsonl");
        let mut bytes = b"{\"type\":\"system\"}\n".to_vec();
        let opener = prompt(bytes.len() as u64, "old-open");
        append_record(&mut bytes, &opener);
        let last = tool(opener.end);
        append_record(&mut bytes, &last);
        let next = prompt(last.end, "new-open");
        append_record(&mut bytes, &next);
        std::fs::write(&path, &bytes).unwrap();
        let binding = SourceBinding {
            channel_id: 810_922_101,
            provider: ShadowProvider::Claude,
            source: source_id_for("retired-session", &path).unwrap(),
        };
        let cursor = Cursor {
            source: binding.source.clone(),
            captured_through: next.end,
            prefix_hash: hex::encode(Sha256::digest(&bytes)),
            retired: true,
            transition_cleanup: None,
        };
        let end = end(&binding, &opener, &last, &bytes);
        assert!(cursor.retired && cursor.captured_through > end.cutoff);
        let verified = validated(vec![end.clone()], &history(&end), &binding).unwrap();
        assert_eq!(verified.len(), 1);
        assert!(!verified[0].captured && verified[0].snapshot.is_some());
        let mut projection = Projection {
            ends: verified,
            ..Projection::default()
        };
        assert!(projection.before(&binding, &opener).is_none());
        assert!(projection.before(&binding, &last).is_none());
        assert!(
            projection.windows[&source_key(&binding.source)]
                .prefix
                .is_none()
        );
        let ended = projection.before(&binding, &next).expect(
            "a verified native prefix proves the cutoff after earlier spool bytes were collected",
        );
        assert_eq!(ended.record.end, end.cutoff);
        let window = &projection.windows[&source_key(&binding.source)];
        assert_eq!(window.open_after, Some(next.end));
        assert_eq!(window.native_id.as_deref(), Some("new-open"));
        assert!(projection.through(&binding.source, next.end).is_none());
    }

    #[test]
    fn exact_end_keeps_another_sources_shared_tool_key_out_of_release() {
        let root = tempfile::tempdir().unwrap();
        let opener = prompt(0, "old-open");
        let last = tool(opener.end);
        let mut bytes = Vec::new();
        append_record(&mut bytes, &opener);
        append_record(&mut bytes, &last);
        let a_path = root.path().join("a.jsonl");
        std::fs::write(&a_path, &bytes).unwrap();
        let a = SourceBinding {
            channel_id: 810_922_102,
            provider: ShadowProvider::Claude,
            source: source_id_for("source-a", &a_path).unwrap(),
        };
        let end = end(&a, &opener, &last, &bytes);
        let mut projection = Projection {
            ends: validated(vec![end.clone()], &history(&end), &a).unwrap(),
            ..Projection::default()
        };
        let b_path = root.path().join("b.jsonl");
        let b_opener = prompt(0, "other-open");
        let b_last = tool(b_opener.end);
        let mut b_bytes = Vec::new();
        append_record(&mut b_bytes, &b_opener);
        append_record(&mut b_bytes, &b_last);
        std::fs::write(&b_path, &b_bytes).unwrap();
        let b = SourceBinding {
            source: source_id_for("source-b", &b_path).unwrap(),
            ..a.clone()
        };
        for (binding, records) in [(&a, [&opener, &last]), (&b, [&b_opener, &b_last])] {
            for record in records {
                assert!(projection.before(binding, record).is_none());
            }
        }
        let shared = UnitKey {
            channel_id: a.channel_id,
            provider: a.provider,
            native_key: "shared-tool:0".into(),
            kind: UnitKind::Tool,
        };
        assert!(
            projection.windows[&source_key(&a.source)]
                .keys
                .contains(&shared)
        );
        assert!(
            projection.windows[&source_key(&b.source)]
                .keys
                .contains(&shared)
        );
        let ended = projection
            .through(&a.source, end.cutoff)
            .expect("the exact old source ends");
        assert!(!ended.release.contains(&shared));
        let other = &projection.windows[&source_key(&b.source)];
        assert!(other.keys.contains(&shared));
        assert_eq!(other.native_id.as_deref(), Some("other-open"));
        assert!(projection.through(&a.source, end.cutoff).is_none());
    }
}
