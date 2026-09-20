use super::*;

#[cfg(unix)]
#[derive(Clone)]
struct PausedTurnWatcherAttachRequest {
    shared: Arc<SharedData>,
    http: Arc<serenity::Http>,
    provider: ProviderKind,
    channel_id: serenity::ChannelId,
    tmux_session_name: String,
    output_path: String,
    initial_offset: u64,
    source: &'static str,
    thread_parent_channel_id: Option<serenity::ChannelId>,
}

#[cfg(unix)]
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct PendingPausedWatcherAttachKey {
    channel_id: u64,
    tmux_session_name: String,
}

#[cfg(unix)]
impl PendingPausedWatcherAttachKey {
    fn new(channel_id: serenity::ChannelId, tmux_session_name: &str) -> Self {
        Self {
            channel_id: channel_id.get(),
            tmux_session_name: tmux_session_name.to_string(),
        }
    }
}

#[cfg(unix)]
static PENDING_PAUSED_WATCHER_ATTACHES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<PendingPausedWatcherAttachKey>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

#[cfg(not(test))]
const PAUSED_WATCHER_COLD_START_RETRY_ATTEMPTS: u32 = 180;
#[cfg(test)]
const PAUSED_WATCHER_COLD_START_RETRY_ATTEMPTS: u32 = 20;
#[cfg(not(test))]
const PAUSED_WATCHER_COLD_START_RETRY_DELAY: std::time::Duration =
    std::time::Duration::from_secs(1);
#[cfg(test)]
const PAUSED_WATCHER_COLD_START_RETRY_DELAY: std::time::Duration =
    std::time::Duration::from_millis(10);

#[cfg(test)]
static TEST_PAUSED_WATCHER_TMUX_LIVE_OVERRIDE: std::sync::OnceLock<
    std::sync::Mutex<Option<std::collections::HashSet<String>>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
static TEST_SUPPRESS_PAUSED_WATCHER_TASK_SPAWN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
fn set_test_paused_watcher_tmux_live_override(names: Option<&[&str]>) {
    let lock = TEST_PAUSED_WATCHER_TMUX_LIVE_OVERRIDE.get_or_init(|| std::sync::Mutex::new(None));
    let mut guard = lock
        .lock()
        .expect("paused watcher tmux-live override lock poisoned");
    *guard = names.map(|slice| slice.iter().map(|name| (*name).to_string()).collect());
}

