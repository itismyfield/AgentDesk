# #8422 설계 — 죽은 Bound 실행이 있는 Herdr 채널에서 synthetic 입력이 claim 전에 영구 보류된다

- 코드 기준: `82b96fa94b`(#8399 S3b-1 w3. `refs/review/i8399-s3b1-w4`는 아직 없다). 아래 file:line은 모두 이 SHA에서 다시 확인했다.
- 상태 차트: [`HerdrSyntheticStart.tla`](../statecharts/input-single-path/HerdrSyntheticStart.tla), [결과 보고](../statecharts/input-single-path/herdr-synthetic-start.report.md).
- 구현은 이 문서에 넣지 않았다. 설계만 담는다.

## 0. 한 줄 결론

synthetic start의 claim 전 판독(`start_wait::observe`)이 #8212가 사람 입력에 쓰는 '죽은 Bound 판정·retire'를 **같은 공통 함수로** 묻게 한다.
증명은 CLI `herdr retire`와 같은 `retire::observed(Scope::Ended)`를 쓴다.
retire가 되면 같은 판독이 Ready를 내고, start는 claim한 뒤 새 실행(Cold)으로 전달된다.
끝내 retire할 수 없는 이유(소유자·endpoint 불일치, 행 판독 불가)가 나오면 이유를 남기고 거절·정산한다.
turn이 잡고 있는 채널(active cancel token, inflight 행)에서는 retire를 거절하고, 그 턴 경계의 wake를 기다린다.

## 1. 정체 경로(82b96fa94b)

1. `admit_start`(owner_input.rs:713) → `start::admit_synthetic` → drive → `start_idle`(owner_input.rs:943).
2. `start_idle`은 `!joins_running_turn()`인 출처(RestartSeed 제외)를 `start_waits`로 보낸다(owner_input.rs:963-971).
3. `start_waits`(owner_input.rs:770): boot·inflight·rest 검사를 지난 뒤 `start_wait::observe`(owner_input_start_wait.rs:33)를 부른다.
4. `observe`: Bound 행이면 `claude_handoff::observe_busy`(claude_handoff.rs:569) → `busy::observe_herdr` → `Context::read`(busy/herdr.rs:303-306) `!execution_alive() → Reading::held()`.
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
| HttpStart(wake-watch 재개·`agents/turn/start`) | headless_turn/admission.rs:111 → owner_input.rs:713, context `origin:"headless"`(start.rs:95) | 탄다. `keyed_to_channel=Some(true)`(owner_input_start_wait.rs:24-30) | **주 대상**: 죽은 Bound를 retire하고 Cold로 전달 |
| Routine | routines/agent_executor/owner_input.rs, scheduled_messages/owner_input.rs | 탄다. 채널 키 여부는 `owner_start_takes_channel` | 채널 키면 같은 수리, 아니면 Legacy(변화 없음) |
| EffortContinue | v2_types/effort_restart.rs:64 | `start_idle`에서는 탄다(owner_drive가 handoff만 건너뛴다, owner_input.rs:235-237) | 같은 수리 |
| Decision | decision_drain | 탄다(`is_person`이지만 `joins_running_turn=false`, origin.rs:36-38) | 같은 수리 |
| RestartSeed | restart_seed.rs:95 | 안 탄다(owner_input.rs:964-967 `Wait::Go`, greeting permit이 판정) | 없음. 재시작 뒤 Bound는 S3b-2 범위 |
| Discord·iMessage(사람) | intake | 안 탄다(`joins_running_turn`, Wait::Go) → claim 뒤 `OwnerTurn::early` | 공통 함수로 바뀐 #8212 경로. 증명 범위가 Missing→Ended로 넓어진다(5.3) |

## 4. 재시작·부팅·incarnation과 인접 소비 경로

- 부팅 재연결(herdr_reader.rs:440-470): Missing만 retire(B3), MissingMarker·RootReplaced는 각자 회수. lone root shell은 Bound로 남는다. 이 설계는 부팅 경로를 바꾸지 않는다. 다음 synthetic·사람 입력이 그 채널을 열 때 공통 함수가 정리한다.
- `boot_ready` gate(A1)와 restart gate·writer boot hold(H1~H3): 재시작 중 hold는 bootstrap probe와 boot ready wake로 풀린다. 수리는 이 앞단을 건드리지 않는다. 공통 함수는 `start_waits`의 boot 검사를 지난 뒤에만 불린다.
- incarnation: `recovered_missing`의 MISSING 목록(retire.rs:107-121)과 `withhold_herdr_execution`은 프로세스 메모리다. 새 프로세스는 DB 행만 다시 읽는다. 공통 함수의 CAS는 nonce·owner를 같이 보므로(retire.rs:93) 다른 incarnation이 이미 retire·relaunch한 행은 Stale로 거절된다(Retry → watch).
- S3b-1 `pending_recovery`(B9): Pending만 받는다. 이 설계는 그 계약을 바꾸지 않는다(Bound는 계속 받지 않는다).
- Cancel(HTTP force B8)·Terminate(B5·B6)·clear(B7): `capture_turn`으로 턴을 잡고 retire하고 RetiredWake로 깨운다. 공통 함수는 이 기계를 부르지 않는다. 다만 같은 행을 두고 경합할 때는 둘 다 nonce CAS(`retire_pg`)를 쓰므로 한쪽만 Written이 된다. 공통 함수 쪽이 Stale이면 Retry로 watch에 남고, B5의 RetiredWake가 깨운다.
- stop(`/stop`): 활성 턴의 cancel token을 끊는다. hosted 행을 바꾸지 않는다. 턴이 끝나면 turn 경계 wake(C4·C6)가 front를 다시 판정한다.
- turn_presence `unbound/herdr_bound.rs`(로그 'unbound hold leaf hosted_active_or_unknown'): 표시용 판독이다. 행을 바꾸지 않는다.

## 5. 수리안 비교

### 안 A(권함) — 공통 '죽은 Bound retire'를 claim 전 판독이 묻는다(코디 ②+#8212 공통화, ①·③ 없음)

- 흐름: `observe`가 Bound 행에서 Held를 읽으면 공통 함수 `dead_bound::retire`를 부른다.
  Retired면 행을 다시 읽어 Ready → `Wait::Go` → claim → `read_as`의 `bound_herdr`가 None → `Phase::Absent` → 새 실행 launch(Cold).
  Reobserve·Retry(증명 안 됨, CAS Stale, endpoint 없음)면 지금처럼 Unknown → watch. watch probe가 `observe`를 다시 부르므로 매 판독에서 retire를 다시 묻는다.
  Refused(소유자·endpoint 불일치, 행 판독 불가)면 이유를 담아 거절·정산한다.
- retire를 같은 드라이브 판정 안에서 하므로 깨울 것이 필요 없다. watch에서 retire되면 다음 판독이 Ready → C1이 한 번 깨운다.
- 비용: 새 child 1개(owned_busy.rs:220-301 블록을 옮김), observe +30, start_idle에 거절 arm 1개, retire.rs bookkeeping 몇 줄.
- 위험: 살아 있는 실행 오판 → `view.ended` 증명(CLI와 같음)만 받는다. 턴과 경합 → active token·inflight 행 검사를 공통 함수 안에서 CAS 직전에 다시 본다.

### 안 B — 판독이 Gone을 내고 S3b-1 terminate 기계로 끝낸다(코디 ①+② 문자 그대로)

- 흐름: busy/herdr.rs:304에서 `provider_absent()==Ok(true) && !execution_alive()`를 Gone으로 내고, `start_waits`가 Gone이면 `terminate_scoped`(B5)를 Bound용 요청으로 부른 뒤 RetiredWake로 깨운다.
- 문제: (1) held_watch는 Gone을 wake 없이 끝낸다(C2). 모델 `GoneNoRetire` 변이가 RED로 이를 보인다. retire가 실패하면 깨울 것이 없다. (2) S3b-1 `NeverStartedTerminate::admits`는 "a Bound one never is"가 계약이다. Bound용 자동 terminate를 넣으면 그 계약을 바꾼다(DESIGN_DELTA 대상). (3) 이미 죽은 실행에 close를 보내는 단계가 더해져 Indeterminate 결과가 늘어난다. (4) `provider_absent`는 기록된 provider 프로세스만 본다. 기록된 root shell 대조가 없어 CLI 증명보다 약하다.
- 비용: busy/herdr.rs·start_waits·herdr_terminate.rs·never_started.rs 계열 수정. A보다 크다.

**권함: A.** S3b-1 계약은 그대로 두고 그 위에 쌓는다. retire 판정은 #8212·CLI와 같은 하나의 증명을 쓴다.
③(owner_input.rs:101 alive를 execution_alive로)은 하지 않는다. retire가 claim 전에 끝나 `read_as`가 이미 Absent를 고른다. 거꾸로 Bound 행을 둔 채 Absent로 launch하면 launch CAS와 충돌한다.
④(tmux `Wait::Event`)는 해당 없음이다. 근거: busy→idle에서 turn_presence가 owned 채널에 WakeOwner를 낸다(supervisor.rs:691-701). 죽은 pane은 `native_wait`이 Gone→Go로 보낸다(owner_input.rs:866-871). 모델 `TmuxBusy`·`TmuxDead` GREEN, `TmuxNoPresenceWake` 변이 RED. 남는 점: Event 대기 중 tmux pane이 죽는 경우는 모델에 넣지 않았다. 구현 시험 T8에서 확인한다.

### 5.1 수정 지점 전수표(안 A)

| 파일 | 변경 | 추정 |
|---|---|---|
| `src/services/discord/router/message_handler/intake_turn/owned_busy/dead_bound.rs`(새 child) | `pub(in crate::services::discord) async fn retire(shared, (provider, channel, key, logical), host, mailbox) -> Result<(), DeadBound>`. owned_busy.rs:220-301 블록(소유자·endpoint 9항, `root_replaced::current`, `retire::observed`, hold 해제, `withhold_herdr_execution`, 재판정)을 옮긴다. 더하는 것: CAS 직전 `mailbox.has_active_turn()==Ok(false)`와 `load_inflight_state_read_only(provider, channel).is_none()`. 하나라도 거짓이거나 판독 실패면 `Reobserve("turn holds the channel")`. Scope는 `Ended` | +125(이동 85 포함) |
| `.../intake_turn/owned_busy.rs`(813줄) | `recover_or_discard`의 recovery 블록을 `dead_bound::retire` 호출로 바꾼다. 오류 분류(Refused/Reobserve/Retry)는 공통 enum으로. `mod dead_bound;` 1줄(FILES_AUTO) | −85/+12 |
| `.../provider_dispatch/owner_input_start_wait.rs`(108줄) | `observe`: Bound + `observation.state()==Held`면 `dead_bound::retire` 호출. Ok → `Native(Ready)`, Refused(why) → 새 `Observed::Reject(why)`, 그 밖은 `Native(Unknown)`. `native()`는 Reject를 None으로 → watch probe의 `unwrap_or(Ready)`가 한 번 깨운다 | +32 |
| `.../provider_dispatch/owner_input.rs`(1795줄, giant) | 호출 교체만: `start_waits`에 `Observed::Reject(why) => Wait::Reject(why)` arm, `start_idle`에 `Wait::Reject` arm 1개(claim 뒤 `start_failure::Failure::claimed(..).discard(reason)`, owned_busy.rs:304-313와 같은 정산. 본체는 start_wait child의 `reject` helper) | +10 |
| `src/services/herdr_launch/retire.rs` | MISSING bookkeeping을 'scope==Missing'에서 'reading==Missing'으로(`recovered_missing` 의미 유지) | +2/−2 |
| 시험(새) `owner_input_start_wait_tests.rs`, `owned_busy/dead_bound_tests.rs` | 5.2 표 | 시험 +350 안팎 |

추정: prod 5파일(+새 child 1 = 수정 4 + 신규 1), 순 코드 증가 약 +95(+181/−87). 상한 30파일/+1500, 정지선 +1200 안에 넉넉히 든다.
giant 규칙: owner_input.rs에는 arm 두 개(호출)만 넣는다. 본체는 child(`dead_bound.rs`, `owner_input_start_wait.rs`)에 둔다.

### 5.2 수용 시험(수정 전 RED → 수정 후 GREEN)

| # | 시험 | 수정 전 | 수정 후 |
|---|---|---|---|
| T1 | Herdr Bound + 실행 죽음(pane Missing) + HttpStart(wake-watch context) + inflight 없음 → `start_idle_for_test` | `Err(synthetic_waits_idle)`, front held, 행 Bound (RED) | 행 Retired(CAS 1회), claim, 새 실행 launch(Cold), 전달 1회 |
| T2 | T1과 같되 lone root shell(provider 끝, pane 남음) | RED | GREEN(Scope::Ended 증명) |
| T3 | Bound + 살아 있는 idle 실행 | 전달(GREEN) | 전달, 행 Bound 그대로, `retire_pg` 호출 0 |
| T4 | Bound + 살아 있으나 판독 불가(output not ready) | watch | watch, 행 Bound(안전) → 판독 회복 뒤 전달 |
| T5 | Bound + 죽음 + active cancel token(managed 턴) | watch | retire 거절·watch 유지 → TurnEnd wake 뒤 retire → Cold 전달 |
| T6 | Bound + 죽음 + non-Managed inflight 행 | Event | Event 그대로(retire 0) → row end wake 뒤 전달 |
| T7 | Bound + 소유자 불일치(token hash 다름) | 영구 watch (RED) | 이유와 함께 거절·정산, 대기열 0 |
| T8 | tmux transcript_turns busy → Event → native idle → turn_presence WakeOwner | 전달(GREEN) | 전달(GREEN). ④ 근거 고정. Event 대기 중 pane 소멸도 함께 본다 |
| T9 | held_watch 판독 중 실행 소멸: Unknown watch → 다음 판독에서 retire → Ready wake | RED | GREEN |
| T10 | #8212 사람 입력 시험(기존) | GREEN | GREEN(공통 함수로 옮긴 뒤 회귀 없음). lone root shell 사람 입력은 Reobserve → retire로 바뀐다 |

### 5.3 남는 위험

- 사람 입력 경로가 Scope::Missing에서 Ended로 넓어진다. 증명은 CLI와 같고, 운영에서 사람이 수동으로 쓰던 판정을 자동화하는 것이다.
- watch probe(2초)가 판독마다 retire를 다시 묻는다. 증명이 안 되면 읽기만 하고 쓰지 않는다. PG 읽기 한 번과 pane 판독이 늘어난다.
- `has_active_turn`과 inflight 판독 뒤 CAS 전에 새 턴이 시작될 수 있다. 새 턴은 Bound 행을 launch하거나 attach해야 하는데, 그 실행은 증명으로 이미 죽었다. 같은 nonce에 attach하면 gate가 거절하고, 새 launch는 nonce가 달라 CAS가 Stale이 된다. 남는 창은 구현 리뷰에서 다시 본다.

### 5.4 '대형 가드레일·운영자 수동 기능' 해당 여부

해당하지 않는다. 새 운영자 명령·설정·감시 루프를 만들지 않는다. 사람 입력 경로(#8212)에 이미 있는 자동 retire를 synthetic 경로가 같은 함수로 쓰게 하는 것뿐이다.

## 6. 상태 차트 요약

| 설정 | 수정 전/후 | 결과 |
|---|---|---|
| `Before` | 수정 전 | `SyntheticSettles` RED(죽은 Bound watch에서 영원히 Keep) |
| `BeforeForeign` | 수정 전 | `SyntheticSettles` RED |
| `Fixed` | 수정 후 | GREEN(retire → Cold 전달) |
| `FixedForeign` | 수정 후 | GREEN(거절·정산) |
| `FixedLive`·`FixedBusy`·`FixedUnreadable`·`FixedManagedTurn`·`FixedExternalTurn` | 수정 후 | GREEN(안전 불변식 유지) |
| `RetireOnHeld`(③식 증명 없는 retire) | 변이 | `NoAutoRetireLive` 깨짐 |
| `RetireUnderTurn`(턴 검사 없음) | 변이 | `NoAutoRetireUnderTurn` 깨짐 |
| `GoneNoRetire`(① 단독) | 변이 | `SyntheticSettles` RED |
| `TmuxBusy`·`TmuxDead` | 현행 | GREEN(④ 해당 없음) |
| `TmuxNoPresenceWake` | 변이 | `SyntheticSettles`·`NoDeadlock` 깨짐 |

RED 2 → GREEN 2. 15개 설정 모두 `scripts/ci/statecharts_tlc.py`의 기대와 일치한다(PASS 15/15).
