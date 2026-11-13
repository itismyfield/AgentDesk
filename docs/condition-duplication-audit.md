# 반복 조건식 검사 실측·운용

조사 corpus는 main `33d1f4b5a9dce7023e2d0014e45a05ed6c530bba`의 운영 Rust 모듈 그래프다. 첫 구현은 `b64dcdd9bd3a4cfa33f66619fe56b09d3e919cef`이며 아래 수리 후 수치도 같은 Rust corpus에서 재측정했다. A lexer·literal·scope·alias·cfg/test 제외 계약을 재사용한다. Rust 변경·cargo·운영 관측은 없다.

기본 CI/prepush 경고·scan API·CLI 모드는 **exact**다. normalized는 `--mode normalized` 또는 `--mode both` 조사 옵션으로 둔다. 서로 다른 `path:line:column` 세 곳 이상인 복합 논리식과 등록 raw accessor predicate를 경고한다. 단일 고수준 helper 호출은 재사용으로 제외한다. warning은 실패 gate가 아니다.

`if`/`else if`/`while`, match guard, bool let·tail·closure를 lexical container에서 찾는다. 등록 accessor 바로 뒤의 균형 잡힌 빈 `.is_none()`·`.is_some()` span은 guard와 별개인 `kind=known_accessor` 후보로 추출한다. 임의 공통 부분식은 탐색하지 않는다. 같은 standalone span을 여러 번 방문해도 source location으로 한 번만 센다.

exact는 공백·주석·숫자 구분자·단일 atom의 불필요 괄호를 정리한다. normalized는 인자·let·closure·match-arm 로컬을 alpha-rename하고 등록 accessor의 qualification/alias·단순 receiver를 정리한다. 함수 generic `<…>`와 그 안의 balanced container를 건너뛰어 `Fn/FnMut/FnOnce` bound의 괄호와 실제 인자를 구별한다. accessor 인자의 bare local `.get()`만 정규화하고, ChannelId 확인이 안 되면 ambiguous와 이유를 남긴다. field·literal·call·operator·평가 순서와 local 재참조 관계를 보존하며 effectful receiver/argument를 지우지 않는다.

```sh
PYTHONPATH=scripts python3 -m audit_maintainability.checks.duplicate_conditions --mode both > target/condition-census.json
python3 -m unittest -v tests.test_duplicate_conditions
python3 scripts/audit_maintainability.py --check
```

## main 전수 전/후와 채택

TP는 같은 판정 복사 후보, FP는 의미가 다른 지역 값을 같은 모양으로 합친 군집, 불명은 미검토 또는 해석 불확실 군집이다. 앞 턴의 상위20·host·추가 UTF-8 표본 판정을 유지하고 새 predicate 군집을 추가 검토했다. **precision은 검토 표본의 TP/(TP+FP)이며 전체 corpus의 추정치가 아니다.** 중첩 식이 겹치므로 sites/functions 합은 고유 함수·지점 총수와 다르다.

| 시점 | 모드 | 전체 군집 | sites / functions 합 | TP / FP / 불명 | 표본 precision | 고정 seed recall |
|---|---|---:|---:|---:|---:|---:|
| 앞 턴 | exact | 62 | 252 / 213 | 21 / 0 / 41 | 100.0% | 25.0% |
| 앞 턴 | normalized | 125 | 948 / 856 | 7 / 14 / 104 | 33.3% | 25.0% |
| 수리 후 | exact | 64 | 266 / 225 | 23 / 0 / 41 | 100.0% | 50.0% |
| 수리 후 | normalized | 126 | 965 / 871 | 7 / 14 / 105 | 33.3% | 50.0% |

기존 normalized FP 14군집은 서로 다른 정책의 로컬 bool/Option을 alpha 동치로 합친 사례다. 대표는 `9090fd016b10`(101곳, `src/cli/doctor/mailbox/observation.rs:45`), `406697d08cbe`(91곳, `src/cli/client.rs:646`), `c7e52521a68e`(80곳, `src/config/session_hosts/migration.rs:318`)다. exact의 let-chain 부분식 한 군집은 계속 불명이다.

