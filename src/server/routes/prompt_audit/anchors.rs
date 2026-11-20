use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::db::provider_session_prompts::SessionPrompt;
use crate::domain::memory::{ANCHOR_CONTEXT_END, AnchorRecord};
use crate::services::{
    discord::prompt_builder::audit::CanonPrompt, memory::ExternalMemoryBackendState,
    provider::ProviderKind,
};

use super::{projection::anchor_context, redaction};

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum ConnectionStatus {
    Unconfigured,
    Unknown,
    Connected,
    Disconnected,
}

#[derive(Serialize)]
struct Connection {
    status: ConnectionStatus,
    checked_at: Option<DateTime<Utc>>,
}

impl From<Option<ExternalMemoryBackendState>> for Connection {
    fn from(state: Option<ExternalMemoryBackendState>) -> Self {
        let status = match &state {
            Some(state)
                if state.configured && (!state.active || state.consecutive_failures > 0) =>
            {
                ConnectionStatus::Disconnected
            }
            Some(state) if state.configured && state.last_checked_at.is_some() => {
                ConnectionStatus::Connected
            }
            Some(state) if state.configured => ConnectionStatus::Unknown,
            _ => ConnectionStatus::Unconfigured,
        };
        Self {
            status,
            checked_at: state
                .and_then(|state| state.last_checked_at)
                .map(DateTime::from),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum SnapshotSource {
    CurrentSession,
    LatestSaved,
    Unavailable,
}

#[derive(Serialize)]
struct Anchor {
    id: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    scope: Option<String>,
    chars: usize,
    limit: Option<u64>,
    text: String,
    masked_count: usize,
    overlap_candidates: Vec<Overlap>,
    receiving_agents: Vec<String>,
}

#[derive(Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
enum Overlap {
    Anchor { index: usize },
    CanonFile { path: &'static str },
}

#[derive(Serialize)]
pub(super) struct AnchorResponse {
    agent_id: String,
    provider: String,
    session_id: Option<String>,
    session_started_at: Option<DateTime<Utc>>,
    source: SnapshotSource,
    memento: Connection,
    anchor_count: i64,
    anchors: Vec<Anchor>,
}

pub(super) fn response(
    id: &str,
    provider: &ProviderKind,
    current: Option<String>,
    snapshot: Option<&SessionPrompt>,
    health: Option<ExternalMemoryBackendState>,
    canon: &[CanonPrompt],
) -> AnchorResponse {
    let source = match (snapshot, &current) {
        (Some(_), Some(_)) => SnapshotSource::CurrentSession,
        (Some(_), None) => SnapshotSource::LatestSaved,
        (None, _) => SnapshotSource::Unavailable,
    };
    let records = snapshot
        .map(|saved| {
            saved
                .anchors
                .as_ref()
                .map_or_else(|| legacy_anchors(saved), |records| records.0.clone())
        })
        .unwrap_or_default();
    let normalized: Vec<_> = records
        .iter()
        .map(|record| normalized(&record.content))
        .collect();
    let canon: Vec<_> = canon
        .iter()
        .map(|file| (file.path, self::normalized(&file.content)))
        .collect();
    let id = redaction::mask(id).0;
    let anchors = records
        .into_iter()
        .enumerate()
        .map(|(index, record)| {
            let overlap_candidates = overlaps(index, &normalized, &canon);
            let (text, masked_count) = redaction::mask(&record.content);
            Anchor {
                id: masked(record.id),
                kind: masked(record.kind),
                scope: masked(record.scope),
                chars: text.chars().count(),
                limit: record.limit,
                text,
                masked_count,
                overlap_candidates,
                receiving_agents: vec![id.clone()],
            }
        })
        .collect();
    AnchorResponse {
        agent_id: id,
        provider: provider.as_str().to_owned(),
        session_id: masked(
            snapshot
                .map(|saved| saved.provider_session_id.clone())
                .or(current),
        ),
        session_started_at: snapshot.map(|saved| saved.started_at),
        source,
        memento: health.into(),
        anchor_count: snapshot.map_or(0, |saved| saved.anchor_count),
        anchors,
    }
}

fn masked(input: Option<String>) -> Option<String> {
    input.map(|value| redaction::mask(&value).0)
}

fn normalized(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn overlaps(index: usize, anchors: &[String], canon: &[(&'static str, String)]) -> Vec<Overlap> {
    let text = &anchors[index];
    if text.is_empty() {
        return Vec::new();
    }
    let anchors = anchors.iter().enumerate().filter_map(|(other, anchor)| {
        (other != index && !anchor.is_empty() && (anchor.contains(text) || text.contains(anchor)))
            .then_some(Overlap::Anchor { index: other })
    });
    let canon = canon.iter().filter_map(|(path, content)| {
        content
            .contains(text)
            .then_some(Overlap::CanonFile { path })
    });
    anchors.chain(canon).collect()
}

fn legacy_anchors(saved: &SessionPrompt) -> Vec<AnchorRecord> {
    if saved.anchor_count <= 0 {
        return Vec::new();
    }
    let Some(block) = anchor_context(&saved.system_prompt).and_then(|range| {
        let text = &saved.system_prompt[range];
        let (_, text) = text.split_once("\n[ANCHOR MEMORY]\n")?;
        text.strip_suffix(ANCHOR_CONTEXT_END)
    }) else {
        return Vec::new();
    };
    let record = |content: &str| AnchorRecord {
        id: None,
        kind: None,
        scope: None,
        limit: None,
        content: content.to_owned(),
    };
    if let Some(items) = block.strip_prefix("- ") {
        let items: Vec<_> = items.split("\n- ").collect();
        if i64::try_from(items.len()).ok() == Some(saved.anchor_count) {
            return items.into_iter().map(record).collect();
        }
    }
    vec![record(block)]
}
