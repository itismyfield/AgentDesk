use std::path::PathBuf;
use std::sync::MutexGuard;
use std::sync::atomic::Ordering::{Relaxed, SeqCst};
use std::time::Duration;

use filetime::{FileTime, set_file_mtime};
use poise::serenity_prelude::{MessageId, UserId};

use super::PaneProof::{Missing, NotProven, Proven, Unmeasured};
use super::Reason::*;
use super::Verdict::*;
use super::*;
use crate::config::TestEnvVarGuard;
use crate::config::test_env_lock::{SharedTestEnvLockGuard, acquire_shared_test_env_lock};
use crate::services::discord::outbound::delivery_record::{
    ConfirmedDeliveryReceipt as Receipt, ExactJsonlSourceIdentity,
};
use crate::services::discord::relay_recovery::tests::orphan_token_finish::queued;
use crate::services::provider::CancelToken;
use crate::services::tmux_common::session_temp_path;

const PROVIDER: ProviderKind = ProviderKind::Codex;
const PAST_GRACE: Duration = Duration::from_secs(61);

type Seam = (SessionPresence, Option<bool>, usize, Option<PathBuf>);
static PANE_SEAMS: LazyLock<Mutex<HashMap<String, Seam>>> = LazyLock::new(Default::default);

fn seams() -> MutexGuard<'static, HashMap<String, Seam>> {
    PANE_SEAMS.lock().unwrap_or_else(|error| error.into_inner())
}

/// Replaces only the two inputs of `pane_proof_from`; `None` runs the production probe.
pub(super) fn pane_seam(session: &str) -> Option<(SessionPresence, Option<bool>)> {
    let mut seams = seams();
    let seam = seams.get_mut(session)?;
    seam.2 += 1;
    if let Some(row) = seam.3.take() {
        write_row(&row);
    }
    Some((seam.0, seam.1))
}

fn set_pane(session: &str, presence: SessionPresence, proven_idle: Option<bool>) {
    let calls = seams().get(session).map_or(0, |seam| seam.2);
    seams().insert(session.to_owned(), (presence, proven_idle, calls, None));
}

fn write_row(row: &Path) {
    std::fs::create_dir_all(row.parent().unwrap()).unwrap();
    std::fs::write(row, "{torn").unwrap();
}

struct Episode {
    registry: super::super::super::HealthRegistry,
    shared: Arc<SharedData>,
    token: Arc<CancelToken>,
    channel: ChannelId,
    anchor: MessageId,
    session: String,
    _guards: (TestEnvVarGuard, tempfile::TempDir, SharedTestEnvLockGuard),
}

impl Drop for Episode {
    fn drop(&mut self) {
        grace_reset(&self.key());
        seams().remove(&self.session);
        #[cfg(unix)]
        if let Some(root) = discord::inflight::inflight_runtime_root() {
            use std::os::unix::fs::PermissionsExt;
            let open = std::fs::Permissions::from_mode(0o755);
            let _ = std::fs::set_permissions(root.join(PROVIDER.as_str()), open);
        }
    }
}

impl Episode {
    fn key(&self) -> GraceKey {
        (PROVIDER.as_str().to_owned(), self.channel.get())
    }

    fn grace_armed(&self) -> bool {
        GRACE.lock().unwrap().contains_key(&self.key())
    }

    fn pane_calls(&self) -> usize {
        seams().get(&self.session).map_or(0, |seam| seam.2)
    }

    /// Writes the receipt (and the ledger entry) through the production writers.
    fn deliver(&self, nonce: &str, ledger: bool) {
        let generation = session_temp_path(&self.session, "generation");
        std::fs::write(&generation, "g").unwrap();
        set_file_mtime(&generation, FileTime::from_unix_time(1_800_599_600, 1)).unwrap();
        let source = ExactJsonlSourceIdentity {
            provider: PROVIDER.as_str().into(),
            tmux_session_name: self.session.clone(),
            turn_nonce: nonce.into(),
            range: (0, 64),
            generation_mtime_ns: delivery_record::current_generation_mtime_ns(&self.session),
            offset_authority_channel_id: self.channel.get(),
            delivery_channel_id: self.channel.get(),
        };
        delivery_record::record_current_pinned_delivery(&source, 5_996_000_001).unwrap();
        if ledger {
            delivery_record::record_pinned_delivery_metadata(&source, "answer", self.anchor.get());
        }
    }

