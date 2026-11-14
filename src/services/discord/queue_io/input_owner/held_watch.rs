//! A front held by its pane's screen or by a cause a read can see (a lock, a buffer load, the
//! transcript) is watched only through that read: the first that clears wakes the drive once.
#![cfg(unix)]
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::Duration;

use super::drive::{Drive, wake};
use crate::services::claude_tui::busy_inject::Screen;

/// One read of the refusing pane or of the cause that refused it.
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
    #[cfg(test)]
    completion: Option<tokio::sync::oneshot::Sender<Observation>>,
}

/// What one look decides for the hold it read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Verdict {
    Wake,
    Keep,
    End,
}

#[cfg(test)]
pub(crate) struct Observation {
    pub(crate) generation: u64,
    pub(crate) verdict: Verdict,
}

#[cfg(test)]
pub(crate) fn watch_observed(
    drive: Arc<Drive>,
    message: u64,
) -> (u64, tokio::sync::oneshot::Receiver<Observation>) {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let generation = {
        let mut all = watches();
        let watch = all.entry(drive.channel).or_default();
        watch.completion = Some(sender);
        watch.generation + 1
    };
    watch(drive, message);
    (generation, receiver)
}

/// A look's verdict from the pane read (`None` when the probe failed to run) and whether its front
/// still leads; a failed read of the pane or of the queue keeps the watch for the next look.
fn verdict(read: Option<Screen>, leads: Result<bool, String>) -> Verdict {
    match (read, leads) {
        (None, _) | (_, Ok(false)) | (Some(Screen::Gone), _) => Verdict::End,
        (Some(Screen::Ready), Ok(true)) => Verdict::Wake,
        _ => Verdict::Keep,
    }
}

fn watches() -> MutexGuard<'static, HashMap<u64, Watch>> {
    static WATCHES: LazyLock<Mutex<HashMap<u64, Watch>>> = LazyLock::new(Default::default);
    WATCHES.lock().unwrap_or_else(|error| error.into_inner())
}

/// Whether a refusal left `channel` a probe no drive has taken yet.
#[cfg(test)]
pub(crate) fn armed(channel: u64) -> bool {
    watches().get(&channel).is_some_and(|w| w.armed.is_some())
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
            let (read, probe) = read.map_or((None, None), |(read, p)| (Some(read), Some(p)));
            let look = verdict(read, leads(&drive, message).await);
            let mut all = watches();
            let Some(watch) = all.get_mut(&channel) else {
                break;
            };
            if watch.generation != generation {
                continue;
            }
            #[cfg(test)]
            if let Some(completion) = watch.completion.take() {
                let _ = completion.send(Observation {
                    generation,
                    verdict: look,
                });
            }
            match look {
                Verdict::Keep => watch.probe = probe,
                Verdict::End | Verdict::Wake => {
                    watch.running = false;
                    if watch.armed.is_none() {
                        all.remove(&channel);
                    }
                    drop(all);
                    if look == Verdict::Wake {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A look wakes only on a ready screen whose front still leads; a failed pane or queue read keeps
    /// the watch, and only a gone pane, a probe that did not run or a moved front ends it.
    #[test]
    fn a_look_ends_only_on_a_gone_pane_or_a_moved_front() {
        let failed = || Err("mailbox unreachable".to_string());
        let cases = [
            (Some(Screen::Ready), Ok(true), Verdict::Wake),
            (Some(Screen::Ready), failed(), Verdict::Keep),
            (Some(Screen::Unknown), Ok(true), Verdict::Keep),
            (Some(Screen::Unknown), failed(), Verdict::Keep),
            (Some(Screen::Blocked), Ok(true), Verdict::Keep),
            (Some(Screen::Blocked), failed(), Verdict::Keep),
            (Some(Screen::Ready), Ok(false), Verdict::End),
            (Some(Screen::Gone), Ok(true), Verdict::End),
            (Some(Screen::Gone), failed(), Verdict::End),
            (None, Ok(true), Verdict::End),
        ];
        for (read, leads, expected) in cases {
            assert_eq!(verdict(read, leads.clone()), expected, "{read:?} {leads:?}");
        }
    }
}