새로 추출된 군집:

기존 is_some 군집도 직접 span 추출로 증가했다: exact `c288fb9d4257` 4→7곳, `557d724bd8a9` 3→5곳, normalized `fd898f27e425` 9→15곳이다. 신규 군집과 이 증가분을 합쳐 exact sites 252→266, normalized 948→965가 된다.

| 모드 / hash 앞12자 | 판정 | sites / functions | 전수 file:line |
|---|---|---:|---|
| exact / fae23cbcf7ab | TP | 6 / 4 | `src/services/discord/commands/config/provider_settings.rs:190`; `src/services/discord/commands/control/clear_teardown.rs:81`; `src/services/discord/commands/control/clear_teardown.rs:115`; `src/services/discord/queue_dispatch.rs:253`; `src/services/discord/router/message_handler/intake_turn.rs:1110`; `src/services/discord/router/message_handler/intake_turn.rs:1754` |
| exact / fdb65fa0e88f | TP | 3 / 3 | `src/services/discord/health/turn_deliver/inject.rs:312`; `src/services/discord/health/turn_deliver/inject/herdr.rs:48`; `src/services/discord/turn_presence/activity/unbound.rs:525` |
| normalized / ccaab0c23194 | 불명 | 11 / 9 | `src/services/discord/commands/config/provider_settings.rs:190`; `src/services/discord/commands/control/clear_teardown.rs:81`; `src/services/discord/commands/control/clear_teardown.rs:115`; `src/services/discord/commands/control/herdr_recovery.rs:28`; `src/services/discord/health/turn_deliver/inject.rs:312`; `src/services/discord/health/turn_deliver/inject/herdr.rs:48`; `src/services/discord/queue_dispatch.rs:253`; `src/services/discord/queue_io/input_owner/claude_handoff.rs:219`; `src/services/discord/router/message_handler/intake_turn.rs:1110`; `src/services/discord/router/message_handler/intake_turn.rs:1754`; `src/services/discord/turn_presence/activity/unbound.rs:525` |

normalized 새 부정 predicate 군집은 unverified receiver 3지점을 포함하여 불명으로 남겼다. `src/services/discord/queue_io/input_owner/claude_handoff.rs:219`의 Channel alias, `src/services/discord/router/message_handler/intake_turn.rs:1110,1754`의 지역 binding은 scanner가 ChannelId로 입증하지 못하며 `normalization_notes`를 보존한다. 두 모드 전체 ambiguous 군집은 exact 0, normalized 1이다. recall은 고정 지점이 한 군집에 나타나는지 세는 검출 지표로, 의미 동치의 증명이 아니다.

**채택: exact warning을 기본으로 사용하고 normalized는 조사 옵션으로 유지한다. hard gate는 채택하지 않는다.** exact는 검토 표본 FP 0이고 이번 predicate 추출로 고정 seed recall도 1/4→2/4다. `hard_gate=False`, severity warn과 실제 harness `--check` 반환 0을 시험한다. 분석 오류는 `condition scan failed` 경고로 남겨 결과 0건으로 숨기지 않는다.

## 고정 seed와 첫 묶음 순서

coverage는 시점별 모든 검출 군집의 seed 지점 합집합이며, recall은 한 군집에 고정 지점 3개 이상이 함께 나타나는 기준이다. positive 4군집(H 두 종류·이름·반응) 및 지점 분모를 유지했다. 고정 지점 3개 이상이 한 군집에 함께 나타나면 검출로 센다. 앞 턴 H-negative의 첫 locator 1111은 다음 guard 줄이므로 실제 accessor 좌표 1110으로 교정했다. 군집/지점 분모와 앞 턴 검출 결과는 바뀌지 않는다.

