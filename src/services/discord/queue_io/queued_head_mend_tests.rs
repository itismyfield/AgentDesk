//! A queued head left Unknown is consumed with no pane input once its transcript proves Claude took
//! it, from the automatic entry points alone; a closed gate, a clear or a failed write leaves it be.

use super::*;
use crate::services::claude_tui::busy_inject::frame;

const SESSION: &str = InjectPane::SESSION;
/// A Claude session other than the one the pane's transcript file is named for.
const OTHER: &str = "7d3e9a10-2c4b-4f6e-8a1d-5b9c0e2f4a6d";
const NONCE: &str = "0a1b2c3d";
/// Older than every queue clock here, as the fake pane stamps its own records.
const OLD_STAMP: &str = "2026-10-06T00:00:00.000Z";

/// Claude's records of a queued paste it absorbed mid-turn: the enqueue, its removal, and the human
/// prompt command naming the removal's ids.
fn absorbed(prompt: &str, stamp: &str) -> String {
    absorbed_in(prompt, stamp, SESSION)
}

/// As `absorbed`, recorded under `session`.
fn absorbed_in(prompt: &str, stamp: &str, session: &str) -> String {
    let (command, delivery) = ("s2b-command", "s2b-delivery");
    [
        json!({"type": "queue-operation", "operation": "enqueue", "timestamp": stamp,
            "sessionId": session, "content": prompt}),
        json!({"type": "queue-operation", "operation": "remove", "timestamp": stamp,
            "sessionId": session, "content": prompt, "reason": "absorbed_mid_turn",
            "commandUuid": command, "deliveryId": delivery}),
        json!({"type": "attachment", "timestamp": stamp, "sessionId": session,
            "attachment": {"type": "queued_command", "prompt": prompt, "source_uuid": command,
            "delivery_id": delivery, "commandMode": "prompt", "origin": {"kind": "human"},
            "humanTurn": true}}),
    ]
    .map(|record| record.to_string())
    .join("\n")
}

fn now_stamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

impl Fixture {
    /// H's frame under `nonce`, in the paste block Claude records it in.
    fn wrapped_head(&self, head: MessageId, nonce: &str) -> String {
        let source = format!("discord:{}:{head}", self.channel);
        paste_block(
            "b0ac",
            &frame(&source, "200", AUTHOR_NAME, nonce, "status?"),
            "b0ac",
        )
    }

    fn append(&self, lines: &str) {
        use std::io::Write;
        let path = self.pane.path(InjectPane::TRANSCRIPT);
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(file, "{lines}").unwrap();
    }

    /// Leaves H Unknown through a paste that failed after its attempt was recorded.
    async fn unknown_head(&self) -> MessageId {
        let head = self.queue().await[0].message_id;
        self.pane.set("fail_paste", "");
        self.kick().await;
        std::fs::remove_file(self.pane.path("fail_paste")).unwrap();
        assert_eq!(self.disk()["head_injection"]["phase"], "unknown");
        assert!(
            self.pane.keys().is_empty(),
            "the failed paste typed nothing"
        );
        head
    }

    /// The nonce the Unknown head's recorded attempt names.
    fn recorded_nonce(&self) -> String {
        let attempt = &self.disk()["head_injection"]["attempt"];
        let nonce = attempt["nonce"].as_str();
        nonce
            .expect("the attempt is saved before the paste")
            .to_string()
    }

    /// The record an older binary left: the same Unknown head without its attempt.
    fn forget_attempt(&self) {
        let mut disk = self.disk();
        let head = disk["head_injection"].as_object_mut().unwrap();
        assert!(head.remove("attempt").is_some());
        std::fs::write(self.path(), serde_json::to_string_pretty(&disk).unwrap()).unwrap();
    }

    fn reconciled_rows(&self) -> usize {
        let rows = self.rows();
        let reconciled = rows.iter().filter(|row| row.outcome == "reconciled");
        reconciled
            .filter(|row| row.head_state == "terminal")
            .count()
    }

