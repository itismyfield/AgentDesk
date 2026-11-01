//! Held boot restores: rows taken under the persist lock before a failed merge, the boot stale
//! list, the hold reason, and each settle verdict.

use super::*;
use crate::services::discord::make_shared_data_for_tests;
use crate::services::turn_orchestrator::{
    ChannelMailboxRegistry, Intervention, InterventionMode, save_channel_pending_dispatch_marker,
    save_channel_queue,
};

const TORN: &[u8] = br#"[{"message_id": 1, "te"#;

fn item(id: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: 1,
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: format!("input {id}"),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
    }
}

/// A test root whose config binds `channels` to this runtime, and the runtime's shared data.
struct Root {
    shared: Arc<SharedData>,
    _env: crate::config::TestEnvVarGuard,
    _dir: tempfile::TempDir,
}

fn root(channels: &[u64]) -> Root {
    let dir = tempfile::tempdir().unwrap();
    let env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        dir.path(),
    );
    let config = crate::runtime_layout::config_file_path(dir.path());
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    let agents: String = channels
        .iter()
        .map(|id| {
            format!(
                "  - id: held-{id}\n    name: \"held {id}\"\n    provider: claude\n    channels:\n      claude:\n        id: \"{id}\"\n        name: \"held-{id}\"\n"
            )
        })
        .collect();
    std::fs::write(config, format!("server:\n  port: 8791\nagents:\n{agents}")).unwrap();
    Root {
        shared: make_shared_data_for_tests(),
        _env: env,
        _dir: dir,
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn queue_path(shared: &SharedData, channel: ChannelId, ext: &str) -> std::path::PathBuf {
    let root = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    (root.join("claude").join(&shared.token_hash)).join(format!("{channel}.{ext}"))
}

fn save_queue(shared: &SharedData, channel: ChannelId, items: &[Intervention]) {
    save_channel_queue(
        &ProviderKind::Claude,
        &shared.token_hash,
        channel,
        items,
        None,
    )
    .unwrap();
}

fn save_rows(shared: &SharedData, channel: ChannelId, rows: &[(MessageId, MessageId)]) {
    let save = super::super::super::queued_placeholders_store::save_channel_queued_placeholders;
    save(&ProviderKind::Claude, &shared.token_hash, channel, rows);
}

fn disk_rows(shared: &SharedData) -> std::collections::HashMap<(ChannelId, MessageId), MessageId> {
    let load = super::super::super::queued_placeholders_store::load_queued_placeholders;
    load(&ProviderKind::Claude, &shared.token_hash)
}

fn mapped(shared: &SharedData, channel: ChannelId, owner: MessageId) -> Option<MessageId> {
    let map = &shared.queued.queued_placeholders;
    map.get(&(channel, owner)).map(|card| *card)
}

fn context(shared: &SharedData, channel: ChannelId) -> QueuePersistenceContext {
    crate::services::discord::queue_persistence_context(shared, &ProviderKind::Claude, channel)
}

/// Runs the boot restore with `between` run after its file reads and before its merges.
async fn boot<F: std::future::Future<Output = ()>>(
    shared: &Arc<SharedData>,
    between: F,
) -> (Vec<(ChannelId, MessageId, MessageId)>, Vec<HeldRestore>) {
    let settings = shared.settings.write().await;
    let (http, task_shared) = (Arc::new(serenity::Http::new("Bot test")), shared.clone());
    let restore = tokio::spawn(async move {
        super::super::restore_and_hold(&http, &task_shared, &ProviderKind::Claude).await
    });
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    between.await;
    drop(settings);
    restore.await.unwrap()
}

#[derive(Default)]
struct Recorder(std::sync::Mutex<Vec<MessageId>>);

impl super::super::super::queued_placeholders::StalePlaceholderDeleter for Recorder {
    fn delete<'a>(
        &'a self,
        _channel_id: ChannelId,
        card: MessageId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        self.0.lock().unwrap().push(card);
        Box::pin(async { Ok(()) })
    }
}

impl Recorder {
    fn deleted(&self) -> Vec<MessageId> {
        self.0.lock().unwrap().clone()
    }
}