    /// Restores the same anchor under a successor token with a fresh nonce.
    async fn restore_successor(&self) -> Arc<CancelToken> {
        let successor = Arc::new(CancelToken::new());
        successor.bind_unmanaged_session_name(&self.session);
        let mailbox = self.shared.mailbox(self.channel);
        let restore = mailbox.restore_active_turn(successor.clone(), UserId::new(7), self.anchor);
        restore.await;
        successor
    }

    async fn pass(&self) -> Verdict {
        reconcile_channel(&PROVIDER, &self.shared, self.channel).await
    }

    /// Pass #1, then pass #2 past the grace.
    async fn two_passes(&self) -> (Verdict, Verdict) {
        let first = self.pass().await;
        tokio::time::advance(PAST_GRACE).await;
        (first, self.pass().await)
    }

    async fn assert_inert(&self, owner: &Arc<CancelToken>) {
        let after = discord::mailbox_snapshot(&self.shared, self.channel).await;
        let kept = after.cancel_token.is_some_and(|t| Arc::ptr_eq(&t, owner));
        let queue = after.intervention_queue.len();
        let global = self.shared.restart.global_active.load(Relaxed);
        let cancelled = owner.cancelled.load(Relaxed);
        let state = (kept, after.active_user_message_id, queue, global, cancelled);
        assert_eq!(state, (true, Some(self.anchor), 3, 1, false));
    }
}

/// A rowless, aged anchor (M, N) on session S with three queued messages.
async fn seed(channel: u64, token: CancelToken) -> Episode {
    let lock = acquire_shared_test_env_lock();
    let root = tempfile::tempdir().unwrap();
    let env =
        TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", root.path());
    let session = format!("AgentDesk-codex-l1b-{channel}-{}", std::process::id());
    let (channel, anchor) = (ChannelId::new(channel), MessageId::new(channel + 9));
    let registry = super::super::super::HealthRegistry::new();
    let shared = discord::make_shared_data_for_tests();
    registry.register("codex".into(), shared.clone()).await;
    let token = Arc::new(token);
    token.bind_unmanaged_session_name(&session);
    let owner = UserId::new(7);
    assert!(discord::mailbox_try_start_turn(&shared, channel, token.clone(), owner, anchor).await);
    for id in 1..=3 {
        let queued = queued(anchor.get() + id);
        discord::mailbox_enqueue_intervention(&shared, &PROVIDER, channel, queued).await;
    }
    let age = Duration::from_secs(738);
    shared.mailbox(channel).age_active_turn_for_test(age).await;
    shared.restart.global_active.store(1, Relaxed);
    set_pane(&session, SessionPresence::Present, Some(true));
    let _guards = (env, root, lock);
    Episode {
        registry,
        shared,
        token,
        channel,
        anchor,
        session,
        _guards,
    }
}

#[tokio::test(start_paused = true)]
async fn delivered_rowless_anchor_defers_then_would_release_without_mutation() {
    let episode = seed(5_996_701, CancelToken::new()).await;
    episode.deliver(episode.token.turn_nonce().unwrap(), true);
    let runtimes = [episode.shared.clone()];
    let heal = super::super::super::relay_auto_heal::run_orphan_token_auto_heal_pass;
    heal(&episode.registry, &PROVIDER, &runtimes).await;
    assert!(episode.grace_armed(), "the health pass reaches the verdict");
    assert_eq!(episode.pass().await, Defer);
    assert_eq!(episode.pane_calls(), 0, "no pane probe before the grace");
    tokio::time::advance(PAST_GRACE).await;
    assert_eq!(episode.pass().await, Release);
    assert_eq!(episode.pane_calls(), 1);
    episode.assert_inert(&episode.token).await;
}

#[derive(Clone, Copy, Debug)]
enum Flip {
    NoLedger,
    MalformedRecord,
    PredecessorNonce,
    RowPresent,
    RowUnmeasurable,
    LegacyNonce,
    RestoredNonce,
}

