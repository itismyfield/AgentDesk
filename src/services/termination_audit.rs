use std::sync::{Mutex, OnceLock};

#[derive(Clone, Default)]
struct AuditRuntime {
    pg_pool: Option<sqlx::PgPool>,
}

#[derive(Clone)]
struct TerminationAuditRecord {
    session_key: String,
    dispatch_id: Option<String>,
    killer_component: String,
    reason_code: String,
    reason_text: Option<String>,
    probe_snapshot: Option<String>,
    last_offset: Option<i64>,
    tmux_alive: Option<bool>,
}

static AUDIT_RUNTIME: OnceLock<Mutex<AuditRuntime>> = OnceLock::new();

fn audit_runtime_slot() -> &'static Mutex<AuditRuntime> {
    AUDIT_RUNTIME.get_or_init(|| Mutex::new(AuditRuntime::default()))
}

#[allow(clippy::too_many_arguments)]
fn build_record(
    session_key: &str,
    dispatch_id: Option<&str>,
    killer_component: &str,
    reason_code: &str,
    reason_text: Option<&str>,
    probe_snapshot: Option<&str>,
    last_offset: Option<u64>,
    tmux_alive: Option<bool>,
) -> TerminationAuditRecord {
    TerminationAuditRecord {
        session_key: session_key.to_string(),
        dispatch_id: dispatch_id.map(str::to_string),
        killer_component: killer_component.to_string(),
        reason_code: reason_code.to_string(),
        reason_text: reason_text.map(str::to_string),
        probe_snapshot: probe_snapshot.map(str::to_string),
        last_offset: last_offset.map(|value| value as i64),
        tmux_alive,
    }
}

/// Initialize audit persistence. Call during startup and after PG is available.
pub fn init_audit_db(pg_pool: Option<sqlx::PgPool>) {
    let Ok(mut runtime) = audit_runtime_slot().lock() else {
        return;
    };
    if let Some(pool) = pg_pool {
        runtime.pg_pool = Some(pool);
    }
}

/// Record a session termination event. Fire-and-forget -- never blocks the kill path.
#[allow(clippy::too_many_arguments)]
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
    let Ok(runtime) = audit_runtime_slot().lock() else {
        return;
    };
    let record = build_record(
        session_key,
        dispatch_id,
        killer_component,
        reason_code,
        reason_text,
        probe_snapshot,
        last_offset,
        tmux_alive,
    );
    persist_record(runtime.pg_pool.clone(), record);
}

/// Record against explicit handles. PostgreSQL is authoritative for #868.
#[allow(clippy::too_many_arguments)]
pub fn record_termination_with_handles(
    pg_pool: Option<&sqlx::PgPool>,
    session_key: &str,
    dispatch_id: Option<&str>,
    killer_component: &str,
    reason_code: &str,
    reason_text: Option<&str>,
    probe_snapshot: Option<&str>,
    last_offset: Option<u64>,
    tmux_alive: Option<bool>,
) {
    let record = build_record(
        session_key,
        dispatch_id,
        killer_component,
        reason_code,
        reason_text,
        probe_snapshot,
        last_offset,
        tmux_alive,
    );
    persist_record(pg_pool.cloned(), record);
}

fn persist_record(pg_pool: Option<sqlx::PgPool>, record: TerminationAuditRecord) {
    let Some(pool) = pg_pool else {
        tracing::debug!("  [termination_audit] skipped insert: postgres backend is unavailable");
        return;
    };

    let record_for_task = record.clone();
    let write_task = async move {
        if let Err(error) = insert_record_pg(&pool, &record_for_task).await {
            tracing::warn!("  [termination_audit] postgres insert failed: {error}");
        }
    };

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(write_task);
        return;
    }

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        match runtime {
            Ok(runtime) => runtime.block_on(write_task),
            Err(error) => {
                tracing::warn!("  [termination_audit] runtime bootstrap failed: {error}");
            }
        }
    });
}

async fn insert_record_pg(
    pool: &sqlx::PgPool,
    record: &TerminationAuditRecord,
) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO session_termination_events
         (session_key, dispatch_id, killer_component, reason_code, reason_text, probe_snapshot, last_offset, tmux_alive)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(&record.session_key)
    .bind(&record.dispatch_id)
    .bind(&record.killer_component)
    .bind(&record.reason_code)
    .bind(&record.reason_text)
    .bind(&record.probe_snapshot)
    .bind(record.last_offset)
    .bind(record.tmux_alive.map(i32::from))
    .execute(pool)
    .await
    .map_err(|error| format!("{error}"))?;
    Ok(())
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