    /// Binds the pane to `file` in its directory, the binding naming `session` or none.
    fn bind(&self, file: &str, session: Option<&str>) {
        let binding = crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
            output_path: self.pane.path(file).display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: session.map(str::to_string),
            last_offset: 0,
            relay_last_offset: None,
        };
        let register = crate::services::tui_prompt_dedupe::register_tmux_runtime_binding;
        register(&self.pane.session(), binding);
    }

    /// A kick that leaves the Unknown head, its queue file and the pane as they were.
    async fn assert_kept(&self, case: &str) {
        let bytes = self.bytes();
        self.kick().await;
        assert_eq!(self.bytes(), bytes, "{case}: the queue file is untouched");
        assert!(self.terminal().is_empty(), "{case}");
        assert_eq!(self.reconciled_rows(), 0, "{case}");
        assert!(self.pane.keys().is_empty(), "{case}: nothing was typed");
    }

    async fn clear_mailbox(&self) {
        let context = crate::services::discord::queue_persistence_context;
        let persistence = context(&self.shared, &CLAUDE, self.channel);
        let _ = self.shared.mailbox(self.channel).clear(persistence).await;
    }
}

/// An Unknown head an older binary recorded, whose absorbed paste the transcript shows past any
/// 32 MiB tail, is consumed after a restart untyped, busy or idle; the tail follows once.
#[tokio::test(flavor = "current_thread")]
async fn s3_a_legacy_unknown_head_proven_taken_is_consumed_untyped_busy_or_idle_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (channel, busy) in [(6_813_441, true), (6_813_442, false)] {
        let mut fixture = Fixture::new(channel, "all").await;
        let _gate = router::busy_inject_support::open_gate(channel);
        let queue = fixture.queue().await;
        let (h, b) = (queue[0].message_id, queue[1].message_id);
        fixture.unknown_head().await;
        fixture.forget_attempt();
        fixture.append(&absorbed(&fixture.wrapped_head(h, NONCE), &now_stamp()));
        if !busy {
            let pad = "x".repeat(33 << 20);
            fixture.append(&format!("{{\"type\":\"progress\",\"pad\":\"{pad}\"}}"));
            fixture.append("{\"type\":\"result\"}");
            fixture.pane.idle();
        }
        fixture.restart(busy).await;
        let calls = fixture.pane.tmux_calls();
        if busy {
            queue_io::spawn_turn_completion_idle_queue_listener(fixture.shared.clone(), CLAUDE);
        } else {
            let reason = "idle_queue_backstop";
            let shared = &fixture.shared;
            queue_io::kick_queued_only_channel_if_context_available(
                shared,
                &CLAUDE,
                fixture.channel,
                reason,
            )
            .await;
        }
        until("the proven head is consumed", || {
            fixture.terminal() == [h.get()]
        })
        .await;
        assert_eq!(
            fixture.pane.tmux_calls(),
            calls,
            "busy={busy}: H was not typed"
        );
        assert!(fixture.disk()["head_injection"].is_null());
        assert_eq!(fixture.reconciled_rows(), 1, "busy={busy}");
        if busy {
            fixture.slow_check().await;
            until("B follows", || fixture.terminal() == [h.get(), b.get()]).await;
            assert_eq!(fixture.pane.keys(), ["paste-buffer", "send-keys"]);
            let buffer = std::fs::read_to_string(fixture.pane.path("buffer")).unwrap();
            assert!(buffer.ends_with("\ndistinct tail B"));
            fixture.assert_still_busy().await;
        } else {
            // The idle dequeue that follows takes B, and nothing was typed.
            let restarted = ChannelMailboxRegistry::default().handle(fixture.channel);
            let context = crate::services::discord::queue_persistence_context;
            let persistence = context(&fixture.shared, &CLAUDE, fixture.channel);
            let take = restarted.take_next_soft(persistence).await;
            assert_eq!(take.intervention.map(|item| item.message_id), Some(b));
            assert!(fixture.pane.keys().is_empty());
        }
    }
}

