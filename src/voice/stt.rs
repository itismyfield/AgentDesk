use anyhow::{Context, Result, bail};
use futures::future::BoxFuture;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tokio::process::Command;
use tracing::{debug, warn};

use super::VoiceConfig;

const STT_TIMEOUT: Duration = Duration::from_secs(120);
const EMPTY_RETRY_DELAY: Duration = Duration::from_millis(300);
const LOW_VOLUME_MEAN_DB: f32 = -35.0;
const LOW_VOLUME_MAX_DB: f32 = -12.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SttConfig {
    pub(crate) ffmpeg_command: String,
    pub(crate) whisper_command: String,
    pub(crate) model_path: PathBuf,
    pub(crate) language: String,
    pub(crate) temp_dir: PathBuf,
    pub(crate) timeout: Duration,
}

impl SttConfig {
    pub(crate) fn from_voice_config(config: &VoiceConfig) -> Self {
        Self {
            ffmpeg_command: config.stt.ffmpeg_command.clone(),
            whisper_command: config.stt.whisper_command.clone(),
            model_path: expand_tilde(&config.stt.model_path),
            language: config.stt.language.clone(),
            temp_dir: expand_tilde(&config.audio.temp_dir),
            timeout: STT_TIMEOUT,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SttCommandKind {
    VolumeDetect,
    Convert,
    Whisper,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SttCommandInvocation {
    pub(crate) kind: SttCommandKind,
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
    pub(crate) output_path: Option<PathBuf>,
    pub(crate) transcript_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SttCommandOutput {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

pub(crate) type SttCommandRunner =
    Arc<dyn Fn(SttCommandInvocation) -> BoxFuture<'static, Result<SttCommandOutput>> + Send + Sync>;

#[derive(Clone)]
pub(crate) struct SttRuntime {
    config: SttConfig,
    runner: SttCommandRunner,
}

impl SttRuntime {
    pub(crate) fn from_voice_config(config: &VoiceConfig) -> Self {
        Self::new(SttConfig::from_voice_config(config))
    }

    pub(crate) fn new(config: SttConfig) -> Self {
        let runner = subprocess_runner(config.timeout);
        Self { config, runner }
    }

    pub(crate) fn with_runner(config: SttConfig, runner: SttCommandRunner) -> Self {
        Self { config, runner }
    }

    pub(crate) async fn transcribe(&self, wav_path: impl AsRef<Path>) -> Result<String> {
        let wav_path = wav_path.as_ref();
        if self.is_low_volume_utterance(wav_path).await? {
            debug!(
                path = %wav_path.display(),
                "voice STT skipped low-volume utterance"
            );
            return Ok(String::new());
        }

        fs::create_dir_all(&self.config.temp_dir)
            .await
            .with_context(|| format!("create STT temp dir {}", self.config.temp_dir.display()))?;

        let temp_id = format!(
            "agentdesk-stt-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        );
        let converted_path = self.config.temp_dir.join(format!("{temp_id}.wav"));
        let transcript_prefix = self.config.temp_dir.join(format!("{temp_id}-transcript"));
        let transcript_path = transcript_prefix.with_extension("txt");

        let result = async {
            self.convert_for_whisper(wav_path, &converted_path).await?;
            self.run_whisper_with_retry(&converted_path, &transcript_prefix, &transcript_path)
                .await
        }
        .await;

        cleanup_temp_file(&converted_path).await;
        cleanup_temp_file(&transcript_path).await;
        result
    }

    async fn is_low_volume_utterance(&self, wav_path: &Path) -> Result<bool> {
        let invocation = SttCommandInvocation {
            kind: SttCommandKind::VolumeDetect,
            program: self.config.ffmpeg_command.clone(),
            args: vec![
                "-hide_banner".to_string(),
                "-nostats".to_string(),
                "-i".to_string(),
                wav_path.to_string_lossy().to_string(),
                "-af".to_string(),
                "volumedetect".to_string(),
                "-f".to_string(),
                "null".to_string(),
                "-".to_string(),
            ],
            output_path: None,
            transcript_path: None,
        };
        let output = (self.runner)(invocation)
            .await
            .with_context(|| format!("run ffmpeg volumedetect for {}", wav_path.display()))?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        let Some(levels) = parse_volume_levels(&stderr) else {
            warn!(
                path = %wav_path.display(),
                "ffmpeg volumedetect output did not contain mean/max volume; continuing with STT"
            );
            return Ok(false);
        };
        Ok(levels.mean_db < LOW_VOLUME_MEAN_DB && levels.max_db < LOW_VOLUME_MAX_DB)
    }

    async fn convert_for_whisper(&self, wav_path: &Path, output_path: &Path) -> Result<()> {
        let invocation = SttCommandInvocation {
            kind: SttCommandKind::Convert,
            program: self.config.ffmpeg_command.clone(),
            args: vec![
                "-y".to_string(),
                "-hide_banner".to_string(),
                "-loglevel".to_string(),
                "error".to_string(),
                "-i".to_string(),
                wav_path.to_string_lossy().to_string(),
                "-ac".to_string(),
                "1".to_string(),
                "-ar".to_string(),
                "16000".to_string(),
                "-f".to_string(),
                "wav".to_string(),
                output_path.to_string_lossy().to_string(),
            ],
            output_path: Some(output_path.to_path_buf()),
            transcript_path: None,
        };
        (self.runner)(invocation).await.with_context(|| {
            format!(
                "convert utterance WAV {} to 16k mono {}",
                wav_path.display(),
                output_path.display()
            )
        })?;
        Ok(())
    }

    async fn run_whisper_with_retry(
        &self,
        converted_path: &Path,
        transcript_prefix: &Path,
        transcript_path: &Path,
    ) -> Result<String> {
        for attempt in 0..=1 {
            if attempt > 0 {
                tokio::time::sleep(EMPTY_RETRY_DELAY).await;
            }
            cleanup_temp_file(transcript_path).await;

            let output = self
                .run_whisper(converted_path, transcript_prefix, transcript_path)
                .await?;
            let raw = read_whisper_text(transcript_path, &output).await?;
            let cleaned = clean_transcript(&raw);
            if !cleaned.is_empty() {
                return Ok(cleaned);
            }
        }

        Ok(String::new())
    }

    async fn run_whisper(
        &self,
        converted_path: &Path,
        transcript_prefix: &Path,
        transcript_path: &Path,
    ) -> Result<SttCommandOutput> {
        let invocation = SttCommandInvocation {
            kind: SttCommandKind::Whisper,
            program: self.config.whisper_command.clone(),
            args: vec![
                "-m".to_string(),
                self.config.model_path.to_string_lossy().to_string(),
                "-f".to_string(),
                converted_path.to_string_lossy().to_string(),
                "-l".to_string(),
                self.config.language.clone(),
                "-nt".to_string(),
                "-otxt".to_string(),
                "-sns".to_string(),
                "-nf".to_string(),
                "-nth".to_string(),
                "0.35".to_string(),
                "-et".to_string(),
                "2.2".to_string(),
                "-lpt".to_string(),
                "-0.8".to_string(),
                "-of".to_string(),
                transcript_prefix.to_string_lossy().to_string(),
            ],
            output_path: None,
            transcript_path: Some(transcript_path.to_path_buf()),
        };
        (self.runner)(invocation).await.with_context(|| {
            format!(
                "run whisper-cli for converted utterance {}",
                converted_path.display()
            )
        })
    }
}

pub(crate) async fn transcribe(wav_path: impl AsRef<Path>) -> Result<String> {
    let config = VoiceConfig::default();
    transcribe_with_config(wav_path, &config).await
}

pub(crate) async fn transcribe_with_config(
    wav_path: impl AsRef<Path>,
    config: &VoiceConfig,
) -> Result<String> {
    SttRuntime::from_voice_config(config)
        .transcribe(wav_path)
        .await
}

async fn read_whisper_text(path: &Path, output: &SttCommandOutput) -> Result<String> {
    match fs::read_to_string(path).await {
        Ok(text) => {
            if text.trim().is_empty() {
                Ok(String::from_utf8_lossy(&output.stdout).to_string())
            } else {
                Ok(text)
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        }
        Err(error) => {
            Err(error).with_context(|| format!("read whisper transcript output {}", path.display()))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct VolumeLevels {
    mean_db: f32,
    max_db: f32,
}

fn parse_volume_levels(output: &str) -> Option<VolumeLevels> {
    let mean_db = parse_db_value(output, "mean_volume:")?;
    let max_db = parse_db_value(output, "max_volume:")?;
    Some(VolumeLevels { mean_db, max_db })
}

fn parse_db_value(output: &str, label: &str) -> Option<f32> {
    for line in output.lines() {
        let Some((_, value)) = line.split_once(label) else {
            continue;
        };
        let value = value.trim().split_whitespace().next()?;
        if value == "-inf" {
            return Some(f32::NEG_INFINITY);
        }
        if let Ok(parsed) = value.parse::<f32>() {
            return Some(parsed);
        }
    }
    None
}

pub(crate) fn clean_transcript(raw: &str) -> String {
    raw.lines()
        .filter_map(clean_transcript_line)
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

fn clean_transcript_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    let without_timestamp = strip_timestamp_prefix(trimmed);
    let normalized = normalize_for_filter(without_timestamp);
    if normalized.is_empty() {
        return None;
    }
    if is_subtitle_marker(&normalized) || is_hallucination_phrase(&normalized) {
        return None;
    }
    if is_repeated_noise_transcript(without_timestamp) {
        return None;
    }

    Some(without_timestamp.trim().to_string())
}

fn strip_timestamp_prefix(line: &str) -> &str {
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed.strip_prefix('[')
        && let Some((_, after)) = rest.split_once(']')
    {
        return after.trim_start_matches([' ', '-', ':']);
    }
    trimmed
}

fn normalize_for_filter(text: &str) -> String {
    text.chars()
        .filter_map(|ch| {
            if ch.is_whitespace()
                || matches!(
                    ch,
                    '[' | ']'
                        | '('
                        | ')'
                        | '{'
                        | '}'
                        | '<'
                        | '>'
                        | '【'
                        | '】'
                        | '「'
                        | '」'
                        | '"'
                        | '\''
                        | '.'
                        | ','
                        | '!'
                        | '?'
                        | ':'
                        | ';'
                        | '-'
                        | '_'
                        | '~'
                        | '…'
                        | '·'
                )
            {
                None
            } else {
                Some(ch)
            }
        })
        .collect()
}

fn is_subtitle_marker(normalized: &str) -> bool {
    const MARKERS: &[&str] = &[
        "끄덕",
        "박수",
        "웃음",
        "기침",
        "한숨",
        "침묵",
        "음악",
        "노래",
        "환호",
        "박수소리",
        "웃음소리",
        "음악소리",
        "기침소리",
    ];
    MARKERS.contains(&normalized)
}

fn is_hallucination_phrase(normalized: &str) -> bool {
    const EXACT_OR_SHORT_PATTERNS: &[&str] = &[
        "구독",
        "좋아요",
        "구독좋아요",
        "좋아요구독",
        "구독과좋아요",
        "좋아요와구독",
        "MBC뉴스",
        "MBCNEWS",
        "KBS뉴스",
        "SBS뉴스",
        "자막제공",
        "한국어자막",
    ];
    if EXACT_OR_SHORT_PATTERNS.contains(&normalized) {
        return true;
    }

    const SUBSTRING_PATTERNS: &[&str] = &[
        "구독좋아요",
        "구독과좋아요",
        "좋아요와구독",
        "시청해주셔서감사합니다",
        "시청해주셔서고맙습니다",
        "다음영상에서만나요",
        "자막제공",
        "자막을켜고시청",
        "광고를포함",
        "MBC뉴스",
    ];
    SUBSTRING_PATTERNS
        .iter()
        .any(|pattern| normalized.contains(pattern))
}

pub(crate) fn is_repeated_noise_transcript(text: &str) -> bool {
    let compact = normalize_for_filter(text);
    let char_count = compact.chars().count();
    if char_count <= 1 {
        return true;
    }

    if compact.chars().all(|ch| {
        matches!(
            ch,
            'ㅋ' | 'ㅎ' | 'ㅠ' | 'ㅜ' | 'ㅡ' | 'ㅏ' | 'ㅓ' | 'ㅗ' | 'ㅣ'
        )
    }) {
        return true;
    }

    let mut chars = compact.chars();
    if char_count >= 3
        && let Some(first) = chars.next()
        && chars.all(|ch| ch == first)
    {
        return true;
    }

    let tokens = text
        .split_whitespace()
        .map(normalize_for_filter)
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    if tokens.len() >= 3 && tokens.iter().all(|token| token == &tokens[0]) {
        return true;
    }

    const SHORT_AD_PHRASES: &[&str] = &[
        "구독좋아요",
        "구독과좋아요",
        "좋아요구독",
        "MBC뉴스",
        "자막제공",
        "한국어자막",
    ];
    char_count <= 20
        && SHORT_AD_PHRASES
            .iter()
            .any(|phrase| compact.contains(phrase))
}

fn subprocess_runner(timeout: Duration) -> SttCommandRunner {
    Arc::new(move |invocation| {
        Box::pin(async move {
            let mut command = Command::new(&invocation.program);
            command.args(&invocation.args);
            command.kill_on_drop(true);

            let output = tokio::time::timeout(timeout, command.output())
                .await
                .with_context(|| {
                    format!(
                        "voice STT {:?} command timed out after {}s: {}",
                        invocation.kind,
                        timeout.as_secs(),
                        invocation.program
                    )
                })?
                .with_context(|| {
                    format!(
                        "spawn voice STT {:?} command {}",
                        invocation.kind, invocation.program
                    )
                })?;

            if !output.status.success() {
                bail!(
                    "voice STT {:?} command exited with status {}; stderr: {}; stdout: {}",
                    invocation.kind,
                    output.status,
                    preview_output(&output.stderr),
                    preview_output(&output.stdout)
                );
            }

            Ok(SttCommandOutput {
                stdout: output.stdout,
                stderr: output.stderr,
            })
        })
    })
}

async fn cleanup_temp_file(path: &Path) {
    match fs::remove_file(path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            warn!(
                path = %path.display(),
                %error,
                "failed to remove voice STT temp file"
            );
        }
    }
}

fn preview_output(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "<empty>".to_string();
    }
    trimmed.chars().take(2048).collect()
}

fn expand_tilde(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if raw == "~" {
        return dirs::home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_config(temp_dir: PathBuf) -> SttConfig {
        SttConfig {
            ffmpeg_command: "ffmpeg".to_string(),
            whisper_command: "whisper-cli".to_string(),
            model_path: PathBuf::from("/models/ko.bin"),
            language: "ko".to_string(),
            temp_dir,
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn clean_transcript_filters_korean_hallucination_lines() {
        for phrase in [
            "구독",
            "좋아요",
            "MBC뉴스",
            "자막 제공 및 광고를 포함하고 있습니다.",
            "(웃음)",
            "[박수]",
        ] {
            assert_eq!(clean_transcript(phrase), "");
        }

        assert_eq!(
            clean_transcript("이 내용도 반영해줘\n구독과 좋아요"),
            "이 내용도 반영해줘"
        );
    }

    #[test]
    fn repeated_noise_heuristic_filters_short_noise() {
        assert!(is_repeated_noise_transcript("ㅋ"));
        assert!(is_repeated_noise_transcript("ㅋㅋㅋㅋㅋㅋ"));
        assert!(is_repeated_noise_transcript("아아아아"));
        assert!(is_repeated_noise_transcript("음 음 음"));
        assert!(is_repeated_noise_transcript("구독 좋아요"));
        assert!(!is_repeated_noise_transcript("이 내용도 반영해줘"));
    }

    #[test]
    fn parses_ffmpeg_volumedetect_output() {
        let levels = parse_volume_levels(
            "[Parsed_volumedetect_0] mean_volume: -41.2 dB\n[Parsed_volumedetect_0] max_volume: -18.0 dB",
        )
        .unwrap();

        assert_eq!(levels.mean_db, -41.2);
        assert_eq!(levels.max_db, -18.0);
    }

    #[tokio::test]
    async fn low_volume_utterance_skips_whisper() {
        let temp = tempfile::tempdir().unwrap();
        let invocations = Arc::new(Mutex::new(Vec::new()));
        let seen = invocations.clone();
        let runner: SttCommandRunner = Arc::new(move |invocation| {
            let seen = seen.clone();
            Box::pin(async move {
                seen.lock().unwrap().push(invocation.kind);
                Ok(SttCommandOutput {
                    stderr: b"mean_volume: -41.2 dB\nmax_volume: -18.0 dB".to_vec(),
                    stdout: Vec::new(),
                })
            })
        });
        let runtime = SttRuntime::with_runner(test_config(temp.path().to_path_buf()), runner);

        let transcript = runtime
            .transcribe(temp.path().join("quiet.wav"))
            .await
            .unwrap();

        assert_eq!(transcript, "");
        assert_eq!(
            *invocations.lock().unwrap(),
            vec![SttCommandKind::VolumeDetect]
        );
    }

    #[tokio::test]
    async fn empty_cleaned_transcript_retries_once() {
        let temp = tempfile::tempdir().unwrap();
        let invocations = Arc::new(Mutex::new(Vec::<SttCommandInvocation>::new()));
        let whisper_calls = Arc::new(AtomicUsize::new(0));
        let seen = invocations.clone();
        let calls = whisper_calls.clone();
        let runner: SttCommandRunner = Arc::new(move |invocation| {
            let seen = seen.clone();
            let calls = calls.clone();
            Box::pin(async move {
                seen.lock().unwrap().push(invocation.clone());
                match invocation.kind {
                    SttCommandKind::VolumeDetect => Ok(SttCommandOutput {
                        stderr: b"mean_volume: -22.0 dB\nmax_volume: -4.0 dB".to_vec(),
                        stdout: Vec::new(),
                    }),
                    SttCommandKind::Convert => {
                        fs::write(invocation.output_path.as_ref().unwrap(), b"wav").await?;
                        Ok(SttCommandOutput::default())
                    }
                    SttCommandKind::Whisper => {
                        let call = calls.fetch_add(1, Ordering::SeqCst);
                        let text = if call == 0 {
                            "구독 좋아요"
                        } else {
                            "이 내용도 반영해줘"
                        };
                        fs::write(invocation.transcript_path.as_ref().unwrap(), text).await?;
                        Ok(SttCommandOutput::default())
                    }
                }
            })
        });
        let runtime = SttRuntime::with_runner(test_config(temp.path().to_path_buf()), runner);

        let transcript = runtime
            .transcribe(temp.path().join("speech.wav"))
            .await
            .unwrap();

        assert_eq!(transcript, "이 내용도 반영해줘");
        assert_eq!(whisper_calls.load(Ordering::SeqCst), 2);
        let invocations = invocations.lock().unwrap();
        let whisper = invocations
            .iter()
            .find(|invocation| invocation.kind == SttCommandKind::Whisper)
            .unwrap();
        assert!(
            whisper
                .args
                .windows(2)
                .any(|pair| pair == ["-m", "/models/ko.bin"])
        );
        assert!(whisper.args.windows(2).any(|pair| pair == ["-l", "ko"]));
        assert!(whisper.args.windows(2).any(|pair| pair == ["-nth", "0.35"]));
        assert!(whisper.args.windows(2).any(|pair| pair == ["-et", "2.2"]));
        assert!(whisper.args.windows(2).any(|pair| pair == ["-lpt", "-0.8"]));
        assert!(whisper.args.iter().any(|arg| arg == "-nt"));
        assert!(whisper.args.iter().any(|arg| arg == "-otxt"));
        assert!(whisper.args.iter().any(|arg| arg == "-sns"));
        assert!(whisper.args.iter().any(|arg| arg == "-nf"));
    }
}
