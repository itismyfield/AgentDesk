use super::*;
use crate::services::memory::{SessionAnchorRequest, load_session_anchor_prompt};

impl BuiltSystemPrompt {
    pub(in crate::services::discord) fn arm_audit(
        &self,
        token: &crate::services::provider::CancelToken,
        shared: &SharedData,
        provider: &ProviderKind,
        channel: ChannelId,
        turn: &str,
        sak: Option<&str>,
    ) {
        crate::services::provider::session_prompt::arm(
            token,
            shared.pg_pool.clone(),
            provider,
            channel.get(),
            turn,
            self.anchor_count,
        );
        if let Some(sak) = sak {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 📦 SAK in system prompt ({} chars) for channel {}",
                sak.len(),
                channel.get()
            );
        }
    }

    pub(in crate::services::discord) async fn with_session_anchors(
        mut self,
        request: SessionAnchorRequest<'_>,
    ) -> Self {
        let agy_launch_channel = (!self.inspection
            && matches!(request.provider, ProviderKind::Antigravity)
            && (request.fresh || request.session_id.is_none()))
        .then_some(request.channel_id);
        let anchors = if self.inspection {
            request.peek_prompt()
        } else {
            load_session_anchor_prompt(request).await
        };
        if let Some((anchors, count)) = anchors {
            self.anchor_count = count;
            self.append_anchor_layer(&anchors);
            if let Some(channel_id) = agy_launch_channel {
                tracing::info!(
                    channel_id,
                    turn_id = self
                        .manifest
                        .as_ref()
                        .map(|manifest| manifest.turn_id.as_str()),
                    anchor_count = count,
                    "attached Memento anchors to Antigravity launch system prompt"
                );
            }
        }
        self
    }

    fn append_anchor_layer(&mut self, anchors: &str) {
        self.system_prompt.push_str("\n\n");
        self.system_prompt.push_str(anchors);
        if let Some(manifest) = self.manifest.as_mut() {
            let layer = prompt_manifest_layer(
                "memento_anchors",
                "memento.context.anchor_memory",
                None,
                PromptContentVisibility::UserDerived,
                anchors,
            );
            manifest.total_input_bytes = manifest.total_input_bytes.saturating_add(
                i64::try_from(anchors.len())
                    .unwrap_or(i64::MAX)
                    .saturating_add(2),
            );
            manifest.total_input_tokens_est = manifest
                .total_input_tokens_est
                .saturating_add(layer.tokens_est);
            manifest.layer_count += 1;
            manifest.layers.push(layer);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::settings::MemoryBackendKind;
    use crate::services::memory::with_memento_test_endpoint;
    use crate::services::stream_json_cli::{
        dialects::agy, policy::ConfiguredToolPolicy, request::ProviderTurnRequest,
        session::ProviderSessionToken,
    };
    use axum::{Json, Router, routing::post};
    use serde_json::{Value, json};
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const AGY_SESSION: &str = "96bb7a8b-4c99-49bb-8917-4979bc776ee4";

    async fn with_memento_fixture(run: impl AsyncFnOnce(Arc<Mutex<Vec<Value>>>, &Path)) {
        let workspace = tempfile::tempdir().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let app = Router::new().route(
            "/mcp",
            post(move |Json(request): Json<Value>| {
                let recorded = recorded.clone();
                async move {
                    let result = if request["method"] == "initialize" {
                        json!({})
                    } else {
                        assert_eq!(request["method"], "tools/call");
                        assert_eq!(request["params"]["name"], "context");
                        let call = {
                            let mut calls = recorded.lock().unwrap();
                            calls.push(request["params"]["arguments"].clone());
                            calls.len()
                        };
                        let content = format!("합성 anchor {call}");
                        let payload = json!({
                            "anchorCount": 1,
                            "anchors": {"permanent": [{"content": content}]},
                            "injectionText": format!("[ANCHOR MEMORY]\n- {content}")
                        });
                        json!({"content": [{"type": "text", "text": payload.to_string()}]})
                    };
                    (
                        [("mcp-session-id", "fixture-session")],
                        Json(json!({"jsonrpc": "2.0", "id": request["id"], "result": result})),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        with_memento_test_endpoint(endpoint, run(calls, workspace.path())).await;
        server.abort();
    }

    async fn agy_turn_argv(
        workspace: &Path,
        channel: u64,
        session_id: Option<&str>,
        fresh: bool,
    ) -> Vec<String> {
        let channel = ChannelId::new(channel);
        let provider = ProviderKind::Antigravity;
        let settings = ResolvedMemorySettings {
            backend: MemoryBackendKind::Memento,
            memory_group: Some("anchor-fixture".into()),
            ..Default::default()
        };
        let current_path = workspace.to_str().unwrap();
        let built = {
            let _root = crate::config::set_agentdesk_root_for_test(workspace);
            build_system_prompt_with_manifest(
                "fixture context",
                &[],
                current_path,
                channel,
                channel,
                "fixture-token",
                None,
                false,
                PromptProfiles::headless(DispatchProfile::Full),
                None,
                None,
                None,
                None,
                Some(&settings),
                true,
                false,
                None,
                None,
                Some("fixture-turn"),
            )
        };
        let built = built
            .with_session_anchors(SessionAnchorRequest {
                settings: &settings,
                provider: &provider,
                current_path,
                channel_id: channel.get(),
                memory_scope_channel_id: channel.get(),
                role_binding: None,
                session_id,
                fresh,
            })
            .await;
        let system_prompt = crate::services::provider::system_prompt_for_provider_turn(
            &provider,
            session_id,
            &built.system_prompt,
        );
        agy::build_argv(&ProviderTurnRequest {
            provider: provider.clone(),
            prompt: "fixture question".into(),
            system_prompt: system_prompt.map(str::to_owned),
            tool_policy: ConfiguredToolPolicy::for_new_stream_json_provider(),
            model: None,
            reasoning_effort: None,
            working_directory: workspace.to_owned(),
            session: session_id.map(ProviderSessionToken::new_opaque),
            timeout: Duration::from_secs(1),
            cancel: None,
            auth_overlay: crate::services::provider_auth_profile::ProviderAuthOverlay::default_for(
                provider,
            ),
        })
        .unwrap()
    }

    fn agy_envelope_system(argv: &[String]) -> &str {
        let envelope = argv.windows(2).find(|pair| pair[0] == "--print").unwrap()[1].as_str();
        let (header, body) = envelope.split_once("\nSYSTEM:\n").unwrap();
        let (system, _) = body.split_once("\nEND_SYSTEM\nUSER:\n").unwrap();
        assert_eq!(header, format!("SYSTEM_LEN={}", system.len()));
        system
    }

    #[tokio::test]
    async fn fresh_agy_session_envelope_contains_memento_anchors_and_scope_hints() {
        with_memento_fixture(async |calls, workspace| {
            let argv = agy_turn_argv(workspace, 7_378_903, None, true).await;
            let system = agy_envelope_system(&argv);
            assert!(system.contains("[Proactive Memory Guidance]"));
            assert!(system.contains("- scope hints: read=`workspace=anchor-fixture, agentId="));
            assert!(
                system.contains("[Memento anchor context]"),
                "missing Memento anchor block"
            );
            assert!(
                system.contains("[ANCHOR MEMORY]\n- 합성 anchor 1\n\n[End Memento anchor context]")
            );
            assert!(!argv.iter().any(|arg| arg == "--conversation"));
            let calls = calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0]["workspace"], "anchor-fixture");
            assert_eq!(calls[0]["tokenBudget"], 1_000);
        })
        .await;
    }

    #[tokio::test]
    async fn agy_session_anchors_reuse_cache_and_reload_on_fresh_turn() {
        with_memento_fixture(async |calls, workspace| {
            let channel = 7_378_904;
            let first = agy_turn_argv(workspace, channel, None, false).await;
            assert!(agy_envelope_system(&first).contains("합성 anchor 1"));
            let followup = agy_turn_argv(workspace, channel, Some(AGY_SESSION), false).await;
            assert!(
                followup
                    .windows(2)
                    .any(|pair| pair == ["--conversation", AGY_SESSION])
            );
            assert!(agy_envelope_system(&followup).contains("합성 anchor 1"));
            assert_eq!(calls.lock().unwrap().len(), 1);

            let fresh = agy_turn_argv(workspace, channel, None, true).await;
            assert!(agy_envelope_system(&fresh).contains("합성 anchor 2"));
            assert_eq!(calls.lock().unwrap().len(), 2);
        })
        .await;
    }

    #[test]
    fn session_anchors_use_native_codex_developer_instructions() {
        let mut built = BuiltSystemPrompt {
            system_prompt: "existing instructions".into(),
            manifest: None,
            anchor_count: 0,
            inspection: false,
        };
        built.append_anchor_layer("[ANCHOR MEMORY]\n- persistent preference");
        let instructions = crate::services::codex::compose_codex_developer_instructions(
            Some(&built.system_prompt),
            None,
        )
        .unwrap();
        assert!(instructions.contains("existing instructions"));
        assert!(instructions.contains("[ANCHOR MEMORY]\n- persistent preference"));
        let options = crate::services::codex::CodexLaunchOptions::new("user task")
            .with_developer_instructions(Some(&instructions));
        for args in [
            crate::services::codex::build_codex_tui_args(&options),
            crate::services::codex::build_codex_exec_args(&options),
        ] {
            assert!(
                args.iter()
                    .any(|arg| arg.starts_with("developer_instructions=")
                        && arg.contains("persistent preference"))
            );
            assert_eq!(args.last().unwrap(), "user task");
        }
        assert!(
            crate::services::provider::system_prompt_for_provider_turn(
                &ProviderKind::Claude,
                Some("after-compact"),
                &built.system_prompt,
            )
            .unwrap()
            .contains("persistent preference")
        );
    }

    #[test]
    fn session_anchors_use_native_claude_system_prompt_on_launch_and_resume() {
        for resume in [false, true] {
            let config = crate::services::claude_tui::session::ClaudeTuiLaunchConfig {
                tmux_session_name: "anchor-launch-test".into(),
                working_dir: std::path::PathBuf::from("/tmp"),
                claude_bin: crate::services::claude_command::ClaudeBinary::from_tmux_wrapper_argv(
                    "claude",
                ),
                agentdesk_exe: std::path::PathBuf::from("agentdesk"),
                hook_endpoint: "http://127.0.0.1:49152".into(),
                session_id: "anchor-session".into(),
                system_prompt: Some("[ANCHOR MEMORY]\n- persistent preference".into()),
                model: None,
                resume,
            };
            let args = crate::services::claude_tui::session::build_claude_tui_args(
                &config,
                std::path::Path::new("/tmp/anchor-settings.json"),
            );
            assert!(
                args.windows(2)
                    .any(|pair| pair[0] == "--append-system-prompt"
                        && pair[1].contains("persistent preference"))
            );
        }
    }
}
