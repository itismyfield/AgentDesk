//! Busy-turn injection behind the real deliver entry: switch, holder and backlog vetoes, the
//! mailbox schedules around them, and a scripted tmux standing in for the pane.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

pub(crate) use super::inject::test_hook as inject_hook;
use super::inject::{self, InjectMode, test_hook};
use super::{HumanInputDelivery, HumanInputRequest, deliver_human_input};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::SharedData;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::inflight::{InflightTurnState, TurnSource};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::turn_orchestrator::{ActiveTurnKind, ChannelMailboxSnapshot, Intervention};
pub(crate) use test_hook::start_without_gateway;
use tokio::sync::Notify;

const BORDER: &str = "────────────────────────────────────────────────────────────";
const FOOTER: &str = "  ⏵⏵ bypass permissions on (shift+tab to cycle)";
const SPINNER: &str = "✻ Thinking… (12s · esc to interrupt)";
const BUSY_TURN: &str = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"go\"}}\n\
    {\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"working\"}]}}\n";

/// Scripted `tmux` of `size` (80,24) pasting only at `live`: folded, or flat with `draw`/`draw.rows`;
/// Enter queues the header as Claude does mid-turn. `gate`/`hold` park keys/capture until `go`.
const FAKE_TMUX: &str = r##"#!/bin/sh
d='@D@'
echo "$*" >> "$d/log"
case "$2" in
display-message) echo "$(cat "$d/attach"),,$(cat "$d/size" 2>/dev/null || echo 80,24)" ;;
capture-pane)
  if [ -f "$d/hold" ]; then
    rm -f "$d/hold"; touch "$d/at_hold"; i=0
    while [ ! -f "$d/go" ] && [ $i -lt 200 ]; do sleep 0.05; i=$((i+1)); done
  fi
  if [ -f "$d/pasted" ]; then cat "$d/cap.pasted"; else cat "$d/cap.before"; fi ;;
load-buffer) for last do :; done; cp "$last" "$d/buffer" ;;
if-shell)
  a=$(cat "$d/attach")
  if [ "$a" != 0 ]; then echo "agentdesk-busy-inject-vetoed $a"; exit 0; fi
  live=$(cat "$d/live" 2>/dev/null || cat "$d/size" 2>/dev/null || echo 80,24)
  case "$6" in *pane_width*)
    case "$6" in *"#{==:#{pane_width},${live%,*}}"*"#{==:#{pane_height},${live#*,}}"*) ;;
    *) echo "agentdesk-busy-inject-vetoed 0"; exit 0 ;; esac ;; esac
  if [ -f "$d/gate" ]; then
    touch "$d/at_gate"; i=0
    while [ ! -f "$d/go" ] && [ $i -lt 80 ]; do sleep 0.05; i=$((i+1)); done
  fi
  [ -f "$d/fail_paste" ] && exit 1
  echo "$7" >> "$d/keys"
  case "$7" in
  paste-buffer*)
    touch "$d/pasted"
    [ -f "$d/draw" ] && {
      cat "$d/draw"; printf '\342\235\257\302\240%s\n' "$(head -n 1 "$d/buffer")"
      if [ -f "$d/draw.rows" ]; then cat "$d/draw.rows"; else awk 'NR > 1 { print ($0 == "" ? "" : "  " $0) }' "$d/buffer"; fi
      cat "$d/draw.tail"; } > "$d/cap.pasted" ;;
  send-keys*) printf '{"type":"queue-operation","operation":"enqueue","content":"%s\\nstatus?","sessionId":"6245","timestamp":"2026-10-06T00:00:00.000Z"}\n' "$(head -n 1 "$d/buffer")" >> "$d/transcript.jsonl" ;;
  esac ;;