| positive seed | exact coverage 전→후 | normalized coverage 전→후 | 고정 지점 전수표 |
|---|---:|---:|---|
| H-presence | 7/9 → 7/9 | 9/9 → 9/9 | `src/config/session_hosts.rs:355`; `src/services/discord/commands/control.rs:466`; `src/services/discord/commands/control/clear_teardown/clear_end_record.rs:45`; `src/services/discord/commands/tui_passthrough.rs:128`; `src/services/discord/router/message_handler/tui_followup/nameless_gate.rs:79`; `src/services/discord/session_runtime.rs:513`; `src/services/discord/tui_prompt_relay.rs:757`; `src/services/herdr_launch.rs:62`; `src/services/tui_o/writer/actor.rs:461` |
| H-negative | 0/4 → 4/4 (한 군집 최대 3) | 0/4 → 4/4 | `src/services/discord/router/message_handler/intake_turn.rs:1110`; `src/services/discord/router/message_handler/intake_turn.rs:1754`; `src/services/discord/queue_dispatch.rs:253`; `src/services/discord/health/turn_deliver/inject.rs:312` |
| N-namespace | 0/7 → 0/7 | 0/7 → 0/7 | `src/services/discord/relay_recovery/decision.rs:252`; `src/services/discord/health/watcher_respawn.rs:167`; `src/services/discord/health/o_transition/codex_census.rs:47`; `src/services/discord/recovery_engine/state_extractors.rs:139`; `src/services/discord/tmux_reaper/host_guard.rs:155`; `src/services/discord/turn_presence/activity/unbound.rs:463`; `src/services/discord/turn_presence/activity/unbound/hold_leaf.rs:70` |
| R-display | 0/4 → 0/4 | 0/4 → 0/4 | `src/services/discord/router/message_handler/pre_admission_control.rs:223`; `src/services/discord/router/message_handler/pre_admission_control.rs:235`; `src/services/discord/router/message_handler/intake_turn.rs:541`; `src/services/discord/router/message_handler/intake_turn/race_loss.rs:426` |

H-presence와 H-negative를 검출한다. N-namespace는 단일 문자열·format·macro 의미 해석이 등록 predicate가 아니어서 FN, R-display는 같은 정책 부분 앞뒤 guard가 달라 FN이다. 정책 호출 자체가 빠진 BusyActiveTurn 경로도 잡지 못하며 공통 정책과 E2E가 담당한다. 첫 묶음 추출 순서는 host → 이름 해석(#8119 뒤) → 반응 표시(#8113 뒤)다. 기존 직접 reaction owner는 재구현하지 않는다.

## 검증·한계·등록

수리 계약은 이름만 다른 세 식의 기본 무경고/normalized 군집, Fn/FnMut/FnOnce bound의 alpha 2→3, 서로 다른 guard의 등록 predicate 검출·polarity/인자/field 보존 시험으로 고정했다. 기본 normalized 복원·generic skip 제거·predicate 추출 제거 뮤턴트는 각각 assertion RED다. 기존 helper 재호출·literal/field/call 순서·가짜 문자열·cfg(test) 제외 시험도 유지한다.

macro expansion, 복잡한 destructuring·let-chain, 임의의 공통 부분식, 문자열 의미 해석과 타입·값 의미 증명은 지원하지 않는다. cfg target별 운영 그래프 합집합을 검사하고 test-only module/item은 제외한다. 그래프 밖 코드나 빠진 정책 경로는 감지하지 못한다. 순수 bool operand의 이름만 바뀌면 alpha 동치가 될 수 있다.

등록 절차 한 줄: 세 번째 복사를 보면 소유 공통 함수로 모으고 새 raw accessor는 A RULES(B 자동 재사용) 또는 B ACCESSORS에 등록한 뒤 2→3·음성 fixture와 census를 검토한다. B 승인 baseline은 없고 결과는 위 CLI로만 재생성한다. A baseline·allowlist·임계값은 변경하지 않는다.

브리프·리뷰 한 줄: `같은 판정이 세 번째 복사되면 소유 공통 함수로 모은다. 새 저수준 접근자 확산은 rule·owner·generated baseline·전수표를 함께 검토하고 반복 조건식은 exact/정규화 precision과 고정 seed recall을 함께 낸다. 공통 함수 호출 반복은 복사가 아니다.`
