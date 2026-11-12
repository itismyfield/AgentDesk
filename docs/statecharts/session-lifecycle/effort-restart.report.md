# Effort 변경 즉시 재기동 상태 차트

코드 기준은 `193a57168f`다. prod 변경·cargo·Rust 시험·운영 설정/DB/세션 접근 없이 기존 `TmuxSession.tla`에 `Effort*Spec` 투영을 추가했다. `InputPath.tla`, `WriterWake.tla`를 먼저 읽고 소유 원본 보존·완료 wake·게시 ack debt 계약을 가져왔다. 기존 TmuxSession cfg의 예상 위반 세 개는 그대로다. 최초 15 cfg와 legacy의 위반 집합을 S1에서 다시 확인해 보존했고, 자연 완료 없는 진행성 4 cfg를 추가해 총 19 cfg다. `expected.tsv`의 기존 행은 그대로 두고 새 4행만 덧붙였다.

## 진입점 전수표와 모델 값

| 경계 | 코드 좌표(기준 SHA) | 모델 값/행동 |
|---|---|---|
| local effort 저장 | `commands/tui_passthrough.rs:146` | CurrentTmux는 변경을 저장하고 다음 새 턴에 재기동; 현재 즉시 중단 행동은 없음 |
| local restart busy 거절 | `commands/tmux_recreate.rs:93` | 현재 busy Deferred; Design은 confirmed end 뒤 동일 session launch |
| Herdr effort | `commands/tui_passthrough/herdr_settings.rs:195`, `:203`, `:216`, `:225` | CurrentHerdr는 old task 종료/ready 후 overlay 적용; restart 0 |
| exact hosted end | `herdr_terminate.rs:437`, `:453`, `:580`, `:596`, `:603` | EffortInterrupt는 시도가 아닌 exact nonce/TurnKey confirmed end와 cutoff 경계의 oracle |
| old inflight 정산 | `herdr_terminate.rs:337`, `turn_finalizer.rs:277`, `turn_finalizer/finalize_context.rs:37` | captured old row만 mute close; reader output 소유 유지 |
| owner Held | `queue_io/input_owner/drive.rs:118`, `queue_io/input_owner/held_watch.rs:62` | waiting users는 queue에 남는다. Gone는 End여서 completion wake가 별도로 필요 |
| transition 종료/wake | `session_transition.rs:120`, `:136`, `:147` | physical resume 후 owner wake; NoWake mutant에서 continuation/사용자 둘 다 멈춤 |
| 자동 입력/재시도 | `queue_io/input_owner/origin.rs:6`, `queue_io/input_owner/start.rs:13`, `:48`, `:56` | continuation은 사용자 원문 재전송이 아닌 stable synthetic ticket; 실패 재시도는 같은 root |
| synthetic source 번호 | `queue_io/input_owner/routine.rs:267` | 원본 user id 재활용 금지. 별도 stable ticket 설계가 port 책임 |
| 큐 꼬리 append | `turn_orchestrator/queued_head/v2/source_admission.rs:746` | 기존 append만 재사용하면 선행 사용자보다 continuation이 늦음 |
| person 우선 이동 | `router/message_handler/provider_dispatch/owner_input.rs:244`, `turn_orchestrator/queued_head/v2.rs:123` | continuation barrier를 실제 queue 선택에 연결해야 함. UserFirst mutant |
| output replay/ack | `tui_o/writer/replay.rs:15`, `:27`, `:54` | accepted old debt는 posted ack 전까지 보존. physical resume은 POST를 기다리지 않음 |

위 `commands/`, `queue_io/`, `router/`, `turn_finalizer*`, `session_transition.rs`, `herdr_terminate.rs`는 `src/services/discord/` 아래다. 그 외 `turn_orchestrator/`, `tui_o/`는 `src/services/` 아래다.

값 범위는 위 코드의 상태/소유 관계를 축약했다. 사용자 원본 두 개는 서로 다른 두 queue id, 요청 두 개는 coalesce와 실행 중 continuation의 재중단을 구별하는 유한 경계다. 제품 queue 한도를 두 개로 바꾼다는 뜻이 아니다. root `1..2`는 실제 turn nonce의 동등성 부류이고 0은 없음이다. notice/continuation/reply count의 2는 중복 반례를 표시하는 포화 상한이다. daemon generation은 0으로 고정하고 TUI binding만 resume마다 증가한다.

## 규칙별 결과

