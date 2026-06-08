//! Compat facade for the session-activity resolver.
//!
//! The real implementation now lives in [`crate::services::session_activity`]
//! (#3037 bucket 3): it is pure domain logic that only depends on
//! `crate::db::session_status` and `crate::services::*`, so it belongs beside
//! the rest of the service-layer infra rather than in the server route layer.
//!
//! This module is mounted via `#[path]` in `server/routes/mod.rs`, so the
//! `crate::server::routes::session_activity::*` path keeps working unchanged for
//! all route-layer callers. Mirrors the established monitoring_store facade
//! pattern.

pub use crate::services::session_activity::SessionActivityResolver;
