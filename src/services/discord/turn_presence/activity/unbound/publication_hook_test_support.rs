type PublicationCallback = Box<dyn FnOnce() + Send>;
struct PublicationHook {
    channel: u64,
    nth: usize,
    seen: usize,
    callback: PublicationCallback,
}
static PUBLICATION_HOOKS: std::sync::Mutex<Vec<PublicationHook>> =
    std::sync::Mutex::new(Vec::new());

pub(in crate::services::discord) fn before_publication(channel: u64) {
    let callback = {
        let mut hooks = PUBLICATION_HOOKS.lock().unwrap();
        hooks
            .iter_mut()
            .find(|hook| hook.channel == channel)
            .and_then(|hook| {
                hook.seen += 1;
                (hook.seen == hook.nth).then_some(())
            })
            .map(|()| {
                let at = hooks
                    .iter()
                    .position(|hook| hook.channel == channel)
                    .unwrap();
                hooks.remove(at).callback
            })
    };
    if let Some(callback) = callback {
        callback();
    }
}

pub(in crate::services::discord) fn install_publication_hook(
    channel: u64,
    nth: usize,
    callback: PublicationCallback,
) {
    let mut hooks = PUBLICATION_HOOKS.lock().unwrap();
    assert!(!hooks.iter().any(|hook| hook.channel == channel));
    hooks.push(PublicationHook {
        channel,
        nth,
        seen: 0,
        callback,
    });
}

pub(in crate::services::discord) fn is_installed(channel: u64) -> bool {
    PUBLICATION_HOOKS
        .lock()
        .unwrap()
        .iter()
        .any(|hook| hook.channel == channel)
}
