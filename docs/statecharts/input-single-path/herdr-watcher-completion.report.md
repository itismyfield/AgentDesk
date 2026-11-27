# Claude Herdr watcher의 terminal과 다음 입력 전달

기준 코드는 `82702fe252aa8efe01f36d7d066d36a207a4a630`이다. 운영 사례 조사 기준은 배포 `8c8c52bdc850c141cfab36515df39177dbd8d6cc`다. 아래의 Discord 파일 좌표는 `src/services/discord/` 기준의 main 좌표다.

[수정 전 차트](herdr-watcher-completion-before.chart.mmd) · [수정 후 차트](herdr-watcher-completion-after.chart.mmd)

## 상태 차트

[`InputOwnership.tla`](InputOwnership.tla)의 `TurnEnds`가 함께 바꾸던 native 상태와 mailbox를 분리한다. `MailboxReleases`의 rollback·cancel 전이를 이어받지 않고, 정상 종료에 terminal 권한과 exact lease 해제 조건을 둔다. `LiveSpec`의 전체 `Next` 약한 공정성 대신, 해당 단계가 계속 가능하면 실행된다는 가정을 각 동작에 둔다. [`NativeCompletionRecord.tla`](NativeCompletionRecord.tla)의 native-owned record → cleanup 규칙은 유지되며, Watcher Pending의 generic backstop에서 native custody로 남는 경계를 `KeepNativeCustody`로 표시한다. 두 역사적 모델과 기존 cfg는 그대로 사용한다.

### 진입점 전수표

