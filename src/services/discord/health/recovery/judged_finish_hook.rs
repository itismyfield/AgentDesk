//! A test seam between a stop's judgement of its channel and the finish that acts on it.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;

use poise::serenity_prelude::ChannelId;

type Hook = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()>>>>;

thread_local! {
    static HOOKS: RefCell<Vec<(ChannelId, Hook)>> = const { RefCell::new(Vec::new()) };
}

/// Runs `hook` once, the next time a stop on `channel` has judged it.
pub(crate) fn set<F>(channel: ChannelId, hook: impl FnOnce() -> F + 'static)
where
    F: Future<Output = ()> + 'static,
{
    let hook: Hook = Box::new(move || Box::pin(hook()));
    HOOKS.with_borrow_mut(|hooks| hooks.push((channel, hook)));
}

pub(super) async fn after_judge(channel: ChannelId) {
    let hook = HOOKS.with_borrow_mut(|hooks| {
        let at = hooks.iter().position(|(held, _)| *held == channel)?;
        Some(hooks.remove(at).1)
    });
    if let Some(hook) = hook {
        hook().await;
    }
}
