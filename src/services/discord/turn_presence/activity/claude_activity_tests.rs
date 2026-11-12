use super::*;
use crate::services::claude_tui::{hook_server, input::submission::tests::ScriptedTmux};
use crate::services::tui_prompt_dedupe::{
    self as dedupe, binding_context as context, binding_events as events,
};
use axum::{body::Body, http::Request};
use chrono::Utc;
use serde_json::{Value, json};
use std::{fs, io::Write, path::PathBuf};
use tower::ServiceExt;

struct Rig {
    _dir: tempfile::TempDir,
    previous_root: Option<PathBuf>,
    session: String,
    path: PathBuf,
    ctx: context::BindingContext,
}
impl Rig {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let previous_root = events::test_root();
        events::set_test_root(Some(dir.path()));
        let root = dir.path().join("projects");
        let project = root.join("project");
        fs::create_dir_all(&project).unwrap();
        let native = uuid::Uuid::new_v4().to_string();
        let session = format!("activity-{}", uuid::Uuid::new_v4().simple());
        let path = project.join(format!("{native}.jsonl"));
        let ctx: context::BindingContext = serde_json::from_value(json!({
            "schema":1,"provider":"claude","created_at":Utc::now(),
            "execution_nonce":uuid::Uuid::new_v4().simple().to_string(),
            "tmux_session":session,"channel_id":8918081,"owner_runtime_root":"fixture",
            "host":"activity-test","expected_native_session_id":native,
            "launch_mode":"fresh","provider_root":root
        }))
        .unwrap();
        context::PreparedIncarnation::create(ctx.clone()).unwrap();
        let marker = crate::services::tmux_common::session_temp_path(&session, "spawn_nonce");
        fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
        fs::write(marker, &ctx.execution_nonce).unwrap();
        let r = Self {
            _dir: dir,
            previous_root,
            session,
            path,
            ctx,
        };
        r.append(json!({"type":"user","uuid":"turn-a","message":{"content":"work"}}));
        r.append(json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"tool-a","name":"Bash","input":{}}]}}));
        let source = crate::services::tui_o::shadow::binding_reader::source_id_for(
            r.ctx.expected_native_session_id.as_deref().unwrap(),
            &r.path,
        )
        .unwrap();
        events::record_verified(
            &events::Proposal {
                channel_id: r.ctx.channel_id.unwrap(),
                provider: "claude",
                tmux_session: &r.session,
                session_id: r.ctx.expected_native_session_id.as_deref(),
                path: r.path.to_str().unwrap(),
                replaced: None,
                cause: events::CauseSource::Hook(events::BindingCause::Startup),
                hook: None,
            },
            &events::SourceId {
                session_id: source.session_id,
                path: source.path,
                dev: source.dev,
                ino: source.ino,
            },
        )
        .unwrap();
        dedupe::register_tmux_runtime_binding(
            &r.session,
            dedupe::TuiRuntimeBinding {
                runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
                output_path: r.path.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: r.ctx.expected_native_session_id.clone(),
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        r
    }
    fn append(&self, mut row: Value) {
        row["sessionId"] = json!(self.ctx.expected_native_session_id);
        row["timestamp"] = json!(Utc::now());
        writeln!(
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .unwrap(),
            "{row}"
        )
        .unwrap();
    }
    async fn hook(&self, kind: &str) {
        let envelope = context::HookBindingEnvelope {
            context: context::CapturedContext::Captured(self.ctx.clone()),
            observed: context::ObservedHookProcess::default(),
        };
        let payload = json!({"session_id":self.ctx.expected_native_session_id,
            "transcript_path":self.path,"hook_event_name":kind,"tool_use_id":"tool-a","tool_name":"Bash"});
        let response =
            hook_server::hook_receiver_router_with_state(hook_server::HookServerState::new())
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(format!(
                            "/hooks/claude/{kind}?session_id={}",
                            self.ctx.expected_native_session_id.as_deref().unwrap()
                        ))
                        .header("content-type", "application/json")
                        .header(context::BINDING_HEADER, envelope.encode().unwrap())
                        .header(
                            "x-agentdesk-relay-request-id",
                            uuid::Uuid::new_v4().to_string(),
                        )
                        .header("x-agentdesk-relay-published-at", Utc::now().to_rfc3339())
                        .header(
                            "x-agentdesk-relay-deadline",
                            (Utc::now() + chrono::Duration::minutes(1)).to_rfc3339(),
                        )
                        .body(Body::from(payload.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        dedupe::clear_tmux_runtime_binding(&self.session);
        events::set_test_root(self.previous_root.as_deref());
    }
}

#[tokio::test]
async fn same_tool_hook_is_busy_across_narrow_resize_footer_and_capture_failure() {
    let _root = crate::config::TestRuntimeRootGuard::with_isolated_config();
    let r = Rig::new();
    r.hook("PreToolUse").await;
    let wide = format!(
        "{}\n❯\n{}\n  ⏵⏵ bypass permissions on · esc to interrupt\n",
        "─".repeat(80),
        "─".repeat(80)
    );
    assert!(crate::services::tmux_common::tmux_capture_indicates_claude_tui_busy(&wide));
    let tmux = ScriptedTmux::new(&wide);
    let ports = LivePorts {
        root: events::test_root(),
    };
    assert!(ports.claude_busy(&r.session).is_busy());
    for width in [46, 22, 80, 46] {
        tmux.show(&format!(
            "{}\n❯\n{}\n  ⏵⏵ bypass permissions on\n  (shift+tab to cycle)\n  context 1%\n",
            "─".repeat(width),
            "─".repeat(width)
        ));
        assert!(
            ports.claude_busy(&r.session).is_busy(),
            "same accepted hook/transcript, width={width}"
        );
    }
    fs::write(tmux.directory().join("capture_fail"), "1").unwrap();
    assert!(ports.claude_busy(&r.session).is_busy());
    assert!(tmux.sends().is_empty());
}

#[path = "claude_activity_leaf_tests.rs"]
mod leaf;
