use super::*;

use crate::services::observability;
use crate::services::observability::events::StructuredEvent;
use crate::voice::receiver::VoiceReceiverConfig;
use crate::voice::{VoiceReceiveHook, VoiceReceiver};
use serde::Serialize;
use serde_json::Value;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use tempfile::TempDir;
use tokio::time::{Duration, sleep};

const SOURCE_CHANNEL_ID: u64 = 3_801_001;
const TARGET_CHANNEL_ID: u64 = 3_801_002;
const GUILD_ID: u64 = 3_801_003;
const USER_ID: u64 = 3_801_004;
const SSRC: u32 = 38_001;

#[derive(Debug, Serialize)]
struct VoicePcmHarnessReport {
    schema_version: u8,
    agent_mode: &'static str,
    live_discord_media_transport_covered: bool,
    receive_boundary: &'static str,
    test_identity: String,
    report_path: Option<String>,
    scenarios: Vec<ScenarioReport>,
    negative_dependency_scenarios: Vec<ScenarioReport>,
}

#[derive(Debug, Serialize)]
struct ScenarioReport {
    scenario_id: &'static str,
    status: &'static str,
    utterance_id: Option<String>,
    channel_id: u64,
    target_channel_id: Option<u64>,
    test_identity: String,
    transcript: Option<String>,
    stt_mode: Option<String>,
    stt_latency_ms: Option<u64>,
    routing_decision: Option<String>,
    foreground_decision: Option<String>,
    tts_observation: Option<String>,
    playback_observation: Option<String>,
    voice_latency_turn: Option<Value>,
    voice_flight_events: Vec<Value>,
    timing_stages: TimingStages,
    raw_failure_reasons: Vec<String>,
}

#[derive(Debug, Default, Serialize)]
struct TimingStages {
    pcm_feed_ms: u128,
    receive_flush_ms: u128,
    async_route_wait_ms: u128,
    foreground_ms: u128,
    harness_metric_finalize_ms: u128,
}

struct RecordingVoiceHook {
    inner: DiscordVoiceBargeInHook,
    completions: StdMutex<Vec<CompletedUtterance>>,
}

impl RecordingVoiceHook {
    fn new(inner: DiscordVoiceBargeInHook) -> Self {
        Self {
            inner,
            completions: StdMutex::new(Vec::new()),
        }
    }
}

impl VoiceReceiveHook for RecordingVoiceHook {
    fn observe_pcm(&self, control_channel_id: u64, user_id: u64, samples: &[i16]) {
        self.inner.observe_pcm(control_channel_id, user_id, samples);
    }

    fn utterance_completed(&self, control_channel_id: u64, utterance: &CompletedUtterance) {
        self.completions
            .lock()
            .expect("voice PCM harness completions lock")
            .push(utterance.clone());
        self.inner
            .utterance_completed(control_channel_id, utterance);
    }
}

struct VoicePcmHarness {
    _temp: TempDir,
    _root_guard: AgentDeskRootGuard,
    runtime: Arc<VoiceBargeInRuntime>,
    shared: Arc<SharedData>,
    receiver: VoiceReceiver,
    hook: Arc<RecordingVoiceHook>,
    source_channel: ChannelId,
    target_channel: ChannelId,
    message_seq: AtomicU64,
}

impl VoicePcmHarness {
    async fn new(transcripts: &[&str]) -> Self {
        let temp = tempfile::tempdir().expect("create voice PCM harness tempdir");
        let root_guard = AgentDeskRootGuard::set(temp.path());
        let shims = install_command_shims(temp.path(), transcripts);
        let voice_config = harness_voice_config(temp.path(), &shims);
        let runtime = Arc::new(VoiceBargeInRuntime::from_voice_config(&voice_config));
        runtime
            .test_state
            .force_synth_success
            .store(true, Ordering::Relaxed);
        runtime.register_voice_context(ChannelId::new(SOURCE_CHANNEL_ID), GuildId::new(GUILD_ID));
        runtime
            .config_cache
            .seed(Instant::now(), Arc::new(harness_config(&voice_config)));

        let shared = voice_handoff_shared_for_tests();
        let inner_hook =
            DiscordVoiceBargeInHook::new(runtime.clone(), shared.clone(), ProviderKind::Claude);
        let hook = Arc::new(RecordingVoiceHook::new(inner_hook));
        let mut receiver_config = VoiceReceiverConfig::from_voice_config(&voice_config);
        receiver_config.keep_recordings = true;
        #[cfg(test)]
        {
            receiver_config.blocking_io_delay = Duration::ZERO;
        }
        let receiver = VoiceReceiver::new_with_hook(receiver_config, Some(hook.clone()));
        receiver
            .register_speaking_for_control_channel(SOURCE_CHANNEL_ID, SSRC, USER_ID)
            .await;

        Self {
            _temp: temp,
            _root_guard: root_guard,
            runtime,
            shared,
            receiver,
            hook,
            source_channel: ChannelId::new(SOURCE_CHANNEL_ID),
            target_channel: ChannelId::new(TARGET_CHANNEL_ID),
            message_seq: AtomicU64::new(3_801_100),
        }
    }

    fn next_message_id(&self) -> MessageId {
        MessageId::new(self.message_seq.fetch_add(1, Ordering::Relaxed))
    }

    fn queue_turn_start(&self, message_id: MessageId) {
        self.runtime
            .test_state
            .turn_start_outcomes
            .lock()
            .expect("voice PCM harness turn start outcomes lock")
            .push_back(Ok(VoiceBackgroundStartOutcome {
                turn_id: format!("voice-announce:{}", message_id.get()),
                driver_kind: VoiceBackgroundDriverKind::AnnounceBotTranscript,
                message_id: Some(message_id),
            }));
    }

    fn queue_background_handoff(&self, message_id: MessageId) {
        self.runtime
            .test_state
            .background_handoff_outcomes
            .lock()
            .expect("voice PCM harness handoff outcomes lock")
            .push_back(Ok(VoiceBackgroundStartOutcome {
                turn_id: format!("voice-announce:{}", message_id.get()),
                driver_kind: VoiceBackgroundDriverKind::AnnounceBotTranscript,
                message_id: Some(message_id),
            }));
    }

