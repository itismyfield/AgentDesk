//! Short operator transactions on the existing ledger: validate, raise the format floor, append.

use std::fs::{File, OpenOptions};

use super::*;
use crate::services::discord::runtime_store::fsync_parent_dir;
use ledger::ResumeApproval;

pub const OPERATOR_RESUME_FLOOR: &str = "operator_resume.floor";

/// What one approval call did, decided under the ledger lock: wrote and synced it, or found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResumeRecord {
    Recorded(ResumeApproval),
    Existing(ResumeApproval),
}

impl ResumeRecord {
    pub fn approval(&self) -> &ResumeApproval {
        match self {
            Self::Recorded(approval) | Self::Existing(approval) => approval,
        }
    }
}

impl OStore {
    fn resume_snapshot(
        &self,
        channel: u64,
    ) -> Result<(durable::LockedFile, LedgerState), StoreError> {
        let path = self.channel_dir(channel).join(LEDGER_FILE);
        let file = OpenOptions::new().read(true).append(true).open(path)?;
        let mut file = durable::LockedFile::try_lock(file)?;
        let init = self
            .read_init(channel)?
            .ok_or_else(|| damage("channel has no init"))?;
        self.read_era()?.ok_or_else(|| damage("store has no era"))?;
        let state = ledger::read_only(&mut file, init.initial_anchor)?;
        if let Some(detail) = state.violation() {
            return Err(StoreError::Rejected(format!("ledger violation: {detail}")));
        }
        Ok((file, state))
    }

    pub fn operator_resume_status(&self, channel: u64) -> Result<LedgerState, StoreError> {
        self.resume_snapshot(channel).map(|(_, state)| state)
    }

    pub fn record_operator_resume(
        &self,
        channel: u64,
        rejected_serial: u64,
        operator: &str,
        reason: &str,
    ) -> Result<ResumeApproval, StoreError> {
        self.record_operator_resume_with_append(
            channel,
            rejected_serial,
            operator,
            reason,
            ledger::append_to,
        )
    }

    /// As `record_operator_resume`, also telling whether this call wrote the approval.
    pub fn record_operator_resume_outcome(
        &self,
        channel: u64,
        rejected_serial: u64,
        operator: &str,
        reason: &str,
    ) -> Result<ResumeRecord, StoreError> {
        let append = ledger::append_to;
        self.resume_transaction(channel, rejected_serial, operator, reason, append)
    }

    pub(super) fn record_operator_resume_with_append(
        &self,
        channel: u64,
        rejected_serial: u64,
        operator: &str,
        reason: &str,
        append: impl FnOnce(&mut File, DateTime<Utc>, &LedgerEntry) -> Result<(), StoreError>,
    ) -> Result<ResumeApproval, StoreError> {
        self.resume_transaction(channel, rejected_serial, operator, reason, append)
            .map(|record| record.approval().clone())
    }

    fn resume_transaction(
        &self,
        channel: u64,
        rejected_serial: u64,
        operator: &str,
        reason: &str,
        append: impl FnOnce(&mut File, DateTime<Utc>, &LedgerEntry) -> Result<(), StoreError>,
    ) -> Result<ResumeRecord, StoreError> {
        let (mut file, mut state) = self.resume_snapshot(channel)?;
        if let Some(existing) = state.approval(rejected_serial) {
            return Ok(ResumeRecord::Existing(existing.clone()));
        }
        if !state.can_resume(rejected_serial)
            || state
                .piece(rejected_serial)
                .is_none_or(|piece| piece.unit_key.channel_id != channel)
            || operator.trim().is_empty()
            || reason.trim().is_empty()
        {
            return Err(StoreError::Rejected(
                "resume requires the latest 400/403/404, no open POST and nonblank audit input"
                    .into(),
            ));
        }
        let floor = self.root.join(OPERATOR_RESUME_FLOOR);
        // The durable name precedes even an incomplete approval append, so rollback cannot lose its reader.
        match std::fs::symlink_metadata(&floor) {
            Ok(_) => fsync_parent_dir(&floor)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if let Err(error) = durable::create_once(&floor, b"1\n") {
                    if error.kind() != io::ErrorKind::AlreadyExists {
                        return Err(error.into());
                    }
                    fsync_parent_dir(&floor)?;
                }
            }
            Err(error) => return Err(error.into()),
        }
        let at = Utc::now();
        let entry = LedgerEntry::OperatorResume {
            rejected_serial,
            approval_id: uuid::Uuid::new_v4(),
            operator: operator.into(),
            reason: reason.into(),
            at,
        };
        append(&mut file, at, &entry)?;
        state.apply(at, entry);
        state
            .approval(rejected_serial)
            .cloned()
            .map(ResumeRecord::Recorded)
            .ok_or_else(|| damage("validated approval was not applied"))
    }
}
