//! Stub module providing type definitions formerly in services::remote.
//! SSH/SFTP functionality is not available in AgentDesk — only types are provided
//! so that other modules (claude, codex) compile without modification.

use serde::{Deserialize, Serialize};

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

/// Stub — always returns Err. Real implementation requires russh.
pub async fn ssh_connect_and_auth(_profile: &RemoteProfile) -> Result<SshConnectionStub, String> {
    Err("SSH not available in AgentDesk build".to_string())
}

/// Placeholder type returned by ssh_connect_and_auth stub.
pub struct SshConnectionStub;