#[cfg(test)]
fn set_test_suppress_paused_watcher_task_spawn(suppress: bool) {
    TEST_SUPPRESS_PAUSED_WATCHER_TASK_SPAWN.store(suppress, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(unix)]
fn paused_watcher_tmux_session_has_live_pane(tmux_session_name: &str) -> bool {
    #[cfg(test)]
    {
        if let Some(lock) = TEST_PAUSED_WATCHER_TMUX_LIVE_OVERRIDE.get()
            && let Ok(guard) = lock.lock()
            && let Some(names) = guard.as_ref()
        {
            return names.contains(tmux_session_name);
        }
    }

    crate::services::tmux_diagnostics::tmux_session_has_live_pane(tmux_session_name)
}

#[cfg(unix)]
fn active_watcher_owner_for_tmux(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
) -> Option<serenity::ChannelId> {
    let owner_channel_id = shared
        .tmux_watchers
        .owner_channel_for_tmux_session(tmux_session_name)?;
    let handle = shared.tmux_watchers.get(&owner_channel_id)?;
    (!handle.cancel.load(std::sync::atomic::Ordering::Relaxed)).then_some(owner_channel_id)
}

#[cfg(all(unix, test))]
fn pending_paused_watcher_attach_count_for_tests() -> usize {
    PENDING_PAUSED_WATCHER_ATTACHES
        .lock()
        .expect("pending paused watcher attach lock poisoned")
        .len()
}

#[cfg(all(unix, test))]
fn clear_pending_paused_watcher_attaches_for_tests() {
    PENDING_PAUSED_WATCHER_ATTACHES
        .lock()
        .expect("pending paused watcher attach lock poisoned")
        .clear();
}

#[cfg(unix)]
fn remove_pending_paused_watcher_attach(key: &PendingPausedWatcherAttachKey) {
    let mut guard = PENDING_PAUSED_WATCHER_ATTACHES
        .lock()
        .expect("pending paused watcher attach lock poisoned");
    guard.remove(key);
}

#[cfg(unix)]
fn schedule_pending_paused_turn_watcher_attach(request: PausedTurnWatcherAttachRequest) {
    let key = PendingPausedWatcherAttachKey::new(request.channel_id, &request.tmux_session_name);
    {
        let mut guard = PENDING_PAUSED_WATCHER_ATTACHES
            .lock()
            .expect("pending paused watcher attach lock poisoned");
        if !guard.insert(key.clone()) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::debug!(
                "  [{ts}] ↻ Pending paused tmux watcher attach already scheduled for channel {} — tmux {}",
                request.channel_id,
                request.tmux_session_name
            );
            return;
        }
    }

    if tokio::runtime::Handle::try_current().is_err() {
        remove_pending_paused_watcher_attach(&key);
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ↻ Unable to schedule paused tmux watcher retry for channel {} — no Tokio runtime",
            request.channel_id
        );
        return;
    }

    super::super::super::task_supervisor::spawn_observed(
        "pending_paused_turn_watcher_attach",
        async move {
            for attempt in 1..=PAUSED_WATCHER_COLD_START_RETRY_ATTEMPTS {
                tokio::time::sleep(PAUSED_WATCHER_COLD_START_RETRY_DELAY).await;
                if let Some(owner) =
                    active_watcher_owner_for_tmux(&request.shared, &request.tmux_session_name)
                {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] ↻ Skipping stale paused tmux watcher cold-start retry for channel {} via attempt {attempt}; tmux {} is already owned by {}",
                        request.channel_id,
                        request.tmux_session_name,
                        owner
                    );
                    remove_pending_paused_watcher_attach(&key);
                    return;
                }

                if paused_watcher_tmux_session_has_live_pane(&request.tmux_session_name) {
                    let owner = attach_paused_turn_watcher_inner(request.clone(), false);
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] ↻ Re-attached paused tmux watcher for channel {} via cold-start retry attempt {attempt}; owner={}",
                        request.channel_id,
                        owner
                    );
                    remove_pending_paused_watcher_attach(&key);
                    return;
                }
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ↻ Giving up paused tmux watcher retry for channel {} after {} attempts — tmux {} never became live",
                request.channel_id,
                PAUSED_WATCHER_COLD_START_RETRY_ATTEMPTS,
                request.tmux_session_name
            );
            remove_pending_paused_watcher_attach(&key);
        },
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) fn attach_paused_turn_watcher(
    shared: &Arc<SharedData>,
    http: Arc<serenity::Http>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    tmux_session_name: Option<String>,
    output_path: Option<String>,
    initial_offset: u64,
    source: &'static str,
    thread_parent_channel_id: Option<serenity::ChannelId>,
) -> serenity::ChannelId {
    #[cfg(unix)]
    if let (Some(tmux_session_name), Some(output_path)) = (tmux_session_name, output_path) {
        return attach_paused_turn_watcher_inner(
            PausedTurnWatcherAttachRequest {
                shared: shared.clone(),
                http,
                provider: provider.clone(),
                channel_id,
                tmux_session_name,
                output_path,
                initial_offset,
                source,
                thread_parent_channel_id,
            },
            true,
        );
    }

    #[cfg(not(unix))]
    {
        let _ = (
            shared,
            http,
            provider,
            tmux_session_name,
            output_path,
            initial_offset,
            source,
            thread_parent_channel_id,
        );
    }

    channel_id
}

