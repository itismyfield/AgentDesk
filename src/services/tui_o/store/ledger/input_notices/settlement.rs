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
                    || crate::services::discord::input_notice_content(prompt).is_some()
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
