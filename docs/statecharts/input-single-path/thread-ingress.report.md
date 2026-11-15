# API thread ingress 상태 차트 — #8125 설계 D2

코드 기준: **main `ad04f6f3ecc33e86e7dc0968822ce2ce3f087e90`**. 설계1 `2b142514b6`의 ThreadIngress/ThreadIngressRace를 갱신했다. 생산 코드·운영 상태 변경, cargo·Rust/PG 수용 시험은 0이다. 모델 GREEN은 구현·운영 수용 완료가 아니다. #8118 O-b는 merge하지 않았다.

TIME start=2026-11-16T01:58:47+09:00 / end=2026-11-16T02:14:35+09:00 elapsed=15m48s / budget=30m. 외부 입력의 시작을 #8184의 Idle delegation 및 owned Busy port에 연결했고, 기존 cold/remote 설계 포트는 그대로 보존했다.

## 코드 진입점과 값의 출처

[설계 전수표 §2](../../api-thread-ingress.design.md)의 E01–E22와 N01–N07가 값 범위의 정본이다. 아래 값은 운영 설정을 읽거나 추측해 넣은 값이 아니다.

| 모델 값·행동 | main 코드 근거 | 투영한 계약 |
|---|---|---|
| `checked/denied/queued`, `hstatus={0,200,403,422,503}` | `agents_turn_target.rs:59,65`; `agents.rs:892,894`; `agents_turn_deliver.rs:345,347`; `message_handler/headless_turn.rs:159`; `turn_host.rs:177,191,203,208` | deterministic 권한/host/runtime 거절은 custody와 효과 이전. main generic ExecutorNotWired는 **422**; runtimeUnavailable는 503. 새 public start가 열렸다는 기능 규칙은 `HerdrExternalWired`로 별도 검사 |
| `hactivity={idle,busy,booting}`, `hprobe={none,idle,busy}` | N01 `owned_busy.rs:18,38,46`; N02 `:150,157,173,190`; N04 `busy/herdr.rs:211,215,218,237` | Idle/Busy/Held 관측값과 준비 중 보관을 분리. booting은 기존 부팅/restore/ready port의 유계 상태이며 새 실행 권한이 아님 |
| `hreserved` 및 `HReturnRead` | N04 `busy/herdr.rs:252,258,277,280,288` | 공개 readiness 읽기예약은 **키 0**으로 반환한 다음 기존 Idle driver에 위임. 위임 자체는 물리 입력이 아님 |
| `hepoch={1,2}`, `hreadEpoch={0,1,2}` | N03 `claude_handoff.rs:464,475`; N04 `busy/herdr.rs:258,280,297`; N07 `owner_input.rs:690,701,707`; `restore_inflight.rs:128,140,153,204` | 실제 nonce/generation 숫자 1/2가 아니다. prior/current execution nonce·process generation을 최대 1회 교체하는 **bounded projection**, 0은 proof 없음. owner node/token/root/provider/logical/channel/source identity도 이 비교 포트에 포함 |
| `HBegin`, `hstarted`, `hdelivered={0,1,2(mutant)}` | N05 `busy/herdr.rs:71,75,90,96,106,111`; `provider_dispatch/owner_input.rs:1470`; `provider_dispatch.rs:338,343` | `HBegin`은 실제 첫 native prompt/key 효과이며 여기서 카운트를 올린다. Idle은 native turn 시작, Busy person은 이미 시작된 native turn에 해당 입력이 합류. `HReceipt`는 후속 ACK 관측뿐이며 효과 카운트를 다시 올리지 않음 |
| `HRestart`, `HUncertainFailure` | E14/N07 복원 및 기존 Unconfirmed/owed notice; N05 keyed 뒤 Uncertain | 효과 **전** 재시작만 proof를 버리고 새 incarnation으로 재관측. 효과 **뒤** 재시작은 uncertain→실제로 보이는 실패 포트, 재전송 0. ACK 전 crash를 다시 submit해 effect 2를 숨기지 않음 |
| `horigin=externalAPI`, Race `origins` | `input_owner/origin.rs:26,31,42,49,59`; `start.rs:21,32,43,47` | `externalAPI`는 API에서 온 canonical provenance의 추상값이며 실제 source 문자열을 바꾸라는 뜻이 아님. iMessage source/GUID는 그대로다. Race는 `imessageStart`, `humanApiDeliver`, `discordPerson`의 **서로 다른 사람 original** 3개. generic HttpStart나 routine을 Person으로 승격하지 않음 |
| `HNativeEnd` | `owner_input.rs:723,727,734,887`; `owned_busy.rs:18` | non-person HttpStart는 Busy person injector를 빌리지 않고 기존 턴 종료 후 Idle own-turn을 기다림. MachineBusy cfg에서 실제 starts liveness 검사 |
| `haccepted/hretained`, `hfailure/hreply` | E11 owner admission, E15 wake, 기존 owed notice/output owner | 채택 전 거절과 채택 뒤 held/실패를 분리. 보이는 실패·reply는 실제 output 성공의 추상 포트이며 시도/로그를 성공으로 세지 않음. 기존 bound channel output은 main; child output은 O-b 수용이 필요한 조건부 포트 |
| Race `owner`, `reservation`, `begun`, `readEpoch`, `restarts` | E09 actual child transition, N02/N04/N05/N06/N07 | 동일 채널 owner 아래 3 original을 순차 효과로 조인. first-key 효과 뒤 crash는 `uncertain`으로 분리하고 `HShowFailure`에서만 visible failed. old proof로 effect를 내는 mutant는 `HerdrCurrentExecutor` RED |

