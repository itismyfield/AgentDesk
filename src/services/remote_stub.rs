//! Disabled remote SSH compatibility surface.
//!
//! #1606 removed user-facing remote execution, #2175 classified Codex remote
//! direct/tmux as "not allowed now", and #2193 defines the ADR prerequisites
//! for any future re-enable. Until those follow-ups land, these profile/auth
//! types exist only so provider signatures can continue to parse old shapes;
//! they are not a supported runtime configuration surface.

use serde::{Deserialize, Serialize};

/// Parse-compatibility only. Per `docs/codex-remote-ssh-policy.md`, password
/// and key-file credentials must not authenticate future remote SSH execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RemoteAuth {
    #[serde(rename = "password")]
    Password { password: String },
    #[serde(rename = "key_file")]
    KeyFile {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        passphrase: Option<String>,
    },
}

/// Parse-compatibility only. AgentDesk does not load `RemoteProfile` entries
/// from operator config, and remote dispatch paths must refuse before any SSH
/// attempt unless the #2193 prerequisites are fully implemented.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteProfile {
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub user: String,
    pub auth: RemoteAuth,
    #[serde(default)]
    pub default_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_path: Option<String>,
}

fn default_port() -> u16 {
    22
}

/// Disabled stub — always returns `Err`.
///
/// A real implementation must replace this module under the #2193 contract:
/// ssh-agent auth, strict host allow-list, no password/key-file credentials,
/// direct SSH only, and cancel-path coverage.
pub async fn ssh_connect_and_auth(_profile: &RemoteProfile) -> Result<SshConnectionStub, String> {
    Err("Remote SSH is disabled by policy (#2193); see docs/codex-remote-ssh-policy.md".to_string())
}

/// Placeholder type returned by ssh_connect_and_auth stub.
#[derive(Debug)]
pub struct SshConnectionStub;

#[cfg(test)]
mod tests {
    use super::{RemoteAuth, RemoteProfile, ssh_connect_and_auth};

    fn profile_with_auth(auth: RemoteAuth) -> RemoteProfile {
        RemoteProfile {
            name: "legacy-profile".to_string(),
            host: "mac-mini.local".to_string(),
            port: 22,
            user: "operator".to_string(),
            auth,
            default_path: "/tmp".to_string(),
            claude_path: None,
        }
    }

    #[test]
    fn password_and_keyfile_auth_parse_for_compatibility_only() {
        let password: RemoteProfile = serde_json::from_str(
            r#"{
                "name": "legacy-password",
                "host": "mac-mini.local",
                "user": "operator",
                "auth": { "password": { "password": "secret" } }
            }"#,
        )
        .expect("legacy password profile should still parse");
        assert!(matches!(password.auth, RemoteAuth::Password { .. }));

        let key_file: RemoteProfile = serde_json::from_str(
            r#"{
                "name": "legacy-key",
                "host": "mac-mini.local",
                "user": "operator",
                "auth": {
                    "key_file": {
                        "path": "/Users/operator/.ssh/id_ed25519",
                        "passphrase": "secret"
                    }
                }
            }"#,
        )
        .expect("legacy key-file profile should still parse");
        assert!(matches!(key_file.auth, RemoteAuth::KeyFile { .. }));
    }

    #[tokio::test]
    async fn ssh_connect_stub_refuses_before_auth_for_all_legacy_variants() {
        for auth in [
            RemoteAuth::Password {
                password: "secret".to_string(),
            },
            RemoteAuth::KeyFile {
                path: "/Users/operator/.ssh/id_ed25519".to_string(),
                passphrase: Some("secret".to_string()),
            },
        ] {
            let err = ssh_connect_and_auth(&profile_with_auth(auth))
                .await
                .expect_err("stub must refuse before any auth attempt");
            assert!(
                err.contains("disabled by policy"),
                "unexpected error: {err}"
            );
            assert!(err.contains("#2193"), "error must cite ADR issue: {err}");
        }
    }
}
