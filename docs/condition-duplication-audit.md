# 반복 조건식 검사 실측·운용

조사 corpus는 main `33d1f4b5a9dce7023e2d0014e45a05ed6c530bba`의 운영 Rust 모듈 그래프다. 검사 A의 lexer·literal·scope·alias 계약과 cfg/test 제외를 재사용한다. Rust 변경·실행·cargo·운영 관측은 없다.

서로 다른 `path:line:column` 세 곳 이상인 복합 논리식과 등록 raw accessor predicate를 경고한다. 단일 고수준 helper 호출은 재사용으로 제외한다. `if`/`else if`/`while`, match guard, bool let·tail·closure를 lexical container에서 찾는다. 단일 accessor 후보는 site의 `kind=known_accessor`로 별도 표시한다.

exact는 공백·주석·숫자 구분자·단일 atom의 불필요 괄호를 정리한다. normalized는 여기에 명시된 인자·let·closure·match-arm 로컬을 alpha-rename하고 등록 accessor의 qualification/alias·단순 receiver를 정리한다. accessor 인자의 bare local `.get()`만 정규화하며, ChannelId 확인이 안 되면 `ambiguous=true`와 이유를 남긴다. field·literal·call·operator·short-circuit 순서와 local 재참조 관계는 보존한다. accessor 밖의 `.get()`과 effectful receiver/argument는 지우지 않는다.

재현 명령(결과 파일은 승인 baseline이 아니다):

```sh
PYTHONPATH=scripts python3 -m audit_maintainability.checks.duplicate_conditions --mode both > target/condition-census.json
python3 -m unittest -v tests.test_duplicate_conditions
python3 scripts/audit_maintainability.py --check
```

## main 전수 결과와 채택

TP는 같은 판정 로직의 복사 후보, FP는 의미가 다른 지역 값들이 같은 모양으로 합쳐진 군집, 불명은 미검토 또는 lexical 해석 불확실 군집이다. 군집 크기 내림차순 상위 20개와 고정 host seed 군집을 검토하고, normalized는 별도로 확인한 UTF-8 경계 군집도 포함했다. exact의 let-chain 잘린 식 하나는 불명으로 남겼다. **아래 precision은 검토 표본의 TP/(TP+FP)이며 corpus 전체 precision 추정치가 아니다.** 나머지 군집을 FP/TP로 간주하지 않았다. sites/functions 합은 군집별 합으로, 겹치는 하위 식이 있으면 중복 집계된다.

| 모드 | 전수 군집 | sites / functions 합 | TP / FP / 불명 군집 | 검토 표본 precision | 고정 positive seed recall |
|---|---:|---:|---:|---:|---:|
| exact | 62 | 252 / 213 | 21 / 0 / 41 | 100.0% | 25.0% (1/4) |
| normalized | 125 | 948 / 856 | 7 / 14 / 104 | 33.3% | 25.0% (1/4) |

normalized 상위 판정표(불명은 위 합계에 포함):

