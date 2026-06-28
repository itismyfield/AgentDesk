# Voice Epic — E2E 검증 시나리오 (#2027)

마지막 업데이트: 2026-06-29
구현: #2018 ~ #2031 (Voice 1~13), #3801 unattended PCM harness, #3802 unattended live media smoke.

이 문서는 Voice 에픽 마무리에 필요한 E2E 시나리오를 세 lane 으로 정의한다.
`controlled` lane 은 로컬에서 생성한 PCM 을 실제 receiver/STT/routing/TTS 관측 경로에
주입하는 결정적 harness 이다. Unattended `real_live` lane 은 별도 테스트 speaker bot 이
실제 Discord voice channel 로 생성 음성을 송출해 Discord gateway/songbird media transport 를
검증한다. Manual `real_live` lane 은 사람이 실제 Discord client/microphone/speaker 로
실측하는 체크리스트다. Controlled harness 는 사람이 voice channel 에 참여할 필요가 없지만,
live Discord media transport 자체를 검증한다고 주장하지 않는다.

## agent_mode lane

- 현재 수동 실측 체크리스트는 실제 Discord voice channel, STT/TTS, 외부 agent 응답을
  쓰므로 `agent_mode: real_live` 로 취급한다.
- 결정적 PCM/fixture 기반 voice harness 는 `agent_mode: controlled` 로 기록하고,
  실제 provider 접촉 없이 미디어 파이프라인만 검증한다.
- unattended live voice media smoke 는 명시적 opt-in 일 때만 `agent_mode: real_live` 로 실행하며,
  보고서에는 `agent_mode`, cell/provider 식별자, `real_provider_contacted`, 실패 귀속
  (Discord media, STT/TTS, provider response, relay/reporting)을 남긴다.

## 결정적 local PCM harness (#3801)

로컬 unattended E2E 명령:

```bash
scripts/e2e/run_voice_pcm_harness.py --report target/voice-pcm-harness-report.json
```

이 harness 는 사람이 Discord voice channel 에 들어가지 않는다. 테스트가 만든 PCM frame 을
`VoiceReceiver::queue_pcm_for_control_channel` 에 넣고, `DiscordVoiceBargeInHook` 의
`observe_pcm` / `utterance_completed` 경계로 전달한다. 이후 파일 STT, voice transcript
announcement, foreground/background routing, TTS/playback 관측, `voice_latency_turn`
metric 을 같은 runtime 경로에서 확인한다. Transcript/STT/TTS dependency 는 tempdir 안의
격리된 command/config shim 으로만 제어하며, 호스트의 실제 `ffmpeg`, `whisper-cli`,
`edge-tts` binary 를 이동하거나 변경하지 않는다.

Covered scenarios:

| scenario_id | 범위 |
|-------------|------|
| `normal-short-ko` | 짧은 한국어 turn, STT transcript, foreground speak, TTS/playback, latency metric |
| `barge-in-while-tts-active` | TTS active 중 PCM barge-in, playback cancel, active turn cancel, explicit-stop event |
| `long-answer-background-handoff-summary` | long-answer handoff, background start, spoken ack + completion summary |
| `spoken-command-language-route-change` | spoken language command routed through dispatcher without transcript injection |
| `language-switch-english-turn` | language switch 이후 영어 STT/announcement/foreground speak |
| `negative-missing-ffmpeg-config-shim` | temp config shim 으로 missing ffmpeg dependency failure 격리 검증 |

Report contract:

- JSON report path 는 `--report` 또는 `ADK_VOICE_PCM_HARNESS_REPORT` 로 지정한다.
- Top-level `agent_mode` 는 `controlled`, `live_discord_media_transport_covered` 는 `false`.
- 각 scenario 는 `scenario_id`, `utterance_id`, channel/test identity, transcript/STT 결과,
  foreground/background routing decision, TTS/playback observation, `voice_latency_turn`,
  `voice_flight_events`, timing stages, raw failure reasons 를 기록한다.

## Unattended live Discord media smoke (#3802)

로컬/CI 기본값에서는 실행하지 않는다. 이 lane 은 실제 Discord guild/channel 에 봇 2개가
접속한다: AgentDesk bot 은 기존 voice runtime 경로로 channel 에 들어가고, 별도 speaker
bot 은 generated TTS audio 를 Discord voice media 로 송출한다. Speaker bot 이 transcript
injection 이나 `VoiceReceiver::queue_pcm_for_control_channel` 을 쓰지 않기 때문에 #3801 PCM
harness 가 우회하는 Discord gateway/songbird receive path 를 검증한다.

