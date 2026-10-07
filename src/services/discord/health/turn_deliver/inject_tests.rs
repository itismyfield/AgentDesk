//! Busy-turn injection behind the real deliver entry: switch, holder and backlog vetoes, the
//! mailbox schedules around them, and a scripted tmux standing in for the pane.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use super::inject::{self, InjectMode, test_hook};
use super::{HumanInputDelivery, HumanInputRequest, deliver_human_input};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::SharedData;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::inflight::{InflightTurnState, TurnSource};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::turn_orchestrator::{ActiveTurnKind, ChannelMailboxSnapshot, Intervention};

const BORDER: &str = "────────────────────────────────────────────────────────────";
const FOOTER: &str = "  ⏵⏵ bypass permissions on (shift+tab to cycle)";
const SPINNER: &str = "✻ Thinking… (12s · esc to interrupt)";
const BUSY_TURN: &str = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"go\"}}\n\
    {\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"working\"}]}}\n";

/// A scripted `tmux`: the composer folds the paste, and the Enter makes the transcript record
/// the pasted header the way Claude queues input typed during a turn. `gate` holds keys until `go`.
const FAKE_TMUX: &str = r#"#!/bin/sh
d='@D@'
echo "$*" >> "$d/log"
case "$2" in
display-message) cat "$d/attach" ;;
capture-pane) if [ -f "$d/pasted" ]; then cat "$d/cap.pasted"; else cat "$d/cap.before"; fi ;;
load-buffer) for last do :; done; cp "$last" "$d/buffer" ;;
if-shell)
  a=$(cat "$d/attach")
  if [ "$a" != 0 ]; then echo "agentdesk-busy-inject-vetoed $a"; exit 0; fi
  if [ -f "$d/gate" ]; then
    touch "$d/at_gate"; i=0
    while [ ! -f "$d/go" ] && [ $i -lt 80 ]; do sleep 0.05; i=$((i+1)); done
  fi
  [ -f "$d/fail_paste" ] && exit 1
  echo "$7" >> "$d/keys"
  case "$7" in
  paste-buffer*) touch "$d/pasted" ;;
  send-keys*) printf '{"type":"queue-operation","operation":"enqueue","content":"%s\\nstatus?","sessionId":"6245","timestamp":"2026-10-06T00:00:00.000Z"}\n' "$(head -n 1 "$d/buffer")" >> "$d/transcript.jsonl" ;;
  esac ;;
esac
exit 0
"#;

/// A scripted pane with a busy transcript, the TUI-direct row and Claude binding naming it, and
/// the switch forced for its channel until dropped.
pub(crate) struct InjectPane {
    dir: tempfile::TempDir,
    channel: u64,
}

