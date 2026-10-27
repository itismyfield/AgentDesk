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

const INTAKE_TURN: &str = include_str!("router/message_handler/intake_turn.rs");
const HEADLESS_TURN: &str = include_str!("router/message_handler/headless_turn.rs");
const ANCESTORS: [(&str, &str, &str, &[&str]); 2] = [
    (
        "intake",
        INTAKE_TURN,
        "pub(super) async fn handle_text_message(",
        &[
            "post_adk_session_status_for_channel(",
            "hosted_tui_busy_preflight",
            "save_intake_inflight(codex_seed.as_ref(), &mut inflight_state)",
        ],
    ),
    (
        "headless",
        HEADLESS_TURN,
        "async fn start_reserved_headless_turn_admitted(",
        &[
            "post_adk_session_status_with_canonical_identity(",
            "save_inflight_state_create_new(&inflight_state)",
        ],
    ),
];
const JUDGE: &str = "turn_teardown_clearance::for_turn(";
const SPAWN: &str = "tokio::task::spawn_blocking(";
const VERDICT_ARGS: [&str; 2] = ["adk_session_key.as_deref()", "tmux_session_name.as_deref()"];
const EXECUTE: &str = "provider_dispatch::execute(";
const SPAWNED_ARGS: [&str; 2] = [
    "tmux_session_name: tmux_session_name.as_deref(),",
    "teardown: teardown_clearance.as_ref(),",
];

/// Classification 2: as-written, comment-free matching inside the ancestor's entry
/// function, so a comment between verdict and spawn is not code. A lexical tripwire.
fn last_step_gaps(source: &str, entry: &str, earlier: &[&str]) -> Vec<String> {
    use crate::services::discord::router::source_guard_support::Lexed;
    let Some(src) = Lexed::new(source).block(entry) else {
        return vec![format!("missing {entry}")];
    };
    let text = src.text();
    let Some((call, end, spawn)) = src.find(JUDGE).and_then(|call| {
        let end = call + text[call..].find(".await;")? + ".await;".len();
        Some((call, end, src.find_after(end, SPAWN)?))
    }) else {
        return vec!["verdict → spawn chain missing".to_string()];
    };
    let mut gaps = Vec::new();
    if src.count(JUDGE) != 1 || src.count(EXECUTE) != 1 {
        gaps.push("one verdict and one provider execute".to_string());
    }
    if !text[end..spawn].trim().is_empty() {
        gaps.push("nothing runs between the verdict and the spawn".to_string());
    }
    for step in earlier {
        if !src.find(step).is_some_and(|at| at < call) {
            gaps.push(format!("{step} must precede the verdict"));
        }
    }
    let (args, spawned) = (src.slice(call..end), src.slice(spawn..text.len()));
    gaps.extend(
        VERDICT_ARGS
            .iter()
            .filter(|arg| !args.contains(arg))
            .map(|arg| format!("verdict lost {arg}")),
    );
    for kept in [EXECUTE].iter().chain(&SPAWNED_ARGS) {
        if !spawned.contains(kept) {
            gaps.push(format!("spawned turn lost {kept}"));
        }
    }
    gaps
}

// Both ancestors judge after the writer post, inflight save and busy preflight, as the
// last step before spawn, on the key and tmux name the spawned turn carries.
#[test]
fn both_ancestors_judge_as_the_last_step_before_spawn() {
    for (file, source, entry, earlier) in ANCESTORS {
        assert_eq!(
            last_step_gaps(source, entry, earlier),
            Vec::<String>::new(),
            "{file}"
        );
    }
}

#[test]
fn last_step_guard_rejects_commented_steps_and_code_before_spawn() {
    use crate::services::discord::router::source_guard_support::{Lexed, forge};
    use crate::test_support::CommentForgery;
    for (file, source, entry, earlier) in ANCESTORS {
        let mut needles = vec![entry, JUDGE, SPAWN, EXECUTE];
        needles.extend(
            earlier
                .iter()
                .chain(&VERDICT_ARGS)
                .chain(&SPAWNED_ARGS)
                .copied(),
        );
        for needle in needles {
            for forgery in CommentForgery::ALL {
                let forged = forge(source, needle, forgery);
                assert!(forged.contains(needle));
                let gaps = last_step_gaps(&forged, entry, earlier);
                assert!(!gaps.is_empty(), "{file}: {forgery:?} `{needle}` passes");
            }
        }
        let call = Lexed::new(source).find(JUDGE).expect("verdict call");
        let end = call + source[call..].find(".await;").expect("awaited verdict") + 7;
        let between = |insert: &str| format!("{}{insert}{}", &source[..end], &source[end..]);
        let prose = between("\n        // run nothing here /* outer /* x */ */");
        assert_eq!(
            last_step_gaps(&prose, entry, earlier),
            Vec::<String>::new(),
            "{file}"
        );
        let coded = between("\n        let _between = ();");
        assert_eq!(last_step_gaps(&coded, entry, earlier).len(), 1, "{file}");
    }
}
