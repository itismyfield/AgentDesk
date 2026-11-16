# 활성 Claude 턴 busy source 선택 (#8114 실기 수리)

기준 코드 `406da315a7`. 새 producer나 source 권한을 만들지 않고, 기존 `HerdrBusyReobserve`의 검증된 source 계약을 활성 mailbox 소비 경로에 적용한 작은 확장이다. 기존 83 cfg와 세 모델(TLA)은 기준 바이트 그대로다. 새 모델은 `EXTENDS HerdrBusyReobserve`로 기존 상수·변수를 재사용하지만 별도 `ActiveSpec`과 10 cfg를 실행한다. 모델 결과는 Rust 재현 시험의 대체 증거가 아니다.

## 코드 근거와 값 범위

| 모델 값·행동 | 기준 코드 좌표 | 의미 |
|---|---|---|
| `inflight=Absent` | `src/services/discord/router/message_handler/provider_isolation.rs:153-165`; `src/services/discord/router/message_handler/intake_turn.rs:1881` | ClaudeTui 활성 행은 정상적으로 output_path=None에서 시작한다. |
| `inflight=Old` | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:341-393` | 활성 턴 경로는 frozen inflight output/native로 Registration을 만들고 observation=None을 전달한다. |
| native/path/dev/ino의 Old→Current | `src/services/tui_prompt_dedupe/runtime_binding.rs:859-883`; `src/services/tui_prompt_dedupe/runtime_binding/claude_source.rs:254` | 정상 hook adoption이 검증된 새 세션·파일·history pin을 발행한다. |
| channel/nonce, registry/pin/current 일치 | `src/services/claude_tui/host_input/busy/herdr.rs:307-349` | 기존 current_source가 검증하는 channel의 nonce history, native/path, opened file identity다. |
| source snapshot+request 선택 | `src/services/discord/router/message_handler/intake_turn/owned_busy.rs:223-259` | prepare는 observation의 동일 source로 Registration/attempt/request를 채운다. |
| `SubmitActive` | `src/services/claude_tui/host_input/busy/herdr.rs:53-57,79-83,107-113`; `src/services/claude_tui/host_input/busy.rs:231-248` | 빈 path는 metadata에서 Held. 관측한 source는 첫 키 직전 immutable snapshot과 재독 값이 같아야 한다. |
| 활성 행이 뒤늦게 채워짐 | `src/services/claude/herdr_turn/first_input.rs:204-265,279,299`; `src/services/discord/turn_bridge/runtime_handoff_loop.rs:751`; `src/services/discord/turn_bridge/runtime_handoff_loop/watcher_handoff.rs:88` | native terminal 뒤 RuntimeReady를 받아야 frozen 행의 path가 채워진다. |

SourceId의 네 값만 tuple로 묶었다. session/path/ino=1은 옛 값, 2는 현재 값, 3은 다른 source; 0은 없는 값이다. dev=1은 동일 장치다. channel/nonce는 SourceId의 필드가 아니라 별도 검증 문맥이며 정상값 1, 다른 채널값 2다. 새 상태 종류나 운영 API를 가정하지 않는다.

`ChangeSession → PublishSource → PersonInput` 순서를 강제하여 정상 현재 source가 이미 발행된 뒤 턴 도중 입력을 검사한다. source가 없어서 아무 일도 안 한 PASS를 막는 `ActiveScenarioReached`를 모든 cfg가 검사한다. prior native turn의 종료 행동은 없으며 local 동작에 weak fairness를 둔다. 따라서 진행성 GREEN은 턴 종료를 기다리지 않는 경로가 있다는 뜻이고 벽시계 지연 상한은 아니다.

## 변경 전 RED / 변경 후 GREEN

| 정상 운영 조건 | Before | Fixed |
|---|---|---|
| 활성 행 path 없음 + session 교체 뒤 검증 source 있음 | `MissingBefore`: HealthyActiveBusyDelivered RED | `MissingFixed`: GREEN |
| 활성 행은 옛 native/path + 새 source 검증 완료 | `OldBefore`: NoForeignSourceKey RED | `OldFixed`: GREEN |
| source Unknown | 기존 검증 경로의 거부 계약 | `UnknownFixed`: Held, 키0 GREEN |
| 다른 native/path/file tuple | 기존 검증 경로의 거부 계약 | `ForeignSourceFixed`: Held, 키0 GREEN |
| 다른 채널의 source 문맥 | 기존 검증 경로의 거부 계약 | `ForeignChannelFixed`: Held, 키0 GREEN |

OldBefore는 입력 대상 pane을 바꾸는 반례가 아니다. pane에 보낼 본문/receipt가 현재 native와 다른 transcript에 귀속되는 것을 `NoForeignSourceKey`가 잡는다. frozen path가 실제로 존재하면 metadata 실패만으로 이 결함을 막지 못한다.

| 원인 제거 mutant | 결과 |
|---|---|
| `NoBindingCheck`: 검증 source 대신 inflight 경로를 사용하고 observation 검사 제거 | NoForeignSourceKey RED |
| `KeepOldSession`: snapshot은 현재인데 request는 옛 inflight session/path 유지 | HealthyActiveBusyDelivered RED |
| `UnknownBusy`: source Unknown을 Busy로 승격 | NoKeyWithoutVerifiedSource, UnknownActiveHeld RED |

공식 checker 실제 결과: **10/10 cfg 기대집합 일치, GREEN5 / expected RED5, safety·property TLC33회 + fairness10회, 실패·오류·미검사0**, 29.7초, checker exit0. checker exit0은 expected RED가 없다는 뜻이 아니다. 모든 config 이름·규칙 집합을 `results.json`과 표로 대조했다.

## 차트와 반례 경로

- [Before 빈 활성 행](herdr-busy-active-source.before.chart.mmd): HealthyActiveBusyDelivered, “검증된 현재 source가 있으면 턴 종료 전에 전달한다.” 경로0–5: 기존 턴→세션 교체→정상 source 발행→입력 도착→빈 행 경로 선택→키0 Held에 영구 정지.
- [Fixed](herdr-busy-active-source.chart.mmd): 같은 입력에서 현재 verified snapshot을 선택하여 한 번 보내고 receipt로 정산한다.
- [옛 경로 Before](herdr-busy-active-source.old-before.chart.mmd): NoForeignSourceKey, “다른 native source로 키 실행0.” 경로0–5에서 옛 inflight source를 고른 뒤 첫 키를 기록한다.
- [Unknown 승격 mutant](herdr-busy-active-source.unknown-mutant.chart.mmd): NoKeyWithoutVerifiedSource 및 UnknownActiveHeld, “Unknown은 Held로 유지하고 키0.” source Unknown을 선택한 경로0–5가 키 실행까지 간다.

view는 null로 숨기는 action이 없다. Wait와 같은 표시 안의 receipt proof 변화만 차트 내부로 접힌다. Before chart는6states/11steps, Fixed TLC는8states/15steps이고 차트에는7states/6arrows를 그린다. 대표 statechart 실행 exit는 Before1/Fixed0/old-before1/unknown-mutant1이다.

## 한계·재현·증거

한 front·한 번의 세션 교체·현재 실행의 고정 channel/nonce를 다룬다. 복수 입력 소유권·중복 owner route·machine 입력 분류·전송 불확실성·부팅/FD/DB 원자성은 기존 입력 모델과 Rust 시험의 계약이다. PublishSource는 정상 hook의 opened-first-record·identity 검증과 append-before-publish를 추상화한다. 그 producer를 수정하거나 binding 검사 없이 fallback을 허가하지 않는다. 실제 수정은 관측을 우회한 활성 입력 소비자의 source 선택만 바꾼다.

원시 로그: `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8114/logs/busy-source-model/`. `source/`는 실행 모델·cfg·expected snapshot, `source-manifest.json`은 SHA256, `results.json`은 config별 실제 규칙/실행수, `raw/<cfg>/{fairness,runN}.out`는 모든 JVM 원문과 exit다. `unchanged-existing-models.json`의86개가 전부 true다. 새 expectation은 expected.tsv에10행 추가했고 기존 행은 바꾸지 않았다.

```sh
TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar \
JAVA=/opt/homebrew/opt/openjdk/bin/java \
python3 scripts/ci/statecharts_tlc.py --root <새10cfg-snapshot-root> --jobs 2
~/.codex/skills/statechart/scripts/statechart <snapshot>/HerdrBusyActiveSource.tla \
  --config <snapshot>/HerdrBusyActiveSourceMissingFixed.cfg \
  --view docs/statecharts/input-single-path/herdr-busy-active-source.view.mjs \
  --out <새-output-folder>
```

색인은 `docs/statecharts/index.d/690-input-single-path-herdr-active-source.md`에 행을 추가한 뒤 공식 `python3 scripts/ci/statechart_index.py`로 생성한다(README 순증1행). cargo/production Rust/운영 접근/커밋·Git index 쓰기는 모델 담당에서0이다.
