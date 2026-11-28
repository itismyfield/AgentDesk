use std::sync::Mutex;

use sqlx::PgPool;

type CachedWake = (Option<String>, &'static str, String, &'static str);
static CACHED_WAKES: Mutex<Vec<CachedWake>> = Mutex::new(Vec::new());

pub(super) fn record_cached_wake(
    pool: &PgPool,
    trigger: &'static str,
    dispatch_id: &str,
    source: &'static str,
) {
    CACHED_WAKES.lock().expect("wake observations").push((
        pool.connect_options().get_database().map(str::to_owned),
        trigger,
        dispatch_id.to_string(),
        source,
    ));
}

pub(crate) fn cached_wakes(dispatch_id: &str) -> Vec<CachedWake> {
    CACHED_WAKES
        .lock()
        .expect("wake observations")
        .iter()
        .filter(|wake| wake.2 == dispatch_id)
        .cloned()
        .collect()
}
