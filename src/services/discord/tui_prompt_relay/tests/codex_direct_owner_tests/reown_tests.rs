//! The real Codex idle loop on a turn the hook wiring delegated to a watcher that then died:
//! the loop re-owns it, and a reader failing before any content leaves it for the repair.
use super::*;
use crate::services::tui_prompt_dedupe::ObservedTuiPrompt;
use synthetic_start::non_bridge_claim::HANDOFF_PAUSE;

const ANCHOR: u64 = 5_808_100_901;

fn content(body: &str) -> String {
    let value = serde_json::from_str::<serde_json::Value>(body).unwrap_or_default();
    value["content"].as_str().unwrap_or_default().to_string()
}

/// How many times the answer was accepted as a message's whole content. It reads the
/// accepted-request history, so the same final edit accepted twice is two deliveries.
fn answers(sent: &[(String, String, String)]) -> usize {
    let finals = sent.iter().filter(|(_, _, body)| content(body) == RESPONSE);
    finals.count()
}

fn run<F: std::future::Future<Output = ()>>(scenario: impl FnOnce(PathBuf) -> F) {
    let _env_lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let root = tempfile::tempdir().expect("isolated root");
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(scenario(root.path().to_path_buf()));
    for slot in [
        &HANDOFF_PAUSE,
        &super::super::super::codex_idle_rollout::CAPTURE_PAUSE,
    ] {
        *slot.lock().unwrap_or_else(|poison| poison.into_inner()) = None;
    }
}

fn arm(
    slot: &synthetic_start::bridge_handoff::reown::PauseSlot,
    channel: ChannelId,
) -> (Arc<Notify>, Arc<Notify>) {
    let (entered, resume) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    *slot.lock().unwrap_or_else(|poison| poison.into_inner()) =
        Some((channel.get(), entered.clone(), resume.clone()));
    (entered, resume)
}

async fn reached(entered: &Notify, what: &str) {
    let reached = tokio::time::timeout(Duration::from_secs(20), entered.notified()).await;
    assert!(reached.is_ok(), "the idle loop never reached {what}");
}

/// One channel whose live watcher reads the rollout itself, the real idle loop running, and
/// the turn it produces: the loop observes a typed prompt with no row yet and parks at its
/// hand-off, the hook wiring claims and delegates the turn, and the watcher dies.
struct Delegated {
    shared: Arc<SharedData>,
    codex: CodexChannel,
    requests: Requests,
    turn_id: String,
    session_key: Option<String>,
    resume: Arc<Notify>,
    _held: (
        crate::services::tui_o::cutover::test_override::ChannelsGuard,
        crate::services::discord::shared_state::test_rest::Guard,
        AbortOnDrop,
    ),
}

impl Delegated {
    async fn new(root: &Path, channel: u64, tmux: &str) -> Self {
        // An empty writer list keeps the channel on the Legacy relay this path serves.
        let boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
        let shared = crate::services::discord::make_shared_data_for_tests();
        let codex = CodexChannel::new(&shared, root, channel, tmux);
        let watched = codex.rollout.to_str().unwrap();
        (shared.tmux_watchers).insert(codex.channel, live_watcher(tmux, watched));
        let (requests, http, server) = recording_discord(channel).await;
        let rest = crate::services::discord::shared_state::test_rest::install(http);
        let mut prompts = crate::services::tui_prompt_dedupe::subscribe_observed_prompts();
        let (entered, resume) = arm(&HANDOFF_PAUSE, codex.channel);
        super::super::super::CODEX_IDLE_ROLLOUT_RELAY_STARTED.store(false, Ordering::Release);
        super::super::super::spawn_codex_idle_rollout_relay(shared.clone());
        append(&codex.rollout, &user_line());
        reached(&entered, "the hand-off").await;
        assert!(codex.row().is_none(), "the loop came through with no row");
        let observed = loop {
            let prompt = prompts.try_recv().expect("the loop published its prompt");
            if prompt.tmux_session_name == tmux {
                break prompt;
            }
        };
        let observer =
            crate::services::tui_prompt_dedupe::external_input_relay_lease("codex", tmux, channel)
                .expect("the loop's lease");
        let turn_id = Self::wire(&shared, &codex, &observed).await;
        assert_eq!(Some(&turn_id), observer.turn_id.as_ref(), "one turn");
        let row = codex.row().expect("claimed row");
        assert_ne!(
            row.effective_relay_owner_kind(),
            RelayOwnerKind::None,
            "the claim delegated the turn"
        );
        let watcher = shared.tmux_watchers.by_tmux_session.get(tmux);
        (watcher.expect("watcher").last_heartbeat_ts_ms).store(1, Ordering::Release);
        Self {
            shared,
            requests,
            turn_id,
            session_key: row.session_key,
            resume,
            codex,
            _held: (boot, rest, server),
        }
    }

