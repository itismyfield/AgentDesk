//! Native input identity and provenance; production delivery remains disabled until migration.

pub(super) mod admission;
pub(super) mod evidence;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::pieces::PieceWork;
use crate::services::tui_input::human_frame::history::{self, ObservedAttempt};
use crate::services::tui_o::shadow::{
    CapturedRecord, ShadowProvider, SourceRange, UnitKey, UnitKind,
};
use crate::services::{discord, tui_prompt_dedupe};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoticeOrigin {
    pub record: SourceRange,
    pub logical_session: String,
    pub binding_seq: u64,
    pub execution_nonce: String,
}

#[derive(Default)]
pub(crate) struct Projection {
    enabled: bool,
    seals: HashMap<UnitKey, [u8; 32]>,
    pending: Vec<crate::services::tui_o::store::ledger::input_notices::NoticeEvidence>,
}

impl Projection {
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }

    pub(crate) fn project(
        &mut self,
        channel: u64,
        provider: ShadowProvider,
        record: &CapturedRecord,
        origin: Result<NoticeOrigin, String>,
    ) -> Result<Option<PieceWork>, String> {
        if !self.enabled
            || provider != ShadowProvider::Claude
            || record.line.iter().all(u8::is_ascii_whitespace)
        {
            return Ok(None);
        }
        let value = serde_json::from_slice(&record.line)
            .map_err(|error| format!("input notice record at {}: {error}", record.start))?;
        let Some((prompt, uuid)) =
            tui_prompt_dedupe::extract_claude_transcript_user_prompt_with_entry_id(&value)
        else {
            return Ok(None);
        };
        if tui_prompt_dedupe::is_discord_relayed_user_prompt(&prompt) {
            return Ok(None);
        }
        let Some(payload) = discord::input_notice_content(&prompt) else {
            return Ok(None);
        };
        let native_key = uuid.ok_or("input notice has no native user UUID")?;
        let origin = origin?;
        if origin.record.start != record.start
            || origin.record.end != record.end
            || origin.logical_session.trim().is_empty()
            || origin.binding_seq == 0
            || origin.execution_nonce.trim().is_empty()
        {
            return Err("input notice has incomplete original provenance".into());
        }
        match history::observed_attempt_checked(
            "claude",
            &origin.logical_session,
            std::slice::from_ref(&prompt),
            Some(&native_key),
        )? {
            ObservedAttempt::Known(_) => return Ok(None),
            // Unconfirmed ownership is kept as evidence and debt until a receipt decides it.
            ObservedAttempt::Pending => {
                return Err("input notice injection ownership pending".into());
            }
            ObservedAttempt::None => {}
        }
        let unit_key = UnitKey {
            channel_id: channel,
            provider,
            native_key,
            kind: UnitKind::InputNotice,
        };
        // Display splitting consumes boundary newlines, so identity seals the extracted bytes.
        let digest: [u8; 32] = Sha256::digest(prompt.as_bytes()).into();
        match self.seals.get(&unit_key) {
            Some(sealed) if sealed == &digest => return Ok(None),
            Some(_) => return Err("input notice UUID changed its content".into()),
            None => {
                self.seals.insert(unit_key.clone(), digest);
            }
        }
        Ok(Some(PieceWork {
            unit_key,
            index: 0,
            payload,
            notice_origin: Some(Box::new(origin)),
        }))
    }
}

#[cfg(test)]
mod tests;