    fn queue_foreground_decision(&self, decision: VoiceForegroundDecision) {
        self.runtime
            .test_state
            .foreground_decisions
            .lock()
            .expect("voice PCM harness foreground decisions lock")
            .push_back(decision);
    }

    fn queue_background_summary(&self, summary: &str) {
        self.runtime
            .test_state
            .background_result_summaries
            .lock()
            .expect("voice PCM harness background summaries lock")
            .push_back(Some(summary.to_string()));
    }

    fn clear_play_requests(&self) {
        self.runtime
            .test_state
            .play_requests
            .lock()
            .expect("voice PCM harness play requests lock")
            .clear();
    }

    async fn feed_pcm_turn(&self, loud: bool) -> (CompletedUtterance, TimingStages) {
        let mut timings = TimingStages::default();
        let frame = if loud {
            vec![16_384, -16_384, 16_384, -16_384].repeat(480)
        } else {
            vec![9_000, -9_000, 9_000, -9_000].repeat(480)
        };

        let feed_started = Instant::now();
        self.receiver
            .register_speaking_for_control_channel(SOURCE_CHANNEL_ID, SSRC, USER_ID)
            .await;
        self.receiver
            .queue_pcm_for_control_channel(SOURCE_CHANNEL_ID, SSRC, &frame)
            .await
            .expect("queue first PCM frame through receiver");
        self.receiver
            .queue_pcm_for_control_channel(SOURCE_CHANNEL_ID, SSRC, &frame)
            .await
            .expect("queue second PCM frame through receiver");
        timings.pcm_feed_ms = feed_started.elapsed().as_millis();

        let flush_started = Instant::now();
        let completed = self
            .receiver
            .flush_for_control_channel(SOURCE_CHANNEL_ID)
            .await;
        timings.receive_flush_ms = flush_started.elapsed().as_millis();
        let utterance = completed
            .into_iter()
            .next()
            .expect("PCM turn should complete one utterance");

        let recorded = self
            .hook
            .completions
            .lock()
            .expect("voice PCM harness completions lock")
            .iter()
            .any(|seen| seen.utterance_id == utterance.utterance_id);
        assert!(recorded, "recording hook must observe completed utterance");
        (utterance, timings)
    }

    async fn reset_scenario_state(&self) {
        self.clear_play_requests();
        self.runtime.clear_playback(self.source_channel);
        self.runtime
            .channels
            .spoken_result_playbacks
            .remove(&SOURCE_CHANNEL_ID);
        self.runtime.channels.playback_finished(self.source_channel);
        self.runtime
            .channels
            .active_voice_routes
            .remove(&SOURCE_CHANNEL_ID);
        self.runtime
            .channels
            .deferred_buffers
            .remove(&SOURCE_CHANNEL_ID);
        self.runtime.cancel_inflight_foreground_calls(
            self.source_channel,
            "voice_pcm_harness_scenario_reset",
        );
        let _ = self.shared.mailbox(self.source_channel).hard_stop().await;
        let _ = self.shared.mailbox(self.target_channel).hard_stop().await;
    }
}

#[cfg(unix)]
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn voice_pcm_harness_unattended_e2e() {
    let _guard = observability::test_runtime_lock();
    observability::reset_for_tests();
    observability::init_observability(None);
    let _env_lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());

    let transcripts = [
        "오늘 일정 알려줘",
        "멈춰",
        "로그 확인하고 오래 걸리면 요약해줘",
        "언어를 영어로 바꿔줘",
        "What's on my schedule today?",
    ];
    let harness = VoicePcmHarness::new(&transcripts).await;
    let mut report = VoicePcmHarnessReport {
        schema_version: 1,
        agent_mode: "controlled",
        live_discord_media_transport_covered: false,
        receive_boundary: "VoiceReceiver::queue_pcm_for_control_channel -> DiscordVoiceBargeInHook",
        test_identity: "voice-pcm-harness/local-unattended".to_string(),
        report_path: report_path().map(|path| path.display().to_string()),
        scenarios: Vec::new(),
        negative_dependency_scenarios: Vec::new(),
    };

    report.scenarios.push(run_short_korean_turn(&harness).await);
    report.scenarios.push(run_barge_in_turn(&harness).await);
    report
        .scenarios
        .push(run_background_handoff_summary_turn(&harness).await);
    report
        .scenarios
        .push(run_spoken_language_command(&harness).await);
    report
        .scenarios
        .push(run_language_switch_turn(&harness).await);
    report
        .negative_dependency_scenarios
        .push(run_missing_ffmpeg_negative().await);

    write_report(&report);
    let failures = report
        .scenarios
        .iter()
        .chain(report.negative_dependency_scenarios.iter())
        .filter(|scenario| scenario.status != "passed")
        .map(|scenario| {
            format!(
                "{}: {}",
                scenario.scenario_id,
                scenario.raw_failure_reasons.join("; ")
            )
        })
        .collect::<Vec<_>>();
    assert!(
        failures.is_empty(),
        "voice PCM harness failures:\n{}",
        failures.join("\n")
    );
}

#[cfg(not(unix))]
#[test]
fn voice_pcm_harness_unattended_e2e() {
    eprintln!("voice PCM harness uses local POSIX command shims and is skipped on non-Unix");
}