    /// The hook relay's half on the event the loop published; returns the turn it claimed.
    async fn wire(
        shared: &Arc<SharedData>,
        codex: &CodexChannel,
        observed: &ObservedTuiPrompt,
    ) -> String {
        let mut hook_lease =
            super::super::super::relay_ownership::record_observed_external_turn_lease(
                shared,
                observed,
                codex.channel,
            );
        let decision = super::super::super::observed_prompt_decision::relay_observed_prompt_injected_prompt_decision(&observed.prompt);
        let deferred = synthetic_start_wiring::wire_tui_direct_synthetic_turn_start(
            shared,
            "codex",
            codex.channel,
            observed,
            MessageId::new(ANCHOR),
            false,
            &decision,
            &mut hook_lease,
        )
        .await;
        assert!(!deferred, "no prior turn, so the claim runs inline");
        hook_lease.turn_id.expect("hook turn id")
    }

    /// The answer reached Discord as one final edit of the claimed anchor.
    fn assert_delivered_once(&self) {
        let sent = deliveries(&self.requests, self.codex.channel);
        assert_eq!(answers(&sent), 1, "one answer: {sent:?}");
        assert!(
            sent.len() <= 2,
            "at most a progress edit before it: {sent:?}"
        );
        let anchor = format!("/channels/{}/messages/{ANCHOR}", self.codex.channel.get());
        for (method, path, _) in &sent {
            assert!(method == "PATCH" && path.ends_with(&anchor), "{sent:?}");
        }
    }

    async fn answered(&self) {
        let delivered = wait_for(Duration::from_secs(25), || {
            !deliveries(&self.requests, self.codex.channel).is_empty()
        });
        assert!(
            delivered.await,
            "the answer never reached Discord: {:?}",
            self.requests.lock().unwrap()
        );
        // Several polls past the delivery: no second answer, tail or repair.
        tokio::time::sleep(Duration::from_secs(3)).await;
        self.assert_delivered_once();
    }
}

/// The loop's own refused turn is re-owned and its answer is delivered once, by the one tail
/// the loop starts under the lease the re-own stored.
#[test]
fn codex_idle_loop_reowns_a_turn_whose_watcher_died_and_delivers_it_once() {
    run(|root| async move {
        let turn = Delegated::new(&root, 5_808_100_100, "AgentDesk-codex-5808-reown").await;
        append(&turn.codex.rollout, &answer_lines());
        turn.resume.notify_one();
        turn.answered().await;
        assert_eq!(tail_starts(turn.codex.channel), 1, "one tail");
        assert!(
            turn.codex.row().is_none(),
            "the delivered turn's row is cleared"
        );
        let lease = crate::services::tui_prompt_dedupe::external_input_relay_lease(
            "codex",
            &turn.codex.tmux,
            turn.codex.channel.get(),
        );
        assert_eq!(
            lease, None,
            "the tail released the lease it was started under"
        );
    });
}

