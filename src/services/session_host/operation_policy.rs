//! Read-only host policy observations. These values grant no execution or teardown authority.

use crate::services::session_host::HostKind;
use crate::services::tmux_common::host_marker::{HostKindMarker, read_host_kind_marker};

/// Why a Herdr-configured channel's tmux session is left as it is, whatever its stored rows say.
pub(crate) fn configured_refusal(channel_id: u64) -> Option<String> {
    let endpoint = crate::config::session_hosts::herdr_endpoint(channel_id)?;
    let key = endpoint.key;
    Some(format!(
        "Herdr 설정 채널(endpoint `{key}`)이라 tmux 세션을 바꾸지 않아요"
    ))
}

/// Whether the marker alone admits a tmux observation; rows can still refuse it.
pub(crate) fn marker_is_tmux(marker: &HostKindMarker) -> bool {
    matches!(
        marker,
        HostKindMarker::Absent | HostKindMarker::Known(HostKind::Tmux)
    )
}

/// The `.host_kind` marker check alone, for a path holding only a tmux name.
pub(crate) fn marker_refusal(tmux_name: &str) -> Option<String> {
    let marker = read_host_kind_marker(tmux_name);
    let tmux = marker_is_tmux(&marker);
    if !tmux {
        tracing::info!(
            name = tmux_name,
            ?marker,
            row_tmux = true,
            "tmux probe deferred: host is not tmux"
        );
    }
    (!tmux).then(|| format!("the host marker of {tmux_name} is not tmux"))
}

/// A provider-auth login target: only the auth namespace with no other host's marker.
pub(crate) fn auth_login_refusal(tmux_name: &str) -> Option<String> {
    if !tmux_name.starts_with("adk-login-") {
        return Some(format!("{tmux_name} is not a provider-auth login session"));
    }
    marker_refusal(tmux_name)
}

/// Each caller observes the current live switch before its own operation.
pub(crate) fn terminate_enabled() -> bool {
    crate::config_live_reload::current()
        .is_some_and(|config| config.runtime.herdr_terminate_enabled == Some(true))
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

    #[test]
    fn configured_and_login_refusals_keep_their_exact_text() {
        let _hosts = crate::config::session_hosts::force_for_test(Some("node"), &[(17, "node")]);
        assert_eq!(
            configured_refusal(17).as_deref(),
            Some("Herdr 설정 채널(endpoint `node-endpoint`)이라 tmux 세션을 바꾸지 않아요")
        );
        assert_eq!(configured_refusal(18), None);
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
