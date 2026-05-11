//! Voice barge-in detection and processing-time interruption policy.

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

pub(crate) const SILENCE_DBFS: f32 = -100.0;
pub(crate) const NORMAL_MEAN_DB_THRESHOLD: f32 = -42.0;
pub(crate) const NORMAL_MAX_DB_FLOOR: f32 = -18.0;
pub(crate) const CONSERVATIVE_MEAN_DB_THRESHOLD: f32 = -35.0;
pub(crate) const CONSERVATIVE_MAX_DB_FLOOR: f32 = -12.0;
pub(crate) const DEFAULT_CONSERVATIVE_TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum BargeInSensitivity {
    #[default]
    Normal,
    Conservative,
}

impl BargeInSensitivity {
    fn thresholds(self) -> BargeInThresholds {
        match self {
            Self::Normal => BargeInThresholds {
                mean_db: NORMAL_MEAN_DB_THRESHOLD,
                max_db_floor: NORMAL_MAX_DB_FLOOR,
                min_candidate_frames: 2,
            },
            Self::Conservative => BargeInThresholds {
                mean_db: CONSERVATIVE_MEAN_DB_THRESHOLD,
                max_db_floor: CONSERVATIVE_MAX_DB_FLOOR,
                min_candidate_frames: 3,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PcmLevels {
    pub mean_db: f32,
    pub max_db: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct BargeInThresholds {
    pub mean_db: f32,
    pub max_db_floor: f32,
    pub min_candidate_frames: u8,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct LiveBargeInCut {
    pub levels: PcmLevels,
    pub sensitivity: BargeInSensitivity,
    pub candidate_frames: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProcessingBargeInDecision {
    AbortAgent,
    DeferPrompt(String),
    IgnoreNoise,
}

pub(crate) trait BargeInPlayerStop: Send + Sync {
    fn stop(&self) -> Result<()>;
}

impl BargeInPlayerStop for songbird::tracks::TrackHandle {
    fn stop(&self) -> Result<()> {
        songbird::tracks::TrackHandle::stop(self)
            .map_err(|error| anyhow!("failed to stop songbird playback track: {error}"))
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LiveBargeInMonitor {
    sensitivity: BargeInSensitivity,
    consecutive_candidate_frames: u8,
    triggered: bool,
}

impl LiveBargeInMonitor {
    pub(crate) fn new(sensitivity: BargeInSensitivity) -> Self {
        Self {
            sensitivity,
            consecutive_candidate_frames: 0,
            triggered: false,
        }
    }

    pub(crate) fn sensitivity(&self) -> BargeInSensitivity {
        self.sensitivity
    }

    pub(crate) fn set_sensitivity(&mut self, sensitivity: BargeInSensitivity) {
        if self.sensitivity != sensitivity {
            self.consecutive_candidate_frames = 0;
        }
        self.sensitivity = sensitivity;
    }

    pub(crate) fn observe_pcm(
        &mut self,
        pcm16_stereo: &[u8],
        player: &dyn BargeInPlayerStop,
        cancellation: &CancellationToken,
    ) -> Result<Option<LiveBargeInCut>> {
        let levels = pcm16_stereo_levels_struct(pcm16_stereo);
        if !is_barge_in_candidate(levels, self.sensitivity) {
            self.consecutive_candidate_frames = 0;
            return Ok(None);
        }

        self.consecutive_candidate_frames = self.consecutive_candidate_frames.saturating_add(1);
        let thresholds = self.sensitivity.thresholds();
        if self.triggered || self.consecutive_candidate_frames < thresholds.min_candidate_frames {
            return Ok(None);
        }

        player.stop()?;
        cancellation.cancel();
        self.triggered = true;

        Ok(Some(LiveBargeInCut {
            levels,
            sensitivity: self.sensitivity,
            candidate_frames: self.consecutive_candidate_frames,
        }))
    }

    pub(crate) fn reset_after_playback_start(&mut self) {
        self.consecutive_candidate_frames = 0;
        self.triggered = false;
    }
}

#[derive(Debug, Clone)]
pub(crate) struct BargeInSensitivityState {
    sensitivity: BargeInSensitivity,
    default_sensitivity: BargeInSensitivity,
    conservative_since: Option<Instant>,
    conservative_ttl: Duration,
}

impl BargeInSensitivityState {
    pub(crate) fn new(default_sensitivity: BargeInSensitivity, conservative_ttl: Duration) -> Self {
        let conservative_since =
            (default_sensitivity == BargeInSensitivity::Conservative).then_some(Instant::now());
        Self {
            sensitivity: default_sensitivity,
            default_sensitivity,
            conservative_since,
            conservative_ttl,
        }
    }

    pub(crate) fn sensitivity(&self) -> BargeInSensitivity {
        self.sensitivity
    }

    pub(crate) fn set_sensitivity(&mut self, sensitivity: BargeInSensitivity, now: Instant) {
        self.sensitivity = sensitivity;
        self.conservative_since = (sensitivity == BargeInSensitivity::Conservative).then_some(now);
    }

    pub(crate) fn apply_voice_command(
        &mut self,
        transcript: &str,
        now: Instant,
    ) -> Option<BargeInSensitivity> {
        let sensitivity = parse_sensitivity_command(transcript)?;
        self.set_sensitivity(sensitivity, now);
        Some(sensitivity)
    }

    pub(crate) fn expire_conservative_if_needed(&mut self, now: Instant) -> bool {
        if self.sensitivity != BargeInSensitivity::Conservative {
            return false;
        }

        let Some(since) = self.conservative_since else {
            self.set_sensitivity(self.default_sensitivity, now);
            return true;
        };

        if now.duration_since(since) < self.conservative_ttl {
            return false;
        }

        self.set_sensitivity(self.default_sensitivity, now);
        true
    }
}

impl Default for BargeInSensitivityState {
    fn default() -> Self {
        Self::new(BargeInSensitivity::Normal, DEFAULT_CONSERVATIVE_TTL)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DeferredBargeInBuffer {
    prompt: String,
    turn_count: usize,
    separator: &'static str,
}

impl Default for DeferredBargeInBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl DeferredBargeInBuffer {
    pub(crate) fn new() -> Self {
        Self {
            prompt: String::new(),
            turn_count: 0,
            separator: "\n\n---\n\n",
        }
    }

    pub(crate) fn push_transcript(&mut self, transcript: &str) -> bool {
        let cleaned = cleaned_transcript_for_prompt(transcript);
        if cleaned.is_empty() || is_repeated_noise_transcript(&cleaned) {
            return false;
        }
        self.push_cleaned_transcript(cleaned);
        true
    }

    pub(crate) fn verify_processing_barge_in_after_stt(
        &mut self,
        transcript: &str,
    ) -> ProcessingBargeInDecision {
        match classify_processing_barge_in_transcript(transcript) {
            ProcessingBargeInDecision::AbortAgent => ProcessingBargeInDecision::AbortAgent,
            ProcessingBargeInDecision::IgnoreNoise => ProcessingBargeInDecision::IgnoreNoise,
            ProcessingBargeInDecision::DeferPrompt(cleaned) => {
                self.push_cleaned_transcript(cleaned.clone());
                ProcessingBargeInDecision::DeferPrompt(cleaned)
            }
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.turn_count
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.prompt.is_empty()
    }

    pub(crate) fn drain_prompt(&mut self) -> Option<String> {
        if self.prompt.is_empty() {
            return None;
        }

        self.turn_count = 0;
        Some(std::mem::take(&mut self.prompt))
    }

    pub(crate) fn acknowledgement_before_drain<'a>(
        &self,
        enabled: bool,
        text: &'a str,
    ) -> Option<&'a str> {
        let text = text.trim();
        if enabled && !self.is_empty() && !text.is_empty() {
            Some(text)
        } else {
            None
        }
    }

    fn push_cleaned_transcript(&mut self, cleaned: String) {
        if !self.prompt.is_empty() {
            self.prompt.push_str(self.separator);
        }
        self.prompt.push_str(&cleaned);
        self.turn_count += 1;
    }
}

pub(crate) async fn run_sensitivity_ttl_reset(
    state: Arc<RwLock<BargeInSensitivityState>>,
    shutdown: CancellationToken,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = interval.tick() => {
                state.write().await.expire_conservative_if_needed(Instant::now());
            }
        }
    }
}

pub(crate) fn pcm16_stereo_levels(buf: &[u8]) -> (f32, f32) {
    let levels = pcm16_stereo_levels_struct(buf);
    (levels.mean_db, levels.max_db)
}

pub(crate) fn pcm16_stereo_levels_struct(buf: &[u8]) -> PcmLevels {
    // Songbird delivers stereo PCM as interleaved i16 samples; barge-in only
    // needs aggregate loudness, so both channels are intentionally measured
    // together instead of split into per-channel levels.
    let mut sample_count = 0_u64;
    let mut square_sum = 0.0_f64;
    let mut max_abs = 0.0_f32;

    for chunk in buf.chunks_exact(2) {
        let sample = i16::from_le_bytes([chunk[0], chunk[1]]) as f32;
        let abs = sample.abs();
        max_abs = max_abs.max(abs);
        square_sum += f64::from(sample * sample);
        sample_count += 1;
    }

    if sample_count == 0 || max_abs <= 0.0 {
        return PcmLevels {
            mean_db: SILENCE_DBFS,
            max_db: SILENCE_DBFS,
        };
    }

    let rms = (square_sum / sample_count as f64).sqrt() as f32;
    PcmLevels {
        mean_db: amplitude_to_dbfs(rms),
        max_db: amplitude_to_dbfs(max_abs),
    }
}

pub(crate) fn is_barge_in_candidate(levels: PcmLevels, sensitivity: BargeInSensitivity) -> bool {
    let thresholds = sensitivity.thresholds();
    levels.mean_db >= thresholds.mean_db && levels.max_db >= thresholds.max_db_floor
}

pub(crate) fn is_explicit_barge_in_transcript(transcript: &str) -> bool {
    let normalized = normalize_transcript(transcript);
    if normalized.is_empty() || is_repeated_noise_normalized(&normalized) {
        return false;
    }
    if contains_negative_stop_context(&normalized) {
        return false;
    }

    const EXACT_COMMANDS: &[&str] = &[
        "stop",
        "스톱",
        "멈춰",
        "멈춰줘",
        "그만",
        "그만해",
        "그만해줘",
        "중단",
        "중단해",
        "취소",
        "취소해",
        "정지",
        "정지해",
        "잠깐",
        "잠깐만",
    ];
    if EXACT_COMMANDS.iter().any(|phrase| normalized == *phrase) {
        return true;
    }

    const CONTAINED_COMMANDS: &[&str] = &[
        "대답그만",
        "말그만",
        "작업중단",
        "응답중단",
        "생성중단",
        "이거취소",
        "지금멈춰",
        "바로멈춰",
        "제발멈춰",
        "stopnow",
    ];
    normalized.chars().count() <= 24
        && CONTAINED_COMMANDS
            .iter()
            .any(|phrase| normalized.contains(phrase))
}

pub(crate) fn is_repeated_noise_transcript(transcript: &str) -> bool {
    let normalized = normalize_transcript(transcript);
    normalized.is_empty() || is_repeated_noise_normalized(&normalized)
}

pub(crate) fn classify_processing_barge_in_transcript(
    transcript: &str,
) -> ProcessingBargeInDecision {
    if is_explicit_barge_in_transcript(transcript) {
        return ProcessingBargeInDecision::AbortAgent;
    }
    let cleaned = cleaned_transcript_for_prompt(transcript);
    if cleaned.is_empty() || is_repeated_noise_transcript(&cleaned) {
        return ProcessingBargeInDecision::IgnoreNoise;
    }
    ProcessingBargeInDecision::DeferPrompt(cleaned)
}

pub(crate) fn parse_sensitivity_command(transcript: &str) -> Option<BargeInSensitivity> {
    let normalized = normalize_transcript(transcript);
    if normalized.is_empty() {
        return None;
    }

    let conservative = [
        "외부보수모드",
        "보수모드",
        "보수감도",
        "보수적으로",
        "감도낮춰",
        "덜민감하게",
        "conservative",
    ];
    if conservative
        .iter()
        .any(|phrase| normalized.contains(phrase))
    {
        return Some(BargeInSensitivity::Conservative);
    }

    let normal = [
        "기본감도",
        "일반감도",
        "일반모드",
        "기본모드",
        "감도기본",
        "평소감도",
        "normal",
    ];
    if normal.iter().any(|phrase| normalized.contains(phrase)) {
        return Some(BargeInSensitivity::Normal);
    }

    None
}

fn amplitude_to_dbfs(amplitude: f32) -> f32 {
    if amplitude <= 0.0 {
        SILENCE_DBFS
    } else {
        (20.0 * (amplitude / 32768.0).log10()).max(SILENCE_DBFS)
    }
}

fn cleaned_transcript_for_prompt(transcript: &str) -> String {
    transcript.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn normalize_transcript(transcript: &str) -> String {
    transcript
        .to_lowercase()
        .chars()
        .filter(|ch| ch.is_alphanumeric() || is_korean(*ch))
        .collect()
}

fn is_korean(ch: char) -> bool {
    matches!(
        ch as u32,
        0x1100..=0x11FF | 0x3130..=0x318F | 0xAC00..=0xD7AF
    )
}

fn contains_negative_stop_context(normalized: &str) -> bool {
    const NEGATIVE_CONTEXTS: &[&str] = &[
        "멈추지마",
        "그만두지마",
        "중단하지마",
        "취소하지마",
        "정지하지마",
        "스톱하지마",
        "dontstop",
        "donotstop",
    ];
    NEGATIVE_CONTEXTS
        .iter()
        .any(|phrase| normalized.contains(phrase))
}

fn is_repeated_noise_normalized(normalized: &str) -> bool {
    let char_count = normalized.chars().count();
    if char_count <= 1 {
        return true;
    }

    const NOISE_PHRASES: &[&str] = &[
        "구독좋아요",
        "좋아요구독",
        "시청해주셔서감사합니다",
        "자막제공",
        "자막뉴스",
        "mbc뉴스",
        "kbs뉴스",
        "sbs뉴스",
        "감사합니다",
    ];
    if char_count <= 18
        && NOISE_PHRASES
            .iter()
            .any(|phrase| normalized.contains(phrase))
    {
        return true;
    }

    let chars: Vec<char> = normalized.chars().collect();
    let unique_count = {
        let mut unique = Vec::new();
        for ch in &chars {
            if !unique.contains(ch) {
                unique.push(*ch);
            }
        }
        unique.len()
    };
    if char_count >= 4 && unique_count <= 2 {
        return true;
    }

    for unit_len in 1..=(char_count / 2) {
        if char_count % unit_len != 0 {
            continue;
        }
        let unit: String = chars.iter().take(unit_len).collect();
        if unit.chars().all(|ch| ch == 'ㅋ' || ch == 'ㅎ') {
            return true;
        }
        let repeated = unit.repeat(char_count / unit_len);
        if repeated == normalized && char_count / unit_len >= 3 {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct MockPlayer {
        stops: AtomicUsize,
    }

    impl BargeInPlayerStop for MockPlayer {
        fn stop(&self) -> Result<()> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn stereo_pcm(samples: &[i16]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(samples.len() * 4);
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn pcm16_stereo_levels_reports_silence() {
        let (mean_db, max_db) = pcm16_stereo_levels(&stereo_pcm(&[0, 0, 0, 0]));

        assert_eq!(mean_db, SILENCE_DBFS);
        assert_eq!(max_db, SILENCE_DBFS);
    }

    #[test]
    fn pcm16_stereo_levels_reports_rms_and_peak_dbfs() {
        let (mean_db, max_db) = pcm16_stereo_levels(&stereo_pcm(&[16_384, -16_384]));

        assert!((-6.1..=-5.9).contains(&mean_db), "mean_db={mean_db}");
        assert!((-6.1..=-5.9).contains(&max_db), "max_db={max_db}");
    }

    #[test]
    fn db_threshold_respects_sensitivity_modes() {
        let normal_only = PcmLevels {
            mean_db: -40.0,
            max_db: -16.0,
        };

        assert!(is_barge_in_candidate(
            normal_only,
            BargeInSensitivity::Normal
        ));
        assert!(!is_barge_in_candidate(
            normal_only,
            BargeInSensitivity::Conservative
        ));
    }

    #[test]
    fn live_monitor_stops_player_and_cancels_once_after_confirmed_frames() {
        let player = MockPlayer::default();
        let cancellation = CancellationToken::new();
        let mut monitor = LiveBargeInMonitor::new(BargeInSensitivity::Normal);
        let loud = stereo_pcm(&[16_384, -16_384, 16_384, -16_384]);

        assert!(
            monitor
                .observe_pcm(&loud, &player, &cancellation)
                .unwrap()
                .is_none()
        );
        let cut = monitor
            .observe_pcm(&loud, &player, &cancellation)
            .unwrap()
            .unwrap();
        assert_eq!(cut.candidate_frames, 2);
        assert!(cancellation.is_cancelled());
        assert_eq!(player.stops.load(Ordering::SeqCst), 1);

        assert!(
            monitor
                .observe_pcm(&loud, &player, &cancellation)
                .unwrap()
                .is_none()
        );
        assert_eq!(player.stops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn explicit_stop_transcript_classifier_accepts_korean_stop_commands() {
        for transcript in [
            "멈춰",
            "그만해줘",
            "작업 중단",
            "지금 바로 멈춰",
            "stop now",
        ] {
            assert!(
                is_explicit_barge_in_transcript(transcript),
                "expected explicit stop: {transcript}"
            );
        }
    }

    #[test]
    fn explicit_stop_transcript_classifier_rejects_noise_and_negative_context() {
        for transcript in ["ㅋㅋㅋㅋ", "구독 좋아요", "멈추지 마", "그만큼 진행해"]
        {
            assert!(
                !is_explicit_barge_in_transcript(transcript),
                "expected non-stop: {transcript}"
            );
        }
    }

    #[test]
    fn processing_transcript_defers_non_stop_and_ignores_noise() {
        assert_eq!(
            classify_processing_barge_in_transcript("다음에는 로그도 같이 봐줘"),
            ProcessingBargeInDecision::DeferPrompt("다음에는 로그도 같이 봐줘".to_string())
        );
        assert_eq!(
            classify_processing_barge_in_transcript("멈춰"),
            ProcessingBargeInDecision::AbortAgent
        );
        assert_eq!(
            classify_processing_barge_in_transcript("ㅎㅎㅎㅎ"),
            ProcessingBargeInDecision::IgnoreNoise
        );
    }

    #[test]
    fn deferred_buffer_merges_multiple_transcripts_into_single_prompt() {
        let mut buffer = DeferredBargeInBuffer::new();

        assert!(buffer.push_transcript("첫 번째 요청"));
        assert!(buffer.push_transcript("두 번째 요청"));
        assert!(buffer.push_transcript("세 번째 요청"));

        let prompt = buffer.drain_prompt().unwrap();
        assert_eq!(buffer.len(), 0);
        assert_eq!(
            prompt,
            "첫 번째 요청\n\n---\n\n두 번째 요청\n\n---\n\n세 번째 요청"
        );
        assert!(buffer.drain_prompt().is_none());
    }

    #[test]
    fn processing_verification_updates_defer_buffer_but_not_explicit_stop() {
        let mut buffer = DeferredBargeInBuffer::new();

        assert_eq!(
            buffer.verify_processing_barge_in_after_stt("이 내용도 반영해줘"),
            ProcessingBargeInDecision::DeferPrompt("이 내용도 반영해줘".to_string())
        );
        assert_eq!(buffer.len(), 1);
        assert_eq!(
            buffer.verify_processing_barge_in_after_stt("멈춰"),
            ProcessingBargeInDecision::AbortAgent
        );
        assert_eq!(buffer.len(), 1);
        assert_eq!(buffer.drain_prompt().unwrap(), "이 내용도 반영해줘");
    }

    #[test]
    fn acknowledgement_before_drain_obeys_toggle_and_buffer_state() {
        let mut buffer = DeferredBargeInBuffer::new();
        assert_eq!(
            buffer.acknowledgement_before_drain(true, "정리해서 볼게요."),
            None
        );

        buffer.push_transcript("추가 요청");
        assert_eq!(
            buffer.acknowledgement_before_drain(true, "정리해서 볼게요."),
            Some("정리해서 볼게요.")
        );
        assert_eq!(
            buffer.acknowledgement_before_drain(false, "정리해서 볼게요."),
            None
        );
    }

    #[test]
    fn conservative_live_monitor_requires_three_confirming_frames() {
        let player = MockPlayer::default();
        let cancellation = CancellationToken::new();
        let mut monitor = LiveBargeInMonitor::new(BargeInSensitivity::Conservative);
        let loud = stereo_pcm(&[16_384, -16_384, 16_384, -16_384]);

        assert!(
            monitor
                .observe_pcm(&loud, &player, &cancellation)
                .unwrap()
                .is_none()
        );
        assert!(
            monitor
                .observe_pcm(&loud, &player, &cancellation)
                .unwrap()
                .is_none()
        );
        let cut = monitor
            .observe_pcm(&loud, &player, &cancellation)
            .unwrap()
            .unwrap();

        assert_eq!(cut.candidate_frames, 3);
        assert_eq!(player.stops.load(Ordering::SeqCst), 1);
        assert!(cancellation.is_cancelled());
    }

    #[test]
    fn sensitivity_voice_commands_toggle_mode() {
        assert_eq!(
            parse_sensitivity_command("외부 보수 모드로 바꿔"),
            Some(BargeInSensitivity::Conservative)
        );
        assert_eq!(
            parse_sensitivity_command("기본 감도로 돌아가"),
            Some(BargeInSensitivity::Normal)
        );
        assert_eq!(parse_sensitivity_command("그냥 계속해"), None);
    }

    #[test]
    fn conservative_mode_expires_after_ttl() {
        let now = Instant::now();
        let mut state =
            BargeInSensitivityState::new(BargeInSensitivity::Normal, Duration::from_secs(60));

        state.set_sensitivity(BargeInSensitivity::Conservative, now);
        assert!(!state.expire_conservative_if_needed(now + Duration::from_secs(59)));
        assert_eq!(state.sensitivity(), BargeInSensitivity::Conservative);

        assert!(state.expire_conservative_if_needed(now + Duration::from_secs(60)));
        assert_eq!(state.sensitivity(), BargeInSensitivity::Normal);
    }
}
