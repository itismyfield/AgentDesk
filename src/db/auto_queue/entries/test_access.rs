//! Test-only reads of private entry status items for tests hosted outside this module.

use super::EntryStatusRow;
use sqlx::PgPool;

pub(crate) async fn load_entry_status_row_pg_for_test(
    pool: &PgPool,
    entry_id: &str,
) -> Result<EntryStatusRow, String> {
    super::load_entry_status_row_pg(pool, entry_id).await
}

pub(crate) fn resume_session_id_from_context_for_test(context: Option<&str>) -> Option<String> {
    super::resume_session_id_from_context(context)
}

impl EntryStatusRow {
    pub(crate) fn status_for_test(&self) -> &str {
        &self.status
    }

    pub(crate) fn dispatch_id_for_test(&self) -> Option<&str> {
        self.dispatch_id.as_deref()
    }

    pub(crate) fn slot_index_for_test(&self) -> Option<i64> {
        self.slot_index
    }

    pub(crate) fn completed_at_for_test(&self) -> Option<&str> {
        self.completed_at.as_deref()
    }
}