esac
exit 0
"##;

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
    pub(in crate::services::discord) fn reseat_row(&self, source: TurnSource, message: u64) {
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

    pub(crate) fn path(&self, name: &str) -> PathBuf {
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

    /// The composer after the paste shows it flat: continuation rows two columns in, or `rows`.
    fn draw_paste(&self, rows: Option<&[String]>) {
        if let Some(rows) = rows {
            self.set("draw.rows", &format!("{}\n", rows.join("\n")));
        }
        self.set(
            "draw",
            &format!("⏺ Working on it.\n\n{SPINNER}\n\n{BORDER}\n"),
        );
        self.set("draw.tail", &format!("{BORDER}\n{FOOTER}"));
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
    deliver_text(registry, channel, "status?").await
}

async fn deliver_text(registry: &HealthRegistry, channel: u64, text: &str) -> String {
    let request = HumanInputRequest {
        channel_id: ChannelId::new(channel),
        provider: ProviderKind::Claude,
        text: text.to_string(),
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
        Ok(HumanInputDelivery::Started { .. }) => "started".to_string(),
        other => format!("{other:?}"),
    }
}

/// Ends the turn holding the channel's mailbox slot.
pub(crate) async fn end_turn(shared: &SharedData, channel: u64) {
    shared
        .mailboxes
        .handle(ChannelId::new(channel))
        .hard_stop()
        .await;
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
pub(crate) async fn claim_kinded(
    shared: &SharedData,
    channel: u64,
    kind: ActiveTurnKind,
) -> Arc<CancelToken> {
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

/// A short input renders flat rather than folded; the deliver still proves the drawn draft is
/// its own, enters it once and confirms it from the transcript.
#[tokio::test(flavor = "current_thread")]
async fn a_flat_paste_drawn_with_its_indent_is_injected_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let inputs = [
        (
            6_687_001,
            "응답에 정확히 한 줄로 [E2E:PR1:pb1-c-s5d-pr1-074645] 만 출력해줘.",
        ),
        (6_687_002, "first line\n   indented second"),
    ];
    let registry = HealthRegistry::new();
    let channels = inputs.map(|(ch, _)| ch);
    let shared = register_inject_runtime(&registry, &channels, Some(pool)).await;
    let mut observed = Vec::new();
    for (ch, text) in inputs {
        let pane = InjectPane::new(ch, "all");
        pane.draw_paste(None);
        let outcome = deliver_text(&registry, ch, text).await;
        let queue = queue_texts(&shared, ch).await.join(",");
        let (keys, seen) = (pane.keys().join("+"), pane.transcript_recorded_the_paste());
        observed.push(format!("{outcome} keys={keys} seen={seen} [{queue}]"));
    }
    let injected = "Ok(Injected { turn_id: None }) keys=paste-buffer+send-keys seen=true []";
    assert_eq!(observed, [injected; 2]);
}

/// At 80x24 a long line wraps as measured on Claude Code 2.1.293: predicted rows prove the paste
/// ours, and a line whose rows are not predictable is handed back unpasted.
#[tokio::test(flavor = "current_thread")]
async fn a_wrapped_paste_is_injected_only_when_its_rows_are_predicted_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let (wrapped, unpredictable) = (6_687_011, 6_687_012);
    let registry = HealthRegistry::new();
    let shared = register_inject_runtime(&registry, &[wrapped, unpredictable], Some(pool)).await;
    let words = |n: usize| ["가나다"; 25][..n].join(" ");
    let measured = [
        format!("  {}", words(11)),
        format!("  {}", words(10)),
        format!("  {} 끝", words(4)),
    ];
    let emoji = [format!("  {}", "😀a".repeat(25)), "  😀a".to_string()];
    let mut observed = Vec::new();
    for (ch, text, rows) in [
        (wrapped, format!("{} 끝", words(25)), measured.as_slice()),
        (unpredictable, "😀a".repeat(26), emoji.as_slice()),
    ] {
        let pane = InjectPane::new(ch, "all");
        pane.draw_paste(Some(rows));
        let outcome = deliver_text(&registry, ch, &text).await;
        let queue = queue_texts(&shared, ch).await.len();
        let (keys, seen) = (pane.keys().join("+"), pane.transcript_recorded_the_paste());
        observed.push(format!("{outcome} keys={keys} seen={seen} queued={queue}"));
    }
    assert_eq!(
        observed,
        [
            "Ok(Injected { turn_id: None }) keys=paste-buffer+send-keys seen=true queued=0",
            "queued handed_back veto=unpredictable_render keys= seen=false queued=1",
        ]
    );
}

/// A pasted `─` or `❯` row the composer reader would misread, a pane of unknown size, and a resize
/// after the prediction each hand the input back unpasted.
#[tokio::test(flavor = "current_thread")]
async fn a_paste_its_rows_cannot_prove_is_handed_back_unpasted_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let x = |n: usize| "x".repeat(n);
    let cases = [
        (6_687_021, "─".to_string(), None),
        (6_687_022, "───".to_string(), None),
        (6_687_023, "❯ hello".to_string(), None),
        (6_687_024, "status?".to_string(), Some(("size", ""))),
        (6_687_025, x(77), Some(("live", "79,24"))),
    ];
    let handed_back = "queued handed_back veto=unpredictable_render keys= seen=false queued=1";
    let expected: Vec<_> = cases
        .iter()
        .map(|case| format!("{}: {handed_back}", case.1))
        .collect();
    let registry = HealthRegistry::new();
    let channels = cases.each_ref().map(|case| case.0);
    let shared = register_inject_runtime(&registry, &channels, Some(pool)).await;
    // The 79-column rows Claude would draw, so a paste the guard let through is not owned.
    let resized = [format!("  {}", x(75)), "  xx".to_string()];
    let mut observed = Vec::new();
    for (ch, text, file) in cases {
        let pane = InjectPane::new(ch, "all");
        pane.draw_paste(
            file.is_some_and(|(name, _)| name == "live")
                .then_some(&resized[..]),
        );
        if let Some((name, value)) = file {
            pane.set(name, value);
        }
        let outcome = deliver_text(&registry, ch, &text).await;
        let queue = queue_texts(&shared, ch).await.len();
        let (keys, seen) = (pane.keys().join("+"), pane.transcript_recorded_the_paste());
        observed.push(format!(
            "{text}: {outcome} keys={keys} seen={seen} queued={queue}"
        ));
    }
    assert_eq!(observed, expected);
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
    let outcome = inject::attempt(&shared, &request, inject::Origin::External).await;
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
    let outcome = inject::attempt(&shared, &request, inject::Origin::External).await;
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
            "queued turn_active veto=holder_changed fenced=true parked_tmux=0 keys=0 [status?]",
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
            let claude = RuntimeHandoffKind::ClaudeTui;
            let pane = inject::tui_session(&provider, claude, row.as_ref(), None, named);
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

impl InjectPane {
    /// A composer holding a half-typed draft, so the pane vetoes the paste as `draft`.
    pub(crate) fn draft(&self) {
        let pane =
            format!("⏺ Working on it.\n\n{SPINNER}\n\n{BORDER}\n❯ half typed\n{BORDER}\n{FOOTER}");
        self.set("cap.before", &pane);
    }

    /// A pane whose turn has ended, so the paste is vetoed as `not_busy`.
    pub(crate) fn idle(&self) {
        let pane = format!("⏺ Done.\n\n{BORDER}\n❯\u{00a0}\n{BORDER}\n{FOOTER}");
        self.set("cap.before", &pane);
    }
}

/// Starts a delivery and returns once it holds its reservation, parked at the first pane capture.
async fn deliver_parked(
    registry: &Arc<HealthRegistry>,
    pane: &InjectPane,
) -> tokio::task::JoinHandle<String> {
    pane.set("hold", "");
    let (registry, ch) = (registry.clone(), pane.channel);
    let request = tokio::spawn(async move { deliver(&registry, ch).await });
    let parked = async {
        while !pane.path("at_hold").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), parked)
        .await
        .expect("delivery reached its first pane capture");
    request
}

/// Input sent while a delivery holds its reservation, as Discord intake would queue it.
pub(crate) async fn send_meanwhile(
    shared: &Arc<SharedData>,
    channel: u64,
    message: u64,
    text: &str,
) {
    let mut item = queued(message);
    item.text = text.to_string();
    let enqueue = crate::services::discord::mailbox_enqueue_intervention;
    let channel = ChannelId::new(channel);
    let sent = enqueue(shared, &ProviderKind::Claude, channel, item).await;
    assert!(sent.enqueued, "{:?}", sent.refusal_reason);
}

/// A paste vetoed after the reservation hands the input back ahead of input sent meanwhile,
/// also when the first handback write does not land; nothing reaches the pane.
#[tokio::test(flavor = "current_thread")]
async fn a_vetoed_paste_hands_the_input_back_ahead_of_input_sent_after_its_reservation_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let ch = 6_845_201;
    let registry = Arc::new(HealthRegistry::new());
    let shared = register_inject_runtime(&registry, &[ch], Some(pool)).await;
    let pane = InjectPane::new(ch, "all");
    pane.draft();
    let request = deliver_parked(&registry, &pane).await;
    send_meanwhile(&shared, ch, ch + 10, "earlier input").await;
    let support = crate::services::turn_orchestrator::test_support::fail_queue_saves;
    support(ChannelId::new(ch), 1);
    pane.set("go", "");
    let outcome = request.await.expect("delivery");
    let faults = crate::services::turn_orchestrator::test_support::queue_save_faults;
    let left = faults(ChannelId::new(ch), true);
    let queue = queue_texts(&shared, ch).await.join(",");
    let keys = pane.keys().len();
    assert_eq!(
        format!("{outcome} [{queue}] keys={keys} faults_left={left}"),
        "queued handed_back veto=draft [status?,earlier input] keys=0 faults_left=0"
    );
}

