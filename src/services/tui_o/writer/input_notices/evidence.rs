//! Retain the native row before the spool can release it, including unresolved identity.

use super::*;
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::store::ledger::input_notices::NoticeEvidence;

impl Projection {
    pub(crate) fn project_record(
        &mut self,
        channel: u64,
        provider: ShadowProvider,
        source: &SourceId,
        record: &CapturedRecord,
        origin: Result<NoticeOrigin, String>,
    ) -> Result<Option<PieceWork>, String> {
        let projected = self.project(channel, provider, record, origin.clone());
        if matches!(projected, Ok(None)) || !self.enabled || provider != ShadowProvider::Claude {
            return projected;
        }
        let input = serde_json::from_slice(&record.line).ok().and_then(|value| {
            tui_prompt_dedupe::extract_claude_transcript_user_prompt_with_entry_id(&value)
        });
        if let Some((prompt, uuid)) = input {
            let range = SourceRange {
                source: source.clone(),
                start: record.start,
                end: record.end,
            };
            let origin = origin.ok().filter(|origin| {
                origin.record == range
                    && !origin.logical_session.trim().is_empty()
                    && origin.binding_seq != 0
                    && !origin.execution_nonce.trim().is_empty()
            });
            let payload = discord::input_notice_content(&prompt);
            let key = uuid
                .filter(|uuid| !uuid.trim().is_empty())
                .map(|native_key| UnitKey {
                    channel_id: channel,
                    provider,
                    native_key,
                    kind: UnitKind::InputNotice,
                });
            self.pending.push(NoticeEvidence {
                record: range,
                raw_record: record.line.clone(),
                prompt_sha256: Some(hex::encode(Sha256::digest(prompt.as_bytes()))),
                prompt: Some(prompt),
                unit_key: key,
                payload,
                origin: origin.map(Box::new),
            });
        }
        projected
    }

    pub(crate) fn take_evidence(&mut self) -> Vec<NoticeEvidence> {
        std::mem::take(&mut self.pending)
    }
}

pub(crate) fn before_migration_floor(
    state: &crate::services::tui_o::store::ledger::input_notices::NoticeState,
    record: &SourceRange,
) -> bool {
    state.migration.as_ref().is_some_and(|snapshot| {
        snapshot
            .sources
            .iter()
            .any(|source| source.source == record.source && record.start < source.lower)
    })
}
