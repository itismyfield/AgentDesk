use std::sync::OnceLock;

static AUDIT_DB: OnceLock<crate::db::Db> = OnceLock::new();

/// Initialize the audit DB handle. Call once during server startup.
pub fn init_audit_db(db: crate::db::Db) {
    let _ = AUDIT_DB.set(db);
}

/// Record a session termination event. Fire-and-forget -- never blocks the kill path.
pub fn record_termination(
    session_key: &str,
    dispatch_id: Option<&str>,
    killer_component: &str,
    reason_code: &str,
    reason_text: Option<&str>,
    probe_snapshot: Option<&str>,
    last_offset: Option<u64>,
    tmux_alive: Option<bool>,
) {
    let Some(db) = AUDIT_DB.get() else { return };
    let conn = match db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("  [termination_audit] failed to open separate conn: {e}");
            return;
        }
    };
    if let Err(e) = conn.execute(
        "INSERT INTO session_termination_events \
         (session_key, dispatch_id, killer_component, reason_code, reason_text, probe_snapshot, last_offset, tmux_alive) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            session_key,
            dispatch_id,
            killer_component,
            reason_code,
            reason_text,
            probe_snapshot,
            last_offset.map(|v| v as i64),
            tmux_alive.map(|v| v as i32),
        ],
    ) {
        tracing::warn!("  [termination_audit] insert failed: {e}");
    }
}

/// Convenience: derive session_key from tmux name, then record.
pub fn record_termination_for_tmux(
    tmux_session_name: &str,
    dispatch_id: Option<&str>,
    killer_component: &str,
    reason_code: &str,
    reason_text: Option<&str>,
    last_offset: Option<u64>,
) {
    let hostname = crate::services::platform::hostname_short();
    let session_key = format!("{}:{}", hostname, tmux_session_name);
    let tmux_alive =
        crate::services::tmux_diagnostics::tmux_session_has_live_pane(tmux_session_name);
    let probe_snapshot = if tmux_alive {
        crate::services::platform::tmux::capture_pane(tmux_session_name, -30)
    } else {
        None
    };
    record_termination(
        &session_key,
        dispatch_id,
        killer_component,
        reason_code,
        reason_text,
        probe_snapshot.as_deref(),
        last_offset,
        Some(tmux_alive),
    );
}