/// Four unwritten handback attempts refuse the delivery, and the input is not requeued on its own;
/// redelivered once the queue writes again, it is new input behind what was sent meanwhile.
#[tokio::test(flavor = "current_thread")]
async fn a_handback_that_never_lands_refuses_the_delivery_and_keeps_no_place_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let ch = 6_845_211;
    let channel = ChannelId::new(ch);
    let registry = Arc::new(HealthRegistry::new());
    let shared = register_inject_runtime(&registry, &[ch], Some(pool)).await;
    let pane = InjectPane::new(ch, "all");
    pane.draft();
    let request = deliver_parked(&registry, &pane).await;
    send_meanwhile(&shared, ch, ch + 10, "earlier input").await;
    let support = crate::services::turn_orchestrator::test_support::fail_queue_saves;
    support(channel, 5);
    pane.set("go", "");
    let outcome = request.await.expect("delivery");
    let faults = crate::services::turn_orchestrator::test_support::queue_save_faults;
    let left = faults(channel, true);
    let abandons = crate::services::turn_orchestrator::test_support::injection_abandons(channel);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let unrecovered = queue_texts(&shared, ch).await.join(",");
    let again = deliver(&registry, ch).await;
    let queue = queue_texts(&shared, ch).await.join(",");
    assert_eq!(
        [
            format!("{outcome} faults_left={left} abandons={abandons} [{unrecovered}]"),
            format!("{again} [{queue}] keys={}", pane.keys().len()),
        ],
        [
            "Err(QueueRefused(\"handback_persistence\")) faults_left=1 abandons=1 [earlier input]",
            "queued external_turn_active veto=queue_nonempty [earlier input,status?] keys=0",
        ]
    );
}