`ThreadIngress.Spec`와 `ThreadIngressRace.Spec`는 설계1의 기존 경계를 그대로 둔다. 추가 `HerdrSpec`은 **외부 API admission을 올바르게 연결한 뒤** 소비할 main #8184의 owned Busy/Idle 포트를 분리해 검사한다. early 관측은 intake 전용(N01)이고 외부 headless 시작은 late 관측(N02)을 소비해야 한다.

## 지키는 규칙과 liveness

- safety: native 효과/답 중복 0(`HerdrExactlyOnce`/`NoDuplicate`), custody 없는 조용한 유실 0, 거절은 명시 403/422/503과 효과·lease 0, Idle 판독예약 반환, 현재 incarnation만 효과, canonical 출처 보존.
- healthy accepted liveness: `HerdrAcceptedStarts`, Race `EachAcceptedStarts`를 실제 `PROPERTY`로 실행한다. starts는 readiness/lease만으로 TRUE가 되지 않고 native first-key 효과 행동에서만 생긴다. busy 사람 입력은 기존 native turn 합류이며 같은 채널에 별도 own-turn을 중복 시작하지 않는다.
- `HerdrAcceptedCompletes` 및 `HerdrDeliveredGetsOutcome`는 실제 전달 또는 보이는 실패를 요구한다. 확정 실패 cfg는 healthy starts 요구를 따로 적용하지 않는다. post-effect crash는 불명 상태의 자동 재전송 없이 보이는 실패로 종결한다.
- fairness는 실제 ready/watch wake·native 턴 종료·writer/notice 성공의 유한 진행 포트에만 둔다. 영구 daemon/DB/Discord 장애나 영구 Busy의 진행을 증명하지 않는다. `SYMMETRY` 없음.

## 실제 수정 전/후 — 기존 모든 liveness cfg

설계1 bytes를 `before/`로 복제해 공식 `scripts/ci/statecharts_tlc.py`로 **19cfg 모두** 실행했다. 갱신 후에도 아래 기대집합은 바뀌지 않았다. `-`는 모든 safety/temporal property 성립이며, 나머지는 named expected RED 집합이다.

