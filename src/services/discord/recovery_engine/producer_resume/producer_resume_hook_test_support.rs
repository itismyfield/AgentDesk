use std::collections::HashMap;
use std::sync::{
    LazyLock, Mutex, Weak,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

pub(in crate::services::discord) type EpilogueObservers = HashMap<(u64, String), Weak<AtomicU64>>;
pub(in crate::services::discord) static EPILOGUES: LazyLock<Mutex<EpilogueObservers>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(in crate::services::discord) fn record_epilogue_body(channel: u64, nonce: Option<&str>) {
    if let Some(observer) = EPILOGUES
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&(channel, nonce.unwrap_or_default().to_owned()))
        .and_then(Weak::upgrade)
    {
        observer.fetch_add(1, Ordering::AcqRel);
    }
}

pub(in crate::services::discord) type PreflightRefusal =
    Option<(String, std::sync::Weak<tokio::sync::Notify>)>;
pub(in crate::services::discord) static PREFLIGHT_REFUSED: std::sync::Mutex<PreflightRefusal> =
    std::sync::Mutex::new(None);

pub(super) fn preflight_refused(tmux: &str) {
    if let Some((key, notification)) = PREFLIGHT_REFUSED.lock().unwrap().as_ref()
        && key == tmux
        && let Some(notification) = notification.upgrade()
    {
        notification.notify_one();
    }
}

thread_local! {
    pub(in crate::services::discord) static ENSURE_HOOK: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}
pub(super) async fn before_ensure() {
    match ENSURE_HOOK.with(|hook| hook.replace(0)) {
        1 => panic!("injected producer startup panic"),
        2 => tokio::time::sleep(Duration::from_secs(5)).await,
        _ => {}
    }
}
