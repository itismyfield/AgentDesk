//! `/effort` on a local Claude TUI ends it at once on its own session: the running turn alone is
//! settled, its transcript end is recorded, and one continuation is admitted ahead of waiting input.

use super::model_tests::TOKEN;
use super::*;
use crate::services::discord::commands::tui_passthrough::effort_save_notice;
use crate::services::discord::herdr_terminate::test_turn;
use crate::services::discord::queue_io::input_owner::ingress;
use crate::services::provider::CancelToken;
use crate::services::tui_o::store::restart_end;
use serde_json::json;

const BOUND: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OLD_TURN: &str = "effort-old-turn";
const CONTINUE: &str =
    "effort 변경 처리를 위해 세션을 다시 시작했으니 중단된 작업을 이어서 진행하라";

fn line(value: Value) -> String {
    format!("{value}\n")
}

impl Fixture {
    fn transcript(&self) -> PathBuf {
        let path = crate::services::claude_tui::transcript_tail::claude_transcript_path;
        path(self.cwd.path(), SESSION, Some(&self._account.2)).unwrap()
    }

    /// The pane's transcript holds a turn with no closer, and the channel's binding log names it.
    fn open_transcript(&self) {
        let prompt = json!({"type":"user","uuid":"old-question","timestamp":"2026-11-14T00:00:00Z",
            "message":{"role":"user","content":"do the long work"}});
        let body = json!({"type":"assistant","uuid":"row-old-body","apiBlockIndex":0,
            "message":{"id":"old-body","role":"assistant","content":[{"type":"text","text":"partial"}]}});
        std::fs::write(self.transcript(), line(prompt) + &line(body)).unwrap();
        let source = crate::services::tui_o::shadow::binding_reader::source_id_for;
        let source = source(SESSION, &self.transcript()).unwrap();
        let now = chrono::Utc::now();
        let event = json!({"seq":1,"channel_id":self.channel.get(),"provider":"claude",
            "tmux_session":self.name,"execution_nonce":BOUND,"old":null,
            "new":{"source":source},"cause":"startup","parent_hint":null,
            "evidence":{"hook_event":"SessionStart","received_at":now},"committed_at":now});
        // Test builds read the binding log only under a root set on the reading thread.
        let root = crate::config::runtime_root().unwrap();
        crate::services::tui_prompt_dedupe::binding_events::set_test_root(Some(&root));
        let dir = root.join("binding_events");
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join(format!("{}.log", self.channel.get()));
        std::fs::write(log, line(event)).unwrap();
    }

    /// A waiting user input in the channel's adopted owner queue.
    async fn waiting_user(&self, id: u64) {
        // A booted server has a generation; originals queued under none are refused.
        let generation = crate::services::discord::runtime_store::generation_path().unwrap();
        std::fs::create_dir_all(generation.parent().unwrap()).unwrap();
        std::fs::write(generation, "5").unwrap();
        let adopted = ingress::adopt(&self.shared, &ProviderKind::Claude, self.channel).await;
        assert_eq!(adopted, Ok(true), "the owner adopts the channel");
        let user = json!({"author_id":7,"message_id":id,"channel_id":self.channel.get(),
            "source_message_ids":[id],"queued_generation":7,"text":format!("user-{id}")});
        let user = serde_json::from_value(user).unwrap();
        let mailbox = self.shared.mailbox(self.channel);
        mailbox
            .v2_admit_ticket(user, format!("user-{id}"))
            .await
            .unwrap();
    }

    /// Turn `OLD_TURN` runs on the pane with an open transcript and one user waiting behind it.
    async fn busy(&self) -> Arc<CancelToken> {
        self.open_transcript();
        self.waiting_user(101).await;
        test_turn::start(&self.shared, self.channel, 9_001, OLD_TURN).await
    }

    /// `/effort level` as the command runs it, on this node.
    async fn effort_now(&self, level: &str) -> String {
        let keys = &self.shared.overrides.fast_mode_session_reset_pending;
        keys.clear();
        let owner = self.own(None).await;
        let _ = self.shared.http.cached_bot_token.set(TOKEN.to_string());
        let provider = &ProviderKind::Claude;
        let said = effort_save_notice(&self.shared, TOKEN, self.channel, provider, level).await;
        self.own(Some(owner)).await;
        said
    }

