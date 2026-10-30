//! Disabled remote SSH surface; AgentDesk has no remote runtime.
//!
//! Only the Codex dispatch parameter still takes these types, and it always refuses them.

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

/// Never loaded from operator config; Codex dispatch refuses it before any SSH attempt.
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

#[cfg(test)]
mod tests {
    use super::{RemoteAuth, RemoteProfile};

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
}
