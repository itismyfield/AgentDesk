//! A person's Discord text offered to a busy Claude TUI pane through production intake: what each
//! injection result leaves, and what a taken message, the gate and commands keep from the pane.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};

use super::discord_mock::{CHANNEL_ID, USER_ID};
use super::{RelayE2eHarness, wait_until};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::health::{
    InjectPane, claim_kinded, inject_hook, queue_texts, send_meanwhile,
};
use crate::services::discord::inject_disposition::{self as disposition, InjectionOutcome};
use crate::services::discord::router::busy_inject_support as hook;
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::ActiveTurnKind;

#[path = "busy_inject_thread_e2e_tests.rs"]
mod thread;

const WAIT: Duration = Duration::from_secs(10);
const QUEUE_UNKNOWN: &str = "⚠️ 메시지 큐 저장 중 오류가 감지되어 접수 표시를 생략했어.";
const UNCONFIRMED: &str = "❓ 터미널 입력을 확인하지 못했습니다. 중복이면 무시하세요.";

/// A Discord id of a message sent moments ago, distinct within this process.
fn fresh_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;
    let ms = chrono::Utc::now().timestamp_millis() - 5_000 - DISCORD_EPOCH_MS;
    (u64::try_from(ms).expect("after the Discord epoch") << 22)
        | NEXT.fetch_add(1, Ordering::SeqCst)
}

/// A runtime on PostgreSQL with its channel bound, startup recovery done, and placeholders
/// answered at once; the pane and the gate are each scenario's own.
struct Runtime {
    h: RelayE2eHarness,
    _db: TestPostgresDb,
}

async fn runtime() -> Runtime {
    let mut db = None;
    let h = RelayE2eHarness::start_bound_on(async {
        let fixture = TestPostgresDb::create().await;
        let pool = fixture.connect_and_migrate().await;
        db = Some(fixture);
        pool
    })
    .await;
    h.shared
        .restart
        .reconcile_done
        .store(true, Ordering::SeqCst);
    h.answer_placeholders_immediately();
    // Notices resolve their HTTP client from the runtime's cached context, as in production.
    h.cache_relay_transport();
    Runtime {
        h,
        _db: db.expect("a database under the harness lock"),
    }
}

/// A gated channel whose Claude pane is mid-turn on input typed over SSH.
struct Busy {
    pane: InjectPane,
    _gate: hook::OpenGate,
    rt: Runtime,
}

async fn busy() -> Busy {
    let rt = runtime().await;
    // The iMessage switch stays off: only the Discord gate opens this channel.
    let pane = InjectPane::new(CHANNEL_ID, "off");
    Busy {
        pane,
        _gate: hook::open_gate(CHANNEL_ID),
        rt,
    }
}

impl Runtime {
    /// A background turn holds the mailbox, so input the pane does not take is queued.
    async fn hold_mailbox(&self) {
        claim_kinded(&self.h.shared, CHANNEL_ID, ActiveTurnKind::Background).await;
    }

    async fn queue(&self) -> Vec<String> {
        queue_texts(&self.h.shared, CHANNEL_ID).await
    }

    /// Reactions added to `message`.
    fn marks(&self, message: u64) -> Vec<String> {
        let ops = self.h.shared.turn_view_reconciler.ops().into_iter();
        let added = ops.filter(|op| op.add && op.target.message_id.get() == message);
        added.map(|op| op.emoji.to_string()).collect()
    }

    /// Bot posts other than turn placeholders.
    fn notices(&self) -> Vec<String> {
        let messages = self.h.messages().into_iter().map(|(_, text)| text);
        messages.filter(|text| text != "...").collect()
    }

    fn starts(&self) -> usize {
        self.h.placeholder_posts() + self.h.provider_starts()
    }
}

/// The outcome the provider file holds for `message`, if any.
fn on_disk(message: u64) -> Option<String> {
    let path = disposition::test_support::ring_file(&ProviderKind::Claude);
    let raw = std::fs::read_to_string(path).ok()?;
    let ring: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let entries = ring["entries"].as_array()?.iter();
    let mut entries = entries.filter(|entry| entry["message_id"].as_u64() == Some(message));
    entries.next().map(|entry| entry["outcome"].to_string())
}

