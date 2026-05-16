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
use super::utils::expand_tilde;

// === Tunables (was scattered constants) ===
// Discord/Songbird PCM is decoded at fixed 48 kHz stereo s16le; these mirror that contract.
const WAV_CHANNELS: u16 = 2;
const WAV_SAMPLE_RATE: u32 = 48_000;
const WAV_BITS_PER_SAMPLE: u16 = 16;

// --- Test-only timing knobs ---
// These are referenced from `#[cfg(test)]` and intentionally kept next to the production
// tunables so wall-clock assumptions live in one place.
#[cfg(test)]
const TEST_SEGMENT_IDLE: Duration = Duration::from_millis(30);
#[cfg(test)]
const TEST_UTTERANCE_IDLE: Duration = Duration::from_millis(100);
/// Tiny gap that must stay below `TEST_SEGMENT_IDLE` so two writes coalesce into one segment.
#[cfg(test)]
const TEST_INTRA_SEGMENT_GAP: Duration = Duration::from_millis(10);
/// Gap between writes that exceeds `TEST_SEGMENT_IDLE` (30ms) but stays under
/// `TEST_UTTERANCE_IDLE` (100ms), so the segment closes without splitting the utterance.
#[cfg(test)]
const TEST_SEGMENT_BOUNDARY_GAP: Duration = Duration::from_millis(50);
/// Wait that exceeds `TEST_UTTERANCE_IDLE` (100ms) so the utterance flushes.
#[cfg(test)]
const TEST_UTTERANCE_FLUSH_WAIT: Duration = Duration::from_millis(130);

type WavFileWriter = WavWriter<std::io::BufWriter<std::fs::File>>;

#[derive(Debug, Clone)]
pub(crate) struct CompletedUtterance {
    pub(crate) user_id: u64,
    pub(crate) control_channel_id: Option<u64>,
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
    /// `false` 면 시작 시 누적된 utterance/segment wav 를 GC 한다 (#2156).
    pub(crate) keep_recordings: bool,
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
            keep_recordings: config.keep_voice_recordings(),
        }
    }
}

impl Default for VoiceReceiverConfig {
    fn default() -> Self {
        Self::from_voice_config(&VoiceConfig::default())
    }
}

pub(crate) trait VoiceReceiveHook: Send + Sync {
    fn observe_pcm(&self, control_channel_id: u64, user_id: u64, samples: &[i16]);

    fn utterance_completed(&self, control_channel_id: u64, utterance: &CompletedUtterance);
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
        Self::new_with_hook(config, None)
    }

    pub(crate) fn new_with_hook(
        config: VoiceReceiverConfig,
        hook: Option<Arc<dyn VoiceReceiveHook>>,
    ) -> Self {
        // #2156: voice 시작 시 누적된 wav 를 정리한다.
        // 환경변수/config 로 보존을 명시한 경우만 건너뛴다.
        if !config.keep_recordings {
            gc_voice_recordings_dir(&config.recordings_dir);
        }
        Self {
            inner: Arc::new(ReceiverState::new(config, hook)),
        }
    }

    pub(crate) fn from_voice_config(config: &VoiceConfig) -> Self {
        Self::new(VoiceReceiverConfig::from_voice_config(config))
    }

    pub(crate) fn from_voice_config_with_hook(
        config: &VoiceConfig,
        hook: Option<Arc<dyn VoiceReceiveHook>>,
    ) -> Self {
        Self::new_with_hook(VoiceReceiverConfig::from_voice_config(config), hook)
    }

    pub(crate) fn event_handler(&self, control_channel_id: u64) -> VoiceReceiverEventHandler {
        VoiceReceiverEventHandler {
            receiver: self.clone(),
            control_channel_id,
        }
    }

    pub(crate) async fn register_speaking(&self, ssrc: u32, user_id: u64) {
        self.inner.register_speaking(ssrc, user_id).await;
    }

    pub(crate) async fn queue_pcm(
        &self,
        ssrc: u32,
        samples: &[i16],
    ) -> Result<bool, VoiceReceiverError> {
        self.inner.queue_pcm(ssrc, samples, None).await
    }

    pub(crate) async fn queue_pcm_for_control_channel(
        &self,
        control_channel_id: u64,
        ssrc: u32,
        samples: &[i16],
    ) -> Result<bool, VoiceReceiverError> {
        self.inner
            .queue_pcm(ssrc, samples, Some(control_channel_id))
            .await
    }

    pub(crate) async fn flush_all(&self) -> Vec<CompletedUtterance> {
        self.inner.flush_all().await
    }

    /// F2 (#2046): 지정된 control_channel_id 범위로만 utterance를 flush.
    /// 멀티-길드 환경에서 한 길드 leave가 다른 길드의 진행 중인 utterance/SSRC 매핑을
    /// 망가뜨리지 않도록 한다.
    pub(crate) async fn flush_for_control_channel(
        &self,
        control_channel_id: u64,
    ) -> Vec<CompletedUtterance> {
        self.inner
            .flush_for_control_channel(control_channel_id)
            .await
    }

    pub(crate) async fn take_pending(&self) -> Vec<CompletedUtterance> {
        self.inner.take_pending().await
    }
}

