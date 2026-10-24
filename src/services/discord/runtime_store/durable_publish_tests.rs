use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use super::flush_probe::{Gate, Plan, Step, Watch};
use super::*;
use crate::services::discord::runtime_store::rename_fault;
use crate::services::discord::{tui_direct_abort_marker as abort, tui_direct_pending_start};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{
    ChannelMailboxRegistry, Intervention, InterventionMode, QueuePersistenceContext,
};

const TOKEN: &str = "durable-token";

/// The process env names one runtime root for the whole test; other env tests wait.
struct RootEnv {
    temp: tempfile::TempDir,
    previous: Option<String>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl RootEnv {
    fn new() -> Self {
        let lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let previous = std::env::var("AGENTDESK_ROOT_DIR").ok();
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
        Self {
            temp,
            previous,
            _lock: lock,
        }
    }

    fn root(&self) -> &Path {
        self.temp.path()
    }

    fn runtime(&self) -> PathBuf {
        self.root().join("runtime")
    }

    fn queue(&self, channel: u64) -> PathBuf {
        self.runtime()
            .join("discord_pending_queue/codex")
            .join(TOKEN)
            .join(format!("{channel}.json"))
    }
}

impl Drop for RootEnv {
    fn drop(&mut self) {
        match &self.previous {
            Some(previous) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", previous) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }
}

fn runtime(threads: usize) -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .enable_all()
        .build()
        .unwrap()
}

fn persistence() -> QueuePersistenceContext {
    QueuePersistenceContext::new(&ProviderKind::Codex, TOKEN, None)
}

fn intervention(id: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(1),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: super::super::process_generation(),
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: format!("msg-{id}"),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

/// Flush steps that ran for `target`, each with the bytes the target held at that moment.
fn flushes_of(watch: &Watch, target: &Path, step: Step) -> Vec<(bool, Option<String>)> {
    watch
        .events()
        .into_iter()
        .filter(|event| event.target == target && event.step == step)
        .map(|event| (event.ok, event.target_bytes))
        .collect()
}

fn capture_logs(run: impl FnOnce()) -> String {
    let buffer = Arc::new(std::sync::Mutex::new(Vec::new()));
    let output = buffer.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || Capture(output.clone()))
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    tracing::subscriber::with_default(subscriber, run);
    let bytes = buffer.lock().unwrap_or_else(|e| e.into_inner()).clone();
    String::from_utf8(bytes).unwrap()
}

struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A pre-rename failure is an `Err` leaving the old bytes; a later flush failure is a published
/// `SyncFailed` holding the whole new bytes. The target is never removed.
#[test]
fn publication_failures_at_each_stage() {
    if !PARENT_DIR_FSYNC_FLUSHES {
        return; // no directory flush to observe; `windows_contract` covers this platform
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("runtime");
    let path = root.join("store").join("1.json");
    publish_durable(&root, &path, "old").unwrap();

    for (step, stage) in [
        (Step::CreateTemp, PrePublishStage::CreateTemp),
        (Step::Write, PrePublishStage::Write),
        (Step::FileSync, PrePublishStage::FileSync),
    ] {
        let plan = Plan {
            fail: vec![step],
            ..Plan::default()
        };
        let watch = Watch::new(temp.path(), plan);
        let error = publish_durable(&root, &path, "new").unwrap_err();
        assert_eq!(error.stage, stage);
        assert_eq!(fs::read_to_string(&path).unwrap(), "old");
        assert_eq!(watch.count(Step::ParentSync), 0, "{stage:?}");
    }
    let watch = Watch::new(temp.path(), Plan::default());
    let blocked = temp.path().join("file-not-dir");
    fs::write(&blocked, "x").unwrap();
    let error = publish_durable(&root, &blocked.join("1.json"), "new").unwrap_err();
    assert_eq!(error.stage, PrePublishStage::CreateDir);
    let fault = rename_fault::Armed::new(&path);
    let error = publish_durable(&root, &path, "new").unwrap_err();
    assert_eq!(error.stage, PrePublishStage::Rename);
    assert_eq!(fault.evidence().1.len(), 1, "the OS refused a real rename");
    drop(fault);
    assert_eq!(fs::read_to_string(&path).unwrap(), "old");
    assert_eq!(
        watch.count(Step::ParentOpen) + watch.count(Step::ParentSync),
        0
    );
    drop(watch);

    for (step, stage) in [
        (Step::ParentOpen, SyncStage::ParentOpen),
        (Step::ParentSync, SyncStage::ParentSync),
        (Step::Ancestors, SyncStage::Ancestors),
    ] {
        let plan = Plan {
            fail: vec![step],
            ..Plan::default()
        };
        let _watch = Watch::new(temp.path(), plan);
        let fresh = root.join(format!("{step:?}")).join("1.json");
        let published = publish_durable(&root, &fresh, "new").unwrap();
        assert!(
            matches!(&published.flush_status, FlushStatus::SyncFailed { stage: s, .. } if *s == stage),
            "{published:?}"
        );
        assert_eq!(fs::read_to_string(&fresh).unwrap(), "new");
    }
    let published = publish_durable(&root, &root.join("ok").join("1.json"), "new").unwrap();
    assert_eq!(published.flush_status, FlushStatus::Flushed);

    let plan = Plan {
        fail: vec![Step::ParentSync],
        ..Plan::default()
    };
    let _watch = Watch::new(temp.path(), plan);
    let context = AtomicWriteContext::new("probe").durable_root(&root);
    let logs = capture_logs(|| {
        critical_publish(&path, "newest", context).expect("published despite the flush failure");
    });
    assert!(logs.contains("directory sync failed"), "{logs}");
    assert!(logs.contains("published=true"), "{logs}");
    assert!(
        !logs.contains("recovery-critical atomic write failed"),
        "{logs}"
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "newest");
}

/// The six recovery-critical writers flush the parent only after their rename made the new bytes
/// visible; a real rename failure flushes nothing and stays the write failure it was.
#[test]
fn b6_sync_only_after_successful_rename() {
    if !PARENT_DIR_FSYNC_FLUSHES {
        return; // no directory flush to observe; `windows_contract` covers this platform
    }
    let env = RootEnv::new();
    let watch = Watch::new(env.root(), Plan::default());
    let channel = ChannelId::new(7_190_001);
    let queue = env.queue(channel.get());
    runtime(2).block_on(async {
        let handle = ChannelMailboxRegistry::default().handle(channel);
        assert!(
            handle
                .enqueue(intervention(1), persistence())
                .await
                .enqueued
        );
        let flushes = flushes_of(&watch, &queue, Step::ParentSync);
        assert_eq!(flushes.len(), 1);
        assert!(flushes[0].1.as_deref().is_some_and(|b| b.contains("msg-1")));

        let fault = rename_fault::Armed::new(&queue);
        let refused = handle.enqueue(intervention(2), persistence()).await;
        assert!(refused.persistence_error.unwrap().contains("rename"));
        assert_eq!(fault.evidence().1.len(), 1, "the OS refused a real rename");
        assert_eq!(flushes_of(&watch, &queue, Step::ParentOpen).len(), 0);
        assert_eq!(flushes_of(&watch, &queue, Step::ParentSync).len(), 1);
        drop(fault);

        crate::services::turn_orchestrator::save_channel_pending_dispatch_marker(
            &ProviderKind::Codex,
            TOKEN,
            channel,
            &intervention(3),
            None,
        )
        .unwrap();
        let cleared = handle.clear(persistence()).await;
        assert!(
            cleared.persistence.failed.is_empty(),
            "{:?}",
            cleared.persistence.failed
        );
    });
    let flushes = flushes_of(&watch, &queue, Step::ParentSync);
    assert_eq!(flushes.len(), 2, "the clear snapshot flushed once");
    assert_eq!(flushes[1].1, fs::read_to_string(&queue).ok());
    let marker = queue.with_extension("dispatch");
    let marker_flushes = flushes_of(&watch, &marker, Step::ParentSync);
    assert!(
        marker_flushes[0]
            .1
            .as_deref()
            .is_some_and(|b| b.contains("msg-3"))
    );
    assert_eq!(
        marker_flushes.len(),
        2,
        "published, then removed by the clear"
    );
    assert_eq!(marker_flushes[1].1, None);

    let record = tui_direct_pending_start::TuiDirectPendingStart {
        provider: "codex".into(),
        channel_id: channel.get(),
        anchor_message_id: 71_900,
        ..Default::default()
    };
    tui_direct_pending_start::persist(&record).unwrap();
    tui_direct_pending_start::delete(&record);
    let base = tempfile::tempdir().unwrap();
    let abort_watch = Watch::new(base.path(), Plan::default());
    abort::set_test_root_override(Some(base.path().to_path_buf()));
    let marker =
        abort::AbortedAnchorMarker::for_abort("codex".into(), 9, 71_901, "t".into(), 1, None);
    abort::record(&marker).unwrap();
    abort::record_commit_tombstone_at(2, "codex", "t", 9, 71_902, "s");
    abort::set_test_root_override(None);

    let published_dirs = |watch: &Watch| -> Vec<PathBuf> {
        watch
            .events()
            .into_iter()
            .filter(|e| e.step == Step::ParentSync && e.ok && e.target_bytes.is_some())
            .map(|e| e.dir)
            .collect()
    };
    let dirs = published_dirs(&watch);
    assert!(dirs.contains(&env.runtime().join("discord_tui_direct_pending_start")));
    let dirs = published_dirs(&abort_watch);
    assert!(dirs.contains(&base.path().join("discord_tui_direct_abort_marker")));
    assert!(dirs.contains(&base.path().join("discord_tui_direct_commit_tombstone")));
    let ancestors = abort_watch.flushed(Step::Ancestors);
    assert!(!ancestors.is_empty());
    assert!(
        ancestors.iter().all(|dir| dir == base.path()),
        "{ancestors:?}"
    );
}

/// A parent flush failure after the rename keeps the enqueue accepted: no memory rollback, no
/// second publication, no refusal.
#[test]
fn b6_parent_failure_keeps_published_outcome() {
    if !PARENT_DIR_FSYNC_FLUSHES {
        return; // no directory flush to observe; `windows_contract` covers this platform
    }
    let env = RootEnv::new();
    let plan = Plan {
        fail: vec![Step::ParentSync],
        ..Plan::default()
    };
    let watch = Watch::new(env.root(), plan);
    let channel = ChannelId::new(7_190_002);
    let queue = env.queue(channel.get());
    runtime(2).block_on(async {
        let handle = ChannelMailboxRegistry::default().handle(channel);
        let result = handle.enqueue(intervention(11), persistence()).await;
        assert!(result.enqueued);
        assert!(result.persistence_error.is_none());
        assert!(result.refusal_reason.is_none());
        let snapshot = handle.snapshot().await;
        assert_eq!(snapshot.intervention_queue.len(), 1, "no rollback");
    });
    assert!(fs::read_to_string(&queue).unwrap().contains("msg-11"));
    let writes = watch
        .events()
        .into_iter()
        .filter(|e| e.target == queue && e.step == Step::Write);
    assert_eq!(
        writes.count(),
        1,
        "the visible snapshot is never written again"
    );
    assert_eq!(
        flushes_of(&watch, &queue, Step::ParentSync),
        vec![(false, fs::read_to_string(&queue).ok())]
    );
}

/// Removing a queue file, a dispatch marker, a pending-start record or an abort marker flushes
/// the parent afterwards; a failed flush neither restores the file nor the in-memory state.
#[test]
fn retirement_syncs_without_resurrecting_memory() {
    if !PARENT_DIR_FSYNC_FLUSHES {
        return; // no directory flush to observe; `windows_contract` covers this platform
    }
    let env = RootEnv::new();
    let plan = Plan {
        fail: vec![Step::ParentSync],
        ..Plan::default()
    };
    let watch = Watch::new(env.root(), plan);
    let channel = ChannelId::new(7_190_003);
    let queue = env.queue(channel.get());
    runtime(2).block_on(async {
        let handle = ChannelMailboxRegistry::default().handle(channel);
        assert!(
            handle
                .enqueue(intervention(21), persistence())
                .await
                .enqueued
        );
        let taken = handle.take_next_soft(persistence()).await;
        assert!(taken.intervention.is_some());
        assert!(taken.persistence_error.is_none());
        assert!(handle.snapshot().await.intervention_queue.is_empty());
    });
    assert!(!queue.exists());
    let removals = flushes_of(&watch, &queue, Step::ParentSync);
    assert_eq!(
        removals.last(),
        Some(&(false, None)),
        "flushed after the unlink"
    );

    crate::services::turn_orchestrator::save_channel_pending_dispatch_marker(
        &ProviderKind::Codex,
        TOKEN,
        channel,
        &intervention(22),
        None,
    )
    .unwrap();
    let marker = queue.with_extension("dispatch");
    let purged = crate::services::turn_orchestrator::remove_channel_pending_queue_files_all_tokens(
        &ProviderKind::Codex,
        channel,
    );
    assert_eq!(purged, 1);
    assert!(!marker.exists());
    assert_eq!(
        flushes_of(&watch, &marker, Step::ParentSync).last(),
        Some(&(false, None))
    );

    let record = tui_direct_pending_start::TuiDirectPendingStart {
        provider: "codex".into(),
        channel_id: channel.get(),
        anchor_message_id: 71_903,
        ..Default::default()
    };
    tui_direct_pending_start::persist(&record).unwrap();
    tui_direct_pending_start::delete(&record);
    assert!(!tui_direct_pending_start::pending_synthetic_start_present(
        "codex",
        channel.get()
    ));
    let records = env.runtime().join("discord_tui_direct_pending_start");
    let removed = watch
        .events()
        .into_iter()
        .filter(|e| e.dir == records && e.target_bytes.is_none());
    assert_eq!(removed.count(), 1);
    assert_eq!(fs::read_dir(&records).unwrap().count(), 0);

    let base = tempfile::tempdir().unwrap();
    let abort_watch = Watch::new(base.path(), Plan::default());
    abort::set_test_root_override(Some(base.path().to_path_buf()));
    let marker =
        abort::AbortedAnchorMarker::for_abort("codex".into(), 9, 71_904, "t".into(), 1, None);
    abort::record(&marker).unwrap();
    abort::delete(&marker);
    assert!(abort::load_all().is_empty());
    abort::set_test_root_override(None);
    let markers = base.path().join("discord_tui_direct_abort_marker");
    let removed = abort_watch
        .events()
        .into_iter()
        .filter(|e| e.dir == markers && e.ok && e.target_bytes.is_none());
    assert_eq!(removed.count(), 1);
}

/// One channel's directory flush stuck in the kernel holds no lock other channels need: their
/// enqueue replies, and the output-side ledger append completes.
#[test]
fn blocked_sync_other_channel_keeps_progress() {
    if !PARENT_DIR_FSYNC_FLUSHES {
        return; // no directory flush to observe; `windows_contract` covers this platform
    }
    let env = RootEnv::new();
    let _all = Watch::new(env.root(), Plan::default());
    let (a, b) = (ChannelId::new(7_190_004), ChannelId::new(7_190_005));
    let gate = Arc::new(Gate::default());
    let plan = Plan {
        block: Some(gate.clone()),
        ..Plan::default()
    };
    let _blocked = Watch::new(&env.queue(a.get()), plan);
    let _release = Release(gate.clone());
    let registry = Arc::new(ChannelMailboxRegistry::default());
    runtime(4).block_on(async {
        let blocked = tokio::spawn({
            let registry = registry.clone();
            async move { registry.handle(a).enqueue(intervention(31), persistence()).await }
        });
        let entered = gate.clone();
        let entered =
            tokio::task::spawn_blocking(move || entered.wait_entered(Duration::from_secs(10)));
        assert!(entered.await.unwrap(), "channel A reached its parent flush");

        let other_handle = registry.handle(b);
        let other = other_handle.enqueue(intervention(32), persistence());
        let other = tokio::time::timeout(Duration::from_secs(10), other).await;
        assert!(other.expect("channel B is not held by A's flush").enqueued);
        let appended = tokio::task::spawn_blocking(move || {
            crate::services::discord::outbound::completed_turn_ledger::append_before_publish_for_tests(
                b.get(), 32, "n", 1, || {},
            )
        });
        let appended = tokio::time::timeout(Duration::from_secs(10), appended).await;
        appended.expect("ledger append is not held").unwrap().unwrap();
        assert!(!blocked.is_finished(), "A is still inside its flush");

        gate.release();
        assert!(blocked.await.unwrap().enqueued);
    });
}

/// Lets a held flush go even when an assertion fails first.
struct Release(Arc<Gate>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// The last-message checkpoint flushes its chain and stays best-effort when a flush fails.
#[test]
fn checkpoint_publication_flushes_and_stays_best_effort() {
    if !PARENT_DIR_FSYNC_FLUSHES {
        return; // no directory flush to observe; `windows_contract` covers this platform
    }
    let env = RootEnv::new();
    let watch = Watch::new(env.root(), Plan::default());
    super::super::save_last_message_id("codex", 7_190_006, 41);
    let path = env.runtime().join("last_message/codex/7190006.txt");
    assert_eq!(
        flushes_of(&watch, &path, Step::ParentSync),
        vec![(true, Some("41".into()))]
    );
    let runtime_dir = env.runtime();
    assert!(watch.flushed(Step::Ancestors).contains(&runtime_dir));
    drop(watch);

    let plan = Plan {
        fail: vec![Step::ParentSync],
        ..Plan::default()
    };
    let _failing = Watch::new(env.root(), plan);
    super::super::save_last_message_id("codex", 7_190_006, 42);
    assert_eq!(fs::read_to_string(&path).unwrap(), "42");
}

/// The completed-turn ledger keeps its own single parent sync and adds only the directories
/// above it, so no directory is flushed twice.
#[test]
fn existing_durable_writers_do_not_double_flush() {
    if !PARENT_DIR_FSYNC_FLUSHES {
        return; // no directory flush to observe; `windows_contract` covers this platform
    }
    let env = RootEnv::new();
    let watch = Watch::new(env.root(), Plan::default());
    let append = |user_msg_id| {
        crate::services::discord::outbound::completed_turn_ledger::append_before_publish_for_tests(
            7_190_007,
            user_msg_id,
            "n",
            1,
            || {},
        )
        .unwrap()
    };
    append(1);
    let ledger_root = env.runtime().join("discord_completed_turn_ledger");
    assert_eq!(
        watch.flushed(Step::Ancestors),
        vec![ledger_root, env.runtime()]
    );
    assert_eq!(
        watch.count(Step::ParentOpen) + watch.count(Step::ParentSync),
        0
    );
    append(2);
    assert_eq!(
        watch.count(Step::Ancestors),
        2,
        "steady appends add no flush"
    );
}

#[path = "durable_stores_tests.rs"]
mod stores;