fn in_memory(message: u64) -> Option<InjectionOutcome> {
    let now = std::time::Instant::now();
    disposition::terminal(Some(&ProviderKind::Claude), MessageId::new(message), now)
}

/// A turn mid-run on the pane takes the text: pasted once, marked 📥, the checkpoint past it,
/// nothing queued or started, and the injection recorded where a restart finds it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_busy_pane_takes_a_person_s_text_and_marks_it_taken_pg() {
    let busy = busy().await;
    let rt = &busy.rt;
    let message = fresh_id();
    rt.h.deliver_user_message(message, "status?").await.unwrap();
    let observed = (
        busy.pane.keys(),
        rt.marks(message),
        rt.h.checkpoint(),
        rt.queue().await,
        rt.starts(),
        on_disk(message),
    );
    let keys = vec!["paste-buffer".to_string(), "send-keys".to_string()];
    let disk = Some("\"observed\"".to_string());
    let taken = (keys, vec!["📥".to_string()], Some(message), vec![], 0, disk);
    assert_eq!(observed, taken);
}

/// A message an injection already took ends at the lookup however the channel stands: held
/// mid-turn, idle, or idle after a restart with only the provider file and no pane to resolve.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_redelivered_injected_message_ends_at_the_lookup_pg() {
    let rt = runtime().await;
    let _gate = hook::open_gate(CHANNEL_ID);
    let (channel, now_ms) = (
        ChannelId::new(CHANNEL_ID),
        chrono::Utc::now().timestamp_millis(),
    );
    let restarted = MessageId::new(fresh_id());
    let record = disposition::record_terminal;
    record(
        &ProviderKind::Claude,
        channel,
        restarted,
        InjectionOutcome::Observed,
        now_ms,
    )
    .unwrap();
    let mut observed = Vec::new();
    rt.h.deliver_user_message(restarted.get(), "status?")
        .await
        .unwrap();
    observed.push(("restarted", hook::seen(restarted.get()).offers, rt.starts()));
    let pane = InjectPane::new(CHANNEL_ID, "off");
    let now = std::time::Instant::now();
    for case in ["idle", "held"] {
        if case == "held" {
            rt.hold_mailbox().await;
        }
        let message = MessageId::new(fresh_id());
        let outcome = InjectionOutcome::Observed;
        disposition::note_terminal(&ProviderKind::Claude, channel, Some(message), outcome, now);
        rt.h.deliver_user_message(message.get(), "status?")
            .await
            .unwrap();
        observed.push((case, hook::seen(message.get()).offers, rt.starts()));
    }
    let left = (pane.keys(), rt.queue().await, rt.h.checkpoint());
    let none = [("restarted", 0, 0), ("idle", 0, 0), ("held", 0, 0)];
    assert_eq!((observed, left), (none.to_vec(), (vec![], vec![], None)));
}

/// The lookup ends a taken message before a pending DM reply can consume it, while a new
/// message still answers that reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_taken_message_ends_before_a_pending_dm_reply_consumes_it_pg() {
    let rt = runtime().await;
    let _gate = hook::open_gate(CHANNEL_ID);
    let pool =
        rt.h.shared
            .pg_pool
            .clone()
            .expect("a runtime on PostgreSQL");
    let register = crate::services::discord_dm_reply_store::register_pending_dm_reply_db;
    let user = USER_ID.to_string();
    let reply = register(Some(&pool), "agent", &user, None, "{}", 600)
        .await
        .unwrap();
    // The reply still waiting for this user, if it was not consumed.
    let pending = || async {
        let load = crate::services::discord_dm_reply_store::load_oldest_pending_dm_reply_db;
        load(Some(&pool), &user).await.unwrap().map(|row| row.id)
    };
    let (channel, now_ms) = (
        ChannelId::new(CHANNEL_ID),
        chrono::Utc::now().timestamp_millis(),
    );
    let taken = MessageId::new(fresh_id());
    let record = disposition::record_terminal;
    record(
        &ProviderKind::Claude,
        channel,
        taken,
        InjectionOutcome::Observed,
        now_ms,
    )
    .unwrap();
    rt.h.deliver_user_message(taken.get(), "yes").await.unwrap();
    let after_taken = pending().await;
    rt.h.deliver_user_message(fresh_id(), "yes").await.unwrap();
    let after_new = pending().await;
    assert_eq!((after_taken, after_new), (Some(reply), None));
}