/// A paste recorded before it started, then left Unknown by a failed write, is consumed after a
/// restart by the listener alone; H is typed once in all, and B follows at the slow check.
#[tokio::test(flavor = "current_thread")]
async fn s3_a_recorded_paste_left_unknown_is_consumed_after_a_restart_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let mut fixture = Fixture::new(6_813_443, "all").await;
    let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
    let queue = fixture.queue().await;
    let (h, b) = (queue[0].message_id, queue[1].message_id);
    let block = paste_block("b0ac", "@BODY@", "b0ac");
    fixture.pane.set("receipt", &absorbed(&block, OLD_STAMP));
    let (after_effect, resume) = inject_hook::park_queued(fixture.channel.get(), "after_effect");
    queue_io::schedule_post_enqueue_idle_queue_kick(
        fixture.shared.clone(),
        CLAUDE,
        fixture.channel,
    );
    reached(&after_effect).await;
    crate::services::turn_orchestrator::test_support::fail_queue_saves(fixture.channel, 1);
    resume.notify_one();
    until("the paste is left Unknown", || {
        fixture
            .rows()
            .iter()
            .any(|row| row.outcome == "unconfirmed")
    })
    .await;
    assert_eq!(fixture.disk()["head_injection"]["phase"], "unknown");
    assert_eq!(fixture.pane.keys(), ["paste-buffer", "send-keys"]);

    fixture.restart(true).await;
    std::fs::remove_file(fixture.pane.path("pasted")).unwrap();
    queue_io::spawn_turn_completion_idle_queue_listener(fixture.shared.clone(), CLAUDE);
    until("the recorded paste is consumed", || {
        fixture.terminal() == [h.get()]
    })
    .await;
    assert_eq!(
        fixture.pane.keys(),
        ["paste-buffer", "send-keys"],
        "H was typed once in all"
    );
    fixture.slow_check().await;
    until("B follows", || fixture.terminal() == [h.get(), b.get()]).await;
    assert_eq!(fixture.pane.keys().len(), 4);
    fixture.assert_still_busy().await;
}

/// With the mode, the queued-head switch or the channel gate closed, the listener and the slow
/// check leave a proven Unknown head's queue file and pane as they were; opened again, it is consumed.
#[tokio::test(flavor = "current_thread")]
async fn s3_a_closed_gate_holds_a_proven_unknown_head_until_it_opens_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (channel, closed) in [
        (6_813_444, "mode"),
        (6_813_445, "queued"),
        (6_813_446, "list"),
    ] {
        let fixture = Fixture::new(channel, "all").await;
        let mut gate = Some(router::busy_inject_support::open_gate(channel));
        let h = fixture.unknown_head().await;
        let nonce = fixture.recorded_nonce();
        fixture.append(&absorbed(&fixture.wrapped_head(h, &nonce), &now_stamp()));
        let forced = inject_hook::forced(channel).unwrap();
        match closed {
            "mode" => {
                inject_hook::clear(channel);
                inject_hook::set_queued_head(channel, true);
            }
            "queued" => inject_hook::set_queued_head(channel, false),
            _ => drop(gate.take()),
        }
        let (bytes, calls, rows) = (fixture.bytes(), fixture.pane.tmux_calls(), fixture.rows());
        queue_io::spawn_turn_completion_idle_queue_listener(fixture.shared.clone(), CLAUDE);
        until("the listener reconciled", || {
            backstop::listener_completed_reconciles(fixture.channel) > 0
        })
        .await;
        quiet().await;
        let producer = "s3 gate-off producer";
        let arm = queue_io::arm_slow_idle_queue_backstop_if_queue_nonempty;
        assert!(arm(&fixture.shared, &CLAUDE, fixture.channel, producer).await);
        fixture.slow_check().await;
        quiet().await;
        assert_eq!(
            fixture.bytes(),
            bytes,
            "{closed}: the queue file is untouched"
        );
        assert_eq!(fixture.pane.tmux_calls(), calls, "{closed}");
        assert_eq!(fixture.rows(), rows, "{closed}: no attempt ran");

        match closed {
            "mode" => inject_hook::set(channel, forced.0, forced.1),
            "queued" => inject_hook::set_queued_head(channel, true),
            _ => gate = Some(router::busy_inject_support::open_gate(channel)),
        }
        assert!(arm(&fixture.shared, &CLAUDE, fixture.channel, producer).await);
        fixture.slow_check().await;
        until("the open gate consumes it", || {
            fixture.terminal() == [h.get()]
        })
        .await;
        assert_eq!(
            fixture.pane.tmux_calls(),
            calls,
            "{closed}: H was not typed"
        );
        drop(gate);
    }
}

