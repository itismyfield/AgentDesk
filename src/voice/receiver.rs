use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use hound::{SampleFormat, WavSpec, WavWriter};
use songbird::{Event, EventContext, EventHandler};
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;

use super::VoiceConfig;

const WAV_CHANNELS: u16 = 2;
const WAV_SAMPLE_RATE: u32 = 48_000;
const WAV_BITS_PER_SAMPLE: u16 = 16;

type WavFileWriter = WavWriter<std::io::BufWriter<std::fs::File>>;

#[derive(Debug, Clone)]
pub(crate) struct CompletedUtterance {
    pub(crate) user_id: u64,
    pub(crate) utterance_id: String,
    pub(crate) path: PathBuf,
    pub(crate) segment_paths: Vec<PathBuf>,
    pub(crate) samples_written: usize,
    pub(crate) started_at: String,
    pub(crate) completed_at: String,
}

#[derive(Debug, Clone)]
pub(crate) struct VoiceReceiverConfig {
    pub(crate) recordings_dir: PathBuf,
    pub(crate) segment_idle: Duration,
    pub(crate) utterance_idle: Duration,
    pub(crate) allowed_user_ids: HashSet<u64>,
}

impl VoiceReceiverConfig {
    pub(crate) fn from_voice_config(config: &VoiceConfig) -> Self {
        let recordings_dir = std::env::var_os("VOICE_AUDIO_DEBUG_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| expand_tilde(&config.audio.recordings_dir));
        let allowed_user_ids = config
            .allowed_user_ids
            .iter()
            .filter_map(|value| value.trim().parse::<u64>().ok())
            .collect();

        Self {
            recordings_dir,
            segment_idle: Duration::from_millis(config.idle.segment_idle_ms),
            utterance_idle: Duration::from_millis(config.idle.utterance_idle_ms),
            allowed_user_ids,
        }
    }
}

impl Default for VoiceReceiverConfig {
    fn default() -> Self {
        Self::from_voice_config(&VoiceConfig::default())
    }
}

#[derive(Debug, Error)]
pub(crate) enum VoiceReceiverError {
    #[error("unknown voice SSRC {0}")]
    UnknownSsrc(u32),
    #[error("failed to create voice recording directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to write WAV {path}: {source}")]
    Wav { path: PathBuf, source: hound::Error },
}

#[derive(Clone)]
pub(crate) struct VoiceReceiver {
    inner: Arc<ReceiverState>,
}

impl VoiceReceiver {
    pub(crate) fn new(config: VoiceReceiverConfig) -> Self {
        Self {
            inner: Arc::new(ReceiverState::new(config)),
        }
    }

    pub(crate) fn from_voice_config(config: &VoiceConfig) -> Self {
        Self::new(VoiceReceiverConfig::from_voice_config(config))
    }

    pub(crate) async fn register_speaking(&self, ssrc: u32, user_id: u64) {
        self.inner.register_speaking(ssrc, user_id).await;
    }

    pub(crate) async fn queue_pcm(
        &self,
        ssrc: u32,
        samples: &[i16],
    ) -> Result<bool, VoiceReceiverError> {
        self.inner.queue_pcm(ssrc, samples).await
    }

    pub(crate) async fn flush_all(&self) -> Vec<CompletedUtterance> {
        self.inner.flush_all().await
    }

    pub(crate) async fn take_pending(&self) -> Vec<CompletedUtterance> {
        self.inner.take_pending().await
    }
}

#[async_trait]
impl EventHandler for VoiceReceiver {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        match ctx {
            EventContext::SpeakingStateUpdate(update) => {
                if let Some(user_id) = update.user_id {
                    let user_id = user_id.0;
                    if user_id != 0 {
                        self.register_speaking(update.ssrc, user_id).await;
                    }
                }
            }
            EventContext::VoiceTick(tick) => {
                for (ssrc, voice) in &tick.speaking {
                    let Some(samples) = voice.decoded_voice.as_deref() else {
                        continue;
                    };
                    if samples.is_empty() {
                        continue;
                    }
                    if let Err(error) = self.queue_pcm(*ssrc, samples).await {
                        if !matches!(error, VoiceReceiverError::UnknownSsrc(_)) {
                            tracing::warn!(error = %error, ssrc, "failed to queue voice PCM");
                        }
                    }
                }
            }
            _ => {}
        }

