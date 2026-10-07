//! Read-only Legacy population and ordered retirement adapters for dormant input moves.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::inflight::{self, InflightEpisodePin, InflightTurnState};
use super::runtime_store;
use crate::services::provider::ProviderKind;
use crate::services::tui_input::handover::{
    Composer, EnqueueOutcome, MoveEvidence, MoveSource, Reconciliation,
};
use crate::services::tui_input::ledger::Ledger;
use crate::services::tui_input::rows::Row;
use crate::services::tui_input::transition::{DeletePhase, Host, Input};
use crate::services::turn_orchestrator::PendingQueueItem;

// The integration supplies existing transcript facts, admission, durable enqueue and actor startup.
pub(in crate::services::discord) trait Effects {
    fn intake_outbox_open(&mut self) -> io::Result<bool>;
    fn evidence(&mut self, key: u64, payload: &Value) -> io::Result<MoveEvidence>;
    /// Handback evidence for a ledger row; an adapter may judge the row's own attempt witness.
    fn reconcile(&mut self, key: u64, row: &Row) -> io::Result<Reconciliation> {
        let evidence = self.evidence(key, &row.input)?;
        Ok(Reconciliation::from_row(row)
            .min(Reconciliation::from_legacy(row.state, evidence.user_record)))
    }
    fn provider_alive(&mut self) -> io::Result<bool>;
    fn materialize_bundle(&mut self, upload: &Value) -> io::Result<Vec<(String, Vec<u8>)>>;
    fn enqueue(&mut self, key: u64, payload: &Value) -> io::Result<EnqueueOutcome>;
    fn start_actor(&mut self) -> io::Result<()>;
    fn notice(&mut self, key: Option<u64>, reason: &'static str) -> io::Result<()>;
}

struct Captured {
    path: PathBuf,
    bytes: Vec<u8>,
    phase: DeletePhase,
}

pub(in crate::services::discord) struct Files<E> {
    root: PathBuf,
    provider: ProviderKind,
    channel: u64,
    captured: Vec<Captured>,
    closing: Option<std::sync::Arc<super::input_runtime::fence::Closing>>,
    effects: E,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

fn read(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Ok(meta) if !meta.file_type().is_file() => {
            Err(invalid("input source is not a regular file"))
        }
        Ok(_) => fs::read(path).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn children(path: &Path) -> io::Result<Vec<PathBuf>> {
    match fs::read_dir(path) {
        Ok(entries) => {
            let mut paths = entries
                .map(|e| e.map(|e| e.path()))
                .collect::<io::Result<Vec<_>>>()?;
            paths.sort();
            Ok(paths)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

fn source_ids(payload: &Value) -> io::Result<BTreeSet<u64>> {
    let key = payload["message_id"]
        .as_u64()
        .filter(|id| *id != 0)
        .ok_or_else(|| invalid("missing primary input id"))?;
    let mut ids = BTreeSet::from([key]);
    if let Some(sources) = payload["source_message_ids"].as_array() {
        for source in sources {
            ids.insert(
                source
                    .as_u64()
                    .filter(|id| *id != 0)
                    .ok_or_else(|| invalid("invalid source id"))?,
            );
        }
    }
    Ok(ids)
}

impl<E: Effects> Files<E> {
    pub fn new(root: &Path, provider: ProviderKind, channel: u64, effects: E) -> Self {
        Self {
            root: root.to_owned(),
            provider,
            channel,
            captured: Vec::new(),
            closing: None,
            effects,
        }
    }

    pub fn frozen(
        root: &Path,
        provider: ProviderKind,
        channel: u64,
        closing: std::sync::Arc<super::input_runtime::fence::Closing>,
        effects: E,
    ) -> io::Result<Self> {
        if closing.channel() != channel || closing.provider() != &provider {
            return Err(invalid("population capability identity mismatch"));
        }
        let mut files = Self::new(root, provider, channel, effects);
        files.closing = Some(closing);
        Ok(files)
    }

    pub fn effects_mut(&mut self) -> &mut E {
        &mut self.effects
    }

    fn population(&self) -> io::Result<super::input_runtime::fence::ClosedPopulationGuard> {
        super::input_runtime::fence::require_worker()
            .map_err(|failure| invalid(format!("input fence: {failure:?}")))?;
        self.closing
            .as_ref()
            .ok_or_else(|| invalid("Frozen population capability required"))?
            .frozen_population(&self.root)
            .map_err(|failure| invalid(format!("input fence: {failure:?}")))
    }

    fn snapshot(&mut self) -> io::Result<()> {
        let provider = self.provider.as_str();
        for (name, phases) in [
            (
                "discord_pending_queue",
                &[DeletePhase::Queue, DeletePhase::Dispatch][..],
            ),
            (
                "discord_queued_placeholders",
                &[DeletePhase::Accessories][..],
            ),
            // Exit-clear records owe a card deletion for an input already gone; they stay put.
        ] {
            for token in children(&self.root.join(name).join(provider))? {
                if !fs::symlink_metadata(&token)?.file_type().is_dir() {
                    if token
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| matches!(n, ".DS_Store" | ".gitkeep"))
                    {
                        continue;
                    }
                    return Err(invalid("population token is not a directory"));
                }
                for phase in phases {
                    let extension = if *phase == DeletePhase::Dispatch {
                        "dispatch"
                    } else {
                        "json"
                    };
                    let path = token.join(format!("{}.{extension}", self.channel));
                    if let Some(bytes) = read(&path)? {
                        self.captured.push(Captured {
                            path,
                            bytes,
                            phase: *phase,
                        });
                    }
                }
            }
        }
        let row = self
            .root
            .join("discord_inflight")
            .join(provider)
            .join(format!("{}.json", self.channel));
        if let Some(bytes) = read(&row)? {
            self.captured.push(Captured {
                path: row,
                bytes,
                phase: DeletePhase::Row,
            });
        }
        for path in children(
            &self
                .root
                .join("discord_busy_followup_retries")
                .join(provider)
                .join(self.channel.to_string()),
        )? {
            if path.extension().is_some_and(|ext| ext == "json")
                && let Some(bytes) = read(&path)?
            {
                self.captured.push(Captured {
                    path,
                    bytes,
                    phase: DeletePhase::Accessories,
                });
            }
        }
        Ok(())
    }

    fn decode_snapshot(&mut self) -> io::Result<(Vec<Input>, Vec<Input>, Option<Input>)> {
        let mut queued = Vec::new();
        let mut markers = Vec::new();
        let mut active = None;
        let mut synthetic = false;
        for file in &self.captured {
            let value: Value = serde_json::from_slice(&file.bytes)?;
            match file.phase {
                DeletePhase::Queue => {
                    for value in value
                        .as_array()
                        .ok_or_else(|| invalid("queue is not an array"))?
                    {
                        queued.push(self.input(value.clone(), MoveSource::Queue)?);
                    }
                }
                DeletePhase::Dispatch => markers.push(self.input(value, MoveSource::DispatchOnly)?),
                DeletePhase::Row => {
                    let state: InflightTurnState = serde_json::from_value(value)?;
                    if state.provider != self.provider.as_str() || state.channel_id != self.channel
                    {
                        return Err(invalid("row belongs to another channel"));
                    }
                    synthetic =
                        state.user_msg_id == 0 || inflight::is_synthetic_create_state(&state);
                    if !synthetic {
                        active = Some(self.input(json!({
                            "author_id":state.request_owner_user_id, "message_id":state.user_msg_id,
                            "text":state.user_text, "source_message_ids":state.source_message_ids,
                            "queued_generation":state.born_generation, "reply_context":state.followup_reply_context,
                            "has_reply_boundary":state.followup_has_reply_boundary, "merge_consecutive":state.followup_merge_consecutive,
                            "pending_uploads":state.followup_pending_uploads, "voice_announcement":state.followup_voice_announcement,
                            "channel_id":self.channel,
                        }), MoveSource::TurnRow)?);
                    }
                }
                DeletePhase::Accessories => {
                    if file
                        .path
                        .starts_with(self.root.join("discord_busy_followup_retries"))
                    {
                        file.path
                            .file_stem()
                            .and_then(|n| n.to_str())
                            .and_then(|n| n.parse::<u64>().ok())
                            .ok_or_else(|| invalid("invalid busy retry source id"))?;
                        let _: super::busy_followup_retry_store::BusyFollowupRetryState =
                            serde_json::from_value(value)?;
                    } else {
                        let _: Vec<super::queued_placeholders_store::QueuedPlaceholderEntry> =
                            serde_json::from_value(value)?;
                    }
                }
            }
        }
        if synthetic {
            self.captured.retain(|file| file.phase != DeletePhase::Row);
        }
        Ok((queued, markers, active))
    }

    fn input(&self, payload: Value, source: MoveSource) -> io::Result<Input> {
        let _: PendingQueueItem = serde_json::from_value(payload.clone())?;
        source_ids(&payload)?;
        if payload["channel_id"]
            .as_u64()
            .is_some_and(|channel| channel != self.channel)
        {
            return Err(invalid("input belongs to a different channel"));
        }
        Ok(Input {
            key: payload["message_id"].as_u64().unwrap(),
            payload,
            source,
            pins: Vec::new(),
        })
    }

    fn pin_uploads(&mut self, ledger: &Ledger, input: &mut Input) -> io::Result<()> {
        let uploads = input.payload["pending_uploads"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut records = Vec::new();
        let mut pins = Vec::new();
        for upload in uploads {
            let entries = if let Some(record) = upload.as_str() {
                let (_, location) = record
                    .split_once(" → ")
                    .ok_or_else(|| invalid("invalid upload record"))?;
                let (path, _) = location
                    .rsplit_once(" (")
                    .ok_or_else(|| invalid("invalid upload size"))?;
                let filename = Path::new(path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .ok_or_else(|| invalid("invalid upload filename"))?;
                vec![(filename.to_owned(), fs::read(path)?)]
            } else {
                self.effects.materialize_bundle(&upload)?
            };
            if entries.is_empty() {
                return Err(invalid("upload materialization is empty"));
            }
            for (filename, bytes) in entries {
                let pin = ledger.pin_blob(
                    &input.key.to_string(),
                    pins.len()
                        .try_into()
                        .map_err(|_| invalid("too many uploads"))?,
                    &filename,
                    &bytes,
                )?;
                let location = self
                    .root
                    .join("input_ledger")
                    .join(self.channel.to_string())
                    .join(&pin.local_path);
                records.push(Value::String(format!(
                    "[File uploaded] {filename} → {} ({} bytes)",
                    location.display(),
                    bytes.len()
                )));
                pins.push(pin);
            }
        }
        input.payload["pending_uploads"] = Value::Array(records);
        input.payload["blob_pins"] = serde_json::to_value(&pins)?;
        input.pins = pins;
        Ok(())
    }
}

impl<E: Effects> Host for Files<E> {
    fn collect(&mut self, ledger: &Ledger) -> io::Result<Vec<Input>> {
        self.captured.clear();
        // Validate without holding a file guard across the external outbox probe.
        drop(self.population()?);
        if self.effects.intake_outbox_open()? {
            return Err(invalid("channel has open intake outbox rows"));
        }
        let guard = self.population()?;
        self.snapshot()?;
        drop(guard);
        let (queued, markers, mut active) = self.decode_snapshot()?;
        for queued_input in &queued {
            let ids = source_ids(&queued_input.payload)?;
            if active
                .iter()
                .chain(markers.iter())
                .any(|input| source_ids(&input.payload).is_ok_and(|other| !ids.is_disjoint(&other)))
            {
                let evidence = self
                    .effects
                    .evidence(queued_input.key, &queued_input.payload)?;
                if !evidence.user_record || evidence.turn_open {
                    return Err(invalid(
                        "queued input overlaps an effect-bearing population",
                    ));
                }
            }
        }
        let mut represented = BTreeSet::new();
        for input in &queued {
            represented.extend(source_ids(&input.payload)?);
        }
        if let Some(input) = &active {
            represented.extend(source_ids(&input.payload)?);
        }
        let mut inputs = Vec::new();
        for marker in markers {
            let ids = source_ids(&marker.payload)?;
            if ids.is_subset(&represented) {
                if let Some(row) = active.as_mut()
                    && source_ids(&row.payload)? == ids
                    && !queued.iter().any(|queued| {
                        source_ids(&queued.payload).is_ok_and(|sources| ids.is_subset(&sources))
                    })
                {
                    row.payload = marker.payload;
                }
                continue;
            }
            if !ids.is_disjoint(&represented) {
                return Err(invalid("partial marker overlap is ambiguous"));
            }
            represented.extend(ids);
            inputs.push(marker);
        }
        if let Some(active) = active {
            let ids = source_ids(&active.payload)?;
            let queued_ids = queued
                .iter()
                .map(|i| source_ids(&i.payload))
                .collect::<io::Result<Vec<_>>>()?;
            if !queued_ids.iter().any(|queued| ids.is_subset(queued)) {
                inputs.push(active);
            }
        }
        inputs.extend(queued);
        let rows = ledger.rows()?;
        let mut covered = BTreeSet::new();
        for input in &inputs {
            let ids = source_ids(&input.payload)?;
            if !ids.is_disjoint(&covered) {
                return Err(invalid("overlapping input populations are ambiguous"));
            }
            covered.extend(ids);
        }
        covered.extend(rows.boundary_keys());
        for (_, row) in rows.open_rows() {
            covered.extend(source_ids(
                row.input.get("legacy_input").unwrap_or(&row.input),
            )?);
        }
        for file in self.captured.iter().filter(|file| {
            file.phase == DeletePhase::Accessories
                && !file
                    .path
                    .starts_with(self.root.join("discord_busy_followup_retries"))
        }) {
            let entries: Vec<super::queued_placeholders_store::QueuedPlaceholderEntry> =
                serde_json::from_slice(&file.bytes)?;
            if entries
                .iter()
                .any(|entry| !covered.contains(&entry.user_message_id))
            {
                return Err(invalid("placeholder is not associated with a moved input"));
            }
        }
        let busy_root = self.root.join("discord_busy_followup_retries");
        self.captured.retain(|file| {
            !file.path.starts_with(&busy_root)
                || file
                    .path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .and_then(|stem| stem.parse::<u64>().ok())
                    .is_some_and(|key| covered.contains(&key))
        });
        Ok(inputs)
    }

    fn pin_input(&mut self, ledger: &Ledger, input: &mut Input) -> io::Result<()> {
        if input.payload.get("legacy_input").is_none() {
            input.payload["legacy_input"] = input.payload.clone();
        }
        if input.pins.is_empty() {
            self.pin_uploads(ledger, input)?;
        }
        for pin in &input.pins {
            ledger.read_blob(pin)?;
        }
        Ok(())
    }
    fn evidence(&mut self, input: &Input) -> io::Result<MoveEvidence> {
        let evidence = self.effects.evidence(input.key, &input.payload)?;
        if input.source == MoveSource::TurnRow
            && !evidence.user_record
            && evidence.composer == Composer::Empty
            && !self.effects.provider_alive()?
        {
            return Err(invalid(
                "provider liveness required before reinjecting a turn row",
            ));
        }
        Ok(evidence)
    }
    fn delete(&mut self, phase: DeletePhase) -> io::Result<()> {
        let population = self.population()?;
        for file in self.captured.iter().filter(|file| file.phase == phase) {
            if phase == DeletePhase::Row {
                let guard = inflight::BorrowedInflightRow::new(
                    &population,
                    &self.root,
                    &self.provider,
                    self.channel,
                )?;
                let Some(bytes) = read(&file.path)? else {
                    runtime_store::fsync_parent_dir(&file.path)?;
                    continue;
                };
                #[cfg(test)]
                let skip_check = crate::services::tui_input::transition::mutant("row_lock_check");
                #[cfg(not(test))]
                let skip_check = false;
                if !skip_check && bytes != file.bytes {
                    return Err(invalid("row snapshot changed under lock"));
                }
                let state: InflightTurnState =
                    serde_json::from_slice(if skip_check { &bytes } else { &file.bytes })?;
                let outcome = inflight::operator_disposition_remove_borrowed(
                    &guard,
                    &InflightEpisodePin::from_state(&state),
                )
                .0;
                if !matches!(
                    outcome,
                    inflight::GuardedClearOutcome::Cleared | inflight::GuardedClearOutcome::Missing
                ) {
                    return Err(invalid("pinned row deletion refused"));
                }
            } else if let Some(bytes) = read(&file.path)? {
                if bytes != file.bytes {
                    return Err(invalid("Legacy snapshot changed before retirement"));
                }
                fs::remove_file(&file.path)?;
            }
            runtime_store::fsync_parent_dir(&file.path)?;
        }
        Ok(())
    }
    fn start_actor(&mut self) -> io::Result<()> {
        self.effects.start_actor()
    }
    fn reconciliation(&mut self, key: u64, row: &Row) -> io::Result<Reconciliation> {
        self.effects.reconcile(key, row)
    }
    fn enqueue(&mut self, key: u64, row: &Row) -> io::Result<EnqueueOutcome> {
        self.effects.enqueue(key, &row.input)
    }
    fn notice(&mut self, key: Option<u64>, reason: &'static str) -> io::Result<()> {
        self.effects.notice(key, reason)
    }
}

#[cfg(test)]
#[path = "input_transition_tests.rs"]
mod tests;