/// A caller cancelled after the reservation leaves the rest to the owner: with the holding turn
/// over and the paste vetoed, the input lands at the queue front and the drain is kicked.
#[tokio::test(flavor = "current_thread")]
async fn a_cancelled_caller_leaves_the_handback_and_its_kick_to_the_owner_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let ch = 6_845_221;
    let channel = ChannelId::new(ch);
    let registry = Arc::new(HealthRegistry::new());
    let shared = register_inject_runtime(&registry, &[ch], Some(pool)).await;
    let pane = InjectPane::new(ch, "all");
    let kicks = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = kicks.clone();
    let hook = crate::services::discord::queue_io::set_idle_queue_kick_hook_for_tests;
    let _hook = hook(Arc::new(move |_, _, kicked, reason| {
        let seen = seen.clone();
        Box::pin(async move {
            if kicked != channel {
                return None;
            }
            seen.lock().unwrap().push(reason);
            Some(Default::default())
        })
    }));
    claim_kinded(&shared, ch, ActiveTurnKind::Background).await;
    let request = deliver_parked(&registry, &pane).await;
    shared.mailboxes.handle(channel).hard_stop().await;
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    pane.idle();
    pane.set("go", "");
    let kicked = async {
        while kicks.lock().unwrap().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    };
    let kicked = tokio::time::timeout(std::time::Duration::from_secs(10), kicked)
        .await
        .is_ok();
    let queue = queue_texts(&shared, ch).await.join(",");
    let reasons = kicks.lock().unwrap().clone();
    assert_eq!(
        format!(
            "kicked={kicked} {reasons:?} [{queue}] keys={}",
            pane.keys().len()
        ),
        "kicked=true [\"post_enqueue_idle_snapshot\"] [status?] keys=0"
    );
}

