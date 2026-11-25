//! Derives each record's tool images into queued ledger entries and FIFO markers. The writer
//! appends the entries before it delivers the markers or collects any segment.

use std::collections::HashMap;

use serde_json::Value;

use super::notice;
use super::turns::{ImageTurns, TurnEdge};
use super::{ImageBody, NativeTurn, image_native_key};
use crate::services::tui_o::shadow::identity::{RecordFact, row_key};
use crate::services::tui_o::shadow::{CapturedRecord, ShadowProvider, SourceId, UnitKey, UnitKind};
use crate::services::tui_o::store::InitSource;
use crate::services::tui_o::store::ledger::LedgerState;
use crate::services::tui_o::store::ledger::tool_images::{ImageEntry, ImageObligation, ImageState};
use crate::services::tui_o::store::spool::source_key;
use crate::services::tui_o::writer::pieces::PieceWork;

pub(crate) struct ImageRun {
    channel: u64,
    provider: ShadowProvider,
    /// The ledger's image state at start plus every entry queued since.
    known: ImageState,
    /// Each init source's durable delivery start; any other source starts at 0.
    origins: HashMap<String, u64>,
    turns: HashMap<String, ImageTurns>,
    source: Option<SourceId>,
    queued: Vec<ImageEntry>,
}

impl ImageRun {
    pub(crate) fn new(
        channel: u64,
        provider: ShadowProvider,
        ledger: &LedgerState,
        sources: &[InitSource],
    ) -> Self {
        let origins = sources
            .iter()
            .map(|source| (source_key(&source.source_id), source.delivery_start));
        Self {
            channel,
            provider,
            known: ledger.tool_images().clone(),
            origins: origins.collect(),
            turns: HashMap::new(),
            source: None,
            queued: Vec::new(),
        }
    }

    /// Names the source the next records come from.
    pub(crate) fn enter(&mut self, source: &SourceId) {
        self.source = Some(source.clone());
    }

    pub(crate) fn take_entries(&mut self) -> Vec<ImageEntry> {
        std::mem::take(&mut self.queued)
    }

    pub(crate) fn has_queued(&self) -> bool {
        !self.queued.is_empty()
    }

    /// Spool keys whose open turn still needs its opener: GC waits for its durable Closed.
    pub(crate) fn live_openers(&self, durable: &ImageState) -> Vec<String> {
        let open = self.turns.iter().filter(|(_, turns)| {
            turns
                .open_turn()
                .is_some_and(|turn| !turns.opener_collectable(durable, turn))
        });
        open.map(|(key, _)| key.clone()).collect()
    }

    /// One record's turn facts and images; returns a marker per image owed from this record.
    pub(crate) fn record(
        &mut self,
        record: &CapturedRecord,
        value: &Value,
        facts: &[RecordFact],
    ) -> Vec<PieceWork> {
        let Some(source) = self.source.clone() else {
            return Vec::new();
        };
        let key = source_key(&source);
        let origin = self.origins.get(&key).copied().unwrap_or(0);
        let turns = (self.turns.entry(key))
            .or_insert_with(|| ImageTurns::starting_at(source.clone(), record.start, origin));
        turns.before_offset(record.start);
        let row = row_key(value);
        let mut edges = Vec::new();
        for fact in facts {
            edges.extend(turns.observe(fact, row.as_deref(), record.start));
        }
        for call in call_ids(self.provider, value) {
            turns.note_call(call);
        }
        let mut images = Vec::new();
        for image in super::extract(self.provider, value) {
            let found = turns.turn_for_image(&image.call_id, record.start, &self.known);
            images.push((image, found));
        }
        for edge in edges {
            self.close(&source, edge);
        }
        let mut markers = Vec::new();
        for (image, found) in images {
            // Two images of a new turn in one row both see it without a span; queue it once.
            if let Some(TurnEdge::Opened {
                turn,
                opener_offset,
            }) = found.open_span
                && self.known.span_of(&source, turn.as_str()).is_none()
            {
                self.queue(ImageEntry::ImageTurnOpened {
                    source: source.clone(),
                    opener_offset,
                    turn: turn.as_str().to_owned(),
                });
            }
            markers.extend(self.image(&source, record, &found.turn, image));
        }
        markers
    }

