use super::*;
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind, upsert_hook_session_with_identity_pg,
};
use crate::db::dispatched_sessions::HookSessionUpsert;
use crate::db::dispatched_sessions::hosted_execution::tests::TOKEN;
use crate::services::discord::inflight::seed_session_row_keyed;

const CHANNEL: u64 = 1_479_671_301_387_059_700;

fn key(name: &str) -> String {
    format!("claude/{TOKEN}/mac-mini:{name}")
}

async fn verdict(pool: Option<&PgPool>, key: Option<&str>, tmux: Option<&str>) -> String {
    match for_turn(pool, &ProviderKind::Claude, CHANNEL, key, tmux).await {
        None => "no tmux".to_string(),
        Some(TeardownClearance::Cleared(session)) => format!("cleared {}", session.name()),
        Some(TeardownClearance::Refused(_)) => "refused".to_string(),
        Some(TeardownClearance::Unkeyed) => "unkeyed".to_string(),
    }
}

// A channel's first turn has no sessions row until its writer posts one, so the verdict
// must follow that post: before it the turn is refused, after it the row admits it.
#[tokio::test]
async fn a_turn_is_cleared_only_by_the_row_its_writer_posted_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (name, first) = ("w2a-first-turn", key("w2a-first-turn"));
    let judged = |key| verdict(Some(&pool), key, Some(name));
    assert_eq!(
        judged(Some(&first)).await,
        "refused",
        "Missing is no legacy answer"
    );
    seed_session_row_keyed(&pool, &first, CHANNEL, None).await;
    assert_eq!(judged(Some(&first)).await, format!("cleared {name}"));
    assert_eq!(
        judged(None).await,
        "unkeyed",
        "only a turn with no key stays name-only"
    );
    let no_tmux = verdict(Some(&pool), Some(&first), None).await;
    assert_eq!(no_tmux, "no tmux");
    let no_pool = verdict(None, Some(&first), Some(name)).await;
    assert_eq!(
        no_pool, "refused",
        "a keyed turn without a pool is not unkeyed"
    );
    pool.close().await;
    db.drop().await;
}

// A scheduled-snapshot turn owns a separate row on the channel; its own exact key admits
// it, where also reading the channel's canonical row would make two rows disagree.
#[tokio::test]
async fn a_snapshot_turn_is_judged_by_its_own_row_only_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (channel_name, snapshot_name) = ("w2a-channel", "w2a-snapshot");
    seed_session_row_keyed(&pool, &key(channel_name), CHANNEL, None).await;
    let (snapshot, channel) = (key(snapshot_name), CHANNEL.to_string());
    let params = HookSessionUpsert {
        session_key: &snapshot,
        instance_id: Some("test-node"),
        agent_id: None,
        provider: "claude",
        status: "idle",
        session_info: None,
        model: None,
        tokens: None,
        cwd: None,
        active_dispatch_id: None,
        thread_channel_id: None,
        channel_id: Some(&channel),
        claude_session_id: None,
        raw_provider_session_id: None,
        turn_start_nonce: None,
        dispatched_origin: false,
    };
    let identity = CanonicalSessionIdentity {
        kind: SessionIdentityKind::ScheduledSnapshot,
        discord_token_hash: TOKEN,
        channel_id: &channel,
    };
    let upsert = upsert_hook_session_with_identity_pg(&pool, params, Some(identity));
    upsert.await.unwrap();
    for name in [snapshot_name, channel_name] {
        let judged = verdict(Some(&pool), Some(&key(name)), Some(name)).await;
        assert_eq!(judged, format!("cleared {name}"));
    }
    pool.close().await;
    db.drop().await;
}

// Both ancestors judge after the writer post, inflight save and busy preflight, as the
// last step before spawn, on the key and tmux name the spawned turn carries.
#[test]
fn both_ancestors_judge_as_the_last_step_before_spawn() {
    let intake = include_str!("router/message_handler/intake_turn.rs");
    let headless = include_str!("router/message_handler/headless_turn.rs");
    for (file, source, earlier) in [
        (
            "intake",
            intake,
            &[
                "post_adk_session_status_for_channel(",
                "hosted_tui_busy_preflight",
            ][..],
        ),
        (
            "headless",
            headless,
            &["post_adk_session_status_with_canonical_identity("][..],
        ),
    ] {
        let judge = "turn_teardown_clearance::for_turn(";
        assert_eq!(source.matches(judge).count(), 1, "{file}");
        let call = source.find(judge).unwrap();
        let end = call + source[call..].find(".await;").unwrap() + ".await;".len();
        let spawn = end + source[end..].find("tokio::task::spawn_blocking(").unwrap();
        let before_spawn = source[end..spawn].trim();
        let capture = "let provider_permit = crate::services::discord::input_runtime::fence::effect::current();";
        assert_eq!(
            before_spawn,
            if file == "headless" {
                format!("{capture}\n    let _provider_task =")
            } else {
                capture.to_owned()
            },
            "{file}: only capability capture precedes spawn after the verdict"
        );
        for step in earlier
            .iter()
            .chain(&["save_inflight_state_create_new(&inflight_state)"])
        {
            assert!(
                source.find(step).is_some_and(|at| at < call),
                "{file}: {step}"
            );
        }
        let args = &source[call..end];
        assert!(args.contains("adk_session_key.as_deref()"), "{file}");
        assert!(args.contains("tmux_session_name.as_deref()"), "{file}");
        let spawned = &source[spawn..];
        assert_eq!(
            source.matches("provider_dispatch::execute(").count(),
            1,
            "{file}"
        );
        assert!(spawned.contains("provider_dispatch::execute("), "{file}");
        assert!(spawned.contains("tmux_session_name: tmux_session_name.as_deref(),"));
        assert!(
            spawned.contains("teardown: teardown_clearance.as_ref(),"),
            "{file}"
        );
    }
}
