use super::*;
use std::sync::{
    Arc, Mutex, OnceLock, Weak,
    atomic::{AtomicBool, Ordering},
};

struct Observed {
    path: std::path::PathBuf,
    pin: InflightEpisodePin,
    key: serde_json::Value,
    removed: AtomicBool,
    ready: tokio::sync::Notify,
}
fn watches() -> &'static Mutex<Vec<Weak<Observed>>> {
    static WATCHES: OnceLock<Mutex<Vec<Weak<Observed>>>> = OnceLock::new();
    WATCHES.get_or_init(Default::default)
}
pub(in crate::services::discord) struct Watch(Arc<Observed>);
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
pub(super) fn capture(path: &Path) -> Option<InflightEpisodePin> {
    let bytes = fs::read_to_string(path).ok()?;
    let row = parse_inflight_state_content(&bytes).ok()?;
    Some(InflightEpisodePin::from_state(&row))
}
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
pub(super) fn unlink(path: &Path) -> std::io::Result<()> {
    let pin = capture(path);
    fs::remove_file(path)?;
    sync_removed(path);
    removed(path, pin);
    Ok(())
}
