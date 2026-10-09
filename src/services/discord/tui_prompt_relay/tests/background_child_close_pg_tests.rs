//! Background children launched in turn N and finished later, read from raw
//! Claude transcript records by the production idle tick against PostgreSQL.

use super::*;
use crate::db::session_observability::{
    BackgroundChildSpawn, close_background_child_for_tool_use_pg, insert_background_child_pg,
};
use crate::services::tmux_turn_liveness::idle_cleanup_session_is_unoccupied;

const TMUX_P: &str = "AgentDesk-claude-bgchild-p-6691";
const TMUX_Q: &str = "AgentDesk-claude-bgchild-q-6691";
const CHANNEL_P: u64 = 6_691_000_101;

fn completion(tool_use_id: &str, status: &str) -> String {
    format!(
        "<task-notification>\n<task-id>bg-{tool_use_id}</task-id>\n\
         <tool-use-id>{tool_use_id}</tool-use-id>\n<status>{status}</status>\n\
         <summary>Background command \"job\" {status}</summary>\n</task-notification>"
    )
}

/// How Claude records a notification drained into a running turn.
fn queued_attachment(text: &str) -> serde_json::Value {
    serde_json::json!({"type": "attachment", "attachment": {"type": "queued_command",
        "prompt": text, "commandMode": "task-notification",
        "origin": {"kind": "task-notification"}}})
}

/// How Claude records a notification that opens its own turn.
fn notification_user(text: &str, is_meta: bool) -> serde_json::Value {
    serde_json::json!({"type": "user", "isMeta": is_meta,
        "origin": {"kind": "task-notification"}, "message": {"role": "user", "content": text}})
}

fn append(path: &Path, records: &[serde_json::Value]) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    for record in records {
        writeln!(file, "{record}").unwrap();
    }
}

/// Binds `tmux` to a transcript whose history the prompt relay already consumed.
fn bind_transcript(root: &Path, tmux: &str) -> PathBuf {
    let path = root.join(format!("{tmux}.jsonl"));
    std::fs::write(
        &path,
        "{\"type\":\"user\",\"message\":{\"content\":\"run the jobs\"}}\n\
         {\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"started\"}]}}\n",
    )
    .unwrap();
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        tmux,
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: path.to_str().unwrap().to_owned(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: std::fs::metadata(&path).unwrap().len(),
            relay_last_offset: None,
        },
    );
    path
}

async fn launch(pool: &sqlx::PgPool, parent: &str, tool_use_id: &str) -> i64 {
    insert_background_child_pg(
        pool,
        &BackgroundChildSpawn {
            parent_session_key: parent.to_string(),
            provider: Some("claude".to_string()),
            tool_name: "Bash".to_string(),
            tool_input: r#"{"command":"sleep 600","run_in_background":true}"#.to_string(),
            tool_use_id: Some(tool_use_id.to_string()),
        },
    )
    .await
    .unwrap()
    .expect("parent row exists")
}

async fn closed(pool: &sqlx::PgPool, child: i64) -> Option<String> {
    sqlx::query_scalar("SELECT status FROM sessions WHERE id = $1 AND closed_at IS NOT NULL")
        .bind(child)
        .fetch_optional(pool)
        .await
        .unwrap()
}

async fn parent_state(pool: &sqlx::PgPool, parent: &str) -> (i32, String, bool) {
    let (children, status): (i32, String) =
        sqlx::query_as("SELECT active_children, status FROM sessions WHERE session_key = $1")
            .bind(parent)
            .fetch_one(pool)
            .await
            .unwrap();
    (
        children,
        status,
        idle_cleanup_session_is_unoccupied(pool, parent).await,
    )
}