- 답 중복 0: `EffortReplyOnce`, 기존 old debt의 ack 이전 보존 `EffortOldDebtNotLost`; Design GREEN. Repost만 RED.
- 입력 조용한 유실 0: `EffortInputNotLost`, `EffortInputOnce`, `EffortUserFifo`; Design GREEN. DropQueue만 유실/FIFO RED.
- 실제 중단 안내 정확히 1: `EffortInterruptedNoticeExactlyOnce`는 각 실제 interrupted root의 durable intent=1, visible<=1, delivery settlement 뒤 visible=1을 검사한다. `EffortInterruptedEventuallyTold`는 actual interrupted→eventually visible=1을 검사한다. 따라서 단지 enqueue나 <=1만으로 PASS하지 않는다. 안내 POST 실패 상태는 완료로 취급하지 않으며 물리 resume은 POST를 기다리지 않는다. NoNotice/DuplicateNotice가 각각 RED.
- 자동 이어하기 정확히 1: `EffortContinuationAtMostOnce` + `EffortContinuationAuthorized` + `EffortContinuationEventuallyInjected`. 성공 재기동된 actual interrupted root만 고정 자동문구를 한번 접수한다. idle/failed는 0, 실패 뒤 외부 retry도 같은 root다. coalesce(주입 전 새 effort)는 같은 root, 실행 중 continuation 중단은 새 root다.
- 우선순위: `EffortContinuationFirst`는 대기 user보다 자동 continuation을 먼저 접수한다. user 사이 FIFO는 별도로 검사한다. UserFirst가 RED.
- 영구 보류 0: `EffortPendingEventuallyProcessed`; 성공 재기동의 ready 상태에 도달한 대기 입력은 결국 접수된다. NoWake/NoContinuation가 RED. launch가 영구 실패한 Failure cfg에는 이 성공 전제 PROPERTY를 걸지 않고 custody/visible failure만 검사한다.
- old debt: `EffortOldDebtEventuallyPosted`는 공정한 출력 성공 가정 아래 old debt의 후속 전달을 검사한다. debt가 남아도 same-session resume과 continuation은 진행할 수 있다.

## 원 작업의 자연 완료에 기대지 않는 재기동 진행성(S1)

`EffortRequestResolves == (changes > 0) ~> (phase \in {"ready", "failed"})`를 Design/Idle/Failure/Retry/Coalesce/Second와 신규 cfg에서 검사한다. 원래 task의 `EffortFinish`를 금지한 NonFinishing과 NonFinishingFailure에서도 각각 ready 또는 명시적 실패에 도달한다. 이 두 cfg에서는 original의 자연 완료가 WF로 보장되지 않으며, continuation/새 사용자 task의 이후 완료 공정성만 유지한다. 즉시 재기동의 뜻은 원 작업의 자연 종료나 다음 새 사용자 턴을 기다리지 않는다는 것이고, 벽시계 지연 상한을 증명한 것은 아니다.

`EffortRest`는 물리 provider가 idle(active none, running/ready/기존 waiting 상태)이거나 명시적 failed인 경우만 허용한다. requested/interrupted/stopped/booting에서 무조건 stutter하는 Rest는 없다. ready 뒤 Held 큐의 진행성은 기존 PROPERTY가 계속 검사하므로 유휴 self-loop가 문제를 숨기지 않는다.

NoInterrupt는 원 작업을 끝내지 않는 같은 cfg에서 중단만 제거하고, NoClose는 exact end 뒤 old row 정산 전이만 제거한다. 각각 `NoDeadlock`와 `EffortRequestResolves`가 RED이며, 원 작업 종료를 기다리는 구현 및 ready 이전 정지를 잡는다. 이 두 mutant에서 기존 continuation/ready 이후 PROPERTY는 공허 GREEN일 수 있으므로 새 request 진행성 검사 없이는 즉시 재기동 검증을 GREEN으로 승인하지 않는다.

## 전/후 표

| cfg (`EffortRestart` 접두사) | states | 결과 | 상세 |
|---|---:|---|---|
| `CurrentTmux` | 40 | GREEN | [report](effort-restart-CurrentTmux.report.md) |
| `CurrentHerdr` | 34 | GREEN | [report](effort-restart-CurrentHerdr.report.md) |
| `Design` | 238 | GREEN | [report](effort-restart-Design.report.md) |
| `Idle` | 38 | GREEN | [report](effort-restart-Idle.report.md) |
| `Failure` | 130 | GREEN | [report](effort-restart-Failure.report.md) |
| `Retry` | 298 | GREEN | [report](effort-restart-Retry.report.md) |
| `Coalesce` | 406 | GREEN | [report](effort-restart-Coalesce.report.md) |
| `Second` | 790 | GREEN | [report](effort-restart-Second.report.md) |
| `NoWake` | 242 | RED: EffortContinuationEventuallyInjected, EffortPendingEventuallyProcessed | [report](effort-restart-NoWake.report.md) |
| `NoContinuation` | 142 | RED: EffortContinuationEventuallyInjected, EffortPendingEventuallyProcessed | [report](effort-restart-NoContinuation.report.md) |
| `UserFirst` | 342 | RED: EffortContinuationFirst | [report](effort-restart-UserFirst.report.md) |
| `DropQueue` | 366 | RED: EffortInputNotLost, EffortUserFifo | [report](effort-restart-DropQueue.report.md) |
| `NoNotice` | 142 | RED: EffortInterruptedEventuallyTold, EffortInterruptedNoticeExactlyOnce | [report](effort-restart-NoNotice.report.md) |
| `DuplicateNotice` | 334 | RED: EffortInterruptedNoticeExactlyOnce | [report](effort-restart-DuplicateNotice.report.md) |
| `Repost` | 448 | RED: EffortReplyOnce | [report](effort-restart-Repost.report.md) |
| `NonFinishing` | 200 | GREEN | [report](effort-restart-NonFinishing.report.md) |
| `NonFinishingFailure` | 104 | GREEN | [report](effort-restart-NonFinishingFailure.report.md) |
| `NoInterrupt` | 8 | RED: EffortRequestResolves, NoDeadlock | [report](effort-restart-NoInterrupt.report.md) |
| `NoClose` | 32 | RED: EffortRequestResolves, NoDeadlock | [report](effort-restart-NoClose.report.md) |

