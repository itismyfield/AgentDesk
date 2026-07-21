use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

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
const PENDING_IO_FLUSH_POLL: Duration = Duration::from_millis(1);
const PENDING_IO_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

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
    #[cfg(test)]
    pub(crate) blocking_io_delay: Duration,
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
            #[cfg(test)]
            blocking_io_delay: Duration::ZERO,
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
    #[allow(dead_code)] // voice runtime wired only when voice config enabled; no target exercises it. See #3034
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

    #[allow(dead_code)] // voice runtime wired only when voice config enabled; no target exercises it. See #3034
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

    pub(crate) async fn register_speaking_for_control_channel(
        &self,
        control_channel_id: u64,
        ssrc: u32,
        user_id: u64,
    ) {
        self.inner
            .register_speaking_for_control_channel(control_channel_id, ssrc, user_id)
            .await;
    }

    /// #3914: drop the SSRC→user mappings for a client that left the voice
    /// session. Songbird emits `ClientDisconnect` on leave but the receiver
    /// previously never handled it, so `ssrc_users` grew monotonically under
    /// long-running channel churn (every (re)join allocates a fresh SSRC).
    pub(crate) async fn forget_disconnected_user(&self, user_id: u64) {
        self.inner.forget_disconnected_user(None, user_id).await;
    }

    /// F2-scoped variant of [`forget_disconnected_user`]: only removes mappings
    /// bound to `control_channel_id` so a leave in one guild/channel never drops
    /// another channel's live SSRC mapping.
    pub(crate) async fn forget_disconnected_user_for_control_channel(
        &self,
        control_channel_id: u64,
        user_id: u64,
    ) {
        self.inner
            .forget_disconnected_user(Some(control_channel_id), user_id)
            .await;
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

    #[allow(dead_code)] // voice runtime wired only when voice config enabled; no target exercises it. See #3034
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

    #[allow(dead_code)] // voice runtime wired only when voice config enabled; no target exercises it. See #3034
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
            EventContext::ClientDisconnect(disconnect) => {
                self.forget_disconnected_user(disconnect.user_id.0).await;
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
                        self.receiver
                            .register_speaking_for_control_channel(
                                self.control_channel_id,
                                update.ssrc,
                                user_id,
                            )
                            .await;
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
            EventContext::ClientDisconnect(disconnect) => {
                self.receiver
                    .forget_disconnected_user_for_control_channel(
                        self.control_channel_id,
                        disconnect.user_id.0,
                    )
                    .await;
            }
            _ => {}
        }

        None
    }
}