/// A terminal notification closes only its own session's child, mid-turn, idle,
/// as `isMeta` or across a restart; live or non-terminal siblings stay occupied.
#[test]
fn transcript_completions_close_their_own_background_children_pg() {
    let temp = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(temp.path());
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let database = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
            let pool = database.connect_and_migrate().await;
            let mut shared = crate::services::discord::make_shared_data_for_tests();
            Arc::get_mut(&mut shared).unwrap().pg_pool = Some(pool.clone());
            let key = |tmux| {
                crate::services::discord::adk_session::build_namespaced_session_key(
                    &shared.token_hash,
                    &ProviderKind::Claude,
                    tmux,
                )
            };
            let (parent_p, parent_q) = (key(TMUX_P), key(TMUX_Q));

            // Turn N: each parent launches background jobs, then ends waiting on them.
            for parent in [&parent_p, &parent_q] {
                sqlx::query("INSERT INTO sessions (session_key, provider, status) VALUES ($1, 'claude', 'turn_active')")
                    .bind(parent).execute(&pool).await.unwrap();
            }
            let p_dup = launch(&pool, &parent_p, "toolu_dup").await;
            let q_dup = launch(&pool, &parent_q, "toolu_dup").await;
            let bash = launch(&pool, &parent_p, "toolu_bash").await;
            let agent = launch(&pool, &parent_p, "toolu_agent").await;
            let meta = launch(&pool, &parent_p, "toolu_meta").await;
            sqlx::query("UPDATE sessions SET status = 'awaiting_bg' WHERE parent_session_id IS NULL")
                .execute(&pool).await.unwrap();
            let transcript_p = bind_transcript(temp.path(), TMUX_P);
            let transcript_q = bind_transcript(temp.path(), TMUX_Q);
            super::super::background_child_completion::forget_completion_cursors_for_tests();
            super::super::claude_idle_runtime::relay_idle_claude_bindings(&shared).await;

            // A completion without its session's identity names no child.
            assert_eq!(close_background_child_for_tool_use_pg(&pool, "", "toolu_dup", "completed").await.unwrap(), None);
            assert_eq!((closed(&pool, p_dup).await, closed(&pool, q_dup).await), (None, None));

            // Turn N+1 is running on P's channel while P's bash and Q's dup finish.
            let channel_p = ChannelId::new(CHANNEL_P);
            shared.tmux_watchers.insert(channel_p, test_watcher_handle(TMUX_P, &transcript_p));
            let running = crate::services::discord::inflight::InflightTurnState::new(
                ProviderKind::Claude, CHANNEL_P, None, 1, 2, 0, "turn N+1".to_string(),
                None, Some(TMUX_P.to_string()), Some(transcript_p.display().to_string()), None, 0,
            );
            crate::services::discord::inflight::save_inflight_state(&running).unwrap();
            append(&transcript_p, &[queued_attachment(&completion("toolu_bash", "completed"))]);
            append(&transcript_q, &[notification_user(&completion("toolu_dup", "completed"), false)]);
            super::super::claude_idle_runtime::relay_idle_claude_bindings(&shared).await;
            assert_eq!(closed(&pool, bash).await.as_deref(), Some("idle"));
            assert_eq!(closed(&pool, q_dup).await.as_deref(), Some("idle"));
            assert_eq!(closed(&pool, p_dup).await, None, "Q's completion must not close P's child");
            assert_eq!(parent_state(&pool, &parent_p).await, (3, "awaiting_bg".to_string(), false));
            assert_eq!(parent_state(&pool, &parent_q).await, (0, "idle".to_string(), true));

            // P goes idle; one job is killed while another only reports progress.
            crate::services::discord::inflight::clear_inflight_state(&ProviderKind::Claude, CHANNEL_P);
            shared.tmux_watchers.remove(&channel_p);
            append(&transcript_p, &[
                notification_user(&completion("toolu_agent", "killed"), false),
                notification_user(&completion("toolu_meta", "running"), false),
            ]);
            super::super::claude_idle_runtime::relay_idle_claude_bindings(&shared).await;
            assert_eq!(closed(&pool, agent).await.as_deref(), Some("aborted"));
            assert_eq!(closed(&pool, meta).await, None, "a non-terminal notification keeps the child");
            assert_eq!(parent_state(&pool, &parent_p).await, (2, "awaiting_bg".to_string(), false));

            // The last completions land while dcserver is down; the restarted
            // process finds them with no read position, one recorded as meta.
            append(&transcript_p, &[
                notification_user(&completion("toolu_dup", "completed"), true),
                notification_user(&completion("toolu_meta", "completed"), true),
            ]);
            super::super::background_child_completion::forget_completion_cursors_for_tests();
            super::super::claude_idle_runtime::relay_idle_claude_bindings(&shared).await;
            assert_eq!(closed(&pool, p_dup).await.as_deref(), Some("idle"));
            assert_eq!(closed(&pool, meta).await.as_deref(), Some("idle"));
            assert_eq!(parent_state(&pool, &parent_p).await, (0, "idle".to_string(), true));

            for tmux in [TMUX_P, TMUX_Q] {
                crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(tmux);
            }
            super::super::background_child_completion::forget_completion_cursors_for_tests();
            drop(shared);
            pool.close().await;
            database.drop().await;
        });
}