impl InjectPane {
    pub(crate) fn new(channel: u64, mode: &str) -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/busy-inject-tmp");
        fs::create_dir_all(&root).unwrap();
        let pane = Self {
            dir: tempfile::tempdir_in(root).unwrap(),
            channel,
        };
        pane.set("transcript.jsonl", BUSY_TURN);
        pane.set("attach", "0");
        pane.set(
            "cap.before",
            &format!("⏺ Working on it.\n\n{SPINNER}\n\n{BORDER}\n❯\u{00a0}\n{BORDER}\n{FOOTER}"),
        );
        pane.fold_paste(1);
        let program = pane.path("tmux");
        pane.set(
            "tmux",
            &FAKE_TMUX.replace("@D@", &pane.dir.path().display().to_string()),
        );
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let session = pane.session();
        let mut row = InflightTurnState::new(
            ProviderKind::Claude,
            channel,
            None,
            0,
            0,
            0,
            "typed over ssh".to_string(),
            None,
            Some(session.clone()),
            None,
            None,
            0,
        );
        row.turn_source = TurnSource::ExternalInput;
        crate::services::discord::inflight::save_inflight_state_create_new(&row)
            .expect("external turn row");
        let binding = crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: pane.path("transcript.jsonl").display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        };
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(&session, binding);
        test_hook::set(channel, InjectMode::parse(Some(mode)), program);
        pane
    }

    /// The tmux name the channel name `inject-<channel>` resolves to.
    pub(crate) fn session(&self) -> String {
        ProviderKind::Claude.build_tmux_session_name(&format!("inject-{}", self.channel))
    }

    /// Rewrites the channel's row with another source and user message id.
    fn reseat_row(&self, source: TurnSource, message: u64) {
        let provider = ProviderKind::Claude;
        let load = crate::services::discord::inflight::load_inflight_state_read_only;
        let mut row = load(&provider, self.channel).expect("seeded row");
        (row.turn_source, row.user_msg_id) = (source, message);
        crate::services::discord::inflight::save_inflight_state(&row).expect("reseated row");
    }

    /// Leaves only the channel name to name the pane.
    async fn drop_row(&self, shared: &SharedData) {
        crate::services::discord::inflight::clear_inflight_state(
            &ProviderKind::Claude,
            self.channel,
        );
        let name = format!("inject-{}", self.channel);
        let map = crate::services::discord::host_defer_gate::tests::map_channel;
        map(shared, ChannelId::new(self.channel), &name).await;
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    pub(crate) fn set(&self, name: &str, value: &str) {
        fs::write(self.path(name), value).unwrap();
    }

    fn lines(&self, name: &str) -> Vec<String> {
        let text = fs::read_to_string(self.path(name)).unwrap_or_default();
        text.lines().map(str::to_string).collect()
    }

    /// The composer after the paste shows a folded placeholder of `lines` line breaks.
    pub(crate) fn fold_paste(&self, lines: usize) {
        let folded = format!("[Pasted text #1 +{lines} lines]");
        let pane =
            format!("⏺ Working on it.\n\n{SPINNER}\n\n{BORDER}\n❯ {folded}\n{BORDER}\n{FOOTER}");
        self.set("cap.pasted", &pane);
    }

    /// The frame the last paste carried.
    pub(crate) fn pasted(&self) -> String {
        fs::read_to_string(self.path("buffer")).unwrap_or_default()
    }

    pub(crate) fn tmux_calls(&self) -> usize {
        self.lines("log").len()
    }

    /// Paste and Enter commands the server applied.
    pub(crate) fn keys(&self) -> Vec<String> {
        let first_word = |line: String| line.split_whitespace().next().unwrap_or("").to_string();
        self.lines("keys").into_iter().map(first_word).collect()
    }

    /// Whether the transcript recorded the header the pasted buffer carried.
    pub(crate) fn transcript_recorded_the_paste(&self) -> bool {
        let header = self.lines("buffer").into_iter().next().unwrap_or_default();
        !header.is_empty()
            && self
                .lines("transcript.jsonl")
                .iter()
                .any(|line| line.contains(&header))
    }
}

impl Drop for InjectPane {
    fn drop(&mut self) {
        test_hook::clear(self.channel);
    }
}

/// A Claude bot on `channels`, owner 100 and allowed author 200, with `pool` for the host guard.
pub(crate) async fn register_inject_runtime(
    registry: &HealthRegistry,
    channels: &[u64],
    pool: Option<sqlx::PgPool>,
) -> Arc<SharedData> {
    let shared = crate::services::discord::make_shared_data_for_tests_with_storage(pool);
    {
        let mut settings = shared.settings.write().await;
        settings.owner_user_id = Some(100);
        settings.allowed_user_ids = vec![200];
        settings.allowed_channel_ids = channels.to_vec();
    }
    registry
        .register("claude".to_string(), shared.clone())
        .await;
    shared
}

/// `<delivery> <reason> [veto=..]` for compact expectations.
async fn deliver(registry: &HealthRegistry, channel: u64) -> String {
    let request = HumanInputRequest {
        channel_id: ChannelId::new(channel),
        provider: ProviderKind::Claude,
        text: "status?".to_string(),
        author_id: 200,
        source: "imessage".to_string(),
        metadata: None,
        channel_name_hint: None,
    };
    match deliver_human_input(registry, request).await {
        Ok(HumanInputDelivery::Queued {
            reason,
            inject_veto,
            ..
        }) => format!("queued {reason} veto={}", inject_veto.unwrap_or_default()),
        other => format!("{other:?}"),
    }
}