struct ReceiverState {
    config: VoiceReceiverConfig,
    hook: Option<Arc<dyn VoiceReceiveHook>>,
    ssrc_users: RwLock<HashMap<VoiceSsrcKey, u64>>,
    users: Mutex<HashMap<VoiceAudioKey, UserAudioState>>,
    pending: Mutex<Vec<CompletedUtterance>>,
    sequence: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct VoiceAudioKey {
    control_channel_id: Option<u64>,
    user_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct VoiceSsrcKey {
    control_channel_id: Option<u64>,
    ssrc: u32,
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
        self.ssrc_users.write().await.insert(
            VoiceSsrcKey {
                control_channel_id: None,
                ssrc,
            },
            user_id,
        );
    }

    async fn register_speaking_for_control_channel(
        &self,
        control_channel_id: u64,
        ssrc: u32,
        user_id: u64,
    ) {
        self.ssrc_users.write().await.insert(
            VoiceSsrcKey {
                control_channel_id: Some(control_channel_id),
                ssrc,
            },
            user_id,
        );
    }

    /// #3914: remove every SSRC mapping for `user_id` (optionally scoped to a
    /// single `control_channel_id`). Called on songbird `ClientDisconnect` so a
    /// leaver's SSRC entries do not accumulate indefinitely.
    async fn forget_disconnected_user(&self, control_channel_id: Option<u64>, user_id: u64) {
        if user_id == 0 {
            return;
        }
        let mut ssrc_users = self.ssrc_users.write().await;
        let before = ssrc_users.len();
        ssrc_users.retain(|key, mapped_user| {
            !(*mapped_user == user_id
                && (control_channel_id.is_none() || key.control_channel_id == control_channel_id))
        });
        let removed = before - ssrc_users.len();
        if removed > 0 {
            tracing::debug!(
                user_id,
                control_channel_id = ?control_channel_id,
                removed,
                "voice receiver dropped SSRC mappings for disconnected client (#3914)"
            );
        }
    }

    async fn queue_pcm(
        self: &Arc<Self>,
        ssrc: u32,
        samples: &[i16],
        control_channel_id: Option<u64>,
    ) -> Result<bool, VoiceReceiverError> {
        let Some(user_id) = self.lookup_user_id(ssrc, control_channel_id).await else {
            return Err(VoiceReceiverError::UnknownSsrc(ssrc));
        };
        if !self.user_allowed(user_id) {
            return Ok(false);
        }
        let key = VoiceAudioKey {
            control_channel_id,
            user_id,
        };

        // F3 (#2046): tokio Mutex(`users`)를 잡은 채로 동기 WAV/디스크 I/O를 수행하면
        // executor 워커 스레드가 차단되고 모든 user의 PCM tick이 직렬화된다.
        // 짧은 메타데이터 갱신만 락 안에서 처리하고, 실제 WAV writer 생성/샘플 쓰기는
        // 락을 풀고 spawn_blocking으로 옮긴 뒤 다시 락을 잡아 active를 복귀시킨다.
        let active_opt: Option<ActiveUtterance> = loop {
            let active = {
                let mut users = self.users.lock().await;
                let user_state = users.entry(key).or_default();
                if user_state.pending_io {
                    None
                } else {
                    user_state.pending_io = true;
                    Some(user_state.active.take())
                }
            };
            if let Some(active) = active {
                break active;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        };

        let mut active = if let Some(active) = active_opt {
            active
        } else {
            // create_active_utterance는 create_dir_all + WavWriter::create(동기 syscall).
            let receiver = self.clone();
            match tokio::task::spawn_blocking(move || {
                receiver.create_active_utterance(user_id, control_channel_id)
            })
            .await
            .map_err(|join_err| VoiceReceiverError::CreateDir {
                path: PathBuf::new(),
                source: std::io::Error::other(format!(
                    "create_active_utterance blocking task join failed: {join_err}"
                )),
            })
            .and_then(|result| result)
            {
                Ok(active) => active,
                Err(error) => {
                    self.clear_pending_io_after_error(key).await;
                    return Err(error);
                }
            }
        };

        if active.control_channel_id.is_none() {
            active.control_channel_id = control_channel_id;
        }
        let utterance_id = active.utterance_id.clone();
        let notify_control_channel_id = control_channel_id.or(active.control_channel_id);

        // 디스크 I/O가 발생할 수 있는 ensure_segment_writer + write_samples도 spawn_blocking.
        let samples_owned: Vec<i16> = samples.to_vec();
        let blocking_io_delay = self.blocking_io_delay();
        let io_result = tokio::task::spawn_blocking(move || {
            if blocking_io_delay > Duration::ZERO {
                std::thread::sleep(blocking_io_delay);
            }
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
        })
        .and_then(|result| result);

        let active = match io_result {
            Ok(active) => active,
            Err(err) => {
                // 디스크 I/O 실패 시 active를 잃지 않도록 복귀하지 않고(이미 동기 함수에서 partial state),
                // user_state는 깨끗하게 두어 다음 PCM에서 새 utterance가 시작되도록 한다.
                // 단 timers는 비활성 상태로 유지된다.
                self.clear_pending_io_after_error(key).await;
                return Err(err);
            }
        };

        // active 복귀 + 타이머 재설정.
        {
            let mut users = self.users.lock().await;
            let user_state = users.entry(key).or_default();
            user_state.active = Some(active);
            user_state.pending_io = false;
            self.arm_timers(key, utterance_id, user_state);
        }

        self.notify_pcm(notify_control_channel_id, user_id, samples);
        Ok(true)
    }

    async fn lookup_user_id(&self, ssrc: u32, control_channel_id: Option<u64>) -> Option<u64> {
        let ssrc_users = self.ssrc_users.read().await;
        if let Some(control_channel_id) = control_channel_id {
            if let Some(user_id) = ssrc_users
                .get(&VoiceSsrcKey {
                    control_channel_id: Some(control_channel_id),
                    ssrc,
                })
                .copied()
            {
                return Some(user_id);
            }
        }
        ssrc_users
            .get(&VoiceSsrcKey {
                control_channel_id: None,
                ssrc,
            })
            .copied()
    }

    async fn clear_pending_io_after_error(&self, key: VoiceAudioKey) {
        let mut users = self.users.lock().await;
        if let Some(mut user_state) = users.remove(&key) {
            abort_timer(user_state.segment_timer.take());
            abort_timer(user_state.utterance_timer.take());
        }
    }

    async fn wait_for_pending_io_to_clear<F>(&self, mut matches: F) -> usize
    where
        F: FnMut(VoiceAudioKey, &UserAudioState) -> bool,
    {
        let started = Instant::now();
        loop {
            let pending_count = {
                let users = self.users.lock().await;
                users
                    .iter()
                    .filter(|(key, state)| matches(**key, state) && state.pending_io)
                    .count()
            };
            if pending_count == 0 {
                return 0;
            }
            if started.elapsed() >= PENDING_IO_FLUSH_TIMEOUT {
                return pending_count;
            }
            tokio::time::sleep(PENDING_IO_FLUSH_POLL).await;
        }
    }

    fn blocking_io_delay(&self) -> Duration {
        #[cfg(test)]
        {
            self.config.blocking_io_delay
        }
        #[cfg(not(test))]
        {
            Duration::ZERO
        }
    }

    async fn finish_segment(
        self: &Arc<Self>,
        key: VoiceAudioKey,
        utterance_id: &str,
    ) -> Result<(), VoiceReceiverError> {
        let mut users = self.users.lock().await;
        let Some(user_state) = users.get_mut(&key) else {
            return Ok(());
        };
        if user_state.pending_io {
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
        key: VoiceAudioKey,
        utterance_id: &str,
        abort_utterance_timer: bool,
    ) -> Result<Option<CompletedUtterance>, VoiceReceiverError> {
        let active = {
            let mut users = self.users.lock().await;
            let Some(user_state) = users.get_mut(&key) else {
                return Ok(None);
            };
            if user_state.pending_io {
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
            users.remove(&key);
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

    #[allow(dead_code)] // voice runtime wired only when voice config enabled; no target exercises it. See #3034
    async fn flush_all(self: &Arc<Self>) -> Vec<CompletedUtterance> {
        let pending_after_wait = self.wait_for_pending_io_to_clear(|_, _| true).await;
        if pending_after_wait > 0 {
            tracing::warn!(
                pending_after_wait,
                "timed out waiting for pending voice receiver I/O before flush_all"
            );
        }
        let active = {
            let mut users = self.users.lock().await;
            let keys = users
                .iter()
                .filter_map(|(key, state)| {
                    (!state.pending_io && state.active.is_some()).then_some(*key)
                })
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| {
                    let mut user_state = users.remove(&key)?;
                    abort_timer(user_state.segment_timer.take());
                    abort_timer(user_state.utterance_timer.take());
                    user_state.active.take()
                })
                .collect::<Vec<_>>()
        };
        if pending_after_wait == 0 {
            self.ssrc_users.write().await.clear();
        }

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
        let pending_after_wait = self
            .wait_for_pending_io_to_clear(|key, _| {
                key.control_channel_id == Some(control_channel_id)
            })
            .await;
        if pending_after_wait > 0 {
            tracing::warn!(
                pending_after_wait,
                control_channel_id,
                "timed out waiting for pending voice receiver I/O before channel flush"
            );
        }
        let (active, drained_user_ids) = {
            let mut users = self.users.lock().await;
            // VoiceAudioKey.control_channel_id == 지정한 채널인 사용자만 골라 제거.
            let matching_keys: Vec<VoiceAudioKey> = users
                .iter()
                .filter_map(|(key, state)| {
                    if key.control_channel_id == Some(control_channel_id)
                        && !state.pending_io
                        && state.active.is_some()
                    {
                        Some(*key)
                    } else {
                        None
                    }
                })
                .collect();
            let mut drained = Vec::new();
            let mut drained_user_ids: Vec<u64> = Vec::new();
            for key in matching_keys {
                if let Some(mut user_state) = users.remove(&key) {
                    abort_timer(user_state.segment_timer.take());
                    abort_timer(user_state.utterance_timer.take());
                    if let Some(active) = user_state.active.take() {
                        drained.push(active);
                        drained_user_ids.push(key.user_id);
                    }
                }
            }
            (drained, drained_user_ids)
        };

        if !drained_user_ids.is_empty() {
            // 해당 control channel 의 SSRC 매핑만 제거 (다른 길드/채널 SSRC 보존).
            let mut ssrc_users = self.ssrc_users.write().await;
            ssrc_users.retain(|ssrc_key, user_id| {
                ssrc_key.control_channel_id != Some(control_channel_id)
                    || !drained_user_ids.contains(user_id)
            });
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

    #[allow(dead_code)] // voice runtime wired only when voice config enabled; no target exercises it. See #3034
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
        key: VoiceAudioKey,
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
                .finish_segment(key, &segment_utterance_id)
                .await
            {
                tracing::warn!(error = %error, user_id = key.user_id, "failed to finish voice segment");
            }
        }));

        let utterance_state = self.clone();
        let utterance_idle = self.config.utterance_idle;
        user_state.utterance_timer = Some(tokio::spawn(async move {
            tokio::time::sleep(utterance_idle).await;
            match utterance_state
                .flush_utterance(key, &utterance_id, false)
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
                    tracing::warn!(error = %error, user_id = key.user_id, "failed to flush voice utterance")
                }
            }
        }));
    }
}

mod recording;
use recording::*;

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
            blocking_io_delay: Duration::ZERO,
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