/// A handback onto a full queue evicts the newest entry once, and the owner dead-letters that
/// overflow through the shared queue-exit feedback.
#[tokio::test(flavor = "current_thread")]
async fn a_handback_onto_a_full_queue_records_its_one_overflow_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let ch = 6_845_231;
    let registry = Arc::new(HealthRegistry::new());
    let shared = register_inject_runtime(&registry, &[ch], Some(pool.clone())).await;
    let pane = InjectPane::new(ch, "all");
    pane.draft();
    let request = deliver_parked(&registry, &pane).await;
    let cap = crate::services::turn_orchestrator::MAX_INTERVENTIONS_PER_CHANNEL as u64;
    for n in 1..=cap {
        send_meanwhile(&shared, ch, ch + 100 + n, &format!("sent {n}")).await;
    }
    pane.set("go", "");
    let outcome = request.await.expect("delivery");
    let kind = crate::db::relay_dead_letter::KIND_QUEUE_OVERFLOW;
    let claim = crate::db::relay_dead_letter::claim_pending_redeliveries;
    let recorded = async {
        loop {
            let rows = claim(&pool, kind, 0, 3_600, 10)
                .await
                .expect("dead letters");
            if !rows.is_empty() {
                break rows;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    };
    let rows = tokio::time::timeout(std::time::Duration::from_secs(10), recorded)
        .await
        .expect("the overflow was dead-lettered");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let more = claim(&pool, kind, 0, 3_600, 10)
        .await
        .expect("dead letters");
    let evicted: Vec<_> = rows.iter().map(|row| row.message_id.clone()).collect();
    let queue = queue_texts(&shared, ch).await;
    assert_eq!(
        format!(
            "{outcome} evicted={evicted:?} more={} len={} first={} last={}",
            more.len(),
            queue.len(),
            queue[0],
            queue[queue.len() - 1]
        ),
        format!(
            "queued handed_back veto=draft evicted=[Some(\"{}\")] more=0 len={cap} first=status? last=sent {}",
            ch + 100 + cap,
            cap - 1
        )
    );
}

/// With the switch off and nothing reserved, a held session transition still keeps the registry
/// purge off an idle mailbox (`transition_busy`); once it is released the purge removes it.
#[tokio::test(flavor = "current_thread")]
async fn a_held_transition_keeps_the_registry_purge_off_an_idle_mailbox() {
    let registry = HealthRegistry::new();
    let ch = 6_845_241;
    let channel = ChannelId::new(ch);
    let shared = register_inject_runtime(&registry, &[ch], None).await;
    let _ = shared.mailboxes.handle(channel).snapshot().await;
    assert_eq!(inject::mode(ch), InjectMode::Off);
    let purge = crate::services::discord::health::purge_idle_channel_mailbox_registry_entry;
    let held = shared.session_transition_lock(channel).try_lock_owned();
    let held = held.expect("transition free");
    let while_held = purge(&registry, Some("claude"), ch).await;
    let kept = shared.mailboxes.peek(channel).is_some();
    drop(held);
    let released = purge(&registry, Some("claude"), ch).await;
    assert_eq!(
        format!("{while_held:?} kept={kept} {released:?}"),
        "MailboxRegistryPurgeResult { removed: false, skipped_reason: Some(\"transition_busy\") } \
         kept=true MailboxRegistryPurgeResult { removed: true, skipped_reason: None }"
    );
}

/// Allowed range: the claim is checked when the actor reserves, the row only at its last read.
/// A row replaced after that read, with the claim unchanged, does not stop the paste.
#[tokio::test(flavor = "current_thread")]
async fn a_row_replaced_after_its_last_read_does_not_stop_the_reservation_allowed_range_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let ch = 6_845_251;
    let registry = Arc::new(HealthRegistry::new());
    register_inject_runtime(&registry, &[ch], Some(pool)).await;
    let pane = InjectPane::new(ch, "all");
    let (reached, resume) = test_hook::park_before_reserve(ch);
    let request = tokio::spawn({
        let registry = registry.clone();
        async move { deliver(&registry, ch).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), reached.notified())
        .await
        .expect("owner parked before its reservation");
    pane.reseat_row(TurnSource::ExternalInput, ch + 77);
    resume.notify_one();
    let outcome = request.await.expect("delivery");
    assert_eq!(
        format!("{outcome} keys={}", pane.keys().join("+")),
        "Ok(Injected { turn_id: None }) keys=paste-buffer+send-keys"
    );
}

/// An idle transcript keeps the caller's own start: the attempt stops before the reservation, the
/// session-transition guard and any pane call.
#[tokio::test(flavor = "current_thread")]
async fn an_idle_transcript_stops_before_the_reservation_and_the_pane() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let ch = 6_845_261;
    let shared = crate::services::discord::make_shared_data_for_tests();
    let pane = InjectPane::new(ch, "all");
    let ended = format!("{BUSY_TURN}{{\"type\":\"result\",\"subtype\":\"success\"}}\n");
    pane.set("transcript.jsonl", &ended);
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
    let outcome = inject::attempt(&shared, &request, inject::Origin::External).await;
    let refused = inject::InjectAttempt::NotSent("not_busy");
    assert_eq!((outcome, pane.tmux_calls()), (refused, 0));
}

