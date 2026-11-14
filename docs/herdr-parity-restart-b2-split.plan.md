# Herdr restart B2 재측정과 분할 제안

상태: B2a/B2b 분할 승인(2026-11-14 02:5x). B2a 종료·disconnect 조각을 먼저 구현하며, A·B1·B2a·B2b는 함께 착지한다.
기준 B1은 `a7b38ee46a867119b075d9159f626e1dcb422840`, main 병합 기준은
`fc4b3dd8474603d2696bf79fa9472d8334445977`(main `8896695e659467a2243fe7b09123e26ba8f033b8`)이다.
이 문서는 [기존 B 분할 계획](herdr-parity-restart-b-split.plan.md)의 B2 비용을 실측으로 보정한다.
수용 계약은 [설계 r3](herdr-parity-restart.design.md) 3.1~3.9를 유지한다.

## 크기 게이트와 현재 상태

초기 B2 예상은 prod 16~22파일 / +790~970이었다. 실제 owner 구현을 쓴 뒤
공식 `pr_cap_prod.py`의 기본 파서로 임시 index의 tree를 측정했다.
공유 index와 HEAD는 바꾸지 않았다. main 병합분은 아래 수치에서 제외했다.

| 측정·추정 | 생산 파일 | 순 코드 |
|---|---:|---:|
| owner queue·gate 작성분 | 9 | +583 |
| 이월 P2·epoch·Expected 연결 포함 현재 WIP | 17 | +616 |
| 미작성 명령·idle 복구·owner 배선·Codex·귀속 판정 | 추가 | +426~602 |
| fixture 산입 여유 | 추가 | +10~30 |
| 최초 재측정 합계 | 약 23~29 | **+1052~1248** |
| 동결 뒤 발견한 alias 응답 미확인 권한 구별 | 추가 | 약 +10 |
| 알려진 보정 포함 | 약 23~29 | **+1062~1258** |

아카이브한 WIP의 `source_admission/restart_seed.rs` (proposed) 순증은 +498이다. 기존 +315~375 예상에는
operation identity/context 검증, alias의 원자적 receipt+bind, transport 응답 미확인과
owner 내부 publish 미확인의 구별, 상태·철회 API의 비용이 충분히 반영되지 않았다.
응답 미확인 때 alias 후보를 creator로 오판할 수 있는 미수정 지점도 발견했다.
이를 고치는 약 +10줄을 줄여 쓰거나 시험을 빼서 계획선을 맞추지 않는다.

상단 추정이 +1200 PLAN 선을 넘으므로 추가 생산 코드를 쓰기 전에 분할한다.
현재 +616은 컴파일·PG·clippy를 통과한 구현이 아니다. 아직 `/restart`의 Herdr 거절은
유지되어 있고, 새 queue API를 공개 명령에 연결하지 않았다.

B1 +600 중 +216은 공식 파서가 두 `tests.rs` 자식을 산입한 부분이었다.
따라서 B2 전체에 600/414 같은 비율을 일괄 곱하지 않았다. 위 수치는 새 기본 파서 실측이다.

## 제안 경계: 종료 세로 조각 → durable seed 활성화

A·B1·B2a·B2b는 각각 리뷰하고 **함께 착지한다**. B2a만 배포하지 않는다.
새 운영자 명령·수동 스위치를 만들지 않는다. 중간 조각의 미완 기능을 Ready라고 표시하지 않는다.

### B2a — 명령의 exact 종료·disconnect·wake와 이월 P2

예상 prod 11~15파일 / 순증 약 +430~650. main 병합분을 제외한다.

- 인증 뒤 `/restart`·`!restart` 공통 명령에 canonical row 분기, 안정 operation identity,
  기존 detached exact terminate, clear epoch 대조 postlude, selector 보존 disconnect와 wake를 연결한다.
- dispatch 진행 중에도 종료한다. row의 dispatch 기대는 고정하되 captured turn과 일치할 때만
  그 dispatch를 결합한다. 불일치 행은 r3대로 종료하고 disconnect의 `false`는 row 미변경으로 표시하되,
  그 이유만으로 종료 실패로 격상하지 않는다.
  기존 HTTP force 경로의 엄격한 captured-dispatch 검사는 그대로 유지한다.
