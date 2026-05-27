//! Phase 0 stub. Phase 1 owns the per-channel spawn gate so two queued
//! Discord turns on the same channel cannot spawn concurrent `claude-e`
//! processes against the same session id.