/// A message the mailbox already runs is the mailbox's: the reservation answers owned, and the
/// hook ends it without a paste, a mark or a second queue entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_the_mailbox_already_runs_is_not_pasted_pg() {
    let busy = busy().await;
    let rt = &busy.rt;
    let message = fresh_id();
    let token = std::sync::Arc::new(crate::services::provider::CancelToken::new());
    let start = crate::services::discord::mailbox_try_start_turn_kinded;
    let (channel, user) = (ChannelId::new(CHANNEL_ID), serenity::UserId::new(USER_ID));
    let kind = ActiveTurnKind::UserOrAgent;
    assert!(
        start(
            &rt.h.shared,
            channel,
            token,
            user,
            MessageId::new(message),
            kind
        )
        .await
    );
    let managed = crate::services::discord::inflight::TurnSource::Managed;
    busy.pane.reseat_row(managed, message);
    rt.h.deliver_user_message(message, "status?").await.unwrap();
    let observed = (
        hook::seen(message).outcomes,
        busy.pane.keys(),
        rt.marks(message),
        rt.queue().await,
    );
    let owned = vec!["NotSent(\"source_owned\")".to_string()];
    assert_eq!(observed, (owned, vec![], vec![], vec![]));
}

/// An owner refused before its reservation releases the message first, so intake queues it
/// behind input that arrived meanwhile, unmarked by the hook.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_owner_refused_before_its_reservation_leaves_the_message_to_intake_pg() {
    let busy = busy().await;
    let rt = &busy.rt;
    rt.hold_mailbox().await;
    let (reached, resume) = inject_hook::park_before_reserve(CHANNEL_ID);
    let message = fresh_id();
    let first = rt.h.spawn_user_message(message, "status?");
    tokio::time::timeout(WAIT, reached.notified())
        .await
        .expect("the owner parked before its reservation");
    let meanwhile = fresh_id();
    rt.h.deliver_user_message(meanwhile, "meanwhile")
        .await
        .unwrap();
    resume.notify_one();
    first.await.unwrap().unwrap();
    let observed = (
        hook::seen(message).outcomes,
        rt.queue().await,
        busy.pane.keys(),
        rt.marks(message),
    );
    let refused = vec!["NotSent(\"queue_nonempty\")".to_string()];
    // Intake merges the message into the input queued just before it.
    let queue = vec!["meanwhile\nstatus?".to_string()];
    assert_eq!(observed, (refused, queue, vec![], vec!["➕".to_string()]));
}

/// Starts `message` and returns once its owner holds the reservation, parked at the first pane
/// capture until `go`.
async fn parked_at_capture(busy: &Busy, message: u64) -> tokio::task::JoinHandle<()> {
    busy.pane.set("hold", "");
    let task = busy.rt.h.spawn_user_message(message, "status?");
    let at_hold = busy.pane.path("at_hold");
    let parked = wait_until(WAIT, move || {
        let at_hold = at_hold.clone();
        Box::pin(async move { at_hold.exists() })
    });
    assert!(parked.await, "the owner reached its first pane capture");
    tokio::spawn(async move { task.await.unwrap().unwrap() })
}

/// A vetoed paste hands the message back ahead of input queued meanwhile, also past a failed
/// first write: marked queued, never 📥, the checkpoint left to the queue.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_vetoed_paste_hands_the_message_back_ahead_of_input_sent_meanwhile_pg() {
    let busy = busy().await;
    let rt = &busy.rt;
    busy.pane.draft();
    let message = fresh_id();
    let request = parked_at_capture(&busy, message).await;
    send_meanwhile(&rt.h.shared, CHANNEL_ID, fresh_id(), "meanwhile").await;
    crate::services::turn_orchestrator::test_support::fail_queue_saves(
        ChannelId::new(CHANNEL_ID),
        1,
    );
    busy.pane.set("go", "");
    request.await.unwrap();
    let observed = (
        rt.queue().await,
        rt.marks(message),
        rt.h.checkpoint(),
        busy.pane.keys(),
        in_memory(message),
    );
    let queue = vec!["status?".to_string(), "meanwhile".to_string()];
    assert_eq!(
        observed,
        (queue, vec!["📬".to_string()], None, vec![], None)
    );
}

