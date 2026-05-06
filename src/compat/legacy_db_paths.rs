//! Legacy runtime-local DB filenames retained for the doctor purge sweep.
//!
//! Pre-Postgres deployments could leave zero-byte stragglers under the
//! runtime root. Doctor walks this list to clean them up. The literals live
//! in `compat/` so production code does not trip the `legacy_sqlite_refs`
//! audit gate.

// REMOVE_WHEN: doctor no longer scans runtime root for legacy DB files —
// every dev/release deployment has gone one full retention cycle without
// any of these filenames being present.
pub const LEGACY_LOCAL_DB_FILENAMES: &[&str] =
    &["agentdesk.db", "data.db", "db.sqlite3", "agentdesk.sqlite"];