| 진입점 | 모델 값 또는 전이 | 기준 코드와 권한 |
| --- | --- | --- |
| ClaudeTui 초기 좌표와 native handoff | `source = Prelaunch/Transcript`, `NativeHandoff` | `router/message_handler/provider_isolation.rs:141–185`, `router/message_handler/intake_turn.rs:1537–1542,1865–1884`의 경로 없는 0 시작점이 `inflight/model.rs:1127–1128`에 남는다. `turn_bridge/runtime_handoff_loop/watcher_handoff.rs:85–104,145–151`의 실제 transcript 채택은 `output_path`, `last_offset`을 갱신한다. |
| 호스트 상태와 본문 게시 | `host = Busy/Idle`, `priorPosts = 0/1`, `HostGoesIdle`, `PublishPriorAnswer` | presence Idle은 token이 있으면 owner wake를 만들지 않는다(`turn_presence/supervisor.rs:661–702`). 본문 게시와 watcher 프레임의 `terminal_commit_ack`는 별도다(`tmux_watcher/completion_gate.rs:119–135`, `tmux_watcher/terminal_relay_plan.rs:617–665`). |
| soft terminal, 같은 시작 구간 | `frame = Unread/Rejected/Submitted`. `OrdinaryInside`, `StoredAtFrameStart`, `StoredInside` | 기존 일반 시작점 검사는 `[data_start_offset, current_offset)`이다(`tmux_watcher/turn_identity/soft_terminal_authority.rs:160–187`). 일반 시작점이 `Some(0)`이고 프레임 시작이 양수인 ClaudeTui Watcher 행에는 같은 현재 Bound execution·행/프레임 turn nonce·source·파일의 저장 시작점을 쓸 수 있다. 저장 시작점은 `[max(data_start_offset, turn_data_start_offset), terminal_event_consumed_offset)` 안에 있어야 한다. |
| soft terminal, identity·경계 실패 | `OtherExecution`, `OtherTurn`, `OtherFile`, `OtherSource`, `UnpinnedFile`, `MissingStart`, `OtherWatcherNonce`, `Ownerless`, `OrdinaryAtFrameEnd`, `StoredAtFrameEnd`, `StoredBeforeFrame`, `StoredInBufferedTail`, `NonPrelaunchBeforeFrame`, `MissingOrdinaryStart`, `OtherRelayOwner`, `OtherRuntime` | `inflight/herdr_turn_evidence.rs:48–60,72–100,129–175,210–232`의 저장 시작, current Bound 및 dev/inode 근거와 `soft_terminal_authority.rs:160–187`의 owner/nonce/경계 검사. `MissingOrdinaryStart`는 일반 `last_offset` 대체값도 프레임 밖인 경우다. `terminal_relay_plan.rs:398–424`는 buffered read end와 소비된 terminal end를 구분한다. 실패한 soft candidate는 별도 exact O 전달 증거가 없으면 lifecycle에 들어가지 않는다(`tmux_watcher/terminal_relay_plan.rs:743–784`, `tmux_watcher.rs:1389–1393`). |
| strict probe와 만료 | `probes = 0/1/2`, `deadline = Far/Pulled/Extended1800`, `recheck = None/HandleMissing/Paused/PausedLive/Unknown/DoneWithoutOwnership` | 15초 간격으로 통과한 두 probe가 8초 deadline을 설정하고, 마지막 strict 판정이 false면 1,800초로 되돌린다(`turn_finalizer/watcher_backstop.rs:21–30,57–180,335–345`, `turn_finalizer/reconcile.rs:279–358`, `turn_finalizer.rs:101–119`). |
| 자연 deadline과 native custody | `RetryFar`, `BackstopTerminal`, `NativeCustody`, `NativeInput` | 자연 deadline은 `Done`의 전달 frontier 예외를 허용한다. 살아 있는 paused handle, PausedLive signal, pane fallback을 쓰지 않는 Herdr hold의 Unknown은 재연장될 수 있다(`turn_finalizer/watcher_backstop.rs:63–91,141–180,335–345`). 종료 판정 뒤에도 generic finalize는 `native_input`이 있는 Unix 항목을 보호한다(`turn_finalizer/reconcile.rs:15–18,39–44`). |
| 정상 watcher, 복원 watcher, 최종 release | `CanonicalRelease`, `lease`, `wake` | `tmux_watcher/terminal_commit_epilogue.rs:452–515`가 exact id·claim으로 canonical finalizer에 제출한다. `turn_finalizer/episode.rs:78–150`, `turn_finalizer/completion_admission_actor.rs:132–147`은 token 제거와 `QueueEligible`을 연결한다. bridge의 live Watcher 위임은 `turn_bridge/post_loop_finalize.rs:518–575`다. |
| 다음 입력 owner drive | `front = Queued/Held/Delivered/Answered`, `nextPosts = 0/1` | 활성 mailbox의 Herdr Busy 소스 관측과 hold는 `router/message_handler/intake_turn/owned_busy.rs:33–95,144–184,533–578`이다. `QueueEligible` 뒤 owner가 동일 front를 다시 진행한다(`turn_finalizer/completion_admission.rs:66–87`, `router/message_handler/provider_dispatch/owner_input.rs:407–426,461–472`). |
| 부팅과 같은 행 복원 | `runtime = Live/Restoring`, `restarted`, `Restart/RestoreInflight` | `inflight/model.rs:236–244`, `inflight.rs:616–637`이 같은 nonce의 `herdr_turn_start`를 보존한다. `recovery_engine/restore_inflight.rs:1983–2008,2199–2255`가 persisted watcher source와 nonce를 사용한다. 다음 프레임에서 current Bound 및 파일 증거를 다시 읽는다. |
| operator inspect/release 및 purge | 별도 mailbox 관측 응답 | GET/POST는 `src/server/routes/turn_lease.rs:15–47`, `turn_lease.rs:71–110,148–247,273–334`의 같은 node-local runtime/actor다. 미등록 및 actor 실패를 관측한 빈 token과 구분하고, purge의 권한은 `src/services/turn_orchestrator/registry_purge.rs:353–452`가 소유한다. |
| HTTP Cancel 및 대기 입력 취소 | 인접 종료/취소 진입점 | `src/server/routes/domains/runtime.rs:123` → `src/server/routes/queue_api.rs:209–238` → `src/services/queue.rs:354–373,460–522`. Herdr stop 보존은 `src/services/queue/herdr_stop.rs:13–45`, scoped force는 `src/services/queue/http_force.rs:53–87`, `/cancel-queued`는 `commands/control.rs:772–828`이다. |
| Herdr Terminate | 인접 host 종료 진입점 | `src/server/routes/herdr_terminate.rs:39–105`, `herdr_terminate.rs:147–185,346–406,490–510,588–627`. 현재 execution의 종료를 확인하고 `HostTerminated`를 제출한다. `herdr_terminate/retired_wake.rs:44–60`이 retired 이후 front를 깨운다. |
| clear와 관리자 정리 | 인접 명시/자동 정리 진입점 | `commands/control.rs:548–620,835–859`, `commands/text_commands.rs:100–109`, `commands/control/herdr_recovery.rs:49–90,200–234`의 active/잔존행 보존. `mailbox_finish.rs:362–423`, `session_idle_cleanup.rs:151–183`, `admin_host_guard.rs:194–265`, `health/recovery.rs:240–296`의 기존 clear guard를 사용한다. |
| `/stop`, `!stop`, agent Stop | 인접 stop settlement | `commands/control.rs:674–742`, `commands/text_commands.rs:98–99`, `commands/text_commands/native_stop.rs:11–25,73–76`, `src/server/routes/agents_stop.rs:40–79` → `turn_bridge/tmux_runtime/judged_stop.rs:59–109,363–400,436–449`. `turn_bridge/tmux_runtime/codex_stop_delivery.rs:415–495`의 `herdr_stop_result`는 Idle에 `NotSent`·host-owned settlement를 남긴다. |
| stale release와 #4030 demotion | 인접 생존/identity 판정 | `router/intake_gate/stale_turn.rs:148,291–302`, `router/intake_queue_transaction.rs:25`, `inflight/removal.rs:438–505`, `tui_direct_pending_start/foreign_recovery.rs:463–499`. Idle은 destructive release 또는 foreign row demotion 권한을 만들지 않는다. |
| fresh-idle과 Herdr own-terminal 증거 | 인접 독립 terminal 판정 | `tmux_watcher/no_result_exits.rs:89–150,170–223,297–341,436–535`는 fresh ready와 own evidence가 필요하다. `inflight/herdr_turn_evidence.rs:103–175,181–242`는 저장 시작과 현재 Bound execution/nonce/dev/inode에 묶인 own terminal을 요구하며, 부족하면 `PausedLive`를 사용한다. commit epilogue도 보존 결정을 재확인한다(`tmux_watcher/terminal_commit_epilogue.rs:262–278,452–483`). |
| 세션 재시작, process drain, incarnation 교체 | 인접 복구/종료 진입점 | `commands/restart.rs:214–325`, `commands/restart/herdr.rs:257–311,396–425`, `commands/effort_restart/herdr.rs:155–172,222–229`, `herdr_terminate/restart_recovery.rs:67–112,137–162`, `runtime_bootstrap/shutdown.rs:133–219`. `runtime_bootstrap/recovery_flush.rs:30–74`, `runtime_bootstrap/queued_recovery.rs:16–58,195–216`, `recovery_engine/herdr_row_end.rs:33–71`, `recovery_engine/herdr_row_end_live.rs:29–67,118–138`, `health/recovery/carried_turn_close.rs:146–180,247–341`은 종료 또는 이전 generation 정리를 각자 증명한다. |