        None
    }
}

struct ReceiverState {
    config: VoiceReceiverConfig,
    ssrc_users: RwLock<HashMap<u32, u64>>,
    users: Mutex<HashMap<u64, UserAudioState>>,
    pending: Mutex<Vec<CompletedUtterance>>,
    sequence: AtomicU64,
}

impl ReceiverState {
    fn new(config: VoiceReceiverConfig) -> Self {
        Self {
            config,
            ssrc_users: RwLock::new(HashMap::new()),
            users: Mutex::new(HashMap::new()),
            pending: Mutex::new(Vec::new()),
            sequence: AtomicU64::new(1),
        }
    }

    async fn register_speaking(&self, ssrc: u32, user_id: u64) {
        self.ssrc_users.write().await.insert(ssrc, user_id);
    }

    async fn queue_pcm(
        self: &Arc<Self>,
        ssrc: u32,
        samples: &[i16],
    ) -> Result<bool, VoiceReceiverError> {
        let Some(user_id) = self.ssrc_users.read().await.get(&ssrc).copied() else {
            return Err(VoiceReceiverError::UnknownSsrc(ssrc));
        };
        if !self.user_allowed(user_id) {
            return Ok(false);
        }

        let mut users = self.users.lock().await;
        let user_state = users.entry(user_id).or_default();
        if user_state.active.is_none() {
            user_state.active = Some(self.create_active_utterance(user_id)?);
        }

        let active = user_state.active.as_mut().expect("active utterance exists");
        active.ensure_segment_writer()?;
        active.write_samples(samples)?;
        let utterance_id = active.utterance_id.clone();
        self.arm_timers(user_id, utterance_id, user_state);
        Ok(true)
    }

    async fn finish_segment(
        self: &Arc<Self>,
        user_id: u64,
        utterance_id: &str,
    ) -> Result<(), VoiceReceiverError> {
        let mut users = self.users.lock().await;
        let Some(user_state) = users.get_mut(&user_id) else {
            return Ok(());
        };
        let Some(active) = user_state.active.as_mut() else {
            return Ok(());
        };
        if active.utterance_id != utterance_id {
            return Ok(());
        }
        user_state.segment_timer.take();
        active.finish_segment()
    }

    async fn flush_utterance(
        self: &Arc<Self>,
        user_id: u64,
        utterance_id: &str,
        abort_utterance_timer: bool,
    ) -> Result<Option<CompletedUtterance>, VoiceReceiverError> {
        let active = {
            let mut users = self.users.lock().await;
            let Some(user_state) = users.get_mut(&user_id) else {
                return Ok(None);
            };
            if user_state
                .active
                .as_ref()
                .is_none_or(|active| active.utterance_id != utterance_id)
            {
                return Ok(None);
            }
            abort_timer(user_state.segment_timer.take());
            if abort_utterance_timer {
                abort_timer(user_state.utterance_timer.take());
            } else {
                user_state.utterance_timer.take();
            }
            let active = user_state.active.take();
            users.remove(&user_id);
            active
        };

        let Some(active) = active else {
            return Ok(None);
        };
        let completed = active.finalize()?;
        self.pending.lock().await.push(completed.clone());
        Ok(Some(completed))
    }

    async fn flush_all(self: &Arc<Self>) -> Vec<CompletedUtterance> {
        let active = {
            let mut users = self.users.lock().await;
            users
                .drain()
                .filter_map(|(_, mut user_state)| {
                    abort_timer(user_state.segment_timer.take());
                    abort_timer(user_state.utterance_timer.take());
                    user_state.active.take()
                })
                .collect::<Vec<_>>()
        };
        self.ssrc_users.write().await.clear();

        let mut completed = Vec::new();
        for active in active {
            match active.finalize() {
                Ok(utterance) => completed.push(utterance),
                Err(error) => tracing::warn!(error = %error, "failed to flush voice utterance"),
            }
        }
        if !completed.is_empty() {
            self.pending.lock().await.extend(completed.clone());
        }
        completed
    }

    async fn take_pending(&self) -> Vec<CompletedUtterance> {
        std::mem::take(&mut *self.pending.lock().await)
    }

    fn user_allowed(&self, user_id: u64) -> bool {
        self.config.allowed_user_ids.is_empty() || self.config.allowed_user_ids.contains(&user_id)
    }