명령:

```bash
ADK_VOICE_LIVE_MEDIA_SMOKE=1 \
ADK_VOICE_LIVE_SAFETY_ACK=I_UNDERSTAND_THIS_USES_LIVE_DISCORD_VOICE \
ADK_VOICE_LIVE_TEST_GUILD_ID=<test guild id> \
ADK_VOICE_LIVE_TEST_VOICE_CHANNEL_ID=<test voice channel id> \
ADK_VOICE_LIVE_TEST_TEXT_CHANNEL_ID=<paired/control text channel id> \
ADK_VOICE_LIVE_CONFIRM_GUILD_ID=<same guild id> \
ADK_VOICE_LIVE_CONFIRM_VOICE_CHANNEL_ID=<same voice channel id> \
ADK_VOICE_LIVE_AGENTDESK_BOT_ID=<agentdesk bot user id> \
ADK_VOICE_LIVE_SPEAKER_BOT_TOKEN_FILE=~/.config/agentdesk/voice-smoke-speaker-token \
ADK_VOICE_LIVE_AGENT_ID=project-agentdesk \
ADK_VOICE_LIVE_PROVIDER_IDENTITY=codex:project-agentdesk \
ADK_VOICE_LIVE_REAL_PROVIDER_CONTACTED=true \
scripts/e2e/run_voice_live_media_smoke.py \
  --allow-live-discord \
  --report target/voice-live-media-smoke-report.json
```

Safety/refusal contract:

- `--allow-live-discord`, `ADK_VOICE_LIVE_MEDIA_SMOKE=1`, and the exact
  `ADK_VOICE_LIVE_SAFETY_ACK` value are all required.
- Test guild/channel ids must be set twice: once as target ids and once as
  confirmation ids. The runner refuses when they differ.
- A separate speaker bot token is required. If `ADK_VOICE_LIVE_SPEAKER_BOT_ID`
  is supplied, it must differ from `ADK_VOICE_LIVE_AGENTDESK_BOT_ID`; after login,
  the runner also refuses if the token resolves to the AgentDesk bot user.
- AgentDesk must already be in the test voice channel, normally through
  `voice.auto_join_channel_ids` or an operator-issued `/voice join`. The runner
  checks voice-channel membership before streaming audio.
- Normal offline CI should only run
  `python3 -m unittest scripts/e2e/test_voice_live_media_smoke.py`.

Optional dependencies/config:

- Install `discord.py` with voice support for the speaker client and ensure
  `ffmpeg` is on PATH for playback.
- Audio generation uses `edge-tts` when available. Operators can instead set
  `ADK_VOICE_LIVE_TTS_COMMAND`, a shell template with `{text}`, `{output}`, and
  `{voice}` placeholders.
- `ADK_API_BASE_URL` defaults to `http://127.0.0.1:8791`; `ADK_API_AUTH_TOKEN`
  or `ADK_API_TOKEN` is used when the observability API is protected.
- The runner polls `/api/analytics/observability?recentLimit=1000` and falls
  back to `ADK_OBSERVABILITY_EVENTS_PATH` or
  `~/.adk/release/logs/observability-events.jsonl`.
- `ADK_VOICE_LIVE_CLEANUP_CHECK_COMMAND` may point to a deployment-specific
  read-only checker. It receives `ADK_VOICE_LIVE_SCENARIO_ID`,
  `ADK_VOICE_LIVE_UTTERANCE_IDS`, `ADK_VOICE_LIVE_VOICE_CHANNEL_ID`, and
  `ADK_VOICE_LIVE_TEXT_CHANNEL_ID`, and should print JSON with `ok: true` plus
  stale-state booleans such as `stale_voice_session`, `playback_task_active`,
  `foreground_call_active`, and `voice_turn_link_active`, all explicitly set to
  `false`. Cleanup proof is fail-closed: unavailable probes, missing cleanup
  probes, or missing stale-state booleans fail the scenario instead of being
  folded into a pass.

Covered unattended live scenarios:

