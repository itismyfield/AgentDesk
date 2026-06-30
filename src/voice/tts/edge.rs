use super::{EDGE_TTS_TEMP_PREFIX, TtsBackend, TtsSynthesisKind};
use crate::voice::config::VoiceConfig;
use crate::voice::utils::expand_tilde;
use anyhow::{Context, Result, bail};
use futures::future::BoxFuture;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tokio::process::Command;
use tracing::warn;

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
            // F5 (#2046): STT/Receiver 와 동일하게 `~` 확장. config 가
            // `~/.adk/voice/tmp` 같은 home-relative 경로를 갖더라도 CWD 의
            // `~` 디렉터리에 임시 파일을 만들지 않도록 한다.
            temp_dir: expand_tilde(&config.audio.temp_dir),
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

    // reason: voice runtime is wired only when voice config is enabled; no
    // compile target exercises it. See #3034.
    #[allow(dead_code)]
    pub(crate) fn with_runner(config: EdgeTtsConfig, runner: EdgeTtsCommandRunner) -> Self {
        Self { config, runner }
    }

    fn invocation_for(&self, text: &str) -> EdgeTtsInvocation {
        let output_path = self.config.temp_dir.join(format!(
            "{}{}-{}.{}",
            EDGE_TTS_TEMP_PREFIX,
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

        // E (#3909): arm a drop guard over the temp output BEFORE running the
        // backend. A barge-in (`barge_in.rs` token cancel → `playback.rs`
        // `synth_task.abort()`) drops this future mid-`.await`; `kill_on_drop`
        // kills the child process but the already-written
        // `agentdesk-edge-tts-*.mp3` never reaches an explicit error-return
        // path, so it would orphan on disk. The guard removes it on
        // drop/cancellation and on every error branch below; it is disarmed
        // only once synthesis succeeds and ownership passes to the caller.
        let mut temp_guard = EdgeTtsTempGuard::arm(invocation.output_path.clone());

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

        temp_guard.disarm();
        Ok(invocation.output_path)
    }
}

/// #3909 (leak E) — drop guard that removes a partially written or orphaned
/// edge-tts temp mp3 whenever synthesis does not complete successfully. This
/// covers both the explicit error branches AND the barge-in abort case, where
/// the synth future is dropped mid-`.await` and never reaches any return path.
/// Disarmed once the output is validated and ownership passes to the caller
/// (the playback layer then owns chunk cleanup via `cleanup_synthesized_chunk`).
struct EdgeTtsTempGuard {
    path: Option<PathBuf>,
}

impl EdgeTtsTempGuard {
    fn arm(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for EdgeTtsTempGuard {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };
        // Synchronous best-effort unlink: a `Drop` impl cannot await
        // `tokio::fs`. A single unlink is cheap and only runs on the
        // abort/error path. On Unix this unlinks even while the
        // (kill_on_drop-terminated) child still holds the fd, so no bytes
        // survive on disk.
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                warn!(
                    path = %path.display(),
                    %error,
                    "failed to remove edge-tts temp output on drop/cancellation"
                );
            }
        }
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

    #[tokio::test]
    async fn edge_backend_removes_partial_output_on_runner_error() {
        let temp = tempfile::tempdir().unwrap();
        let seen_path = Arc::new(Mutex::new(None::<PathBuf>));
        let runner_seen_path = seen_path.clone();
        let runner: EdgeTtsCommandRunner = Arc::new(move |invocation| {
            let runner_seen_path = runner_seen_path.clone();
            Box::pin(async move {
                fs::write(&invocation.output_path, b"partial").await?;
                *runner_seen_path.lock().unwrap() = Some(invocation.output_path.clone());
                anyhow::bail!("mock edge-tts failure")
            })
        });
        let backend = EdgeTtsBackend::with_runner(test_config(temp.path().to_path_buf()), runner);

        let error = backend
            .synthesize("안녕하세요", TtsSynthesisKind::Progress)
            .await
            .unwrap_err();

        assert!(
            error.to_string().contains("run edge-tts for progress TTS"),
            "unexpected error: {error:?}"
        );
        let path = seen_path.lock().unwrap().clone().unwrap();
        assert!(
            !path.exists(),
            "partial edge-tts output should be removed on runner failure"
        );
    }

    #[test]
    fn from_voice_config_expands_tilde_in_temp_dir() {
        // F5 (#2046): config 의 `~/.adk/voice/tmp` 가 절대경로로 풀려야 한다.
        let mut voice = VoiceConfig::default();
        voice.audio.temp_dir = PathBuf::from("~/.adk/voice/tmp");
        let cfg = EdgeTtsConfig::from_voice_config(&voice);
        if let Some(home) = dirs::home_dir() {
            assert_eq!(cfg.temp_dir, home.join(".adk/voice/tmp"));
        } else {
            assert_eq!(cfg.temp_dir, PathBuf::from("~/.adk/voice/tmp"));
        }

        let mut absolute = VoiceConfig::default();
        absolute.audio.temp_dir = PathBuf::from("/var/tmp/agentdesk-voice");
        let cfg = EdgeTtsConfig::from_voice_config(&absolute);
        assert_eq!(cfg.temp_dir, PathBuf::from("/var/tmp/agentdesk-voice"));
    }

    #[tokio::test]
    async fn edge_backend_removes_empty_output_on_validation_error() {
        let temp = tempfile::tempdir().unwrap();
        let seen_path = Arc::new(Mutex::new(None::<PathBuf>));
        let runner_seen_path = seen_path.clone();
        let runner: EdgeTtsCommandRunner = Arc::new(move |invocation| {
            let runner_seen_path = runner_seen_path.clone();
            Box::pin(async move {
                fs::write(&invocation.output_path, b"").await?;
                *runner_seen_path.lock().unwrap() = Some(invocation.output_path.clone());
                Ok(())
            })
        });
        let backend = EdgeTtsBackend::with_runner(test_config(temp.path().to_path_buf()), runner);

        let error = backend
            .synthesize("안녕하세요", TtsSynthesisKind::Progress)
            .await
            .unwrap_err();

        assert!(
            error.to_string().contains("edge-tts produced empty output"),
            "unexpected error: {error:?}"
        );
        let path = seen_path.lock().unwrap().clone().unwrap();
        assert!(
            !path.exists(),
            "empty edge-tts output should be removed on validation failure"
        );
    }

    #[tokio::test]
    async fn edge_backend_removes_temp_output_on_synth_abort() {
        // E (#3909): a barge-in abort drops the synth future mid-`.await`
        // (playback.rs `synth_task.abort()`). The temp mp3 the runner already
        // wrote must be removed by the drop guard even though synthesis never
        // reached an explicit error-return path.
        let temp = tempfile::tempdir().unwrap();
        let written = Arc::new(tokio::sync::Notify::new());
        let seen_path = Arc::new(Mutex::new(None::<PathBuf>));
        let written_for_runner = written.clone();
        let seen_for_runner = seen_path.clone();
        let runner: EdgeTtsCommandRunner = Arc::new(move |invocation| {
            let written = written_for_runner.clone();
            let seen = seen_for_runner.clone();
            Box::pin(async move {
                // Simulate edge-tts having written the output file, then a long
                // synthesis still in flight when the barge-in abort lands.
                fs::write(&invocation.output_path, b"partial mp3 bytes").await?;
                *seen.lock().unwrap() = Some(invocation.output_path.clone());
                written.notify_one();
                // Never completes; the test aborts the task while parked here.
                futures::future::pending::<()>().await;
                Ok(())
            })
        });
        let backend = EdgeTtsBackend::with_runner(test_config(temp.path().to_path_buf()), runner);

        let handle = tokio::spawn(async move {
            backend
                .synthesize("안녕하세요", TtsSynthesisKind::Final)
                .await
        });

        // Wait until the runner has written the temp file, then abort mid-await.
        written.notified().await;
        let path = seen_path.lock().unwrap().clone().unwrap();
        assert!(path.exists(), "temp output should exist before the abort");

        handle.abort();
        let _ = handle.await;

        assert!(
            !path.exists(),
            "edge-tts temp output must be removed when the synth future is \
             aborted/dropped (barge-in), not just on explicit error returns"
        );
    }
}
