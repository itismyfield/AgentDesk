//! Routine script registry initialization and boot-time registration audit (#5727).

use sqlx::PgPool;
use std::path::PathBuf;
use std::sync::Arc;

use crate::services::routines::RoutineScriptLoader;

pub(super) fn load_registry(
    script_dirs: &[PathBuf],
) -> anyhow::Result<(Arc<RoutineScriptLoader>, usize)> {
    let loader = Arc::new(RoutineScriptLoader::new_shared(script_dirs)?);
    let count = loader.load_dirs(script_dirs)?;
    Ok((loader, count))
}

/// Boot-time (once per process) WARN listing `*.js` files present under the
/// configured routine script directories that have no `routines` row. Hot
/// reloads do not repeat it; `agentdesk doctor` carries the same finding as
/// the `routine_scripts_registered` check.
pub(super) async fn warn_once_unregistered(pg_pool: &PgPool, script_dirs: &[PathBuf]) {
    use crate::services::routines::{
        discover_routine_script_refs, registered_routine_script_refs,
        unregistered_routine_script_refs,
    };

    let registered = match registered_routine_script_refs(pg_pool).await {
        Ok(registered) => registered,
        Err(error) => {
            tracing::debug!(error = %error, "unregistered routine script check skipped");
            return;
        }
    };
    let dirs = script_dirs.to_vec();
    let discovered =
        match tokio::task::spawn_blocking(move || discover_routine_script_refs(&dirs)).await {
            Ok(discovered) => discovered,
            Err(error) => {
                tracing::debug!(error = %error, "unregistered routine script scan failed");
                return;
            }
        };
    let unregistered = unregistered_routine_script_refs(&discovered, &registered);
    if unregistered.is_empty() {
        return;
    }
    tracing::warn!(
        count = unregistered.len(),
        scripts = %unregistered.join(", "),
        dirs = ?script_dirs,
        "routine scripts present in routines.dir but not registered in the routines table; attach them via POST /api/routines or remove the files"
    );
}