/// The re-owned turn's reader cannot open the rollout, so it fails before any content. The row,
/// its turn and its mailbox actor stay, and once the rollout is readable the ownerless repair
/// resumes that same turn and delivers the answer once, with no second prompt.
#[test]
fn codex_reader_failure_before_content_leaves_the_reowned_turn_for_the_repair() {
    use std::os::unix::fs::PermissionsExt;
    run(|root| async move {
        let turn = Delegated::new(&root, 5_808_100_200, "AgentDesk-codex-5808-reader").await;
        let (channel, rollout) = (turn.codex.channel, &turn.codex.rollout);
        append(rollout, &answer_lines());
        let mode = |mode| std::fs::set_permissions(rollout, std::fs::Permissions::from_mode(mode));
        mode(0o000).expect("unreadable rollout");
        turn.resume.notify_one();
        let failed = wait_for(Duration::from_secs(15), || tail_starts(channel) == 1);
        assert!(failed.await, "the loop never tailed the re-owned turn");
        // Past the failed reader and several repair polls on the unreadable rollout.
        tokio::time::sleep(Duration::from_secs(3)).await;
        let row = turn.codex.row().expect("the undelivered row is kept");
        assert_eq!(row.effective_relay_owner_kind(), RelayOwnerKind::None);
        assert_eq!(row.external_turn_id.as_deref(), Some(turn.turn_id.as_str()));
        assert_eq!(row.user_msg_id, ANCHOR);
        let mailbox = crate::services::discord::mailbox_snapshot(&turn.shared, channel).await;
        assert_eq!(
            mailbox.active_user_message_id,
            Some(MessageId::new(ANCHOR)),
            "the turn's mailbox actor is kept"
        );
        assert!(deliveries(&turn.requests, channel).is_empty());

        let capture_pause = &super::super::super::codex_idle_rollout::CAPTURE_PAUSE;
        let (capturing, capture) = arm(capture_pause, channel);
        mode(0o644).expect("readable rollout");
        reached(&capturing, "the repair tail's capture").await;
        let row = turn.codex.row().expect("row under repair");
        let lease = crate::services::tui_prompt_dedupe::external_input_relay_lease(
            "codex",
            &turn.codex.tmux,
            channel.get(),
        )
        .expect("repair lease");
        assert_eq!(
            (row.external_turn_id.as_deref(), lease.turn_id.as_deref()),
            (Some(turn.turn_id.as_str()), Some(turn.turn_id.as_str())),
            "the repair resumes the original turn"
        );
        assert_eq!(
            (&row.session_key, &lease.session_key),
            (&turn.session_key, &turn.session_key)
        );
        assert_eq!(row.user_msg_id, ANCHOR, "the same episode, not a successor");
        capture.notify_one();
        turn.answered().await;
        assert_eq!(
            tail_starts(channel),
            2,
            "the failed tail and the repair tail"
        );
    });
}

/// The delivery oracle counts accepted requests, not the message's final state: a rejected POST
/// is no delivery, and one answer accepted twice with the same body is two.
#[test]
fn the_delivery_oracle_counts_accepted_answers_not_the_final_message() {
    run(|_root| async move {
        let channel = ChannelId::new(5_808_100_300);
        let rejected = Arc::new(AtomicUsize::new(1));
        let (requests, http, _server) = recording_discord_failing(channel.get(), rejected).await;
        let sent = || answers(&deliveries(&requests, channel));
        let post = serenity::CreateMessage::new().content(RESPONSE);
        assert!(channel.send_message(&*http, post).await.is_err());
        assert_eq!(sent(), 0, "a rejected attempt is not a delivery");
        for accepted in 1..=2 {
            let edit = serenity::EditMessage::new().content(RESPONSE);
            let edited = channel.edit_message(&*http, MessageId::new(ANCHOR), edit);
            edited.await.expect("accepted edit");
            assert_eq!(sent(), accepted, "each accepted answer counts");
        }
    });
}