pub(crate) async fn queue_texts(shared: &SharedData, channel: u64) -> Vec<String> {
    let snapshot =
        crate::services::discord::mailbox_snapshot(shared, ChannelId::new(channel)).await;
    let queue = snapshot.intervention_queue.iter();
    queue.map(|item| item.text.clone()).collect()
}

/// The intake's own state for a head it claimed, as PR1's front requeue rebuilds it.
fn intake_state(channel: u64, message: u64, token: Option<&CancelToken>) -> InflightTurnState {
    let mut state = InflightTurnState::new(
        ProviderKind::Claude,
        channel,
        None,
        7,
        message,
        0,
        "earlier input".to_string(),
        None,
        None,
        None,
        None,
        0,
    );
    state.turn_nonce = token.and_then(|token| token.turn_nonce().map(str::to_owned));
    state.busy_followup_retry_user_msg_id = message;
    state.set_followup_requeue_context(None, false, false, Vec::new(), None, true);
    state
}

/// Intake's claim of its head, with the user id and message the requeue later names.
async fn claim(shared: &SharedData, channel: u64) -> Arc<CancelToken> {
    claim_kinded(shared, channel, ActiveTurnKind::UserOrAgent).await
}

/// A claim of `kind` on message `channel + 10`, as intake, a TUI-direct relay or a monitor takes it.
async fn claim_kinded(shared: &SharedData, channel: u64, kind: ActiveTurnKind) -> Arc<CancelToken> {
    let token = Arc::new(CancelToken::new());
    let (user, message) = (UserId::new(7), MessageId::new(channel + 10));
    let start = crate::services::discord::mailbox_try_start_turn_kinded;
    let channel = ChannelId::new(channel);
    assert!(start(shared, channel, token.clone(), user, message, kind).await);
    token
}

