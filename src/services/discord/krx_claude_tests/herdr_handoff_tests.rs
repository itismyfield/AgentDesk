//! Moved from src/services/claude/herdr_turn/handoff.rs: uses the discord herdr owner port.

use crate::db::dispatched_sessions::hosted_execution::{HostedExecution, HostedOwner};
use crate::domain::provider_auth_profile::ProviderAuthOverlay;
use crate::services::claude::herdr_turn::handoff::*;
use crate::services::claude::herdr_turn::{AttachRequest, HerdrTurn, HerdrTurnPorts};
use crate::services::claude_tui::hook_server::HookEvent;
use crate::services::herdr_launch::{HerdrLaunchEndpoint, HerdrLaunchHost};
use crate::services::provider::ProviderKind;
use crate::services::session_host::{HerdrTarget, SocketHerdrLaunchHost};
use std::sync::Arc;
use tokio::sync::broadcast;

#[test]
fn test_instance_claude_herdr_default_profile_refuses_before_launch_subscription() {
    crate::services::provider_auth_profile::instance_env_tests::with_instance(
        concat!(
            module_path!(),
            "::test_instance_claude_herdr_default_profile_refuses_before_launch_subscription"
        ),
        || {
            struct Ports;
            impl HerdrTurnPorts for Ports {
                fn owner_port(&self) -> crate::services::herdr_launch::OwnerPort {
                    crate::services::discord::HERDR_OWNER_PORT
                }
                fn launch_host(&self) -> Option<Arc<dyn HerdrLaunchHost>> {
                    Some(Arc::new(SocketHerdrLaunchHost::new(
                        vec![],
                        Default::default(),
                    )))
                }
                fn hook_events(&self) -> broadcast::Receiver<HookEvent> {
                    panic!("default profile must refuse before launch subscription")
                }
                fn attach(&self, _: &AttachRequest<'_>) -> Result<bool, String> {
                    panic!("default profile must never attach")
                }
                fn confirm_bound(
                    &self,
                    _: &HostedOwner,
                    _: &HostedExecution,
                    _: &HerdrTarget,
                ) -> Result<(), String> {
                    panic!("default profile must never bind")
                }
                fn auth_overlay(&self, _: &str) -> Result<ProviderAuthOverlay, String> {
                    Ok(ProviderAuthOverlay::default_for(ProviderKind::Claude))
                }
            }
            let instance = crate::config::test_instance::current().unwrap().unwrap();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let _entered = runtime.enter();
            let pool = sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://unused@127.0.0.1:1/unused")
                .unwrap();
            let turn = HerdrTurn {
                pool: &pool,
                owner: HostedOwner {
                    provider: "claude".into(),
                    discord_token_hash: "synthetic".into(),
                    channel_id: "41".into(),
                    logical_key: format!("{}-claude-herdr-env", instance.tmux.prefix),
                    owner_node: instance.instance_id.clone(),
                    runtime_root: instance.root.display().to_string(),
                },
                channel_id: 41,
                endpoint: HerdrLaunchEndpoint {
                    execution_node: instance.instance_id.clone(),
                    config_key: instance.herdr.endpoint_key.clone(),
                    socket_addr: instance.herdr.socket.display().to_string(),
                    herdr_session: instance.herdr.session.clone(),
                },
                row: None,
                prompt: "scope prompt",
                working_dir: instance.root.to_str().unwrap(),
                session_id: None,
                force_fresh: true,
                system_prompt: None,
                model: None,
                hook_endpoint: Some("http://127.0.0.1:9".into()),
                cancel: None,
            };
            assert_eq!(
                launched_source(&turn, runtime.handle(), &Ports)
                    .err()
                    .as_deref(),
                Some("herdr launch auth_overlay: test instance host declaration rejected")
            );
        },
    );
}