Current cfg의 interruption/continuation 규칙은 실제 중단 자체가 없어 공허하게 성립한다. 이 GREEN은 현재 코드가 즉시 restart 수용 기준을 충족한다는 뜻이 아니다. Design 이후 cfg는 설계의 전이이고 아직 prod 구현이 없다. legacy `TmuxSession.cfg`는 400 states, 기존 `{NoDeadlock, RowNamesSession, RowOnlyWithTurn}` RED를 유지했다.

Second는 790 states로 그림 예산 500을 넘겨 chart를 생성하지 않는다. 모든 safety/PROPERTY 검사는 완료했고 report가 있다. 나머지는 각각 chart를 제공한다. Design 그림은 11 states/16 arrows다. 같은 라벨 안의 input 도착·안내 POST·old debt 전달 등은 접어 보이며, TLC 검사에서는 생략하지 않는다. mutant의 20개 초과 화살표는 반례 문맥을 보존했다.

## 반례를 코드로 한 단계씩 대조

이 표는 main의 새 결함을 주장하지 않는다. main에는 즉시 effort restart 자체가 없다. 코드로 확인한 기존 API의 조합 위험만 구현 수용 시험으로 넘긴다.

| 모델 경로 | 코드 대조 | 인계 판정 |
|---|---|---|
| UserFirst: request→user 대기→confirmed end→old row close→resume→user 선접수(7단계) | 기존 admission은 `source_admission.rs:750` tail append. `prefer_person`은 machine 앞의 person을 당길 수 있음. 자동 continuation을 그냥 admit하면 우선순위 계약이 없음 | 코드로 확인: continuation barrier/indexed 선택 수용 시험 |
| DropQueue: request→user 대기→confirmed end→old row close→기존 clear를 재사용(5단계) | `commands/control.rs:552`가 `clear_channel_discarding_catch_up_backlog` 호출. effort용 teardown에서 이 경로를 쓰면 queue를 버림 | 코드로 확인: pending-preserving teardown 수용 시험 |
| NoWake: request→confirmed end→resume→wake 없이 대기(continuation 8단계/user 10단계 lasso) | `held_watch.rs:64` Gone는 End, wake가 아님. `session_transition.rs:136` 기존 completion이 wake 제공 | 코드로 확인: success/failure guard release 후 owner wake 수용 시험 |
| NoContinuation: resume 후 pending root가 남아 영구 대기(10단계 lasso) | 새 자동 continuation port는 아직 없음. `start::admit_synthetic`의 Accepted/Pending/Rejected를 구별해야 함 | 설계 mutant만 확인. main 결함/실재 버그로 올리지 않음 |
| NoNotice/DuplicateNotice | 기존 HostTerminated가 mute인 사실은 확인. 새로운 durable effort interruption notice의 구현은 없음 | 설계 obligation; main 결함으로 올리지 않음 |
| NoInterrupt: 원 작업이 끝나지 않는 request 대기(4단계 lasso) | 기존 `commands/tmux_recreate.rs:93` busy Deferred는 현재 next-turn 계약이다. 새 즉시 명령은 이 대기에 남으면 안 됨 | 코드로 확인한 현재 분기와 설계 회귀 mutant; `busy_effort_restarts_while_original_never_finishes` 수용 시험 |
| NoClose: confirmed end 뒤 정산 정지(7단계 lasso) | 기존 `herdr_terminate.rs:603`은 exact old turn 정산을 호출한다. 새 adapter에서 이를 빼는 구현을 모델로 제거 | 설계 obligation. 실제 main에서 정산 omission을 재현했다고 주장하지 않음 |
| Repost: 동일 old body를 resume 뒤 재게시(6단계) | 기존 replay가 acked record만 제외함을 확인했으며 실제 main에서 duplicate가 발생함을 확인한 것은 아님 | 설계 guard mutant. live 반례로 인계하지 않음 |