    /// The owner's stored snapshot: its queue and its restart intent.
    async fn owner(&self) -> Value {
        let mailbox = self.shared.mailbox(self.channel);
        let read = mailbox.v2_request(|slot| slot.as_ref().ok_or("no owner")?.json());
        read.await.unwrap()
    }

    fn ends(&self) -> Vec<restart_end::RestartEnd> {
        let root = crate::config::runtime_root().unwrap();
        restart_end::read(&root, self.channel.get()).unwrap()
    }

    async fn saved_level(&self) -> (Option<String>, bool) {
        let key = self.channel.get().to_string();
        let settings = self.shared.settings.read().await;
        let pending = settings.channel_relaunch_pending.contains(&key);
        (settings.channel_effort_levels.get(&key).cloned(), pending)
    }
}

/// Where each of `texts` first appears among the owner's queued items, in queue order.
fn queued(owner: &Value, texts: &[&str]) -> Vec<Option<usize>> {
    let items = owner["items"].as_array().unwrap();
    let at = |text: &&str| {
        items
            .iter()
            .position(|item| item.to_string().contains(*text))
    };
    texts.iter().map(at).collect()
}

/// How many interruption notices the outbox holds for the channel.
async fn notices(fx: &Fixture) -> i64 {
    let pool = fx.shared.pg_pool.as_ref().unwrap();
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM message_outbox WHERE target = $1 AND reason_code = $2",
    )
    .bind(format!("channel:{}", fx.channel.get()))
    .bind("effort.restart_interrupted");
    count.fetch_one(pool).await.unwrap()
}

fn continuations(owner: &Value) -> usize {
    let items = owner["items"].as_array().unwrap();
    let held = |item: &&Value| item.to_string().contains(CONTINUE);
    items.iter().filter(held).count()
}

// A running turn does not defer the restart: the TUI ends now on its own session, only that turn
// is settled, its transcript end is recorded, and its one continuation is queued ahead of the user.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_effort_restarts_now_same_session_pg() {
    let test = "services::discord::commands::control::relaunch_tests::restart_tests::busy_effort_restarts_now_same_session_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fx = Fixture::new(40, RuntimeHandoffKind::ClaudeTui).await;
    ingress::ADOPTS
        .scope(true, async {
            let token = fx.busy().await;
            fx.tmux.take_calls();
            let said = fx.effort_now("high").await;
            assert!(said.contains("effort를 `high`로 저장했습니다"), "{said}");
            assert!(said.contains("지금 다시 시작"), "{said}");
            let calls = fx.tmux.take_calls();
            assert!(
                called(&calls, "kill-session"),
                "the TUI ends now: {calls:?}"
            );
            assert!(!called(&calls, "send-keys"), "nothing is typed: {calls:?}");

            assert_eq!(
                fx.session_id().await.as_deref(),
                Some(SESSION),
                "session kept"
            );
            let kept = (Some("work".to_string()), Some(format!("{SESSION}\nwork")));
            assert_eq!(fx.markers(), kept, "account markers kept");
            assert_eq!(fx.saved_level().await, (Some("high".to_string()), false));

            let active = test_turn::active(&fx.shared, fx.channel).await;
            assert!(active.is_none(), "the cut turn no longer holds the channel");
            assert_eq!(test_turn::inflight(fx.channel), None, "its row is settled");
            let cancelled = token.cancelled.load(Ordering::Relaxed);
            assert!(!cancelled, "a restart is not a user cancel of the turn");

            let ends = fx.ends();
            assert_eq!(ends.len(), 1, "one end for the one open turn");
            let length = std::fs::metadata(fx.transcript()).unwrap().len();
            let end = (ends[0].cutoff, ends[0].native_turn_id.as_deref());
            assert_eq!(end, (length, Some("old-question")));
            let named = (
                ends[0].tmux_session.as_str(),
                ends[0].execution_nonce.as_str(),
            );
            assert_eq!(
                named,
                (fx.name.as_str(), BOUND),
                "the end names the old process"
            );

            let owner = fx.owner().await;
            let intent = &owner["effort_restart"];
            assert_eq!(intent["native_session"], SESSION, "{intent}");
            assert_eq!(intent["interrupted_episode"], OLD_TURN, "{intent}");
            assert_eq!(intent["ready"], true, "{intent}");
            assert_eq!(intent["continuation"]["submitted"], false, "{intent}");
            assert_eq!(continuations(&owner), 1, "{owner}");
            let order = queued(&owner, &[CONTINUE, "user-101"]);
            assert_eq!(
                order,
                [Some(0), Some(1)],
                "continuation first, user kept: {owner}"
            );
            assert_eq!(notices(&fx).await, 1, "the cut turn is told once");
            assert!(!said.contains("중단했고"), "the notice says that: {said}");
        })
        .await;
    tc::cleanup_session_temp_files(&fx.name);
}

