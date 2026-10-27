use super::*;
use crate::config::TestEnvVarGuard as Guard;
use crate::services::claude_tui::hook_server::{
    adoption_retry::DurableKind,
    observation_ingress::{IngressOutcome, observe_binding_hook},
};
use crate::services::codex_tui::canary::{CANARY_CHANNEL, CANARY_TMUX};
use crate::services::codex_tui::session::source_observation::SOURCE_MODE_TEST;
use crate::services::tui_prompt_dedupe::{
    self as dedupe,
    binding_context::{
        self, BINDING_HEADER, CapturedContext, HookBindingEnvelope, ObservedHookProcess,
    },
    binding_events,
};

const ID: &str = "019e660d-4859-7522-9cee-8ba7c4e7c745";

/// Launches a verified fresh incarnation whose CODEX_HOME is a symlink, then reports its
/// rollout through the hook ingress under the alias or the resolved spelling.
fn adopt_under_symlinked_home(via_alias: bool) -> IngressOutcome {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _state = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (root, _env) = binding_context::tests::fixture_after_shared_test_env_lock();
    let _tmux = binding_context::tests::fake_tmux(root.path());
    let _hosts = crate::config::session_hosts::force_for_test(None, &[]);
    let _endpoint_lock = crate::services::claude_tui::hook_server::tests::ENDPOINT_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let _endpoint = crate::services::claude_tui::hook_server::publish_hook_endpoint(
        "http://127.0.0.1:9".into(),
    );
    let _hooks = Guard::set_value_after_shared_test_env_lock(
        "AGENTDESK_CODEX_DIRECT_TUI_HOOKS",
        std::ffi::OsStr::new("1"),
    );
    // Verified eligibility probes the CLI; a fixture keeps the test off any installed Codex.
    let cli = root.path().join("codex-fixture");
    std::fs::write(&cli, "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'codex-cli 0.157.1'; else echo '--dangerously-bypass-hook-trust'; fi\n").unwrap();
    std::fs::set_permissions(&cli, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let _cli = Guard::set_path_after_shared_test_env_lock("AGENTDESK_CODEX_PATH", &cli);
    dedupe::reset_state_for_tests();
    binding_events::set_test_root(Some(root.path()));
    let real = root.path().join("real-home");
    std::fs::create_dir_all(real.join("sessions")).unwrap();
    let alias = root.path().join("alias-home");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let _home = Guard::set_path_after_shared_test_env_lock("CODEX_HOME", &alias);
    let overlay = crate::services::provider_auth_profile::ProviderAuthOverlay::default_for(
        ProviderKind::Codex,
    );
    SOURCE_MODE_TEST.with(|m| m.set(Some(CodexSourceMode::Verified)));
    let prepared = prepare_codex_tui_incarnation_with_policy(
        CANARY_TMUX,
        Some(CANARY_CHANNEL),
        &CodexLaunchOptions::new("symlinked home prompt"),
        &overlay,
        None,
        false,
    );
    SOURCE_MODE_TEST.with(|m| m.set(None));
    let context = prepared.unwrap().context;
    assert_eq!(context.source_policy.as_deref(), Some("verified"));
    let marker = crate::services::tmux_common::session_temp_path(CANARY_TMUX, "spawn_nonce");
    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(&marker, &context.execution_nonce).unwrap();
    dedupe::register_tmux_channel(CANARY_TMUX, CANARY_CHANNEL);
    let _fake =
        crate::services::provider_teardown::tests::test_support::FakeTmux::install(CANARY_TMUX);
    let home = if via_alias {
        alias
    } else {
        real.canonicalize().unwrap()
    };
    let rollout = home.join("sessions").join(format!("rollout-{ID}.jsonl"));
    let stamp = (context.created_at + chrono::Duration::seconds(1)).to_rfc3339();
    let header = serde_json::json!({"type":"session_meta", "timestamp":stamp, "payload":{"id":ID,
        "timestamp":stamp, "cwd":root.path(), "source":"cli", "originator":"codex_cli_rs"}});
    std::fs::write(&rollout, format!("{header}\n")).unwrap();
    let envelope = HookBindingEnvelope {
        context: CapturedContext::Captured(context),
        observed: ObservedHookProcess::default(),
    };
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
    let payload =
        serde_json::json!({"session_id":ID, "transcript_path":rollout, "source":"startup"});
    let outcome = observe_binding_hook(
        "codex",
        "session-start",
        Some(ID),
        Some(ID),
        &payload,
        &headers,
    );
    binding_events::forget_channel_for_tests(CANARY_CHANNEL);
    binding_events::set_test_root(None);
    dedupe::reset_state_for_tests();
    outcome
}

#[test]
fn verified_launch_under_symlinked_codex_home_adopts_either_rollout_spelling() {
    let outcomes = [false, true].map(adopt_under_symlinked_home);
    assert_eq!(
        outcomes,
        [IngressOutcome::Durable(DurableKind::Adopted); 2],
        "a symlinked CODEX_HOME must not demote the launch to legacy publication \
         (resolved spelling, alias spelling)"
    );
}