### 입력 출처

| 출처 | 시작점과 terminal 권한 |
| --- | --- |
| 사람의 owner front | `owned_busy.rs:33–95,144–184`가 이전 episode의 active 상태에서 확인되지 않은 source를 만나면 front를 유지한다. 이전 episode의 exact terminal과 `QueueEligible` 뒤 다시 배달한다. 자기 턴이 되면 자기 nonce와 저장 시작을 사용한다. |
| busy follow-up retry | `router/message_handler/intake_turn.rs:1865–1884`, `busy_followup_retry_store.rs:264,645`의 retry message id는 원래 front/notice 추적에 쓰인다. terminal 시작점 대신 사용할 수 없으며, follow-up이 새 턴을 이루면 새 episode와 그 시작 증거를 사용한다. |
| 시스템 주입 | `src/server/routes/domains/runtime.rs:133`, `router/message_handler/headless_turn/entrypoints.rs:200–223`, `health/turn_deliver.rs:164–181`의 machine start는 헤드리스 예약과 기존 admission을 사용한다. 시스템 출처 자체로 terminal 권한을 얻지 않는다. 동일 ClaudeTui·Watcher·Herdr proof가 생긴 episode에 같은 판정을 적용한다. |

## 설계 선택

- Native handoff 또는 guarded stamp에서 일반 시작점을 다시 찍는 안은 이후 복원과 여러 비교자가 하나의 좌표를 쓰게 한다. handoff의 `last_offset`은 해당 terminal 레코드의 끝일 수 있다(`src/services/claude/herdr_turn/first_input.rs:636–660`). 이 안에는 원래 시작점을 특정하는 별도 근거와 모든 cursor 소비자의 범위 검증이 필요하다.
- soft terminal의 구간 확인에서 검증한 Herdr 저장 시작을 쓰는 안은 이미 durable한 episode 증거로 프레임을 판별한다. 현재 Bound execution과 watcher의 같은 nonce·source·파일을 함께 검증하고 일반 시작점이 `Some(0)`인 양수 suffix 프레임에서 소비된 범위까지 확인할 수 있어 이 안을 선택했다. 일반 시작점과 cursor의 저장 계약은 보존된다.

같은 행을 부팅 때 읽으면 저장된 Herdr 시작점의 nonce와 dev/inode가 남는다. fail-closed 처리는 복원 행의 새 incarnation, 다른 파일, 경로 전환 및 file pin 없는 옛 형식에 동일하게 적용된다. 복원 watcher의 일반 읽기 위치는 여전히 `max(last_offset, turn_start_offset)`이며 truncate 때는 0이다(`recovery_engine/terminal_watcher.rs:128–149`). `Restart`의 GREEN은 동일 행에서 프레임이 다시 관측되는 경우를 다룬다. 부팅 전에 이미 지나간 terminal에 대한 강제 rewind는 포함하지 않는다.