/// A clear during the proof read restores nothing, one after the consume supersedes B untyped, and
/// a file the actor's own clear rewrote back or failed to rewrite holds no head it consumes.
#[tokio::test(flavor = "current_thread")]
async fn s3_a_clear_around_the_proof_neither_revives_nor_types_the_head_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    {
        let fixture = Fixture::new(6_813_447, "all").await;
        let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
        let h = fixture.unknown_head().await;
        let nonce = fixture.recorded_nonce();
        fixture.append(&absorbed(&fixture.wrapped_head(h, &nonce), &now_stamp()));
        let (at_commit, resume) = inject_hook::park_queued(fixture.channel.get(), "mend_commit");
        let caller = fixture.spawn_kick();
        reached(&at_commit).await;
        fixture.clear_mailbox().await;
        resume.notify_one();
        caller.await.unwrap();
        assert!(fixture.queue().await.is_empty(), "nothing was restored");
        assert!(fixture.terminal().is_empty());
        let disk = fixture.disk();
        assert!(disk.is_array() || disk["head_injection"].is_null());
        assert_eq!(fixture.reconciled_rows(), 0);
        assert!(fixture.pane.keys().is_empty());
    }
    {
        let fixture = Fixture::new(6_813_448, "all").await;
        let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
        let h = fixture.unknown_head().await;
        let nonce = fixture.recorded_nonce();
        fixture.append(&absorbed(&fixture.wrapped_head(h, &nonce), &now_stamp()));
        fixture.kick().await;
        assert_eq!(fixture.terminal(), [h.get()]);
        fixture.clear_mailbox().await;
        fixture.kick().await;
        assert!(fixture.queue().await.is_empty(), "B was superseded");
        assert_eq!(fixture.terminal(), [h.get()], "H stays submitted");
        assert!(fixture.pane.keys().is_empty(), "neither H nor B was typed");
    }
    {
        let fixture = Fixture::new(6_813_449, "all").await;
        let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
        let h = fixture.unknown_head().await;
        let nonce = fixture.recorded_nonce();
        fixture.append(&absorbed(&fixture.wrapped_head(h, &nonce), &now_stamp()));
        let stale = fixture.bytes();
        fixture.clear_mailbox().await;
        std::fs::write(fixture.path(), &stale).unwrap();
        fixture.kick().await;
        quiet().await;
        assert_eq!(fixture.bytes(), stale, "nothing was written over it");
        assert!(fixture.queue().await.is_empty());
        assert_eq!(fixture.reconciled_rows(), 0);
        assert!(fixture.pane.keys().is_empty());
    }
    {
        let fixture = Fixture::new(6_813_475, "all").await;
        let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
        let h = fixture.unknown_head().await;
        let nonce = fixture.recorded_nonce();
        fixture.append(&absorbed(&fixture.wrapped_head(h, &nonce), &now_stamp()));
        let stale = fixture.bytes();
        let fail = crate::services::turn_orchestrator::ChannelMailboxRegistry::fail_clear_removals;
        fail(fixture.channel, false, 2, false);
        fail(fixture.channel, true, 2, false);
        crate::services::turn_orchestrator::test_support::fail_queue_saves(fixture.channel, 2);
        fixture.clear_mailbox().await;
        assert_eq!(fixture.bytes(), stale, "the failed clear left the old file");
        fixture.kick().await;
        quiet().await;
        let context = crate::services::discord::queue_persistence_context;
        let persistence = context(&fixture.shared, &CLAUDE, fixture.channel);
        let mailbox = fixture.shared.mailbox(fixture.channel);
        assert!(mailbox.unknown_queued_head(persistence).await.is_none());
        assert_eq!(fixture.bytes(), stale, "nothing was written over it");
        assert!(fixture.queue().await.is_empty(), "nothing was restored");
        assert_eq!(fixture.reconciled_rows(), 0);
        assert!(fixture.pane.keys().is_empty(), "neither H nor B was typed");
    }
}

/// A second entry point while the first holds the proof reaches neither the pane nor the head, and
/// a failed commit keeps the head Unknown before the same queue; the next kick consumes it once.
#[tokio::test(flavor = "current_thread")]
async fn s3_an_overlapping_kick_or_a_failed_write_never_consumes_twice_or_types_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::new(6_813_450, "all").await;
    let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
    let queue = fixture.queue().await;
    let (h, b) = (queue[0].message_id, queue[1].message_id);
    fixture.unknown_head().await;
    let nonce = fixture.recorded_nonce();
    fixture.append(&absorbed(&fixture.wrapped_head(h, &nonce), &now_stamp()));
    let (at_commit, resume) = inject_hook::park_queued(fixture.channel.get(), "mend_commit");
    let first = fixture.spawn_kick();
    reached(&at_commit).await;
    let reason = "idle_queue_backstop";
    queue_io::kick_queued_only_channel_if_context_available(
        &fixture.shared,
        &CLAUDE,
        fixture.channel,
        reason,
    )
    .await;
    assert_eq!(fixture.disk()["head_injection"]["phase"], "unknown");
    crate::services::turn_orchestrator::test_support::fail_queue_saves(fixture.channel, 1);
    resume.notify_one();
    first.await.unwrap();
    assert_eq!(fixture.disk()["head_injection"]["phase"], "unknown");
    assert_eq!(fixture.queue().await.len(), 2);
    assert!(fixture.terminal().is_empty());
    fixture.kick().await;
    assert_eq!(fixture.terminal(), [h.get()]);
    assert_eq!(fixture.queue().await[0].message_id, b);
    assert_eq!(fixture.reconciled_rows(), 1);
    assert!(fixture.pane.keys().is_empty());
}

