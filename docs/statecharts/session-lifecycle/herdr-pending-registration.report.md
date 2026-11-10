# Codex Herdr Pending 재사용과 등록부 복구

기준: `bab12c1de9d735a86ab6276572f4d997700682e6`. `HerdrSession`의 Crash/Boot/Send/Bind를 확장한다. 기존 RootRecovery는 Bound root 교체를 다루므로 Pending 첫 입력에 적용하지 않았다. 원래 모델은 변경하지 않았다.

## 실행 결과

Java `/usr/bin/java` 21, tla2tools 1.7.4, `scripts/ci/statecharts_tlc.py --root <scoped-root> --jobs 2`로 7개 설정을 검사했다. 기대 위반 집합 불일치 0, 7.0초. WSL에는 statechart frontend가 없어 저장소 TLC 실행기를 사용했다. 직접 TLC Before exit 13, After exit 0도 확인했다. 조사 시작부터 30분 이내에 완료했다.

| 설정 | 결과 | 검사 |
|---|---|---|
| Before | RED: PendingReuseBound | registry 복구 없이 첫 입력 후 영구 hold |
| After | GREEN | evidence 있는 Pending 재사용 → Startup 기록 → Bound |
| Unplaced | GREEN | 위치 없는 Pending을 기존 receipt로 복구한 뒤 같은 전이 |
| Evidence | GREEN | 위치만 있는 Pending의 evidence 재조회 뒤 같은 전이 |
| Other | GREEN | 다른 등록 nonce 보존, 입력 0 |
| Marker | GREEN | spawn nonce 불일치면 입력 0 |
| Bound | GREEN | Pending이 아닌 현재 행이면 입력 0 |

모든 설정에서 NoOtherNonceOverwrite, NoDuplicateInput, RestoreOnlyPending을 검사한다. 정상 3개 설정의 PendingReuseBound는 실제 PROPERTY다. 거절 설정에는 성공 liveness를 강제하지 않는다. CHECK_DEADLOCK FALSE는 정상 종료/거절 종단 상태만 허용한다. symmetry는 사용하지 않는다.

Retired 행은 기존 `src/services/session_host/herdr_gate.rs:356`의 live-state 관문이 이미 거절한다(main `be25647227a712c740aae40e43ee3658ffb59613`). 모델의 Bound 설정과 Rust의 cached Pending → current Bound 시험은 이번 Pending 재검증을 확인한다. Retired 관문은 새로 구현하지 않는다.

## 반례를 코드에 대조

Before: 유효 Pending/evidence와 registry 1 → Crash(registry 0, row/marker 유지) → Boot → first_input::write가 입력을 보냄 → native UUID Startup → observe_codex_herdr_hook의 등록부 판정이 None → Source 기록 없음 → hold 무한 유지. Rust 재현은 취소 후 Pending을 남긴 기존 시험에서 registry를 비우고 실제 boot reconnect를 거쳐 같은 입력을 재사용한다. 네이티브 UUID relay 결과를 쓰며 attach/hold/송신 수를 확인한다.

After의 PrepareInput은 최신 Pending row·nonce·spawn marker를 검증하고 listing order → source authority → registry leaf lock 순서로 빈 항목만 채운다. 이미 같은 nonce면 admission 상태를 유지하고, 다른 nonce면 거절한다. 이 전이의 구현 증거와 삭제 뮤턴트는 레인 보고서에 기록한다.

# Pending 재사용 진입점·모델 값의 출처

기준: bab12c1de9d735a86ab6276572f4d997700682e6. 운영 설정/runtime/DB를 읽지 않고 코드만 확인했다.

| 생성·재사용 진입점 | 코드 좌표 | 남는 상태·모델 대응 |
|---|---|---|
| fresh Pending 설치 | herdr_launch.rs:471–482 → hosted_execution.rs:411 | durable Pending, nonce 1, memory registry 1, marker 1 |
| create 결과에서 location 기록 | herdr_launch.rs:515–566 | Pending(location 있음, expected 없음) = placed |
| provider evidence 기록 | herdr_launch.rs:592–615 → hosted_execution.rs:433 | Pending(expected 있음) = evidenced |
| 재시작 후 Pending boot pass | recovery_engine/herdr_reader.rs:455–465 | durable row 유지, 메모리 등록부 0, Startup 기록 없음 |
| Pending 공통 선택 | codex/herdr_turn.rs:156–164 | held/options 관문 뒤 pending_launch, first_prompt 합류 |
| unplaced Pending 복구 | codex/herdr_turn.rs:240–253 → herdr_launch/unplaced.rs:224–312,445–466 | exact receipt/pane/evidence를 CAS 기록한 뒤 같은 nonce 재사용; retired면 fresh launch |
| evidence 없는 Pending 재조회 | codex/herdr_turn.rs:255–264 → herdr_launch.rs:606–630 | 새 pane 생성 없이 기존 pane evidence 재조회 후 같은 nonce |
| evidence 있는 Pending 재사용 | codex/herdr_turn.rs:265 | nonce 그대로 first_prompt 합류 |
| legacy 첫 입력 | codex/herdr_turn.rs:450 → first_input.rs:9–21 | current_record 재조회, gate, write |
| owner 첫 입력 | codex/herdr_turn.rs:446–448 → first_input.rs:26–67 | 읽은 record로 owner callback/physical write |
| 물리 입력 합류 | first_input.rs:70–104 | target/composer 판정 뒤 alias·placeholder·hold 설치, 한 번의 input plan |
| native Startup 관찰 | runtime_binding/codex_hook.rs:324–342 | registry가 정확히 1일 때만 Source 기록, 0/2는 기존 Proceed |
| Bound·hold 해제 | first_input.rs:141–172 → herdr_turn.rs:279–297 | 자신의 Startup을 읽어 attach, Bound, hold 종료 |
| 다른 nonce 등록·표식 교체 | runtime_binding/adopt_skip.rs:205–247, herdr_launch.rs:482–489 | registry 2 또는 marker 2; 새 복구가 덮어쓰면 안 됨 |

1/2는 실제 UUID nonce의 동등/다름만 추상화한다. 0은 메모리에 등록되지 않음을 뜻한다. HerdrSession의 evidenced는 HostedState::Bound가 아니라 expected evidence가 있는 Pending을 뜻한다. 모델의 pending/placed/evidenced는 위 세 Pending 복구 갈래이며, 모두 first_input 앞에 합류한다. Bound 행·다른 marker·다른 등록 nonce는 입력 전에 거절해야 한다.

모델은 원래 HerdrSession의 Crash/Boot/Send/Bind와 상태 집합을 확장한 파생 모델이다. provider·row·rollout 파일이 유효하고 전달되는 Startup, 공정한 재시작/복구/입력/기록/attach를 가정한다. 첫 입력을 받은 뒤 다른 실행이 다시 supersede하는 환경은 liveness 범위 밖이고, 다른 nonce가 먼저 등록된 경우는 별도 safety 설정으로 검사한다. 네트워크/PG 오류, DB 행과 입력의 원자성은 Rust 관문·시험의 의무다.
