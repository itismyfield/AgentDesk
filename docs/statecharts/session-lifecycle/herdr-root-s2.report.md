# Herdr RootReplaced recovery — S2 preclaim and completion wake

기준 코드는 S1 r2와 main `b8af0ace7000a024570ef1791211138a3fcc1b9e`를 병합한 `61bc2876e3f4d593ec062031af10f7cb4dab8af1`이다. 반례 대조 표의 Rust 좌표는 이 고정 병합 SHA를 기준으로 대조했다. S2 실제 연결은 별도의 `61bc + S2` 좌표 절에 구분한다. production 수정 뒤의 실행 증거는 레인 구현 보고서에서 따로 확인한다. 이 문서는 TLA+ 실행과 코드 경로의 정적 대조 결과다.

S1의 `HerdrRootBefore.cfg`, `HerdrRootFixed.cfg`, `HerdrSameRoot.cfg`에는 S2 연결을 끄는 상수만 추가했다. 기대 위반 집합은 그대로다. Before는 claim 권리와 실제 start의 분리로 상태 수만 28→33이며 Fixed/SameRoot는 16/7로 유지된다. `HerdrSession.tla`·`HerdrSession.cfg`도 바꾸지 않았다.

## 검사 결과

| 설정 | distinct states | 직접 TLC exit | 결과 |
|---|---:|---:|---|
| `HerdrPreclaimBefore.cfg` | 27 | 13 | ReplacedInputProgress RED |
| `HerdrPreclaimFixed.cfg` | 16 | 0 | GREEN |
| `HerdrRootWakeBefore.cfg` | 13 | 13 | ReplacedInputProgress RED |
| `HerdrRootWakeFixed.cfg` | 18 | 0 | GREEN |
| `HerdrPreclaimRetired.cfg` | 11 | 0 | GREEN |
| `HerdrPreclaimPending.cfg` | 6 | 0 | GREEN |
| `HerdrPreclaimNonlocal.cfg` | 25 | 0 | GREEN |
| `HerdrPreclaimSameRoot.cfg` | 7 | 0 | GREEN |

직접 TLC의 진행 반례 exit는 13이다. `statechart` frontend로 실행한 PreclaimBefore/WakeBefore는 각각 exit 1, 두 Fixed는 각각 exit 0이다. `scripts/ci/statecharts_tlc.py` 전체는 **68개 설정, 155.9초, exit 0, 기대/실제 불일치 0**이다. Before의 safety는 모두 유지되고 TSV의 네 번째 필드에 선언한 `ReplacedInputProgress`만 깨진다.

보존한 safety: `SameRootNoCreate`, `InputAtMostOnce`, `NoRetireLiveExecution`, `PreserveHeldInput`, `NoUnverifiedServerTrust`, `InputUsesCurrentExecution`, `PendingBeforeCreate`, `OldGrantUnchanged`. S2는 `NonApplicableNoPreclaimRead`, `HealthyNoPreclaimRead`, `CompletionWakeAtMostOnce`, `OldRetiredNoWake`를 추가한다. 기존 Retired의 정상 fresh launch는 별도 `RetiredInputProgress`, 같은 root의 재사용은 `SameRootInputProgress`를 실제 PROPERTY로 검사한다. Pending·비로컬 설정은 이 root 복구가 다른 owner를 방해하지 않는 safety만 검사한다.

## 순서·트리거·custody

신규 설정은 `InputAfterFirstPass = FALSE`다. 이 설정에서 `ObserveExecution`은 첫 owner 시도(옛 binding start, claim 전 Held, 동기 복구 대기 또는 같은 root submit) 뒤에만 실행되도록 환경 순서를 고정한다. 그래서 PreclaimBefore의 실제 반례는 Q 수락 → front claim → 옛 binding start/inflight 기록 → 첫 reader pass 순서이고, 첫 pass 뒤에 Q가 오는 S1 가정으로 GREEN이 되는 경로가 아니다.