async fn run_short_korean_turn(harness: &VoicePcmHarness) -> ScenarioReport {
    let scenario_id = "normal-short-ko";
    let mut failures = Vec::new();
    harness.clear_play_requests();
    let message_id = harness.next_message_id();
    harness.queue_turn_start(message_id);
    harness.queue_foreground_decision(VoiceForegroundDecision::Speak(
        "오늘 일정 확인했어요.".to_string(),
    ));

    let (utterance, mut timings) = harness.feed_pcm_turn(false).await;
    let wait_started = Instant::now();
    let start = wait_for_turn_start(harness, &utterance.utterance_id).await;
    timings.async_route_wait_ms = wait_started.elapsed().as_millis();
    let announcement = wait_for_announcement(message_id).await;
    if start.is_none() {
        failures.push("voice turn start was not recorded from PCM utterance".to_string());
    }
    let foreground_started = Instant::now();
    if let Some(announcement) = announcement.as_ref() {
        if !harness
            .runtime
            .try_handle_voice_transcript_announcement(
                &harness.shared,
                harness.source_channel,
                announcement,
            )
            .await
        {
            failures.push("foreground handler did not consume canonical announcement".to_string());
        }
    } else {
        failures.push("canonical voice announcement metadata was not cached".to_string());
    }
    timings.foreground_ms = foreground_started.elapsed().as_millis();

    expect_playback_request(
        harness,
        SOURCE_CHANNEL_ID,
        "voice barge-in acknowledgement",
        &mut failures,
    );
    let latency = finalize_latency_turn(harness.source_channel, 42, 18, &mut timings);
    assert_latency_utterance(&latency, &utterance.utterance_id, &mut failures);
    let flight_events = flight_events_for(&utterance.utterance_id);
    expect_route(&flight_events, "queued", &mut failures);
    expect_route(&flight_events, "foreground_speak", &mut failures);

    scenario_report(
        scenario_id,
        utterance,
        announcement,
        Some("오늘 일정 알려줘"),
        "foreground_speak",
        Some("speak"),
        Some("progress_tts_synthesized"),
        Some("playback_request_observed"),
        None,
        latency,
        flight_events,
        timings,
        failures,
    )
}

async fn run_barge_in_turn(harness: &VoicePcmHarness) -> ScenarioReport {
    let scenario_id = "barge-in-while-tts-active";
    let mut failures = Vec::new();
    harness.clear_play_requests();
    install_active_voice_route(
        &harness.runtime,
        harness.source_channel,
        harness.target_channel,
    );
    let player = Arc::new(MockPlayer::default());
    let playback_cancel = CancellationToken::new();
    harness.runtime.reset_after_playback_start(
        harness.source_channel,
        player.clone(),
        playback_cancel.clone(),
    );
    let active_token = Arc::new(crate::services::provider::CancelToken::new());
    assert!(
        harness
            .shared
            .mailbox(harness.target_channel)
            .try_start_turn(
                active_token.clone(),
                serenity::UserId::new(USER_ID),
                MessageId::new(3_801_800),
            )
            .await
    );

    let (utterance, mut timings) = harness.feed_pcm_turn(true).await;
    let wait_started = Instant::now();
    let stop_event = wait_for_flight_route(&utterance.utterance_id, "explicit_stop").await;
    timings.async_route_wait_ms = wait_started.elapsed().as_millis();

    if player.stops.load(Ordering::SeqCst) == 0 {
        failures.push("live PCM barge-in did not stop the registered playback".to_string());
    }
    if !playback_cancel.is_cancelled() {
        failures.push("live PCM barge-in did not cancel playback token".to_string());
    }
    if !active_token.cancelled.load(Ordering::Relaxed) {
        failures.push("spoken stop did not cancel the active routed background turn".to_string());
    }
    if stop_event.is_none() {
        failures.push("explicit-stop voice flight event was not emitted".to_string());
    }
    let latency = finalize_latency_turn(harness.source_channel, 0, 0, &mut timings);
    assert_latency_utterance(&latency, &utterance.utterance_id, &mut failures);
    let flight_events = flight_events_for(&utterance.utterance_id);
    harness.reset_scenario_state().await;

    scenario_report(
        scenario_id,
        utterance,
        None,
        Some("멈춰"),
        "explicit_stop",
        Some("barge_in_cancel"),
        Some("tts_cancelled"),
        Some("playback_cancelled"),
        Some(TARGET_CHANNEL_ID),
        latency,
        flight_events,
        timings,
        failures,
    )
}

async fn run_background_handoff_summary_turn(harness: &VoicePcmHarness) -> ScenarioReport {
    let scenario_id = "long-answer-background-handoff-summary";
    let mut failures = Vec::new();
    harness.clear_play_requests();
    let start_message_id = harness.next_message_id();
    let handoff_message_id = harness.next_message_id();
    harness.queue_turn_start(start_message_id);
    harness.queue_foreground_decision(VoiceForegroundDecision::HandoffBackground(
        "로그 확인 후 원인 요약".to_string(),
    ));
    harness.queue_background_handoff(handoff_message_id);
    harness.queue_background_summary("백그라운드 작업이 끝났고 요약을 준비했어요.");

    let (utterance, mut timings) = harness.feed_pcm_turn(false).await;
    let wait_started = Instant::now();
    let start = wait_for_turn_start(harness, &utterance.utterance_id).await;
    timings.async_route_wait_ms = wait_started.elapsed().as_millis();
    let announcement = wait_for_announcement(start_message_id).await;
    if start.is_none() {
        failures.push("voice turn start was not recorded for long-answer handoff".to_string());
    }

    let foreground_started = Instant::now();
    if let Some(announcement) = announcement.as_ref() {
        let handled = harness
            .runtime
            .try_handle_voice_transcript_announcement(
                &harness.shared,
                harness.source_channel,
                announcement,
            )
            .await;
        if !handled {
            failures.push("foreground handoff handler did not consume announcement".to_string());
        }
    } else {
        failures.push("handoff announcement metadata was not cached".to_string());
    }
    harness
        .runtime
        .speak_voice_background_completion_summary(
            &harness.shared,
            harness.source_channel,
            harness.target_channel,
            "작업 완료. 상세 로그는 텍스트 채널에 남겼습니다.",
            false,
        )
        .await;
    timings.foreground_ms = foreground_started.elapsed().as_millis();

    expect_background_start(harness, &utterance.utterance_id, &mut failures);
    expect_playback_request(
        harness,
        SOURCE_CHANNEL_ID,
        "voice barge-in acknowledgement",
        &mut failures,
    );
    expect_playback_request(
        harness,
        SOURCE_CHANNEL_ID,
        "voice background result summary",
        &mut failures,
    );
    let latency = finalize_latency_turn(harness.source_channel, 88, 31, &mut timings);
    assert_latency_utterance(&latency, &utterance.utterance_id, &mut failures);
    let flight_events = flight_events_for(&utterance.utterance_id);
    expect_route(&flight_events, "queued", &mut failures);
    expect_route(&flight_events, "background_handoff", &mut failures);

    scenario_report(
        scenario_id,
        utterance,
        announcement,
        Some("로그 확인하고 오래 걸리면 요약해줘"),
        "background_handoff",
        Some("handoff_background"),
        Some("handoff_ack_and_summary_tts"),
        Some("playback_request_observed"),
        Some(TARGET_CHANNEL_ID),
        latency,
        flight_events,
        timings,
        failures,
    )
}