#[test]
fn a_failed_merge_installs_its_rows_before_a_live_handoff_can_save() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (c, q, p, r) = (6_709_701_u64, 6_709_711, 6_709_712, 6_709_721);
    let root = root(&[c]);
    let (shared, channel) = (root.shared.clone(), ChannelId::new(c));
    let (q, p) = (MessageId::new(q), MessageId::new(p));
    save_queue(&shared, channel, &[item(q.get())]);
    let valid = std::fs::read(queue_path(&shared, channel, "json")).unwrap();
    save_rows(&shared, channel, &[(q, p)]);
    let (reached_tx, reached) = tokio::sync::oneshot::channel();
    let (release, release_rx) = tokio::sync::oneshot::channel();
    *PAUSE_AFTER_QUEUE_MERGES.lock().unwrap() = Some((reached_tx, release_rx));
    runtime().block_on(async {
        let restore = tokio::spawn({
            let shared = shared.clone();
            async move {
                boot(&shared, async {
                    std::fs::write(queue_path(&shared, channel, "json"), TORN).unwrap();
                })
                .await
            }
        });
        reached.await.unwrap();
        // The merge failed; before the restore goes on, the file heals and Q runs.
        std::fs::write(queue_path(&shared, channel, "json"), &valid).unwrap();
        let mailbox = shared.mailbox(channel);
        assert!(
            mailbox
                .enqueue(item(r), context(&shared, channel))
                .await
                .enqueued
        );
        let taken = mailbox.take_next_soft(context(&shared, channel)).await;
        assert_eq!(taken.intervention.map(|item| item.message_id), Some(q));
        let handed = shared.remove_queued_placeholder(channel, q).await;
        assert_eq!(handed, Some(p), "Q's handoff takes its card");
        release.send(()).unwrap();
        let (stale, held) = restore.await.unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].reason, HoldReason::QueueRead);
        assert!(!stale.iter().any(|card| card.2 == p), "{stale:?}");
    });
}

#[test]
fn held_rows_survive_another_save_and_a_restart_judges_them_again() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (c, q, p, r, pr) = (6_709_702_u64, 6_709_731, 6_709_732, 6_709_741, 6_709_742);
    let root = root(&[c]);
    let (shared, channel) = (root.shared.clone(), ChannelId::new(c));
    let (q, p) = (MessageId::new(q), MessageId::new(p));
    save_queue(&shared, channel, &[item(q.get())]);
    let valid = std::fs::read(queue_path(&shared, channel, "json")).unwrap();
    save_rows(&shared, channel, &[(q, p)]);
    runtime().block_on(async {
        let (_, held) = boot(&shared, async {
            std::fs::write(queue_path(&shared, channel, "json"), TORN).unwrap();
        })
        .await;
        assert_eq!(held[0].rows, [(q, p)]);
        assert_eq!(
            mapped(&shared, channel, q),
            Some(p),
            "held rows are installed"
        );
        // Another owner's save writes the channel from the map.
        let (r, pr) = (MessageId::new(r), MessageId::new(pr));
        shared.insert_queued_placeholder_locked(channel, r, pr);
        assert_eq!(
            disk_rows(&shared).get(&(channel, q)),
            Some(&p),
            "Q's row stays on disk"
        );
        // A restart with a readable queue restores the row through the normal filter.
        std::fs::write(queue_path(&shared, channel, "json"), &valid).unwrap();
        let restarted = Arc::new(make_shared_data_for_tests());
        let (_, held) = boot(&restarted, async {}).await;
        assert!(held.is_empty());
        assert_eq!(mapped(&restarted, channel, q), Some(p));
    });
}

#[test]
fn a_held_conflict_card_stays_on_the_boot_stale_list_beside_a_normal_channel() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (c, d) = (6_709_703_u64, 6_709_704_u64);
    let root = root(&[c, d]);
    let shared = root.shared.clone();
    let (held_channel, normal) = (ChannelId::new(c), ChannelId::new(d));
    let (q, p, p2) = (
        MessageId::new(6_709_751),
        MessageId::new(6_709_752),
        MessageId::new(6_709_753),
    );
    let (x, px) = (MessageId::new(6_709_761), MessageId::new(6_709_762));
    save_queue(&shared, held_channel, &[item(q.get())]);
    save_queue(&shared, normal, &[item(x.get())]);
    // Q already owns a newer card in memory, while the file still names the old one.
    shared.insert_queued_placeholder_locked(held_channel, q, p2);
    save_rows(&shared, held_channel, &[(q, p)]);
    save_rows(&shared, normal, &[(x, px)]);
    runtime().block_on(async {
        let (stale, held) = boot(&shared, async {
            std::fs::write(queue_path(&shared, held_channel, "json"), TORN).unwrap();
        })
        .await;
        assert_eq!(held.len(), 1);
        assert!(stale.contains(&(held_channel, q, p)), "{stale:?}");
        assert_eq!(
            mapped(&shared, normal, x),
            Some(px),
            "the normal filter ran"
        );
    });
}

