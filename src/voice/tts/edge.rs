use super::{TtsBackend, TtsSynthesisKind};
use crate::voice::config::VoiceConfig;
use anyhow::{Context, Result, bail};
use futures::future::BoxFuture;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tokio::process::Command;

const DEFAULT_EDGE_TTS_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EdgeTtsConfig {
    pub(crate) command: String,
    pub(crate) voice: String,
    pub(crate) rate: String,
    pub(crate) temp_dir: PathBuf,
    pub(crate) timeout: Duration,
}

impl EdgeTtsConfig {
    pub(crate) fn from_voice_config(config: &VoiceConfig) -> Self {
        Self {
            command: config.tts.edge.command.clone(),
            voice: config.tts.edge.voice.clone(),
            rate: config.tts.edge.rate.clone(),
            temp_dir: config.audio.temp_dir.clone(),
            timeout: DEFAULT_EDGE_TTS_TIMEOUT,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EdgeTtsInvocation {
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
    pub(crate) output_path: PathBuf,
}

pub(crate) type EdgeTtsCommandRunner =
    Arc<dyn Fn(EdgeTtsInvocation) -> BoxFuture<'static, Result<()>> + Send + Sync>;

#[derive(Clone)]
pub(crate) struct EdgeTtsBackend {
    config: EdgeTtsConfig,
    runner: EdgeTtsCommandRunner,
}

impl EdgeTtsBackend {
    pub(crate) fn from_voice_config(config: &VoiceConfig) -> Self {
        Self::new(EdgeTtsConfig::from_voice_config(config))
    }

    pub(crate) fn new(config: EdgeTtsConfig) -> Self {
        let runner = subprocess_runner(config.timeout);
        Self { config, runner }
    }

    pub(crate) fn with_runner(config: EdgeTtsConfig, runner: EdgeTtsCommandRunner) -> Self {
        Self { config, runner }
    }

    fn invocation_for(&self, text: &str) -> EdgeTtsInvocation {
        let output_path = self.config.temp_dir.join(format!(
            "agentdesk-edge-tts-{}-{}.{}",
            std::process::id(),
            uuid::Uuid::new_v4(),
            self.output_extension()
        ));
        let output_arg = output_path.to_string_lossy().to_string();

        EdgeTtsInvocation {
            program: self.config.command.clone(),
            args: vec![
                "-v".to_string(),
                self.config.voice.clone(),
                "--rate".to_string(),
                self.config.rate.clone(),
                "-t".to_string(),
                text.to_string(),
                "--write-media".to_string(),
                output_arg,
            ],
            output_path,
        }
    }
}

impl TtsBackend for EdgeTtsBackend {
    fn cache_key_parts(&self) -> Vec<String> {
        vec![
            "edge".to_string(),
            self.config.voice.clone(),
            self.config.rate.clone(),
        ]
    }

    fn output_extension(&self) -> &'static str {
        "mp3"
    }

    async fn synthesize(&self, text: &str, kind: TtsSynthesisKind) -> Result<PathBuf> {
        let invocation = self.invocation_for(text);
        if let Some(parent) = invocation.output_path.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("create edge-tts temp dir {}", parent.display()))?;
        }

        (self.runner)(invocation.clone())
            .await
            .with_context(|| format!("run edge-tts for {} TTS", kind.as_str()))?;

        let metadata = fs::metadata(&invocation.output_path)
            .await
            .with_context(|| {
                format!("stat edge-tts output {}", invocation.output_path.display())
            })?;
        if !metadata.is_file() || metadata.len() == 0 {
            bail!(
                "edge-tts produced empty output: {}",
                invocation.output_path.display()
            );
        }

        Ok(invocation.output_path)
    }
}

fn subprocess_runner(timeout: Duration) -> EdgeTtsCommandRunner {
    Arc::new(move |invocation| {
        Box::pin(async move {
            let mut command = Command::new(&invocation.program);
            command.args(&invocation.args);
            command.kill_on_drop(true);

            let output = tokio::time::timeout(timeout, command.output())
                .await
                .with_context(|| {
                    format!(
                        "edge-tts timed out after {}s: {}",
                        timeout.as_secs(),
                        invocation.program
                    )
                })?
                .with_context(|| format!("spawn edge-tts command {}", invocation.program))?;

            if !output.status.success() {
                bail!(
                    "edge-tts exited with status {}; stderr: {}; stdout: {}",
                    output.status,
                    preview_output(&output.stderr),
                    preview_output(&output.stdout)
                );
            }

            Ok(())
        })
    })
}

fn preview_output(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "<empty>".to_string();
    }
    trimmed.chars().take(2048).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn test_config(temp_dir: PathBuf) -> EdgeTtsConfig {
        EdgeTtsConfig {
            command: "edge-tts".to_string(),
            voice: "ko-KR-SunHiNeural".to_string(),
            rate: "+5%".to_string(),
            temp_dir,
            timeout: Duration::from_secs(60),
        }
    }

    #[tokio::test]
    async fn edge_backend_invokes_expected_command_args() {
        let temp = tempfile::tempdir().unwrap();
        let seen = Arc::new(Mutex::new(Vec::<EdgeTtsInvocation>::new()));
        let runner_seen = seen.clone();
        let runner: EdgeTtsCommandRunner = Arc::new(move |invocation| {
            let runner_seen = runner_seen.clone();
            Box::pin(async move {
                fs::write(&invocation.output_path, b"mp3 bytes").await?;
                runner_seen.lock().unwrap().push(invocation);
                Ok(())
            })
        });
        let backend = EdgeTtsBackend::with_runner(test_config(temp.path().to_path_buf()), runner);

        let path = backend
            .synthesize("안녕하세요", TtsSynthesisKind::Final)
            .await
            .unwrap();

        assert_eq!(fs::read(&path).await.unwrap(), b"mp3 bytes");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let invocation = &seen[0];
        assert_eq!(invocation.program, "edge-tts");
        assert_eq!(
            invocation.args,
            vec![
                "-v",
                "ko-KR-SunHiNeural",
                "--rate",
                "+5%",
                "-t",
                "안녕하세요",
                "--write-media",
                path.to_str().unwrap()
            ]
        );
        assert_eq!(
            backend.cache_key_parts(),
            vec!["edge", "ko-KR-SunHiNeural", "+5%"]
        );
    }

    #[tokio::test]
    async fn edge_backend_rejects_missing_output() {
        let temp = tempfile::tempdir().unwrap();
        let runner: EdgeTtsCommandRunner = Arc::new(move |_invocation| Box::pin(async { Ok(()) }));
        let backend = EdgeTtsBackend::with_runner(test_config(temp.path().to_path_buf()), runner);

        let error = backend
            .synthesize("안녕하세요", TtsSynthesisKind::Progress)
            .await
            .unwrap_err();

        assert!(
            error.to_string().contains("stat edge-tts output"),
            "unexpected error: {error:?}"
        );
    }
}