## 범위 밖과 외부 가정

1. `EffortSameSession`의 sameSession=TRUE는 고정한 strict launch port 입력이다. wrong-session launch를 탐지했다는 증거가 아니다. 실제 provider session/thread equality, account, exact execution nonce, binding pin은 코드 수용 시험이 검증한다.
2. lifecycle 진입 전 capability=`supported`라는 oracle를 가정한다. `unavailable`/`unknown`에서는 설정 저장·중단을 하지 않는 preflight 계약은 별도 classifier/adapter 시험 대상이다. 특히 Claude ultracodeAvailable을 실제 TUI에서 얻는 adapter의 기술 검증을 대신하지 않는다.
3. EffortResume은 same session + new binding + ready + 검증된 launch 인자/capability를 축약한다. `revision`은 launch에 실은 최신 요청 번호다. 실제 model effort active 증거는 가정하지 않는다. Codex fresh `turn_context.reasoning_effort`는 첫 입력(C 또는 user) 뒤 비동기로 확인하며 Resume/C 선행조건으로 기다리지 않는다. Claude 별도 probe의 active 값도 재기동 incarnation active 증거가 아니다.
4. confirmed end와 durable cutoff를 얻기 전 kill 실패·unknown host는 모델의 EffortInterrupt에 진입하지 않는다. 이 안전 경계, dcserver crash가 end/cutoff/notice/continuation 기록 사이에 오는 경우, opaque MayHaveSent 입력의 원본 복구, 실제 queue 인덱스/재정렬, native transcript cutoff 소비는 구현 acceptance가 담당한다. 모델이 영속 write의 crash 원자성을 증명한 것은 아니다.
5. input 값은 person 원본 두 개와 독립 synthetic continuation이다. 기존 Routine/HttpStart/재시도 lineage 전체의 동시 병합·외부 입력은 모델 밖이다. continuation의 idempotency ticket이 반복 RPC/worker crash에서도 유지되는지는 owner store 시험이 담당한다.
6. 기본 cfg는 provider 작업·notice/output 전송이 공정하게 끝나고 변경 요청은 유한하다고 가정한다(WF). NonFinishing/NonFinishingFailure와 두 새 mutant는 original의 자연 완료를 금지하므로 original 완료 공정성에 기대지 않는다. 실제 host end/ready port의 공정한 성공/실패 관측은 여전히 외부 가정이다. 영구 네트워크 장애·무한 effort 변경·영구 start 실패에 무조건 eventual processing을 주장하지 않는다. Failure cfg는 입력을 durable hold하고 실패를 보이는 것으로 멈춘다. Retry action은 외부 재요청의 추상화이며 새 자동 retry loop 설계가 아니다.
7. output의 old debt는 one durable body/ack의 축약이다. parser와 archive/rotation/owed cutoff 전부를 다시 증명하지 않는다. 기존 WriterWake/replay 계약을 소비하되 POST 완료를 physical resume의 선행조건으로 넣지 않았다.

## 재실행과 원시 증거

```sh
unset TMUX TMUX_PANE
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
~/.claude/skills/statechart/scripts/statechart docs/statecharts/session-lifecycle/TmuxSession.tla --config docs/statecharts/session-lifecycle/EffortRestartDesign.cfg --view docs/statecharts/session-lifecycle/effort-restart.view.mjs --out /tmp/i8109-effort-new
python3 scripts/ci/statecharts_tlc.py --base 193a57168f --head HEAD --jobs 2
```

S1 raw evidence는 `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8109/sc-out/s1-<Name>/{tlc.out,tlc-<PROPERTY>.out}`와 `logs/s1-sc-<Name>.log`다. 새 4 cfg의 최종 view 결과는 `sc-out/s1-final-<Name>`와 `logs/s1-sc-final-<Name>.log`에 있다. legacy는 `sc-out/s1-Legacy`, `logs/s1-sc-Legacy.log`다. 최초 결과는 `<Name>-final2` 폴더에 남는다. 초기 렌더 label 오류는 `*-r2`, `*-checked` 등 이전 폴더에 별도로 남고 위 최종 결과에 섞지 않았다. 원시 TLC의 clean finish/temporal checking을 report 생성 도구가 검사했으며 최종 CI expected match는 커밋 후 루트가 실행한다.

최초 모델링 10:04~10:26 KST, S1 보강은 10:41 KST 시작이다. 각 레그의 30분 제한 내 멈춘다. Rust 시험·prod 빌드·GUI 조작은 실행하지 않았다.