    #[tokio::test]
    async fn same_user_in_distinct_control_channels_has_independent_utterances() {
        let temp = tempfile::tempdir().unwrap();
        let receiver = VoiceReceiver::new(test_config(temp.path().to_path_buf()));
        receiver
            .register_speaking_for_control_channel(101, 42, 7)
            .await;
        receiver
            .register_speaking_for_control_channel(202, 42, 7)
            .await;

        receiver
            .queue_pcm_for_control_channel(101, 42, &[1; 480])
            .await
            .unwrap();
        receiver
            .queue_pcm_for_control_channel(202, 42, &[2; 960])
            .await
            .unwrap();
        tokio::time::sleep(TEST_UTTERANCE_FLUSH_WAIT).await;

        let mut pending = receiver.take_pending().await;
        pending.sort_by_key(|utterance| utterance.control_channel_id);
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].user_id, 7);
        assert_eq!(pending[0].control_channel_id, Some(101));
        assert_eq!(pending[0].samples_written, 480);
        assert_eq!(pending[1].user_id, 7);
        assert_eq!(pending[1].control_channel_id, Some(202));
        assert_eq!(pending[1].samples_written, 960);
        assert_ne!(pending[0].utterance_id, pending[1].utterance_id);
    }

    #[tokio::test]
    async fn pending_io_gap_blocks_timers_and_serializes_later_pcm() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = test_config(temp.path().to_path_buf());
        config.blocking_io_delay = Duration::from_millis(150);
        let receiver = VoiceReceiver::new(config);
        receiver
            .register_speaking_for_control_channel(123, 42, 7)
            .await;

        receiver
            .queue_pcm_for_control_channel(123, 42, &[1; 480])
            .await
            .unwrap();

        let second_receiver = receiver.clone();
        let second = tokio::spawn(async move {
            let samples = vec![2; 480];
            second_receiver
                .queue_pcm_for_control_channel(123, 42, &samples)
                .await
                .unwrap();
        });

        let key = VoiceAudioKey {
            control_channel_id: Some(123),
            user_id: 7,
        };
        assert!(
            wait_for_pending_io(&receiver, key).await,
            "second PCM write must expose the pending-I/O guard while active is out for blocking I/O"
        );

        let third_receiver = receiver.clone();
        let third = tokio::spawn(async move {
            let samples = vec![3; 480];
            third_receiver
                .queue_pcm_for_control_channel(123, 42, &samples)
                .await
                .unwrap();
        });

        tokio::time::sleep(TEST_UTTERANCE_FLUSH_WAIT).await;
        assert!(
            receiver.take_pending().await.is_empty(),
            "utterance timer must not finalize while active is temporarily pending I/O"
        );

        second.await.unwrap();
        third.await.unwrap();
        tokio::time::sleep(TEST_UTTERANCE_FLUSH_WAIT).await;

        let pending = receiver.take_pending().await;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].control_channel_id, Some(123));
        assert_eq!(pending[0].samples_written, 1_440);
    }

    #[tokio::test]
    async fn flush_for_control_channel_waits_for_pending_io() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = test_config(temp.path().to_path_buf());
        config.blocking_io_delay = Duration::from_millis(150);
        let receiver = VoiceReceiver::new(config);
        receiver
            .register_speaking_for_control_channel(123, 42, 7)
            .await;

        receiver
            .queue_pcm_for_control_channel(123, 42, &[1; 480])
            .await
            .unwrap();

        let second_receiver = receiver.clone();
        let second = tokio::spawn(async move {
            let samples = vec![2; 480];
            second_receiver
                .queue_pcm_for_control_channel(123, 42, &samples)
                .await
                .unwrap();
        });

        let key = VoiceAudioKey {
            control_channel_id: Some(123),
            user_id: 7,
        };
        assert!(wait_for_pending_io(&receiver, key).await);

        let flushed = receiver.flush_for_control_channel(123).await;
        second.await.unwrap();

        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].control_channel_id, Some(123));
        assert_eq!(flushed[0].samples_written, 960);
        assert_eq!(receiver.take_pending().await.len(), 1);
        let err = receiver
            .queue_pcm_for_control_channel(123, 42, &[3; 480])
            .await
            .unwrap_err();
        assert!(matches!(err, VoiceReceiverError::UnknownSsrc(42)));
    }

    #[tokio::test]
    async fn flush_all_waits_for_pending_io_before_draining() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = test_config(temp.path().to_path_buf());
        config.blocking_io_delay = Duration::from_millis(150);
        let receiver = VoiceReceiver::new(config);
        receiver.register_speaking(42, 7).await;

        receiver.queue_pcm(42, &[1; 480]).await.unwrap();
        let second_receiver = receiver.clone();
        let second = tokio::spawn(async move {
            let samples = vec![2; 480];
            second_receiver.queue_pcm(42, &samples).await.unwrap();
        });

        let key = VoiceAudioKey {
            control_channel_id: None,
            user_id: 7,
        };
        assert!(wait_for_pending_io(&receiver, key).await);

        let flushed = receiver.flush_all().await;
        second.await.unwrap();

        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].control_channel_id, None);
        assert_eq!(flushed[0].samples_written, 960);
        assert_eq!(receiver.take_pending().await.len(), 1);
    }

    /// #3914: a songbird `ClientDisconnect` must drop the leaver's SSRC mapping
    /// so it cannot accumulate, and must leave other speakers untouched.
    #[tokio::test]
    async fn client_disconnect_drops_only_the_leavers_ssrc_mapping() {
        let temp = tempfile::tempdir().unwrap();
        let receiver = VoiceReceiver::new(test_config(temp.path().to_path_buf()));
        receiver.register_speaking(42, 7).await;
        receiver.register_speaking(43, 8).await;

        // User 8 leaves — user 7's mapping must survive.
        receiver.forget_disconnected_user(8).await;
        assert!(receiver.queue_pcm(42, &[1; 480]).await.is_ok());
        assert!(matches!(
            receiver.queue_pcm(43, &[1; 480]).await.unwrap_err(),
            VoiceReceiverError::UnknownSsrc(43)
        ));

        // Now user 7 leaves — its mapping is gone too (no monotonic growth).
        receiver.forget_disconnected_user(7).await;
        assert!(matches!(
            receiver.queue_pcm(42, &[1; 480]).await.unwrap_err(),
            VoiceReceiverError::UnknownSsrc(42)
        ));
    }

    /// #3914 / F2: a disconnect in one control channel must not drop the same
    /// user's SSRC mapping in another control channel.
    #[tokio::test]
    async fn client_disconnect_is_scoped_to_control_channel() {
        let temp = tempfile::tempdir().unwrap();
        let receiver = VoiceReceiver::new(test_config(temp.path().to_path_buf()));
        receiver
            .register_speaking_for_control_channel(101, 42, 7)
            .await;
        receiver
            .register_speaking_for_control_channel(202, 43, 7)
            .await;

        receiver
            .forget_disconnected_user_for_control_channel(101, 7)
            .await;

        assert!(matches!(
            receiver
                .queue_pcm_for_control_channel(101, 42, &[1; 480])
                .await
                .unwrap_err(),
            VoiceReceiverError::UnknownSsrc(42)
        ));
        assert!(
            receiver
                .queue_pcm_for_control_channel(202, 43, &[1; 480])
                .await
                .is_ok()
        );
    }

    async fn wait_for_pending_io(receiver: &VoiceReceiver, key: VoiceAudioKey) -> bool {
        for _ in 0..50 {
            let pending = {
                let users = receiver.inner.users.lock().await;
                users
                    .get(&key)
                    .map(|state| state.pending_io && state.active.is_none())
                    .unwrap_or(false)
            };
            if pending {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        false
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
