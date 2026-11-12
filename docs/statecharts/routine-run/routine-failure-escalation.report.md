# 연속 실패 N/N+1 에스컬레이션 모델 (r2b)

코드 기준 `5bcbb82297f5cc3ab8897b81ddb9891459e0321b`, 사용자 결정 적용 r2b, 2026-11-12 KST. 실행 단위 RoutineRun의 exact1과 별개인 **여러 terminal routine run 사이의 실패 정책**만 모델링했다. 기존 RoutineRun.tla+18cfg19파일은 HEAD와 바이트 동일이며 소스 SHA256 `1afd1bc11f42c7909d7490e1b719096c53f8eaf526309f0b1d5df26d8800b5b3`를 보존했다. Rust·cargo·운영 실행·API·키 호출은 하지 않았다.

## 사용자 계약과 모델 경계

- N은 기본3, cfg는 양수 N1/N3을 검사한다. terminal failed만 누적한다. attempt가 실패했다가 같은 run이 재시도 성공한 결과는 terminal succeeded이므로0으로 reset한다. interrupted는 중립이고 기존 helper처럼 skipped/paused 등 나머지 nonfailed terminal도 reset한다.
- k=1..N은 notify-only, k=N+1은 명령봇 Announce incident 한 번, k>N+1은 다시 notify-only다. 다음 성공/nonfailed reset 전에는 추가 incident가 없다. 현재 사용자 답이 명시하지 않은 N+2+ 처분을 이 no-repeat 정책으로 구체화했다.
- incident는 실패했던 routine run을 다시 실행하는 것이 아니다. 별도 실제 Discord message original이 **기존 outbox→Announce→허용 bot intake→기존 owner**를 통해 사고 대응 입력이 된다. 새 큐·writer·권한·직접 handoff 주입을 만들지 않는다.
- 요청 commit, 실제 Discord 게시, bot intake, owner 효과를 별도 action/변수로 둔다. Enqueue 또는 HTTP200을 provider 실행 성공으로 올리지 않는다. channel Busy에서는 사고 대응 original이 기존 turn에 합류할 수 있으므로 nativeKeys는 이 입력에 대한 owner effect의 관측 키이며 새 CLI 세션/독립 native 턴 개수 보장은 아니다.
- 정상 ACK+허용 봇 cfg는 기존 intake/owner가 계속 진행할 수 있다는 공정성 아래 `ConfirmedIncidentsReachExistingOwner`까지 확인한다. **IntakeRefused는 native0의 안전·전송 결과 종결만 GREEN**이며 턴 발생 성공의 증거가 아니다.
- Unknown cfg는 같은 outbox row를 held 상태로 보존하고 대시보드 `completion_unconfirmed` 결과를 종결한다. blind repost하지 않는다. 실제 Discord가 받았을 수도 없을 수도 있으므로 owner effect0/1이 모두 가능하다. eventual remediation 턴을 보장하는 property는 이 cfg에 넣지 않았다. 원 routine run은 이미 failed terminal이다.

현행 generic routine outbox는 이 Unknown 계약을 충족하지 않는다. TransientFailure를 Failed로 평탄화하고 retry할 수 있으며 routine-runtime은 headless nonce helper 대상이 아니다. 따라서 이 GREEN은 H 조각이 같은 기존 outbox row에 stable nonce/ACK/unknown 보존을 최소 확장한 **제안 모델**의 결과다. routing_notice의 다른 owner 권위를 빌리거나 새 receipt/probe를 가정하지 않는다.

## 실제 source와 변수/action 연결

