# Voice Epic — E2E 검증 시나리오 (#2027)

마지막 업데이트: 2026-05-11
구현: #2018 ~ #2031 (Voice 1~13). #2027 (Voice #10) 마무리 작업의 일부.

이 문서는 Voice 에픽 마무리에 필요한 E2E 5개 시나리오를 정의한다. 시나리오 자체는
mac-mini 의 실제 voice channel + 마이크 환경에서 사람이 음성으로 수행해야 하므로
자동화 대신 **실측 체크리스트**로 보존한다. 본문은 시나리오·기대 동작·관찰 포인트·
체크박스를 포함하며, 실측 후 결과(통과/실패 + 짧은 메모)를 직접 기록한다.

## 사전 준비

- mac-mini 에 voice channel 이 join 되어 있을 것 (`/voice join` 슬래시 명령 또는
  `auto_join_channel_ids` 자동 join).
- `agentdesk doctor` 의 voice 섹션이 모두 `PASS`(특히 `voice_whisper_cli`,
  `voice_edge_tts`, `voice_ffmpeg`, `voice_udp_socket`).
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
| `agentdesk doctor` 에 voice 섹션 + whisper-cli 제거 시 명시적 에러 | `which whisper-cli` → `mv $(which whisper-cli) /tmp/whisper-cli.bak && agentdesk doctor` 로 fail 확인, 끝나면 복구 |
| E2E 시나리오 5개 실측 결과를 본 문서에 기록 | 각 시나리오 체크박스 + 메모 |
| `cargo test --workspace` 통과 | voice (66/66) + observability (8/8) PASS, PG-스위트 109건 실패는 사전부터 PG 환경 필요 — 본 변경과 무관 |