    fn create_active_utterance(&self, user_id: u64) -> Result<ActiveUtterance, VoiceReceiverError> {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let started_at = chrono::Local::now();
        let timestamp = started_at.format("%Y%m%d-%H%M%S%.3f").to_string();
        let utterance_id = format!("{timestamp}-{sequence:06}");
        let user_dir = format!("user_{user_id}");
        let utterance_dir = self
            .config
            .recordings_dir
            .join("utterances")
            .join(&user_dir);
        let segment_dir = self.config.recordings_dir.join("segments").join(&user_dir);
        create_dir_all(&utterance_dir)?;
        create_dir_all(&segment_dir)?;

        let utterance_path = utterance_dir.join(format!("{utterance_id}.wav"));
        let utterance_writer = create_wav_writer(&utterance_path)?;

        Ok(ActiveUtterance {
            user_id,
            utterance_id,
            utterance_path,
            utterance_writer,
            segment_dir,
            current_segment_path: None,
            segment_writer: None,
            segment_paths: Vec::new(),
            next_segment_index: 1,
            samples_written: 0,
            started_at: started_at.to_rfc3339(),
        })
    }

    fn arm_timers(
        self: &Arc<Self>,
        user_id: u64,
        utterance_id: String,
        user_state: &mut UserAudioState,
    ) {
        abort_timer(user_state.segment_timer.take());
        abort_timer(user_state.utterance_timer.take());

        let segment_state = self.clone();
        let segment_utterance_id = utterance_id.clone();
        let segment_idle = self.config.segment_idle;
        user_state.segment_timer = Some(tokio::spawn(async move {
            tokio::time::sleep(segment_idle).await;
            if let Err(error) = segment_state
                .finish_segment(user_id, &segment_utterance_id)
                .await
            {
                tracing::warn!(error = %error, user_id, "failed to finish voice segment");
            }
        }));

        let utterance_state = self.clone();
        let utterance_idle = self.config.utterance_idle;
        user_state.utterance_timer = Some(tokio::spawn(async move {
            tokio::time::sleep(utterance_idle).await;
            match utterance_state
                .flush_utterance(user_id, &utterance_id, false)
                .await
            {
                Ok(Some(completed)) => {
                    tracing::info!(
                        user_id = completed.user_id,
                        path = %completed.path.display(),
                        "voice utterance flushed"
                    );
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(error = %error, user_id, "failed to flush voice utterance")
                }
            }
        }));
    }
}

#[derive(Default)]
struct UserAudioState {
    active: Option<ActiveUtterance>,
    segment_timer: Option<JoinHandle<()>>,
    utterance_timer: Option<JoinHandle<()>>,
}

struct ActiveUtterance {
    user_id: u64,
    utterance_id: String,
    utterance_path: PathBuf,
    utterance_writer: WavFileWriter,
    segment_dir: PathBuf,
    current_segment_path: Option<PathBuf>,
    segment_writer: Option<WavFileWriter>,
    segment_paths: Vec<PathBuf>,
    next_segment_index: u32,
    samples_written: usize,
    started_at: String,
}

impl ActiveUtterance {
    fn ensure_segment_writer(&mut self) -> Result<(), VoiceReceiverError> {
        if self.segment_writer.is_some() {
            return Ok(());
        }

        let segment_path = self.segment_dir.join(format!(
            "{}_segment_{:03}.wav",
            self.utterance_id, self.next_segment_index
        ));
        self.next_segment_index += 1;
        let segment_writer = create_wav_writer(&segment_path)?;
        self.current_segment_path = Some(segment_path);
        self.segment_writer = Some(segment_writer);
        Ok(())
    }

    fn write_samples(&mut self, samples: &[i16]) -> Result<(), VoiceReceiverError> {
        for sample in samples {
            self.utterance_writer
                .write_sample(*sample)
                .map_err(|source| VoiceReceiverError::Wav {
                    path: self.utterance_path.clone(),
                    source,
                })?;
            if let Some(writer) = self.segment_writer.as_mut() {
                writer
                    .write_sample(*sample)
                    .map_err(|source| VoiceReceiverError::Wav {
                        path: self
                            .current_segment_path
                            .clone()
                            .unwrap_or_else(|| self.segment_dir.clone()),
                        source,
                    })?;
            }
        }
        self.samples_written += samples.len();
        Ok(())
    }

