//! The single boot owner of loader-verdict row retirement. Event payloads keep
//! the loader's site strings so existing analysis queries still match.

use super::*;

/// Counts from one reaper pass; `already_ran` marks a caller that waited on
/// another bot's pass for the same provider.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BootReapReport {
    pub(super) already_ran: bool,
    pub(super) kept: usize,
    pub(super) refused: usize,
    pub(super) changed: usize,
    pub(super) missing: usize,
    pub(super) incomplete: usize,
    pub(super) reaped_stale: usize,
    pub(super) reaped_provider_mismatch: usize,
    pub(super) reaped_malformed: usize,
    pub(super) max_reaped_age_secs: Option<u64>,
}

/// Per-provider completion gate: the first caller starts the reaper and every
/// later caller waits for that run, so none reaches a mint surface before it.
/// The pass is spawned outside any caller, so a cancelled caller cannot restart it.
#[derive(Default)]
pub(super) struct BootReapOnce {
    passes: std::sync::Mutex<HashMap<String, tokio::sync::watch::Receiver<Option<BootReapReport>>>>,
}

impl BootReapOnce {
    pub(super) async fn run_once(
        &self,
        provider: &ProviderKind,
        reap: impl FnOnce() -> BootReapReport + Send + 'static,
    ) -> BootReapReport {
        let (mut pass, already_ran) = {
            let mut passes = self.passes.lock().unwrap_or_else(|p| p.into_inner());
            match passes.entry(provider.as_str().to_string()) {
                std::collections::hash_map::Entry::Occupied(pass) => (pass.get().clone(), true),
                std::collections::hash_map::Entry::Vacant(slot) => {
                    let (done, pass) = tokio::sync::watch::channel(None);
                    tokio::task::spawn_blocking(move || done.send_replace(Some(reap())));
                    (slot.insert(pass).clone(), false)
                }
            }
        };
        let mut report = match pass.wait_for(Option::is_some).await {
            Ok(report) => report.clone().unwrap_or_default(),
            Err(_) => {
                tracing::warn!("inflight boot reaper failed; rows left in place");
                BootReapReport::default()
            }
        };
        report.already_ran = already_ran;
        report
    }
}

pub(crate) async fn reap_inflight_rows_at_boot_blocking(
    provider: &ProviderKind,
    pg_pool: Option<sqlx::PgPool>,
) -> BootReapReport {
    static ONCE: std::sync::OnceLock<BootReapOnce> = std::sync::OnceLock::new();
    let guard = ONCE.get_or_init(BootReapOnce::default);
    reap_inflight_rows_at_boot_with_guard(guard, provider, pg_pool).await
}

/// The first caller of a provider's pass also starts its custody notice pass.
pub(super) async fn reap_inflight_rows_at_boot_with_guard(
    guard: &BootReapOnce,
    provider: &ProviderKind,
    pg_pool: Option<sqlx::PgPool>,
) -> BootReapReport {
    let owned = provider.clone();
    let reap = move || {
        inflight_runtime_root()
            .inspect(|root| super::boot_custody::preserve_before_boot_reap(root, &owned))
            .map(|root| reap_inflight_rows_at_boot_in_root(&root, &owned))
            .unwrap_or_default()
    };
    let report = guard.run_once(provider, reap).await;
    if !report.already_ran {
        super::custody_notice::spawn_boot_custody_notice(provider, pg_pool);
    }
    let provider = provider.as_str();
    tracing::info!(
        provider,
        already_ran = report.already_ran,
        "inflight boot reaper settled"
    );
    report
}

/// The episode, turn and write generation first read without the lock are
/// still the row now held under it.
fn same_snapshot(unlocked: &InflightTurnState, locked: &InflightTurnState) -> bool {
    InflightEpisodePin::from_state(unlocked).matches_state(locked)
        && InflightTurnIdentity::from_state(unlocked).matches_state(locked)
        && locked.save_generation == unlocked.save_generation
        && locked.updated_at == unlocked.updated_at
}

pub(super) fn reap_inflight_rows_at_boot_in_root(
    root: &Path,
    provider: &ProviderKind,
) -> BootReapReport {
    let mut report = BootReapReport::default();
    let Ok(entries) = fs::read_dir(inflight_provider_dir(root, provider)) else {
        return report;
    };
    let allocation = crate::services::discord::runtime_store::process_generation_binding();
    for path in entries.flatten().map(|entry| entry.path()) {
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let age_secs = inflight_age_secs_for_path(&path);
        let (label, unchanged, locked, _lock, stale) =
            match classify_inflight_row(&path, provider, allocation) {
                RowVerdict::Keep(..) => {
                    report.kept += 1;
                    continue;
                }
                RowVerdict::Skip(incomplete) => {
                    *if incomplete {
                        &mut report.incomplete
                    } else {
                        &mut report.missing
                    } += 1;
                    continue;
                }
                RowVerdict::KeepGateRefused(state, _lock) => {
                    record_loader_generation_gate(&state, allocation, &path);
                    report.refused += 1;
                    continue;
                }
                RowVerdict::HideStale(reason, unlocked, locked, lock) => {
                    let unchanged = same_snapshot(&unlocked, &locked);
                    (
                        "load_inflight_states_from_root_stale",
                        unchanged,
                        Some(locked),
                        lock,
                        Some(reason),
                    )
                }
                RowVerdict::HideForeign(label, unlocked, locked, lock) => (
                    label,
                    same_snapshot(&unlocked, &locked),
                    Some(locked),
                    lock,
                    None,
                ),
                RowVerdict::HideMalformed(unlocked, locked, lock) => (
                    "load_inflight_states_from_root_malformed",
                    unlocked == locked,
                    None,
                    lock,
                    None,
                ),
            };
        // The verdict's lock is still held: revalidate, then retire under it.
        if !unchanged {
            report.changed += 1;
            continue;
        }
        if let (Some(reason), Some(locked)) = (&stale, &locked) {
            emit_loader_generation_gate_allowed(provider, locked, allocation, &path);
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ⚠ {reason}: {}", path.display());
        }
        let channel_id = locked
            .as_ref()
            .map_or(channel_id_from_path(&path), |row| row.channel_id);
        let user_msg_id = locked.as_ref().map_or(0, |row| row.user_msg_id);
        let generation = allocation.generation;
        log_loader_inflight_remove(
            provider,
            channel_id,
            user_msg_id,
            label,
            &path,
            locked.as_ref(),
            generation,
        );
        if fs::remove_file(&path).is_ok() {
            *match (&stale, &locked) {
                (Some(_), _) => &mut report.reaped_stale,
                (None, Some(_)) => &mut report.reaped_provider_mismatch,
                (None, None) => &mut report.reaped_malformed,
            } += 1;
            report.max_reaped_age_secs = report.max_reaped_age_secs.max(age_secs);
        }
    }
    tracing::info!(
        provider = %provider.as_str(),
        kept = report.kept,
        refused = report.refused,
        changed = report.changed,
        missing = report.missing,
        incomplete = report.incomplete,
        reaped_stale = report.reaped_stale,
        reaped_provider_mismatch = report.reaped_provider_mismatch,
        reaped_malformed = report.reaped_malformed,
        max_reaped_age_secs = ?report.max_reaped_age_secs,
        "inflight boot reaper"
    );
    report
}