async fn run_spoken_language_command(harness: &VoicePcmHarness) -> ScenarioReport {
    let scenario_id = "spoken-command-language-route-change";
    let mut failures = Vec::new();
    harness.clear_play_requests();
    let (utterance, mut timings) = harness.feed_pcm_turn(false).await;
    let wait_started = Instant::now();
    let snapshot = wait_for_language(harness, "en").await;
    timings.async_route_wait_ms = wait_started.elapsed().as_millis();
    if snapshot.is_none() {
        failures
            .push("spoken language command did not switch runtime STT language to en".to_string());
    }
    let flight_events = flight_events_for(&utterance.utterance_id);
    if !flight_events.is_empty() {
        failures.push(
            "dispatcher-only language command unexpectedly emitted turn flight events".to_string(),
        );
    }
    crate::voice::metrics::discard(harness.source_channel.get());

    scenario_report(
        scenario_id,
        utterance,
        None,
        Some("언어를 영어로 바꿔줘"),
        "dispatcher_command",
        Some("language_changed:en"),
        Some("not_applicable"),
        Some("no_turn_playback"),
        None,
        None,
        flight_events,
        timings,
        failures,
    )
}

async fn run_language_switch_turn(harness: &VoicePcmHarness) -> ScenarioReport {
    let scenario_id = "language-switch-english-turn";
    let mut failures = Vec::new();
    harness.clear_play_requests();
    let message_id = harness.next_message_id();
    harness.queue_turn_start(message_id);
    harness.queue_foreground_decision(VoiceForegroundDecision::Speak(
        "I checked the schedule.".to_string(),
    ));

    let (utterance, mut timings) = harness.feed_pcm_turn(false).await;
    let wait_started = Instant::now();
    let start = wait_for_turn_start(harness, &utterance.utterance_id).await;
    timings.async_route_wait_ms = wait_started.elapsed().as_millis();
    let announcement = wait_for_announcement(message_id).await;
    if start.is_none() {
        failures.push("voice turn start was not recorded after language switch".to_string());
    }
    if announcement
        .as_ref()
        .is_some_and(|value| !value.language.to_ascii_lowercase().starts_with("en"))
    {
        failures.push("announcement language did not stay switched to English".to_string());
    }
    let foreground_started = Instant::now();
    if let Some(announcement) = announcement.as_ref() {
        let handled = harness
            .runtime
            .try_handle_voice_transcript_announcement(
                &harness.shared,
                harness.source_channel,
                announcement,
            )
            .await;
        if !handled {
            failures.push("foreground handler did not consume English announcement".to_string());
        }
    } else {
        failures.push("English voice announcement metadata was not cached".to_string());
    }
    timings.foreground_ms = foreground_started.elapsed().as_millis();

    expect_playback_request(
        harness,
        SOURCE_CHANNEL_ID,
        "voice barge-in acknowledgement",
        &mut failures,
    );
    let latency = finalize_latency_turn(harness.source_channel, 36, 21, &mut timings);
    assert_latency_utterance(&latency, &utterance.utterance_id, &mut failures);
    let flight_events = flight_events_for(&utterance.utterance_id);
    expect_route(&flight_events, "queued", &mut failures);
    expect_route(&flight_events, "foreground_speak", &mut failures);

    scenario_report(
        scenario_id,
        utterance,
        announcement,
        Some("What's on my schedule today?"),
        "foreground_speak",
        Some("speak"),
        Some("progress_tts_synthesized"),
        Some("playback_request_observed"),
        None,
        latency,
        flight_events,
        timings,
        failures,
    )
}

async fn run_missing_ffmpeg_negative() -> ScenarioReport {
    let scenario_id = "negative-missing-ffmpeg-config-shim";
    let mut failures = Vec::new();
    let temp = tempfile::tempdir().expect("create negative harness tempdir");
    let _root_guard = AgentDeskRootGuard::set(temp.path());
    let shims = install_command_shims(temp.path(), &["이 발화는 STT에 도달하면 안 됩니다"]);
    let mut voice_config = harness_voice_config(temp.path(), &shims);
    voice_config.stt.ffmpeg_command = temp
        .path()
        .join("missing-ffmpeg-shim")
        .display()
        .to_string();
    let runtime = Arc::new(VoiceBargeInRuntime::from_voice_config(&voice_config));
    runtime
        .config_cache
        .seed(Instant::now(), Arc::new(harness_config(&voice_config)));
    let shared = voice_handoff_shared_for_tests();
    let inner_hook = DiscordVoiceBargeInHook::new(runtime.clone(), shared, ProviderKind::Claude);
    let hook = Arc::new(RecordingVoiceHook::new(inner_hook));
    let mut receiver_config = VoiceReceiverConfig::from_voice_config(&voice_config);
    receiver_config.keep_recordings = true;
    #[cfg(test)]
    {
        receiver_config.blocking_io_delay = Duration::ZERO;
    }
    let receiver = VoiceReceiver::new_with_hook(receiver_config, Some(hook));
    receiver
        .register_speaking_for_control_channel(SOURCE_CHANNEL_ID + 90, SSRC + 90, USER_ID)
        .await;

    let mut timings = TimingStages::default();
    let samples = vec![8_000, -8_000, 8_000, -8_000].repeat(480);
    let feed_started = Instant::now();
    receiver
        .register_speaking_for_control_channel(SOURCE_CHANNEL_ID + 90, SSRC + 90, USER_ID)
        .await;
    receiver
        .queue_pcm_for_control_channel(SOURCE_CHANNEL_ID + 90, SSRC + 90, &samples)
        .await
        .expect("queue negative PCM frame through receiver");
    timings.pcm_feed_ms = feed_started.elapsed().as_millis();
    let flush_started = Instant::now();
    let completed = receiver
        .flush_for_control_channel(SOURCE_CHANNEL_ID + 90)
        .await;
    timings.receive_flush_ms = flush_started.elapsed().as_millis();
    let utterance = completed
        .into_iter()
        .next()
        .expect("negative PCM turn should still produce WAV utterance");

    let wait_started = Instant::now();
    sleep(Duration::from_millis(5_400)).await;
    timings.async_route_wait_ms = wait_started.elapsed().as_millis();

    let starts = runtime
        .test_state
        .turn_starts
        .lock()
        .expect("negative turn starts lock")
        .clone();
    if !starts.is_empty() {
        failures.push("missing ffmpeg shim unexpectedly allowed a voice turn start".to_string());
    }
    if !flight_events_for(&utterance.utterance_id).is_empty() {
        failures.push("missing ffmpeg shim unexpectedly emitted route flight events".to_string());
    }

    scenario_report(
        scenario_id,
        utterance,
        None,
        None,
        "dependency_failure:missing_ffmpeg_config_shim",
        None,
        Some("not_reached"),
        Some("not_reached"),
        None,
        None,
        Vec::new(),
        timings,
        failures,
    )
}