`PreclaimRecover`는 정확한 Bound·local/current와 기존 `candidate` 또는 다른 witness를 가리키는 기존 grant가 있을 때만 활성화된다. fresh 관측을 `preclaimReads`로 센다. 새 증거가 없는 건강한 same-root 행에서는 이 수가 0이다. 동기 복구 동안 Q는 `recovering` custody를 유지하고 inflight를 만들지 않는다. cleanup 완료가 같은 handoff를 `queued`로 되돌리므로 동기 복구에는 별도 wake가 필요 없다.

완료 wake는 `seenIncomplete`를 가진 exact row가 cleanup을 마친 뒤에만 `completionPending`을 세우고, `notified`가 false일 때 한 번 발생한다. 초기 Retired 행은 이 process가 미완료로 본 적이 없으므로 wake 0이다. 임의의 옛 Retired 행 하나를 대표값으로 검사하며, Rust 시험의 3행 부팅 census를 이 모델에서 실행했다고 주장하지 않는다.

`HoldBeforeClaim`은 claim 전에 일시적인 실제 guard가 한 번 보류하는 상황을 나타낸다. Q는 Held이되 inflight는 없다. background의 정상 종료 증명·quiet·CAS·cleanup은 계속 가능하고 완료 wake만이 같은 Q를 다시 실행 가능하게 한다. `AwaitWake`는 owner가 신호를 기다리는 정지 상태의 stutter이며, wake가 없을 때 그 상태를 fairness로 빠져나오게 하지 않는다.

## S2 실제 연결 좌표 — 61bc + S2

아래는 S2 구현을 읽은 source snapshot의 좌표이며, 위/아래 반례 표에 쓰인 수정 전 61bc 좌표와 구분한다. 제출 head SHA는 아직 정하지 않았고 source anchor는 `61bc2876e3f4d593ec062031af10f7cb4dab8af1 + S2`다. 레인의 `s2-delta-audit-stage3.json`에 이때 읽은 production 5파일 SHA를 기록했다. 이 절 추가는 문서 대조이며 검증된 TLA/cfg/기대값을 바꾸지 않는다.