    fn finish_segment(&mut self) -> Result<(), VoiceReceiverError> {
        let Some(writer) = self.segment_writer.take() else {
            return Ok(());
        };
        let Some(path) = self.current_segment_path.take() else {
            return Ok(());
        };
        writer
            .finalize()
            .map_err(|source| VoiceReceiverError::Wav {
                path: path.clone(),
                source,
            })?;
        self.segment_paths.push(path);
        Ok(())
    }

    fn finalize(mut self) -> Result<CompletedUtterance, VoiceReceiverError> {
        self.finish_segment()?;
        self.utterance_writer
            .finalize()
            .map_err(|source| VoiceReceiverError::Wav {
                path: self.utterance_path.clone(),
                source,
            })?;
        Ok(CompletedUtterance {
            user_id: self.user_id,
            utterance_id: self.utterance_id,
            path: self.utterance_path,
            segment_paths: self.segment_paths,
            samples_written: self.samples_written,
            started_at: self.started_at,
            completed_at: chrono::Local::now().to_rfc3339(),
        })
    }
}

fn abort_timer(timer: Option<JoinHandle<()>>) {
    if let Some(timer) = timer {
        timer.abort();
    }
}

fn create_dir_all(path: &Path) -> Result<(), VoiceReceiverError> {
    fs::create_dir_all(path).map_err(|source| VoiceReceiverError::CreateDir {
        path: path.to_path_buf(),
        source,
    })
}

fn create_wav_writer(path: &Path) -> Result<WavFileWriter, VoiceReceiverError> {
    WavWriter::create(path, wav_spec()).map_err(|source| VoiceReceiverError::Wav {
        path: path.to_path_buf(),
        source,
    })
}

fn wav_spec() -> WavSpec {
    WavSpec {
        channels: WAV_CHANNELS,
        sample_rate: WAV_SAMPLE_RATE,
        bits_per_sample: WAV_BITS_PER_SAMPLE,
        sample_format: SampleFormat::Int,
    }
}

fn expand_tilde(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if raw == "~" {
        return dirs::home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(dir: PathBuf) -> VoiceReceiverConfig {
        VoiceReceiverConfig {
            recordings_dir: dir,
            segment_idle: Duration::from_millis(30),
            utterance_idle: Duration::from_millis(100),
            allowed_user_ids: HashSet::new(),
        }
    }

    #[tokio::test]
    async fn short_pause_stays_in_one_utterance() {
        let temp = tempfile::tempdir().unwrap();
        let receiver = VoiceReceiver::new(test_config(temp.path().to_path_buf()));
        receiver.register_speaking(42, 7).await;

        receiver.queue_pcm(42, &[1; 960]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        receiver.queue_pcm(42, &[2; 960]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(130)).await;

        let pending = receiver.take_pending().await;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].segment_paths.len(), 1);
        assert_eq!(pending[0].samples_written, 1_920);
        assert_eq!(
            hound::WavReader::open(&pending[0].path).unwrap().duration(),
            960
        );
    }

    #[tokio::test]
    async fn segment_idle_splits_segments_without_splitting_utterance() {
        let temp = tempfile::tempdir().unwrap();
        let receiver = VoiceReceiver::new(test_config(temp.path().to_path_buf()));
        receiver.register_speaking(42, 7).await;

        receiver.queue_pcm(42, &[1; 480]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        receiver.queue_pcm(42, &[2; 480]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(130)).await;

        let pending = receiver.take_pending().await;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].segment_paths.len(), 2);
        assert!(pending[0].segment_paths.iter().all(|path| path.exists()));
        assert_eq!(
            hound::WavReader::open(&pending[0].path).unwrap().duration(),
            480
        );
    }

    #[tokio::test]
    async fn allowed_user_filter_ignores_unlisted_speaker() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = test_config(temp.path().to_path_buf());
        config.allowed_user_ids.insert(7);
        let receiver = VoiceReceiver::new(config);
        receiver.register_speaking(42, 8).await;

        assert!(!receiver.queue_pcm(42, &[1; 480]).await.unwrap());
        tokio::time::sleep(Duration::from_millis(130)).await;

        assert!(receiver.take_pending().await.is_empty());
    }
}
