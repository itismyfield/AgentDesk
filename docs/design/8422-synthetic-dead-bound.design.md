# #8422 설계 — 죽은 Bound 실행이 있는 Herdr 채널에서 synthetic 입력이 claim 전에 영구 보류된다

- 개정: **rev2**(WebGPT pro 설계 리뷰 `review-design-1.out.md`의 P1 3·P2 2 반영). rev1 대비 바뀐 곳은 0절, 1절 4단계, 3절 사람·HttpStart 행, 4절, 5~7절이다.
- 코드 기준: `82b96fa94b`(#8399 S3b-1 w3. `refs/review/i8399-s3b1-w4`는 아직 없다). 아래 file:line은 모두 이 SHA에서 다시 확인했다.
- 상태 차트: [`HerdrSyntheticStart.tla`](../statecharts/input-single-path/HerdrSyntheticStart.tla), [결과 보고](../statecharts/input-single-path/herdr-synthetic-start.report.md).
- 구현은 이 문서에 넣지 않았다. 설계만 담는다.

## 0. 한 줄 결론

synthetic start의 claim 전 판독(`start_wait::observe`)이 **provider와 상관없이**(Claude·Codex) #8212가 사람 입력에 쓰는 '죽은 Bound 판정·retire'를 **같은 공통 함수로** 묻게 한다.
공통 함수는 요청한 provider를 받아 owner와 비교한다. 증명은 CLI `herdr retire`와 같은 `retire::observed(Scope::Ended)`를 쓴다.
마지막 무턴 판독부터 CAS까지 channel transition lease를 쥔다. claim 전·watch 호출은 `try_acquire`로 얻고, 이미 transition 안에 있는 사람 입력은 그 guard를 `borrow`해서 넘긴다. lease를 못 얻거나, token이 있거나, inflight 판독이 정확히 `Ok(None)`이 아니면 Reobserve다.
retire가 되면 같은 판독이 Ready를 내고, start는 claim한 뒤 새 실행(Cold)으로 전달된다.
끝내 retire할 수 없는 이유(소유자·endpoint 불일치, 행 판독 불가)가 나오면 새 `fail_claimed_locally`로 정산한다. 원 origin의 outcome은 **Failed**, 사유는 채널 또는 durable outbox에 남기고, 반환은 `NotSent(..)`다.

## 1. 정체 경로(82b96fa94b)

1. `admit_start`(owner_input.rs:713) → `start::admit_synthetic` → drive → `start_idle`(owner_input.rs:943).
2. `start_idle`은 `!joins_running_turn()`인 출처(RestartSeed 제외)를 `start_waits`로 보낸다(owner_input.rs:963-971).
3. `start_waits`(owner_input.rs:770): boot·inflight·rest 검사를 지난 뒤 `start_wait::observe`(owner_input_start_wait.rs:33)를 부른다.
4. `observe`: Claude는 Bound 행이면 `claude_handoff::observe_busy`(claude_handoff.rs:569) → `busy::observe_herdr` → `Context::read`(busy/herdr.rs:303-306) `!execution_alive() → Reading::held()`.
   Codex는 그보다 먼저 `activity_now`로 조기 반환한다(owner_input_start_wait.rs:67-76). watcher가 없는 채널은 unbound 판독이 Bound 행을 `Hold("hosted_active_or_unknown")`으로 남겨(turn_presence/activity/unbound.rs:319-329) `Activity::Unknown → Screen::Unknown`이 된다. 끝난 rollout의 마지막 턴이 Open이면 Busy를 거쳐 Unknown("open_without_progress")으로 남는 경로도 있다(리뷰 P1-1 근거).
5. `BusyState::Held → Screen::Unknown`(owner_input_start_wait.rs:91-95) → `Wait::Watch`(owner_input.rs:791-803) → `held_watch::arm` + `hold_start` → `Err(START_WAITS)`(owner_input.rs:974-978).
6. held_watch는 2초마다 같은 `observe`를 다시 부른다. Unknown이면 Keep, Gone이면 **wake 없이** End다(held_watch.rs:65-71).
   herdr 판독은 Gone을 내지 않으므로 Keep이 끝없이 이어진다.
7. hold에는 시간 deadline이 없다(drive.rs:110-114는 held면 watch만 건다).
   #8212 수리(`OwnerTurn::early` → `recover_or_discard`, owned_busy.rs:99-111, 208-339)는 `joins_running_turn()`인 사람 입력에만 열린다(owned_busy.rs:111).

## 2. 진입점 전수표

### 2a. synthetic 입력이 claim 전에 hold·대기되는 지점

| # | file:line | 무엇 | 모델 값·행동 |
|---|---|---|---|
| A1 | owner_input.rs:771-774 | `boot_ready` 아님 → `Wait::Event` | Drive→queued(awake=F); boot ready wake |
| A2 | owner_input.rs:778-783 | non-Managed inflight 행 또는 `holds_rest` → `Wait::Event` | `turn="external"` Drive; TurnEnd(row end wake) |
| A3 | owner_input.rs:784-804 | `observe`가 Native(Ready 아님) → `Wait::Watch(observe 재호출)` | Drive→held+watch; **정체 지점** |
| A4 | owner_input.rs:805-813 | tmux transcript_turns, activity busy → `Wait::Event` | Host=tmux Drive(exec=busy); NativeEnds+presence wake |
| A5 | owner_input.rs:813-821, 858-872 | tmux pane `native_wait`: Blocked/Unknown → Watch, Ready/Gone → Go | Host=tmux TmuxRead |
| A6 | owner_input.rs:846-856 | pane 이름 미정 → `named_wait` Watch | (모델 밖: 이름 확정 시 Ready) |
| A7 | owner_input_start_wait.rs:61-65 | `HostedRowUnreadable` → Unknown | (모델 밖: 일시 판독 실패, watch가 다시 읽음) |
| A8 | owner_input_start_wait.rs:78-86 | Bound 행인데 `start_target` 실패 → Unknown | Foreign/unknown 계열; 수리 뒤 공통 함수가 판단 |
| A9 | owner_input_start_wait.rs:91-95 | `BusyState::Held/Legacy → Unknown` | `HerdrRead`의 dead→Unknown; **정체 원인** |
| A10 | busy/herdr.rs:304-306 | `!execution_alive() → Reading::held()` | exec=dead |
| A11 | claude_handoff.rs:587, 590-600, 602-604 | Unknown record·owner 불일치·target 없음 → Held | Foreign=TRUE |
| R1 | owner_input.rs:262 | 활성 턴 중 비사람 start → `Err(START_WAITS)` (hold 없음, prefer_person 뒤 stalled) | turn 경계 wake |
| R2 | owner_input.rs:974-978 | `Wait::Watch` → arm + `hold_start` + `Err(START_WAITS)` | Drive→held |
| R3 | owner_input.rs:979-981 | `Wait::Event` → `Err(START_WAITS)` (hold 없음) | Drive→queued(awake=F) |
| R4 | owner_input.rs:1115-1118 | claim 뒤 늦은 판독이 START_WAITS → `hold_start` | tmux 전용(R5에서 옴) |
| R5 | owner_input.rs:1220-1244 | `execute_owned` tmux synthetic late read Blocked/Unknown → arm + `Err(START_WAITS)` | tmux 전용(`TurnHost::Tmux` 조건, :1221-1223) |
| H1 | owner_input.rs:188-196 | `restart_gate::inflight` → `hold_start` + WRITER_HELD | 재시작 경로(4절) |
| H2 | owner_input.rs:197-220, 1078-1092 | writer boot held → bootstrap_probe watch + hold | 재시작 경로(4절) |
| H3 | owner_input.rs:1107-1113 | canary bootstrap → BOOTSTRAP_WAITS hold | 재시작 경로 |
| H4 | owner_input/preclaim_wait.rs:27-40 (start_idle:961) | root recovery 보류 → retry wait(due 있음) | 모델 밖(due로 깨움) |
| H5 | claude_handoff.rs:85-95, 344-358 | handoff journal `retry_after` → `hold_front` + schedule | 모델 밖(schedule이 :470-478에서 깨움) |
| H6 | turn_orchestrator/queued_head/v2.rs:554-561 | `hold_front`: 원장에 held 표시 | 모든 hold의 공통 기록 |
| H7 | held_watch.rs:99-160 | watch 루프, `verdict`(:60-66) | WatchLook |

START_WAITS를 돌려주는 곳은 R1·R2·R3·R4·R5 다섯 곳이다(정의 owner_input.rs:1127).
`target.rs:78/182/200`의 `Route::Held("synthetic_waits_idle")`는 busy=true로 등록된 경우의 다른 판정이고, 이 경로에 해당하지 않는다(코디 추적과 같다).

### 2b. Herdr hosted_execution 행을 Bound→Retired로 바꾸는 경로

| # | file:line | 진입 | 판정 조건 | 턴·inflight가 있으면 |
|---|---|---|---|---|
| B1 | herdr_launch/retire.rs:35-67, 70-103 | `retire::observed(scope)` + `on_reading` | `view.ended`(herdr_gate.rs:914-945: pane Missing 또는 기록된 root shell만 남음) + nonce CAS. Scope::Missing은 Missing만 받는다 | 자체 검사 없음(호출자 책임) |
| B2 | owned_busy.rs:208-301 | **#8212** 사람 입력 `recover_or_discard` | 소유자·endpoint 9항 일치 + `root_replaced::current` + B1(Scope::Missing) + hold 해제 + 재판정 | 호출 전 `has_active_turn()==Ok(false)`(owned_busy.rs:161) |
| B3 | herdr_reader.rs:449-458 | 부팅 재연결 Missing | `HostReconcile::Missing` + B1(Scope::Missing) | 부팅 전 |
| B4 | cli/herdr.rs:224 | CLI `agentdesk herdr retire` | B1(Scope::Ended) | 운영자 판단 |
| B5 | herdr_terminate.rs:414-441, 229-250 | `terminate_explicit_herdr` → `terminate_scoped` → `retire_confirmed` | close 전송 + `read_end` + CAS. 끝에 RetiredWake(retired_wake.rs:46-61) | `capture_turn`(herdr_terminate.rs:151-190)으로 잡아 정산 |
| B6 | herdr_terminate.rs:710, server/routes/herdr_terminate.rs:92-94 | 운영자 terminate 라우트 | B5 | B5 |
| B7 | commands/control/clear_teardown.rs:210, clear_teardown/execution_cleanup.rs:123 | `/clear`·`!clear` | B5 | B5 |
| B8 | herdr_terminate/http_force.rs:18 | HTTP force cancel | B5 경유 | B5 |
| B9 | herdr_terminate/never_started.rs:24-26, queue_io/input_owner/pending_recovery.rs:32 | **S3b-1** 자동 never-started | Pending + launch 증거 + 시작 기록 없음. **Bound는 받지 않는다** | claim한 front만, 턴 있으면 거절 |
| B10 | recovery_engine/herdr_row_end.rs:418, 509; herdr_row_end_live.rs:70, 136 | inflight 행 끝 정리 | `execution_end==Ended` + CAS | **inflight 행이 있을 때만** 돈다 |
| B11 | herdr_terminate/restart_recovery.rs:74, 143-152; commands/restart/herdr.rs:396 | restart 명령 회수 | `execution_end==Ended` | restart 범위 |
| B12 | herdr_terminate/root_replaced.rs:539 | root 교체 회수 | root 교체 증명 | 회수 범위 |
| B13 | turn_finalizer/native_terminal.rs:133 | native terminal 판독 | `execution_end` | 턴 끝 |
| B14 | claude/herdr_return.rs:84; herdr_launch.rs:711; herdr_server_replaced.rs:286 | tmux 복귀·launch 교체·server 교체 | 각자 CAS | 각 경로 |

inflight 행이 없는 채널의 Bound 행을, 사람 입력 없이 자동으로 retire하는 경로는 없다. B10은 행이 있어야 돌고, B9는 Pending만 받고, B3은 Missing만 받는다.
그래서 pane이 남아 있고 provider만 끝난 죽은 Bound(lone root shell)는 재시작해도 그대로다. 운영 사례에서 "배포 재시작 뒤에도 풀리지 않음"은 이것으로 설명된다.

### 2c. held front를 다시 깨우는 경로

| # | file:line | 계기 | 죽은 Bound에서 오는가 |
|---|---|---|---|
| C1 | held_watch.rs:65-71, 149 | watch 판독 Ready → `wake` | **안 온다**(Unknown Keep) |
| C2 | held_watch.rs:67 | Gone → End(**wake 없음**) | 와도 깨우지 않는다 → 후보 ① 단독은 무효 |
| C3 | herdr_terminate/retired_wake.rs:46-61 | B5 계열이 행을 바꾸고 queue가 남았을 때 owner_wake | 운영자·clear·HTTP force가 있을 때만(수동) |
| C4 | turn_presence/supervisor.rs:691-701, 873-877 | owned 채널의 busy→idle(`turned_idle`) → WakeOwner | herdr 죽은 Bound는 idle 전환이 없다 |
| C5 | queue_io/input_owner/ingress.rs:463-466 | 새 입력 Accepted → owner_wake | 새 입력이 오면 다시 같은 판정(정체 반복) |
| C6 | queue_dispatch/kickoff.rs:120-127; queue_io.rs:381-384; backstop.rs:428, 834 | idle kickoff·writer ready·backstop | 같은 판정 반복 |
| C7 | queue_io/input_owner/claude_handoff.rs:470-478 | handoff journal 만기 | 해당 없음 |
| C8 | queue_io/input_owner/routine.rs:399; restart_gate.rs:133; start_failure.rs:349 | 루틴·restart gate·start 실패 정산 | 해당 없음 |
| C9 | drive.rs:124-170 | owner deadline(`settle_due`) | hold에는 due가 없다 |

## 3. 입력 출처 분류(origin.rs:4-16, 36-38)

| 출처 | 만드는 곳 | `start_waits`를 타나 | 수리 영향 |
|---|---|---|---|
| HttpStart(wake-watch 재개·`agents/turn/start`) | headless_turn/admission.rs:111 → owner_input.rs:713, context `origin:"headless"`(start.rs:95) | 탄다. `keyed_to_channel=Some(true)`(owner_input_start_wait.rs:24-30) | **주 대상**: Claude·Codex 모두 죽은 Bound를 retire하고 Cold로 전달 |
| Routine | routines/agent_executor/owner_input.rs, scheduled_messages/owner_input.rs | 탄다. 채널 키 여부는 `owner_start_takes_channel` | 채널 키면 같은 수리, 아니면 Legacy(변화 없음) |
| EffortContinue | v2_types/effort_restart.rs:64 | `start_idle`에서는 탄다(owner_drive가 handoff만 건너뛴다, owner_input.rs:235-237) | 같은 수리 |
| Decision | decision_drain | 탄다(`is_person`이지만 `joins_running_turn=false`, origin.rs:36-38) | 같은 수리. 거절 시 원 event 실패 정산을 지킨다(`NotSent` 반환, decision_drain.rs:172-179) |
| RestartSeed | restart_seed.rs:95 | 안 탄다(owner_input.rs:964-967 `Wait::Go`, greeting permit이 판정) | 없음. 재시작 뒤 Bound는 S3b-2 범위 |
| Discord·iMessage(사람) | intake | 안 탄다(`joins_running_turn`, Wait::Go) → claim 뒤 `OwnerTurn::early` | 공통 함수로 바뀐 #8212 경로(Claude만, owned_busy.rs:111). 증명 범위가 Missing→Ended로 넓어지고, transition guard를 borrow한다. Refused 정산도 Completed→Failed로 바뀐다(T10) |

## 4. 재시작·부팅·incarnation과 인접 소비 경로

- 부팅 재연결(herdr_reader.rs:440-470): Missing만 retire(B3), MissingMarker·RootReplaced는 각자 회수. lone root shell은 Bound로 남는다. 이 설계는 부팅 경로를 바꾸지 않는다. 다음 synthetic·사람 입력이 그 채널을 열 때 공통 함수가 정리한다.
- `boot_ready` gate(A1)와 restart gate·writer boot hold(H1~H3): 재시작 중 hold는 bootstrap probe와 boot ready wake로 풀린다. 수리는 이 앞단을 건드리지 않는다. 공통 함수는 `start_waits`의 boot 검사를 지난 뒤에만 불린다.
- incarnation: `recovered_missing`의 MISSING 목록(retire.rs:107-121)과 `withhold_herdr_execution`은 프로세스 메모리다. 새 프로세스는 DB 행만 다시 읽는다. 공통 함수의 CAS는 nonce·owner를 같이 보므로(retire.rs:93) 다른 incarnation이 이미 retire·relaunch한 행은 Stale로 거절된다(Retry → watch).
- S3b-1 `pending_recovery`(B9): Pending만 받는다. 이 설계는 그 계약을 바꾸지 않는다(Bound는 계속 받지 않는다).
- Cancel(HTTP force B8)·Terminate(B5·B6)·clear(B7): `capture_turn`으로 턴을 잡고 retire하고 RetiredWake로 깨운다. 공통 함수는 이 기계를 부르지 않는다. 다만 같은 행을 두고 경합할 때는 둘 다 nonce CAS(`retire_pg`)를 쓰므로 한쪽만 Written이 된다. 공통 함수 쪽이 Stale이면 Retry로 watch에 남고, B5의 RetiredWake가 깨운다.
- stop(`/stop`): 활성 턴의 cancel token을 끊는다. hosted 행을 바꾸지 않는다. 턴이 끝나면 turn 경계 wake(C4·C6)가 front를 다시 판정한다.
- turn_presence unbound 판독(`activity/unbound.rs:319-329`, `unbound/herdr_bound.rs`): 행을 바꾸지 않는다. 다만 Codex start 판독(`activity_now`)의 입력이므로, Codex 죽은 Bound가 Unknown으로 오는 출처다(P1-1).
- channel transition lock: headless claim은 lock을 얻은 뒤 token을 등록한다(router/turn_start.rs:142-179, 획득 :151). 사람 입력은 intake에서 guard를 얻어(intake_turn.rs:663, runtime_transition.rs:4-60, turn_start.rs:577-605) `OwnerTurn::early`(intake_turn.rs:985)를 지나 mailbox claim(:1010 `complete_mailbox_claim`)까지 쥔다. 공통 recovery는 이 lock에 참여한다(5.1 계약 L).

## 5. 수리안 비교

### 안 A(권함, 리뷰도 유지 권고) — 공통 '죽은 Bound recovery'를 provider와 상관없이 claim 전 판독이 묻는다

- 흐름: `observe`가 Bound 행의 provider 판독에서 보류(Claude `BusyState::Held|Legacy`, Codex `Activity::Unknown`)를 얻으면 공통 함수 `dead_bound::recover`를 부른다. busy(Blocked)·Ready는 **그 실행이 살아 있음이 확인된 경우에만** 빠른 길을 탄다(impl-w2, 9절).
  - 원칙: rollout의 Idle은 턴 종료 증거이지 실행 생존 증거가 아니다. Codex의 activity 판독이 Ready/Blocked여도 정확한 endpoint·실행의 `execution_alive()`가 거짓이면 Unknown으로 보고 recovery로 보낸다.
  - `Retired`면 행을 다시 읽어 Ready → `Wait::Go` → claim → `read_as`의 `bound_herdr`가 None → `Phase::Absent` → 새 실행(새 nonce) launch(Cold).
  - `Reobserve`·`Retry`(증명 안 됨, token·inflight 있음, lease 못 얻음, CAS Stale, endpoint 없음)면 지금처럼 Unknown → watch. watch probe가 `observe`를 다시 부르므로 판독마다 recovery를 다시 묻는다.
  - `Refused`(소유자·endpoint 불일치, 행 판독 불가)면 `Wait::Reject(reason)` → claim → `fail_claimed_locally` → Failed 정산.
- 비용·위험은 5.3에 정리했다.

### 안 B — 판독이 Gone을 내고 S3b-1 terminate 기계로 끝낸다(rev1과 같이 기각)

held_watch는 Gone을 wake 없이 끝낸다(held_watch.rs:65-71, 모델 `GoneNoRetire` RED). 또 S3b-1 `NeverStartedTerminate::admits`의 "a Bound one never is" 계약을 바꿔야 한다. 이미 죽은 실행에 close를 보내 Indeterminate가 늘고, `provider_absent`에는 root shell 대조가 없다.

③(owner_input.rs:101 alive를 execution_alive로)은 하지 않는다. claim 전에 행이 Retired가 되면 `read_as`가 이미 Absent를 고른다(리뷰도 확인).
④ tmux `Wait::Event`는 해당 없음이다. Event 뒤 pane이 죽으면 pane 사망 감지 → watcher 종료 → registry binding 제거 → unbound 부재 판독 Idle → turn_presence WakeOwner 순서로 깨어난다(리뷰가 소스로 확인). T8에서 이 순서 전체를 고정한다. 모델 `TmuxDead`는 처음부터 죽은 경우만 다루므로 이 순서의 증거로 쓰지 않는다.

### 5.1 공통 함수 계약

`dead_bound::recover(shared, provider, (channel, key, logical), host, mailbox, lease: LeaseRef<'_>) -> Result<Retired, DeadBound>`
`DeadBound = Refused(String) | Reobserve(String) | Retry(String)`

- **P(provider, 리뷰 P1-1)**: 요청한 `ProviderKind`를 받는다. owner 검사(지금 owned_busy.rs:239-253)의 `record.owner.provider != "claude"`를 `record.owner.provider != provider.as_str()`로 바꾼다. 나머지 8항(token hash, channel, logical key, runtime root, local node, endpoint 4항)은 그대로 둔다.
  종료 증명은 provider와 상관없는 `retire::observed(pool, row, Scope::Ended)`다(herdr_gate.rs:914-945 `ended_across`: Missing이거나, 기록된 root shell만 남고 provider가 끝남). provider마다 retire 함수를 따로 만들지 않는다.
- **L(transition lease, 리뷰 P1-2)**: 마지막 무턴 판독부터 CAS까지 `SessionTransitionLease`(session_transition.rs:17-77)를 쥔다.
  - `LeaseRef::Acquire`: claim 전 판독과 held_watch probe가 쓴다. `try_acquire`(:29-44)이 `SessionTransitionBusy`이면 곧바로 `Reobserve("transition busy")`다. 기다리지 않는다.
  - `LeaseRef::Borrow(&OwnedMutexGuard<()>)`: 이미 intake transition 안에 있는 사람 입력이 쓴다(`borrow`, :46-62). guard는 `IntakeRuntimeTransition`의 private `_guard`(turn_start.rs:577-581)라서 `fn guard(&self) -> &OwnedMutexGuard<()>` accessor를 더하고, `OwnerTurn::early`에 인자로 넘긴다(intake_turn.rs:985 호출 교체). 자기 lock에 막히지 않는다.
  - lease 아래 순서: ① lease 획득 → ② `mailbox.has_active_turn()==Ok(false)` → ③ `load_inflight_state_read_only_result(provider, channel)==Ok(None)` → ④ `root_replaced::current` → ⑤ `retire::observed(Scope::Ended)`(pane 판독 + nonce CAS) → ⑥ hold 해제 확인·`withhold_herdr_execution` → ⑦ lease 해제 → ⑧ host 재판정. ②~⑤가 한 lease 안에 있어야 한다.
  - 배제 근거: headless claim은 같은 lock을 `acquire`한 뒤에야 token을 등록한다(turn_start.rs:151 → :161-172). 사람 입력은 guard를 쥔 채 mailbox claim을 한다(intake_turn.rs:1010). 그래서 ② 뒤 ⑤ 전에 이 두 경로의 새 token이 끼어들 수 없다. 반대로 headless claim이 먼저 lock을 쥐면 ①이 Busy로 Reobserve가 된다. 다만 headless claim의 `acquire`는 최대 3초 기다리므로(session_transition.rs:4-5) ①~⑦은 짧아야 한다. ⑤는 pane snapshot과 process 판독뿐이고 close를 보내지 않는다.
  - lock 밖 token 등록 경로 감사(구현 레그 첫 단계): `mailbox_try_start_turn*` prod 호출자 중 위 두 경로 밖에 있는 것은 mailbox_ops.rs, recovery_engine/runtime.rs, tmux_reaper.rs, relay_recovery_auto_heal_apply.rs, idle_detector.rs, turn_finalizer/{finalize.rs, cleanup/terminal_release.rs}, tui_prompt_relay/synthetic_start/bridge_handoff.rs(admission_fence가 lease를 씀, admission_fence.rs:40-44), outbound/o_writer_io/agy.rs다. 각 호출이 lock 아래인지, 아니면 증명된 죽은 Herdr 실행에 token을 걸 수 없는 경로(tmux·살아 있는 watcher 복원)인지 표로 남긴다. 둘 다 아니면 그 호출에 lease를 더하는 것까지 이 조각에 넣는다.
  - held_watch 순서(held_watch.rs:121-132: probe(:123) → `leads`(:125) → generation(:130)): recovery의 부작용은 lead·generation 확인보다 먼저 일어날 수 있다. 그래도 안전 조건(무턴·inflight·증명)은 모두 lease 안 ②~⑤에서 다시 보므로, front가 더는 선두가 아니어도 retire 자체는 맞다(죽은 실행만 Retired). lead·generation은 wake 여부만 정한다. 다음 front는 Retired 행을 읽고 Cold로 간다.
- **T(무턴 판독, 리뷰 P2-1)**: inflight는 `load_inflight_state_read_only_result`(inflight.rs:930-945)로 읽는다. `Ok(None)`만 통과한다. `Ok(Some(_))`는 `Reobserve("inflight row")`, `Err(_)`는 `Reobserve("inflight unreadable")`다. `.ok().flatten()`인 `load_inflight_state_read_only`(:912-920)는 쓰지 않는다. `start_waits`의 non-Managed 검사(owner_input.rs:778-783)는 지금처럼 둔다. 판독 오류는 거기서 None으로 지나가도 recovery에서 다시 막힌다.
- **R(거절 정산, 리뷰 P1-3)**: `Failure::discard`(start_failure.rs:117-176) → `complete_claimed_locally`(control.rs:337-363, 항상 `TerminalOutcome::Completed`)는 쓰지 않는다.
  - control.rs: `complete_claimed_locally`의 몸통을 `fn take_claimed(&mut self, (front, number), followers, outcome: TerminalOutcome)`로 옮긴다. `complete_claimed_locally`는 `Completed`로, 새 `fail_claimed_locally`는 `Failed`로 그것을 부른다. 성공 의미는 그대로다. `owe_queued(.., Failed)`의 선례는 readiness_hold.rs:124에 있다.
  - start_failure.rs: 안내·outbox 블록(:143-175)을 `fn notify(..)`로 떼어 `discard`와 새 `fail`이 같이 쓴다. `fail`은 정확한 claim(`claim_number`·`claimed`)을 확인한 뒤 `fail_claimed_locally`로 원본과 follower를 제거한다. 사유는 채널 안내, 실패하면 recovery_notice outbox로 남긴다. 반환은 `Err("NotSent(\"hosted_recovery_refused\")")`다.
  - Decision: `decision_drain`은 `Some(Ok(()))`를 전달로 센다(decision_drain.rs:172-173). `NotSent` 오류는 `unsent`로 판정되어 `fail`로 정산된다(:174, :180-182).
  - 사람 입력의 Refused 분기(owned_busy.rs:304-313, 지금은 `Failure::owner(..).discard`)도 같은 `fail`로 바꾼다. 같은 공통 recovery의 같은 결과라서 두 경로의 outcome이 갈리지 않게 한다.
- **C(로그, 리뷰 P2-2)**: watch probe는 `start_wait`의 probe 생성 함수 안에 마지막 사유(`Option<String>`)를 붙든 `FnMut` closure로 만든다. 사유가 바뀔 때만 `decided(..)`(owned_busy.rs:582-590) 수준의 info를 남기고, 같은 사유는 debug로 낮춘다. 캐시·backoff·새 감시는 더하지 않는다.

### 5.2 수정 지점 전수표(안 A, rev2)

| 파일 | 변경 | 추정(순) |
|---|---|---|
| `src/services/discord/router/message_handler/intake_turn/owned_busy/dead_bound.rs`(새 child) | 5.1 계약 P·L·T 본체. owned_busy.rs:220-301 블록(82줄)을 옮기고, provider 인자, `LeaseRef`, lease 안 ②③ 판독, 오류 분류를 더한다 | +170(이동 82 포함) |
| `.../intake_turn/owned_busy.rs`(813줄) | `recover_or_discard`의 블록을 `dead_bound::recover(.., LeaseRef::Borrow(guard))` 호출로 바꾼다. `early`가 guard 인자를 받는다. Refused 분기는 `Failure::fail`. `mod dead_bound;`(FILES_AUTO) | −82/+18 |
| `src/services/discord/router/message_handler/intake_turn.rs`(2262줄, giant) | 호출 교체만: `OwnerTurn::early(.., intake_runtime_transition.guard())` 인자 1개 | +1 |
| `src/services/discord/router/turn_start.rs`(670줄) | `IntakeRuntimeTransition::guard()` accessor | +4 |
| `.../provider_dispatch/owner_input_start_wait.rs`(108줄) | `observe`: Codex 조기 반환(:67-76)을 'Bound 행이 아니거나 Unknown이 아니면 반환'으로 좁히고, Bound+보류면 두 provider 모두 `dead_bound::recover(.., LeaseRef::Acquire)`. 새 `Observed::Reject(String)`. watch probe 생성 함수(사유 변화만 info) | +55 |
| `.../provider_dispatch/owner_input.rs`(1795줄, giant) | 호출 교체만: `start_waits`의 Watch closure 블록(:794-803)을 `start_wait::probe(..)` 호출로 바꾸고, `Observed::Reject → Wait::Reject`, `start_idle`에 `Wait::Reject` arm(claim 뒤 `Failure::claimed(..).fail(..)`) | +12/−9 |
| `.../provider_dispatch/start_failure.rs`(579줄) | 안내·outbox 블록을 `notify`로 떼고 `fail` 추가(5.1 R) | +32/−12 |
| `src/services/turn_orchestrator/queued_head/v2/control.rs`(416줄) | `take_claimed` 공유 + `fail_claimed_locally` | +16/−6 |
| `src/services/herdr_launch/retire.rs` | MISSING bookkeeping을 'scope==Missing'에서 'reading==Missing'으로(`recovered_missing` 의미 유지) | +2/−2 |
| lock 밖 token 등록 경로(5.1 L 감사 결과) | 감사에서 lock도 없고 죽은 Herdr에 token을 걸 수 있는 호출이 나오면 lease 추가 | 0~+30(감사 결과에 따름) |

추정: prod 9파일(신규 1 + 수정 8), 순 코드 증가 약 **+190**(+310/−119). 감사 결과에 따라 최대 11파일/+220. 상한 30파일/+1500, 정지선 +1200 안이다.
giant 규칙: intake_turn.rs(인자 1개)와 owner_input.rs(closure 블록 이동·arm 호출)에는 호출 교체만 넣는다. 본체는 `dead_bound.rs`·`owner_input_start_wait.rs`·`start_failure.rs`·`control.rs`에 둔다.
시험 추정: +600 안팎(새 `owner_input_start_wait_tests.rs`, `owned_busy/dead_bound_tests.rs`, control·start_failure 단위 시험).

### 5.3 수용 시험(수정 전 RED → 수정 후 GREEN)

| # | 시험 | 수정 전 | 수정 후 |
|---|---|---|---|
| T1c / T1x | Claude / Codex. Bound + pane Missing + HttpStart(wake-watch context) + 무턴. Codex는 `hosted_active_or_unknown`에서 출발 | `Err(synthetic_waits_idle)`, front held, 행 Bound (RED) | Retired CAS 1, 새 execution nonce, Cold 전달 1 |
| T2c / T2x | 위와 같되 lone root shell | RED | GREEN(Scope::Ended) |
| T3 | Claude·Codex 살아 있는 idle 실행 | 전달 | 전달, `retire_pg` 0, 행 Bound |
| T4c / T4x | 살아 있으나 판독 불가(Claude output not ready, Codex `Activity::Unknown` + provider ExactAlive) | watch | watch, retire 0 → 판독 회복 뒤 전달 |
| T5 | 죽음 + 처음부터 active token(managed) | watch | Reobserve, retire 0 → TurnEnd wake 뒤 retire → Cold |
| T6 | 죽음 + non-Managed inflight 행 | Event | Event, retire 0 → row end wake 뒤 전달 |
| T7 | 소유자 불일치 + `origin_id`가 있는 HttpStart, 같은 조건의 Decision | 영구 watch (RED) | provider write 0, 큐 0, 안내 또는 outbox 1. owed receipt 상태·PG 반영 뒤·재시작 뒤 모두 `terminal.outcome=="failed"`. Decision은 Delivered가 아니다. 반환 `NotSent("hosted_recovery_refused")` |
| T8 | tmux busy → Event → (a) native idle, (b) pane 사망 → watcher 종료 → binding 제거 → unbound Idle → WakeOwner | 전달 | 전달. ④ 근거 고정 |
| T9c / T9x | Claude / Codex. 판독 불가로 watch 중 실행 소멸 → 다음 probe에서 retire → Ready wake | RED | GREEN |
| T11 (barrier) | ② 무턴 판독과 ⑤ CAS 사이 barrier. (a) barrier 전에 headless claim이 lock을 쥐고 token 등록 → ①이 Busy → CAS 0. (b) helper가 lease를 먼저 얻음 → `claim_reserved_headless_turn`이 CAS와 lease 해제 뒤에야 token을 등록하고, 그 start는 Retired 행을 읽어 Cold로 간다. (c) Borrow 경로는 자기 guard에 막히지 않는다 | (설계 1 계약에는 lease가 없음) | (a) CAS 0 (b) 순서 고정 (c) 통과 |
| T12 | inflight 판독 오류 주입(`Err`) | (설계 1: `.ok().flatten()`이라 retire) | CAS 0, hosted 행·inflight 파일 보존, 판독 회복 뒤 다음 probe에서 retire |
| T13 | 증명 불가 상태로 probe 여러 번 | — | DB 쓰기 0, 사용자 알림 0, 조회 횟수 계측(probe당 root_replaced 3회 + pane 판독), 같은 사유 info 로그 1회 |
| T10 | 사람 입력(#8212) 회귀: (a) Missing → 기존처럼 retire·Cold (b) lone root shell → Reobserve에서 retire·Cold로 바뀜 (c) borrowed lease로 자기 transition 안에서 통과 (d) inflight 행·판독 오류면 Reobserve (e) Refused면 Failed 정산 | (a) GREEN (b)~(e) 지금과 다름 | 모두 GREEN |

뮤턴트(구현 시험에서 RED를 확인할 것): Codex 조기 반환 복원, owner provider를 `"claude"`로 고정, lease 제거, Borrow 대신 무조건 `try_acquire`, `.ok().flatten()` 복원, 거절 outcome을 Completed로 되돌림.

### 5.4 비용과 남는 위험

- probe 비용(리뷰 P2-2 정정): Held가 남은 동안 probe(2초)마다 다음을 한다. `turn_host::for_observation` 판독, `root_replaced::current`의 DB 조회 3회(canonical hosted 행, canonical session row/key resolve, session key로 hosted 행 재조회, root_replaced.rs:285-306), `retire::observed`의 pane 판독(snapshot + foreground process, 상태에 따라 provider 증명 추가, herdr_gate.rs:836-908). 채널 하나에 분당 약 30회 probe, 이 세 조회만 분당 약 90회다. CAS와 host 재판정은 retire가 성공할 때만 더해진다. 개인 운영 규모에서 문제가 될 근거는 없어 캐시·backoff는 넣지 않는다. 사유가 바뀔 때만 info를 남긴다.
- lease를 쥔 시간 동안 headless claim이 최대 3초 기다린다. ①~⑦에 close·네트워크 대기가 없으니 이 창은 pane 판독 시간과 같다. 3초를 넘기면 headless claim은 Conflict로 실패한다(기존 동작).
- lock 밖 token 등록 경로는 구현 레그의 감사로 닫는다(5.1 L). 감사 전까지는 이 창이 열려 있을 수 있다고 본다.
- 사람 입력 경로의 동작 변화: 증명 범위 Missing→Ended, transition guard borrow, Refused 정산 Completed→Failed. 모두 T10으로 고정한다.

### 5.5 '대형 가드레일·운영자 수동 기능' 해당 여부

해당하지 않는다. 새 운영자 명령·설정·감시 루프를 만들지 않는다. #8212에 이미 있는 자동 retire를 synthetic 경로와 Codex가 같은 함수로 쓰게 한다. 기존 transition lease와 실패 정산 경계를 재사용할 뿐이다.

## 6. 상태 차트 요약(rev2)

모델에 넣은 축: provider(Claude·Codex), origin(synthetic·person), 실행 소멸 두 꼴(missing·shell), 판독→CAS 분리(`retiring` 단계)와 그 사이 다른 start의 token 등록(`TurnStart`, transition lock을 먼저 얻어야 함), inflight 판독 오류, 거절 outcome.

| 설정 | 구분 | 결과 |
|---|---|---|
| Before / BeforeShell / BeforeCodex / BeforeForeign | 수정 전 synthetic | `SyntheticSettles`·`OwnedStartDelivered` RED |
| BeforePersonShell | 수정 전 사람(#8212 Missing만) | 같은 두 규칙 RED |
| BeforePersonMissing | 수정 전 사람 Missing | GREEN(#8212 회귀 기준) |
| Fixed / FixedShell / FixedCodex / FixedCodexShell / FixedForeign / FixedPersonShell / FixedPersonMissing | 수정 후 | GREEN |
| FixedCodexLive / FixedCodexUnreadable / FixedLive / FixedBusy / FixedUnreadable | 살아 있거나 판독 불가 | GREEN(retire 0) |
| FixedManagedTurn / FixedExternalTurn / FixedInflightUnreadable | 턴·inflight·판독 오류 | GREEN(retire 거절 뒤 전달) |
| FixedRace / FixedRaceLive / FixedPersonRace | 판독→CAS 사이 token 등록 경합 | GREEN |
| CodexEarlyReturn | 뮤턴트 | 두 liveness RED |
| OwnerClaudeConst | 뮤턴트 | `OwnedStartDelivered` RED(Codex를 거절함) |
| NoLease | 뮤턴트 | `NoAutoRetireUnderTurn` 깨짐 |
| NoBorrow | 뮤턴트(사람 경로가 자기 lock에 막힘) | 두 liveness RED |
| RejectAsCompleted | 뮤턴트 | `RejectedIsFailed` 깨짐 |
| OkFlatten | 뮤턴트 | `NoAutoRetireUnderTurn` 깨짐 |
| RetireOnHeld / RetireUnderTurn | 뮤턴트 | `NoAutoRetireLive` / `NoAutoRetireUnderTurn` 깨짐 |
| GoneNoRetire | 뮤턴트(① 단독) | 두 liveness RED |
| TmuxBusy / TmuxDead / TmuxNoPresenceWake | tmux | GREEN / GREEN / `NoDeadlock`·두 liveness |

수정 전 RED 5(Before, BeforeShell, BeforeCodex, BeforeForeign, BeforePersonShell) → 수정 후 GREEN 5(Fixed, FixedShell, FixedCodex, FixedForeign, FixedPersonShell).
설계 1의 빈틈(P1 3·P2 1)은 뮤턴트 `CodexEarlyReturn`·`OwnerClaudeConst`·`NoLease`·`RejectAsCompleted`·`OkFlatten`이 RED로 보인다.
36개 설정 모두 `scripts/ci/statecharts_tlc.py`의 기대와 일치한다(PASS 36/36).

## 7. rev1 → rev2 리뷰 대응

| 리뷰 | 대응 | 위치 |
|---|---|---|
| P1-1 Codex가 공통 retire에 도달하지 않음 | observe가 두 provider 모두 recovery로 잇고, owner는 요청 provider와 비교. T1·T2·T4·T9 provider별 | 1절 4단계, 5.1 P, 5.2, 5.3 |
| P1-2 무턴 판독과 CAS 사이 token 등록 창 | transition lease 계약(Acquire/Borrow, ②~⑤ 한 lease), held_watch 순서 설명, lock 밖 등록 경로 감사, T11 barrier | 4절, 5.1 L, 5.3 T11 |
| P1-3 거절을 Completed로 정산 | `fail_claimed_locally` + `Failure::fail`, `NotSent` 반환, 사람 Refused도 같이. T7 확장 | 5.1 R, 5.2, 5.3 T7·T10e |
| P2-1 inflight 판독 fail-open | `_result` API, `Ok(None)`만 통과. T12 | 5.1 T |
| P2-2 비용 과소 기재 | 실제 경로로 정정, 사유 변화만 info. T13 | 5.1 C, 5.4 |
| 사람 입력 영향 | T10 (a)~(e) | 3절, 5.3 |

## 8. 구현 반영(impl-w1, S3b-1 w4 `4e9dd9e38d` merge 위)

### 8.1 계약 L 보강: 증명 → 무턴 판독 → CAS
`retire::observed`를 `prove`(판독만)와 `on_reading`(CAS)으로 나눴다. 공통 recovery는 lease 아래에서 ① `root_replaced::current` → ② `prove` → ③ token·inflight `_result` → ④ `on_reading` 순서로 간다.
이 순서에서는 살아 있던 provider가 남긴 증거로 만들어진 token·inflight가 ③에서 이미 보인다. 증명 뒤에는 그런 증거가 새로 생길 수 없다.

### 8.2 등록 경로 감사(리뷰 xh P2-1, 완료 조건)
token 등록(`mailbox_try_start_turn*`, `try_start_turn_with_stale_busy_heal`, `restore_active_turn*`)과 inflight 신규 생성(`save_inflight_state*`)의 prod 호출자 전수:

| # | 경로 | 만드는 것 | transition lock | Herdr 죽은 Bound에서 도달 | 처리 |
|---|---|---|---|---|---|
| 1 | router/turn_start.rs:142-179 `claim_reserved_headless_turn` | token | `acquire`(:151) | 예 | 이미 배제 |
| 2 | intake_turn.rs:663 → 1011 사람 intake(`try_start_turn_with_stale_busy_heal`, message_handler.rs:110-150) | token | intake guard | 예 | 이미 배제, recovery는 borrow |
| 3 | tui_prompt_relay/synthetic_start/claim/admitted.rs:279 → tui_direct_pending_start/state.rs:403, bridge_handoff.rs `capture_dormant` | inflight·token | AdmissionFence lease(admission_fence.rs:40-44) | 예 | 이미 배제 |
| 4 | recovery_engine/runtime.rs:189-386 `reregister_active_turn_from_inflight(_under_episode_guard)` | token | 없음 | caller별: (a) 부팅 inflight 복원 restore_inflight.rs:793·1962(`restore_inflight_states`, 아래 #11의 부팅 barrier 안), (b) 운영자 rebind의 episode handoff manual_rebind/episode_handoff.rs:97(`LockedInflightEpisode`를 쥐고, 살아 있는 tmux 세션 확인이 먼저), (c) runtime.rs:885는 시험 | 모두 기존 inflight 행을 전제로 하므로 ③ inflight 판독이 먼저 막는다 |
| 5 | provider_isolation.rs:278 `save_intake_inflight` | inflight | (#2 뒤) | token 등록 뒤 | ③ token 판독이 먼저 막음 |
| 6 | foreign_row.rs:47, headless_turn/replay_start.rs:73, headless_turn.rs:1015 | inflight | (#1 뒤) | token 등록 뒤 | ③ token 판독이 먼저 막음 |
| 7 | tmux_watcher/liveness.rs:473-529 `reacquire_watcher_inflight_for_active_stream`(호출 streaming_status_tick.rs:282, loop_poll_prologue.rs:436) | inflight 단독 | 없었음 | **아니오**(impl-w2 정정): 두 호출자 모두 `watcher_pane_actively_streaming`(liveness.rs:386-388)을 요구하고, 이 함수는 `HostSnapshot::tmux_only`(watch_host.rs:99-105)가 Herdr에서 `None`이라 거짓이다. 시험 `a_herdr_snapshot_never_reads_a_streaming_pane_so_no_reacquire_runs` | 보수적 보호로 lease 유지(`reacquire_under_transition`, `try_acquire`) |
| 8 | tmux.rs:614-680 `start_monitor_auto_turn_when_available`(+ monitor_auto_turn_inflight.rs:54) | token(+ 뒤따르는 inflight) | 없었음 | 예(같은 watcher) | **lease 적용**: headless claim처럼 `acquire`(최대 3초), 실패는 기존 deferred 경로 |
| 9 | recovery_engine/manual_rebind/mod.rs:770 | inflight | 없음 | 아니오: 운영자 `/rebind`, 살아 있는 tmux 세션 확인이 먼저 | 근거만 |
| 10 | turn_orchestrator.rs:799 `restore_active_turn_kinded` | (정의 wrapper) | — | — | 해당 없음 |
| 11 | recovery_engine/restore_inflight.rs:2248 → queue_io/turn_admission.rs:219-245 `mailbox_recovery_kickoff` | token | 없음 | 부팅 inflight 복원 안에서만 돈다 | 부팅 barrier: 복원 완료(`reconcile_done`) 전에는 ingress가 `Hold::RestartRecovery`(input_owner/ingress.rs:191-198)이고 `start_waits`도 `boot_ready` 전에는 Event(owner_input.rs:771-774)라 recovery와 겹치지 않는다. 완료 뒤 owner wake. 기존 시험 `owner_ingress_tests::automatic_ingress_and_handoff_keep_the_first_hold_reason`, `owner_input_idle_tests::active_reconcile_completion_resumes_the_busy_owner_once` |

시험 전용 호출(분리): outbound/o_writer_io/agy.rs:184·199, tmux_reaper.rs:951·996·1085·1160·1210·1305·1476, turn_finalizer/cleanup/terminal_release.rs:182·205·264·284, tui_prompt_relay/bridge_completion.rs:121·135·150·205·281·360, tmux_watcher/placeholder_reclaim.rs:193, health/turn_deliver.rs:696(`seed_*_for_tests`), health/{recovery,watcher_respawn,snapshot}.rs의 시험 모듈, turn_finalizer/episode.rs:884, router/message_handler/turn_lifecycle.rs:242.
감사 경로 11, lease 적용 2(#7 보수적 보호, #8). 상태 차트의 `InflightStart`(watcher의 inflight 단독 생성)와 뮤턴트 `inflightNoLease`는 보수적 경합 모델이다. 실제 Herdr #7 경로의 도달 증거가 아니다.

### 8.3 리뷰 xh P2-2: 사람 입력의 Ended 확장과 Missing watch
`owned_busy/missing_watch.rs`는 이제 Missing만이 아니라 CLI와 같은 `view.ended`(Missing 또는 기록된 root shell만 남음)를 읽는다. 무턴(token 없음)이고 inflight가 정확히 `Ok(None)`이면 Ready로 owner를 깨운다.
watch 자체는 retire하지 않는다. 깨어난 intake가 공통 recovery를 다시 탄다(lone root shell → inflight 판독 오류 → 회복 → wake → retire → Cold).

### 8.4 리뷰 xh P2-3: MISSING marker
CLI `retire::observed(Scope::Ended)`는 marker를 남기지 않는다(그대로). 부팅 `Scope::Missing`도 그대로다. 공통 recovery는 실제 판독이 Missing일 때만 `retire::note_missing`으로 남긴다(lone root shell은 남기지 않음).

### 8.5 Codex 시험 범위
Codex Herdr 시험 rig(`provider_dispatch_codex_herdr_tests.rs`)는 provider 실행 단계(`fx.turn`)용이다. owner 판독 단계(runtime root, 세션 키, session_hosts)가 구성되어 있지 않다.
이번 조각은 provider에 따라 갈리는 owner 비교를 순수 함수 `owner_differs`로 떼어 Claude·Codex 단위 시험으로 고정했다(뮤턴트 OwnerClaudeConst RED).
Codex 판독 경로의 조기 반환은 코드에서 없앴다(두 provider가 같은 `screen == Unknown && bound` 분기를 지남). T1x·T2x·T4x·T9x E2E는 Codex owner rig가 필요해 남는 일로 둔다.

## 9. impl-w2 리뷰 대응(WebGPT pro impl-1: P1 1·P2 3)

| 리뷰 | 대응 |
|---|---|
| P1-1 Codex 닫힌 rollout Idle이 죽은 Bound를 가림 | `owner_input_start_wait::observe`: Codex Bound의 Ready/Blocked에 `execution_alive()`(정확한 endpoint·실행, `claude_handoff::bound_herdr` → `HerdrTarget::execution_alive`, blocking이라 `spawn_blocking`)를 결합한다. 살아 있지 않거나 확인하지 못하면 Unknown으로 보고 공통 recovery(prove → no_turn → on_reading)로 간다. 판정은 `live_rollout` 순수 함수다. Claude는 busy 판독 `Context::read`(busy/herdr.rs:303-306)가 `execution_alive()`를 먼저 보고 Held를 내므로 같은 원칙이 이미 성립한다(변경 없음). 상태 차트: 물리 실행(exec)과 rollout을 별도 축으로, 죽은 Bound로의 followup은 start 실패(Failed)로, 불변식 `DeliveredNeedsExecution` |
| P2-1 감사표 #7·kickoff·#4 | 8.2 표의 #7을 정정(tmux_only 배제, Herdr 미도달)하고 #4 caller별 근거, #11 kickoff 부팅 barrier를 넣었다. lease 코드는 유지한다 |
| P2-2 T11b 실제 경쟁자 | barrier hook 안에서 실제 `start_monitor_auto_turn_when_available`(tmux.rs, cfg(test) 래퍼 `monitor_start_for_test`)를 띄우고, lease 안에서 token이 생기지 않음(`poll_within` 500ms 창)과 lease 해제 뒤 등록됨을 확인한다. tmux.rs의 실제 `acquire`를 지우면 창 안에서 token이 생겨 RED |
| P2-3 직접 timeout | 새 시험의 직접 `tokio::time::timeout` 8곳을 `await_within`/`poll_within`으로 바꿨다 |

Codex owner 단계 E2E(닫힌 rollout Idle을 먼저 확인한 뒤 물리 상태만 바꾸고 실제 wake-watch HttpStart, retire CAS 1·new nonce·Cold 1·old pane write 0·retained 0)는 이 저장소에 Codex Herdr owner rig가 없어 작성하지 않았다. 대신 판정 함수 시험과 HerdrRig 위의 생존 판독 시험으로 고정했다.