## 검증 범위

`AnswerOnce`는 답 게시 수를, `NoSilentLoss`는 exact frame을 놓쳐 지속적인 재연장에 갇힌 front가 없는지를 검사한다. `HeldEventuallyDelivered`는 호스트 Idle, 앞 답 게시, front Held가 함께 관측되면 front가 전달 또는 응답 상태에 도달한다는 liveness다. `DeliveredEventuallyAnswered`는 전달된 front의 답 게시까지 확인한다. 본문이 이미 게시되는 흐름을 모델 경계로 삼으며, 별도 relay writer·네트워크 오류·canonical CAS 실신패·#8430 보류 상한은 이 모델의 탐색 범위에서 제외한다.

`Before`/`After`는 같은 episode의 저장 시작이 프레임 첫 바이트 또는 내부에 있는 경우다. `Boundaries`와 `IgnoreIdentity`는 외부 프레임의 거부와 이후 독립적인 natural backstop을 검사한다. 유효하지 않은 프레임은 자체로 전달 liveness의 근거를 만들지 않는다. `Restart`는 한 번의 재시작 뒤 같은 durable 행을 복원하는 경우다. `DoneWithoutOwnership`과 handle 미관측의 자연 deadline 종료는 허용한다. paused/PausedLive 및 pane fallback이 없는 Herdr Unknown이 계속되면 다음 자연 deadline도 연장되는 경로를 허용한다.

## TLC 결과

| cfg (`HerdrWatcherCompletion…`) | 이전/이후 | 실제 탈락 규칙 | TLC 실행 수 | 초 |
| --- | --- | --- | ---: | ---: |
| `Before.cfg` | 기존 authority RED | `NoSilentLoss`, `HeldEventuallyDelivered` | 4 | 6.8 |
| `After.cfg` | 같은 입력 및 exact frame GREEN | 없음 | 3 | 5.3 |
| `Boundaries.cfg` | 파일·소스·execution·nonce·owner·offset 경계 GREEN | 없음 | 1 | 1.7 |
| `IgnoreIdentity.cfg` | 위 경계에서 저장 시작의 execution·turn nonce·파일 확인을 제거한 뮤턴트 RED | `ExactFrameAuthority` | 2 | 2.6 |
| `Restart.cfg` | 동일 행 복원 GREEN | 없음 | 3 | 5.4 |

TLC 1.7.4의 저장소 checker는 5 cfg를 선택해 21.8초에 5개 기대 결과를 확인했다(exit 0). 상태 차트 도구는 `Before` 118 상태·208 전이(exit 1), `After` 36 상태·56 전이(exit 0)를 탐색했다. `Before`의 8단계 안전성 반례는 경로 없는 초기 좌표 → transcript handoff → 호스트 Idle → front Hold → 앞 답 게시 → terminal 거부 → strict probe 두 번 → 1,800초 연장이다. 10단계 liveness 반례는 연장 뒤 자연 deadline 재확인과 재연장을 반복한다. `Before`의 전달 후 응답 liveness는 전달되는 자연 backstop 경로도 포함하고, 그 경로의 `AnswerOnce`가 유지된다. 차트의 동일 라벨에 머무는 `Before` 모델 전이 8개는 그림에서 생략된다.

아래 명령은 모델과 cfg를 scratch로 복사하고 5개 기대 행만 골라 저장소 checker를 실행한다. 차트는 `suffix`에 `Before` 또는 `After`를 넣어 새 출력 폴더로 다시 만든다.

```sh
unset TMUX TMUX_PANE
export TLA2TOOLS="$HOME/.local/share/tla2tools/tla2tools-1.7.4.jar"
export JAVA=/opt/homebrew/opt/openjdk/bin/java
scratch_dir=$(mktemp -d)
mkdir "$scratch_dir/input-single-path"
cp docs/statecharts/input-single-path/HerdrWatcherCompletion.tla \
  docs/statecharts/input-single-path/HerdrWatcherCompletion*.cfg \
  docs/statecharts/input-single-path/herdr-watcher-completion.view.mjs \
  "$scratch_dir/input-single-path/"
rg '^input-single-path/HerdrWatcherCompletion' docs/statecharts/expected.tsv > "$scratch_dir/expected.tsv"
python3 scripts/ci/statecharts_tlc.py --root "$scratch_dir"
suffix=After
"$HOME/.codex/skills/statechart/scripts/statechart" \
  "$scratch_dir/input-single-path/HerdrWatcherCompletion.tla" \
  --config "$scratch_dir/input-single-path/HerdrWatcherCompletion${suffix}.cfg" \
  --view "$scratch_dir/input-single-path/herdr-watcher-completion.view.mjs" \
  --out "$(mktemp -d)"
```