#[allow(clippy::too_many_arguments)]
pub(super) fn attach_paused_turn_watcher_for_inflight(
    shared: &Arc<SharedData>,
    http: Arc<serenity::Http>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    tmux_session_name: Option<String>,
    output_path: Option<String>,
    initial_offset: u64,
    source: &'static str,
    thread_parent_channel_id: Option<serenity::ChannelId>,
    inflight_state: &mut InflightTurnState,
) -> serenity::ChannelId {
    let owner_channel_id = attach_paused_turn_watcher(
        shared,
        http,
        provider,
        channel_id,
        tmux_session_name,
        output_path,
        initial_offset,
        source,
        thread_parent_channel_id,
    );
    if inflight_state.set_watcher_owner_channel_id(owner_channel_id.get()) {
        let outcome = crate::services::discord::inflight::save_inflight_state_if_identity_unchanged(
            inflight_state,
            "attach_paused_turn_watcher_for_inflight",
        );
        if !matches!(
            outcome,
            crate::services::discord::inflight::GuardedSaveOutcome::Saved
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}]   ⚠ inflight owner-channel save skipped: {outcome:?}");
        }
    }
    owner_channel_id
}

#[cfg(unix)]
fn attach_paused_turn_watcher_inner(
    request: PausedTurnWatcherAttachRequest,
    allow_cold_start_retry: bool,
) -> serenity::ChannelId {
    let PausedTurnWatcherAttachRequest {
        shared,
        http,
        provider,
        channel_id,
        tmux_session_name,
        output_path,
        initial_offset,
        source,
        thread_parent_channel_id,
    } = request;
    let mut watcher_owner_channel_id = channel_id;

    {
        let existing_owner_for_tmux =
            active_watcher_owner_for_tmux(&shared, &tmux_session_name).is_some();
        let tmux_live = paused_watcher_tmux_session_has_live_pane(&tmux_session_name);
        if !tmux_live && !existing_owner_for_tmux {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ Deferring paused tmux watcher attach for channel {} ({source}) — tmux {} is not live yet",
                channel_id,
                tmux_session_name
            );
            if allow_cold_start_retry {
                schedule_pending_paused_turn_watcher_attach(PausedTurnWatcherAttachRequest {
                    shared,
                    http,
                    provider,
                    channel_id,
                    tmux_session_name,
                    output_path,
                    initial_offset,
                    source,
                    thread_parent_channel_id,
                });
            }
            return watcher_owner_channel_id;
        }

        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let paused = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let resume_offset = Arc::new(std::sync::Mutex::new(None::<u64>));
        let pause_epoch = Arc::new(std::sync::atomic::AtomicU64::new(u64::from(
            !allow_cold_start_retry,
        )));
        let turn_delivered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let last_heartbeat_ts_ms = Arc::new(std::sync::atomic::AtomicI64::new(
            super::super::super::tmux_watcher_now_ms(),
        ));
        let handle = TmuxWatcherHandle {
            tmux_session_name: tmux_session_name.clone(),
            output_path: output_path.clone(),
            paused: paused.clone(),
            resume_offset: resume_offset.clone(),
            cancel: cancel.clone(),
            pause_epoch: pause_epoch.clone(),
            turn_delivered: turn_delivered.clone(),
            last_heartbeat_ts_ms: last_heartbeat_ts_ms.clone(),
        };
        let claim = super::super::super::tmux::claim_or_reuse_watcher_with_thread_parent(
            &shared.tmux_watchers,
            channel_id,
            handle,
            &provider,
            source,
            super::super::super::tmux::thread_follow_up_parent_from_live(thread_parent_channel_id),
        );
        watcher_owner_channel_id = claim.owner_channel_id();
        if claim.should_spawn() {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ Attaching tmux watcher for turn on channel {} ({})",
                channel_id,
                claim.as_str()
            );
            if claim.replaced_existing() {
                shared.record_tmux_watcher_reconnect(channel_id);
            }
            #[cfg(test)]
            let suppress_spawn =
                TEST_SUPPRESS_PAUSED_WATCHER_TASK_SPAWN.load(std::sync::atomic::Ordering::Relaxed);
            #[cfg(not(test))]
            let suppress_spawn = false;
            if !suppress_spawn {
                if tokio::runtime::Handle::try_current().is_ok() {
                    super::super::super::task_supervisor::spawn_observed_tmux_watcher(
                        "router_tmux_output_watcher",
                        shared.clone(),
                        tmux_session_name.clone(),
                        cancel.clone(),
                        super::super::super::tmux::tmux_output_watcher(
                            channel_id,
                            http,
                            shared.clone(),
                            output_path,
                            tmux_session_name,
                            initial_offset,
                            cancel,
                            paused,
                            resume_offset,
                            pause_epoch,
                            turn_delivered,
                            last_heartbeat_ts_ms,
                        ),
                    );
                } else {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ↻ Unable to spawn tmux watcher for channel {} — no Tokio runtime",
                        channel_id
                    );
                }
            }
        }
    }

    // Deferred retries prepare their pause before claim and never pause a later owner.
    // Immediate turn starts still open a pause window before provider input.
    if !allow_cold_start_retry {
        return watcher_owner_channel_id;
    }

    if let Some(watcher) = shared.tmux_watchers.get(&watcher_owner_channel_id) {
        watcher
            .pause_epoch
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        watcher
            .paused
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    watcher_owner_channel_id
}