/// A handback whose four writes all fail ends the message with the owner's refusal notice: it is
/// neither queued behind input sent meanwhile nor marked, and the checkpoint stays before it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_handback_that_never_lands_ends_the_message_with_a_notice_pg() {
    let busy = busy().await;
    let rt = &busy.rt;
    rt.hold_mailbox().await;
    busy.pane.draft();
    let message = fresh_id();
    let request = parked_at_capture(&busy, message).await;
    send_meanwhile(&rt.h.shared, CHANNEL_ID, fresh_id(), "meanwhile").await;
    crate::services::turn_orchestrator::test_support::fail_queue_saves(
        ChannelId::new(CHANNEL_ID),
        5,
    );
    busy.pane.set("go", "");
    request.await.unwrap();
    let replies =
        rt.h.messages()
            .into_iter()
            .filter(|(to, _)| *to == Some(message));
    let observed = (
        hook::seen(message).outcomes,
        rt.queue().await,
        rt.marks(message),
        rt.h.checkpoint(),
        replies.count(),
        rt.notices().len(),
    );
    let failed = vec!["HandbackFailed(\"handback_persistence\")".to_string()];
    let left = vec!["meanwhile".to_string()];
    assert_eq!(observed, (failed, left, vec![], None, 1, 1));
}

/// A handback whose answer is lost, after the write or before it, is never resent or queued
/// again by intake: the queue holds the message at most once and the person is told once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_handback_with_an_unknown_answer_is_neither_resent_nor_queued_again_pg() {
    let mut observed = Vec::new();
    for fault in [
        inject_hook::HandbackFault::AnswerLost,
        inject_hook::HandbackFault::Dropped,
    ] {
        let busy = busy().await;
        let rt = &busy.rt;
        rt.hold_mailbox().await;
        busy.pane.draft();
        inject_hook::fault_handback(CHANNEL_ID, fault);
        let message = fresh_id();
        rt.h.deliver_user_message(message, "status?").await.unwrap();
        observed.push((
            hook::seen(message).outcomes,
            rt.queue().await,
            rt.marks(message),
            rt.h.checkpoint(),
            rt.notices(),
            busy.pane.keys(),
        ));
    }
    let unknown = vec!["HandbackFailed(\"handback_unknown\")".to_string()];
    let notice = vec![QUEUE_UNKNOWN.to_string()];
    let written = vec!["status?".to_string()];
    let lost = (
        unknown.clone(),
        written,
        vec![],
        None,
        notice.clone(),
        vec![],
    );
    let dropped = (unknown, vec![], vec![], None, notice, vec![]);
    assert_eq!(observed, [lost, dropped]);
}

/// An intake task aborted while the pane takes the paste leaves the owner to finish: the
/// injection is recorded in memory and on disk and the claim on the message ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_aborted_intake_leaves_the_owner_to_record_the_injection_pg() {
    let busy = busy().await;
    let rt = &busy.rt;
    busy.pane.set("gate", "");
    let message = fresh_id();
    let task = rt.h.spawn_user_message(message, "status?");
    let at_gate = busy.pane.path("at_gate");
    let gated = wait_until(WAIT, move || {
        let at_gate = at_gate.clone();
        Box::pin(async move { at_gate.exists() })
    });
    assert!(gated.await, "the paste reached the pane");
    task.abort();
    busy.pane.set("go", "");
    let recorded = wait_until(WAIT, move || {
        Box::pin(async move { on_disk(message).is_some() })
    });
    let recorded = recorded.await;
    let source = disposition::test_support::source_entry(&ProviderKind::Claude, message);
    let observed = (recorded, in_memory(message), source, busy.pane.keys().len());
    assert_eq!(observed, (true, Some(InjectionOutcome::Observed), None, 2));
}