#[test]
fn a_queue_read_failure_is_told_apart_on_both_merges() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (fresh, adopted, written) = (6_709_705_u64, 6_709_706_u64, 6_709_707_u64);
    let root = root(&[fresh, adopted, written]);
    let shared = root.shared.clone();
    let channels = [fresh, adopted, written].map(ChannelId::new);
    let token = shared.token_hash.clone();
    for (n, channel) in channels.iter().take(2).enumerate() {
        let marker = item(6_709_770 + n as u64);
        save_channel_pending_dispatch_marker(
            &ProviderKind::Claude,
            &token,
            *channel,
            &marker,
            None,
        )
        .unwrap();
    }
    save_queue(&shared, channels[2], &[item(6_709_779)]);
    runtime().block_on(async {
        // An actor that already adopted its namespace reads the queue only through the loader.
        let hydrate = shared.mailbox(channels[1]);
        let adopted = hydrate.hydrate_pending_queue_from_disk(context(&shared, channels[1]));
        assert!(adopted.await.persistence_error.is_none());
        crate::services::turn_orchestrator::test_support::fail_queue_saves(channels[2], 3);
        let (_, held) = boot(&shared, async {
            for channel in &channels[..2] {
                std::fs::write(queue_path(&shared, *channel, "json"), TORN).unwrap();
            }
        })
        .await;
        crate::services::turn_orchestrator::test_support::queue_save_faults(channels[2], true);
        let reason = |channel: ChannelId| {
            let found = held.iter().find(|held| held.channel == channel);
            found.map(|held| held.reason.clone())
        };
        assert_eq!(
            reason(channels[0]),
            Some(HoldReason::QueueRead),
            "adopt's read"
        );
        assert_eq!(
            reason(channels[1]),
            Some(HoldReason::QueueRead),
            "the loader's read"
        );
        assert!(
            matches!(reason(channels[2]), Some(HoldReason::Other(_))),
            "a write"
        );
    });
}

#[test]
fn the_merge_runs_with_only_its_own_channel_locked() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = root(&[]);
    let shared = root.shared.clone();
    let (c, d) = (ChannelId::new(6_709_708), ChannelId::new(6_709_709));
    let (q, p) = (MessageId::new(6_709_781), MessageId::new(6_709_782));
    save_rows(&shared, c, &[(q, p)]);
    runtime().block_on(async {
        let (lock_c, lock_d) = (
            shared.queued_placeholders_persist_lock(c),
            shared.queued_placeholders_persist_lock(d),
        );
        let merge = async {
            assert!(
                lock_c.try_lock().is_err(),
                "the channel is locked while merging"
            );
            assert!(lock_d.try_lock().is_ok(), "other channels are not");
            crate::services::turn_orchestrator::HydratePendingQueueResult {
                persistence_error: Some("refused".into()),
                ..Default::default()
            }
        };
        let (result, installed) =
            guarded_merge(&shared, &ProviderKind::Claude, c, false, merge).await;
        assert!(result.persistence_error.is_some());
        assert_eq!(installed.map(|(rows, _)| rows), Some(vec![(q, p)]));
        assert_eq!(mapped(&shared, c, q), Some(p));
        assert!(lock_c.try_lock().is_ok(), "released after the install");
    });
}

/// A channel held with Q → P installed, its queue file holding `queue`.
fn held(shared: &SharedData, channel: ChannelId, q: MessageId, p: MessageId) -> HeldRestore {
    shared.insert_queued_placeholder_locked(channel, q, p);
    HeldRestore {
        channel,
        reason: HoldReason::QueueRead,
        queue: Some(vec![item(q.get())]),
        marker: None,
        rows: vec![(q, p)],
    }
}

fn ids(n: u64) -> (ChannelId, MessageId, MessageId) {
    let base = 6_709_800 + n * 10;
    (
        ChannelId::new(base),
        MessageId::new(base + 1),
        MessageId::new(base + 2),
    )
}

