use super::*;
use std::cell::RefCell;

struct SelectionHook {
    channel: ChannelId,
    entered: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}
thread_local! {
    static BEFORE_TAKE: RefCell<Option<SelectionHook>> = const { RefCell::new(None) };
}
pub(in crate::services::discord) fn pause_before_take(
    channel: ChannelId,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (entered, observed) = tokio::sync::oneshot::channel();
    let (resume, release) = tokio::sync::oneshot::channel();
    BEFORE_TAKE.with(|slot| {
        *slot.borrow_mut() = Some(SelectionHook {
            channel,
            entered,
            release,
        })
    });
    (observed, resume)
}
pub(super) async fn before_take(channel: ChannelId) {
    let hook = BEFORE_TAKE.with(|slot| {
        if slot
            .borrow()
            .as_ref()
            .is_some_and(|hook| hook.channel == channel)
        {
            slot.borrow_mut().take()
        } else {
            None
        }
    });
    if let Some(hook) = hook {
        hook.entered.send(()).unwrap();
        hook.release.await.unwrap();
    }
}