- idle Bound·placed Pending의 이미 끝난 실행은 기존 `execution_end`·`retire_confirmed`를
  transition/fence 아래 호출하는 작은 child로 처리한다. 실제 restart가 이미 끝난 row를 만난 경우에도
  이 child를 소비한다. 새로운 close는 보내지 않는다. 확인 불가면 보이는 실패다.
- operation identity와 in-flight gate/메모리 완료 결과는 작은 공용 child에 둔다.
  명령과 owner의 gate 대기가 바로 소비한다. durable queue 본체를 끌어와 미사용 API를 제출하지 않는다.
- 이 단계의 Herdr 인사말은 기존 headless 거절/실패 표시를 유지한다. 종료 확인과 인사말 실패를
  구별하고 successor Ready를 주장하지 않는다. 기존 큐의 대기 입력은 보존하고 해제 뒤 깨운다.
- A 최초 Resume cause 가드와 B1 projection-before-record 시험 보강을 첫 생산 커밋에 포함한다.
  idle 복구 본체·operation 재처리와 captured dispatch 결산 불변도 이 조각에서 고정한다.
  seed-only dispatch 상속 차단은 실제 seed 소비자가 생기는 B2b에서 구현·시험한다.

안전 경계는 새 queue API를 켜지 않고 기존 종료 서비스를 소비하는 데 있다.
작동 중 입력을 별도 purge·cancel-first 경로로 버리지 않는다. seedless crash는 r3의 기존 예외다.

### B2b — durable seed·owner permit·Codex resume·최종 Ready

예상 prod 18~25파일 / 순증 약 +850~1100. 승인 후 B2a 실측으로 다시 보정한다.

- 현재 +583 queue WIP에서 operation identity/gate의 B2a 공용 부분을 재사용하고,
  나머지 admission·alias·prefix·상태·철회·일반 ingress 거절을 연결한다.
- operation ticket을 현재 row의 새 context보다 먼저 조회한다. Existing/Joined/Expired/Conflict는
  close·create·seed를 늘리지 않는다. Full/V2 미채택/설정·effort 대기는 seedless 종료로 진행한다.
- mailbox 응답 미확인과 owner 내부 publish 미확인을 구별한다. alias 여부·creator 권한이
  확인되지 않은 후보로 종료하지 않고 같은 operation lookup으로 확정한다.
- durable bind에서 읽은 seed만 허용하는 typed permit을 claim 전에 소비한다.
  idle close 직후 crash는 B2a의 종료 증거 복구를 재사용한다. 새 close 0이다.
  다른 successor nonce는 ticket/attempt의 정확한 소유 증거가 없으면 합류하지 않는다.
- 보호된 head+followers 바로 뒤, 나머지 입력 앞에 seed를 넣는다. seed에만 dispatch 상속을 금지하고
  뒤 입력의 thread/card 재발견은 tmux 기준선을 유지한다.
- Codex에는 typed seed permit에서만 `HerdrLaunch.resume=true`와 A의 내구 시작점을 전달한다.
  일반 재기동은 fresh다. `HerdrTurnPlan`의 typed 선택 필드로 전달하면 광범위한 turn 필드 변경을 피할 수 있다.
- Queued/Attempt/Settled/Retained/clear 폐기/철회/Gone을 구별한다. admission이나 현재 Bound row만으로
  Ready를 말하지 않는다. 해당 seed의 실제 제출·source/nonce 귀속과 결과 응답을 함께 검사한다.
- 최종 `/restart`·`!restart` 시험에서 seed와 뒤 입력이 각각 정확히 한 번 처리되는 것을 확인한다.

## 재사용 포트와 giant 파일 경계