| scenario_id | 범위 |
|-------------|------|
| `normal-short-live-media` | Speaker bot streams a short generated prompt; AgentDesk must emit STT/queued/foreground speak evidence and `voice_latency_turn`. |
| `barge-in-while-tts-active-live-media` | Speaker bot starts a long-answer prompt, then streams a stop/follow-up utterance; report requires `explicit_stop` cancellation evidence. |
| `long-answer-background-handoff-summary-live-media` | Speaker bot asks for long/background work; report requires queued/background handoff evidence and latency/playback evidence. |

Report contract:

- Top-level `agent_mode` is `real_live`, `live_discord_media_transport_covered`
  is `true`, and tokens are never written to the report.
- Each scenario records guild/channel ids, scenario id, utterance ids, media
  receive counters, STT/transcript evidence, routing/foreground decision,
  TTS/playback or cancellation evidence, an utterance-matched
  `voice_latency_turn`, raw
  `voice_flight_event` payloads, cleanup evidence, timing stages, and raw
  failure reasons.
- The runner may keep a short pre-start observability grace window for
  diagnostics, but pass/fail evidence, representative `utterance_id`, and
  `voice_latency_turn` matching are derived only from events recorded at or
  after the scenario start timestamp.
- `failure_attribution.source` separates likely `discord_media`, `stt_tts`,
  `provider_response`, `cleanup`, and `reporting` failures.
- `real_provider_contacted` is operator-declared via
  `ADK_VOICE_LIVE_REAL_PROVIDER_CONTACTED` so controlled-provider staging runs
  do not masquerade as real-provider coverage.

Relationship to other lanes:

- #3801 PCM harness is deterministic and fast. It validates AgentDesk receive,
  STT/routing/TTS observability after local PCM injection but intentionally does
  not cover Discord gateway/songbird media transport.
- #3802 unattended live media smoke covers the real Discord/songbird transport
  path without requiring a human speaker/listener, but it is slower and depends
  on live Discord resources.
- #3596 manual physical evidence still covers the human Discord client,
  microphone, speaker, and subjective audio quality path that an unattended bot
  cannot prove.

## 머신/클라이언트 분리 (중요)

ADK voice는 Discord 표준 voice channel을 그대로 사용하므로 **사용자 마이크/스피커는
어느 Discord 클라이언트(폰/맥/iPad)에서든 동작한다**. 처리는 dcserver가 동작 중인
머신에서 일어난다.

| 책임 | 머신 |
|------|------|
| 사용자 마이크 입력 / 봇 TTS 출력 듣기 | 임의 Discord 클라이언트 |
| voice 패킷 수신 (songbird) / STT (whisper-cli) / TTS (edge-tts) | dcserver 호스트 — 현재 `mac-book-release` |
| `agentdesk doctor` 실행 | dcserver 호스트와 같은 머신 |

## 사전 준비

아래 준비는 manual `real_live` lane 전용이다. `scripts/e2e/run_voice_pcm_harness.py` 는
로컬 tempdir 과 command shim 을 사용하므로 Discord voice join, 마이크, live media transport
준비가 필요 없다.

- 봇이 voice channel 에 join 되어 있을 것 (`/voice join` 슬래시 명령 또는
  `auto_join_channel_ids` 자동 join).
- dcserver 호스트에서 `agentdesk doctor` 의 voice 섹션이 모두 `PASS`
  (특히 `voice_whisper_cli`, `voice_edge_tts`, `voice_ffmpeg`, `voice_udp_socket`).
- `~/.adk/release/logs/observability-events.jsonl` 가 비어 있거나 직전 위치를
  기록해 둘 것 — 시나리오마다 새 `voice_latency_turn` 라인을 확인해야 한다.

## 공통 검증 명령

| 단계 | 명령 / 위치 |
|------|-------------|
| latency 평균 확인 | `/voice latency` (Discord 슬래시 명령) |
| JSONL 로그 tail | `tail -f ~/.adk/release/logs/observability-events.jsonl \| grep voice_latency_turn` |
| doctor voice 섹션 | `agentdesk doctor` 출력에서 `[voice]` 그룹 |

## 시나리오 1 — 정상 turn

- 입력: 짧은 한국어 질문 1문장 ("오늘 일정 알려줘")
- 기대 동작:
  1. STT → 한국어 transcript 정상
  2. agent → 평이한 답변 (10줄 이내)
  3. TTS → 1~2 chunk 로 자연스럽게 재생