| hash 앞 12자 | 판정 | sites / functions | 대표 file:line | 근거 |
|---|---|---:|---|---|
| 9090fd016b10 | FP | 101 / 87 | `src/cli/doctor/mailbox/observation.rs:45`; `src/cli/doctor/mailbox/observation.rs:146` | 서로 다른 의미의 로컬 bool/Option을 같은 모양으로 합침 |
| 406697d08cbe | FP | 91 / 77 | `src/cli/client.rs:646`; `src/cli/doctor/orchestrator.rs:1690` | 서로 다른 의미의 로컬 bool/Option을 같은 모양으로 합침 |
| c7e52521a68e | FP | 80 / 70 | `src/cli/doctor/mailbox/observation.rs:48`; `src/config/session_hosts/migration.rs:318` | 서로 다른 의미의 로컬 bool/Option을 같은 모양으로 합침 |
| 56ca6cc536f3 | FP | 31 / 29 | `src/dispatch/dispatch_cancel.rs:575`; `src/engine/loader.rs:604` | 서로 다른 의미의 로컬 bool/Option을 같은 모양으로 합침 |
| 76ca70a00f53 | FP | 27 / 27 | `src/cli/direct.rs:114`; `src/cli/migrate/plan.rs:1110` | 서로 다른 의미의 로컬 bool/Option을 같은 모양으로 합침 |
| 868701a27ff7 | FP | 17 / 17 | `src/services/claude/host_gate.rs:125`; `src/services/codex/process_session_launch.rs:42` | 서로 다른 의미의 로컬 bool/Option을 같은 모양으로 합침 |
| b2991a7306e7 | FP | 16 / 16 | `src/kanban/terminal_cleanup.rs:83`; `src/services/auto_queue/control_routes.rs:420` | 서로 다른 의미의 로컬 bool/Option을 같은 모양으로 합침 |
| d3c28058a974 | FP | 16 / 16 | `src/cli/doctor/orchestrator/api_exposure.rs:15`; `src/dispatch/dispatch_context.rs:344` | 서로 다른 의미의 로컬 bool/Option을 같은 모양으로 합침 |
| f5fa07ed4153 | FP | 15 / 14 | `src/server/routes/agents_turn_deliver.rs:160`; `src/services/codex/absence_launch.rs:234` | 서로 다른 의미의 로컬 bool/Option을 같은 모양으로 합침 |
| 5c9175ce9d50 | FP | 13 / 13 | `src/cli/doctor/mailbox/observation.rs:47`; `src/dispatch/dispatch_context.rs:542` | 서로 다른 의미의 로컬 bool/Option을 같은 모양으로 합침 |
| 89726c351bdc | TP | 13 / 10 | `src/services/claude_tui/hosting/followup_support.rs:275`; `src/services/claude_tui/hosting/followup_support.rs:279` | 공백·NBSP 판정 복사 |
| f4761494ceac | FP | 13 / 12 | `src/services/auto_queue.rs:545`; `src/services/claude/herdr_turn.rs:413` | 서로 다른 의미의 로컬 bool/Option을 같은 모양으로 합침 |
| 0f3326733049 | FP | 11 / 11 | `src/db/dispatched_sessions/canonical_identity.rs:558`; `src/kanban/terminal_cleanup.rs:178` | 서로 다른 의미의 로컬 bool/Option을 같은 모양으로 합침 |
| 58fb81018941 | FP | 11 / 11 | `src/services/codex_tui/busy_inject/rollout.rs:190`; `src/services/codex_tui/rollout_tail/following_source.rs:247` | 서로 다른 의미의 로컬 bool/Option을 같은 모양으로 합침 |
| af8089eed986 | TP | 11 / 11 | `src/services/discord/health/recovery.rs:397`; `src/services/discord/inflight/clear_store/mod.rs:852` | 복구 identity 판정 복사 |
| bdc98524b1da | TP | 11 / 11 | `src/server/routes/agents_turn_deliver.rs:112`; `src/services/claude/session_handoff/pinned_tmux.rs:116` | 비어 있지 않은 ASCII 숫자 판정 |
| 605c2945453b | FP | 10 / 10 | `src/server/routes/health_api/session_repair.rs:106`; `src/services/discord/commands/control/clear_teardown/clear_end_record.rs:46` | 서로 다른 의미의 로컬 bool/Option을 같은 모양으로 합침 |
| e1b448eb8556 | TP | 10 / 9 | `src/services/discord/router/intake_gate/queue_effects.rs:63`; `src/services/discord/router/intake_gate/queue_effects.rs:203` | 원본/병합 message identity 판정 |
| 1f314bd5f276 | TP | 9 / 9 | `src/services/discord/inflight/save_store/identity_gate.rs:143`; `src/services/discord/inflight/save_store/identity_gate.rs:290` | user id 없는 turn identity 판정 |
| fd898f27e425 | TP | 9 / 9 | `src/config/session_hosts.rs:355`; `src/services/discord/commands/control.rs:466` | 등록 host endpoint presence 판정 |
| a31959b98ce7 | TP | 8 / 8 | `src/db/prompt_manifests/redaction.rs:26`; `src/db/session_observability.rs:297` | UTF-8 경계 되감기 |

**채택: 기본 normalized 경고를 CI·prepush의 기존 maintainability audit에 연결한다. hard gate는 채택하지 않는다.** 지역 bool 이름을 지우면 다른 정책까지 합쳐지는 FP가 많고 known seed FN도 남는다. exact 모드는 별도 정밀 조사에 쓴다. `warn`과 `hard_gate=False`를 함께 고정하고 실제 `--check` 반환 0도 시험한다. 분석 오류는 `condition scan failed` 경고로 명시하며 분석 결과 0건으로 숨기지 않는다.

## 고정 seed와 첫 묶음 순서

설계 부록 A/A2의 첫 대상에서 positive 4군집(H 두 종류·이름·반응)을 고정했다. 한 군집에 고정 지점 3개 이상이 함께 들어오면 검출로 센다. FN/미지원을 분모에서 빼지 않는다. 아래는 해당 seed 지점 coverage이며 전체 candidate site 수와 다를 수 있다.

