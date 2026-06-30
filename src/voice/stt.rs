use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures::future::BoxFuture;
use hound::{SampleFormat, WavSpec, WavWriter};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use super::VoiceConfig;
use super::config::VoiceSttMode;
use super::stt_streaming::{
    StreamingDecodeWindow, StreamingDecodeWindowMeta, StreamingOverlapConfig,
    WHISPER_STREAM_SAMPLE_RATE_HZ, WhisperStreamOverlapSegmenter,
};
use super::utils::expand_tilde;

// === Tunables (was scattered constants) ===
// Pipeline timing
const STT_TIMEOUT: Duration = Duration::from_secs(120);
const EMPTY_RETRY_DELAY: Duration = Duration::from_millis(300);

// Volume gating (ffmpeg `volumedetect` thresholds, in dBFS).
// An utterance is treated as silence/noise (and skipped before whisper) only
// when BOTH its mean volume is below the configured mean floor
// (`SttConfig::speech_start_db`, sourced from `voice.thresholds.speech_start_db`)
// AND its peak volume is below `LOW_VOLUME_MAX_DB`.
const LOW_VOLUME_MAX_DB: f32 = -12.0;

// whisper-cli decoding thresholds passed via CLI flags.
// Kept as &str because whisper-cli consumes them as command-line args verbatim.
/// `-nth` no-speech-threshold: token probability below which a chunk is considered silence.
const WHISPER_NO_SPEECH_THRESHOLD: &str = "0.35";
/// `-et` entropy threshold: decoder fallback trigger when output entropy exceeds this.
const WHISPER_ENTROPY_THRESHOLD: &str = "2.2";
/// `-lpt` log-probability threshold: decoder fallback trigger when avg logprob falls below this.
const WHISPER_LOGPROB_THRESHOLD: &str = "-0.8";

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SttConfig {
    pub(crate) ffmpeg_command: String,
    pub(crate) whisper_command: String,
    pub(crate) model_path: PathBuf,
    pub(crate) language: String,
    pub(crate) temp_dir: PathBuf,
    pub(crate) timeout: Duration,
    /// Mean-volume floor (dBFS) for the low-volume silence gate, sourced from
    /// `voice.thresholds.speech_start_db` (#3912). Utterances whose mean volume
    /// is below this (and whose peak is below `LOW_VOLUME_MAX_DB`) are skipped.
    pub(crate) speech_start_db: f32,
    pub(crate) stream_overlap: StreamingOverlapConfig,
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
            speech_start_db: config.thresholds.speech_start_db,
            stream_overlap: StreamingOverlapConfig {
                sample_rate_hz: WHISPER_STREAM_SAMPLE_RATE_HZ,
                step_ms: config.stt.stream.step_ms,
                length_ms: config.stt.stream.length_ms,
                keep_ms: config.stt.stream.keep_ms,
            },
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct SttSessionHandle {
    id: uuid::Uuid,
}

impl SttSessionHandle {
    pub(crate) fn new() -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PartialTranscript {
    pub(crate) session: SttSessionHandle,
    pub(crate) text: String,
    pub(crate) window: Option<StreamingDecodeWindowMeta>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FinalTranscript {
    pub(crate) session: SttSessionHandle,
    pub(crate) text: String,
}

#[async_trait]
pub(crate) trait VoiceStt: Send + Sync {
    async fn start_session(&self, language: &str) -> Result<SttSessionHandle>;
    async fn feed(&self, session: &SttSessionHandle, pcm: &[f32]) -> Result<()>;
    async fn poll_partial(&self, session: &SttSessionHandle) -> Result<Option<PartialTranscript>>;
    async fn finalize(&self, session: SttSessionHandle) -> Result<FinalTranscript>;
    async fn transcribe_file(&self, wav_path: &Path) -> Result<String>;
}

#[derive(Clone)]
pub(crate) struct SttRuntime {
    config: SttConfig,
    runner: SttCommandRunner,
}

impl SttRuntime {
    // reason: voice runtime is wired only when voice config is enabled; no
    // compile target exercises it. See #3034.
    #[allow(dead_code)]
    pub(crate) fn from_voice_config(config: &VoiceConfig) -> Self {
        Self::new(SttConfig::from_voice_config(config))
    }

    // reason: voice runtime is wired only when voice config is enabled; no
    // compile target exercises it. See #3034.
    #[allow(dead_code)]
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
        Ok(levels.mean_db < self.config.speech_start_db && levels.max_db < LOW_VOLUME_MAX_DB)
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
                WHISPER_NO_SPEECH_THRESHOLD.to_string(),
                "-et".to_string(),
                WHISPER_ENTROPY_THRESHOLD.to_string(),
                "-lpt".to_string(),
                WHISPER_LOGPROB_THRESHOLD.to_string(),
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

#[async_trait]
impl VoiceStt for SttRuntime {
    async fn start_session(&self, _language: &str) -> Result<SttSessionHandle> {
        Ok(SttSessionHandle::new())
    }

    async fn feed(&self, _session: &SttSessionHandle, _pcm: &[f32]) -> Result<()> {
        Ok(())
    }

    async fn poll_partial(&self, _session: &SttSessionHandle) -> Result<Option<PartialTranscript>> {
        Ok(None)
    }

    async fn finalize(&self, session: SttSessionHandle) -> Result<FinalTranscript> {
        Ok(FinalTranscript {
            session,
            text: String::new(),
        })
    }

    async fn transcribe_file(&self, wav_path: &Path) -> Result<String> {
        self.transcribe(wav_path).await
    }
}

#[derive(Clone)]
pub(crate) enum VoiceSttRuntime {
    File(SttRuntime),
    Stream {
        stream: WhisperStream,
        fallback: SttRuntime,
    },
}

impl VoiceSttRuntime {
    pub(crate) fn from_voice_config(config: &VoiceConfig) -> Self {
        Self::with_runner(
            config.stt.mode,
            SttConfig::from_voice_config(config),
            subprocess_runner(STT_TIMEOUT),
        )
    }

    pub(crate) fn with_runner(
        mode: VoiceSttMode,
        config: SttConfig,
        runner: SttCommandRunner,
    ) -> Self {
        let fallback = SttRuntime::with_runner(config.clone(), runner.clone());
        match mode {
            VoiceSttMode::File => Self::File(fallback),
            VoiceSttMode::Stream => Self::Stream {
                stream: WhisperStream::with_runner(config, runner),
                fallback,
            },
        }
    }

    pub(crate) fn is_streaming(&self) -> bool {
        matches!(self, Self::Stream { .. })
    }

    /// #3910: drop the inner streaming session for `session` without running a
    /// final decode. Used when a speaker leaves the voice channel mid-utterance
    /// so the underlying `WhisperStream` session is not stranded in the inner
    /// map until the runtime is rebuilt/dropped. File mode keeps no inner
    /// session, so this is a no-op there.
    pub(crate) async fn discard_stream_session(&self, session: &SttSessionHandle) {
        if let Self::Stream { stream, .. } = self {
            stream.discard_session(session).await;
        }
    }
}

#[async_trait]
impl VoiceStt for VoiceSttRuntime {
    async fn start_session(&self, language: &str) -> Result<SttSessionHandle> {
        match self {
            Self::File(runtime) => runtime.start_session(language).await,
            Self::Stream { stream, .. } => stream.start_session(language).await,
        }
    }

    async fn feed(&self, session: &SttSessionHandle, pcm: &[f32]) -> Result<()> {
        match self {
            Self::File(runtime) => runtime.feed(session, pcm).await,
            Self::Stream { stream, .. } => stream.feed(session, pcm).await,
        }
    }

    async fn poll_partial(&self, session: &SttSessionHandle) -> Result<Option<PartialTranscript>> {
        match self {
            Self::File(runtime) => runtime.poll_partial(session).await,
            Self::Stream { stream, .. } => stream.poll_partial(session).await,
        }
    }

    async fn finalize(&self, session: SttSessionHandle) -> Result<FinalTranscript> {
        match self {
            Self::File(runtime) => runtime.finalize(session).await,
            Self::Stream { stream, .. } => stream.finalize(session).await,
        }
    }

    async fn transcribe_file(&self, wav_path: &Path) -> Result<String> {
        match self {
            Self::File(runtime) => runtime.transcribe_file(wav_path).await,
            Self::Stream { fallback, .. } => fallback.transcribe_file(wav_path).await,
        }
    }
}

#[derive(Clone)]
pub(crate) struct WhisperStream {
    config: SttConfig,
    runner: SttCommandRunner,
    sessions: Arc<Mutex<HashMap<SttSessionHandle, Arc<Mutex<WhisperStreamSession>>>>>,
}

struct WhisperStreamSession {
    language: String,
    segmenter: WhisperStreamOverlapSegmenter,
    last_partial: Option<PartialTranscript>,
    last_partial_taken: bool,
}

impl WhisperStream {
    pub(crate) fn with_runner(config: SttConfig, runner: SttCommandRunner) -> Self {
        Self {
            config,
            runner,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn session(
        &self,
        session: &SttSessionHandle,
    ) -> Result<Arc<Mutex<WhisperStreamSession>>> {
        self.sessions
            .lock()
            .await
            .get(session)
            .cloned()
            .with_context(|| format!("unknown voice STT stream session {}", session.id))
    }

    async fn decode_window(
        &self,
        session: &SttSessionHandle,
        language: &str,
        window: &StreamingDecodeWindow,
    ) -> Result<String> {
        fs::create_dir_all(&self.config.temp_dir)
            .await
            .with_context(|| {
                format!(
                    "create streaming STT temp dir {}",
                    self.config.temp_dir.display()
                )
            })?;

        let temp_id = format!(
            "agentdesk-stt-stream-{}-{}-{}",
            std::process::id(),
            session.id,
            window.meta.sequence
        );
        let wav_path = self.config.temp_dir.join(format!("{temp_id}.wav"));
        let transcript_prefix = self.config.temp_dir.join(format!("{temp_id}-transcript"));
        let transcript_path = transcript_prefix.with_extension("txt");

        let result = async {
            write_stream_window_wav(&wav_path, &window.samples, window.meta.sample_rate_hz).await?;
            let invocation = SttCommandInvocation {
                kind: SttCommandKind::Whisper,
                program: self.config.whisper_command.clone(),
                args: vec![
                    "-m".to_string(),
                    self.config.model_path.to_string_lossy().to_string(),
                    "-f".to_string(),
                    wav_path.to_string_lossy().to_string(),
                    "-l".to_string(),
                    language.to_string(),
                    "-nt".to_string(),
                    "-otxt".to_string(),
                    "-sns".to_string(),
                    "-nf".to_string(),
                    "-nth".to_string(),
                    WHISPER_NO_SPEECH_THRESHOLD.to_string(),
                    "-et".to_string(),
                    WHISPER_ENTROPY_THRESHOLD.to_string(),
                    "-lpt".to_string(),
                    WHISPER_LOGPROB_THRESHOLD.to_string(),
                    "-of".to_string(),
                    transcript_prefix.to_string_lossy().to_string(),
                ],
                output_path: None,
                transcript_path: Some(transcript_path.clone()),
            };
            let output = (self.runner)(invocation).await.with_context(|| {
                format!(
                    "run whisper-cli for streaming STT window {} session {}",
                    window.meta.sequence, session.id
                )
            })?;
            let raw = read_whisper_text(&transcript_path, &output).await?;
            Ok(clean_transcript(&raw))
        }
        .await;

        cleanup_temp_file(&wav_path).await;
        cleanup_temp_file(&transcript_path).await;
        result
    }

    /// #3910: forget a session's inner state without finalizing/decoding it.
    /// Removing the entry drops the `Arc<Mutex<WhisperStreamSession>>`; combined
    /// with aborting the per-tick feed task, the session is freed on channel
    /// leave instead of lingering until `finalize()` (which never runs when the
    /// speaker simply leaves the channel).
    pub(crate) async fn discard_session(&self, session: &SttSessionHandle) {
        self.sessions.lock().await.remove(session);
    }
}

#[async_trait]
impl VoiceStt for WhisperStream {
    async fn start_session(&self, language: &str) -> Result<SttSessionHandle> {
        let handle = SttSessionHandle::new();
        let segmenter = WhisperStreamOverlapSegmenter::new(self.config.stream_overlap)?;
        self.sessions.lock().await.insert(
            handle.clone(),
            Arc::new(Mutex::new(WhisperStreamSession {
                language: language.to_string(),
                segmenter,
                last_partial: None,
                last_partial_taken: false,
            })),
        );
        Ok(handle)
    }

    async fn feed(&self, session: &SttSessionHandle, pcm: &[f32]) -> Result<()> {
        if pcm.is_empty() {
            return Ok(());
        }

        let session_state = self.session(session).await?;
        let mut state = session_state.lock().await;
        let windows = state.segmenter.feed(pcm);
        let language = state.language.clone();
        for window in windows {
            let text = self.decode_window(session, &language, &window).await?;
            state.last_partial = Some(PartialTranscript {
                session: session.clone(),
                text,
                window: Some(window.meta),
            });
            state.last_partial_taken = false;
        }
        Ok(())
    }

    async fn poll_partial(&self, session: &SttSessionHandle) -> Result<Option<PartialTranscript>> {
        let session_state = self.session(session).await?;
        let mut state = session_state.lock().await;
        if state.last_partial_taken {
            return Ok(None);
        }
        state.last_partial_taken = true;
        Ok(state.last_partial.clone())
    }

    async fn finalize(&self, session: SttSessionHandle) -> Result<FinalTranscript> {
        let session_state = self
            .sessions
            .lock()
            .await
            .remove(&session)
            .with_context(|| format!("unknown voice STT stream session {}", session.id))?;
        let mut state = session_state.lock().await;
        let language = state.language.clone();
        let mut text = state
            .last_partial
            .as_ref()
            .map(|partial| partial.text.clone())
            .unwrap_or_default();
        if let Some(window) = state.segmenter.finish() {
            text = self.decode_window(&session, &language, &window).await?;
        }
        Ok(FinalTranscript { session, text })
    }

    async fn transcribe_file(&self, wav_path: &Path) -> Result<String> {
        SttRuntime::with_runner(self.config.clone(), self.runner.clone())
            .transcribe(wav_path)
            .await
    }
}

// reason: voice runtime is wired only when voice config is enabled; no compile
// target exercises it. See #3034.
#[allow(dead_code)]
pub(crate) async fn transcribe(wav_path: impl AsRef<Path>) -> Result<String> {
    let config = VoiceConfig::default();
    transcribe_with_config(wav_path, &config).await
}

// reason: voice runtime is wired only when voice config is enabled; no compile
// target exercises it. See #3034.
#[allow(dead_code)]
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

async fn write_stream_window_wav(path: &Path, samples: &[f32], sample_rate_hz: u32) -> Result<()> {
    let path = path.to_path_buf();
    let samples = samples.to_vec();
    tokio::task::spawn_blocking(move || {
        let spec = WavSpec {
            channels: 1,
            sample_rate: sample_rate_hz,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut writer = WavWriter::create(&path, spec)
            .with_context(|| format!("create streaming STT window WAV {}", path.display()))?;
        for sample in samples {
            let sample = sample.clamp(-1.0, 1.0);
            writer
                .write_sample((sample * i16::MAX as f32) as i16)
                .with_context(|| format!("write streaming STT window WAV {}", path.display()))?;
        }
        writer
            .finalize()
            .with_context(|| format!("finalize streaming STT window WAV {}", path.display()))?;
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("streaming STT WAV writer task failed")??;
    Ok(())
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
            speech_start_db: -35.0,
            stream_overlap: StreamingOverlapConfig {
                sample_rate_hz: 1_000,
                step_ms: 4,
                length_ms: 8,
                keep_ms: 2,
            },
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

    /// Regression guard for #3912: `voice.thresholds.speech_start_db` must
    /// actually reach the low-volume gate. With the same audio levels, only the
    /// configured threshold differs, and the gate decision must flip — proving
    /// the config value is live rather than a no-op.
    #[tokio::test]
    async fn speech_start_db_threshold_is_wired_into_low_volume_gate() {
        let temp = tempfile::tempdir().unwrap();
        let wav = temp.path().join("clip.wav");
        // ffmpeg volumedetect reports mean -38 dB, peak -20 dB (peak is below the
        // -12 dB max floor, so the silence decision hinges on the mean threshold).
        let runner: SttCommandRunner = Arc::new(move |_invocation| {
            Box::pin(async move {
                Ok(SttCommandOutput {
                    stderr: b"mean_volume: -38.0 dB\nmax_volume: -20.0 dB".to_vec(),
                    stdout: Vec::new(),
                })
            })
        });

        // Strict floor (-45): mean -38 is *above* -45 -> treated as speech.
        let mut strict = test_config(temp.path().to_path_buf());
        strict.speech_start_db = -45.0;
        let strict_runtime = SttRuntime::with_runner(strict, runner.clone());
        assert!(
            !strict_runtime.is_low_volume_utterance(&wav).await.unwrap(),
            "mean -38 dB must NOT be gated as silence when speech_start_db is -45"
        );

        // Permissive floor (-35): mean -38 is *below* -35 -> treated as silence.
        let mut permissive = test_config(temp.path().to_path_buf());
        permissive.speech_start_db = -35.0;
        let permissive_runtime = SttRuntime::with_runner(permissive, runner);
        assert!(
            permissive_runtime
                .is_low_volume_utterance(&wav)
                .await
                .unwrap(),
            "mean -38 dB MUST be gated as silence when speech_start_db is -35"
        );
    }

    /// #3912: the config default must equal the effective gate default so that
    /// the documented `voice.thresholds.speech_start_db` matches real behavior.
    #[test]
    fn speech_start_db_default_matches_effective_low_volume_gate() {
        assert_eq!(VoiceConfig::default().thresholds.speech_start_db, -35.0);
        assert_eq!(
            SttConfig::from_voice_config(&VoiceConfig::default()).speech_start_db,
            -35.0,
            "config default must reach the gate unchanged (config-default == gate-default)"
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
        assert!(
            whisper
                .args
                .windows(2)
                .any(|pair| pair == ["-nth", WHISPER_NO_SPEECH_THRESHOLD])
        );
        assert!(
            whisper
                .args
                .windows(2)
                .any(|pair| pair == ["-et", WHISPER_ENTROPY_THRESHOLD])
        );
        assert!(
            whisper
                .args
                .windows(2)
                .any(|pair| pair == ["-lpt", WHISPER_LOGPROB_THRESHOLD])
        );
        assert!(whisper.args.iter().any(|arg| arg == "-nt"));
        assert!(whisper.args.iter().any(|arg| arg == "-otxt"));
        assert!(whisper.args.iter().any(|arg| arg == "-sns"));
        assert!(whisper.args.iter().any(|arg| arg == "-nf"));
    }

    #[tokio::test]
    async fn whisper_stream_decodes_overlap_windows_and_finalizes_tail() {
        let temp = tempfile::tempdir().unwrap();
        let invocations = Arc::new(Mutex::new(Vec::<SttCommandInvocation>::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = invocations.clone();
        let call_counter = calls.clone();
        let runner: SttCommandRunner = Arc::new(move |invocation| {
            let seen = seen.clone();
            let call_counter = call_counter.clone();
            Box::pin(async move {
                seen.lock().unwrap().push(invocation.clone());
                assert_eq!(invocation.kind, SttCommandKind::Whisper);
                assert!(invocation.args.windows(2).any(|pair| pair == ["-l", "en"]));
                assert!(
                    invocation
                        .args
                        .windows(2)
                        .any(|pair| pair == ["-m", "/models/ko.bin"])
                );
                let call = call_counter.fetch_add(1, Ordering::SeqCst);
                let text = if call == 0 {
                    "partial window"
                } else {
                    "final tail"
                };
                fs::write(invocation.transcript_path.as_ref().unwrap(), text).await?;
                Ok(SttCommandOutput::default())
            })
        });
        let stream = WhisperStream::with_runner(test_config(temp.path().to_path_buf()), runner);
        let session = stream.start_session("en").await.unwrap();

        stream.feed(&session, &[0.1, 0.2, 0.3]).await.unwrap();
        assert!(stream.poll_partial(&session).await.unwrap().is_none());

        stream.feed(&session, &[0.4]).await.unwrap();
        let partial = stream.poll_partial(&session).await.unwrap().unwrap();
        assert_eq!(partial.session, session);
        assert_eq!(partial.text, "partial window");
        assert_eq!(partial.window.unwrap().sequence, 0);
        assert!(stream.poll_partial(&session).await.unwrap().is_none());

        stream.feed(&session, &[0.5, 0.6]).await.unwrap();
        let final_transcript = stream.finalize(session.clone()).await.unwrap();

        assert_eq!(final_transcript.session, session);
        assert_eq!(final_transcript.text, "final tail");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let invocations = invocations.lock().unwrap();
        assert_eq!(invocations.len(), 2);
        for invocation in invocations.iter() {
            assert!(
                invocation.args.iter().any(|arg| arg == "-otxt"),
                "streaming windows must use whisper text output for clean parsing"
            );
        }
    }

    #[test]
    fn voice_stt_runtime_selects_file_by_default_and_stream_when_configured() {
        let temp = tempfile::tempdir().unwrap();
        let runner: SttCommandRunner =
            Arc::new(|_| Box::pin(async { Ok(SttCommandOutput::default()) }));

        let file = VoiceSttRuntime::with_runner(
            VoiceSttMode::File,
            test_config(temp.path().to_path_buf()),
            runner.clone(),
        );
        let stream = VoiceSttRuntime::with_runner(
            VoiceSttMode::Stream,
            test_config(temp.path().to_path_buf()),
            runner,
        );

        assert!(!file.is_streaming());
        assert!(stream.is_streaming());
    }
}
