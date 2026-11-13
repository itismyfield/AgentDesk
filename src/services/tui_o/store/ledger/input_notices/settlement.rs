//! Terminal evidence retains its original record and never withdraws a prepared send.
use super::super::LedgerState;
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Settled {
    Policy {
        prompt_sha256: String,
    },
    Owned {
        origin: Box<NoticeOrigin>,
        native_key: String,
        prompt_sha256: String,
        owner: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settlement {
    pub proof: Settled,
    pub expired_after: Option<u64>,
}

// This sufficient subset uses the neutral parser; other policy shapes keep normal admission.
pub(crate) fn policy_proven(prompt: &str) -> bool {
    use crate::services::tui_prompt_control as control;
    if control::classify_local_only_slash_control(prompt).is_some() {
        return true;
    }
    let clean = control::strip_terminal_controls(prompt);
    let mut text = control::strip_leading_injection_wrapper(clean.trim_start()).trim_start();
    if ["task-notification", "subagent_notification"]
        .iter()
        .any(|tag| {
            text.strip_prefix(&format!("<{tag}")).is_some_and(|tail| {
                tail.starts_with('>') || tail.chars().next().is_some_and(char::is_whitespace)
            })
        })
        || [
            "This session is being continued from a previous conversation",
            "Please continue the conversation from where we left it off",
        ]
        .iter()
        .any(|opening| text.starts_with(opening))
    {
        return true;
    }
    if let Some(tail) = text.strip_prefix("<local-command-caveat>")
        && let Some((_, rest)) = tail.split_once("</local-command-caveat>")
    {
        text = rest.trim_start();
    }
    text.starts_with("<command-name>")
        || text.starts_with("<command-message>")
        || text.starts_with("/loop ")
}

impl NoticeState {
    pub(crate) fn revision(&self, id: &RecordId) -> u64 {
        self.evidence.get(id).map_or(0, |e| {
            u64::from(e.prompt.is_some())
                | (u64::from(e.prompt_sha256.is_some()) << 1)
                | (u64::from(e.unit_key.is_some()) << 2)
                | (u64::from(e.payload.is_some()) << 3)
                | (u64::from(e.origin.is_some()) << 4)
                | (u64::from(self.unresolved.contains_key(id)) << 5)
        })
    }

    pub(crate) fn terminal(
        &self,
        ledger: &LedgerState,
        id: &RecordId,
        evidence: &NoticeEvidence,
    ) -> bool {
        self.settled.contains_key(id)
            || evidence.unit_key.as_ref().is_some_and(|key| {
                ledger.excluded(key).is_some()
                    || ledger.latest_piece(key, 0).is_some_and(|(_, piece)| {
                        matches!(piece.outcome, Some(super::super::PieceOutcome::Posted(_)))
                    })
            })
    }

    pub(super) fn settle(
        &mut self,
        record: SourceRange,
        settlement: Settlement,
    ) -> Result<Option<UnitKey>, String> {
        let id = record_id(&record);
        let evidence = self
            .evidence
            .get(&id)
            .ok_or("settled notice has no evidence")?;
        evidence.validate()?;
        let prompt = evidence
            .prompt
            .as_deref()
            .ok_or("settled notice has no prompt")?;
        let digest = hex::encode(Sha256::digest(prompt.as_bytes()));
        match &settlement.proof {
            Settled::Policy { prompt_sha256 } => {
                if *prompt_sha256 != digest
                    || !policy_proven(prompt)
                    || settlement.expired_after.is_some()
                {
                    return Err("settled notice policy is not a machine or local-only input".into());
                }
            }
            Settled::Owned {
                origin,
                native_key,
                prompt_sha256,
                owner,
            } => {
                if evidence.origin.as_deref() != Some(origin)
                    || origin.record != record
                    || evidence
                        .unit_key
                        .as_ref()
                        .is_none_or(|key| key.native_key != *native_key)
                    || *prompt_sha256 != digest
                    || owner.trim().is_empty()
                    || settlement
                        .expired_after
                        .is_some_and(|seq| seq <= origin.binding_seq)
                {
                    return Err("settled notice ownership proof changed its source or UUID".into());
                }
            }
        }
        if self
            .settled
            .get(&id)
            .is_some_and(|kept| *kept != settlement)
        {
            return Err("settled notice disposition changed".into());
        }
        self.settled.entry(id).or_insert(settlement);
        Ok(evidence.unit_key.clone())
    }
}

pub(crate) fn apply(
    ledger: &mut LedgerState,
    at: chrono::DateTime<chrono::Utc>,
    entry: NoticeEntry,
) {
    let started = matches!(entry, NoticeEntry::MigrationStarted { .. });
    let terminal_update = match &entry {
        NoticeEntry::Settled { .. } => true,
        NoticeEntry::Materialized { evidence } => ledger
            .input_notices
            .settled
            .contains_key(&record_id(&evidence.record)),
        _ => false,
    };
    let settled_key = match &entry {
        NoticeEntry::Settled { record, .. } => ledger
            .input_notices
            .evidence
            .get(&record_id(record))
            .and_then(|e| e.unit_key.as_ref()),
        NoticeEntry::Materialized { evidence } if terminal_update => evidence.unit_key.as_ref(),
        _ => None,
    };
    if settled_key.is_some_and(|key| ledger.latest_piece(key, 0).is_some()) {
        ledger.violate("settled notice still has a prepared or posted send".into());
        return;
    }
    let reason = if terminal_update {
        "input_notice_source"
    } else {
        "input_notice_legacy_imported"
    };
    match ledger.input_notices.apply(entry) {
        Ok(Some(key)) => {
            ledger.excluded.insert(key, reason.into());
        }
        Ok(None) => {}
        Err(reason) => ledger.violate(reason),
    }
    if started && ledger.input_notices.migration.is_some() {
        ledger.input_notices.frozen_at.get_or_insert(at);
    }
}