fn queued(message: u64) -> Intervention {
    let generation = crate::services::discord::runtime_store::process_generation();
    let id = MessageId::new(message);
    Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: id,
        queued_generation: generation,
        source_message_ids: vec![id],
        source_message_queued_generations: vec![
            crate::services::turn_orchestrator::SourceMessageQueuedGeneration::user_instruction(
                id, generation,
            ),
        ],
        source_text_segments: Vec::new(),
        text: "earlier input".to_string(),
        mode: crate::services::turn_orchestrator::InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

#[test]
fn the_switch_opens_only_on_external_or_all() {
    assert_eq!(inject::INJECT_ENV, "ADK_BUSY_INJECT");
    #[rustfmt::skip]
    let cases = [(None, InjectMode::Off), (Some(""), InjectMode::Off), (Some("on"), InjectMode::Off),
        (Some("External"), InjectMode::Off), (Some("external"), InjectMode::External),
        (Some(" all\n"), InjectMode::All), (Some("off"), InjectMode::Off)];
    for (value, mode) in cases {
        assert_eq!(InjectMode::parse(value), mode, "{value:?}");
    }
}

#[test]
fn any_holder_takes_input_unless_a_claimed_input_has_not_reached_its_row() {
    let row = |source, message| {
        let mut row = intake_state(9, message, None);
        row.turn_source = source;
        row
    };
    let (external, managed) = (
        row(TurnSource::ExternalInput, 0),
        row(TurnSource::Managed, 5),
    );
    let (monitor, adopted) = (
        row(TurnSource::MonitorTriggered, 0),
        row(TurnSource::ExternalAdopted, 0),
    );
    let stale = row(TurnSource::Managed, 6);
    let claim = |kind| ChannelMailboxSnapshot {
        cancel_token: Some(Arc::new(CancelToken::new())),
        active_user_message_id: Some(MessageId::new(5)),
        active_turn_kind: kind,
        ..ChannelMailboxSnapshot::default()
    };
    let (idle, turn) = (
        ChannelMailboxSnapshot::default(),
        claim(ActiveTurnKind::UserOrAgent),
    );
    let (background, monitor_turn) = (
        claim(ActiveTurnKind::Background),
        claim(ActiveTurnKind::MonitorAutoTurn),
    );
    let discord = || Ok(Some("discord:9:5".to_string()));
    let in_flight = || Err(inject::INPUT_IN_FLIGHT);
    #[rustfmt::skip]
    let cases = [
        (&idle, Some(&external), Ok(None)),
        (&idle, Some(&monitor), Ok(None)),
        (&idle, Some(&adopted), Ok(None)),
        (&idle, Some(&managed), discord()),
        (&idle, None, Ok(None)),
        (&turn, Some(&managed), discord()),
        (&background, Some(&external), Ok(None)),
        (&background, Some(&managed), discord()),
        (&background, None, Ok(None)),
        (&monitor_turn, Some(&monitor), Ok(None)),
        // A claimed input whose own row is not on disk has not reached the pane yet.
        (&turn, Some(&external), in_flight()),
        (&turn, Some(&stale), in_flight()),
        (&turn, None, in_flight()),
    ];
    for (index, (snapshot, row, expected)) in cases.into_iter().enumerate() {
        let holder = inject::holder(snapshot, row, 9).map(|holder| holder.turn_id);
        assert_eq!(holder, expected, "case {index}");
    }
    let queued_item = ChannelMailboxSnapshot {
        intervention_queue: vec![queued(11)],
        ..ChannelMailboxSnapshot::default()
    };
    let reserved = ChannelMailboxSnapshot {
        pending_user_dispatch: Some(MessageId::new(11)),
        ..ChannelMailboxSnapshot::default()
    };
    let backlog = [&idle, &queued_item, &reserved].map(inject::backlog);
    let nonempty = Err(inject::QUEUE_NONEMPTY);
    assert_eq!(backlog, [Ok(()), nonempty, nonempty]);
}

/// Each schedule leaves earlier input ahead: queued, dequeued before its claim, claimed over the
/// row, or paused between the handback's claim release and its front requeue on a busy pane.
#[tokio::test(flavor = "current_thread")]
async fn input_queued_reserved_or_claimed_before_a_deliver_stays_ahead_of_it_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let channels = [6_245_401, 6_245_402, 6_245_403, 6_245_404];
    let registry = HealthRegistry::new();
    let shared = register_inject_runtime(&registry, &channels, Some(pool)).await;
    let provider = ProviderKind::Claude;
    let [requeued, reserved, claimed, released] = channels.map(|ch| InjectPane::new(ch, "all"));
    let earlier = |ch: u64| intake_state(ch, ch + 10, None);
    let requeue = crate::services::discord::mailbox_requeue_inflight_for_followup_retry;

    let channel = ChannelId::new(channels[0]);
    assert!(
        requeue(&shared, &provider, channel, &earlier(channels[0]))
            .await
            .enqueued
    );
    let after_requeue = deliver(&registry, channels[0]).await;

    let channel = ChannelId::new(channels[1]);
    let enqueue = crate::services::discord::mailbox_enqueue_intervention;
    let head = queued(channels[1] + 10);
    assert!(enqueue(&shared, &provider, channel, head).await.enqueued);
    let take = crate::services::discord::idle_queue_take_next_soft_if_ready;
    let taken = take(&shared, &provider, channel).await.into_intervention();
    let (_head, _, _lease) = taken.expect("dequeued head");
    let after_dequeue = deliver(&registry, channels[1]).await;

    let _held = claim(&shared, channels[2]).await;
    let while_claimed = deliver(&registry, channels[2]).await;

    let token = claim(&shared, channels[3]).await;
    let state = intake_state(channels[3], channels[3] + 10, Some(&token));
    let (reached, resume) = crate::services::discord::live_bridge::handback_gap::arm(channels[3]);
    let handback = tokio::spawn({
        let (shared, provider) = (shared.clone(), provider.clone());
        async move {
            let defer = crate::services::discord::live_bridge::defer_unstarted_turn;
            defer(&shared, &provider, &state, &token, true, "test_handback").await
        }
    });
    reached.notified().await;
    let after_release = deliver(&registry, channels[3]).await;
    resume.notify_one();
    assert!(handback.await.expect("handback"));

    let mut observed = Vec::new();
    for (pane, outcome) in [
        (&requeued, after_requeue),
        (&reserved, after_dequeue),
        (&claimed, while_claimed),
        (&released, after_release),
    ] {
        let queue = queue_texts(&shared, pane.channel).await.join(",");
        let (calls, keys) = (pane.tmux_calls(), pane.keys().len());
        observed.push(format!("{outcome} [{queue}] tmux={calls} keys={keys}"));
    }
    assert_eq!(
        observed,
        [
            "queued external_turn_active veto=queue_nonempty [earlier input,status?] tmux=0 keys=0",
            "queued external_turn_active veto=queue_nonempty [status?] tmux=0 keys=0",
            "queued turn_active veto=input_in_flight [status?] tmux=0 keys=0",
            "queued external_turn_active veto=transition_busy [earlier input,status?] tmux=0 keys=0",
        ]
    );
}