struct AgentDeskRootGuard {
    previous: Option<OsString>,
}

impl AgentDeskRootGuard {
    fn set(path: &Path) -> Self {
        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", path);
        }
        Self { previous }
    }
}

impl Drop for AgentDeskRootGuard {
    fn drop(&mut self) {
        unsafe {
            if let Some(previous) = self.previous.take() {
                std::env::set_var("AGENTDESK_ROOT_DIR", previous);
            } else {
                std::env::remove_var("AGENTDESK_ROOT_DIR");
            }
        }
    }
}

fn scenario_report(
    scenario_id: &'static str,
    utterance: CompletedUtterance,
    announcement: Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
    transcript_override: Option<&str>,
    routing_decision: &str,
    foreground_decision: Option<&str>,
    tts_observation: Option<&str>,
    playback_observation: Option<&str>,
    target_channel_id: Option<u64>,
    latency: Option<crate::voice::metrics::LatencyTurn>,
    flight_events: Vec<Value>,
    timing_stages: TimingStages,
    raw_failure_reasons: Vec<String>,
) -> ScenarioReport {
    let transcript = announcement
        .as_ref()
        .map(|value| value.transcript.clone())
        .or_else(|| transcript_override.map(str::to_string));
    let stt_mode = announcement
        .as_ref()
        .and_then(|value| value.stt_mode.clone())
        .or_else(|| stt_mode_for(&utterance.utterance_id))
        .or_else(|| transcript_override.map(|_| "file".to_string()));
    let stt_latency_ms = announcement
        .as_ref()
        .and_then(|value| value.stt_latency_ms)
        .or_else(|| stt_latency_for(&utterance.utterance_id));
    ScenarioReport {
        scenario_id,
        status: if raw_failure_reasons.is_empty() {
            "passed"
        } else {
            "failed"
        },
        utterance_id: Some(utterance.utterance_id),
        channel_id: utterance.control_channel_id.unwrap_or(SOURCE_CHANNEL_ID),
        target_channel_id,
        test_identity: format!("voice-pcm-harness/{scenario_id}"),
        transcript,
        stt_mode,
        stt_latency_ms,
        routing_decision: Some(routing_decision.to_string()),
        foreground_decision: foreground_decision.map(str::to_string),
        tts_observation: tts_observation.map(str::to_string),
        playback_observation: playback_observation.map(str::to_string),
        voice_latency_turn: latency.map(|turn| turn.to_payload()),
        voice_flight_events: flight_events,
        timing_stages,
        raw_failure_reasons,
    }
}

fn finalize_latency_turn(
    channel_id: ChannelId,
    tts_synth_ms: u64,
    first_audio_ms: u64,
    timings: &mut TimingStages,
) -> Option<crate::voice::metrics::LatencyTurn> {
    let started = Instant::now();
    crate::voice::metrics::record_agent(channel_id.get(), 7);
    let turn = crate::voice::metrics::record_tts(channel_id.get(), tts_synth_ms, first_audio_ms);
    timings.harness_metric_finalize_ms = started.elapsed().as_millis();
    turn
}

fn assert_latency_utterance(
    latency: &Option<crate::voice::metrics::LatencyTurn>,
    utterance_id: &str,
    failures: &mut Vec<String>,
) {
    match latency {
        Some(turn) if turn.utterance_id.as_deref() == Some(utterance_id) => {}
        Some(turn) => failures.push(format!(
            "voice_latency_turn utterance mismatch: got {:?}, expected {utterance_id}",
            turn.utterance_id
        )),
        None => failures.push("voice_latency_turn metric was not emitted".to_string()),
    }
}

fn expect_playback_request(
    harness: &VoicePcmHarness,
    channel_id: u64,
    context: &'static str,
    failures: &mut Vec<String>,
) {
    let plays = harness
        .runtime
        .test_state
        .play_requests
        .lock()
        .expect("voice PCM harness play requests lock")
        .clone();
    if !plays
        .iter()
        .any(|(seen_channel, seen_context)| *seen_channel == channel_id && *seen_context == context)
    {
        failures.push(format!(
            "expected TTS playback request context `{context}` on channel {channel_id}"
        ));
    }
}

fn expect_background_start(
    harness: &VoicePcmHarness,
    utterance_id: &str,
    failures: &mut Vec<String>,
) {
    let starts = harness
        .runtime
        .test_state
        .background_starts
        .lock()
        .expect("voice PCM harness background starts lock")
        .clone();
    if !starts.iter().any(|start| {
        start.utterance_id == utterance_id
            && start.driver_kind == VoiceBackgroundDriverKind::AnnounceBotTranscript
            && start.target_channel_id == harness.target_channel
    }) {
        failures.push("background handoff did not start through announce-bot driver".to_string());
    }
}

fn expect_route(events: &[Value], route: &str, failures: &mut Vec<String>) {
    if !events
        .iter()
        .any(|event| event.get("route").and_then(Value::as_str) == Some(route))
    {
        failures.push(format!("missing voice_flight_event route `{route}`"));
    }
}

async fn wait_for_turn_start(
    harness: &VoicePcmHarness,
    utterance_id: &str,
) -> Option<TestVoiceBackgroundStart> {
    wait_until(|| {
        harness
            .runtime
            .test_state
            .turn_starts
            .lock()
            .expect("voice PCM harness turn starts lock")
            .iter()
            .find(|start| start.utterance_id == utterance_id)
            .cloned()
    })
    .await
}

