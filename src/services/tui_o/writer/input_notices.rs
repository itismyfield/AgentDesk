//! Native input identity and provenance; the writer owns Claude terminal-input notices.

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

/// The writer derives Claude terminal-input notices only where the channel's transcript turns
/// were confirmed; other channels keep their live owner.
pub(crate) fn writer_owns(channel: u64, provider: ShadowProvider) -> bool {
    provider == ShadowProvider::Claude
        && crate::services::tui_o::turn_mode::transcript_turns(channel)
}

/// The live relay leaves a notice to the writer only once the writer's frozen legacy cohort is
/// durable, so every input it still announced before that stays inside the cohort.
pub(crate) fn writer_posts(channel: u64, provider: ShadowProvider) -> bool {
    writer_owns(channel, provider) && handed_over(channel)
}

fn handed_over(channel: u64) -> bool {
    use std::sync::{Mutex, OnceLock, PoisonError};
    static RECORDED: OnceLock<Mutex<std::collections::HashSet<(std::path::PathBuf, u64)>>> =
        OnceLock::new();
    let Some(root) = crate::config::runtime_root() else {
        return false;
    };
    let recorded = RECORDED.get_or_init(Default::default);
    let key = (root, channel);
    if (recorded.lock().unwrap_or_else(PoisonError::into_inner)).contains(&key) {
        return true;
    }
    let found =
        crate::services::tui_o::store::input_notice_floor::migration_recorded(&key.0, channel);
    if found {
        (recorded.lock().unwrap_or_else(PoisonError::into_inner)).insert(key);
    }
    found
}

/// Discord input by its frame, the pre-v1 `[📱 …]` inject frame included even inside the one paste
/// block Claude may record it in; only this classification looks through the block.
pub(crate) fn discord_input(prompt: &str) -> bool {
    let framed = crate::services::claude_tui::busy_inject::pasted_inner(prompt).unwrap_or(prompt);
    tui_prompt_dedupe::is_discord_relayed_user_prompt(prompt)
        || crate::services::tui_input::human_frame::opens_legacy_inject(framed)
}

/// A native user row always carries the JSON string `"user"`, so other rows skip the parse.
fn may_be_user(line: &[u8]) -> bool {
    line.windows(6).any(|window| window == b"\"user\"")
}

/// Only a row that may become a notice needs its binding origin, which reads the whole binding log.
pub(crate) fn needs_origin(record: &CapturedRecord) -> bool {
    may_be_user(&record.line)
        && serde_json::from_slice(&record.line)
            .ok()
            .and_then(|value| {
                tui_prompt_dedupe::extract_claude_transcript_user_prompt_with_entry_id(&value)
            })
            .is_some_and(|(prompt, _)| {
                !discord_input(&prompt) && discord::input_notice_content(&prompt).is_some()
            })
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

    pub(crate) fn for_provider(channel: u64, provider: ShadowProvider) -> Self {
        Self {
            enabled: writer_owns(channel, provider),
            ..Self::default()
        }
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
        if !self.enabled || provider != ShadowProvider::Claude || !may_be_user(&record.line) {
            return Ok(None);
        }
        let value = serde_json::from_slice(&record.line)
            .map_err(|error| format!("input notice record at {}: {error}", record.start))?;
        let Some((prompt, uuid)) =
            tui_prompt_dedupe::extract_claude_transcript_user_prompt_with_entry_id(&value)
        else {
            return Ok(None);
        };
        if discord_input(&prompt) {
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