| 책임 | 현재 코드 좌표 | 연결 경계 |
|---|---|---|
| 기존 detached 종료 | `src/services/discord/herdr_terminate/http_force.rs:30` | SharedData health registry와 Expected·postlude·Released 재사용 |
| captured dispatch 고정 | `src/services/discord/herdr_terminate.rs:499` | typed 조건만. row 기대와 captured 결합의 차이를 명시 |
| 이미 끝난 실행 관측·retire | `src/services/discord/herdr_terminate.rs:217`, `:224` | 작은 recovery child. giant에 새 복구 본문을 넣지 않음 |
| selector 보존 disconnect | `src/services/discord/herdr_terminate/retired_exclusion.rs:100` | exclusion 아래 기존 CAS, dispatch 불일치는 false 유지 |
| owner의 기존 wake | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:602` | gate Drop 뒤 typed 연결 |
| prefix와 admission bind | `src/services/turn_orchestrator/queued_head/v2.rs:398`, `v2/source_admission.rs:627` | B1 고정 followers와 기존 atomic commit 재사용 |
| headless 제한 | `src/services/discord/router/message_handler/headless_turn.rs:159` | typed permit만 선택. 일반 시작 거절 유지 |
| Codex A 포트 | `src/services/codex/herdr_turn.rs:192`, `src/services/herdr_launch/codex_resume.rs:13` | seed-only typed 요청, 기존 source/O EOF 검증 유지 |
| dispatch 재발견 | `src/services/dispatches/thread_reuse.rs:247` | seed만 차단, backlog는 기존 tmux 정책 |

`headless_turn.rs`·`owner_input.rs`·`herdr_terminate.rs`와 source-admission 부모에는
선언·typed 연결·거절만 둔다. 새 trait·Core·serde variant·SnapshotV2 필드는 없다.
운영 파일·DB·프로세스·Herdr server를 조작하지 않는다.

## 동일 operation의 보장 범위

seed가 있으면 기존 durable ticket/receipt와 alias bind가 권위다. bind가 만료돼 receipt만 남아도
이전 요청임을 답하고 재종료하지 않는다. 같은 프로세스의 seedless 완료 결과는 메모리에 유지한다.
TTL/개수 제한으로 지웠다가 같은 operation을 다시 종료하는 정책을 넣지 않는다.
메모리 완료 결과는 명령 future가 아니라 detached 작업에서 Gate 해제 전에 기록한다.
요청 취소 때문에 완료를 기억하지 않은 채 owner를 깨우고 같은 operation을 다시 종료하지 않는다.

r3 3.5·3.6에 명시된 **seedless crash 뒤에는 내구 기록이 없다**는 예외는 그대로다.
그 경우까지 영속적 재호출 불변을 새로 보장하거나 기존 Discord 60초/1024개 dedup을
그 증거로 승격하지 않는다. 새 durable operation store도 만들지 않는다.

## 검증 분배

| 조각 | 실제 진입점과 RED→GREEN | 원인 변이 |
|---|---|---|
| B2a | `/restart`/`!restart` row 전수, dispatch 진행·row/captured 불일치, Full·clear, 종료 후 원본 보존·wake, idle close 직후 재진입 | exact 기대 생략, captured 결합 오판, epoch 생략, disconnect/wake 누락 |
| B2a 이월 | 최초 Startup/non-SessionStart 거절 후 실제 Resume 채택; create 반환 뒤 pane 기록 직전 별도 wait_idle 최초 poll Pending | Resume cause 가드 제거; begin_effect 성공 뒤 조기 finish_start |
| B2b | 실제 명령→durable seed→owner→provider→receipt/source/Ready→뒤 입력, 재부팅·옛 형식, 동시 operation/alias·commit 미확인 | Conflict→seedless, lookup 생략, prepend/append 오순서, 복구 reclose, permit/dispatch/Ready 귀속 생략 |

각 조각에 실제 PG `--test-threads=1`, 선택=실행=통과·skip0, 원복 바이트 동일,
`cargo test --lib --no-run`, clippy 변경 파일 경고0, Windows lib/tests check,
`prepush_gates.sh --static-only`를 요구한다. 이미 완료된 다른 head의 Rust 증거를 새 head에 재사용하지 않는다.
공식 생성 목록은 공식 생성기로만 동기화한다. static·Windows 시간을 마지막에 남겨 둔다.

## 미컴파일 WIP 인계

미컴파일 source 24파일의 raw diff·SHA·사본을 보고서의 로그 루트에 보존하고 바이트를 대조한 뒤,
checkout의 source는 main 병합 기준으로 복원했다. review ref에 제출한 완성 구현이 아니다.
코디 승인 뒤 B2a/B2b 소유 파일을 나누고 보관 사본의 필요한 부분을 해당 조각에서 이어 쓴다.
stash나 다른 worktree를 쓰지 않았으며 아카이브와 복원 결과를 각각 manifest로 남겼다.
첫 B2 생산 커밋은 이월 P2의 해당 계약·시험이 함께 들어가는 시점에 만든다.