/// Background, monitor, adopted and row-less holders of a busy pane all take the input through
/// the real deliver entry, with nothing queued.
#[tokio::test(flavor = "current_thread")]
async fn a_busy_pane_takes_the_input_whoever_holds_the_channel_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let channels = [6_245_701, 6_245_702, 6_245_703, 6_245_704];
    let registry = HealthRegistry::new();
    let shared = register_inject_runtime(&registry, &channels, Some(pool)).await;
    let [background, monitor, adopted, rowless] = channels.map(|ch| InjectPane::new(ch, "all"));
    claim_kinded(&shared, background.channel, ActiveTurnKind::Background).await;
    monitor.reseat_row(TurnSource::MonitorTriggered, 0);
    claim_kinded(&shared, monitor.channel, ActiveTurnKind::MonitorAutoTurn).await;
    adopted.reseat_row(TurnSource::ExternalAdopted, 0);
    rowless.drop_row(&shared).await;
    let mut observed = Vec::new();
    for pane in [&background, &monitor, &adopted, &rowless] {
        let outcome = deliver(&registry, pane.channel).await;
        let queue = queue_texts(&shared, pane.channel).await.join(",");
        let (keys, seen) = (pane.keys().join("+"), pane.transcript_recorded_the_paste());
        observed.push(format!("{outcome} keys={keys} seen={seen} [{queue}]"));
    }
    let injected = "Ok(Injected { turn_id: None }) keys=paste-buffer+send-keys seen=true []";
    assert_eq!(observed, [injected; 4]);
}

/// A channel whose input moved to the input runtime keeps its pane untouched.
#[tokio::test(flavor = "current_thread")]
async fn a_channel_closed_to_legacy_input_takes_no_paste() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let ch = 6_245_801;
    let shared = crate::services::discord::make_shared_data_for_tests();
    let pane = InjectPane::new(ch, "all");
    let fence = crate::services::discord::input_runtime::fence::Gate::protect;
    let gate = fence(ProviderKind::Claude, ch).expect("gate");
    let _closing = gate.close().expect("closing");
    let request = HumanInputRequest {
        channel_id: ChannelId::new(ch),
        provider: ProviderKind::Claude,
        text: "status?".to_string(),
        author_id: 200,
        source: "imessage".to_string(),
        metadata: None,
        channel_name_hint: None,
    };
    let outcome = inject::attempt(&shared, &request).await;
    let refused = inject::InjectAttempt::NotSent("input_runtime_owned");
    assert_eq!((outcome, pane.tmux_calls()), (refused, 0));
}

/// A row stamped with another runtime stops the attempt before the session-transition guard
/// or any pane I/O: a held transition does not turn it into `transition_busy`.
#[tokio::test(flavor = "current_thread")]
async fn a_session_of_another_runtime_stops_before_the_transition_and_the_pane() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let ch = 6_245_802;
    let shared = crate::services::discord::make_shared_data_for_tests();
    let pane = InjectPane::new(ch, "all");
    let load = crate::services::discord::inflight::load_inflight_state_read_only;
    let mut row = load(&ProviderKind::Claude, ch).expect("seeded row");
    row.runtime_kind = Some(RuntimeHandoffKind::LegacyTmuxWrapper);
    crate::services::discord::inflight::save_inflight_state(&row).expect("stamped row");
    let transition = shared.session_transition_lock(ChannelId::new(ch));
    let _held = transition.try_lock_owned().expect("transition free");
    let request = HumanInputRequest {
        channel_id: ChannelId::new(ch),
        provider: ProviderKind::Claude,
        text: "status?".to_string(),
        author_id: 200,
        source: "imessage".to_string(),
        metadata: None,
        channel_name_hint: None,
    };
    let outcome = inject::attempt(&shared, &request).await;
    let refused = inject::InjectAttempt::NotSent("session_unresolved");
    assert_eq!((outcome, pane.tmux_calls()), (refused, 0));
}

