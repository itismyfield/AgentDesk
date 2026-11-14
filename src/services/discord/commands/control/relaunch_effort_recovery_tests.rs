//! What an effort restart leaves for the launch and for a restarted server.

use super::*;
use crate::services::discord::commands::effort_restart::resume_owed;

// A server that stopped after recording a ready continuation and before queueing it admits it on
// restart under its stored ticket, once however often it is asked; one not yet ready is left.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restored_intent_resumes_one_ticket_pg() {
    let test = "services::discord::commands::control::relaunch_tests::restart_tests::recovery_tests::restored_intent_resumes_one_ticket_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fx = Fixture::new(45, RuntimeHandoffKind::ClaudeTui).await;
    ingress::ADOPTS
        .scope(true, async {
            resume_owed(&fx.shared, fx.channel).await;
            assert!(
                fx.owner_or_none().await.is_none(),
                "nothing owed, nothing adopted"
            );

            fx.waiting_user(101).await;
            fx.record_intent(1, false).await;
            resume_owed(&fx.shared, fx.channel).await;
            assert_eq!(continuations(&fx.owner().await), 0, "not ready, not queued");

            let ticket = fx.record_intent(2, true).await;
            let mark = crate::services::claude_tui::session::owner_first_submit;
            for _ in 0..2 {
                resume_owed(&fx.shared, fx.channel).await;
                let owner = fx.owner().await;
                assert_eq!(owner["effort_restart"]["continuation"]["ticket"], ticket);
                assert_eq!(continuations(&owner), 1, "{owner}");
                let order = queued(&owner, &[CONTINUE, "user-101"]);
                assert_eq!(order, [Some(0), Some(1)], "{owner}");
                assert!(mark(SESSION), "the resume leaves its first submit to it");
            }
            assert!(!mark("another-session"));
        })
        .await;
    tc::cleanup_session_temp_files(&fx.name);
}

impl Fixture {
    async fn owner_or_none(&self) -> Option<Value> {
        let mailbox = self.shared.mailbox_peek(self.channel)?;
        let read = mailbox.v2_request(|slot| Ok(slot.as_ref().map(|owner| owner.json())));
        read.await.unwrap().map(Result::unwrap)
    }

    /// The owner's stored intent for a turn cut on `SESSION`, as the restart records it; its ticket.
    async fn record_intent(&self, revision: u64, ready: bool) -> Value {
        let request = serde_json::json!({"operation":"stopped-restart","native_session":SESSION,
            "interrupted_episode":"cut-before-the-stop","desired_revision":revision,"ready":ready});
        let request = serde_json::from_value(request).unwrap();
        let channel = self.channel.get();
        let generation = crate::services::discord::runtime_store::process_generation();
        let mailbox = self.shared.mailbox(self.channel);
        let record = mailbox.v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            owner.prepare_effort(Some(request), channel, generation)
        });
        serde_json::to_value(record.await.unwrap().continuation.unwrap().ticket).unwrap()
    }
}