| 모델에서 확인한 연결 | 실제 S2 source 좌표와 경계 |
|---|---|
| 비대상 채널은 기존 경로 | `src/services/discord/herdr_terminate/root_replaced/preclaim.rs:19–29`가 Claude/Codex·configured Herdr endpoint·local node를 PG 접근 전에 검사한다. 그 뒤에만 `:30–35` pool/canonical row를 읽고 `:40–44` Bound/local/current를 재검사한다. Retired·Pending·비로컬은 Ok(false)이며 새 Held를 만들지 않는다. |
| 기존 replacement 증거로만 fresh 관측 | 같은 preclaim `:46–59`가 cached candidate 또는 Replaced를 요구하고, 그 뒤 `:61–65`에서 fresh exact probe를 수행한다. 건강한 Bound/no evidence는 pane read 0이다. |
| 같은 live pane은 recovery하지 않음 | preclaim `:68–80`의 fresh Match는 기존 정책 증거 준비 뒤 Ok(true)로 반환한다. `:87` common recover를 호출하지 않아 살아 있는 exact root를 retire하지 않는다. 다른 관측은 `:82–89`의 기존 evidence candidate로 strict recovery를 수행하며, 정책·old process·lone shell·current/quiet/fence/CAS guard를 생략하지 않는다. |
| owner claim 이전에 같은 Q 복구 | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:522–527`은 before_idle을 await하고, 그 뒤 `:528`에서 claim_turn을 수행한다. 기존 front context·original/ticket을 유지한다. |
| legacy dequeue 이전에 같은 복구 | `src/services/discord/queue_dispatch.rs:224–243`이 큐 선택/소비 전에 before_idle을 실행한다. fresh exact-live라도 이전-generation inflight hold가 있으면 기존 hold_queue/request를 유지한다. |
| 이 process의 미완료 완료만 식별 | `src/services/discord/herdr_terminate/root_replaced.rs:128–157`은 session_id와 state만 정규화한 전체 Known execution identity, Held/CleanupPending 조건을 공통 predicate로 검사한다. `:86–96` prune도 같은 predicate로 다른 provider pass가 그 marker를 먼저 지우지 않게 보존한다. 이 2-provider interleaving은 단일-row 모델 자체의 증명 범위가 아니라 Rust 회귀시험·코드 대조 범위다. |
| 완료 게시 뒤 한 번 wake | `src/services/discord/recovery_engine/herdr_reader.rs:270–275`는 prune 전에 incomplete를 읽고, `:311–320,345–350`에서 cleanup 완료/Bound candidate 완료만 모은다. `:354–360` Pass 결과 게시 뒤 `:361–367` immediate kickoff를 요청한다. common recover가 반환해 transition lease가 해제된 뒤이며, 오래된 즉시 완료 Retired나 계속 Held는 전이 wake가 아니다. |

## 반례 1: preclaim이 없으면 현재 generation inflight가 quiet를 막는다

아래 번호는 `s2-model-herdr-preclaim-before-chart-separated-final`의 `report.md`의 counterexample 0–18이다.

| 단계 | 모델 행동 | 고정 코드와 대조 |
|---:|---|---|
| 0 | N1 Bound, W0 grant, root R0 | `src/services/session_host/herdr/policy/permit.rs:146`의 exact endpoint·nonce·witness grant |
| 1 | dcserver 재시작, process 증거 비움 | `src/services/session_host/herdr/launch_host.rs:216`의 새 ProofCache; durable grant와 별개 |
| 2 | Herdr witness W0→W1 | `src/services/session_host/herdr_gate.rs:905`의 현재 socket witness 판독 |
| 3 | root R0→R1 lone shell, 원래 실행 종료 | `src/services/session_host/herdr_gate/root_replaced.rs:38`의 다른 root·lone shell/원래 process 부재 조건 |
| 4 | reconciliation 완료 | `src/services/discord/herdr_terminate/root_replaced.rs:359`의 restart reconciliation guard |
| 5 | 같은 Q 수락 | `src/services/discord/queue_io/input_owner/drive.rs:36`의 같은 front·claim 경로 |
| 6 | front grant를 claim 권리로 이동, inflight는 아직 없음 | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:522`, `src/services/turn_orchestrator/queued_head/v2.rs:300`; 이 권리 이동은 quiet를 막지 않음 |
| 7 | 그 뒤 옛 binding 시작이 현재 generation inflight를 기록 | `src/services/discord/router/message_handler/intake_turn.rs:150`, `src/services/discord/router/message_handler/headless_turn.rs:917`의 create-new inflight |
| 8 | pane 거절 뒤 Q Held, 현재 generation inflight 보존 | `src/services/discord/router/message_handler/intake_turn.rs:150` 및 `src/services/discord/router/message_handler/headless_turn.rs:917`의 create-new inflight, `src/services/session_host/herdr_gate.rs:973`의 PaneUnverified |
| 9 | 첫 reader pass가 exact candidate 관측 | `src/services/discord/recovery_engine/herdr_reader.rs:304`의 candidate 보존·관측 경로 |
| 10 | 현재 서버 startup 증거 생산 | `src/services/discord/herdr_terminate/root_replaced.rs:387` → `src/services/session_host/herdr/launch_host.rs:322` → `src/services/session_host/herdr/policy/cache.rs:71` |
| 11 | 관측 뒤 새 target | `src/services/discord/herdr_terminate/root_replaced.rs:405` |
| 12 | 정책·pane·old process 종료 증명 성공 | `src/services/session_host/herdr_gate/root_replaced.rs:23` |
| 13 | quiet가 현재 inflight 때문에 보류 | `src/services/discord/herdr_terminate/root_replaced.rs:258`의 inflight 검사; 이를 정리하지 않음 |
| 14 | 다음 reader pass 재시도 | `src/services/discord/recovery_engine/herdr_reader.rs:326`의 retained candidate recovery |
| 15 | 같은 exact 후보 재관측 | 같은 reader `:304` |
| 16 | 같은 witness의 정책 준비 재사용 | `src/services/session_host/herdr/launch_host.rs:315`의 endpoint·witness·FileToken 결과 기억 |
| 17 | 새 target 재구성 | recovery `:405` |
| 18 | 종료 증명은 계속 성공하지만 12–18 반복 | 종료 증명은 inflight를 정산하지 않으며 quiet는 계속 false. 이전 generation 전용 row-end의 `src/services/discord/recovery_engine/herdr_row_end.rs:56` guard도 현재 generation에는 적용되지 않음 |