| positive seed | exact 지점 coverage | normalized 지점 coverage | 고정 호출 지점 전수표 |
|---|---:|---:|---|
| H-presence | 7/9 | 9/9 | `src/config/session_hosts.rs:355`; `src/services/discord/commands/control.rs:466`; `src/services/discord/commands/control/clear_teardown/clear_end_record.rs:45`; `src/services/discord/commands/tui_passthrough.rs:128`; `src/services/discord/router/message_handler/tui_followup/nameless_gate.rs:79`; `src/services/discord/session_runtime.rs:513`; `src/services/discord/tui_prompt_relay.rs:757`; `src/services/herdr_launch.rs:62`; `src/services/tui_o/writer/actor.rs:461` |
| H-negative | 0/4 | 0/4 | `src/services/discord/router/message_handler/intake_turn.rs:1111`; `src/services/discord/router/message_handler/intake_turn.rs:1754`; `src/services/discord/queue_dispatch.rs:253`; `src/services/discord/health/turn_deliver/inject.rs:312` |
| N-namespace | 0/7 | 0/7 | `src/services/discord/relay_recovery/decision.rs:252`; `src/services/discord/health/watcher_respawn.rs:167`; `src/services/discord/health/o_transition/codex_census.rs:47`; `src/services/discord/recovery_engine/state_extractors.rs:139`; `src/services/discord/tmux_reaper/host_guard.rs:155`; `src/services/discord/turn_presence/activity/unbound.rs:463`; `src/services/discord/turn_presence/activity/unbound/hold_leaf.rs:70` |
| R-display | 0/4 | 0/4 | `src/services/discord/router/message_handler/pre_admission_control.rs:223`; `src/services/discord/router/message_handler/pre_admission_control.rs:235`; `src/services/discord/router/message_handler/intake_turn.rs:541`; `src/services/discord/router/message_handler/intake_turn/race_loss.rs:426` |

H-presence만 검출된다. H-negative는 larger condition의 공유 부분과 서로 다른 helper/조건 조합이어서 FN, N-namespace는 단일 문자열/format/macro 해석이 등록 predicate가 아니어서 FN, R-display는 같은 정책 부분 앞뒤의 guard가 달라 FN이다. 정책 호출 자체가 없는 BusyActiveTurn 같은 경로는 감지하지 못하며 공통 정책 함수와 E2E가 담당한다. 첫 묶음의 추출 순서는 host → 이름 해석(#8119 뒤) → 반응 표시(#8113 뒤)다. 직접 reaction 호출은 이미 설계가 지정한 owner에 모여 있어 재구현하지 않는다.

고정 negative는 `test_plain_helper_reuse_is_not_copied_logic`, `test_literals_order_fields_and_variable_identity_are_preserved`, `test_get_and_fields_outside_registered_accessor_stay_distinct`, `test_string_code_alone_cannot_supply_the_third_site`다. helper 재호출은 5회여도 0, 다른 literal/field/call 순서의 세 번째 식과 문자열 속 가짜 코드는 군집을 완성하지 못한다. threshold4·정규화 제거·&& 정렬·string 오인·known accessor 누락 뮤턴트가 각각 assertion RED다.

## 한계와 등록 절차

macro는 소스 토큰만 보며 expansion 결과는 보지 않는다. cfg target별 운영 코드의 합집합을 검사하고 test-only module/item은 제외한다. 타입·값 의미를 증명하지 않으며 복잡한 destructuring·let-chain, 임의의 공통 부분식, 문자열 의미 해석을 완전히 지원하지 않는다. 모듈 그래프 밖 코드나 누락된 정책 경로는 분모 밖이다. 두 operand의 이름만 서로 바뀐 순수 bool 식은 alpha 동치가 될 수 있으므로 FP 검토가 필요하다.

등록 절차 한 줄: 세 번째 복사를 보면 소유 공통 함수로 모으고, 새 raw accessor는 A `RULES`(B가 자동 재사용) 또는 B `ACCESSORS`에 등록한 뒤 2→3·음성 fixture와 census를 함께 검토한다. B에는 승인 baseline이 없으며 결과 갱신은 위 생성 명령으로 한다. A의 기존 baseline·allowlist·threshold는 이 변경에서 건드리지 않는다.

브리프·리뷰 틀에 넣을 한 줄 제안: `같은 판정이 세 번째 복사되면 소유 공통 함수로 모은다. 새 저수준 접근자 확산은 rule·owner·generated baseline·전수표를 함께 검토하고, 반복 조건식은 exact/정규화 precision과 고정 seed recall을 함께 낸다. 공통 함수 호출 반복은 복사가 아니다.`