| cfg | D1 수정 전 actual broken | D2 수정 후 actual broken |
|---|---|---|
| `ThreadIngressBefore.cfg` | BoundThreadAdmitted | BoundThreadAdmitted |
| `ThreadIngressBoundChannel.cfg` | - | - |
| `ThreadIngressDoubleSubmit.cfg` | AcceptedCompletes,NoDuplicate,NoSilentLoss,SuccessBacked | AcceptedCompletes,NoDuplicate,NoSilentLoss,SuccessBacked |
| `ThreadIngressDropRestart.cfg` | AcceptedCompletes,DeliveredGetsOutcome,NoSilentLoss | AcceptedCompletes,DeliveredGetsOutcome,NoSilentLoss |
| `ThreadIngressFixedBoot.cfg` | - | - |
| `ThreadIngressFixedCold.cfg` | - | - |
| `ThreadIngressFixedColdTmux.cfg` | - | - |
| `ThreadIngressFixedLive.cfg` | - | - |
| `ThreadIngressFixedRestart.cfg` | - | - |
| `ThreadIngressForeign.cfg` | - | - |
| `ThreadIngressForeignChannel.cfg` | - | - |
| `ThreadIngressNoReadyWake.cfg` | AcceptedCompletes,NoDeadlock | AcceptedCompletes,NoDeadlock |
| `ThreadIngressParentFail.cfg` | - | - |
| `ThreadIngressParentMalformed.cfg` | - | - |
| `ThreadIngressParentOpen.cfg` | ParentFailClosed | ParentFailClosed |
| `ThreadIngressPolicyOverridesLive.cfg` | LiveActualHostPreserved | LiveActualHostPreserved |
| `ThreadIngressRaceFixed.cfg` | - | - |
| `ThreadIngressRaceStale.cfg` | NoStaleBootstrap | NoStaleBootstrap |
| `ThreadIngressRaceTwo.cfg` | - | - |

## 신규 Herdr cfg의 main gate baseline과 제안 후

`prewire-v4/`는 **동일 신모델 bytes에서 HAdmit의 executor gate만 항상 닫는 main projection**이다. main `headless_turn.rs:159→turn_host.rs:191`의 422를 투영한다. 새 cfg가 설계1에 없었다는 사실을 숨기지 않고, 동일 fixture의 closed-gate control을 별도로 실제 실행했다. baseline 14cfg에서 accepted=0이므로 temporal 성립은 vacuous이고 성공 증거가 아니다. 기능 불변식 `HerdrExternalWired`가 수락 경로 부재를 RED로 드러낸다. RuntimeUnready/Foreign은 제안 후에도 채택 전 거절·효과 0이다.

| 신규 cfg | main closed-gate actual broken / liveness | 제안 후 actual broken |
|---|---|---|
| `ThreadIngressHerdrBefore.cfg` | HerdrExternalWired / temporal 전부 성립(accepted 0) | HerdrExternalWired |
| `ThreadIngressHerdrBoot.cfg` | HerdrExternalWired / temporal 전부 성립(accepted 0) | - |
| `ThreadIngressHerdrBusy.cfg` | HerdrExternalWired / temporal 전부 성립(accepted 0) | - |
| `ThreadIngressHerdrDoubleSubmit.cfg` | HerdrExternalWired / temporal 전부 성립(accepted 0) | HerdrAcceptedCompletes,HerdrExactlyOnce,HerdrNoSilentLoss |
| `ThreadIngressHerdrFailure.cfg` | HerdrExternalWired / temporal 전부 성립(accepted 0) | - |
| `ThreadIngressHerdrForeign.cfg` | - / temporal 전부 성립(accepted 0) | - |
| `ThreadIngressHerdrIdle.cfg` | HerdrExternalWired / temporal 전부 성립(accepted 0) | - |
| `ThreadIngressHerdrKeepReservation.cfg` | HerdrExternalWired / temporal 전부 성립(accepted 0) | HerdrReservationReturned |
| `ThreadIngressHerdrMachineBusy.cfg` | HerdrExternalWired / temporal 전부 성립(accepted 0) | - |
| `ThreadIngressHerdrNoWake.cfg` | HerdrExternalWired / temporal 전부 성립(accepted 0) | HerdrAcceptedCompletes,HerdrAcceptedStarts,NoDeadlock |
| `ThreadIngressHerdrReclassify.cfg` | HerdrExternalWired / temporal 전부 성립(accepted 0) | HerdrSourcePreserved |
| `ThreadIngressHerdrRestart.cfg` | HerdrExternalWired / temporal 전부 성립(accepted 0) | - |
| `ThreadIngressHerdrRuntimeUnready.cfg` | - / temporal 전부 성립(accepted 0) | - |
| `ThreadIngressHerdrStaleEpoch.cfg` | HerdrExternalWired / temporal 전부 성립(accepted 0) | HerdrCurrentExecutor |

