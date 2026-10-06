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
        let folded = "[Pasted text #1 +1 lines]";
        pane.set(
            "cap.pasted",
            &format!("⏺ Working on it.\n\n{SPINNER}\n\n{BORDER}\n❯ {folded}\n{BORDER}\n{FOOTER}"),
        );
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

    pub(crate) fn session(&self) -> String {
        format!("AgentDesk-claude-inject-{}", self.channel)
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
    let token = Arc::new(CancelToken::new());
    let message = MessageId::new(channel + 10);
    let start = crate::services::discord::mailbox_try_start_turn;
    let user = UserId::new(7);
    assert!(
        start(
            shared,
            ChannelId::new(channel),
            token.clone(),
            user,
            message
        )
        .await
    );
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
fn only_a_tui_direct_row_or_with_all_a_discord_turn_on_its_own_row_holds_a_pane_for_input() {
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
    let background = claim(ActiveTurnKind::Background);
    let (ext, all) = (InjectMode::External, InjectMode::All);
    let held = || Err(inject::HOLDER_UNSUPPORTED);
    #[rustfmt::skip]
    let cases = [
        (ext, &idle, Some(&external), Ok(None)),
        (all, &idle, Some(&external), Ok(None)),
        (ext, &turn, Some(&managed), held()),
        (all, &turn, Some(&managed), Ok(Some("discord:9:5".to_string()))),
        // An intake holding the claim over the TUI-direct row is mid-transition, not a holder.
        (all, &turn, Some(&external), held()),
        (all, &background, Some(&managed), held()),
        (all, &turn, Some(&stale), held()),
        (all, &turn, None, held()),
        (all, &idle, None, held()),
        (all, &idle, Some(&managed), held()),
        (all, &idle, Some(&monitor), held()),
        (all, &idle, Some(&adopted), held()),
    ];
    for (index, (mode, snapshot, row, expected)) in cases.into_iter().enumerate() {
        assert_eq!(
            inject::holder(mode, snapshot, row, 9),
            expected,
            "case {index}"
        );
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
            "queued turn_active veto=holder_unsupported [status?] tmux=0 keys=0",
            "queued external_turn_active veto=transition_busy [earlier input,status?] tmux=0 keys=0",
        ]
    );
}

/// Waits until a query on `sessions` queues behind the test's table lock.
async fn lookup_parked(pool: &sqlx::PgPool) {
    let parked = async {
        while !sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname = current_database() \
             AND wait_event_type = 'Lock' AND query ILIKE '%sessions%')",
        )
        .fetch_one(pool)
        .await
        .unwrap()
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), parked)
        .await
        .expect("host lookup parked");
}

/// The host lookup awaits PostgreSQL inside the transition: intake cannot claim, and input queued
/// or claimed around the transition while it waits still vetoes the paste.
#[tokio::test(flavor = "current_thread")]
async fn input_queued_or_claimed_while_the_host_lookup_waits_still_goes_first_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let channels = [6_245_501, 6_245_502];
    let registry = HealthRegistry::new();
    let shared = register_inject_runtime(&registry, &channels, Some(pool.clone())).await;
    let mut observed = Vec::new();
    for (index, ch) in channels.into_iter().enumerate() {
        let pane = InjectPane::new(ch, "all");
        let mut lock = pool.begin().await.unwrap();
        sqlx::query("LOCK TABLE sessions IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *lock)
            .await
            .unwrap();
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
        let _claim = if index == 0 {
            let enqueue = crate::services::discord::mailbox_enqueue_intervention;
            let channel = ChannelId::new(ch);
            assert!(
                enqueue(&shared, &ProviderKind::Claude, channel, queued(ch + 10))
                    .await
                    .enqueued
            );
            None
        } else {
            Some(claim(&shared, ch).await)
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
            "queued external_turn_active veto=holder_unsupported fenced=true parked_tmux=0 keys=0 [status?]",
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
