use super::*;

/// How often an input held by a person's draft looks for that protection to lift.
pub(super) const DRAFT_RELEASE_POLL: std::time::Duration = std::time::Duration::from_secs(5);
/// A stalled pane is watched for at most one day; unobserved holds stay persisted.
const DRAFT_RELEASE_WATCH: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

static DRAFT_RELEASE_WATCHES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<(u64, String)>>,
> = std::sync::LazyLock::new(Default::default);

static OBSERVED_HINTS: std::sync::LazyLock<
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

pub(super) fn draft_release_watches()
-> std::sync::MutexGuard<'static, std::collections::HashSet<(u64, String)>> {
    DRAFT_RELEASE_WATCHES
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

/// One watch per channel and pane: once the pane protects no draft, the held inputs get a fresh
/// budget and the queue one kickoff.
pub(super) fn watch_draft_release(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
    session: String,
) {
    let key = (channel_id.get(), session.clone());
    {
        let mut watches = draft_release_watches();
        if !watches.insert(key.clone()) {
            return;
        }
        OBSERVED_HINTS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&key);
    }
    tokio::spawn(async move {
        let binding_key = crate::services::claude_tui::input::PaneIdentity::capture;
        let mut binding = binding_key(&session);
        let started = tokio::time::Instant::now();
        let mut any_released = false;
        let mut last_hint = None;
        while started.elapsed() < DRAFT_RELEASE_WATCH {
            tokio::time::sleep(DRAFT_RELEASE_POLL).await;
            if binding.is_none() {
                binding = binding_key(&session);
            }
            if binding.is_none() || binding_key(&session) != binding {
                continue;
            }
            if shared
                .tmux_watchers
                .channel_binding(&channel_id)
                .is_some_and(|current| current.tmux_session_name != session)
            {
                continue;
            }
            let released = draft_lifted(&session).await;
            #[cfg(unix)]
            let hint = if !released
                && crate::services::claude_tui::busy_inject::stash_enabled(Some(channel_id.get()))
            {
                let session = session.clone();
                tokio::task::spawn_blocking(move || {
                    crate::services::claude_tui::input::submission::wake_hint(&session)
                })
                .await
                .unwrap_or(None)
            } else {
                None
            };
            #[cfg(not(unix))]
            let hint: Option<String> = None;
            if !released && (hint.is_none() || hint == last_hint) {
                continue;
            }
            let snapshot = super::super::super::mailbox_snapshot(&shared, channel_id).await;
            let live_ids = snapshot
                .intervention_queue
                .iter()
                .flat_map(|input| {
                    std::iter::once(input.message_id.get())
                        .chain(input.source_message_ids.iter().map(|id| id.get()))
                })
                .collect();
            if binding_key(&session) != binding {
                continue;
            }
            let pass = super::super::super::busy_followup_retry_store::release_draft_holds(
                &provider,
                channel_id.get(),
                &session,
                &live_ids,
            );
            any_released |= pass.released > 0;
            tracing::info!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                released = pass.released,
                retry = pass.retry,
                "claude_tui draft protection lifted; releasing held follow-ups"
            );
            // A failed step leaves held inputs behind, so the watch stays for the next poll.
            if pass.retry {
                continue;
            }
            if any_released {
                let reason = "claude_tui_draft_hold_released";
                let (shared, provider) = (shared.clone(), provider.clone());
                if hint.is_some() {
                    super::super::super::queue_io::schedule_deferred_idle_queue_kickoff_immediate(
                        shared, provider, channel_id, reason,
                    );
                } else {
                    super::super::super::schedule_deferred_idle_queue_kickoff(
                        shared, provider, channel_id, reason,
                    );
                }
            }
            if released {
                break;
            }
            last_hint = hint;
            if let Some(hint) = &last_hint {
                OBSERVED_HINTS
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(key.clone(), hint.clone());
            }
            any_released = false;
        }
        let mut watches = draft_release_watches();
        OBSERVED_HINTS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&key);
        watches.remove(&key);
    });
}

/// A pane still marked protected is read off the runtime, between composer mutations.
async fn draft_lifted(session: &str) -> bool {
    use crate::services::claude_tui::composer_lock::{draft_guarded, draft_released};
    if !draft_guarded(session) {
        return true;
    }
    let session = session.to_string();
    let released = tokio::task::spawn_blocking(move || draft_released(&session)).await;
    released.unwrap_or(false)
}