- 관찰: `voice_latency_turn` 1줄이 JSONL 에 추가되고 `total_ms` 가 5초 이내
- 체크: [ ] 통과 / [ ] 실패 — 메모:

## 시나리오 2 — Barge-in (말 끊기)

- 입력: TTS 가 답변을 읽는 도중에 사용자가 새 발화 시작
- 기대 동작:
  1. 진행 중인 TTS 가 즉시 중단 (`voice barge-in` 로그)
  2. 새 발화가 STT → agent 경로로 정상 진입
  3. 30 초 무조건 clear 가 다음 turn 의 barge-in 을 막지 않는지 확인
     (#2046 Voice rev2 Finding 4 회귀 테스트)
- 관찰: 이전 turn 의 `voice_latency_turn` 은 `tts_play_ms` 가 작거나 0,
  새 turn 의 라인이 별도 추가
- 체크: [ ] 통과 / [ ] 실패 — 메모:

## 시나리오 3 — 긴 diff 답변

- 입력: "현재 git status 와 마지막 commit diff 보여줘" 같은 긴 결과 유발 질문
- 기대 동작:
  1. agent 답변이 길더라도 `spoken_result_only` sanitizer 가 코드 블록·diff·
     verification log 헤더를 제거하고 한국어 자연어 요약만 TTS 로 전달
  2. TTS chunk 가 4~6개 이상 분할되어 prefetch 가 동작하는지 (`first_chunk_synthesis_ms`
     이 `first_audio_start_ms` 보다 짧음)
  3. JSONL `voice_latency_turn.total_ms` 가 30초 이내 (외부 LLM 응답 지연 포함)
- 관찰: 별도로 텍스트 채널에는 원본 diff 가 그대로 게시되고, 음성에는 요약만
- 체크: [ ] 통과 / [ ] 실패 — 메모:

## 시나리오 4 — 음성 명령 (sensitivity / verbose / language)

- 입력: 차례로 발화
  1. "민감도를 conservative 로 바꿔줘"
  2. "verbose 진행 보고 켜줘"
  3. "언어를 영어로 바꿔줘"
- 기대 동작:
  - 각 명령이 `apply_dispatcher_command` 에서 매칭되어 텍스트 채널에 확인 메시지
  - `/voice sensitivity` 의 결과가 새 turn 부터 적용
  - 영어 전환 후 다음 STT 결과가 영어 transcript
- 관찰: `voice_latency_turn` 은 명령 자체에는 추가되지 않음 (turn 없이 dispatcher 종료),
  실제 turn 발화에서만 추가
- 체크: [ ] 통과 / [ ] 실패 — 메모:

## 시나리오 5 — 언어 전환 후 정상 turn

- 입력: 시나리오 4 의 마지막 영어 전환 직후 영어로 짧은 문장 발화
  ("What's on my schedule today?")
- 기대 동작:
  1. STT 가 영어 transcript 반환
  2. agent 답변이 영어로 생성
  3. TTS 가 영어 voice 로 (config 의 `tts.edge.voice` 가 ko-KR-* 라면 한국어 voice
     로 영어 발화 — 발음 어색하지만 동작은 정상)
- 관찰: `voice_latency_turn.utterance_id` 가 다른 시나리오와 별개의 식별자
- 체크: [ ] 통과 / [ ] 실패 — 메모:

## DoD (Issue #2027)

| DoD | 충족 방법 |
|-----|-----------|
| 한 voice turn 후 latency JSONL 기록 | 시나리오 1 통과 후 `tail` 로 1줄 확인 |
| `/voice latency` 가 최근 5 turn 평균 출력 | 시나리오 1~5 진행 후 `/voice latency` 슬래시 |
| voice dependency failure 가 실제 host binary 변경 없이 관측됨 | #3801 PCM harness 의 `negative-missing-ffmpeg-config-shim` 이 temp config shim 으로 fail path 확인 |
| E2E 시나리오 5개 실측 결과를 본 문서에 기록 | 각 시나리오 체크박스 + 메모 |
| `cargo test --workspace` 통과 | voice (66/66) + observability (8/8) PASS, PG-스위트 109건 실패는 사전부터 PG 환경 필요 — 본 변경과 무관 |
