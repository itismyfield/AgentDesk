use super::{AdmitOutcome, Admitter, ApprovedRejected, LedgerError, PieceKey, PriorBudget};

impl ApprovedRejected {
    pub fn for_test(rejected_serial: u64, key: PieceKey, payload: String, anchor: u64) -> Self {
        Self {
            rejected_serial,
            key,
            payload,
            anchor,
        }
    }

    pub fn key_for_test(&self) -> &PieceKey {
        &self.key
    }
}

impl Admitter<'_> {
    pub async fn adopt_operator_for_test(
        &self,
        approved: &ApprovedRejected,
        prior: PriorBudget,
    ) -> Result<AdmitOutcome, LedgerError> {
        self.adopt_operator(approved, prior).await
    }
}
