//! Read-only host policy observations. These values grant no execution or teardown authority.

use crate::services::session_host::{HostKind, HostLiveness, HostSessionRef, host_for};
use crate::services::tmux_common::host_marker::{HostKindMarker, read_host_kind_marker};
pub use crate::services::tmux_common::host_marker::{marker_is_tmux, marker_refusal};

/// A provider-auth login target: only the auth namespace with no other host's marker.
pub fn auth_login_refusal(tmux_name: &str) -> Option<String> {
    if !tmux_name.starts_with("adk-login-") {
        return Some(format!("{tmux_name} is not a provider-auth login session"));
    }
    marker_refusal(tmux_name)
}

/// Each caller observes the current live switch before its own operation.
pub fn terminate_enabled() -> bool {
    crate::config_live_reload::current()
        .is_some_and(|config| config.runtime.herdr_terminate_enabled == Some(true))
}

/// Only an explicit local tmux/process marker permits a historical liveness probe.
pub fn historical_execution_liveness(name: &str) -> Option<HostLiveness> {
    let HostKindMarker::Known(kind @ (HostKind::Tmux | HostKind::Process)) =
        read_host_kind_marker(name)
    else {
        return None;
    };
    Some(host_for(kind).liveness(HostSessionRef { kind, name }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_states_keep_observation_and_refusal_equivalent() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let name = "adk-login-observation";
        let path = crate::services::tmux_common::session_temp_path(name, "host_kind");
        assert_eq!(marker_refusal(name), None);
        assert_eq!(auth_login_refusal(name), None);
        let refused = Some(format!("the host marker of {name} is not tmux"));
        for (raw, tmux) in [
            ("tmux", true),
            ("herdr\n", false),
            ("process", false),
            ("zellij", false),
            ("", false),
        ] {
            std::fs::write(&path, raw).unwrap();
            assert_eq!(
                marker_refusal(name),
                if tmux { None } else { refused.clone() },
                "{raw:?}"
            );
            assert_eq!(
                auth_login_refusal(name),
                if tmux { None } else { refused.clone() },
                "{raw:?}"
            );
        }
        std::fs::write(&path, [0xff, 0xfe]).unwrap();
        assert_eq!(marker_refusal(name), refused);
        assert_eq!(auth_login_refusal(name), refused);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert_eq!(marker_refusal(name), refused);
        assert_eq!(auth_login_refusal(name), refused);
    }

    #[tokio::test]
    async fn configured_and_login_refusals_keep_their_exact_text() {
        use crate::services::session_host::host_decision::host_decision;
        let _hosts = crate::config::session_hosts::force_for_test(Some("node"), &[(17, "node")]);
        let refusal = async |channel| {
            host_decision(None, None, None, channel)
                .await
                .tmux_refusal()
        };
        assert_eq!(
            refusal(17).await.as_deref(),
            Some("Herdr 설정 채널(endpoint `node-endpoint`)이라 tmux 세션을 바꾸지 않아요")
        );
        assert_eq!(refusal(18).await, None);
        assert_eq!(
            auth_login_refusal("AgentDesk-login-x").as_deref(),
            Some("AgentDesk-login-x is not a provider-auth login session")
        );
        assert_eq!(
            auth_login_refusal("adk-login").as_deref(),
            Some("adk-login is not a provider-auth login session")
        );
    }

    #[test]
    fn terminate_switch_reads_each_live_snapshot() {
        let lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let _live = crate::config_live_reload::test_live_guard(lock);
        let mut config = crate::config::Config::default();
        for (flag, expected) in [
            (Some(true), true),
            (Some(false), false),
            (Some(true), true),
            (None, false),
        ] {
            config.runtime.herdr_terminate_enabled = flag;
            crate::config_live_reload::install(config.clone());
            assert_eq!(terminate_enabled(), expected, "{flag:?}");
        }
    }
}
