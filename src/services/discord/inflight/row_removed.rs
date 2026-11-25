use super::*;
#[cfg(unix)]
use std::sync::{
    Arc, Mutex, OnceLock, Weak,
    atomic::{AtomicBool, Ordering},
};

#[cfg(unix)]
struct Observed {
    path: std::path::PathBuf,
    pin: InflightEpisodePin,
    key: serde_json::Value,
    removed: AtomicBool,
    ready: tokio::sync::Notify,
}
#[cfg(unix)]
fn watches() -> &'static Mutex<Vec<Weak<Observed>>> {
    static WATCHES: OnceLock<Mutex<Vec<Weak<Observed>>>> = OnceLock::new();
    WATCHES.get_or_init(Default::default)
}
#[cfg(unix)]
pub(in crate::services::discord) struct Watch(Arc<Observed>);
#[cfg(unix)]
impl Watch {
    pub(in crate::services::discord) fn key(&self) -> serde_json::Value {
        self.0.key.clone()
    }
    pub(in crate::services::discord) async fn removed(self) {
        let ready = self.0.ready.notified();
        tokio::pin!(ready);
        ready.as_mut().enable();
        if !self.0.removed.load(Ordering::Acquire) {
            ready.await;
        }
    }
}
// Registration and capture share the existing row lock, so a deletion cannot slip between them.
#[cfg(unix)]
pub(in crate::services::discord) fn watch(
    provider: &ProviderKind,
    channel: u64,
) -> Result<Option<Watch>, String> {
    let root = inflight_runtime_root().ok_or("inflight root unavailable")?;
    let path = inflight_state_path(&root, provider, channel);
    let _lock = lock_inflight_state_path(&path)?;
    let bytes = match fs::read_to_string(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    let row = parse_inflight_state_content(&bytes).map_err(|e| e.to_string())?;
    let observed = Arc::new(Observed {
        path,
        pin: InflightEpisodePin::from_state(&row),
        key: serde_json::json!({"provider":row.provider,"channel":row.channel_id,"owner":row.request_owner_user_id,"anchor":row.user_msg_id,"nonce":row.turn_nonce,"generation":row.born_generation,"started_at":row.started_at}),
        removed: AtomicBool::new(false),
        ready: tokio::sync::Notify::new(),
    });
    let mut watches = watches().lock().unwrap_or_else(|e| e.into_inner());
    watches.retain(|w| w.strong_count() > 0);
    watches.push(Arc::downgrade(&observed));
    Ok(Some(Watch(observed)))
}
// Callers retain their existing guards; an unreadable identity never authorizes a closing wake.
#[cfg(unix)]
pub(super) fn capture(path: &Path) -> Option<InflightEpisodePin> {
    let bytes = fs::read_to_string(path).ok()?;
    let row = parse_inflight_state_content(&bytes).ok()?;
    Some(InflightEpisodePin::from_state(&row))
}
#[cfg(unix)]
pub(super) fn removed(path: &Path, pin: Option<InflightEpisodePin>) {
    let Some(pin) = pin else { return };
    let mut watches = watches().lock().unwrap_or_else(|e| e.into_inner());
    watches.retain(|weak| {
        let Some(watch) = weak.upgrade() else {
            return false;
        };
        if watch.path == path
            && watch.pin.is_same_episode_as(&pin)
            && !watch.removed.swap(true, Ordering::AcqRel)
        {
            watch.ready.notify_one();
        }
        true
    });
}
/// Every inflight row delete; an AGY custody row is its owner's to remove, so it is kept here.
/// The AGY owner's permit on this thread: one row path and the exact route it expects there.
pub(super) struct OwnerPermit {
    pub(super) path: std::path::PathBuf,
    pub(super) route: super::AgyCustodyRoute,
}

std::thread_local! {
    static OWNER: std::cell::RefCell<Option<OwnerPermit>> = const { std::cell::RefCell::new(None) };
}

/// Runs `clear` holding `permit` (none when `None`); the permit ends with `clear`.
pub(super) fn with_owner<T>(permit: Option<OwnerPermit>, clear: impl FnOnce() -> T) -> T {
    let earlier = OWNER.with(|owner| owner.replace(permit));
    let result = clear();
    OWNER.with(|owner| owner.replace(earlier));
    result
}

/// Whether the permit names this locked row: its path when given, and its whole stored route,
/// writer generation included, on a row the owner still ends.
pub(super) fn owner_permits(path: Option<&Path>, row: &InflightTurnState) -> bool {
    OWNER.with(|owner| {
        owner.borrow().as_ref().is_some_and(|permit| {
            path.is_none_or(|path| path == permit.path)
                && super::end_authority(row) == super::EndAuthority::Agy
                && matches!(&row.agy_custody, Some(super::AgyCustodyField::Route(route))
                    if *route == permit.route)
        })
    })
}

pub(super) fn unlink(
    path: &Path,
    remove: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    let bytes = fs::read(path).unwrap_or_default();
    let content = String::from_utf8_lossy(&bytes);
    if super::agy_end_authority::generic_end_keeps(&content)
        && !serde_json::from_str::<InflightTurnState>(&content)
            .is_ok_and(|row| owner_permits(Some(path), &row))
    {
        return Err(std::io::Error::other("AGY custody row kept for its owner"));
    }
    #[cfg(unix)]
    let pin = capture(path);
    #[cfg(not(unix))]
    let _ = path;
    remove()?;
    #[cfg(unix)]
    {
        sync_removed(path);
        removed(path, pin);
    }
    Ok(())
}

#[cfg(all(test, unix))]
#[path = "row_removed_tests.rs"]
mod tests;