/// Proof counts only in the Claude session the bound transcript file is named for: records of
/// another session or a binding naming another session consume nothing; a binding naming it does.
#[tokio::test(flavor = "current_thread")]
async fn s3_a_proof_counts_only_in_the_session_its_bound_file_is_named_for_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (channel, recorded_in, bound) in
        [(6_813_471, OTHER, None), (6_813_474, SESSION, Some(OTHER))]
    {
        let fixture = Fixture::new(channel, "all").await;
        let _gate = router::busy_inject_support::open_gate(channel);
        let h = fixture.unknown_head().await;
        let head = fixture.wrapped_head(h, &fixture.recorded_nonce());
        fixture.append(&absorbed_in(&head, &now_stamp(), recorded_in));
        fixture.bind(InjectPane::TRANSCRIPT, bound);
        let case = format!("records of {recorded_in}, binding {bound:?}");
        fixture.assert_kept(&case).await;
        if bound.is_some() {
            fixture.bind(InjectPane::TRANSCRIPT, Some(SESSION));
            fixture.kick().await;
            assert_eq!(fixture.terminal(), [h.get()]);
            assert_eq!(fixture.reconciled_rows(), 1);
            assert!(fixture.pane.keys().is_empty(), "H was not typed");
        }
    }
}

/// A paste that cannot be named in its transcript before it starts, the file unreadable or not
/// named for a session, is handed back untyped, and typed once at the next kick that can name it.
#[tokio::test(flavor = "current_thread")]
async fn s3_an_unnamed_paste_is_handed_back_untyped_and_typed_once_named_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (channel, unnamed) in [(6_813_472, "unreadable"), (6_813_473, "unnamed")] {
        let fixture = Fixture::new(channel, "all").await;
        let _gate = router::busy_inject_support::open_gate(channel);
        let queue = fixture.queue().await;
        let (h, b) = (queue[0].message_id, queue[1].message_id);
        let transcript = fixture.pane.path(InjectPane::TRANSCRIPT);
        if unnamed == "unreadable" {
            let moved = fixture.pane.path("moved");
            let (at_name, resume) = inject_hook::park_queued(channel, "name_paste");
            let caller = fixture.spawn_kick();
            reached(&at_name).await;
            std::fs::rename(&transcript, &moved).unwrap();
            resume.notify_one();
            caller.await.unwrap();
            std::fs::rename(&moved, &transcript).unwrap();
        } else {
            std::fs::copy(&transcript, fixture.pane.path("legacy.jsonl")).unwrap();
            fixture.bind("legacy.jsonl", None);
            fixture.kick().await;
            fixture.bind(InjectPane::TRANSCRIPT, None);
        }
        let rows = fixture.rows();
        let row = rows.last().unwrap();
        assert_eq!(
            (row.outcome, row.reason, row.head_state),
            ("not_sent", "queued_paste_unnamed", "queued"),
            "{unnamed}"
        );
        assert!(fixture.pane.keys().is_empty(), "{unnamed}: nothing typed");
        assert!(fixture.disk()["head_injection"].is_null(), "{unnamed}");
        let ids = |queue: Vec<Intervention>| queue.iter().map(|i| i.message_id).collect::<Vec<_>>();
        assert_eq!(
            ids(fixture.queue().await),
            [h, b],
            "{unnamed}: the queue is kept"
        );

        fixture.kick().await;
        assert_eq!(fixture.terminal(), [h.get()], "{unnamed}");
        assert_eq!(fixture.pane.keys(), ["paste-buffer", "send-keys"]);
    }
}