async fn wait_for_announcement(
    message_id: MessageId,
) -> Option<crate::voice::prompt::VoiceTranscriptAnnouncement> {
    wait_until(|| crate::voice::announce_meta::global_store().peek_clone(message_id)).await
}

async fn wait_for_language(
    harness: &VoicePcmHarness,
    language: &str,
) -> Option<VoiceRuntimeConfigSnapshot> {
    for _ in 0..150 {
        let snapshot = harness.runtime.runtime_config_snapshot().await;
        if snapshot.stt_language == language {
            return Some(snapshot);
        }
        sleep(Duration::from_millis(20)).await;
    }
    None
}

async fn wait_for_flight_route(utterance_id: &str, route: &str) -> Option<Value> {
    wait_until(|| {
        flight_events_for(utterance_id)
            .into_iter()
            .find(|event| event.get("route").and_then(Value::as_str) == Some(route))
    })
    .await
}

async fn wait_until<T, F>(mut f: F) -> Option<T>
where
    F: FnMut() -> Option<T>,
{
    for _ in 0..200 {
        if let Some(value) = f() {
            return Some(value);
        }
        sleep(Duration::from_millis(20)).await;
    }
    None
}

fn flight_events_for(utterance_id: &str) -> Vec<Value> {
    observability::events::recent(1_000)
        .into_iter()
        .filter(|event| {
            event.event_type == crate::voice::flight::VOICE_FLIGHT_EVENT_TYPE
                && event.payload.get("utterance_id").and_then(Value::as_str) == Some(utterance_id)
        })
        .map(|event| event.payload)
        .collect()
}

fn stt_mode_for(utterance_id: &str) -> Option<String> {
    stt_event_for(utterance_id)
        .and_then(|event| event.payload.get("stt_mode").cloned())
        .and_then(|value| value.as_str().map(str::to_string))
}

fn stt_latency_for(utterance_id: &str) -> Option<u64> {
    stt_event_for(utterance_id)
        .and_then(|event| event.payload.get("stt_latency_ms").cloned())
        .and_then(|value| value.as_u64())
}

fn stt_event_for(utterance_id: &str) -> Option<StructuredEvent> {
    observability::events::recent(1_000)
        .into_iter()
        .find(|event| {
            event.event_type == crate::voice::flight::VOICE_FLIGHT_EVENT_TYPE
                && event.payload.get("utterance_id").and_then(Value::as_str) == Some(utterance_id)
                && event.payload.get("stt_mode").is_some()
        })
}

fn harness_voice_config(temp: &Path, shims: &CommandShims) -> VoiceConfig {
    let mut config = VoiceConfig::default();
    config.enabled = true;
    config.keep_recordings = true;
    config.wake_words.clear();
    config.audio.recordings_dir = temp.join("recordings");
    config.audio.transcripts_dir = temp.join("transcripts");
    config.audio.tts_cache_dir = temp.join("tts-cache");
    config.audio.temp_dir = temp.join("tmp");
    config.tts.progress_cache_dir = temp.join("tts-progress-cache");
    config.stt.ffmpeg_command = shims.ffmpeg.display().to_string();
    config.stt.whisper_command = shims.whisper.display().to_string();
    config.stt.model_path = temp.join("fake-whisper-model.bin");
    config.stt.language = "ko".to_string();
    config.tts.edge.command = shims.edge_tts.display().to_string();
    config.idle.segment_idle_ms = 10;
    config.idle.utterance_idle_ms = 25;
    config.allowed_user_ids = vec![USER_ID.to_string()];
    fs::write(&config.stt.model_path, b"fake model").expect("write fake model marker");
    config
}

fn harness_config(voice_config: &VoiceConfig) -> crate::config::Config {
    let mut config = crate::config::Config::default();
    config.voice = voice_config.clone();
    let mut agent = test_agent("codex");
    agent.id = "project-agentdesk".to_string();
    agent.name = "AgentDesk".to_string();
    agent.aliases = vec!["에이디케이".to_string(), "agentdesk".to_string()];
    agent.voice.channel_id = Some(SOURCE_CHANNEL_ID.to_string());
    agent.channels.codex = Some(crate::config::AgentChannel::from(
        TARGET_CHANNEL_ID.to_string(),
    ));
    agent.provider = "codex".to_string();
    config.agents = vec![agent];
    config
}

struct CommandShims {
    ffmpeg: PathBuf,
    whisper: PathBuf,
    edge_tts: PathBuf,
}