#[derive(Clone)]
pub(crate) struct VoiceReceiverEventHandler {
    receiver: VoiceReceiver,
    control_channel_id: u64,
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

#[async_trait]
impl EventHandler for VoiceReceiverEventHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        match ctx {
            EventContext::SpeakingStateUpdate(update) => {
                if let Some(user_id) = update.user_id {
                    let user_id = user_id.0;
                    if user_id != 0 {
                        self.receiver.register_speaking(update.ssrc, user_id).await;
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
                    if let Err(error) = self
                        .receiver
                        .queue_pcm_for_control_channel(self.control_channel_id, *ssrc, samples)
                        .await
                    {
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
    hook: Option<Arc<dyn VoiceReceiveHook>>,
    ssrc_users: RwLock<HashMap<u32, u64>>,
    users: Mutex<HashMap<u64, UserAudioState>>,
    pending: Mutex<Vec<CompletedUtterance>>,
    sequence: AtomicU64,
}

impl ReceiverState {
    fn new(config: VoiceReceiverConfig, hook: Option<Arc<dyn VoiceReceiveHook>>) -> Self {
        Self {
            config,
            hook,
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
        control_channel_id: Option<u64>,
    ) -> Result<bool, VoiceReceiverError> {
        let Some(user_id) = self.ssrc_users.read().await.get(&ssrc).copied() else {
            return Err(VoiceReceiverError::UnknownSsrc(ssrc));
        };
        if !self.user_allowed(user_id) {
            return Ok(false);
        }

        // F3 (#2046): tokio Mutex(`users`)를 잡은 채로 동기 WAV/디스크 I/O를 수행하면
        // executor 워커 스레드가 차단되고 모든 user의 PCM tick이 직렬화된다.
        // 짧은 메타데이터 갱신만 락 안에서 처리하고, 실제 WAV writer 생성/샘플 쓰기는
        // 락을 풀고 spawn_blocking으로 옮긴 뒤 다시 락을 잡아 active를 복귀시킨다.
        let mut active_opt: Option<ActiveUtterance> = {
            let mut users = self.users.lock().await;
            let user_state = users.entry(user_id).or_default();
            user_state.active.take()
        };

        if active_opt.is_none() {
            // create_active_utterance는 create_dir_all + WavWriter::create(동기 syscall).
            let receiver = self.clone();
            active_opt = Some(
                tokio::task::spawn_blocking(move || {
                    receiver.create_active_utterance(user_id, control_channel_id)
                })
                .await
                .map_err(|join_err| VoiceReceiverError::CreateDir {
                    path: PathBuf::new(),
                    source: std::io::Error::other(format!(
                        "create_active_utterance blocking task join failed: {join_err}"
                    )),
                })??,
            );
        }

        let mut active = active_opt.expect("active utterance present");
        if active.control_channel_id.is_none() {
            active.control_channel_id = control_channel_id;
        }
        let utterance_id = active.utterance_id.clone();
        let notify_control_channel_id = control_channel_id.or(active.control_channel_id);

        // 디스크 I/O가 발생할 수 있는 ensure_segment_writer + write_samples도 spawn_blocking.
        let samples_owned: Vec<i16> = samples.to_vec();
        let io_result = tokio::task::spawn_blocking(move || {
            active.ensure_segment_writer()?;
            active.write_samples(&samples_owned)?;
            Ok::<ActiveUtterance, VoiceReceiverError>(active)
        })
        .await
        .map_err(|join_err| VoiceReceiverError::Wav {
            path: PathBuf::new(),
            source: hound::Error::IoError(std::io::Error::other(format!(
                "voice WAV write blocking task join failed: {join_err}"
            ))),
        })?;

        let active = match io_result {
            Ok(active) => active,
            Err(err) => {
                // 디스크 I/O 실패 시 active를 잃지 않도록 복귀하지 않고(이미 동기 함수에서 partial state),
                // user_state는 깨끗하게 두어 다음 PCM에서 새 utterance가 시작되도록 한다.
                // 단 timers는 비활성 상태로 유지된다.
                return Err(err);
            }
        };

        // active 복귀 + 타이머 재설정.
        {
            let mut users = self.users.lock().await;
            let user_state = users.entry(user_id).or_default();
            user_state.active = Some(active);
            self.arm_timers(user_id, utterance_id, user_state);
        }

        self.notify_pcm(notify_control_channel_id, user_id, samples);
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
        // F1 (#2046): hook 기반 단방향 소비 시 pending Vec 누적을 스킵해 메모리 누수 방지.
        // hook이 없을 때만 폴링(`take_pending`) 경로를 위해 pending에 보관.
        if self.hook.is_none() {
            self.pending.lock().await.push(completed.clone());
        }
        self.notify_utterance_completed(&completed);
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
        // F1 (#2046): hook 등록 시 pending에 push하지 않음.
        if !completed.is_empty() && self.hook.is_none() {
            self.pending.lock().await.extend(completed.clone());
        }
        for utterance in &completed {
            self.notify_utterance_completed(utterance);
        }
        completed
    }

    /// F2 (#2046): 특정 control_channel_id에 묶인 utterance만 flush.
    /// 다른 길드/채널의 진행 중인 utterance·SSRC 매핑은 보존된다.
    async fn flush_for_control_channel(
        self: &Arc<Self>,
        control_channel_id: u64,
    ) -> Vec<CompletedUtterance> {
        let (active, drained_user_ids) = {
            let mut users = self.users.lock().await;
            // active.control_channel_id == 지정한 채널인 사용자만 골라 제거.
            let matching_user_ids: Vec<u64> = users
                .iter()
                .filter_map(|(user_id, state)| {
                    state.active.as_ref().and_then(|active| {
                        if active.control_channel_id == Some(control_channel_id) {
                            Some(*user_id)
                        } else {
                            None
                        }
                    })
                })
                .collect();
            let mut drained = Vec::new();
            let mut drained_user_ids: Vec<u64> = Vec::new();
            for user_id in matching_user_ids {
                if let Some(mut user_state) = users.remove(&user_id) {
                    abort_timer(user_state.segment_timer.take());
                    abort_timer(user_state.utterance_timer.take());
                    if let Some(active) = user_state.active.take() {
                        drained.push(active);
                        drained_user_ids.push(user_id);
                    }
                }
            }
            (drained, drained_user_ids)
        };

        if !drained_user_ids.is_empty() {
            // 해당 user들의 SSRC 매핑만 제거 (다른 길드 SSRC 보존).
            let mut ssrc_users = self.ssrc_users.write().await;
            ssrc_users.retain(|_, user_id| !drained_user_ids.contains(user_id));
        }

        let mut completed = Vec::new();
        for active in active {
            match active.finalize() {
                Ok(utterance) => completed.push(utterance),
                Err(error) => tracing::warn!(error = %error, "failed to flush voice utterance"),
            }
        }
        if !completed.is_empty() && self.hook.is_none() {
            self.pending.lock().await.extend(completed.clone());
        }
        for utterance in &completed {
            self.notify_utterance_completed(utterance);
        }
        completed
    }

    async fn take_pending(&self) -> Vec<CompletedUtterance> {
        std::mem::take(&mut *self.pending.lock().await)
    }

    fn user_allowed(&self, user_id: u64) -> bool {
        self.config.allowed_user_ids.is_empty() || self.config.allowed_user_ids.contains(&user_id)
    }

    fn create_active_utterance(
        &self,
        user_id: u64,
        control_channel_id: Option<u64>,
    ) -> Result<ActiveUtterance, VoiceReceiverError> {
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
            control_channel_id,
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

    fn notify_pcm(&self, control_channel_id: Option<u64>, user_id: u64, samples: &[i16]) {
        let Some(control_channel_id) = control_channel_id else {
            return;
        };
        if let Some(hook) = &self.hook {
            hook.observe_pcm(control_channel_id, user_id, samples);
        }
    }

    fn notify_utterance_completed(&self, utterance: &CompletedUtterance) {
        let Some(control_channel_id) = utterance.control_channel_id else {
            return;
        };
        if let Some(hook) = &self.hook {
            hook.utterance_completed(control_channel_id, utterance);
        }
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
    control_channel_id: Option<u64>,
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
            control_channel_id: self.control_channel_id,
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

/// #2156: voice 시작 시 호출된다. 정확히 다음 2단계 레이아웃만 정리한다:
///   `<recordings_dir>/utterances/user_<id>/<utterance-id>.wav`
///   `<recordings_dir>/segments/user_<id>/<utterance-id>_segment_NNN.wav`
/// 더 깊거나 다른 레이아웃의 파일은 손대지 않는다 (운영자가 의도적으로 모아둔
/// 것일 수 있음). 디렉토리 자체는 남겨 매 utterance 가 다시 `create_dir_all`
/// 비용을 치르지 않게 한다. 에러는 debug 로그로만 흘려 GC 가 시작 흐름을
/// 막지 않게 한다.
///
/// 안전 가드:
/// - symlink user 디렉토리는 따라가지 않고 skip 한다 (외부 트리로 빠져 외부
///   파일을 지울 위험 차단).
/// - symlink 파일 entry 도 skip (자체 wav 가 아니라 외부 wav 를 가리킬 수 있음).
fn gc_voice_recordings_dir(recordings_dir: &Path) {
    let utterance_root = recordings_dir.join("utterances");
    let segment_root = recordings_dir.join("segments");
    let removed_utterances = gc_wav_subtree(&utterance_root);
    let removed_segments = gc_wav_subtree(&segment_root);
    if removed_utterances + removed_segments > 0 {
        tracing::info!(
            removed_utterances,
            removed_segments,
            recordings_dir = %recordings_dir.display(),
            "voice recordings GC removed accumulated wav files (#2156)"
        );
    }
}

fn gc_wav_subtree(root: &Path) -> usize {
    let mut removed = 0usize;
    // Root 자체(예: `utterances`, `segments`) 가 symlink 면 따라가지 않는다.
    // `fs::symlink_metadata` 는 마지막 컴포넌트에 대해 symlink 를 그대로 보고하므로
    // 외부 트리로 빠지는 GC 진입을 차단할 수 있다.
    match fs::symlink_metadata(root) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            tracing::debug!(
                path = %root.display(),
                "voice recordings GC skipped: root is a symlink"
            );
            return 0;
        }
        Ok(_) => {}
        Err(_) => return 0,
    }
    let Ok(top) = fs::read_dir(root) else {
        return 0;
    };
    for user_dir in top.flatten() {
        // Symlink user 디렉토리는 정책상 따라가지 않는다 — 외부 트리로 빠질 수 있음.
        // `DirEntry::file_type` 와 `symlink_metadata` 모두 마지막 컴포넌트의
        // symlink 를 그대로 보고하므로 어떤 쪽을 써도 무방하지만, root 검사와
        // 동일하게 `symlink_metadata` 로 통일해 보안 의도를 일관되게 표현한다.
        let user_path = user_dir.path();
        match fs::symlink_metadata(&user_path) {
            Ok(md) if md.file_type().is_symlink() => continue,
            Ok(_) => {}
            Err(_) => continue,
        }
        let Ok(entries) = fs::read_dir(&user_path) else {
            continue;
        };
        for entry in entries.flatten() {
            let entry_path = entry.path();
            // Symlink 파일도 skip — 가리키는 대상이 외부 wav 일 수 있다.
            match fs::symlink_metadata(&entry_path) {
                Ok(md) if md.file_type().is_symlink() => continue,
                Ok(_) => {}
                Err(_) => continue,
            }
            if entry_path.extension().and_then(|ext| ext.to_str()) != Some("wav") {
                continue;
            }
            match fs::remove_file(&entry_path) {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => tracing::debug!(
                    error = %error,
                    path = %entry_path.display(),
                    "voice recordings GC could not remove file"
                ),
            }
        }
    }
    removed
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc as StdArc, Mutex as StdMutex};

    fn test_config(dir: PathBuf) -> VoiceReceiverConfig {
        VoiceReceiverConfig {
            recordings_dir: dir,
            segment_idle: TEST_SEGMENT_IDLE,
            utterance_idle: TEST_UTTERANCE_IDLE,
            allowed_user_ids: HashSet::new(),
            // 테스트는 GC 가 짠 후 생성한 utterance wav 를 즉시 검사하므로
            // GC 그 자체와 무관. 다만 keep_recordings=true 로 두면 시작 시
            // wav 삭제가 일어나지 않아 결정적이다.
            keep_recordings: true,
        }
    }

    #[derive(Default)]
    struct MockHook {
        pcm_frames: AtomicUsize,
        completions: AtomicUsize,
        control_channels: StdMutex<Vec<u64>>,
    }

    impl VoiceReceiveHook for MockHook {
        fn observe_pcm(&self, control_channel_id: u64, _user_id: u64, _samples: &[i16]) {
            self.pcm_frames.fetch_add(1, Ordering::SeqCst);
            self.control_channels
                .lock()
                .unwrap()
                .push(control_channel_id);
        }

        fn utterance_completed(&self, control_channel_id: u64, _utterance: &CompletedUtterance) {
            self.completions.fetch_add(1, Ordering::SeqCst);
            self.control_channels
                .lock()
                .unwrap()
                .push(control_channel_id);
        }
    }

    #[tokio::test]
    async fn short_pause_stays_in_one_utterance() {
        let temp = tempfile::tempdir().unwrap();
        let receiver = VoiceReceiver::new(test_config(temp.path().to_path_buf()));
        receiver.register_speaking(42, 7).await;

        receiver.queue_pcm(42, &[1; 960]).await.unwrap();
        tokio::time::sleep(TEST_INTRA_SEGMENT_GAP).await;
        receiver.queue_pcm(42, &[2; 960]).await.unwrap();
        tokio::time::sleep(TEST_UTTERANCE_FLUSH_WAIT).await;

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
        tokio::time::sleep(TEST_SEGMENT_BOUNDARY_GAP).await;
        receiver.queue_pcm(42, &[2; 480]).await.unwrap();
        tokio::time::sleep(TEST_UTTERANCE_FLUSH_WAIT).await;

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
        tokio::time::sleep(TEST_UTTERANCE_FLUSH_WAIT).await;

        assert!(receiver.take_pending().await.is_empty());
    }

    #[tokio::test]
    async fn receive_hook_observes_pcm_and_completed_utterance_with_control_channel() {
        let temp = tempfile::tempdir().unwrap();
        let hook = StdArc::new(MockHook::default());
        let receiver = VoiceReceiver::new_with_hook(
            test_config(temp.path().to_path_buf()),
            Some(hook.clone()),
        );
        receiver.register_speaking(42, 7).await;

        receiver
            .queue_pcm_for_control_channel(123, 42, &[1; 480])
            .await
            .unwrap();
        tokio::time::sleep(TEST_UTTERANCE_FLUSH_WAIT).await;

        // F1 (#2046): hook 가 등록된 경로에서는 pending Vec 누적이 꺼져 있으므로
        // take_pending 은 비어 있어야 한다 (메모리 누수 방지). hook 콜백으로만
        // utterance 가 통보된다.
        let pending = receiver.take_pending().await;
        assert!(pending.is_empty());
        assert_eq!(hook.pcm_frames.load(Ordering::SeqCst), 1);
        assert_eq!(hook.completions.load(Ordering::SeqCst), 1);
        assert_eq!(*hook.control_channels.lock().unwrap(), vec![123, 123]);
    }

    /// #2156: 시작 시 recordings_dir 의 누적 wav 가 모두 사라지고, 디렉토리는
    /// 보존되어야 한다. 비-wav 파일과 외부 디렉토리는 손대지 않는다.
    #[test]
    fn gc_voice_recordings_dir_removes_legacy_wavs_and_preserves_other_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let utterance_user = root.join("utterances").join("user_42");
        let segment_user = root.join("segments").join("user_42");
        fs::create_dir_all(&utterance_user).unwrap();
        fs::create_dir_all(&segment_user).unwrap();

        let stale_utterance = utterance_user.join("20260101-000001-000001.wav");
        let stale_segment = segment_user.join("20260101-000001-000001_segment_001.wav");
        let unrelated_txt = utterance_user.join("transcript-sidecar.txt");
        let outside_file = root.join("unrelated.wav");
        fs::write(&stale_utterance, b"RIFF").unwrap();
        fs::write(&stale_segment, b"RIFF").unwrap();
        fs::write(&unrelated_txt, b"hello").unwrap();
        fs::write(&outside_file, b"RIFF").unwrap();

        gc_voice_recordings_dir(root);

        assert!(!stale_utterance.exists(), "utterance wav must be removed");
        assert!(!stale_segment.exists(), "segment wav must be removed");
        assert!(
            unrelated_txt.exists(),
            "non-wav sidecars in user dir must be preserved (transcript sidecars are handled per-utterance)"
        );
        assert!(
            outside_file.exists(),
            "files outside utterances/segments subtrees must not be touched"
        );
        assert!(
            utterance_user.is_dir() && segment_user.is_dir(),
            "user directories must remain to avoid re-create cost on the next utterance"
        );
    }

    #[test]
    fn gc_voice_recordings_dir_is_a_noop_when_directory_missing() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("does-not-exist");
        // Just verify no panic / no error propagation.
        gc_voice_recordings_dir(&missing);
        assert!(!missing.exists());
    }

    /// #2156: GC 는 `<root>/utterances` 또는 `<root>/segments` 가 symlink 면
    /// 따라가지 않는다 — 외부 트리의 wav 가 의도치 않게 삭제되지 않도록 한다.
    #[cfg(unix)]
    #[test]
    fn gc_voice_recordings_dir_skips_symlinked_root() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();

        // 외부 트리(레코딩 루트 바깥)에 wav 를 둔다.
        let outside = temp.path().join("outside");
        let outside_user = outside.join("user_99");
        fs::create_dir_all(&outside_user).unwrap();
        let external_wav = outside_user.join("external.wav");
        fs::write(&external_wav, b"RIFF").unwrap();

        // 레코딩 루트의 utterances 디렉토리를 외부 트리로 향하는 symlink 로 만든다.
        let recordings_root = temp.path().join("recordings");
        fs::create_dir_all(&recordings_root).unwrap();
        symlink(&outside, recordings_root.join("utterances")).unwrap();

        gc_voice_recordings_dir(&recordings_root);

        assert!(
            external_wav.exists(),
            "wav files outside the recordings tree must survive even when the GC root is symlinked into them"
        );
    }

    /// #2156: user-dir 또는 wav 파일이 symlink 면 GC 가 따라가지 않고 skip 해야 한다.
    #[cfg(unix)]
    #[test]
    fn gc_voice_recordings_dir_skips_symlinked_user_dir_and_wav_entry() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();

        // 외부 user 디렉토리와 외부 wav (target of symlinks).
        let outside = temp.path().join("outside");
        let outside_user = outside.join("user_99");
        fs::create_dir_all(&outside_user).unwrap();
        let external_wav_target = outside_user.join("external.wav");
        fs::write(&external_wav_target, b"RIFF").unwrap();
        let other_external_wav = temp.path().join("target.wav");
        fs::write(&other_external_wav, b"RIFF").unwrap();

        // recordings_dir 안에 symlink user_dir 와 symlink wav 를 만든다.
        let recordings_root = temp.path().join("recordings");
        let utterances_root = recordings_root.join("utterances");
        fs::create_dir_all(&utterances_root).unwrap();
        symlink(&outside_user, utterances_root.join("user_42_link")).unwrap();
        let real_user = utterances_root.join("user_1");
        fs::create_dir_all(&real_user).unwrap();
        symlink(&other_external_wav, real_user.join("symlink-to.wav")).unwrap();
        let real_wav = real_user.join("real.wav");
        fs::write(&real_wav, b"RIFF").unwrap();

        gc_voice_recordings_dir(&recordings_root);

        assert!(
            external_wav_target.exists(),
            "files under a symlinked user_dir must not be touched"
        );
        assert!(
            other_external_wav.exists(),
            "files referenced by a symlinked wav entry must not be touched"
        );
        assert!(
            !real_wav.exists(),
            "real (non-symlinked) wav files must still be removed"
        );
    }
}