#[test]
fn a_cleared_held_queue_settles_and_cleans_its_card() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = root(&[]);
    let shared = root.shared.clone();
    let (c, q, p) = ids(1);
    std::fs::create_dir_all(queue_path(&shared, c, "json").parent().unwrap()).unwrap();
    std::fs::write(queue_path(&shared, c, "json"), TORN).unwrap();
    let mut restore = held(&shared, c, q, p);
    let recorder = Recorder::default();
    runtime().block_on(async {
        shared.mailbox(c).clear(context(&shared, c)).await;
        let report = settle_once(&shared, &ProviderKind::Claude, &recorder, &mut restore).await;
        assert!(report.settled);
        assert_eq!(report.stale, [(c, q, p)]);
        assert_eq!(recorder.deleted(), [p]);
        assert_eq!(mapped(&shared, c, q), None);
    });
}

#[test]
fn a_healed_held_queue_settles_and_keeps_its_card() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = root(&[]);
    let shared = root.shared.clone();
    let (c, q, p) = ids(2);
    save_queue(&shared, c, &[item(q.get())]);
    let mut restore = held(&shared, c, q, p);
    let recorder = Recorder::default();
    runtime().block_on(async {
        let report = settle_once(&shared, &ProviderKind::Claude, &recorder, &mut restore).await;
        assert!(report.settled);
        assert!(report.stale.is_empty() && recorder.deleted().is_empty());
        assert_eq!(mapped(&shared, c, q), Some(p));
        assert!(report.rewatched.contains(&q.get()));
    });
}

#[test]
fn a_still_unreadable_or_refused_purge_queue_stays_held() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = root(&[]);
    let shared = root.shared.clone();
    let (c, q, p) = ids(3);
    save_queue(&shared, c, &[item(q.get())]);
    std::fs::write(queue_path(&shared, c, "json"), TORN).unwrap();
    let mut restore = held(&shared, c, q, p);
    let recorder = Recorder::default();
    runtime().block_on(async {
        let purged = shared
            .mailbox(c)
            .purge_queue(context(&shared, c), false)
            .await;
        assert!(purged.queued_head_refused, "a torn queue refuses the purge");
        let report = settle_once(&shared, &ProviderKind::Claude, &recorder, &mut restore).await;
        assert!(!report.settled);
        assert_eq!(restore.rows, [(q, p)]);
        assert_eq!(mapped(&shared, c, q), Some(p));
        assert!(recorder.deleted().is_empty());
        assert_eq!(std::fs::read(queue_path(&shared, c, "json")).unwrap(), TORN);
    });
}

/// Q taken into a reservation whose caller is gone and old enough to be orphaned.
async fn orphan(shared: &SharedData, channel: ChannelId) {
    let mailbox = shared.mailbox(channel);
    let taken = mailbox.take_next_soft(context(shared, channel)).await;
    assert!(taken.intervention.is_some() && taken.dispatch_lease.is_some());
    drop(taken);
    let age = std::time::Duration::from_secs(60);
    mailbox.age_inbound_waits_for_test(age).await;
}

#[test]
fn an_orphaned_reservation_with_its_marker_settles_with_the_card_kept() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = root(&[]);
    let shared = root.shared.clone();
    let (c, q, p) = ids(4);
    save_queue(&shared, c, &[item(q.get())]);
    let mut restore = held(&shared, c, q, p);
    let recorder = Recorder::default();
    runtime().block_on(async {
        orphan(&shared, c).await;
        let report = settle_once(&shared, &ProviderKind::Claude, &recorder, &mut restore).await;
        assert!(
            report.settled,
            "the orphan is cleared and its marker restored"
        );
        assert_eq!(mapped(&shared, c, q), Some(p));
        assert!(recorder.deleted().is_empty());
        let queue = shared.mailbox(c).snapshot().await.intervention_queue;
        assert_eq!(queue.first().map(|item| item.message_id), Some(q));
    });
}

#[test]
fn an_orphaned_reservation_without_a_marker_settles_with_the_card_cleaned() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = root(&[]);
    let shared = root.shared.clone();
    let (c, q, p) = ids(5);
    save_queue(&shared, c, &[item(q.get())]);
    let mut restore = held(&shared, c, q, p);
    let recorder = Recorder::default();
    runtime().block_on(async {
        orphan(&shared, c).await;
        std::fs::remove_file(queue_path(&shared, c, "dispatch")).unwrap();
        let report = settle_once(&shared, &ProviderKind::Claude, &recorder, &mut restore).await;
        assert!(report.settled, "no new input or clear is needed");
        assert_eq!(recorder.deleted(), [p]);
        assert_eq!(mapped(&shared, c, q), None);
    });
}

