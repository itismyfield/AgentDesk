use super::*;

impl OStore {
    pub fn seed_boundary_resolved(
        &self,
        channel: u64,
        source: &SourceId,
        from: u64,
    ) -> Result<(SourceId, u64), StoreError> {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(self.channel_dir(channel).join(LEDGER_FILE))?;
        let at = Utc::now();
        ledger::append_to(
            &mut file,
            at,
            &LedgerEntry::BoundaryResolved {
                source: source.clone(),
                from,
                excluded_range: 0..from,
                operator: "replay-fixture".into(),
                at,
                evidence: None,
            },
        )?;
        Ok((source.clone(), from))
    }

    /// Seeds a durable approval for writer replay tests without an operator command.
    pub fn record_operator_resume(
        &self,
        channel: u64,
        rejected_serial: u64,
        operator: &str,
        reason: &str,
    ) -> Result<ledger::ResumeApproval, StoreError> {
        let path = self.channel_dir(channel).join(LEDGER_FILE);
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open(path)?;
        let init = self
            .read_init(channel)?
            .ok_or_else(|| damage("missing init"))?;
        let mut state = ledger::read_only(&mut file, init.initial_anchor)?;
        if let Some(approval) = state.approval(rejected_serial) {
            return Ok(approval.clone());
        }
        let at = Utc::now();
        let entry = LedgerEntry::OperatorResume {
            rejected_serial,
            approval_id: uuid::Uuid::new_v4(),
            operator: operator.into(),
            reason: reason.into(),
            at,
        };
        ledger::append_to(&mut file, at, &entry)?;
        state.apply(at, entry);
        state
            .approval(rejected_serial)
            .cloned()
            .ok_or_else(|| damage("test approval was not applied"))
    }
}