| source | 변수/action 및 범위 |
|---|---|
| `routines/discord_log/failure_alert.rs:19-40` | RecordRun/count. immutable Program prefix에서 running/interrupted 제외 후 leading failed count를 별도 reference FailedBefore로 계산하여 CounterMatchesHistory 검사 |
| `failure_alert.rs:45-68`, `src/config.rs:2135-2138,2216-2217` | 현재는 global threshold 기본3에서 정확히 한 번 알림. r2b의 per-routine N와 first-failed snapshot은 신규 설계이며 cfg의 상수N으로 축약 |
| `routines/store.rs:3228-3267` | 실제 저장 terminal 기준. checkpoint 오류가 겉으로 succeeded인 outcome을 failed로 바꿀 수도 있으므로 in-memory status를 정본으로 삼지 않음 |
| `message_outbox.rs:230-258,287-296` | requests/requestCounts/immutable streak key: 기존 persistent outbox enqueue transaction이 같은 event key를 재사용함. 모델의 sequence는 새 durable incident ledger가 아님 |
| `src/domain/bot_role.rs:9-14` | notices와 NotifyIntakeStarts mutant. Notify는 턴 입력 아님; Announce만 명령 역할 |
| `src/cli/direct.rs:363-374`, `discord/agent_handoff.rs:340-370` | 기존 send-to-agent의 실제 Announce 게시 선례. 새 direct turn-start를 호출하지 않음 |
| `src/server/outbox_worker.rs:75-113`, `outbox_actionable_delivery.rs:117-133`, `discord/outbound/send_gate.rs:404-433` | LoadOutbox→PostOutbox. 기존 delivery IDs와 writer/source/bot/target 권한을 유지 |
| `discord/router/intake_gate.rs:325-363,764-813`, `intake_gate/gate.rs:84-120`, `dispatch_policy.rs:226-261` | ExistingBotIntake/AllowedBot. 인증된 실제 봇 identity·메시지 ID를 사용하고 body 표지나 whitelist 재추론으로 권한을 만들지 않음 |
| `discord/router/intake_dispatch.rs:784-824`, `queue_io/input_owner/ingress.rs:395-473`, `provider_dispatch/owner_input.rs:831-920` | ExistingOwnerStarts. 기존 admission/claim/Busy 경로의 계속 가능한 effect를 추상화. 실제 host/current-source/key-time 증거는 별도 수용 시험 |
| `src/domain/outbound/result.rs:103-105`, `src/services/discord/outbound/manual_delivery.rs:650-654`, `src/services/discord/outbound/manual_delivery/headless_nonce.rs:71-97` | 현재 generic Unknown→Failed/retry gap. BlindUnknownRepost mutant가 이를 H의 금지 계약으로 드러냄 |
| `message_outbox.rs:292-294,766-770` | Unknown은 failed/cancelled로 바꾸면 key가 해제됨. held+NULLexpiry로 key/row를 남기는 H 계약을 row=held와 completion_unconfirmed로 추상화. GC 실제 실행은 모델 밖 |

접두사 `routines/`는 `src/services/routines/`, `discord/`는 `src/services/discord/`이며 `message_outbox.rs`는 `src/services/message_outbox.rs`다. firstFailed는 Program의 logical run index를 UUID처럼 취급한다. **실제 SQL이 first failed UUID를 정확히 읽는지, 첫 실패 snapshotN를 고정하는지, 동시 latest-terminal CAS·GC·author.bot provenance는 이 모델의 증명 범위 밖이고 H 수정 전 RED다.** FirstFailedBefore reference helper는 현재 invariant에 사용하지 않으며 firstID 정확성 증거로 세지 않는다. 모델은 주어진 stable logical incident key의 반복 방지만 검사한다.

## 검사한 경계와 실제 결과

Program은 첫 streak의 failed N+2개→interrupted→succeeded→다음 streak failed N+1개→skipped→failed→paused다. 실패 카운터는 N+2에서 포화시켜 N+1과 그 이후를 구별한다. routine 결과 RecordRun과 outbox/owner 진행은 독립적으로 interleave할 수 있다. 별도 사고 대응 턴 실패를 routine terminal event로 되먹이는 action은 없으며, 실제 recursion/callback 차단은 H의 호출부 시험 대상이다.

| cfg | states / steps | 실제 결과 |
|---|---:|---|
| `RoutineFailureEscalationBlindUnknown.cfg` | 445 / 811 | NoBlindUnknownRepost, NoDuplicateDiscordMessage RED |
| `RoutineFailureEscalationDuplicate.cfg` | 259 / 528 | AtMostOneTriggerPerStreak, AllEligibleStreaksOffered, AtMostOneRemediationPerIncident RED |
| `RoutineFailureEscalationIntakeRefused.cfg` | 63 / 107 | GREEN |
| `RoutineFailureEscalationN1.cfg` | 76 / 131 | GREEN |
| `RoutineFailureEscalationN3.cfg` | 90 / 155 | GREEN |
| `RoutineFailureEscalationNoReset.cfg` | 70 / 120 | CounterMatchesHistory, AllEligibleStreaksOffered RED |
| `RoutineFailureEscalationNotifyTriggers.cfg` | 174 / 351 | OrdinaryNotifyDoesNotStart RED |
| `RoutineFailureEscalationOffByOne.cfg` | 100 / 175 | TriggerOnlyAtNPlusOne RED |
| `RoutineFailureEscalationUnknownN1.cfg` | 147 / 262 | GREEN |
| `RoutineFailureEscalationUnknownN3.cfg` | 167 / 298 | GREEN |