#[cfg(all(test, unix))]
mod relay_state_contract_refs {
    //! #4268 — relay-state contract symbol anchor for the `pause_epoch` producer
    //! (compiler-checked existence). `attach_paused_turn_watcher_inner` is the
    //! sole production writer of `TmuxWatcherHandle::pause_epoch` (invariant I5),
    //! and it is a private fn nameable only from within `watchdog`, so its anchor
    //! lives here rather than in the central blocks.
    //!
    //! Gated `all(test, unix)` (not plain `test`): `attach_paused_turn_watcher_inner`
    //! is itself `#[cfg(unix)]`, so on windows test builds it is compiled out and
    //! a plain `#[cfg(test)]` anchor referencing it fails to compile (E0432,
    //! #4268 r3 / #4394). Matching its platform gate makes the anchor and the
    //! symbol appear/disappear together. `unix` is true on the required ubuntu
    //! `check_fast` compile, so that required job still compiles this block and
    //! proves the symbol exists. `#[cfg(all(test, unix))]` is one of the two
    //! byte-exact cfg spellings the checker whitelists (the other is
    //! `#[cfg(test)]`); a windows-only gate is rejected because no required job
    //! compiles it.
    //!
    //! See the header on `inflight::store::relay_state_contract_refs` for the
    //! contract, the CI wiring, and why there are no `// sym:` labels.
    #[test]
    fn contract_symbols_exist() {
        use super::attach_paused_turn_watcher_inner as _;
    }
}

#[cfg(all(test, unix))]
mod cold_start_retry_tests {
    //! #5776: the after-precheck fixtures model completed runtime handoff state
    //! before invoking the real deferred attach; they do not replay scheduler timing.
    //! A retry must preserve that incarnation. A fresh retry still starts paused
    //! at epoch 1. Preparing that state before claim also avoids a late registry
    //! write to a replacement installed after the retry's own claim.
    use super::*;
    use crate::services::discord::{tmux, tmux_watcher_now_ms};
    use std::sync::{LazyLock, Mutex, MutexGuard};
    use tokio::time::{Duration, sleep, timeout};

    static RETRY_TEST_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    struct RetryTestGuard {
        _lock: MutexGuard<'static, ()>,
    }

    impl RetryTestGuard {
        fn new() -> Self {
            let lock = RETRY_TEST_MUTEX
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            clear_pending_paused_watcher_attaches_for_tests();
            set_test_paused_watcher_tmux_live_override(Some(&[]));
            set_test_suppress_paused_watcher_task_spawn(true);
            Self { _lock: lock }
        }
    }

    impl Drop for RetryTestGuard {
        fn drop(&mut self) {
            set_test_paused_watcher_tmux_live_override(None);
            set_test_suppress_paused_watcher_task_spawn(false);
            clear_pending_paused_watcher_attaches_for_tests();
        }
    }

    fn test_watcher_handle(
        tmux_session_name: &str,
        output_path: &str,
        paused: bool,
    ) -> TmuxWatcherHandle {
        TmuxWatcherHandle {
            tmux_session_name: tmux_session_name.to_string(),
            output_path: output_path.to_string(),
            paused: Arc::new(std::sync::atomic::AtomicBool::new(paused)),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_heartbeat_ts_ms: Arc::new(
                std::sync::atomic::AtomicI64::new(tmux_watcher_now_ms()),
            ),
        }
    }