따라서 정책 증거가 이미 생산 가능하고 S1이 적용돼 있어도, 옛 binding start가 reader 복구보다 앞서 inflight를 만들면 Q 효과는 0이다. claim 권리만 이동한 단계는 inflight=false/quiet=true이며, 그 상태에서 reader가 먼저 Retired를 만들면 같은 claim이 fresh plan을 계속 탈 수 있도록 StartLaunch/SendFresh가 claimed custody도 허용한다. 단순히 preclaim 호출을 claim_turn 바로 뒤로 옮겼다는 이유만으로 quiet가 실패한다고 모델링하지 않았다. 그 더 이른 순서 계약은 Rust fixture의 BEFORE_CAS owner.claimed=false 단언으로 확인한다. Fixed는 step 6의 claim 전에 `PreclaimRecover`를 실행하여 inflight 없이 종료·CAS·cleanup을 기다리고 같은 Q로 Pending→create 1→Bound→submit 1을 완료한다.

## 반례 2: 완료 wake가 없으면 이미 Held인 Q가 남는다

아래 번호는 `s2-model-herdr-root-wake-before-chart-separated-final`의 `report.md`의 0–12다.

| 단계 | 모델 행동 | 고정 코드와 대조 |
|---:|---|---|
| 0–4 | N1 Bound → dcserver/Herdr 재시작 → reconciliation | 위 반례의 0–4와 같은 owner·witness·root 경계 |
| 5 | 같은 Q 수락 | owner front custody가 생성됨 |
| 6 | claim 전에 guard 보류, inflight 없이 Q Held | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:504`의 start_idle claim 전 `NotSent` 반환 계약; `src/services/discord/queue_io/input_owner/drive.rs:36` claim 이전에는 front를 소비하지 않음 |
| 7 | background reader가 exact candidate 관측 | `src/services/discord/recovery_engine/herdr_reader.rs:304` |
| 8 | S1 정책 준비 | `src/services/discord/herdr_terminate/root_replaced.rs:387`, `src/services/session_host/herdr/launch_host.rs:322` |
| 9 | 새 target | `src/services/discord/herdr_terminate/root_replaced.rs:405` |
| 10 | strict end proof | `src/services/session_host/herdr_gate/root_replaced.rs:23` |
| 11 | quiet 확인 뒤 exact Retired CAS | `src/services/discord/herdr_terminate/root_replaced.rs:430–451`; Q Held/front grant는 active/inflight가 아니므로 quiet를 막지 않음 |
| 12 | cleanup 완료, Q는 여전히 Held | `src/services/discord/herdr_terminate/root_replaced.rs:493`의 old hold release; `src/services/discord/recovery_engine/herdr_reader.rs:326–331`은 성공 시 continue하고 wake가 없음. `src/services/discord/herdr_terminate.rs:325–339`의 `settle_captured(None)`도 바로 반환 |

WakeBefore는 마지막 상태에서 영원히 stutter한다. WakeFixed는 pass 결과가 게시된 완료 전이를 한 번 알리고 같은 Q를 다시 queued로 만든 뒤 create 1·submit 1을 완료한다. 이미 완료된 Retired를 매 pass 깨우거나 Held 자체를 깨우는 전이는 없다.

## 비공허성·fairness와 한계

진행 전제는 정상 local/current, old execution 종료, replacement provider 부재, 검증 가능한 peer/image·false config·시작 이전 파일·동일 directory·정상 cache이며 **startup cache나 admission 성공을 전제로 넣지 않았다**. `Spec`의 실제 `WF_vars`는 기존 guard가 활성일 때만 진행을 요구한다. Before에 없는 preclaim 또는 wake의 fairness는 넣지 않았고 quiet·정책 실패를 건너뛰는 공정성도 없다.

모델은 exact row 하나·original Q 하나·dcserver generation 증가 한 번·Herdr 교체 한 번을 다룬다. 여러 행의 channel dedup, OS process provenance·PG 오류·config token 바이트·동시 두 번째 server 교체·관측이 같은 위치를 돌려준 뒤의 실제 pane 검증은 Rust fixture/정적 감사를 따른다. same-root 모델은 원래 witness/root가 살아 있는 재사용 대조이며, 새 witness+cached candidate+exact-live 조합 전체를 모델링했다고 주장하지 않는다. Retired의 location None과 다른 endpoint는 이 모델에서 같은 ‘root preclaim 비대상’으로 추상화하고 각각의 ingress 실행은 Rust 시험에서 확인한다.

## 차트·재실행·증거

차트는 `docs/statecharts/session-lifecycle/herdr-root-s2.view.mjs`에서 생성했다. PreclaimBefore 9상태/9화살표, PreclaimFixed 13/12, WakeBefore 9/9, WakeFixed 14/13이다. 같은 reader label 내부의 정책·target·proof 단계 각각 10/4/1/6개만 합쳐 표시하며, 반례의 코드 대조 표에는 그 단계도 남겼다. 외부 실패/권한 guard 전이를 숨기지 않았다.

```sh
unset TMUX TMUX_PANE
export JAVA=/opt/homebrew/opt/openjdk/bin/java
export TLA2TOOLS="$HOME/.local/share/tla2tools/tla2tools-1.7.4.jar"
python3 scripts/ci/statecharts_tlc.py
$JAVA -XX:+UseParallelGC -jar "$TLA2TOOLS" -metadir /tmp/herdr-preclaim-fresh-meta -config docs/statecharts/session-lifecycle/HerdrPreclaimBefore.cfg docs/statecharts/session-lifecycle/HerdrRootRecovery.tla
~/.codex/skills/statechart/scripts/statechart docs/statecharts/session-lifecycle/HerdrRootRecovery.tla --config docs/statecharts/session-lifecycle/HerdrPreclaimFixed.cfg --view docs/statecharts/session-lifecycle/herdr-root-s2.view.mjs --out /tmp/herdr-preclaim-fresh-chart
```

최종 모델 SHA256은 `57ac02623dcc66aa44bb97b8b07608f1b990e8f2745fdb05bc26f4a7c991685e`, TLC jar SHA256은 `936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88`이다. 최종 전체 검사 로그는 `s2-model-suite-claim-separated.log`다. 원시 증거는 `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i7788/logs/s2-model-*`에 있다. direct-results JSON은 TLC13과 frontend1을 구분하며, frontend-results JSON·각 `*-chart-separated-final`의 `report.md`·TLC 원문은 실제 chart의 반례를 보존한다. `s2-model-suite-claim-separated.log`는 전체 68개 기대값 대조, environment.json은 모델/config/view/jar fingerprint다. 초기 직접 TLC의 repo-root `states/`는 내용 확인 후 `s2-model-tlc-default-states/`로 이동해 보존했다. cargo·PG·운영 herdr 연결은 하지 않았다.