#[cfg(unix)]
fn install_command_shims(temp: &Path, transcripts: &[&str]) -> CommandShims {
    let bin = temp.join("bin");
    fs::create_dir_all(&bin).expect("create voice PCM shim bin dir");
    let queue = temp.join("stt-transcripts.queue");
    fs::write(&queue, format!("{}\n", transcripts.join("\n")))
        .expect("write fake whisper transcript queue");
    let whisper_log = temp.join("stt-whisper.log");
    let ffmpeg = bin.join("ffmpeg");
    let whisper = bin.join("whisper-cli");
    let edge_tts = bin.join("edge-tts");

    fs::write(
        &ffmpeg,
        "#!/bin/sh\n\
set -eu\n\
for arg in \"$@\"; do\n\
  if [ \"$arg\" = \"volumedetect\" ]; then\n\
    echo '[Parsed_volumedetect_0 @ 0x0] mean_volume: -10.0 dB' >&2\n\
    echo '[Parsed_volumedetect_0 @ 0x0] max_volume: -1.0 dB' >&2\n\
    exit 0\n\
  fi\n\
done\n\
in=''\n\
out=''\n\
prev=''\n\
for arg in \"$@\"; do\n\
  if [ \"$prev\" = '-i' ]; then in=\"$arg\"; fi\n\
  out=\"$arg\"\n\
  prev=\"$arg\"\n\
done\n\
mkdir -p \"$(dirname \"$out\")\"\n\
cp \"$in\" \"$out\"\n",
    )
    .expect("write fake ffmpeg shim");

    fs::write(
        &whisper,
        format!(
            "#!/bin/sh\n\
set -eu\n\
queue={queue}\n\
log={log}\n\
if [ ! -f \"$queue\" ]; then\n\
  echo 'missing transcript queue' >&2\n\
  exit 2\n\
fi\n\
IFS= read -r transcript < \"$queue\" || transcript=''\n\
tail -n +2 \"$queue\" > \"$queue.tmp\"\n\
mv \"$queue.tmp\" \"$queue\"\n\
prefix=''\n\
language=''\n\
prev=''\n\
for arg in \"$@\"; do\n\
  if [ \"$prev\" = '-of' ]; then prefix=\"$arg\"; fi\n\
  if [ \"$prev\" = '-l' ]; then language=\"$arg\"; fi\n\
  prev=\"$arg\"\n\
done\n\
if [ -z \"$prefix\" ]; then\n\
  echo 'missing -of transcript prefix' >&2\n\
  exit 2\n\
fi\n\
mkdir -p \"$(dirname \"$prefix\")\"\n\
printf '%s\\n' \"$transcript\" > \"$prefix.txt\"\n\
printf '%s|%s\\n' \"$language\" \"$transcript\" >> \"$log\"\n\
printf '%s\\n' \"$transcript\"\n",
            queue = sh_quote(&queue),
            log = sh_quote(&whisper_log)
        ),
    )
    .expect("write fake whisper shim");

    fs::write(
        &edge_tts,
        "#!/bin/sh\n\
set -eu\n\
out=''\n\
prev=''\n\
for arg in \"$@\"; do\n\
  if [ \"$prev\" = '--write-media' ]; then out=\"$arg\"; fi\n\
  prev=\"$arg\"\n\
done\n\
if [ -z \"$out\" ]; then\n\
  echo 'missing --write-media' >&2\n\
  exit 2\n\
fi\n\
mkdir -p \"$(dirname \"$out\")\"\n\
printf 'fake mp3 bytes' > \"$out\"\n",
    )
    .expect("write fake edge-tts shim");

    make_executable(&ffmpeg);
    make_executable(&whisper);
    make_executable(&edge_tts);
    CommandShims {
        ffmpeg,
        whisper,
        edge_tts,
    }
}

#[cfg(not(unix))]
fn install_command_shims(_temp: &Path, _transcripts: &[&str]) -> CommandShims {
    panic!("voice PCM harness command shims are only implemented on Unix")
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)
        .unwrap_or_else(|error| panic!("stat {}: {error}", path.display()))
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)
        .unwrap_or_else(|error| panic!("chmod {}: {error}", path.display()));
}

fn sh_quote(path: &Path) -> String {
    let raw = path.display().to_string();
    format!("'{}'", raw.replace('\'', "'\\''"))
}

fn report_path() -> Option<PathBuf> {
    std::env::var_os("ADK_VOICE_PCM_HARNESS_REPORT")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn write_report(report: &VoicePcmHarnessReport) {
    let Some(path) = report_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .unwrap_or_else(|error| panic!("create report dir {}: {error}", parent.display()));
    }
    let file = fs::File::create(&path)
        .unwrap_or_else(|error| panic!("create report {}: {error}", path.display()));
    serde_json::to_writer_pretty(file, report)
        .unwrap_or_else(|error| panic!("write report {}: {error}", path.display()));
}

// ---------------------------------------------------------------------------
// #3906 — deterministic voice intake feedback (P1 ack signal + P4 done signal)
// ---------------------------------------------------------------------------

const PROCESSING_CHIME_CONTEXT: &str = "voice processing chime";
const DONE_CHIME_CONTEXT: &str = "voice done chime";

fn play_request_contexts(harness: &VoicePcmHarness) -> Vec<(u64, &'static str)> {
    harness
        .runtime
        .test_state
        .play_requests
        .lock()
        .expect("voice PCM harness play requests lock")
        .clone()
}

fn count_play_context(harness: &VoicePcmHarness, channel_id: u64, context: &str) -> usize {
    play_request_contexts(harness)
        .into_iter()
        .filter(|(seen_channel, seen_context)| {
            *seen_channel == channel_id && *seen_context == context
        })
        .count()
}

fn assert_no_play_context(harness: &VoicePcmHarness, context: &str) {
    let plays = play_request_contexts(harness);
    assert!(
        !plays
            .iter()
            .any(|(_, seen_context)| *seen_context == context),
        "did not expect any `{context}` playback, saw: {plays:?}"
    );
}

// P1: an utterance that resolves to a real Target emits the deterministic
// Phase-1 intake chime BEFORE start_voice_turn runs.
#[cfg(unix)]
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn voice_intake_chime_fires_before_turn_start() {
    let _guard = observability::test_runtime_lock();
    observability::reset_for_tests();
    observability::init_observability(None);
    let _env_lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());

    let harness = VoicePcmHarness::new(&["오늘 일정 알려줘"]).await;
    harness.clear_play_requests();
    let message_id = harness.next_message_id();
    harness.queue_turn_start(message_id);
    harness.queue_foreground_decision(VoiceForegroundDecision::Speak(
        "오늘 일정 확인했어요.".to_string(),
    ));

    let (utterance, _timings) = harness.feed_pcm_turn(false).await;
    let start = wait_for_turn_start(&harness, &utterance.utterance_id).await;
    assert!(
        start.is_some(),
        "utterance must resolve to a Target and reach start_voice_turn"
    );

    assert_eq!(
        count_play_context(&harness, SOURCE_CHANNEL_ID, PROCESSING_CHIME_CONTEXT),
        1,
        "exactly one deterministic intake chime must fire on the source voice channel"
    );
}

// P1 / #3905-proof: even when start_voice_turn fails (publish Err), the Phase-1
// intake chime is still recorded because it fires upstream of every
// VoiceTurnStartFailed exit and the DuplicateSuppressed drop.
#[cfg(unix)]
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn voice_intake_chime_fires_even_when_turn_start_fails() {
    let _guard = observability::test_runtime_lock();
    observability::reset_for_tests();
    observability::init_observability(None);
    let _env_lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());

    let harness = VoicePcmHarness::new(&["로그 확인해줘"]).await;
    harness.clear_play_requests();
    // Stub start_voice_turn's publish to fail; take_test_turn_start_outcome still
    // records the turn_start, so wait_for_turn_start remains a valid barrier.
    harness
        .runtime
        .test_state
        .turn_start_outcomes
        .lock()
        .expect("voice PCM harness turn start outcomes lock")
        .push_back(Err("voice publish failed for #3906 test".to_string()));

    let (utterance, _timings) = harness.feed_pcm_turn(false).await;
    let start = wait_for_turn_start(&harness, &utterance.utterance_id).await;
    assert!(
        start.is_some(),
        "start_voice_turn must be reached even when publish fails"
    );

    assert_eq!(
        count_play_context(&harness, SOURCE_CHANNEL_ID, PROCESSING_CHIME_CONTEXT),
        1,
        "intake chime must fire deterministically even when the turn-start publish fails"
    );
}