    #[tokio::test]
    async fn deferred_paused_watcher_attach_retries_when_tmux_goes_live() {
        let _guard = RetryTestGuard::new();
        let shared = super::super::super::super::make_shared_data_for_tests();
        let channel = serenity::ChannelId::new(1485506232256168199);
        let tmux_name = format!(
            "AgentDesk-codex-cold-start-retry-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );

        let owner = attach_paused_turn_watcher(
            &shared,
            Arc::new(poise::serenity_prelude::Http::new("Bot test-token")),
            &ProviderKind::Codex,
            channel,
            Some(tmux_name.clone()),
            Some("/tmp/agentdesk-cold-start-retry-output.jsonl".to_string()),
            42,
            "unit-test-cold-start-restore",
            None,
        );

        assert_eq!(owner, channel);
        assert!(
            !shared.tmux_watchers.contains_key(&channel),
            "cold-start attach must not create a dead-pane watcher immediately"
        );
        assert_eq!(
            pending_paused_watcher_attach_count_for_tests(),
            1,
            "dead tmux attach should leave a bounded retry registered"
        );

        set_test_paused_watcher_tmux_live_override(Some(&[tmux_name.as_str()]));

        timeout(Duration::from_secs(1), async {
            loop {
                if shared.tmux_watchers.contains_key(&channel)
                    && pending_paused_watcher_attach_count_for_tests() == 0
                {
                    break;
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("retry should attach once tmux becomes live");

        let watcher = shared
            .tmux_watchers
            .get(&channel)
            .expect("retry should install a watcher slot");
        assert_eq!(watcher.tmux_session_name, tmux_name);
        assert_eq!(
            watcher.output_path,
            "/tmp/agentdesk-cold-start-retry-output.jsonl"
        );
        assert!(
            watcher.paused.load(std::sync::atomic::Ordering::Relaxed),
            "reattached restored-turn watcher must stay paused until turn bridge hands off"
        );
        assert_eq!(
            watcher
                .pause_epoch
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "a fresh deferred attach must establish the first turn's pause epoch"
        );
    }

    fn assert_retry_after_precheck_preserves_handoff(source: &'static str) {
        let _guard = RetryTestGuard::new();
        let shared = super::super::super::super::make_shared_data_for_tests();
        let channel = serenity::ChannelId::new(1485506232256168202);
        let tmux_name = format!("AgentDesk-cold-start-after-precheck-{source}");
        let runtime_path = "/tmp/agentdesk-after-precheck-runtime.jsonl";
        set_test_paused_watcher_tmux_live_override(Some(&[tmux_name.as_str()]));

        // R1: the scheduled retry has passed its owner precheck.
        assert_eq!(active_watcher_owner_for_tmux(&shared, &tmux_name), None);

        // H2: runtime handoff installs a healthy watcher and finishes unpausing
        // it before R resumes. Use the real registry claim to establish W.
        let active = test_watcher_handle(&tmux_name, runtime_path, false);
        active
            .pause_epoch
            .store(1, std::sync::atomic::Ordering::Relaxed);
        let cancel = active.cancel.clone();
        let paused = active.paused.clone();
        let pause_epoch = active.pause_epoch.clone();
        let epoch_before = pause_epoch.load(std::sync::atomic::Ordering::Relaxed);
        let claim = tmux::claim_or_reuse_watcher(
            &shared.tmux_watchers,
            channel,
            active,
            &ProviderKind::Codex,
            "turn_bridge_tmux_ready",
        );
        assert!(
            claim.should_spawn(),
            "handoff must install the first watcher"
        );
        assert_eq!(claim.owner_channel_id(), channel);
        assert!(!paused.load(std::sync::atomic::Ordering::Relaxed));

        // R2: execute the production deferred-retry caller after H2. Its
        // provisional wrapper path must not replace the runtime transcript.
        let owner = attach_paused_turn_watcher_inner(
            PausedTurnWatcherAttachRequest {
                shared: shared.clone(),
                http: Arc::new(poise::serenity_prelude::Http::new("Bot test-token")),
                provider: ProviderKind::Codex,
                channel_id: channel,
                tmux_session_name: tmux_name.clone(),
                output_path: "/tmp/agentdesk-after-precheck-provisional.jsonl".to_string(),
                initial_offset: 0,
                source,
                thread_parent_channel_id: None,
            },
            false,
        );

        assert_eq!(owner, channel);
        assert_eq!(
            active_watcher_owner_for_tmux(&shared, &tmux_name),
            Some(channel)
        );
        assert_eq!(
            shared.tmux_watchers.len(),
            1,
            "retry must preserve one watcher"
        );
        let watcher = shared
            .tmux_watchers
            .get(&channel)
            .expect("handoff owner remains");
        assert!(Arc::ptr_eq(&watcher.cancel, &cancel));
        assert!(Arc::ptr_eq(&watcher.paused, &paused));
        assert!(Arc::ptr_eq(&watcher.pause_epoch, &pause_epoch));
        assert!(!watcher.cancel.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(watcher.tmux_session_name, tmux_name);
        assert_eq!(watcher.output_path, runtime_path);
        assert!(
            !watcher.paused.load(std::sync::atomic::Ordering::Relaxed),
            "{source}: retry after owner precheck must preserve the completed handoff's unpaused state"
        );
        assert_eq!(
            watcher
                .pause_epoch
                .load(std::sync::atomic::Ordering::Relaxed),
            epoch_before,
            "{source}: deferred retry must not begin another turn's pause epoch"
        );
    }

    #[tokio::test]
    async fn message_retry_after_precheck_preserves_handoff_incarnation() {
        assert_retry_after_precheck_preserves_handoff("turn_start_message");
    }

    #[tokio::test]
    async fn headless_retry_after_precheck_preserves_handoff_incarnation() {
        assert_retry_after_precheck_preserves_handoff("turn_start_headless");
    }

    #[tokio::test]
    async fn cold_start_retry_does_not_repause_existing_live_handoff_watcher() {
        let _guard = RetryTestGuard::new();
        let shared = super::super::super::super::make_shared_data_for_tests();
        let channel = serenity::ChannelId::new(1485506232256168201);
        let tmux_name = format!(
            "AgentDesk-claude-cold-start-active-owner-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let output_path = "/tmp/agentdesk-cold-start-active-owner-output.jsonl";

        let owner = attach_paused_turn_watcher(
            &shared,
            Arc::new(poise::serenity_prelude::Http::new("Bot test-token")),
            &ProviderKind::Claude,
            channel,
            Some(tmux_name.clone()),
            Some(output_path.to_string()),
            0,
            "turn_start_headless",
            None,
        );

        assert_eq!(owner, channel);
        assert!(
            !shared.tmux_watchers.contains_key(&channel),
            "cold-start attach must not create a dead-pane watcher immediately"
        );
        assert_eq!(
            pending_paused_watcher_attach_count_for_tests(),
            1,
            "dead tmux attach should leave a bounded retry registered"
        );

        let active = test_watcher_handle(&tmux_name, output_path, false);
        let paused_flag = active.paused.clone();
        let claim = tmux::claim_or_reuse_watcher(
            &shared.tmux_watchers,
            channel,
            active,
            &ProviderKind::Claude,
            "unit-test-tmux-ready-handoff",
        );
        assert_eq!(claim.owner_channel_id(), channel);
        assert!(!paused_flag.load(std::sync::atomic::Ordering::Relaxed));

        timeout(Duration::from_secs(1), async {
            loop {
                if pending_paused_watcher_attach_count_for_tests() == 0 {
                    break;
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("retry should retire itself when a handoff watcher already owns the tmux");

        assert!(
            !paused_flag.load(std::sync::atomic::Ordering::Relaxed),
            "stale cold-start retry must not pause a watcher already unpaused by turn bridge handoff"
        );
    }
}
