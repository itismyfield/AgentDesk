//! The canary without a core session keeps its queued input while its workspace is unusable.

use super::*;
use crate::services::codex_tui::canary::CANARY_TMUX;
use crate::services::discord::commands::control::codex_clear_relaunch_tests::CanaryFixture;
use crate::services::turn_orchestrator::{Intervention, InterventionMode, QueueIdentity};

const ORIGINAL_MESSAGE: u64 = 1_561_159_600_206_516_285;

fn original(message: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: MessageId::new(message),
        queued_generation: 322,
        source_message_ids: vec![MessageId::new(message)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: "original input".into(),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

fn break_workspace(fixture: &CanaryFixture) {
    let config = crate::runtime_layout::config_file_path(fixture.root.path());
    let yaml = std::fs::read_to_string(&config).unwrap();
    let workspace = serde_json::to_string(fixture.root.path()).unwrap();
    let missing = serde_json::to_string(&fixture.root.path().join("missing-workspace")).unwrap();
    assert!(yaml.contains(&workspace));
    std::fs::write(&config, yaml.replace(&workspace, &missing)).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn canary_without_core_session_and_unresolved_workspace_parks_original_pg() {
    let name = concat!(
        module_path!(),
        "::canary_without_core_session_and_unresolved_workspace_parks_original_pg"
    );
    if !crate::services::tui_o::cutover::test_override::in_empty_list_process(name)
        || !crate::services::discord::admin_host_guard::tests::api_child(
            name.split_once("::").unwrap().1,
        )
    {
        return;
    }
    let mut fixture = CanaryFixture::new().await;
    let api = crate::services::discord::admin_host_guard::tests::Recorder::start_with(Arc::new(
        |method, path| {
            (method == axum::http::Method::GET
                && path.ends_with(&format!("/channels/{CANARY_CHANNEL}")))
            .then(|| {
                serde_json::json!({
                    "id": CANARY_CHANNEL.to_string(), "type": 0,
                    "name": "adk-codex-tui-e2e", "guild_id": "42000",
                    "position": 0, "permission_overwrites": [], "nsfw": false,
                    "parent_id": null
                })
            })
        },
    ))
    .await;
    crate::services::discord::internal_api::init(api.port, None);
    Arc::get_mut(&mut fixture.shared).unwrap().api_port = api.port;
    let channel = ChannelId::new(CANARY_CHANNEL);
    fixture.shared.core.lock().await.sessions.remove(&channel);
    // An ended canary with no verified history: no pin holds the input, only the workspace check.
    fixture.fake.mark_missing();
    crate::services::session_backend::remove_process_session(CANARY_TMUX);
    crate::services::tmux_common::cleanup_session_temp_files(CANARY_TMUX);
    std::fs::write(fixture.history(), b"").unwrap();
    assert!(!crate::services::tui_prompt_dedupe::codex_verified_requires_proof(CANARY_TMUX));
    let persistence =
        super::super::queue_persistence_context(&fixture.shared, &ProviderKind::Codex, channel);
    let queued = fixture
        .shared
        .mailbox(channel)
        .enqueue(original(ORIGINAL_MESSAGE), persistence)
        .await;
    assert!(queued.enqueued);
    let identity = QueueIdentity::of(&original(ORIGINAL_MESSAGE));
    // The workspace directory disappears from config; intake could only answer with an error.
    break_workspace(&fixture);
    api.take();
    let deps = crate::services::discord::router::IntakeDeps {
        http: &api.http,
        cache: None,
        ctx_for_chained_dispatch: None,
        shared: &fixture.shared,
        token: "e51-isolated-test-bot",
    };
    assert_eq!(
        super::super::kickoff::kickoff_idle_queues_with_deps(&deps, &ProviderKind::Codex).await,
        0
    );
    let retained = fixture.shared.mailbox(channel).snapshot().await;
    assert_eq!(retained.intervention_queue.len(), 1);
    assert_eq!(QueueIdentity::of(&retained.intervention_queue[0]), identity);
    let sent = api.take();
    assert!(
        !sent.iter().any(|call| call.contains("No active session")),
        "{sent:?}"
    );
    assert!(
        !fixture
            .fake
            .take_calls()
            .iter()
            .any(|call| call.starts_with("new-session "))
    );
}

/// Only the canary is held here: another Codex channel in the same state, a Claude canary, or a
/// canary with a core session or a usable workspace pass on to the existing intake path.
#[tokio::test(flavor = "current_thread")]
async fn workspace_hold_stays_on_the_sessionless_codex_canary_pg() {
    let fixture = CanaryFixture::new().await;
    let canary = ChannelId::new(CANARY_CHANNEL);
    let other = ChannelId::new(CANARY_CHANNEL + 7);
    let codex = ProviderKind::Codex;
    assert!(
        !parked(&fixture.shared, &codex, canary).await,
        "core session"
    );
    fixture.shared.core.lock().await.sessions.remove(&canary);
    assert!(
        !parked(&fixture.shared, &codex, canary).await,
        "usable workspace"
    );
    break_workspace(&fixture);
    assert!(
        !parked(&fixture.shared, &codex, other).await,
        "another channel"
    );
    let claude = ProviderKind::Claude;
    assert!(
        !parked(&fixture.shared, &claude, canary).await,
        "another provider"
    );
    assert!(parked(&fixture.shared, &codex, canary).await);
}