Race 신규 3cfg는 **이미 채택한 서로 다른 사람 original**의 owner 조인 포트이다. main에서 아직 거절하는 idle external start가 채택되었다고 가정한 뒤의 조건부 검사이며, main API 수락 성공의 증거로 사용하지 않는다. 수정 전 대응은 위 기존 Race 3cfg의 실제 결과이며 거기에는 Herdr proof/epoch 행동이 없었다. 새 Herdr 경합 속성은 Idle/Busy 정상 2cfg GREEN, old epoch mutant 1cfg `HerdrCurrentExecutor` expected RED다. 모든 입력별 `EachAcceptedStarts`, `EachAcceptedCompletes`, `EachDeliveredGetsOutcome`를 실제 검사했다.

| 신규 Race cfg | 수정 전 경계 | 제안 후 actual broken |
|---|---|---|
| `ThreadIngressRaceHerdrIdle.cfg` | D1에는 Herdr 관측 spec/cfg 없음. 기존 RaceFixed GREEN/2b142514b6 bytes 실제 실행 | - |
| `ThreadIngressRaceHerdrBusy.cfg` | D1에는 Busy probe spec/cfg 없음. 기존 Race 3cfg 실제 before/after 집합은 위 표; main API 채택을 가정한 PASS 없음 | - |
| `ThreadIngressRaceHerdrStale.cfg` | D1 old snapshot 제어는 NoStaleBootstrap RED였으며 epoch/proof 규칙은 신규 | HerdrCurrentExecutor |


## 반례와 구현 시험에 넘길 것

| 제어 | 반례·코드 대조 | 실제 구현 수용 |
|---|---|---|
| HerdrBefore | checked→422 deny, accepted0. E13 generic gate는 main에 남음 | 실제 start/idle deliver가 E13 switched admission→기존 executor까지 도달하고 fallback 0 |
| KeepReservation | Idle 판독예약 보유 상태로 첫 native 효과 | N04 반환 제거 mutant, 예약 현재성/반환과 첫 키 0/1 spy |
| StaleEpoch/RaceHerdrStale | old proof 확보→재시작→old proof로 first-key | N03/N04 exact identity 및 same-front generation 제거 mutant. old endpoint key0/new effect1 |
| NoWake | accepted→crash/boot→현재 준비→held 고착 | N06/N07 wake 제거: `HerdrAcceptedStarts`와 completes, deadlock RED. 실제 준비 이벤트 뒤 owner wake 확인 |
| Reclassify | API original을 Discord provenance로 바꿈 | source/GUID/child 유지, Person 정책과 transport provenance 별도 단언 |
| DoubleSubmit | 첫 native effect1→ACK를 재주입 허가로 오해→effect2 | Idle delegate+기존 driver 둘 다 prompt 제출하는 mutant. first-key 효과 계수로 RED |
| post-effect crash | native effect1→ACK 전 crash→uncertain→실제로 보이는 실패, native 재효과 0 | Unconfirmed를 NotSent로 바꾸지 않기. receipt 불명·provider failure와 원본 보유를 실제 route/owner 시험 |
| MachineBusy | non-person request accepted→Busy held→기존 native end→Idle first-key | machine source를 Person Busy 경로로 올리는 mutant와 API own-turn 대기 회귀 |

채택 뒤 실패는 deterministic denied와 다르다. 현재 main deliver_once가 origin reserve 뒤 받은 늦은 host refusal를 영속 unknown으로 남기는 경계는 모델의 거절 효과0 증거가 아니다. 설계 T1에서 deterministic host/runtime 검사를 pre-admission으로 옮기고, 그 뒤 TOCTOU는 retained/visible terminal로 처리해야 한다.

## 차트와 재현 증거