/// Input a mailbox has not loaded from disk yet still goes first: the deliver reserves nothing and
/// never reaches the pane.
#[tokio::test(flavor = "current_thread")]
async fn input_left_on_disk_for_an_unloaded_mailbox_stays_ahead_of_a_deliver_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let ch = 6_845_271;
    let channel = ChannelId::new(ch);
    let registry = Arc::new(HealthRegistry::new());
    let shared = register_inject_runtime(&registry, &[ch], Some(pool)).await;
    let pane = InjectPane::new(ch, "all");
    let provider = ProviderKind::Claude;
    let persistence =
        crate::services::discord::queue_persistence_context(&shared, &provider, channel);
    let earlier = crate::services::turn_orchestrator::ChannelMailboxRegistry::default();
    let written = earlier
        .handle(channel)
        .enqueue(queued(ch + 10), persistence)
        .await;
    assert!(written.enqueued);
    let outcome = deliver(&registry, ch).await;
    let queue = queue_texts(&shared, ch).await.join(",");
    assert_eq!(
        format!(
            "{outcome} [{queue}] tmux={} keys={}",
            pane.tmux_calls(),
            pane.keys().len()
        ),
        "queued external_turn_active veto=queue_nonempty [earlier input,status?] tmux=0 keys=0"
    );
}

/// With the switch on, a deliver whose start waits out another input's handback claims behind it:
/// B is vetoed once A has ended and takes the queue front, C queues behind B, and the kick runs B.
#[tokio::test(flavor = "current_thread")]
async fn a_deliver_waiting_on_a_handback_claims_behind_it_and_the_kick_runs_the_handback_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let ch = 6_845_281;
    let channel = ChannelId::new(ch);
    let registry = Arc::new(HealthRegistry::new());
    let shared = register_inject_runtime(&registry, &[ch], Some(pool)).await;
    let pane = InjectPane::new(ch, "all");
    let starts = start_without_gateway(ch);
    claim(&shared, ch).await;
    pane.reseat_row(TurnSource::Managed, ch + 10);
    let (c_done, runs) = (Arc::new(Notify::new()), Arc::new(Mutex::new(Vec::new())));
    let (after_c, seen) = (c_done.clone(), runs.clone());
    let hook = crate::services::discord::queue_io::set_idle_queue_kick_hook_for_tests;
    let _hook = hook(Arc::new(move |shared, provider, kicked, _| {
        let (after_c, seen) = (after_c.clone(), seen.clone());
        Box::pin(async move {
            if kicked != channel {
                return None;
            }
            after_c.notified().await;
            let take = crate::services::discord::idle_queue_take_next_soft_if_ready;
            let ran = match take(&shared, &provider, kicked).await.into_intervention() {
                Some((head, _, lease)) => {
                    let start =
                        crate::services::discord::queue_io::mailbox_try_start_turn_behind_queue;
                    let token = Arc::new(CancelToken::new());
                    let claimed = start(&shared, kicked, token, head.author_id, head.message_id);
                    let claimed = claimed.await;
                    drop(lease);
                    format!("{} claimed={claimed}", head.text)
                }
                None => "nothing dequeued".to_string(),
            };
            seen.lock().unwrap().push(ran);
            Some(Default::default())
        })
    }));
    let b = deliver_parked(&registry, &pane).await;
    end_turn(&shared, ch).await;
    pane.idle();
    pane.set(
        "transcript.jsonl",
        &format!("{BUSY_TURN}{{\"type\":\"result\",\"subtype\":\"success\"}}\n"),
    );
    let c = tokio::spawn({
        let registry = registry.clone();
        async move { deliver_text(&registry, ch, "later").await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), starts.notified())
        .await
        .expect("C's start waits on the transition B holds");
    pane.set("go", "");
    let (b, c) = (b.await.expect("B"), c.await.expect("C"));
    c_done.notify_one();
    let ran = async {
        while runs.lock().unwrap().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), ran)
        .await
        .expect("the handback's kick ran");
    let queue = queue_texts(&shared, ch).await.join(",");
    let ran = runs.lock().unwrap().clone();
    assert_eq!(
        format!(
            "B: {b} | C: {c} | kick: {ran:?} [{queue}] keys={}",
            pane.keys().len()
        ),
        "B: queued handed_back veto=not_busy | C: queued session_transition veto=not_busy \
         | kick: [\"status? claimed=true\"] [later] keys=0"
    );
}