지정 statechart CLI로10cfg 동일 source를 실행했다. **5 GREEN·5 기대 RED**이며 모든 cfg가 실제 상태/전이를 탐색했다. 기본 N3는90states/155steps, N1은76/131이다. Unknown N3/N1도 각각167/298,147/262로 안전과 typed outcome 종결을 검사했다. Unknown을 전달/owner 성공으로 세지 않았다.

공식 checker 결과: 동일 바이트10cfg 전부 expected 집합 일치·exit0, 총91.2초(35회의 plain TLC 실행 및 각 cfg fairness precheck). 원시 증거 `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8117/sc-out-r2b/<cfg>/`의 report.md·tlc.out·chart.mmd, `results.json`, `model-execution-summary.json`, `official-checker.stdout.log`, exact-byte 입력 `ci-root/`다. NoReset/OffByOne은 라벨만 교정하여 `<cfg>-labels/`에도 실행했다(모델 전이/계수 동일).

## mutant 최단 반례와 코드 대조

- NotifyTriggers: 첫 failed 알림→잘못된 notify intake로2step OrdinaryNotifyDoesNotStart RED. 기존 Notify/OperationalAlert는 intake가 거부해야 한다.
- OffByOne: N3에서 세 번째 failed에 요청해3step TriggerOnlyAtNPlusOne RED. 정확한 네 번째 failed이어야 한다.
- Duplicate: 같은 streak에 별도 enqueue를 허용하면3step AtMostOneTriggerPerStreak RED, 끝에서 총 요청 수가 어긋나 AllEligibleStreaksOffered RED, 두 번째 기존 owner 처리까지12step AtMostOneRemediationPerIncident RED. 실제 first-failed event key와 기존 persistent dedupe를 유지해야 한다.
- NoReset: 첫 succeeded 뒤 counter가 남아7step CounterMatchesHistory RED; 다음 streak가 N+1에 이르러도 새 incident가 없어14step AllEligibleStreaksOffered RED. 재시도 중 오류가 있었어도 저장 terminal succeeded이면 reset한다.
- BlindUnknown: N+1 요청→Unknown→다시 pending으로 올려5step NoBlindUnknownRepost RED. Discord가 첫 POST를 받았던 경로에서는6step NoDuplicateDiscordMessage RED다. Unknown terminal outcome과 held row는 새 POST 권한이 아니다.

이는 읽기 전용 code-step 대조이며 Rust/Discord 실재현·production call-site mutant는 NOT_RUN이다. 모델 반례가 H의 RED/GREEN·실제 actor 호출 시험을 대신하지 않는다.

## 공정성·가시성·제외

WF(RecordRun)은 이 고정된 terminal event history를 결국 읽는다는 가정, WF(Delivery)는 계속 가능한 기존 outbox/intake/owner 단계가 진행한다는 가정이다. Unknown에는 자동 probe/recovery 공정성을 추가하지 않았다. 결과를 completion_unconfirmed로 기록하면 attempt는 종결되지만 durable held row/key는 남는다. 정상 ACK+허용 봇의 owner 효과 속성은 위 명시된 가용성 가정 아래만 적용하며 native process 실행 관측의 대체가 아니다.

view는 Done self-loop만 숨긴다. 같은 화면의 세부 전이를 CLI가 합쳤으며 broken rule 조합은 서로 다른 라벨로 남겼다. 기본 N3/N1 차트9상태14화살표, Unknown N3/N1 차트9상태15화살표다. TTL 경과·nonce/ACK CAS·N PATCH·outbox worker 경쟁·실제 Discord rate limit·GC·author provenance·provider 실패 재귀·동일 native turn 합류는 H 시험 범위다.

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar JAVA=/opt/homebrew/opt/openjdk/bin/java
~/.claude/skills/statechart/scripts/statechart docs/statecharts/routine-run/RoutineFailureEscalation.tla \
  --config docs/statecharts/routine-run/RoutineFailureEscalationN3.cfg \
  --view docs/statecharts/routine-run/routine-failure-escalation.view.mjs \
  --out /tmp/i8117-r2b-new
```

[기본 N3](routine-failure-escalation.chart.mmd)·[Unknown](routine-failure-escalation-unknown.chart.mmd)·[일반 알림 오동작](routine-failure-escalation-notify.chart.mmd)·[경계 오류](routine-failure-escalation-boundary.chart.mmd)·[중복 요청](routine-failure-escalation-duplicate.chart.mmd)·[reset 누락](routine-failure-escalation-reset.chart.mmd)·[Unknown 재게시](routine-failure-escalation-blind-unknown.chart.mmd).

최종 escalation 모델 SHA256 `511bbb35abf2af5f943d79189cd56403a052d84e8dccc8d4ab836e1452049527`.