/// Applies one flip to the seeded shape; returns the token that must survive, or None to skip.
async fn flip_one(episode: &Episode, flip: Flip) -> Option<Arc<CancelToken>> {
    let nonce = episode.token.turn_nonce().unwrap_or("legacy");
    let predecessor = matches!(flip, Flip::PredecessorNonce);
    let ledger = !predecessor && !matches!(flip, Flip::NoLedger);
    episode.deliver(if predecessor { "nonce-a" } else { nonce }, ledger);
    let channel = episode.channel.get();
    let root = discord::inflight::inflight_runtime_root().unwrap();
    match flip {
        Flip::MalformedRecord => {
            let record = delivery_record::delivery_record_path(&PROVIDER, channel).unwrap();
            std::fs::write(record, "{\"confirmed_deliveries\": [").unwrap();
        }
        Flip::PredecessorNonce => {
            let ledger = completed_turn_ledger::ledger_path(&PROVIDER, channel).unwrap();
            std::fs::create_dir_all(ledger.parent().unwrap()).unwrap();
            let (id, future) = (episode.anchor.get(), u64::MAX / 2);
            let entry = format!(
                r#"{{"entries":[{{"user_msg_id":{id},"committed_at_epoch_ms":{future}}}]}}"#
            );
            std::fs::write(ledger, entry).unwrap();
        }
        Flip::RowPresent => write_row(&inflight_state_path(&root, &PROVIDER, channel)),
        #[cfg(unix)]
        Flip::RowUnmeasurable => {
            use std::os::unix::fs::PermissionsExt;
            let dir = root.join(PROVIDER.as_str());
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
            if std::fs::read_dir(&dir).is_ok() {
                return None;
            }
        }
        #[cfg(not(unix))]
        Flip::RowUnmeasurable => return None,
        Flip::RestoredNonce => {
            let before = discord::mailbox_snapshot(&episode.shared, episode.channel).await;
            let successor = episode.restore_successor().await;
            let after = discord::mailbox_snapshot(&episode.shared, episode.channel).await;
            assert_eq!(after.turn_started_at, before.turn_started_at);
            assert_ne!(after.active_turn_nonce, before.active_turn_nonce);
            return Some(successor);
        }
        Flip::NoLedger | Flip::LegacyNonce => {}
    }
    Some(episode.token.clone())
}

#[tokio::test(start_paused = true)]
async fn each_single_flip_of_the_delivered_shape_keeps_the_anchor() {
    let cases = [
        (Flip::NoLedger, Refuse(NoLedgerWitness)),
        (Flip::MalformedRecord, Refuse(ReceiptUnreadable)),
        (Flip::PredecessorNonce, Refuse(NoEpisodeReceipt)),
        (Flip::RowPresent, NotApplicable),
        (Flip::RowUnmeasurable, Refuse(UnmeasuredRow)),
        (Flip::LegacyNonce, NotApplicable),
        (Flip::RestoredNonce, Refuse(NoEpisodeReceipt)),
    ];
    for (index, (flip, expected)) in cases.into_iter().enumerate() {
        let token = match flip {
            Flip::LegacyNonce => CancelToken::from_persisted_turn_nonce(None),
            _ => CancelToken::new(),
        };
        let episode = seed(5_996_720 + index as u64, token).await;
        let Some(owner) = flip_one(&episode, flip).await else {
            eprintln!("skipping {flip:?}: directory permissions are not enforced");
            continue;
        };
        assert_eq!(episode.two_passes().await, (expected, expected), "{flip:?}");
        episode.assert_inert(&owner).await;
    }
}

#[tokio::test(start_paused = true)]
async fn production_pane_probe_without_a_binding_holds() {
    let episode = seed(5_996_709, CancelToken::new()).await;
    episode.deliver(episode.token.turn_nonce().unwrap(), true);
    set_pane(&episode.session, SessionPresence::Present, None);
    assert_eq!(episode.two_passes().await, (Defer, Hold(PaneNotProvenIdle)));
    assert!(episode.grace_armed(), "a pane hold keeps the grace");
    episode.assert_inert(&episode.token).await;
}