/// An owner that dies outside the pane effect tells the person the input is unknown, marks
/// nothing, keeps the checkpoint before the message, records no injection and frees the message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_owner_that_dies_outside_the_effect_reports_the_input_unknown_pg() {
    let busy = busy().await;
    let rt = &busy.rt;
    inject_hook::crash_owner(CHANNEL_ID);
    let message = fresh_id();
    rt.h.deliver_user_message(message, "status?").await.unwrap();
    let source = disposition::test_support::source_entry(&ProviderKind::Claude, message);
    let observed = (
        hook::seen(message).outcomes,
        rt.notices(),
        rt.marks(message),
        rt.h.checkpoint(),
        in_memory(message),
        source,
    );
    let failed = vec!["OwnerFailed { turn_id: None }".to_string()];
    let notice = vec![UNCONFIRMED.to_string()];
    assert_eq!(observed, (failed, notice, vec![], None, None, None));
}

/// A text command on a gated busy channel runs as before: looked up, recorded as consumed, and
/// never offered, claimed or recorded as an injection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_text_command_on_a_gated_channel_is_never_offered_pg() {
    let busy = busy().await;
    let rt = &busy.rt;
    rt.hold_mailbox().await;
    let message = fresh_id();
    rt.h.deliver_user_message(message, "!stop").await.unwrap();
    let observed = (
        consumed(message),
        hook::seen(message).offers,
        disposition::test_support::calls(message),
        on_disk(message),
        busy.pane.keys(),
    );
    assert_eq!(observed, (true, 0, vec!["taken"], None, vec![]));
}

/// Whether intake recorded `message` as a consumed text command.
fn consumed(message: u64) -> bool {
    let root = crate::services::discord::runtime_store::last_message_root().expect("runtime root");
    let path = root
        .join("claude")
        .join(format!("{CHANNEL_ID}.consumed.json"));
    std::fs::read_to_string(path).is_ok_and(|raw| raw.contains(&message.to_string()))
}

/// Before startup recovery ends, and while a restart drains, the hook passes text on untouched:
/// no claim, no paste, the queue takes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_recovery_and_a_restart_drain_keep_text_from_the_pane_pg() {
    let busy = busy().await;
    let rt = &busy.rt;
    rt.hold_mailbox().await;
    let restart = &rt.h.shared.restart;
    let mut observed = Vec::new();
    for (case, reconciled, draining) in [("recovery", false, false), ("drain", true, true)] {
        restart.reconcile_done.store(reconciled, Ordering::SeqCst);
        restart.restart_pending.store(draining, Ordering::SeqCst);
        let message = fresh_id();
        rt.h.deliver_user_message(message, case).await.unwrap();
        let calls = disposition::test_support::calls(message);
        observed.push((case, hook::seen(message).offers, calls, busy.pane.keys()));
    }
    let passed = |case| (case, 1, vec!["taken"], vec![]);
    assert_eq!(observed, [passed("recovery"), passed("drain")]);
}

/// Set in the child process the gate-off scenario re-runs itself in, so no other test built the table.
const OFF_CHILD: &str = "ADK_BUSY_INJECT_OFF_CHILD";

/// With the gate off, a busy channel queues text and runs commands as before, and no injection
/// step is reached: no lookup or claim call, and no injection table built.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_channel_outside_the_gate_reaches_no_injection_step_pg() {
    if std::env::var_os(OFF_CHILD).is_none() {
        let module = module_path!().split_once("::").unwrap().1;
        let name = "a_channel_outside_the_gate_reaches_no_injection_step_pg";
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("{module}::{name}"),
                "--test-threads=1",
                "--nocapture",
            ])
            .env(OFF_CHILD, "1")
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&output.stdout).to_string()
            + &String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success() && log.contains("1 passed"), "{log}");
        return;
    }
    let rt = runtime().await;
    let pane = InjectPane::new(CHANNEL_ID, "off");
    rt.hold_mailbox().await;
    let (text, command) = (fresh_id(), fresh_id());
    rt.h.deliver_user_message(text, "status?").await.unwrap();
    rt.h.deliver_user_message(command, "!stop").await.unwrap();
    let calls = [text, command].map(disposition::test_support::calls);
    let observed = (
        rt.queue().await,
        rt.marks(text),
        pane.keys(),
        consumed(command),
        calls,
        hook::seen(text),
        disposition::test_support::table_built(),
    );
    let queued = vec!["status?".to_string()];
    let none = hook::Seen::default();
    let base = (
        queued,
        vec!["📬".to_string()],
        vec![],
        true,
        [vec![], vec![]],
        none,
        false,
    );
    assert_eq!(observed, base);
}