#[test]
fn a_settle_kicks_and_arms_the_backstop_when_nothing_starts() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = root(&[]);
    let shared = root.shared.clone();
    let (c, q, p) = ids(6);
    save_queue(&shared, c, &[item(q.get())]);
    let mut restore = held(&shared, c, q, p);
    let recorder = Recorder::default();
    let kicked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = kicked.clone();
    let _hook = super::super::super::queue_io::set_idle_queue_kick_hook_for_tests(Arc::new(
        move |_, _, _, _| {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Some(IdleQueueKickoffChannelOutcome { started: false }) })
        },
    ));
    runtime().block_on(async {
        orphan(&shared, c).await;
        let report = settle_once(&shared, &ProviderKind::Claude, &recorder, &mut restore).await;
        assert!(
            report.settled,
            "a kick that starts nothing does not keep the hold"
        );
        assert_eq!(kicked.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(report.kick.is_some_and(|outcome| !outcome.started));
        assert!(report.backstop, "the queued Q arms the backstop");
    });
}

#[test]
fn a_marker_its_clear_could_not_remove_decides_nothing() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = root(&[]);
    let shared = root.shared.clone();
    let (cleared, kept) = (ids(7), ids(8));
    let recorder = Recorder::default();
    for (c, q, _) in [cleared, kept] {
        std::fs::create_dir_all(queue_path(&shared, c, "json").parent().unwrap()).unwrap();
        std::fs::write(queue_path(&shared, c, "json"), TORN).unwrap();
        std::fs::write(queue_path(&shared, c, "dispatch"), format!("torn {q}")).unwrap();
    }
    let mut held_cleared = held(&shared, cleared.0, cleared.1, cleared.2);
    let mut held_kept = held(&shared, kept.0, kept.1, kept.2);
    std::fs::remove_file(queue_path(&shared, kept.0, "json")).unwrap();
    runtime().block_on(async {
        ChannelMailboxRegistry::fail_clear_removals(cleared.0, true, 2, false);
        shared
            .mailbox(cleared.0)
            .clear(context(&shared, cleared.0))
            .await;
        assert!(queue_path(&shared, cleared.0, "dispatch").exists());
        let provider = ProviderKind::Claude;
        let report = settle_once(&shared, &provider, &recorder, &mut held_cleared).await;
        assert!(
            report.settled,
            "the clear discarded the marker it could not remove"
        );
        assert_eq!(recorder.deleted(), [cleared.2]);
        let report = settle_once(&shared, &provider, &recorder, &mut held_kept).await;
        assert!(
            !report.settled,
            "a trusted marker that cannot be read decides nothing"
        );
        assert_eq!(mapped(&shared, kept.0, kept.1), Some(kept.2));
    });
}

#[test]
fn a_settle_rewatches_the_whole_restored_queue() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = root(&[]);
    let shared = root.shared.clone();
    let (c, q, p) = ids(9);
    let m = MessageId::new(q.get() + 5);
    // The boot saw only the marker; the queue holding Q healed later.
    save_queue(&shared, c, &[item(q.get())]);
    let marker = item(m.get());
    save_channel_pending_dispatch_marker(
        &ProviderKind::Claude,
        &shared.token_hash,
        c,
        &marker,
        None,
    )
    .unwrap();
    let mut restore = held(&shared, c, q, p);
    restore.queue = None;
    restore.marker = Some((marker, None));
    let recorder = Recorder::default();
    runtime().block_on(async {
        let report = settle_once(&shared, &ProviderKind::Claude, &recorder, &mut restore).await;
        assert!(report.settled);
        assert!(
            report.rewatched.contains(&q.get()),
            "{:?}",
            report.rewatched
        );
        assert!(
            report.rewatched.contains(&m.get()),
            "{:?}",
            report.rewatched
        );
    });
}

#[test]
fn retries_back_off_to_a_quarter_hour() {
    let secs: Vec<u64> = (0..7).map(|n| backoff(n).as_secs()).collect();
    assert_eq!(secs, [5, 15, 60, 300, 900, 900, 900]);
}
