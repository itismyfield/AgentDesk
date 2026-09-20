use super::*;

impl BuiltSystemPrompt {
    #[allow(clippy::too_many_arguments)]
    pub(in crate::services::discord) async fn with_session_anchors(
        mut self,
        settings: &ResolvedMemorySettings,
        provider: &ProviderKind,
        current_path: &str,
        channel_id: ChannelId,
        memory_scope_channel_id: ChannelId,
        role_binding: Option<&RoleBinding>,
        session_id: Option<&str>,
        fresh: bool,
    ) -> Self {
        if let Some(anchors) = crate::services::memory::load_session_anchor_prompt(
            settings,
            provider,
            current_path,
            channel_id.get(),
            memory_scope_channel_id.get(),
            role_binding,
            session_id,
            fresh,
        )
        .await
        {
            self.append_anchor_layer(&anchors);
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

    #[test]
    fn session_anchors_use_native_codex_developer_instructions() {
        let mut built = BuiltSystemPrompt {
            system_prompt: "existing instructions".into(),
            manifest: None,
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
