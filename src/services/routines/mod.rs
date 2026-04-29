/// Routines core runtime — ORDER-P0-001: durable store + boot recovery.
///
/// This module is intentionally small at this stage. It exposes only
/// `RoutineStore` (PG-backed claim/finish/recovery transactions) and the
/// `boot_recovery` entry point called by the `routine-runtime` worker on
/// startup. No JS loader, no tick loop, no agent executor — those land in
/// ORDER-P0-002 and ORDER-P0-003.
///
/// SQLite users: this module is never instantiated when `pg_pool` is `None`,
/// so there are zero side effects for non-PG deployments.
pub mod store;

pub use store::RoutineStore;