    /// A verified physical end: the open turn closes at `end_offset`, keeping its calls.
    pub(crate) fn physical_end(&mut self, source: &SourceId, end_offset: u64) {
        let Some(turns) = self.turns.get_mut(&source_key(source)) else {
            return;
        };
        if let Some(edge) = turns.physical_end(end_offset) {
            self.close(source, edge);
        }
    }

    /// Records a turn's Closed only when it has a span; a replayed close of a closed span is
    /// already known and not queued again.
    fn close(&mut self, source: &SourceId, edge: TurnEdge) {
        let TurnEdge::Closed { turn, end_offset } = edge else {
            return;
        };
        let Some(span) = self.known.span_of(source, turn.as_str()) else {
            return;
        };
        if span.end_offset == Some(end_offset) {
            return;
        }
        self.queue(ImageEntry::ImageTurnClosed {
            source: source.clone(),
            turn: turn.as_str().to_owned(),
            end_offset,
        });
    }

    fn image(
        &mut self,
        source: &SourceId,
        record: &CapturedRecord,
        turn: &NativeTurn,
        image: super::ToolImage,
    ) -> Option<PieceWork> {
        let native_key = match &image.body {
            ImageBody::Inline { sha256, .. } => image_native_key(turn, sha256),
            ImageBody::Omitted(_) => {
                format!("ref:{}:{}:{}", turn.as_str(), image.call_id, image.index)
            }
        };
        let unit_key = UnitKey {
            channel_id: self.channel,
            provider: self.provider,
            native_key,
            kind: UnitKind::ToolImage,
        };
        let index = u32::try_from(image.index).unwrap_or(u32::MAX);
        if let Some(known) = self.known.obligation(&unit_key) {
            // Only the occurrence that created the obligation marks its place again on replay.
            let origin = known.record_start == record.start
                && known.call_id == image.call_id
                && known.image_index == index;
            let caption = notice::caption(&known.display_name, &known.marker);
            return (origin && known.sha256.is_some()).then(|| marker(unit_key, caption));
        }
        let (sha256, media_type, size, omitted) = match &image.body {
            ImageBody::Inline {
                media_type,
                sha256,
                bytes,
            } => (
                Some(sha256.clone()),
                Some(media_type.clone()),
                Some(bytes.len() as u64),
                None,
            ),
            ImageBody::Omitted(reason) => (None, None, None, Some(*reason)),
        };
        let display_name =
            notice::display_name(image.index, media_type.as_deref().unwrap_or_default());
        let obligation = ImageObligation {
            marker: notice::marker(&unit_key.native_key),
            unit_key: unit_key.clone(),
            turn: turn.as_str().to_owned(),
            call_id: image.call_id,
            image_index: index,
            source: source.clone(),
            record_start: record.start,
            record_end: record.end,
            sha256,
            media_type,
            size,
            display_name,
        };
        let caption = notice::caption(&obligation.display_name, &obligation.marker);
        self.queue(ImageEntry::ImageObligation(Box::new(obligation)));
        match omitted {
            Some(reason) => {
                self.queue(ImageEntry::ImageOmitted {
                    unit_key,
                    reason: reason.as_str().to_owned(),
                });
                None
            }
            None => Some(marker(unit_key, caption)),
        }
    }

    fn queue(&mut self, entry: ImageEntry) {
        // A rejected entry is still appended, so the ledger's own check stops the channel.
        let _ = self.known.apply(entry.clone());
        self.queued.push(entry);
    }
}

fn marker(unit_key: UnitKey, caption: String) -> PieceWork {
    PieceWork {
        unit_key,
        index: 0,
        payload: caption,
        output_origin: None,
        notice_origin: None,
    }
}

/// Tool call ids in one row: Claude `tool_use` blocks, Codex function and custom tool calls.
fn call_ids(provider: ShadowProvider, value: &Value) -> Vec<&str> {
    match provider {
        ShadowProvider::Claude => {
            let blocks = value.pointer("/message/content").and_then(Value::as_array);
            let calls = blocks
                .into_iter()
                .flatten()
                .filter(|block| block["type"] == "tool_use");
            calls.filter_map(|block| block["id"].as_str()).collect()
        }
        ShadowProvider::Codex => {
            let payload = &value["payload"];
            let call = value["type"] == "response_item"
                && matches!(
                    payload["type"].as_str(),
                    Some("function_call" | "custom_tool_call")
                );
            call.then(|| payload["call_id"].as_str())
                .flatten()
                .into_iter()
                .collect()
        }
        ShadowProvider::Antigravity => Vec::new(),
    }
}