/// A reserved headless claim returns the very token it registered on the mailbox, in either
/// admission order.
#[tokio::test(flavor = "current_thread")]
async fn a_headless_claim_returns_the_token_it_registered() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let router = crate::services::discord::router::reserve_headless_turn;
    let mut observed = Vec::new();
    for (n, behind) in [false, true].into_iter().enumerate() {
        let channel = ChannelId::new(6_845_901 + n as u64);
        let reservation = router();
        let reservation = if behind {
            reservation.behind_queue()
        } else {
            reservation
        };
        let claim = crate::services::discord::router::claim_reserved_headless_turn;
        let identity = (ProviderKind::Claude, None);
        let claimed = claim(&shared, channel, UserId::new(100), &reservation, identity).await;
        let Ok((_transition, token)) = claimed else {
            panic!("an idle channel claims");
        };
        let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel).await;
        let registered = snapshot.cancel_token.expect("the claim registered a token");
        observed.push((behind, Arc::ptr_eq(&token, &registered)));
    }
    assert_eq!(observed, [(false, true), (true, true)]);
}

const NO_MESSAGE_CHILD: &str = "ADK_INJECT_NO_MESSAGE_CHILD";

/// External input has no Discord message, so its injection answers as before and builds no
/// disposition table or ring file; it runs in its own process so the table check sees only itself.
#[tokio::test(flavor = "current_thread")]
async fn an_injection_without_a_discord_message_records_no_disposition_pg() {
    if std::env::var_os(NO_MESSAGE_CHILD).is_none() {
        let module = module_path!().split_once("::").unwrap().1;
        let name = "an_injection_without_a_discord_message_records_no_disposition_pg";
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("{module}::{name}"), "--test-threads=1"])
            .args(["--nocapture"])
            .env(NO_MESSAGE_CHILD, "1")
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&output.stdout).to_string()
            + &String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success() && log.contains("1 passed"), "{log}");
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let channel = 6_845_911;
    let registry = HealthRegistry::new();
    let shared = register_inject_runtime(&registry, &[channel], Some(pool)).await;
    let pane = InjectPane::new(channel, "all");
    claim_kinded(&shared, channel, ActiveTurnKind::Background).await;
    let outcome = deliver(&registry, channel).await;
    let support = crate::services::discord::inject_disposition::test_support::table_built;
    let ring = crate::services::discord::inject_disposition::test_support::ring_file;
    let observed = (
        outcome,
        pane.transcript_recorded_the_paste(),
        support(),
        ring(&ProviderKind::Claude).exists(),
    );
    let injected = "Ok(Injected { turn_id: None })".to_string();
    assert_eq!(observed, (injected, true, false, false));
}