/// Holds `sessions` so a host lookup parks until the returned transaction ends.
async fn lock_sessions(pool: &sqlx::PgPool) -> sqlx::Transaction<'static, sqlx::Postgres> {
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE sessions IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    lock
}

/// Whether a query on `sessions` waits behind the test's table lock.
async fn lookup_waiting(pool: &sqlx::PgPool) -> bool {
    let waiting = async {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname = current_database() \
             AND wait_event_type = 'Lock' AND query ILIKE '%sessions%')",
        )
        .fetch_one(pool)
        .await
        .unwrap()
    };
    waiting.await
}

/// Waits until a query on `sessions` queues behind the test's table lock.
async fn lookup_parked(pool: &sqlx::PgPool) {
    let parked = async {
        while !lookup_waiting(pool).await {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), parked)
        .await
        .expect("host lookup parked");
}

/// The host lookup awaits PostgreSQL inside the transition: intake cannot claim, and input queued,
/// claimed or a new holder installed around the transition while it waits still vetoes the paste.
#[tokio::test(flavor = "current_thread")]
async fn input_queued_or_claimed_while_the_host_lookup_waits_still_goes_first_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let channels = [6_245_501, 6_245_502, 6_245_503];
    let registry = HealthRegistry::new();
    let shared = register_inject_runtime(&registry, &channels, Some(pool.clone())).await;
    let mut observed = Vec::new();
    for (index, ch) in channels.into_iter().enumerate() {
        let pane = InjectPane::new(ch, "all");
        let lock = lock_sessions(&pool).await;
        let input = deliver(&registry, ch);
        tokio::pin!(input);
        tokio::select! {
            outcome = &mut input => panic!("deliver finished before its host lookup: {outcome}"),
            () = lookup_parked(&pool) => {}
        }
        let intake = crate::services::discord::try_intake_runtime_transition_after_redirect;
        let fenced = intake(&shared, ChannelId::new(ch), (None, false, String::new()))
            .await
            .is_err();
        let _claim = match index {
            0 => {
                let enqueue = crate::services::discord::mailbox_enqueue_intervention;
                let channel = ChannelId::new(ch);
                let earlier = enqueue(&shared, &ProviderKind::Claude, channel, queued(ch + 10));
                assert!(earlier.await.enqueued);
                None
            }
            1 => Some(claim(&shared, ch).await),
            _ => Some(claim_kinded(&shared, ch, ActiveTurnKind::Background).await),
        };
        let parked_calls = pane.tmux_calls();
        lock.rollback().await.unwrap();
        let outcome = input.await;
        let queue = queue_texts(&shared, ch).await.join(",");
        let keys = pane.keys().len();
        observed.push(format!(
            "{outcome} fenced={fenced} parked_tmux={parked_calls} keys={keys} [{queue}]"
        ));
    }
    assert_eq!(
        observed,
        [
            "queued external_turn_active veto=queue_nonempty fenced=true parked_tmux=0 keys=0 [earlier input,status?]",
            "queued turn_active veto=input_in_flight fenced=true parked_tmux=0 keys=0 [status?]",
            "queued background_turn veto=holder_changed fenced=true parked_tmux=0 keys=0 [status?]",
        ]
    );
}

