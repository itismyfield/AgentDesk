//! External voice runtime process launch primitives.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::voice::runtime_boundary::VOICE_RUNTIME_PROTOCOL_VERSION;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct VoiceRuntimeProcessConfig {
    pub enabled: bool,
    pub command: Option<PathBuf>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

impl Default for VoiceRuntimeProcessConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            command: None,
            args: Vec::new(),
            env: BTreeMap::new(),
        }
    }
}

impl VoiceRuntimeProcessConfig {
    pub(crate) fn launch_spec(&self) -> Option<VoiceRuntimeLaunchSpec> {
        if !self.enabled {
            return None;
        }

        let executable = self.command.clone()?;
        Some(VoiceRuntimeLaunchSpec {
            executable,
            args: self.args.clone(),
            env: self.env.clone(),
            protocol_version: VOICE_RUNTIME_PROTOCOL_VERSION,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VoiceRuntimeLaunchSpec {
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub protocol_version: u16,
}

#[async_trait]
pub(crate) trait VoiceRuntimeProcessSupervisor: Send + Sync {
    async fn start(&self, spec: VoiceRuntimeLaunchSpec) -> Result<Box<dyn VoiceRuntimeProcess>>;
}

#[async_trait]
pub(crate) trait VoiceRuntimeProcess: Send + Sync {
    fn protocol_version(&self) -> u16;
    fn child_id(&self) -> Option<u32>;
    async fn stop(&self) -> Result<()>;
}

#[derive(Debug, Default)]
pub(crate) struct DisabledVoiceRuntimeProcessSupervisor;

#[async_trait]
impl VoiceRuntimeProcessSupervisor for DisabledVoiceRuntimeProcessSupervisor {
    async fn start(&self, _spec: VoiceRuntimeLaunchSpec) -> Result<Box<dyn VoiceRuntimeProcess>> {
        Err(anyhow!(
            "external voice runtime process launch is disabled by configuration"
        ))
    }
}

#[derive(Debug, Default)]
pub(crate) struct TokioVoiceRuntimeProcessSupervisor;

#[async_trait]
impl VoiceRuntimeProcessSupervisor for TokioVoiceRuntimeProcessSupervisor {
    async fn start(&self, spec: VoiceRuntimeLaunchSpec) -> Result<Box<dyn VoiceRuntimeProcess>> {
        let mut command = Command::new(&spec.executable);
        command
            .args(&spec.args)
            .envs(&spec.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let child = command.spawn().with_context(|| {
            format!(
                "launch external voice runtime process `{}`",
                spec.executable.display()
            )
        })?;

        Ok(Box::new(TokioVoiceRuntimeProcess {
            protocol_version: spec.protocol_version,
            child: Mutex::new(child),
        }))
    }
}

struct TokioVoiceRuntimeProcess {
    protocol_version: u16,
    child: Mutex<Child>,
}

#[async_trait]
impl VoiceRuntimeProcess for TokioVoiceRuntimeProcess {
    fn protocol_version(&self) -> u16 {
        self.protocol_version
    }

    fn child_id(&self) -> Option<u32> {
        self.child.try_lock().ok().and_then(|child| child.id())
    }

    async fn stop(&self) -> Result<()> {
        let mut child = self.child.lock().await;
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        child
            .kill()
            .await
            .context("stop external voice runtime process")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_config_is_disabled_by_default() {
        let config = VoiceRuntimeProcessConfig::default();

        assert!(!config.enabled);
        assert!(config.launch_spec().is_none());
    }

    #[test]
    fn process_config_builds_launch_spec_only_when_enabled() {
        let mut env = BTreeMap::new();
        env.insert("ADK_VOICE_RUNTIME".to_string(), "external".to_string());
        let config = VoiceRuntimeProcessConfig {
            enabled: true,
            command: Some(PathBuf::from("/bin/agentdesk-voice-runtime")),
            args: vec!["--stdio".to_string()],
            env,
        };

        let spec = config.launch_spec().unwrap();

        assert_eq!(
            spec.executable,
            PathBuf::from("/bin/agentdesk-voice-runtime")
        );
        assert_eq!(spec.args, vec!["--stdio"]);
        assert_eq!(
            spec.env.get("ADK_VOICE_RUNTIME").map(String::as_str),
            Some("external")
        );
        assert_eq!(spec.protocol_version, VOICE_RUNTIME_PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn disabled_supervisor_never_spawns() {
        let supervisor = DisabledVoiceRuntimeProcessSupervisor;
        let spec = VoiceRuntimeLaunchSpec {
            executable: PathBuf::from("/bin/false"),
            args: Vec::new(),
            env: BTreeMap::new(),
            protocol_version: VOICE_RUNTIME_PROTOCOL_VERSION,
        };

        let error = match supervisor.start(spec).await {
            Ok(_) => panic!("disabled supervisor unexpectedly spawned a process"),
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .contains("external voice runtime process launch is disabled")
        );
    }
}