// The cut turn's own completion, arriving after the restart, names that turn alone: a turn that
// started since keeps the channel, its row and its token.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_old_completion_keeps_successor_pg() {
    let test = "services::discord::commands::control::relaunch_tests::restart_tests::late_old_completion_keeps_successor_pg";
    if !api_child(test) {
        return;
    }
    use crate::services::discord::turn_finalizer::{FinalizeContext, TerminalEvent, TurnKey};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fx = Fixture::new(41, RuntimeHandoffKind::ClaudeTui).await;
    ingress::ADOPTS
        .scope(true, async {
            fx.busy().await;
            let said = fx.effort_now("high").await;
            assert!(said.contains("지금 다시 시작"), "{said}");
            let successor = test_turn::start(&fx.shared, fx.channel, 9_002, "successor").await;

            let generation = fx.shared.restart.current_generation;
            let old =
                TurnKey::new(fx.channel, 9_001, generation).with_episode_nonce(Some(OLD_TURN));
            for event in [TerminalEvent::Complete, TerminalEvent::Cancel] {
                let late = fx.shared.turn_finalizer.submit_terminal(
                    old,
                    ProviderKind::Claude,
                    event,
                    FinalizeContext::watcher(),
                    fx.shared.clone(),
                );
                late.await;
                let active = test_turn::active(&fx.shared, fx.channel).await;
                let same = active.is_some_and(|active| Arc::ptr_eq(&active, &successor));
                assert!(same, "the successor still holds the channel");
                let row = test_turn::inflight(fx.channel);
                assert_eq!(row, Some((9_002, Some("successor".to_string()))));
                assert!(!successor.cancelled.load(Ordering::Relaxed));
            }
            assert_eq!(
                continuations(&fx.owner().await),
                1,
                "still one continuation"
            );
        })
        .await;
    tc::cleanup_session_temp_files(&fx.name);
}

// A TUI that does not end keeps everything: the turn still runs, the user still waits, and no
// continuation, end record or cancel exists. The level stays saved for the next launch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_end_keeps_turn_and_queue_and_owes_no_continuation_pg() {
    let test = "services::discord::commands::control::relaunch_tests::restart_tests::failed_end_keeps_turn_and_queue_and_owes_no_continuation_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fx = Fixture::new(42, RuntimeHandoffKind::ClaudeTui).await;
    ingress::ADOPTS
        .scope(true, async {
            let token = fx.busy().await;
            fx.tmux.fail_kills();
            let said = fx.effort_now("high").await;
            assert!(said.contains("지금은 재기동하지 않았습니다"), "{said}");
            assert!(
                !said.contains("지금 다시 시작"),
                "a failed end is not a restart: {said}"
            );
            let active = test_turn::active(&fx.shared, fx.channel).await;
            let same = active.is_some_and(|active| Arc::ptr_eq(&active, &token));
            assert!(same, "the turn still runs");
            let row = test_turn::inflight(fx.channel);
            assert_eq!(row, Some((9_001, Some(OLD_TURN.to_string()))));
            assert!(!token.cancelled.load(Ordering::Relaxed));
            assert!(
                fx.ends().is_empty(),
                "no end is recorded for a live process"
            );
            let owner = fx.owner().await;
            assert!(owner["effort_restart"].is_null(), "{owner}");
            assert_eq!(
                queued(&owner, &[CONTINUE, "user-101"]),
                [None, Some(0)],
                "{owner}"
            );
            assert_eq!(fx.saved_level().await, (Some("high".to_string()), true));
            assert_eq!(notices(&fx).await, 0, "nothing was cut, nothing is told");
        })
        .await;
    tc::cleanup_session_temp_files(&fx.name);
}

