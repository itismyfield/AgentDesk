//! A Decision original on a queue no input owner drives is admitted only while its event is open
//! and as PG approved it; a closed or unapproved one leaves the queue unrun.
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::decisions::EventKind;
use crate::db::decisions::test_support::{insert_event, row};
use crate::services::turn_orchestrator::{
    InterventionMode, QueuePersistenceContext, SourceMessageQueuedGeneration,
};

const TEXT: &str = "[사용자 결정 ref=r1] 질문 → 선택: A";
const ACTOR: u64 = 7;

/// The queue entry the Decision delivery makes of an approved event: its source names the origin.
fn entry(message: u64, origin: Option<String>, (actor, text): (u64, &str)) -> Intervention {
    let message = serenity::MessageId::new(message);
    let mut source = SourceMessageQueuedGeneration::user_instruction(message, 1);
    source.origin_id = origin;
    Intervention {
        author_id: serenity::UserId::new(actor),
        author_is_bot: false,
        message_id: message,
        queued_generation: 1,
        source_message_ids: vec![message],
        source_message_queued_generations: vec![source],
        source_text_segments: Vec::new(),
        text: text.to_string(),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
    }
}

#[tokio::test]
async fn decision_original_leaves_the_queue_unrun_unless_its_event_is_open_pg() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _mode = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "ADK_INTAKE_ROUTING_MODE",
        std::ffi::OsStr::new("disabled"),
    );
    let root = tempfile::tempdir().expect("runtime root");
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let _config = local_admission_config_after_shared_test_env_lock(root.path());
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let http = std::sync::Arc::new(serenity::Http::new("Bot queued-decision-test"));
    let deps = super::super::super::message_handler::IntakeDeps {
        http: &http,
        cache: None,
        ctx_for_chained_dispatch: None,
        shared: &shared,
        token: "queued-decision-test",
    };
    // (case, the event's state, what the queue entry claims, admitted, the state it ends in)
    type Claim = fn(&str) -> Option<(String, u64, &'static str)>;
    fn own(_: &str) -> Option<(String, u64, &'static str)> {
        None
    }
    let cases: [(&str, &str, Claim, bool, &str); 10] = [
        ("open", "normal_wait", own, true, "may_have_sent"),
        ("failed", "failed", own, false, "failed"),
        ("unknown", "unknown_closed", own, false, "unknown_closed"),
        ("delivered", "delivered", own, false, "delivered"),
        ("closing", "closing", own, false, "closing"),
        ("journaled", "may_have_sent", own, false, "unknown_closed"),
        (
            "another body",
            "normal_wait",
            |origin| Some((origin.to_string(), ACTOR, "another body")),
            false,
            "normal_wait",
        ),
        (
            "another actor",
            "normal_wait",
            |origin| Some((origin.to_string(), ACTOR + 1, TEXT)),
            false,
            "normal_wait",
        ),
        (
            "another opinion",
            "normal_wait",
            |origin| Some((format!("{origin}:opinion:5"), ACTOR, TEXT)),
            false,
            "normal_wait",
        ),
        (
            "no event",
            "normal_wait",
            |_| Some(("decision:not-an-event".to_string(), ACTOR, TEXT)),
            false,
            "normal_wait",
        ),
    ];
    for (index, (case, state, claim, admitted, ends)) in cases.into_iter().enumerate() {
        let channel = serenity::ChannelId::new(8_211_300 + index as u64);
        let approved = (EventKind::Selection, channel.get(), ACTOR);
        let event = insert_event(&pool, approved, TEXT, state).await;
        let origin = format!("decision:{event}");
        let (origin, actor, text) = claim(&origin).unwrap_or((origin, ACTOR, TEXT));
        let persistence =
            QueuePersistenceContext::new(&ProviderKind::Claude, &shared.token_hash, None);
        let queued = entry(8_211_400 + index as u64, Some(origin), (actor, text));
        shared
            .mailbox(channel)
            .enqueue(queued, persistence.clone())
            .await;
        let taken = shared
            .mailbox(channel)
            .take_next_soft(persistence.clone())
            .await;
        let head = taken.intervention.unwrap();
        let disposition = admit_queued_intake(
            &deps,
            ProviderKind::Claude,
            channel,
            &head,
            head.author_id,
            "decision-owner".to_string(),
            false,
            false,
            "queued_decision_test",
            taken.dispatch_lease,
        )
        .await;
        let ran = matches!(disposition, QueuedAdmissionDisposition::Admitted(_));
        assert_eq!(ran, admitted, "{case}: admitted");
        assert_eq!(row(&pool, event).await.0, ends, "{case}: event state");
        if !admitted {
            let again = shared.mailbox(channel).take_next_soft(persistence).await;
            assert!(again.intervention.is_none(), "{case}: put back");
        }
    }
    pool.close().await;
    db.drop().await;
}

/// Inside a Decision start, an entry rebuilt from the request text still names the event and
/// the start reads as a wait; outside one nothing is stamped.
#[tokio::test]
async fn decision_start_that_queues_its_input_again_keeps_the_event_origin() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let origin = format!("decision:{}", uuid::Uuid::new_v4());
    let rebuilt = || {
        super::super::super::response_format::build_race_requeued_intervention(
            serenity::UserId::new(ACTOR),
            serenity::MessageId::new(9),
            TEXT,
            true,
            None,
            false,
            true,
            Vec::new(),
        )
    };
    let start = async { (rebuilt(), decision_queue::starting_origin()) };
    let ((entry, named), back) = decision_queue::started_under(Some(origin.clone()), start).await;
    assert_eq!(decision_queue::origin(&entry), Some(origin.as_str()));
    assert_eq!(
        (named, back, entry.merge_consecutive),
        (Some(origin), true, false)
    );
    assert_eq!(decision_queue::origin(&rebuilt()), None);
    let (_, back) = decision_queue::started_under(Some("decision:x".into()), async {}).await;
    assert!(!back);
}