/// After the host lookup, a pane rebound to another transcript vetoes the paste, and input queued
/// while the last name lookup waits still goes first: no await is left after the final reads.
#[tokio::test(flavor = "current_thread")]
async fn a_rebound_transcript_or_input_queued_during_the_last_lookups_vetoes_the_paste_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let channels = [6_245_511, 6_245_512];
    let registry = HealthRegistry::new();
    let shared = register_inject_runtime(&registry, &channels, Some(pool.clone())).await;
    let mut observed = Vec::new();
    for (index, ch) in channels.into_iter().enumerate() {
        let pane = InjectPane::new(ch, "all");
        let lock = lock_sessions(&pool).await;
        let input = deliver(&registry, ch);
        tokio::pin!(input);
        tokio::select! {
            outcome = &mut input => panic!("deliver finished before its host lookup: {outcome}"),
            () = lookup_parked(&pool) => {}
        }
        let core = if index == 0 {
            let mut binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(
                &pane.session(),
            )
            .expect("pane binding");
            binding.output_path = pane.path("replaced.jsonl").display().to_string();
            let register = crate::services::tui_prompt_dedupe::register_tmux_runtime_binding;
            register(&pane.session(), binding);
            lock.rollback().await.unwrap();
            None
        } else {
            let core = shared.core.lock().await;
            let reached = test_hook::final_lookup_signal(ch);
            lock.rollback().await.unwrap();
            let at_lookup =
                tokio::time::timeout(std::time::Duration::from_secs(10), reached.notified());
            tokio::select! {
                outcome = &mut input => panic!("deliver finished past a held name lookup: {outcome}"),
                reached = at_lookup => reached.expect("resolve reached its final name lookup"),
            }
            let enqueue = crate::services::discord::mailbox_enqueue_intervention;
            let channel = ChannelId::new(ch);
            let earlier = enqueue(&shared, &ProviderKind::Claude, channel, queued(ch + 10));
            let earlier = tokio::time::timeout(std::time::Duration::from_secs(10), earlier);
            assert!(earlier.await.expect("enqueue needs no core lock").enqueued);
            Some(core)
        };
        drop(core);
        let outcome = input.await;
        let queue = queue_texts(&shared, ch).await.join(",");
        observed.push(format!("{outcome} keys={} [{queue}]", pane.keys().len()));
    }
    assert_eq!(
        observed,
        [
            "queued external_turn_active veto=session_unresolved keys=0 [status?]",
            "queued external_turn_active veto=queue_nonempty keys=0 [earlier input,status?]",
        ]
    );
}

/// A row stamped with another runtime, or a first bound candidate of another runtime, keeps the
/// session out of injection even when a later candidate names a Claude TUI pane.
#[test]
fn the_row_stamp_or_the_first_bound_pane_decides_whether_a_session_is_tui() {
    let register = crate::services::tui_prompt_dedupe::register_tmux_runtime_binding;
    let bind = |name: &str, kind: RuntimeHandoffKind| {
        let binding = crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: kind,
            output_path: format!("/tmp/{name}.jsonl"),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        };
        register(name, binding);
    };
    let provider = ProviderKind::Claude;
    let (headless, tui_name) = ("agentdesk-6245-p25-headless", "agentdesk-6245-p25");
    let tui = provider.build_tmux_session_name(tui_name);
    bind(headless, RuntimeHandoffKind::LegacyTmuxWrapper);
    bind(&tui, RuntimeHandoffKind::ClaudeTui);
    let row = |kind: Option<RuntimeHandoffKind>, session: Option<&str>| {
        let mut row = InflightTurnState::new(
            provider.clone(),
            6_245_521,
            None,
            0,
            0,
            0,
            "typed".to_string(),
            None,
            session.map(str::to_string),
            None,
            None,
            0,
        );
        row.runtime_kind = kind;
        row
    };
    let legacy = Some(RuntimeHandoffKind::LegacyTmuxWrapper);
    let cases = [
        (
            "headless row, own binding",
            Some(row(legacy, Some(headless))),
            None,
        ),
        (
            "unstamped row, headless binding first",
            Some(row(None, Some(headless))),
            Some(tui_name),
        ),
        (
            "headless row, tui by name",
            Some(row(legacy, None)),
            Some(tui_name),
        ),
        ("no row, tui by name", None, Some(tui_name)),
        (
            "tui row",
            Some(row(Some(RuntimeHandoffKind::ClaudeTui), Some(&tui))),
            None,
        ),
    ];
    let observed: Vec<String> = cases
        .iter()
        .map(|(case, row, named)| {
            let named = named.map(str::to_string);
            let pane = inject::tui_session(&provider, row.as_ref(), None, named);
            format!("{case}: {:?}", pane.map(|(session, _)| session == tui))
        })
        .collect();
    let clear = crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding;
    let _ = (clear(headless), clear(&tui));
    assert_eq!(
        observed,
        [
            "headless row, own binding: None",
            "unstamped row, headless binding first: None",
            "headless row, tui by name: None",
            "no row, tui by name: Some(true)",
            "tui row: Some(true)",
        ]
    );
}