// With no turn running the TUI still ends now, and nothing is owed: no continuation, no end
// record, and the waiting user keeps the front.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_effort_ends_the_tui_and_owes_nothing_pg() {
    let test = "services::discord::commands::control::relaunch_tests::restart_tests::idle_effort_ends_the_tui_and_owes_nothing_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fx = Fixture::new(43, RuntimeHandoffKind::ClaudeTui).await;
    ingress::ADOPTS
        .scope(true, async {
            fx.open_transcript();
            fx.waiting_user(101).await;
            fx.tmux.take_calls();
            let said = fx.effort_now("low").await;
            assert!(said.contains("다음 입력부터 같은 세션"), "{said}");
            assert!(called(&fx.tmux.take_calls(), "kill-session"));
            assert_eq!(fx.session_id().await.as_deref(), Some(SESSION));
            assert!(fx.ends().is_empty());
            let owner = fx.owner().await;
            assert!(owner["effort_restart"].is_null(), "{owner}");
            assert_eq!(
                queued(&owner, &[CONTINUE, "user-101"]),
                [None, Some(0)],
                "{owner}"
            );
            assert_eq!(fx.saved_level().await, (Some("low".to_string()), false));
            assert_eq!(notices(&fx).await, 0);

            // No TUI runs now: a further change is saved and restarts nothing.
            let said = fx.effort_now("high").await;
            assert!(said.contains("실행 중인 TUI가 없어"), "{said}");
            assert!(!called(&fx.tmux.take_calls(), "kill-session"));
        })
        .await;
    tc::cleanup_session_temp_files(&fx.name);
}

// A second change before the continuation is submitted ends the TUI again and keeps the same one
// continuation; a change while a turn runs over that waiting continuation ends nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_effort_keeps_the_one_waiting_continuation_pg() {
    let test = "services::discord::commands::control::relaunch_tests::restart_tests::a_second_effort_keeps_the_one_waiting_continuation_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fx = Fixture::new(44, RuntimeHandoffKind::ClaudeTui).await;
    ingress::ADOPTS
        .scope(true, async {
            fx.busy().await;
            let said = fx.effort_now("high").await;
            assert!(said.contains("지금 다시 시작"), "{said}");
            let first = fx.owner().await["effort_restart"].clone();

            fx.tmux.mark_present();
            fx.tmux.take_calls();
            let said = fx.effort_now("max").await;
            assert!(said.contains("지금 다시 시작"), "{said}");
            assert!(called(&fx.tmux.take_calls(), "kill-session"));
            let owner = fx.owner().await;
            let second = &owner["effort_restart"];
            assert_eq!(
                second["continuation"]["ticket"],
                first["continuation"]["ticket"]
            );
            assert_eq!(second["interrupted_episode"], OLD_TURN);
            assert_eq!(continuations(&owner), 1, "{owner}");
            assert_eq!(
                queued(&owner, &[CONTINUE, "user-101"]),
                [Some(0), Some(1)],
                "{owner}"
            );
            assert_eq!(fx.ends().len(), 1, "the one cut turn has one end");
            assert_eq!(notices(&fx).await, 1, "and one notice");

            fx.tmux.mark_present();
            let running = test_turn::start(&fx.shared, fx.channel, 9_003, "another").await;
            fx.tmux.take_calls();
            let said = fx.effort_now("low").await;
            assert!(said.contains("지금은 재기동하지 않았습니다"), "{said}");
            assert!(!called(&fx.tmux.take_calls(), "kill-session"));
            let active = test_turn::active(&fx.shared, fx.channel).await;
            assert!(active.is_some_and(|active| Arc::ptr_eq(&active, &running)));
            assert_eq!(continuations(&fx.owner().await), 1);
        })
        .await;
    tc::cleanup_session_temp_files(&fx.name);
}

#[path = "relaunch_effort_recovery_tests.rs"]
mod recovery_tests;
