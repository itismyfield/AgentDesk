//! Background children launched in one turn and finished in a later one, driven
//! through the production observed-prompt relay against PostgreSQL.

use super::*;
use crate::db::session_observability::{BackgroundChildSpawn, insert_background_child_pg};
use crate::services::tmux_turn_liveness::idle_cleanup_session_is_unoccupied;
use crate::services::tui_prompt_dedupe::ObservedTuiPrompt;

const PARENT: &str = "claude/bgchild/host:AgentDesk-claude-bgchild-6691";
const TMUX: &str = "AgentDesk-claude-bgchild-6691";

fn task_notification(tool_use_id: Option<&str>, status: &str) -> ObservedTuiPrompt {
    let tool_use_tag = tool_use_id
        .map(|id| format!("<tool-use-id>{id}</tool-use-id>"))
        .unwrap_or_default();
    ObservedTuiPrompt {
        provider: ProviderKind::Claude.as_str().to_string(),
        tmux_session_name: TMUX.to_string(),
        prompt: format!(
            "<task-notification><task-id>bg-task</task-id>{tool_use_tag}\
             <status>{status}</status><summary>Background command \"sleep\" {status}</summary>\
             </task-notification>"
        ),
        source_event_id: None,
        observed_at: chrono::Utc::now(),
        external_input_lease_generation:
            crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
        ssh_direct_observation_generation:
            crate::services::tui_prompt_dedupe::SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED,
        hook_prompt_id: None,
        native_turn_id: None,
        steer_echo: false,
    }
}

async fn launch(pool: &sqlx::PgPool, tool_name: &str, input: &str, tool_use_id: &str) -> i64 {
    insert_background_child_pg(
        pool,
        &BackgroundChildSpawn {
            parent_session_key: PARENT.to_string(),
            provider: Some("claude".to_string()),
            tool_name: tool_name.to_string(),
            tool_input: input.to_string(),
            tool_use_id: Some(tool_use_id.to_string()),
        },
    )
    .await
    .unwrap()
    .expect("parent row exists")
}

async fn child_state(pool: &sqlx::PgPool, child: i64) -> (bool, String) {
    sqlx::query_as("SELECT closed_at IS NOT NULL, status FROM sessions WHERE id = $1")
        .bind(child)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn active_children(pool: &sqlx::PgPool) -> i32 {
    sqlx::query_scalar("SELECT active_children FROM sessions WHERE session_key = $1")
        .bind(PARENT)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A child whose completion arrives in a later turn closes by its launching
/// tool-use id, and only that child: a live sibling keeps the session occupied.
#[test]
fn later_turn_task_notification_closes_only_its_own_background_child_pg() {
    let temp = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(temp.path());
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let database = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
            let pool = database.connect_and_migrate().await;
            sqlx::query("INSERT INTO sessions (session_key, provider, status) VALUES ($1, 'claude', 'turn_active')")
                .bind(PARENT)
                .execute(&pool)
                .await
                .unwrap();

            // Turn N launches two background children, then ends with both running.
            let agent = launch(&pool, "Agent", r#"{"description":"review","run_in_background":true}"#, "toolu_bg_agent").await;
            let bash = launch(&pool, "Bash", r#"{"command":"sleep 600","run_in_background":true}"#, "toolu_bg_bash").await;
            sqlx::query("UPDATE sessions SET status = 'idle' WHERE session_key = $1")
                .bind(PARENT)
                .execute(&pool)
                .await
                .unwrap();
            assert_eq!(active_children(&pool).await, 2);
            assert!(!idle_cleanup_session_is_unoccupied(&pool, PARENT).await);

            // Turn N+1 arrives after a restart: nothing of turn N survives in memory.
            let mut shared = crate::services::discord::make_shared_data_for_tests();
            Arc::get_mut(&mut shared).unwrap().pg_pool = Some(pool.clone());

            // A notification without the launching id cannot name a child.
            relay_observed_prompt(&shared, task_notification(None, "completed")).await;
            assert_eq!(active_children(&pool).await, 2);

            relay_observed_prompt(&shared, task_notification(Some("toolu_bg_bash"), "completed")).await;
            assert_eq!(child_state(&pool, bash).await, (true, "idle".to_string()));
            assert_eq!(child_state(&pool, agent).await, (false, "turn_active".to_string()));
            assert_eq!(active_children(&pool).await, 1);
            assert!(
                !idle_cleanup_session_is_unoccupied(&pool, PARENT).await,
                "a still-running background child keeps the session occupied"
            );

            relay_observed_prompt(&shared, task_notification(Some("toolu_bg_agent"), "killed")).await;
            assert_eq!(child_state(&pool, agent).await, (true, "aborted".to_string()));
            assert_eq!(active_children(&pool).await, 0);
            assert!(idle_cleanup_session_is_unoccupied(&pool, PARENT).await);

            // A replayed notification and a replayed launch leave the count alone.
            relay_observed_prompt(&shared, task_notification(Some("toolu_bg_bash"), "completed")).await;
            let replay = insert_background_child_pg(
                &pool,
                &BackgroundChildSpawn {
                    parent_session_key: PARENT.to_string(),
                    provider: Some("claude".to_string()),
                    tool_name: "Bash".to_string(),
                    tool_input: r#"{"command":"sleep 600","run_in_background":true}"#.to_string(),
                    tool_use_id: Some("toolu_bg_bash".to_string()),
                },
            )
            .await
            .unwrap();
            assert_eq!(replay, None);
            assert_eq!(active_children(&pool).await, 0);
            assert!(idle_cleanup_session_is_unoccupied(&pool, PARENT).await);

            drop(shared);
            pool.close().await;
            database.drop().await;
        });
}
