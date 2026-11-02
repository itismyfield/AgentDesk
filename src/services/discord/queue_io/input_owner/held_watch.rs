//! A front held on a screen where Enter may not submit is watched only through that pane's state:
//! the first read that would submit wakes the drive once, and an unchanged screen hands nothing.
#![cfg(unix)]
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::Duration;

use super::drive::{Drive, wake};

/// One read of the refusing pane: `Some(true)` once Enter would submit, `None` once it is gone.
pub(crate) type ScreenProbe = Box<dyn FnMut() -> Option<bool> + Send>;

/// A held pane is read this often; each read is a few bounded tmux queries and no handoff.
const LOOK: Duration = Duration::from_secs(2);

#[derive(Default)]
struct Watch {
    probe: Option<ScreenProbe>,
    /// The held front's message; the watch ends once another one leads the queue.
    message: u64,
    running: bool,
}

fn watches() -> MutexGuard<'static, HashMap<u64, Watch>> {
    static WATCHES: LazyLock<Mutex<HashMap<u64, Watch>>> = LazyLock::new(Default::default);
    WATCHES.lock().unwrap_or_else(|error| error.into_inner())
}

/// Leaves the probe of the pane that just refused `channel`'s front, for the drive to watch.
pub(crate) fn arm(channel: u64, probe: ScreenProbe) {
    watches().entry(channel).or_default().probe = Some(probe);
}

/// Watches the pane that held `message` until its screen would submit, then wakes the drive once.
/// One watch runs per channel; it ends quietly when that front no longer leads or the pane is gone.
pub(crate) fn watch(drive: Arc<Drive>, message: u64) {
    {
        let mut all = watches();
        let Some(watch) = all.get_mut(&drive.channel).filter(|w| w.probe.is_some()) else {
            return;
        };
        watch.message = message;
        if std::mem::replace(&mut watch.running, true) {
            return;
        }
    }
    tokio::spawn(async move {
        let channel = drive.channel;
        let recovered = loop {
            tokio::time::sleep(LOOK).await;
            let (message, probe) = match watches().get_mut(&channel) {
                Some(watch) => (watch.message, watch.probe.take()),
                None => break false,
            };
            let (Some(mut probe), true) = (probe, leads(&drive, message).await) else {
                break false;
            };
            let read = tokio::task::spawn_blocking(move || (probe(), probe)).await;
            match read {
                Ok((Some(true), _)) => break true,
                Ok((Some(false), probe)) => {
                    if let Some(watch) = watches().get_mut(&channel) {
                        watch.probe.get_or_insert(probe);
                    }
                }
                _ => break false,
            }
        };
        watches().remove(&channel);
        if recovered {
            wake(drive);
        }
    });
}

/// Whether `message` is still the owner's unattempted front.
async fn leads(drive: &Drive, message: u64) -> bool {
    let front = drive.mailbox.v2_request(|slot| {
        let owner = slot.as_ref().ok_or("V2 not adopted")?;
        Ok(owner.front().map(|front| front.1.message_id))
    });
    matches!(front.await, Ok(Some(id)) if id == message)
}