// P1 negative: an empty / whitespace-only transcript is dropped before the
// Target resolution, so NO intake chime is emitted.
#[cfg(unix)]
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn voice_intake_chime_absent_on_empty_transcript() {
    let _guard = observability::test_runtime_lock();
    observability::reset_for_tests();
    observability::init_observability(None);
    let _env_lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());

    let harness = VoicePcmHarness::new(&["   "]).await;
    harness.clear_play_requests();

    let (utterance, _timings) = harness.feed_pcm_turn(false).await;
    // ignored_noise (reason=empty_transcript) is the terminal barrier for this path.
    let ignored = wait_for_flight_route(&utterance.utterance_id, "ignored_noise").await;
    assert!(
        ignored.is_some(),
        "empty transcript must be recorded as ignored_noise"
    );

    assert_no_play_context(&harness, PROCESSING_CHIME_CONTEXT);
}

// P1 negative: a barge-in routed to handle_processing_transcript returns before
// the Target resolution, so NO intake chime is emitted.
#[cfg(unix)]
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn voice_intake_chime_absent_on_active_turn_barge_in() {
    let _guard = observability::test_runtime_lock();
    observability::reset_for_tests();
    observability::init_observability(None);
    let _env_lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());

    let harness = VoicePcmHarness::new(&["멈춰"]).await;
    harness.clear_play_requests();
    install_active_voice_route(
        &harness.runtime,
        harness.source_channel,
        harness.target_channel,
    );
    let player = Arc::new(MockPlayer::default());
    let playback_cancel = CancellationToken::new();
    harness.runtime.reset_after_playback_start(
        harness.source_channel,
        player.clone(),
        playback_cancel.clone(),
    );
    let active_token = Arc::new(crate::services::provider::CancelToken::new());
    assert!(
        harness
            .shared
            .mailbox(harness.target_channel)
            .try_start_turn(
                active_token.clone(),
                serenity::UserId::new(USER_ID),
                MessageId::new(3_801_900),
            )
            .await
    );

    let (utterance, _timings) = harness.feed_pcm_turn(true).await;
    let stop_event = wait_for_flight_route(&utterance.utterance_id, "explicit_stop").await;
    assert!(
        stop_event.is_some(),
        "barge-in stop must route through handle_processing_transcript"
    );

    assert_no_play_context(&harness, PROCESSING_CHIME_CONTEXT);
    harness.reset_scenario_state().await;
}

// Regression: the foreground announcement path no longer emits its own chime, so
// a normal successful turn yields exactly ONE intake chime (not two).
#[cfg(unix)]
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn voice_foreground_path_does_not_double_chime() {
    let _guard = observability::test_runtime_lock();
    observability::reset_for_tests();
    observability::init_observability(None);
    let _env_lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());

    let harness = VoicePcmHarness::new(&["오늘 일정 알려줘"]).await;
    harness.clear_play_requests();
    let message_id = harness.next_message_id();
    harness.queue_turn_start(message_id);
    harness.queue_foreground_decision(VoiceForegroundDecision::Speak(
        "오늘 일정 확인했어요.".to_string(),
    ));

    let (utterance, _timings) = harness.feed_pcm_turn(false).await;
    wait_for_turn_start(&harness, &utterance.utterance_id).await;
    let announcement = wait_for_announcement(message_id).await;
    if let Some(announcement) = announcement.as_ref() {
        assert!(
            harness
                .runtime
                .try_handle_voice_transcript_announcement(
                    &harness.shared,
                    harness.source_channel,
                    announcement,
                )
                .await,
            "foreground handler must consume the announcement"
        );
    } else {
        panic!("canonical voice announcement metadata was not cached");
    }

    assert_eq!(
        count_play_context(&harness, SOURCE_CHANNEL_ID, PROCESSING_CHIME_CONTEXT),
        1,
        "a normal turn must emit exactly one intake chime (foreground path adds none)"
    );
}

// P4: the turn-DONE branch plays the distinct descending done chime, not the
// rising processing chime.
#[cfg(unix)]
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn voice_turn_done_plays_distinct_done_chime() {
    let _env_lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let harness = VoicePcmHarness::new(&[]).await;
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    harness
        .runtime
        .spawn_progress_worker(harness.shared.clone(), shutdown.clone());
    harness.clear_play_requests();

    harness
        .runtime
        .publish_progress(harness.source_channel, "agent:done");

    let recorded = wait_until(|| {
        play_request_contexts(&harness)
            .into_iter()
            .find(|(channel, context)| {
                *channel == SOURCE_CHANNEL_ID && *context == DONE_CHIME_CONTEXT
            })
    })
    .await;
    assert!(
        recorded.is_some(),
        "turn-done must play the distinct done chime"
    );
    assert_no_play_context(&harness, PROCESSING_CHIME_CONTEXT);
    shutdown.store(true, Ordering::Relaxed);
}

// P4: done_chime_path resolves to a non-empty WAV at a file name distinct from
// the processing chime.
#[cfg(unix)]
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn done_chime_path_is_a_distinct_nonempty_wav() {
    assert_ne!(
        DONE_CHIME_FILE_NAME, PROCESSING_CHIME_FILE_NAME,
        "done and processing chimes must use distinct asset file names"
    );
    let _env_lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());

    let harness = VoicePcmHarness::new(&[]).await;
    let done_path = harness
        .runtime
        .done_chime_path()
        .await
        .expect("done chime path must resolve");
    assert!(
        done_path.ends_with(DONE_CHIME_FILE_NAME),
        "done chime path must point at the done-chime asset: {}",
        done_path.display()
    );
    let meta = fs::metadata(&done_path).expect("done chime WAV must exist on disk");
    assert!(meta.len() > 0, "done chime WAV must be non-empty");
}