[정상 Idle](thread-ingress-herdr.chart.mmd), [옛 incarnation 제어](thread-ingress-herdr-stale.chart.mmd), [wake 제거](thread-ingress-herdr-no-wake.chart.mmd)는 최종 모델/config bytes를 statechart wrapper로 다시 실행해 만든 산출물이다. `HBegin` 직후 `입력1`이고 `HReceipt`가 카운트를 올리지 않는 것을 상태에서 확인할 수 있다.

원시 로그 정본: `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8125/logs/design2-model/`.

- `before.log`, `before/`: 수정 전 원본 19cfg.
- `final-v5.log`, `final-v5/`: 최종 **36cfg=22 GREEN+14 expected RED**, 모든 기대집합 일치. mismatch/tool error/skip/미선택 0. 기존19+신규 Herdr14+새 경합3. 141 TLC runs, 64.3s. 모든 cfg에는 liveness가 있고 official harness가 각 PROPERTY를 독립 검사한다.
- `prewire-v4.log`, `prewire-v4/`: 14cfg closed-gate baseline, 12 expected RED+2 GREEN. accepted0 temporal 성립을 기능 PASS로 쓰지 않음.
- `chart-v5-ThreadIngressHerdr*/`: 최종 모델의 원시 TLC 출력/규칙별 보고·차트. 기대 RED wrapper exit1은 모델 반례이며 도구 오류가 아님.
- `model-sha256.txt`: 두 모델+36 cfg examples+Herdr view(총39개)의 SHA256. final-v5 scratch bytes를 커밋 대상 bytes와 실제 대조했다.
- 초기 `after`, `final`, `final-v2`, `final-v3`, `final-v4` 및 초기 chart는 교정 중 진단 기록이며 최종 증거에 사용하지 않는다.

재현은 저장된 `check.py <새 scratch>`가 `.cfg.example`을 실제 `.cfg`로 materialize하고 header의 EXPECTED_SAFETY/LIVENESS에서 scratch manifest를 만든 뒤 공식 harness를 실행한다. `check.py <새 scratch> prewire`는 위 main gate control만 바꾼다. 기대값을 actual output에서 역생성하지 않는다.

```sh
unset TMUX TMUX_PANE AGENTDESK_ROOT_DIR AGENTDESK_CONFIG AGENTDESK_SINGLE_MESSAGE_PANEL
umask 077
TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar \
JAVA=/opt/homebrew/opt/openjdk/bin/java \
python3 scripts/ci/statecharts_tlc.py --root <materialized-scratch> --jobs 3
```

정본 `docs/statecharts/expected.tsv`는 변경하지 않았다. `.cfg.example`만 존재하므로 저장소 CI 자동 선택은 0일 수 있으며 **이를 PASS로 쓰지 않는다**. 이 레그는 명시 materialization된 36cfg 목록과 공식 harness actual 결과를 증거로 삼는다. 정식 cfg/expected 등록은 구현 조각에서 코디네이터 승인을 받아 진행한다.

## 범위 밖과 한계

- 같은 GUID API 동시요청/durable origin DB의 atomic commit 및 서로 다른 same-text original 구분은 모델의 ticket 포트 바깥이며 실제 route/PG/mutant 시험이 필요하다.
- 다중 node/outbox·cold/remote placement는 기존 D1 포트를 보존했다. T1/T2a를 그것까지 구현한 것으로 확대하지 않는다.
- parent 확인 권한/actual child host·O-b child writer의 실제 준비는 코드 검증·통합 수용의 별도 경계다. 본 모델의 reply 포트가 O-b의 구현·운영 증거를 대신하지 않는다.
- Cancel·Terminate·clear·stop의 모든 진입점은 설계 전수표에 고정했다. 이 모델은 기존 exact-front/epoch와 visible terminal 포트를 소비하며 새 kill/삭제/reset 권한을 만들지 않는다. native modal·draft·source inode 검사는 N03~N05의 기존 입력 모델/생산 경계에 남긴다.
- all fields의 실제 DB/lease/notice 원자성을 TLA 한 행동으로 승격해 보장하지 않는다. 실제 pre-admission 효과0, first-key exact identity, after-effect no-resend는 구현 대상 시험과 원인 제거 production mutant로 확인해야 한다.
