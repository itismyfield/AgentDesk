//! A front held on a screen where Enter may not submit is watched only through that pane's state:
//! the first read that would submit wakes the drive once, and an unchanged screen hands nothing.
#![cfg(unix)]
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::Duration;

use super::drive::{Drive, wake};
use crate::services::claude_tui::busy_inject::Screen;

/// One read of the refusing pane.
pub(crate) type ScreenProbe = Box<dyn FnMut() -> Screen + Send>;

/// A held pane is read this often; each read is a few bounded tmux queries and no handoff.
const LOOK: Duration = Duration::from_secs(2);

#[derive(Default)]
struct Watch {
    /// The probe the last refusal left, until the drive names the front it held.
    armed: Option<ScreenProbe>,
    /// The watched hold: its generation, front message and probe (out while a read runs).
    generation: u64,
    message: u64,
    probe: Option<ScreenProbe>,
    running: bool,
}

/// What one look decided for the hold it read.
enum Look {
    Wake,
    Keep(ScreenProbe),
    End,
}

fn watches() -> MutexGuard<'static, HashMap<u64, Watch>> {
    static WATCHES: LazyLock<Mutex<HashMap<u64, Watch>>> = LazyLock::new(Default::default);
    WATCHES.lock().unwrap_or_else(|error| error.into_inner())
}

/// Leaves the probe of the pane that just refused `channel`'s front, for the drive to watch.
pub(crate) fn arm(channel: u64, probe: ScreenProbe) {
    watches().entry(channel).or_default().armed = Some(probe);
}

/// Watches the pane that held `message` until its screen would submit, then wakes the drive once.
/// One task runs per channel; a newer hold replaces the one it watches, never ending with it.
pub(crate) fn watch(drive: Arc<Drive>, message: u64) {
    {
        let mut all = watches();
        let Some(watch) = all.get_mut(&drive.channel) else {
            return;
        };
        let Some(probe) = watch.armed.take() else {
            return;
        };
        watch.generation += 1;
        (watch.message, watch.probe) = (message, Some(probe));
        if std::mem::replace(&mut watch.running, true) {
            return;
        }
    }
    tokio::spawn(async move {
        let channel = drive.channel;
        loop {
            tokio::time::sleep(LOOK).await;
            let taken = watches()
                .get_mut(&channel)
                .map(|w| (w.generation, w.message, w.probe.take()));
            let Some((generation, message, Some(mut probe))) = taken else {
                break;
            };
            let read = tokio::task::spawn_blocking(move || (probe(), probe)).await;
            let look = match (read, leads(&drive, message).await) {
                (Err(_), _) | (_, Ok(false)) | (Ok((Screen::Gone, _)), _) => Look::End,
                (Ok((Screen::Ready, _)), Ok(true)) => Look::Wake,
                // A failed read of the pane or of the queue keeps the watch for the next look.
                (Ok((_, probe)), _) => Look::Keep(probe),
            };
            let mut all = watches();
            let Some(watch) = all.get_mut(&channel) else {
                break;
            };
            if watch.generation != generation {
                continue;
            }
            match look {
                Look::Keep(probe) => watch.probe = Some(probe),
                Look::End | Look::Wake => {
                    watch.running = false;
                    if watch.armed.is_none() {
                        all.remove(&channel);
                    }
                    drop(all);
                    if matches!(look, Look::Wake) {
                        wake(drive);
                    }
                    return;
                }
            }
        }
        if let Some(watch) = watches().get_mut(&channel) {
            watch.running = false;
        }
    });
}

/// Whether `message` is still the owner's unattempted front; `Err` when the queue was not read.
async fn leads(drive: &Drive, message: u64) -> Result<bool, String> {
    let front = drive.mailbox.v2_request(|slot| {
        let front = slot.as_ref().and_then(|owner| owner.front());
        Ok(front.map(|front| front.1.message_id))
    });
    Ok(front.await? == Some(message))
}