#[tokio::test(start_paused = true)]
async fn a_row_written_during_the_pane_probe_blocks_the_release() {
    let episode = seed(5_996_711, CancelToken::new()).await;
    episode.deliver(episode.token.turn_nonce().unwrap(), true);
    let root = discord::inflight::inflight_runtime_root().unwrap();
    let row = inflight_state_path(&root, &PROVIDER, episode.channel.get());
    seams().get_mut(&episode.session).unwrap().3 = Some(row);
    assert_eq!(episode.two_passes().await, (Defer, NotApplicable));
    assert_eq!(episode.pane_calls(), 1);
    episode.assert_inert(&episode.token).await;
}

#[tokio::test(start_paused = true)]
async fn a_refusal_or_a_new_episode_restarts_the_grace() {
    let episode = seed(5_996_712, CancelToken::new()).await;
    let nonce = episode.token.turn_nonce().unwrap();
    episode.deliver(nonce, true);
    assert_eq!(episode.pass().await, Defer);
    let channel = episode.channel.get();
    std::fs::remove_file(completed_turn_ledger::ledger_path(&PROVIDER, channel).unwrap()).unwrap();
    tokio::time::advance(PAST_GRACE).await;
    assert_eq!(episode.pass().await, Refuse(NoLedgerWitness));
    assert!(!episode.grace_armed(), "a refusal ends the grace");
    episode.deliver(nonce, true);
    assert_eq!(episode.pass().await, Defer, "after a refusal");

    let successor = episode.restore_successor().await;
    episode.deliver(successor.turn_nonce().unwrap(), true);
    tokio::time::advance(PAST_GRACE).await;
    assert_eq!(episode.pass().await, Defer, "new (M, N)");
    tokio::time::advance(PAST_GRACE).await;
    assert_eq!(episode.pass().await, Release);
    episode.assert_inert(&successor).await;
}

#[tokio::test(start_paused = true)]
async fn nothing_is_decided_before_the_boot_reconcile_completes() {
    let episode = seed(5_996_710, CancelToken::new()).await;
    episode.deliver(episode.token.turn_nonce().unwrap(), true);
    episode.shared.restart.reconcile_done.store(false, SeqCst);
    let pending = Hold(BootReconcilePending);
    assert_eq!(episode.two_passes().await, (pending, pending));
    assert!(!episode.grace_armed());
    assert_eq!(episode.pane_calls(), 0);

    episode.shared.restart.reconcile_done.store(true, SeqCst);
    assert_eq!(episode.two_passes().await, (Defer, Release));
    episode.assert_inert(&episode.token).await;
}

#[test]
fn decide_orders_the_gates_and_maps_every_reason() {
    type Edit = fn(&mut DeliveredReleaseObservation);
    let evidence = DeliveredReleaseObservation {
        restart_pinned: false,
        session: Some("S".into()),
        row_present: Some(false),
        ledger_committed_at_ms: Some(0),
        record_readable: true,
        episode_receipt: true,
    };
    let edits: [(Edit, Verdict); 7] = [
        (|o| o.restart_pinned = true, NotApplicable),
        (|o| o.session = None, Refuse(PaneUnmeasured)),
        (|o| o.row_present = Some(true), NotApplicable),
        (|o| o.row_present = None, Refuse(UnmeasuredRow)),
        (|o| o.ledger_committed_at_ms = None, Refuse(NoLedgerWitness)),
        (|o| o.record_readable = false, Refuse(ReceiptUnreadable)),
        (|o| o.episode_receipt = false, Refuse(NoEpisodeReceipt)),
    ];
    for (edit, expected) in edits {
        let mut obs = evidence.clone();
        edit(&mut obs);
        assert_eq!(decide(&obs, true, Some(Proven)), expected, "{obs:?}");
    }
    // K14: the ledger time (0 here) is telemetry, so it never blocks a release.
    let panes = [
        (None, false, Defer),
        (Some(Proven), false, Defer),
        (Some(Proven), true, Release),
        (Some(Missing), true, Refuse(PaneMissing)),
        (Some(Unmeasured), true, Hold(PaneUnmeasured)),
        (Some(NotProven), true, Hold(PaneNotProvenIdle)),
        (None, true, Hold(PaneUnmeasured)),
    ];
    for (pane, grace_elapsed, expected) in panes {
        assert_eq!(decide(&evidence, grace_elapsed, pane), expected, "{pane:?}");
    }
}

