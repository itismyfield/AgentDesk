//! Watches for inputs held by a person's draft, keyed by channel and pane, with the last wake
//! hint each watch observed. The retry policy reads them; the draft-release watch updates them.

static DRAFT_RELEASE_WATCHES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<(u64, String)>>,
> = std::sync::LazyLock::new(Default::default);

pub(in crate::services::discord) static OBSERVED_HINTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<(u64, String), String>>,
> = std::sync::LazyLock::new(Default::default);

/// An unseen idle observation holds the queue until the existing watch releases its source.
#[cfg(unix)]
pub(in crate::services::discord) fn idle_release_pending(
    channel: u64,
    session: &str,
) -> Option<bool> {
    let key = (channel, session.to_string());
    if !draft_release_watches().contains(&key) {
        return None;
    }
    let observed = crate::services::claude_tui::input::submission::wake_hint_observed(session);
    let watches = draft_release_watches();
    if !watches.contains(&key) {
        return None;
    }
    let hint = match observed {
        Err(()) => return Some(true),
        Ok(None) => return None,
        Ok(Some(hint)) => hint,
    };
    Some(
        OBSERVED_HINTS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
            != Some(&hint),
    )
}

pub(in crate::services::discord) fn draft_release_watches()
-> std::sync::MutexGuard<'static, std::collections::HashSet<(u64, String)>> {
    DRAFT_RELEASE_WATCHES
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}
