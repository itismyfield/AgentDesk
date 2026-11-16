//! Saved/native overlap is certified only for the original pinned record range and exact text.
use super::*;
use crate::services::tui_o::shadow::capture::{Fingerprint, file_identity};
use crate::services::tui_o::shadow::{CapturedRecord, SourceId, UnitKey, UnitKind};
use crate::services::tui_o::writer::rotation::Owe;
use std::io::{Read, Seek, SeekFrom};

type Replay = (
    Vec<PieceWork>,
    Vec<(PieceWork, SourceRange)>,
    Option<&'static str>,
);
pub(super) fn plan(
    snapshot: &Snapshot,
    provider: ShadowProvider,
    ledger: Option<&crate::services::tui_o::store::ledger::LedgerState>,
) -> Replay {
    let saved = snapshot.row["full_response"]
        .as_str()
        .unwrap_or_default()
        .trim();
    let outcome = &snapshot.record.evidence["outcome"];
    let terminal = outcome["kind"] == "terminal";
    let result = outcome["result"].as_str().unwrap_or_default().trim();
    let body = if terminal && !result.is_empty() {
        result
    } else {
        saved
    };
    let unit_key = UnitKey {
        channel_id: snapshot.record.channel_id,
        provider,
        native_key: format!("herdr:{}:saved", snapshot.record.episode_key),
        kind: UnitKind::Body,
    };
    let pieces: Vec<_> = crate::domain::message_split::split_for_shadow(body)
        .into_iter()
        .enumerate()
        .map(|(index, (payload, _))| PieceWork {
            unit_key: unit_key.clone(),
            index: index as u32,
            payload,
            output_origin: None,
            notice_origin: None,
        })
        .collect();
    let ambiguous = pieces
        .iter()
        .any(|p| p.payload.encode_utf16().count() > crate::services::discord::DISCORD_MSG_LIMIT)
        || snapshot.row["response_sent_offset"] != 0
        || snapshot.row["terminal_delivery_committed"] == true
        || snapshot.row["session_bound_delivered"] == true
        || (terminal && !saved.is_empty() && saved != body)
        || snapshot.row["silent_turn"] == true
        || snapshot
            .row
            .get("routine_presentation")
            .is_some_and(|v| !v.is_null() && v != &json!({}));
    if ambiguous {
        return (
            pieces,
            Vec::new(),
            Some("ambiguous prior visibility or saved/native overlap"),
        );
    }
    if snapshot.record.evidence["source"].is_null() {
        return (pieces, Vec::new(), None);
    }
    let aliases = terminal
        .then(|| native(snapshot, provider, body, ledger))
        .and_then(Result::ok);
    match aliases {
        Some(aliases) => (pieces, aliases, None),
        None => (pieces, Vec::new(), Some("unproved native coverage")),
    }
}

fn native(
    snapshot: &Snapshot,
    provider: ShadowProvider,
    body: &str,
    ledger: Option<&crate::services::tui_o::store::ledger::LedgerState>,
) -> Result<Vec<(PieceWork, SourceRange)>, String> {
    let evidence = &snapshot.record.evidence;
    let source: SourceId =
        serde_json::from_value(evidence["source"].clone()).map_err(|e| e.to_string())?;
    let outcome = &evidence["outcome"];
    let (start, end) = (
        outcome["start"].as_u64().ok_or("missing start")?,
        outcome["end"].as_u64().ok_or("missing end")?,
    );
    if evidence["execution_nonce"]
        .as_str()
        .is_none_or(|s| s.trim().is_empty())
        || snapshot
            .row
            .get("hosted_execution_nonce")
            .filter(|v| !v.is_null())
            .is_some_and(|v| v != &evidence["execution_nonce"])
        || start >= end
        || end - start > 16 * 1024 * 1024
    {
        return Err("unbounded native turn".into());
    }
    let mut file = std::fs::File::open(&source.path).map_err(|e| e.to_string())?;
    let before =
        Fingerprint::of(&file.metadata().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    if (source.dev, source.ino) != file_identity(&file.metadata().map_err(|e| e.to_string())?)
        || before.len < end
    {
        return Err("native file identity changed".into());
    }
    file.seek(SeekFrom::Start(start))
        .map_err(|e| e.to_string())?;
    let mut bytes = vec![0; (end - start) as usize];
    file.read_exact(&mut bytes).map_err(|e| e.to_string())?;
    let mut owed = super::super::owed::OwedLog::new(snapshot.record.channel_id, provider);
    let mut deriver = super::super::pieces::UnitDeriver::new(snapshot.record.channel_id, provider);
    let mut offset = start;
    if !bytes.ends_with(b"\n") {
        return Err("partial native record".into());
    }
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        let record = CapturedRecord {
            start: offset,
            end: offset + line.len() as u64,
            line: line[..line.len() - 1].to_vec(),
        };
        let derived = deriver.derive(&record);
        owed.owe(&source, &record, derived);
        offset = record.end;
    }
    let after =
        Fingerprint::of(&file.metadata().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    if before != after || Fingerprint::path(&source).map_err(|e| e.1)? != after {
        return Err("native range changed during coverage read".into());
    }
    let mut aliases = owed.body_coordinates()?;
    if aliases.is_empty()
        || aliases
            .iter()
            .map(|(piece, _)| piece.payload.as_str())
            .collect::<String>()
            != body
    {
        return Err("native text is not exact saved coverage".into());
    }
    // A later transcript with the same text cannot lend its native IDs to the old input.
    let ledger = ledger
        .filter(|l| l.violation().is_none())
        .ok_or("no original O ledger evidence")?;
    let delivery = snapshot.row["intake_outbox_id"]
        .as_i64()
        .filter(|id| *id > 0)
        .ok_or("original native input identity unknown")?;
    for (piece, _) in &mut aliases {
        let (serial, prepared) = ledger
            .latest_piece(&piece.unit_key, piece.index)
            .ok_or("native unit absent from original ledger")?;
        let artifact = ledger
            .output_origin(serial)
            .ok_or("native unit has no original admission")?;
        if !matches!(
            artifact,
            crate::services::cluster::intake_router_hook::owner_record::output::Artifact::Original(
                _
            )
        ) {
            return Err("native unit is not an original admission".into());
        }
        let stamp = serde_json::to_value(artifact).map_err(|e| e.to_string())?;
        if prepared.payload != piece.payload
            || stamp["payload_hash"] != sha(piece.payload.as_bytes())
            || stamp["origin"]["delivery"] != delivery
            || stamp["origin"]["provider"] != provider_name(provider)
            || stamp["origin"]["channel"] != snapshot.record.channel_id.to_string()
        {
            return Err("native admission belongs to another input or payload".into());
        }
        piece.output_origin = Some(artifact.clone());
    }
    Ok(aliases)
}