/// A cancelled request leaves its paste running: the transition stays held until the effect ends,
/// and the unconfirmed alert is still recorded.
#[tokio::test(flavor = "current_thread")]
async fn a_cancelled_request_keeps_the_transition_and_its_alert_until_the_paste_ends_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let ch = 6_245_601;
    let registry = Arc::new(HealthRegistry::new());
    let shared = register_inject_runtime(&registry, &[ch], Some(pool)).await;
    let pane = InjectPane::new(ch, "external");
    pane.set("gate", "");
    pane.set("fail_paste", "");
    let request = tokio::spawn({
        let registry = registry.clone();
        async move { deliver(&registry, ch).await }
    });
    let at_gate = async {
        while !pane.path("at_gate").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), at_gate)
        .await
        .expect("paste reached the scripted tmux");
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    let held = |shared: &SharedData| {
        shared
            .session_transition_lock(ChannelId::new(ch))
            .try_lock_owned()
            .is_err()
    };
    let held_while_pasting = held(&shared);
    pane.set("go", "");
    let released = async {
        while held(&shared) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    };
    let released = tokio::time::timeout(std::time::Duration::from_secs(10), released)
        .await
        .is_ok();
    let alerted = crate::services::observability::events::recent(10_000)
        .iter()
        .any(|event| {
            event.event_type == "busy_inject_unconfirmed"
                && event.channel_id == Some(ch)
                && event.payload["detail"] == "paste_failed"
        });
    let queue = queue_texts(&shared, ch).await;
    assert_eq!(
        (held_while_pasting, released, alerted, queue.len()),
        (true, true, true, 0)
    );
}

/// An effect that panics outside its catch_unwind is reported once from the join, while a paste
/// that fails inside the effect keeps its single alert.
#[tokio::test(flavor = "current_thread")]
async fn an_effect_that_dies_outside_its_guard_is_still_reported_once_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let (crashed, failed) = (6_245_602, 6_245_603);
    let registry = HealthRegistry::new();
    let shared = register_inject_runtime(&registry, &[crashed, failed], Some(pool)).await;
    let panes = [crashed, failed].map(|ch| InjectPane::new(ch, "external"));
    test_hook::crash_effect(crashed);
    panes[1].set("fail_paste", "");
    let mut observed = Vec::new();
    for pane in &panes {
        let ch = pane.channel;
        let outcome = deliver(&registry, ch).await;
        let alerts: Vec<_> = crate::services::observability::events::recent(10_000)
            .iter()
            .filter(|event| {
                event.event_type == "busy_inject_unconfirmed" && event.channel_id == Some(ch)
            })
            .map(|event| {
                event.payload["detail"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        let free = shared
            .session_transition_lock(ChannelId::new(ch))
            .try_lock_owned()
            .is_ok();
        let queue = queue_texts(&shared, ch).await.len();
        observed.push(format!(
            "{outcome} alerts={alerts:?} free={free} queue={queue}"
        ));
    }
    assert_eq!(
        observed,
        [
            "Ok(Unconfirmed { turn_id: None, detail: \"executor_failed\" }) alerts=[\"executor_failed\"] free=true queue=0",
            "Ok(Unconfirmed { turn_id: None, detail: \"paste_failed\" }) alerts=[\"paste_failed\"] free=true queue=0",
        ]
    );
}