#[test]
fn episode_receipt_names_exactly_this_nonce_session_and_channel() {
    let source = ExactJsonlSourceIdentity {
        provider: PROVIDER.as_str().into(),
        tmux_session_name: "S".into(),
        turn_nonce: "N".into(),
        range: (0, 64),
        generation_mtime_ns: 7,
        offset_authority_channel_id: 42,
        delivery_channel_id: 42,
    };
    let receipt = Receipt {
        source,
        delivery_channel_id: 42,
        message_id: 9,
    };
    let holds = |edit: fn(&mut Receipt)| {
        let mut record = DeliveryRecord::default();
        record.confirmed_deliveries.push(receipt.clone());
        edit(&mut record.confirmed_deliveries[0]);
        episode_receipt(&record, &PROVIDER, 42, "N", "S")
    };
    assert!(holds(|_| {}));
    let foreign: [fn(&mut Receipt); 10] = [
        |r| r.source.turn_nonce = "N2".into(),
        |r| r.source.turn_nonce = String::new(),
        |r| r.source.tmux_session_name = "S2".into(),
        |r| r.source.provider = "claude".into(),
        |r| r.source.offset_authority_channel_id = 43,
        |r| r.source.delivery_channel_id = 43,
        |r| r.delivery_channel_id = 43,
        |r| r.message_id = 0,
        |r| r.source.generation_mtime_ns = 0,
        |r| r.source.range = (64, 64),
    ];
    for (index, edit) in foreign.into_iter().enumerate() {
        assert!(!holds(edit), "foreign receipt #{index}");
    }
}

#[test]
fn row_presence_and_pane_proof_fail_closed() {
    let root = Some(Path::new("/r"));
    assert_eq!(row_presence_from(None, Ok(false)), None);
    assert_eq!(row_presence_from(root, Err(io::Error::other("stat"))), None);
    assert_eq!(row_presence_from(root, Ok(false)), Some(false));
    assert_eq!(row_presence_from(root, Ok(true)), Some(true));

    use SessionPresence as P;
    let unprobed = || -> bool { panic!("only a present pane is probed") };
    assert_eq!(pane_proof_from(P::Missing, unprobed), Missing);
    assert_eq!(pane_proof_from(P::ProbeFailed, unprobed), Unmeasured);
    assert_eq!(pane_proof_from(P::Present, || false), NotProven);
    assert_eq!(pane_proof_from(P::Present, || true), Proven);
}

/// Pins call count and adjacency of the pane probe, not its execution.
#[test]
fn pane_probe_is_the_destructive_grade_idle_proof() {
    let source = include_str!("delivered_release.rs");
    let boundary = ["#[cfg(test)]\n", "#[path"].concat();
    let production = &source[..source.find(&boundary).expect("test module boundary")];
    let lines: Vec<&str> = production.lines().collect();
    let line_of = |needle: &str| lines.iter().position(|line| line.contains(needle));

    let probe = ["tmux_turn_liveness::", "provider_session_is_proven_idle("].concat();
    assert_eq!(production.matches(&probe).count(), 1);
    let presence = line_of(&["session_", "presence("].concat()).expect("presence probe");
    let proof = line_of(&probe).expect("idle proof");
    assert!(
        proof > presence && proof - presence <= 10,
        "{presence}->{proof}"
    );
    for weaker in ["independent_tmux_", "jsonl_ready_", "tui_structurally_"] {
        assert!(!production.contains(weaker), "{weaker}");
    }
    let seam = ["pane_", "seam"].concat();
    for index in (1..lines.len()).filter(|&index| lines[index].contains(&seam)) {
        assert_eq!(lines[index - 1].trim(), "#[cfg(test)]", "{}", lines[index]);
    }
}
