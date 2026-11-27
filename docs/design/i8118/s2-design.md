# #8118 S2 설계 w2 — Herdr 부모 채널 아래 스레드 세션의 배치·활성화

- 본문은 WebGPT pro 설계 w1(`design-s2-1`, VERDICT: DESIGN_READY)이고, 이 문서에 옮겨 실었다. 그 위에 독립 설계 리뷰 `review-design-s2-1`(WebGPT xh, P0 0 · P1 8, VERDICT: ISSUES)을 반영해 w2로 고쳤다.
- 코드 기준은 `b52e052e24e13781569050ec940eb35b9496ed54`다(prod는 base `b04ea56650`과 같다). `origin/main` `c28732c52c`는 아래 좌표 파일을 바꾸지 않았다.
- 경로 표기: `src/services/` 아래는 접두를 생략한다. 그 밖의 경로는 `src/`부터 적는다. 새로 만들 파일은 "신규"로 표시한다.
- 리뷰의 P1 8건은 모두 코드로 확인했고(반증 0), 개선 방향을 채택했다. 그중 P1-3·P1-4·P1-6은 범위나 조각 배정을 고쳐서 채택했다.
- w2가 바꾼 곳은 맨 앞 `## w2 변경` 표에 모았고, 본문 해당 자리에는 `[w2-P1-n]` 표시를 달았다. 표시가 없는 본문은 w1 그대로다.

<!-- doc-path-refs: proposed src/services/session_host/thread_launch.rs -->
<!-- doc-path-refs: proposed src/services/discord/recovery_engine/herdr_reader/thread_lifecycle.rs -->
<!-- doc-path-refs: proposed src/services/discord/herdr_terminate/idle.rs -->

## w2 변경

| P1 | 리뷰 지적 확인 | 바뀐 결정 | 코드 좌표(b52e052e24) | 걸리는 조각 | 추가 시험·뮤턴트 |
|---|---|---|---|---|---|
| 1 | **확인.** 콜드 Claude는 첫 프롬프트가 transcript를 만든 뒤 첫 레코드를 읽고 나서야 Bind한다. 자기 SessionStart를 확인한 attach도 `bound=false`로 성공할 수 있다 | Q5·C6·T-S2-1의 순서를 고친다: **정확히 검증한 placed Pending → 영속 출력 예약 + 원본 attempt → 첫 키 → transcript 첫 레코드 검증 → Bind·seed.** 첫 키와 Bind 사이에는 그 원본만 키를 쓸 수 있다. `Bound`는 첫 키의 전제가 아니다 | `claude/herdr_turn.rs:752-777`(`prompt_and_read`); `claude/herdr_turn/first_input.rs:534-601`(`attached.bound` false면 transcript 뒤 `bind_once_logged`); `discord/tui_prompt_relay/herdr_source.rs:177-190`(`bound = baseline.is_some() && bind_pg(..)`) | S2c | T-S2-1 개정: 첫 Enter 전 transcript 없음, 정상 시작과 첫 키 전 재시작 각각. 뮤턴트 Bound 선행 요구(모델 `MutW1BindFirst` → `FirstInputResolves` RED) |
| 2 | **확인.** exact retry는 `existing_retry`라 ingress가 owner를 건너뛰고, Claude driver는 `launch_hook` 없이 `first_input::submit`을 직접 부른다. owner hook에만 둔 예약은 우회된다 | 첫 키 전 출력 예약을 **공통 `first_input` write**(`submit`·`submit_owned`가 함께 지나는 자리)에서 강제한다. exact retry의 기존 receipt·미전송 정산을 그 예약 결과에 잇는다. 예약 실패는 키 0회, 원본·receipt 보존이다 | `discord/router/message_handler/intake_turn/worker_entry.rs:156-183`(`existing_retry(start)` `:182`); `discord/queue_io/input_owner/ingress.rs:407-417`(`EXISTING_RETRY` → `None`); `claude/herdr_turn.rs:764-773` | S2c | T-S2-19(신규): 실제 `start_exact_retry`에서 예약 실패 → 키 0·같은 receipt 보존, 성공 → 예약 뒤 제출 1회. 뮤턴트 직접 `submit` 가드 제거(모델 `MutW1RetrySkipsReserve` → `KeyAfterReservation` RED) |
| 3 | **확인.** implementation·review·rework 기본 전략은 `reset_provider_state=true`다. 새 자식이 Herdr로 판정되면 reset 가드가 거절하고, intake는 입력을 돌려준 뒤 `Executed`로 끝나 `for_turn`에 가지 않는다. I3 재사용 사전 판정은 counter-model provider 확정(`:788-797`) 전에 돈다 | I3 라우팅에서 **자식 ID·실행 provider·정규 owner를 먼저 고정**하고 원본 mailbox는 따로 유지한다. **확인된 vacant**(S2a 공통 관측의 확인된 실행 없음)에서 내부 provider reset은 fresh 의도로 처리하고 거절하지 않는다. 명시 tmux 재생성(`recreate_tmux`)과 live/미확인 대상은 기존 가드 또는 원본을 보존하는 typed 결과다. (수정 채택: vacant 판정은 S2a 공통 관측만 쓰고 Unknown은 Hold다) | `src/dispatch/dispatch_context.rs:205-212`; `discord/router/message_handler/intake_turn.rs:788-797,867-888`; `discord/admin_host_guard.rs:96-142`(`managed_reset_refusal` → `configured_refusal`); `intake_turn/adk_thread.rs:166-205` | S2c(관측 기반은 S2a-2) | T-S2-20(신규): 실제 I2/I3 기본 dispatch → tab 1·제출 1·tmux reset 0. 두 provider의 행이 다른 재사용 사례 포함. 뮤턴트 vacant reset 거절(모델 `MutW1ResetRefuses` → `NoInputRefused`·`FirstInputResolves` RED) |
| 4 | **확인.** `create_checked`는 소유 기록 실패·cwd 불일치·두 번째 fence 거절·command 전송 결과(확정 미전송 포함)를 모두 `CreatedUnconfirmed`로 합친다. launch는 위치만 기록하고 실행 증거(`expected`)가 없다. 주기 복구는 unplaced만 다루고 `NeverStartedTerminate`는 `expected`를 요구한다 | `launch_host`가 command의 **확정 미전송**과 **전송 불명**을 구분해 보존한다. 확정 미전송: exact nonce·marker·root를 확인하고 잔재를 닫은 뒤 retire하고, 같은 원본을 재개한다(Q3 fresh successor 증거로 쓴다). 전송 불명: 기존 pane의 증거를 다시 확인한다. (수정 채택: 기존 채널 workspace 경로에도 생산 호출자가 있는 결함이라 **S2b**로 당긴다. S2c tab 경로는 같은 구분을 소비한다) | `session_host/herdr/launch_host.rs:625-655`(`unconfirmed(format!("command input: {other:?}"))` `:654`); `herdr_launch.rs:608-650`(위치 기록); `claude/herdr_turn/pending.rs:15-35`; `discord/herdr_terminate/never_started.rs:23-25` | S2b | T-S2-21(신규): create ACK → command 미전송 → 재시작, 그리고 명령은 실행됐지만 응답이 유실된 대조군. 생존 tab·최초 제출 각 1회. 뮤턴트 두 결과 합치기 |
| 5 | **확인.** 사라진 pane retire와 root-replaced 정산은 relay가 시작한 rehydrate pass 안의 Herdr reconnect가 한다. w1 Q10은 relay spawn 앞에서 Bound 검증을 기다린다 | 부팅에서 **소비자 시작 전에 기존 종료 판정으로 끝난 행을 exact retire**하고, 남은 미확인은 그 owner에만 Hold를 게시한 뒤 소비자를 시작한다. 실제 Bound만 seed한다. 독립 부팅 복구를 새로 짓지 않고 기존 reconnect·종료 판정을 재사용한다 | `discord/runtime_bootstrap.rs:298-340`(native boot `:302` 뒤 relay spawn `:311`); `discord/tui_prompt_relay/rehydration.rs:270-274`; `discord/recovery_engine/herdr_reader.rs:273-304`(root replaced), `:436-470`(`retire::observed(.., Scope::Missing)`) | S2b | T-S2-9 개정: 옛 root 또는 사라진 pane을 가리키는 행과 정상 tmux 채널을 함께 부팅 → 정상 채널은 작업을 받고 미확인 자식에는 키·create 0. 뮤턴트 seed gate가 stale Bound를 기다림(모델 `MutW1BootGateBlocks` → `BootServesOthers` RED) |
| 6 | **확인.** slot reset은 Discord archive(`:390`)를 DB 정산(`:391`)·map 해제보다 먼저 한다. 등록 runtime 경로의 `managed_reset_verdict`(`:511-519`)가 Retired를 거절한다. 종료 엔진은 `retire_confirmed`를 `settle_and_release`보다 먼저 기록한다. dispatch 재사용 실패·중복 스레드 archive는 슬롯 판정을 우회한다 | 슬롯 해제를 **하나의 해제 판정**으로 묶는다: exact 실행의 종료 + 정산 완료(`settle_and_release` 뒤 증거) + slot map 전이. archive는 그 판정이 확보된 뒤에만 한다. Herdr 해제 경로는 managed tmux·mailbox reset을 부르지 않는다. dispatch 재사용 실패·중복 archive 경로도 같은 판정을 거친다. `Retired` 상태만으로 정산 완료를 추정하지 않는다 | `auto_queue/runtime.rs:387-445`(archive `:390`), `:498-526`(`managed_reset_verdict`); `discord/herdr_terminate.rs:617-642`(`retire_confirmed` `:623`, `settle_and_release` `:630`); `dispatches/discord_delivery/orchestration.rs:847-904` | S2b | T-S2-8 개정: 자동·수동 reset과 dispatch 재사용 각각에 successor 설치, Retired CAS 직후 정산 지연, Discord 조회 실패를 넣어 기존 map·출력 보존과 archive 0회. 뮤턴트 archive를 판정 앞에 둠 / Herdr 해제에서 managed reset 호출 |
| 7 | **확인.** restart는 mailbox를 가져오기만 하고, `Seed::of`는 V2 owner가 없으면 `"V2 not adopted"`로 실패한다. 기존 vacant 시험은 fixture에서 `install_owner`를 부른다. restart 경로에 `ingress::adopt` 호출이 없다 | restart gate 아래에서 **기존 local TUI `ingress::adopt`를 부른 뒤** 같은 deterministic seed를 접수한다 | `discord/commands/restart/herdr.rs:279-310`; `discord/queue_io/input_owner/restart_seed.rs:38-64`(`:56-58`); `discord/herdr_terminate/http_force_tests/restart_channel_tests.rs:838-866` | S2c | T-S2-22(신규): owner를 미리 설치하지 않은 rowless·FreshAfterKill 스레드에서 실제 `/restart`와 같은 명령 재생 → 원본·tab·제출 각 1. 뮤턴트 adopt 생략(모델 `MutW1RestartNoSeed` → `FirstInputResolves` RED). **T0 겹침**: 두 파일 모두 `refs/review/i8399-s3b2-w4` |
| 8 | **확인.** `JUDGED`는 `BTreeMap<u64, bool>`, 곧 채널당 bool 하나다. 같은 스레드에 counter-model Codex 턴이 들어와 Tmux를 기록하면, watcher 없는 Claude prompt 경로(`owner_channel_for_tmux_session`의 `runs_herdr`)와 notice fence가 Claude Bound를 놓친다 | 현재 실행 캐시와 seed·무효화를 **(channel, provider, 정규 owner)**로 구분하고 해당 nonce의 변경만 반영한다. E80·E81·E85·E86(필요하면 E82·E83)에는 이미 가진 provider·source identity를 넘긴다 | `session_host/thread_host.rs:87-116`; `discord/tui_prompt_relay.rs:726-795`(`:754`); `tui_o/writer/input_notices/admission.rs:131-144` | **S2a**(S2a-2) | T-S2-23(신규): 같은 채널의 두 provider 행을 두 순서로 seed하고, Codex의 Tmux 판정·retire 뒤에도 Claude prompt·notice fence가 유지된다. 뮤턴트 채널 단위 bool 복원 |

### 리뷰가 미확인으로 남긴 항목 → 시험 계획

| 항목 | 코드 확인 | 처리 | 조각 |
|---|---|---|---|
| rowless + live tmux + selector 없음 | **확인(결함).** 행이 없으면 probe 전에 `Observed::Nothing`(`host_decision.rs:253-255`). 활성화 뒤에는 Herdr로 간다 | S2a 공통 관측은 행이 없어도 정규 tmux 이름(provider·채널에서 도출)으로 probe한다. T-S2-3b: rowless + live tmux → Tmux, Herdr create 0 | S2a-2 |
| alias 키로만 남은 행 | 미확인 | T-S2-3c: alias 행 + live tmux → Tmux. alias 행 + tmux 없음 + selector 없음 → Herdr fresh | S2a-2 |
| probe 실패 | 확인(`host_decision.rs:288` → Unknown) | T-S2-3d: probe 실패 → Hold, 키·create 0 | S2a-2 |
| 정상 fresh dispatch의 O 소유권·수락·checkpoint 실성립 | 미확인 | T-S2-5에 실제 fresh dispatch 스레드의 `o_writer_ready` 성립을 생산 경계로 확인 | S2c |
| I11 legacy 이력 재큐의 generation·ticket | 미확인. I11은 원래 message ID로 별도 internal follow-up을 enqueue한다(`turn_bridge/recovery_text.rs:896-932`) | Q12의 owner attempt 증명을 I11에 그대로 쓰지 않는다. T-S2-13에 I11을 따로 넣어 원본 payload ≤ 1 | S2c |
| I20 외부 소비 | 이 SHA에서 dormant API(`src/lib.rs:339-348`) | 공통 headless 관계·판정 시험에 포함(w1 U4 유지) | S2c |

### 리뷰 P2(후속·보강, 계약 반영)

- Herdr → Herdr resume의 출력 보호: `claude_thread_writer:<parent>`는 `launch_mode=fresh`일 때만 보호 근거로 읽힌다(`tui_prompt_dedupe/binding_events/claude_fold.rs:24-62`). idle retire → native resume에서 첫 키 전 예약과 늦은 terminal의 source 보존 시험을 S2c에 둔다.
- 직접 Herdr 입력과 idle close의 경합: 직접 입력을 허용하는 대상은 조건부 close가 가능해질 때까지 자동 idle 종료에서 뺀다(S2b, C10 범위 명시).
- `/stop` 배포 전제: `herdr_cancel_enabled`(기본 false) 운영값을 U5·배포 레그에 넣는다.
- 같은 nonce의 Pending 진전(위치 채택·늦은 SessionStart가 `PendingUnplaced` watch를 깨우지 않음, `claude/herdr_turn/retry_probe.rs:78-86`)과 tab snapshot 필수 필드 누락 → `Held`·create 0(`herdr_launch/unplaced.rs:185-203`)을 S2c 시험에 넣는다.
- S2b idle 종료는 S2c에서 처음 기록한 자식 incarnation으로 한정한다. 같은 Bound fixture로 S2b까지는 close·archive 0회, S2c부터 exact 해제 1회를 확인한다.

### 조각·크기 재추정(w2)

| 조각 | w1 추정 | w2 추정(prod 파일 / 순증) | 바뀐 이유 |
|---|---|---|---|
| S2a-1 | 5 / +70~120 | 5 / +70~120 | 그대로 |
| S2a-2 | (S2a 합계 22~26 / +350~600) | 19~21 / +320~520 | P1-8: `tui_prompt_relay.rs`, `tui_prompt_relay/synthetic_start/admission_fence.rs`, `tui_o/writer/actor.rs`(giant 1057, 호출만), `tui_o/writer/input_notices/admission.rs`. rowless probe |
| S2a-3 | (위에 포함) | 8~9 / +100~200 | 그대로(Q-P1: S2c로 옮길지는 미정) |
| S2b | 9~12 / +350~600 | 15~18 / +600~950 | P1-4(`launch_host.rs`, `herdr_launch.rs`, `pending.rs`, `never_started.rs`), P1-5(`runtime_bootstrap.rs` 순서, `herdr_reader.rs` 재사용), P1-6(`orchestration.rs`, 종료 엔진 정산 증거, registry verdict 분기) |
| S2c | 25~29 / +650~1100 | 28~32 / +800~1300 | P1-1·2(`first_input.rs`, `herdr_turn.rs`, `worker_entry.rs` 정산 연결), P1-3(`adk_thread.rs`, reset 경로), P1-7(`restart/herdr.rs`, `restart_seed.rs`). P1-4의 `launch_host` 부분은 S2b로 옮김 |

- S2a는 T0 #8430·#8432·#8399 겹침 파일을 새로 건드리지 않는다. 새로 붙는 P1-8 파일 중 `admission_fence.rs`·`writer/actor.rs`는 `mkrx_w7`(가시성)과만 겹친다.
- S2c가 T0와 겹치는 파일:
  - `restart/herdr.rs`, `restart_seed.rs`(`i8399_s3b2`)
  - `provider_dispatch/owner_input.rs`(`i8430_s1`·`i8399_s3b2`)
  - `session_host/herdr_gate.rs`(`i8430_s1`)
- S2c 추정 상한(32파일 / +1300)이 prod 30파일 상한과 +1200 정지선을 넘는다. 분할안은 `## 범위 선택지`에 있다.

### 상태 차트(w2)

`docs/statecharts/thread-host/ThreadHostS1b.tla` `SpecS2`의 `S2` 모드가 이제 w2 계약이다. w1의 결함은 `MutW1*` 뮤턴트로 재현한다. 결과는 report "Design w2" 절에 있다.

| P1 | w1 결함 재현(RED) | w2 계약 |
|---|---|---|
| 1 | `MutW1BindFirst`: `FirstInputResolves` RED(Placed에서 Bind는 첫 키를, 첫 키는 Bound를 기다림) | `S2W2All`·`S2All` GREEN |
| 2 | `MutW1RetrySkipsReserve`: `KeyAfterReservation` RED(3단계: 예약 없이 첫 키) | GREEN |
| 3 | `MutW1ResetRefuses`: `NoInputRefused` RED(1단계), `FirstInputResolves` RED | GREEN |
| 5 | `MutW1BootGateBlocks`: `BootServesOthers` RED(0단계: gate가 영원히 대기) | `S2W2Boot` GREEN, main 대조 `S2W2BootMain` GREEN |
| 7 | `MutW1RestartNoSeed`: `FirstInputResolves` RED | GREEN |

## 범위 선택지

P1 8건은 모두 닫는 방향이 정해졌다(반영 8/8, 기각 0). 반영 뒤 새로 드러난 결함 축은 없다(rowless probe는 리뷰가 미확인으로 남긴 항목이고, S2a 관측 계약 안에서 닫힌다). 그래서 범위는 유지한다. 다만 S2c 추정이 상한을 넘으므로 아래 분할을 함께 낸다.

- **안 A(권고): S2c를 생산 호출자가 있는 선행 조각과 활성화 조각으로 나눈다.**
  - **S2c-1(선행, 약 10~12파일 / +300~500):** 오늘의 생산 경로에서도 의미가 있는 것들이다.
    - P1-2 공통 write의 예약 강제 골격: 자기 boot Herdr 채널의 exact retry에도 같은 경계가 생긴다.
    - P1-3 I3의 자식·provider·owner 선고정: counter-model 재사용은 오늘도 있다.
    - P1-7 restart의 `adopt` 후 seed: 자기 boot Herdr 채널의 vacant `/restart`도 같은 결함이다.
    - Herdr 클라이언트 `tab.create` 요청·응답 모델: protocol fixture로 시험한다.
  - **S2c-2(활성화, 약 18~22파일 / +500~800):** `next_host` 본문, child plan·permit, tab 배치·marker 복구, 첫 입력 순서(P1-1), vacant reset fresh 의도(P1-3 나머지), tmux 생성 거절.
  - 위험: S2c-1의 tab 모델은 S2c-2 전까지 호출자가 없다. 이것을 피하려면 tab 모델을 S2c-2로 옮긴다(S2c-2 +100).
- **안 B: S2c를 하나로 두고 S2b로 더 당긴다.** P1-4를 이미 당겼으므로 추가로 당길 후보는 Herdr 클라이언트 tab 모델뿐이다. 그러면 S2b에 호출자 없는 코드가 생긴다. 권고하지 않는다.
- **안 C: 활성화 범위를 줄인다.** S2c 활성화를 "사람·봇 첫 메시지(I1·I2)의 새 스레드"로 한정하고, I3 redirect·exact retry(I10)·`/restart` vacant(I16)는 기존대로 Tmux 또는 Hold로 둔다. DoD 1의 "새 스레드"는 덮지만 dispatch 스레드(I3)의 Herdr화가 늦어진다. 18:12 증상(조율 스레드, I1)은 닫힌다.

---

# 본문(w1 전문, w2 표시 포함)

#8118 S2 설계 — Herdr 부모 채널 아래 스레드 세션의 배치·활성화

S2는 Claude 자식의 부모 workspace 내 tab 배치, 첫 입력과 출력 보호, Pending 복구, idle 해제와 슬롯 회전을 함께 완성한다. 신규 활성화는 마지막 착지 조각 한 곳에서만 한다.

소스 기준은 **`b52e052e24e13781569050ec940eb35b9496ed54`**다. base b04ea56650e5397989d33f1be199be21a83c8d0e와 비교해 변경이 문서·상태 차트에 한정됨을 확인했다. 아래 기존 코드 좌표는 모두 이 고정 SHA 기준이며, src/services/ 접두는 생략한다. 신규 파일은 신규로 표시한다. 

1. 요약 — 결정 목록

| 항목 | 결정 |
|---|---|
| FreshAfterKill | Herdr fresh. Legacy 행이 남아 있어도 tmux가 없고 selector도 없으면 새 실행으로 취급한다. |
| 기존 tmux 보존 | live tmux 또는 기존 tmux selector가 있으면 Tmux를 유지한다. transcript 부재에 따른 FreshSubstitution도 Tmux 안에서 처리한다. |
| selector 정본 | canonical DB 행의 claude_session_id. 행 상태와 같은 조회에서 읽는다. Herdr Retired의 native ID는 tmux selector와 구분한다. |
| 첫 실행 의도와 허가 | ChildFresh 실행 의도와 ChildLaunchPermit(Fresh)를 분리한다. 후자는 late child의 O 준비 예외만 허용한다. |
| 배치 | 검증된 부모 Bound pane의 workspace에 tab.create. 부모 pane이 없으면 D1 명시 거절한다. |
| 복구 | 자식의 기존 nonce와 부모 endpoint를 사용한다. tab marker로 원래 pane을 찾고, 미확정 입력은 같은 owner custody에서 기다린다. [w2-P1-1] 첫 키는 Bind 앞(placed Pending + 예약 뒤). [w2-P1-4] command 확정 미전송은 exact 정리 뒤 같은 원본 재개. |
| 출력 | 모든 새 Claude Herdr 자식 incarnation에 claude_thread_writer:<parent>를 기록하고 첫 키 전에 예약을 확정한다. |
| 해제 | idle 자식 해제를 S2에 포함한다. 기존 주기 복구 pass가 해제하고, 슬롯은 확정 Retired 또는 rowless 상태에서 회전한다. [w2-P1-6] 종료 + 정산 완료 + map 전이를 묶은 해제 판정 뒤에만 archive. |
| 관계·부팅 | 부모 관계를 O candidate 존재와 분리한다. 관계 복원과 Bound seed를 소비자 시작보다 앞에 둔다. [w2-P1-5] 끝난 행은 그 앞에서 exact retire, 미확인은 owner 단위 Hold로 두고 소비자는 시작한다. [w2-P1-8] 현재 실행 기록은 provider·정규 owner별. |
| 범위 | canonical Claude TUI 입력은 S2. Codex 신규 자식은 S4. routine Process 및 독립 label/snapshot 실행은 S1r. |
| 분할 | S2a: 현재 소비자·판정 정합 → S2b: 부팅·복구·해제 → S2c: tab 배치·첫 입력·활성화. 활성화는 S2c에만 둔다. |


현재 코드에서 이 결정을 요구하는 지점은 turn_host.rs:219–243, host_decision.rs:197–204, herdr_reader.rs:207–218,363–366, session_idle_cleanup.rs:25–42,75–81, auto_queue/runtime.rs:498–507,555–590이다.

2. Q1~Q16 답
Q1. FreshAfterKill은 Herdr인가, tmux인가?

Herdr fresh로 정한다.

“Legacy 행이 있으면 Tmux” 안은 행의 잔존 여부에 다음 실행의 호스트를 맡긴다. 이 안은 18:12 증상을 그대로 남기고, 같은 스레드도 GC 전후에 답이 달라진다. S2에서는 현재 실행과 재개 의도로 구분한다.

| 확인된 상태 | 다음 행동 |
|---|---|
| 부모 endpoint와 owner가 일치하는 Herdr Bound | 현재 Herdr 사용 |
| Herdr Pending | 기존 실행 복구 대기. 새 create·Tmux 생성 금지 |
| Herdr Retired | Herdr 후속 실행 대상으로 판단 |
| Legacy/행 없음 + tmux Present | selector 유무와 관계없이 Tmux 유지 |
| Legacy/행 없음 + tmux Missing + 기존 tmux selector 있음 | Tmux 재생성 |
| Legacy/행 없음 + tmux Missing + selector 없음 | Claude 자식 Herdr fresh |
| 행·selector·tmux 관측·관계가 미확인 | Hold |


현재 turn_host는 일치하는 Bound만 Herdr로 보내고, Legacy는 :242에서 Tmux로 보낸다. host_decision에는 이미 Present/Missing/ProbeFailed 구분이 있다. 이 관측을 공통화한다. 

FreshAfterKill은 기존 native session을 Herdr로 resume하는 입구가 아니다. ChildFresh는 뒤의 handoff 단계에서도 강제로 fresh를 유지해야 한다.

Q2. selector 출처와 FreshSubstitution은?

canonical DB 행의 claude_session_id를 호스트 연속성의 정본으로 사용한다.

requested_session_id는 launch 단계의 값이며, intake 안에서도 변경된다. resumed=false 역시 native launch의 결과다. 둘을 다음 호스트 판정에 사용하지 않는다.

HostedObservation의 기존 조회에 selector를 포함해 hosted 상태와 selector를 같은 행 snapshot으로 읽는다. 조회 실패는 None으로 축약하지 않는다. 현재 구조와 조회는 src/db/dispatched_sessions/hosted_execution.rs:203–224,247–312에 있다. 

구분은 다음과 같다.

Legacy의 selector: 기존 tmux 연속성. 있으면 Tmux.
Herdr Retired의 native ID: 기존 Herdr 실행 계열의 연속성. Herdr 후속 실행에 사용한다.
FreshSubstitution: 이미 Tmux로 선택된 실행의 transcript가 없어 새 UUID를 쓰는 처리. 호스트를 다시 선택하지 않는다.

현재 selector 삭제도 hosted_execution IS NULL인 행을 대상으로 하므로 두 출처를 구별할 근거가 있다(src/db/dispatched_sessions.rs:2093–2126). FreshSubstitution 자체는 claude/tui_session_launch/prepared.rs:119–151이다. 

S3를 다시 여는 두 경로도 닫는다.

claude/herdr_turn/handoff.rs:83–118,175–264는 durable binding에서 session을 다시 고르고 기존 tmux를 종료할 수 있다. ChildFresh는 기존 force_fresh 분기로 보내 이 경로를 건너뛴다.
discord/queue_io/input_owner/claude_handoff.rs:118–159,198,271–286는 다음 호스트가 Herdr이고 행이 Legacy/없음이면 handoff journal을 시작할 수 있다. ChildFresh는 journal 신규 개시 대상에서 제외하고 일반 cold owner start로 보낸다.

두 giant 파일에는 조건·호출 교체만 넣는다. 판단 본체는 공통 child 판단 모듈이 소유한다.

Q3. 첫 launch 허가의 정확한 조건은?

fresh 실행 의도와 O readiness 예외 허가를 분리한다.

child plan은 부모·배치 대상과 fresh 의도를 가진다.
first: Option<ChildLaunchPermit(Fresh)>는 late child의 첫 O 준비 예외를 표현한다.
HerdrTurn.force_fresh에는 입력의 기존 fresh 의도와 child fresh 의도를 함께 반영한다.
정상 fresh dispatch는 예외 허가 없이 ordinary O 준비 경로를 사용한다.

이 분리가 필요한 이유는 정상 fresh dispatch의 first=None을 “resume 가능”으로 해석하면 Q2의 binding 재탐색이 다시 열리기 때문이다.

최초 예외 허가 조건

다음을 모두 만족해야 한다.

Claude TUI이며 기존 herdr_turn_enabled가 켜져 있다.
확인된 child-parent 관계와 canonical owner/key가 일치한다.
부모 정책이 Herdr이고 endpoint가 로컬이다.
tmux Missing과 selector 부재를 확인했다.
시작 대상은 Legacy/없음이며, 기존 Pending·불일치 Bound·Unknown이 아니다.
부모의 O 선택·writer 기반이 유효하다.
child가 late origin이며 adoption이 Deferred 또는 child_parked()다.
ordinary o_writer_ready(child)가 false라서 이 예외가 실제로 필요하다.

late 조건의 현재 근거는 tui_o/cutover/claude_thread.rs:14–22, parked 조건은 channel_policy/adoption/child_run.rs:60–68이다. 정상 fresh dispatch는 children.rs:205–212에서 Adoption::Pending으로 만들어진다. 이 O adoption의 Pending은 예외 허가 대상이 아니다. DB의 HostedState::Pending과도 구별한다.

ordinary readiness는 O ownership·writer acceptance·binding checkpoint를 요구한다(herdr_launch.rs:79–118). 현재 Claude는 이것이 false면 configured_plan에서 바로 거절된다. 

Pending과 미전송 successor

Pending은 새 Fresh permit을 받지 않는다. 기존 nonce의 durable context를 검증하여 같은 실행의 복구·첫 입력 보호 경로에 들어간다.

> **[w2-P1-4]** create ACK 뒤 command가 **확정 미전송**이면 launch_host가 그 사실을 보존한다. exact nonce·marker·root를 확인해 잔재를 닫고 retire한 것이 이 절의 "확정 미전송 fresh successor" 증거가 된다. 전송 불명은 증거를 다시 확인할 뿐 successor 허가가 아니다(S2b).

다만 다음 정상 경로를 막아서는 안 된다.

fresh Pending → 재시작 → create 미전송 또는 untouched residue 제거 확인 → Retired → fresh successor.

이 경우에는 확정 미전송 fresh successor라는 증거로 다음 nonce의 Fresh permit을 받을 수 있다. 최소 증거는 다음과 같다.

동일 owner의 Retired 행과 기존 fresh child context;
일치하는 source_policy·부모·logical key·nonce;
unplaced 종료라면 location=None, expected=None;
기존 입력 hold의 durable 부재;
owner 원본이 아직 미시도이거나 기존 owner 판단이 KnownNotSent.

Retired만으로 MayHaveSent/Unconfirmed를 KnownNotSent로 바꾸지 않는다. retired_hold::before_fresh가 hold를 지우는 동작도 미전송 증거로 사용하지 않는다. unplaced retire의 현재 제한은 herdr_launch.rs:684–715다.

허가는 plan이 소유하고 HerdrTurn이 빌려 쓴다. 복제·직렬화·전역 캐시하지 않는다. 입력의 일회성 소비는 기존 owner의 non-copy claim과 durable attempt가 담당한다. 사전 판정은 eligibility만 읽으며 create·출력 예약·입력 소비를 하지 않는다.

Q4. 배치는 부모 workspace tab인가, 채널별 workspace인가?

부모 Bound pane의 workspace에 자식 tab을 만든다.

채널별 workspace 안은 기존 코드를 덜 바꾸지만 부모와 자식의 배치를 분리한다. 부모 workspace tab 안은 부모 아래 자식을 모으려는 S2 목적과 맞으며, protocol 22의 저장소 fixture로 요청·응답을 확인할 수 있다.

| protocol 22 계약 | 고정 소스 좌표 |
|---|---|
| TabCreateParams: workspace, cwd, label, focus, env | tests/fixtures/herdr/herdr-schema-0.9.3.json:4273–4305 |
| tab.create method | 같은 파일 :5132–5146 |
| tab_created, tab, root_pane 응답 | :9401–9419 |
| snapshot의 tabs 및 TabInfo | :10503–10572 |



배치 계약은 다음과 같다.

HostedOwner는 자식 그대로 둔다. 부모는 물리적 배치의 근거다.
부모 canonical row가 Bound이며 provider·token·runtime·endpoint가 일치하는지 확인한다.
같은 server witness로 부모 pane을 읽어 실제 workspace_id를 얻는다.
child의 기존 logical_key#execution_nonce marker를 tab label로 넣는다.
focus=false, 기존 child cwd를 사용한다.
응답의 workspace·tab·root pane·cwd를 대조한 뒤 기존 command-send 경계를 진행한다.
create는 재전송하지 않는다. 결과 불명은 Pending으로 남긴다.

현재 create→검증→command send 순서는 session_host/herdr/launch_host.rs:594–655다. 이 순서와 user-default grant, restore-off witness, NotSent/Indeterminate 구분을 보존한다. 

D1: 신규 create에 필요한 부모 Bound pane이 없거나 확인되지 않으면 명시 거절한다. workspace 생성으로 우회하지 않는다. Pending 설치 전 확인하고, 실제 create 직전에도 target/witness를 재검사한다.

복구: 원래 endpoint 전체의 tabs에서 정확한 marker를 찾는다. 현재 부모 workspace에만 한정하지 않는다. 부모 pane이 이동하거나 사라져도 이미 만들어진 자식 tab은 복구할 수 있다. 부모 endpoint와 기존 grant가 맞지 않으면 Hold한다.

자식 잔재 판정은 tab의 pane_count=1과 정확한 marker/root를 사용한다. 부모 workspace 전체에 pane/tab이 하나여야 한다는 기존 조건은 사용하지 않는다. herdr_gate.rs:1152–1252의 residue 본체를 child 파일로 옮겨 scope 검증을 확장한다. child tab 경로에 bare label·workspace marker fallback은 추가하지 않는다.

Q5. 재시작 뒤 Pending 복구와 재생 입력 보류는?

기존 unplaced resolver를 부모 endpoint로 연결하고, owner 입력을 typed wait로 보존한다.

현재 herdr_reader.rs:363–366은 자기 boot만 읽는다. 또 :207–218은 rows를 읽은 다음 unplaced 복구를 실행하므로 후반 순회가 이전 snapshot을 사용할 수 있다. 복구 이후 최신 행을 다시 읽어 후속 처리를 해야 한다. 

복구 상태를 다음처럼 구분한다.

unplaced Pending → marker·process 증거 채택 → placed Pending → own SessionStart/source 확인 → Bound → 현재 실행 seed → 첫 입력 진행

> **[w2-P1-1] 위 순서를 대체한다:** unplaced Pending → marker·process 증거 채택 → **정확히 검증한 placed Pending → 영속 출력 예약 + 원본 attempt → 첫 키 → transcript 첫 레코드 검증 → Bind → 현재 실행 seed.** 콜드 Claude는 첫 프롬프트가 transcript를 만든 뒤에야 Bind한다(`claude/herdr_turn/first_input.rs:534-601`). 그래서 Bound를 첫 키의 전제로 두지 않는다. 첫 키와 Bind 사이에는 그 원본만 키를 쓸 수 있다.

Resolution::Adopted는 Bound가 아니다. 기존 채택은 Pending에 위치·증거를 채우며, 실제 bind는 discord/tui_prompt_relay/herdr_source.rs:132–190에서 source와 nonce를 확인한 뒤 수행한다. 

I7·I8·I10·I11에는 다음을 적용한다.

PendingUnplaced, PendingUnproven, PendingUnstarted를 기존 owner wait로 유지한다.
원본 source ID·generation·ticket을 바꾸지 않는다.
resolver는 입력을 제출하지 않는다.
복구 진전은 기존 owner를 깨우는 신호다. 새 original enqueue가 아니다.
endpoint·DB·증거 미확인은 보류다. Tmux나 새 create로 넘어가지 않는다.

현재 wait 원인과 attach 경계는 claude/herdr_turn/pending.rs:7–69, unplaced lease와 재시도는 herdr_reader.rs:368–416에 있다. 기존 quiet/retry 동안 입력이 소모되지 않아야 한다.

부팅 전체가 Pending의 원격 Bind 완료를 기다리지는 않는다. Pending을 식별해 입력·출력을 보류한 뒤 복구 worker와 attach 경로를 시작해야 순환 대기가 생기지 않는다.

Q6. idle 해제와 슬롯 회전을 S2에 포함하는가?

둘 다 포함한다. 슬롯의 즉시 종료 포트 확장은 하지 않는다.

슬롯 reset만 여는 안은 idle 자식 pane을 남긴다. idle 정리를 전부 #7583으로 넘기는 안은 EventuallyReleased를 충족하지 못한다. S2는 Claude Herdr 자식에 한정한 해제를 맡는다.

정기 해제

session_idle_cleanup은 메시지 수신으로 기동하고 메모리 세션만 읽는다(:25–42,53–70). 이것만 수정하면 재시작 뒤 무입력 자식이 남을 수 있다.

기존 주기 경로에 자식 해제를 붙인다.

discord/tui_prompt_relay/source_tick.rs:28–49
→ rehydration.rs:251–274
→ herdr_reader::reconnect_pass.

후보는 기존 thread GC의 1시간 idle 기준과 status·active dispatch·active children 조건을 재사용하되, 확인된 Bound Claude child로 한정한다(src/db/dispatched_sessions/thread_gc.rs:55–75).

새 herdr_terminate/idle.rs가 자동 idle 종료의 조건을 소유한다. transition/fence 아래에서 다음을 다시 확인한다.

같은 row·owner·nonce;
idle 조건 지속;
active turn·owner front·queued input·dispatch·active child 없음;
미정산 입력 hold와 출력 custody 없음;
같은 실행의 source/pane이 idle임을 확인할 수 있음.

공통 종료 엔진의 exact close→종료 확인→CAS retire→정산을 재사용한다(herdr_terminate.rs:454–642). 사전 idle 검사 후 operator 종료를 호출하는 방식은 사용하지 않는다. 검사와 종료 사이의 새 턴을 종료할 수 있기 때문이다. 

일반 idle 종료는 native selector를 보존한다. 이후의 Herdr→Herdr 재개는 S3에 해당하지 않는다.

슬롯 회전

슬롯이 직접 Bound pane을 종료하지 않는다. 주기 해제로 Retired가 된 뒤 다음 reset 시 회전한다.

> **[w2-P1-6]** 슬롯 해제는 "exact 실행 종료 + 정산 완료(`settle_and_release` 뒤 증거) + slot map 전이"를 하나로 묶은 판정 뒤에만 archive한다(`auto_queue/runtime.rs:387-445`의 archive → DB 순서를 뒤집는다). Herdr 해제 경로는 `managed_reset_verdict`(`:511-519`)와 managed tmux·mailbox reset을 거치지 않는다. dispatch 재사용 실패·중복 archive(`dispatches/discord_delivery/orchestration.rs:847-904`)도 같은 판정을 쓴다. `Retired`만으로 정산 완료를 추정하지 않는다(종료 엔진은 retire를 정산보다 먼저 기록한다, `herdr_terminate.rs:623,630`).

| 실제 상태 | 슬롯 처리 |
|---|---|
| Herdr Bound·Pending·미확인 | 보류 |
| 확정 Herdr Retired | 정확한 row·nonce의 정산 허용 |
| rowless | 기존 rowless 정산 허용 |
| Legacy tmux | 기존 keyed tmux gate 유지 |


E78뿐 아니라 E79도 바꿔야 한다. 현재 후단 gate도 Retired를 “confirmed legacy tmux가 아님”으로 거절한다(runtime.rs:555–590).

Herdr Retired를 기존 broad UPDATE에 그대로 합치지 않는다. runtime.rs:176–191은 hosted state/nonce 조건이 없으므로, successor가 생긴 뒤 selector를 지울 수 있다. Herdr 정산에는 exact row·Retired nonce 조건을 사용하고, 불일치하면 slot binding도 보존한다.

기존 herdr_terminate_enabled 종료 권한은 유지한다(session_host/operation_policy.rs:16–19). S2용 새 스위치는 만들지 않는다. S2 활성화·중단은 herdr_turn_enabled로 하며, 종료 권한이 명시적으로 꺼진 동안은 pane·selector·slot을 보존한다. liveness는 종료가 허가되고 idle 조건이 지속되는 운영 전제에서 보장한다.

Q7. 실행 없는 스레드의 /restart·reset·locator는?

확인된 실행 부재에서만 다음 호스트를 사용한다.

> **[w2-P1-7]** fresh 대상의 `/restart` vacant 경로는 restart gate 아래에서 기존 local TUI `ingress::adopt`를 먼저 부르고 같은 deterministic seed를 접수한다. owner가 없으면 `Seed::of`가 `"V2 not adopted"`로 실패한다(`queue_io/input_owner/restart_seed.rs:56-58`).

fresh 대상: /restart는 Herdr의 vacant 경로에서 첫 입력을 시작한다. tmux reset은 거절한다. locator는 canonical/preferred key만 반환한다.
live tmux 또는 기존 tmux selector 있음: 기존 Tmux 명령 경로를 유지한다.
Bound Herdr: 현재 Herdr 명령 경로를 유지한다.
Pending·미확인: Hold하며 guessed tmux 조작을 하지 않는다.

기존 /restart는 이미 Herdr/Tmux/Hold를 나누고(restart.rs:291–316), Herdr의 Missing/Legacy를 Vacant로 처리한다(restart/herdr.rs:205–213). locator도 Herdr 판정이면 observe_old를 생략한다(session_locator.rs:177–194).

HostDecision::Herdr는 명령이 선택할 호스트를 뜻한다. 현재 Bound pane의 존재 증거로 사용하지 않는다. 현재 실행은 별도의 검증된 observation으로 표현한다.

또한 input_runs_herdr의 tmux probe 생략을 제거한다. 현재 probe_tmux=false는 Legacy를 실제 관측 없이 Nothing으로 바꾼다(host_decision.rs:149–160,281–282). 이 상태에서 Nothing만 Herdr로 바꾸면 live tmux가 부모 동의에 묶인다.

Q8. C행 13곳은 각각 어떻게 처리하는가?

| E행 | 판정 | 계약과 근거 |
|---|---|---|
| E20 | S4 | Codex absence 분류는 유지. 실제 생성 gate와 분류 predicate를 분리한다. discord/codex_absence.rs:28–48 |
| E32 | 남김 | thread: Some이면 own-boot 분기를 건너뛴다. src/server/routes/agents_turn_deliver/origin_admission.rs:17–26 |
| E34 | 바꿈 | 공통 consent endpoint를 확인된 부모 정책으로 해석. user-default·local 조건 유지. herdr_consent.rs:31–42 |
| E35 | 바꿈, 공통 함수 소비 | 자식 checkpoint도 실제 custody 수용을 기다린다. intake_dispatch.rs:585–631,895–901 |
| E36 | 남김 | legacy 즉시 주입을 보수적으로 제외해도 기존 owner/큐로 진행한다. 기존 tmux의 eventual drain 회귀 시험을 둔다. busy_inject.rs:84–110 |
| E47 | 바꿈, 공통 함수 소비 | child clear가 지운 정확한 source/generation만 동의 정산. clear_report.rs:136–170 |
| E56 | 바꿈, 공통 함수 소비 | 부모 endpoint의 server-replaced 증거를 자식 nonce에 적용. root_replaced/preclaim.rs:22–55 |
| E59 | 첫 커밋에서 바꿈 | herdr_codex에 이미 읽은 herdr_now 전달. effort.rs:135–158,184–205 |
| E60 | 첫 커밋에서 바꿈 | 저장·capability·restart가 동일 현재 호스트 판정을 사용. effort.rs:257–263, tui_passthrough.rs:94–98 |
| E73 | 바꿈 | 부모 endpoint로 Pending 복구. herdr_reader.rs:363–393 |
| E74 | 바꿈, 공통 함수 소비 | demand/ensure/recover의 endpoint는 부모, canonical owner는 자식. herdr_server_replaced.rs:27–44,47–94 |
| E84 | 남김 | 등록 binding 필터에서 동적 자식이 제외된다. 자식 Bound는 별도 소비자. unbound.rs:267–288,520–534 |
| E87 | S4 | Codex 전용이며 Claude는 앞에서 반환한다. writer/rotation/readers.rs:801–809 |


E34의 공통 변경에서 endpoint 선택과 입력 identity를 분리한다. 부모는 서버 선택에만 사용한다. mailbox·consent intent·HostedOwner를 부모로 바꾸지 않는다.

herdr_consent/return_input.rs:14–16의 own mapping guard는 유지한다. 이 명령은 개별 자식 실행이 아니라 부팅 설정의 채널 binding rollback을 다룬다. 자식에게 부모 binding rollback을 열지 않는다.

Q9. Codex 자식은 S2인가?

신규 Codex 자식 활성화는 S4다.

S2는 다음만 수행한다.

새 Claude 자식의 Herdr fresh 활성화;
이미 검증된 Codex Herdr Bound의 현재 동작 보존;
첫 커밋의 /effort 현재 호스트 수리;
공통 tmux 생성 gate의 provider 구분 정리.

configured_plan의 Codex switch 조건을 완화하지 않는다(turn_host.rs:327–343,421–429). Codex Pending/첫 입력은 Claude와도 다르므로 같은 first permit을 적용하지 않는다(codex/herdr_turn.rs:287–325).

생성 gate는 provider가 명시된 Tmux / Herdr / Hold 판정을 받는다. channel-only 캐시의 마지막 provider를 믿지 않는다. dispatch가 provider를 바꿀 수 있기 때문이다(intake_turn.rs:788–797).

E20의 “로컬 Codex TUI인가” 분류에는 Hold를 Herdr로 축약하는 생성 차단 bool을 사용하지 않는다.

Q10. JUDGED boot seed는 어디에 두며 D행은 어떻게 기다리는가?

provider runtime의 공통 시작 경계에서 seed를 끝낸다. 동기 조회 함수 안에서 기다리지 않는다.

> **[w2-P1-5]** 이 절의 "Bound 검증 실패는 hold"를 고친다. 소비자 시작 전에 기존 종료 판정(`recovery_engine/herdr_reader.rs:273-304,436-470`)으로 **끝난 것이 확인된 행은 exact retire**하고, 남은 미확인은 **그 owner에만 Hold**를 게시한 뒤 소비자를 시작한다. 그 뒤 복구를 계속하고 실제 Bound만 seed한다. 사라진 pane의 retire는 지금 relay rehydrate 안에서 일어나므로(`tui_prompt_relay/rehydration.rs:270-274`), gate가 그것을 기다리면 순환이 된다.

기존 runtime_bootstrap.rs:302–304의 native boot 준비 뒤, :309–311의 confirm·presence 등록·relay spawn 앞에 둔다. 단, 그 전에 다음 순서 변경이 필요하다.

기존 cached_bot_token 설정(:324)을 앞으로 이동한다.
runtime_bootstrap/o_writer_host.rs:523–540의 checked Discord lookup 설치를 writer spawn에서 prepare 단계로 이동한다.
저장된 관계와 예약을 복원하고, 필요한 관계를 checked Discord read로 확정한다.
로컬 provider·token·runtime 소유 행을 판정한다.
검증된 Bound를 seed한 뒤 소비자를 시작한다.

현재 lookup은 writer spawn에서야 설치된다(o_writer_host.rs:561). 그대로 두고 seed만 앞에 넣으면 부모 확인을 할 수 없다. 또 record_judged는 이미 확인된 Child만 기록하므로 관계 복원보다 먼저 호출하면 기록이 버려진다(thread_host.rs:90–97). 

공통 gate가 막는 시작점은 다음과 같다.

relay·synthetic start: runtime_bootstrap.rs:309–311;
presence: :327;
REST worker 등록·queue 복원·intake: :332–340;
gateway 등록·writer·intake: :405–413;
standby 등록·writer·intake: :452–457.

DB·관계·Bound 검증 실패는 기존 native boot와 같은 hold/retry로 처리한다(runtime_bootstrap/native_input.rs:80–128). HTTP도 runtime 등록 전에 RuntimeUnavailable이므로 D30에 먼저 도달하지 않는다.

Pending은 별도다. Pending을 true 또는 false로 위장해 seed하지 않는다. Pending임을 확인하고 owner wait와 미게시 상태를 유지한 뒤 복구 경로를 시작한다. 실제 Bind 성공 때 seed한다. 이 구조가 아직 시작하지 않은 attach를 부팅이 기다리는 순환 대기를 피한다.

Q11. 첫 커밋 계약은?

S2a 첫 커밋은 요청된 세 수리로 고정한다.

① /clear·`!clear`

저장된 Herdr 행을 먼저 처리한다.

Pending/Bound: 기존 exact terminate;
Retired: 기존 exact selector 삭제;
Unknown/Conflict 또는 어느 호스트도 확정하지 못한 Hold: end_unconfirmed, destructive effect 없음;
확인된 Legacy/Missing이고 Tmux인 경우만 기존 managed teardown.

호출 조건만 넓혀서는 부족하다. 현재 end_herdr는 미확인에도 false를 반환할 수 있으므로, 처리됨 / Legacy·Missing 확인 / 미확인을 구별해 미확인 상태의 tmux fallback을 막는다. resume_fence도 .is_herdr() 하나에 의존하지 않는다. 근거: clear_teardown.rs:72–81,148–191, resume_fence.rs:23–24.

② recorded_execution

is_live_thread_endpoint의 검증 결과를 verified_bound로 전달한다. gate refusal은 true일 때만 현재 Herdr를 기록한다. 활성화 후에는 fresh Herdr 계획 자체도 현재 실행 증거로 기록하지 않는다. 근거: turn_host.rs:171–187,255–277.

③ /effort

herdr_codex의 own-boot 검사를 이미 읽은 herdr_now로 바꾼다. capability·저장·passthrough가 동일 판정을 사용한다. 근거: effort.rs:135–158,184–205,257–263, tui_passthrough.rs:94–98.

시험과 필수 뮤턴트는 §6의 T-S2-10으로 고정한다.

Q12. 재시작 재생이 첫 입력을 다시 제출하지 않는 근거는?

기존 owner attempt와 Herdr의 durable hold를 유지한다. 새 중복 방지 장치는 만들지 않는다.

| 단계 | 현재 생산 경계 |
|---|---|
| front를 reserve한 뒤 effect | discord/queue_io/input_owner/pump.rs:76–97 |
| Reserved durable commit, claim 취소 | turn_orchestrator/queued_head/v2.rs:303–338 |
| head가 있으면 새 front를 주지 않음 | 같은 파일 :417–425 |
| 키 전 MayHaveSent 기록 | queued_head/v2/attempt.rs:97–118 |
| 실제 attached target으로 owner hook 호출 | claude/herdr_turn/first_input.rs:108–168 |
| 이전 hold 판정 → 새 hold 기록 → 키 → outcome | 같은 파일 :269–333 |
| hold fsync·rename | claude/herdr_turn/hold_record.rs:101–116 |
| 성공 시 원본 제거와 terminal identity 함께 commit | queued_head/v2/retained.rs:122–146 |
| crash/불명은 Unconfirmed 보관 | 같은 파일 :65–83 |
| 자동 재큐는 KnownNotSent·Reserved만 | 같은 파일 :339–352 |



첫 키 이후 성공 정산 전에 죽었다면 결과가 불명일 수 있다. 이 경우 자동 재전송하지 않는다. exact receipt 또는 기존 미전송 증거로 해결한다.

I7·I8·I10·I11은 원본 identity를 유지한다. Pending 복구나 I3 redirect를 새 original 생성의 이유로 사용하지 않는다. pending_handover도 snapshot nonce가 아니라 실제 attached nonce를 사용한다(first_input.rs:69–78).

> **[w2-P1-2]** I10 exact retry는 owner hook 없이 `first_input::submit`으로 들어간다(`intake_turn/worker_entry.rs:182`, `queue_io/input_owner/ingress.rs:415-417`, `claude/herdr_turn.rs:770`). 그래서 첫 키 전 예약은 owner hook이 아니라 공통 `first_input` write에 둔다. I11은 별도 internal follow-up이므로 이 표의 owner attempt 증명을 그대로 적용하지 않는다(T-S2-13에 따로 넣음).

Q13. 루틴·예약 입력은 S2인가, S1r인가?

출처 이름이 아니라 실행 identity로 구분한다.

| 실행 형태 | 범위 |
|---|---|
| canonical Claude TUI, Standing::Channel | S2 |
| routine Process | S1r |
| 독립 label·scheduled snapshot, Standing::Apart | S1r |


현재 routine은 별도 label을 받는 경로가 있고, owner가 받아도 label이 있으면 Apart가 된다(routines/agent_executor/launch.rs:168–171, headless_turn/admission.rs:18–32). Herdr launch는 channel canonical row를 사용한다(herdr_launch.rs:796–811). 따라서 Apart 허용만 켜면 격리 identity를 잃는다.

S2에서는 label을 제거하거나 canonical child에 합치지 않는다. 지원되지 않는 조합은 구체적인 이유를 가진 unsupported 결과와 원본 보존으로 처리한다. S1r 지원 또는 운영자가 지원되는 대상으로 재지정하기 전까지 무한 자동 재시도하지 않는다. 이것을 FirstInputResolves의 성공 대상으로 세지 않는다.

Q14. 관계 기록은 O 선택과 독립해야 하는가? bootstrap은?

관계는 독립시킨다. 기존 child 저장 구조를 재사용하되 O candidate와 구별한다.

현재는 다음 지점들이 관계를 O 선택에 묶는다.

cutover.rs:179–225: O 선택 부모만 lookup·record;
children.rs:79–86: candidate가 없으면 child 읽기도 없음;
children.rs:476–480: policy_channel이 child 읽기에 의존;
channel_policy.rs:253–280: kind가 O 선택 기반.

record_thread_child의 조건 하나만 넓혀서는 해결되지 않는다. 세 파일에서 확인된 parent/kind/held/conflict 관계 조회를 O candidate 조회와 분리한다. O candidate는 기존 O 선택 조건에서만 만든다. 

관계만 기록했다고 writer가 켜지지 않는다. 부모가 Herdr이지만 O 미선택이면 Herdr 정책으로 판정한 뒤 출력 준비 실패를 명시한다. Tmux fallback은 하지 않는다.

headless 진입도 판정 전에 checked 관계를 확보한다. 현재 headless_turn.rs:86–87의 부모 조회는 기록까지 보장하지 않는다. 실패와 NotThread를 구별하는 기존 session_runtime/channel_routing.rs:296–303 의미를 사용한다.

E39 bootstrap은 유지한다. session_runtime.rs:487–638은 tmux spawn이 아니라 메모리 세션, 안정적 child 이름, worktree·cwd 준비다. 부모 Herdr라는 이유로 건너뛰면 자식의 작업 디렉터리 격리를 잃는다. 기존 관계 기록 호출만 개선된 recorder를 사용한다.

Q15. dispatch redirect에서 부모 ID 판정을 바꾸는가?

최종 host·create proof 판정은 실제 child ID로 한다. 원본 custody identity는 보존한다.

> **[w2-P1-3]** 기본 dispatch(implementation·review·rework, `reset_provider_state=true`, `src/dispatch/dispatch_context.rs:205-212`)는 reset이 거절되면 `Executed`로 끝나 `for_turn`에 가지 않는다(`intake_turn.rs:867-888`). 그래서 I3에서 자식·실행 provider·정규 owner를 먼저 고정한다. 확인된 vacant의 provider reset은 fresh 의도로 처리하고, `recreate_tmux`와 live/미확인 대상만 기존 가드나 원본 보존 typed 결과로 보낸다.

현재 intake_turn.rs:624–646에서 target이 child로 바뀐다. 부모 Bound는 consent assessment에서 proof 확인을 생략할 수 있으므로 부모 통과를 새 child create의 허가로 사용할 수 없다(herdr_consent/admission.rs:154–159).

I3의 순서는 다음으로 고정한다.

원본의 권한·큐 custody를 유지한다.
재사용 또는 새 thread target을 결정하고 재시도 target을 기존 dispatch의 active thread에 고정한다.
checked child-parent 관계를 확보한다.
child canonical row와 부모 endpoint proof로 host·consent를 판단한다.
통과한 뒤 bootstrap·실행 준비를 진행한다.
spawn 직전 공통 for_turn에서 다시 확인한다.

라우팅 전 부모 판정은 라우팅 필터로만 사용한다. redirect 가능한 I3를 부모의 Herdr 동의 때문에 먼저 소비하거나 종결하지 않는다. 재사용 child가 실제 Tmux이면 부모 동의에 묶이지 않아야 한다.

정책/실행 target과 원본 custody를 분리한다. 부모 owner가 가진 원본에서 request.channel_id만 child로 덮어쓰면 잘못된 mailbox를 읽을 수 있다(admission.rs:108–110,146–152,368–375). 보류는 실제 원본 parent/source/generation을 가리킨다. 새 child original로 복제하지 않는다.

본체는 intake_turn/adk_thread.rs 또는 그 child에 둔다. giant intake_turn.rs·`intake_dispatch.rs`에는 호출 교체·기존 블록 이동만 둔다.

Q16. 크기와 분할은?

당초 21~25파일/+830~1130 추정을 그대로 사용하지 않는다. 세 착지 조각으로 나눈다.

이번 소스에서는 다음이 추가로 필요하다.

selector와 실제 실행을 함께 읽는 관측;
provider를 구별하는 생성 gate;
O 관계 분리;
lookup 등록을 포함한 boot seed 순서;
메시지 수신과 독립된 idle 해제;
슬롯의 후단 gate와 exact-row 정산;
두 handoff 입구의 ChildFresh 제외;
redirect target/custody 구분.

이들은 각각 Q2·Q5·Q6·Q10·Q14·Q15의 확인된 생산 경계에서 나온다.

tab 배치만 먼저 착지시키지 않는다. S2a·S2b는 기존 생산 소비자가 호출하는 수리이고, tab.create·marker 복구·first permit·활성화는 S2c에서 함께 착지한다. 새 전환 스위치나 임시 호환 경로는 두지 않는다.

각 조각에 prod 30파일/순 +1500 상한과 +1200 정지선을 적용한다. 실제 측정은 pr_cap_prod.py만 사용하며, 조각별 결과와 S2 누적 결과를 모두 남긴다. 상세 소유·추정은 §5에 적는다.

3. 계약 — 불변식과 liveness

| ID | 확정 계약 | 코드 대응 |
|---|---|---|
| C1 | 다음 정책·현재 실행·명령 대상은 구별한다. fresh plan은 현재 Bound 증거가 아니다. | thread_host.rs:63–164, turn_host.rs:157–187, host_decision.rs:183–205 |
| C2 | Legacy의 live tmux 또는 selector가 있으면 Tmux 유지. FreshSubstitution으로 호스트를 바꾸지 않는다. | host_decision.rs:263–290, prepared.rs:119–151 |
| C3 | ChildFresh는 기존 tmux 종료·native resume·handoff journal 신규 개시를 하지 않는다. | 두 claude_handoff 경로, Q2 좌표 — [w2-P1-2] 공통 `first_input` write의 예약 강제로 exact retry도 같은 경계를 지난다. |
| C4 | 신규 자식 owner는 자식이며 배치는 검증된 부모 workspace의 tab이다. | herdr_launch.rs:224–235,548–603, launch_host.rs:594–655 |
| C5 | Pending commit 전에 create하지 않는다. create 결과 불명에서 재전송하지 않는다. | herdr_launch.rs:548–624, unplaced.rs:229–311 |
| C6 | Pending 복구는 같은 nonce를 채택하거나 확정 종료한다. Adopted를 Bound로 취급하지 않는다. | pending.rs:7–69, herdr_source.rs:132–190 — [w2-P1-1] 순서: placed Pending → 예약 → 첫 키 → transcript 검증 → Bind. Adopted도 placed Pending도 Bound가 아니다. |
| C7 | 모든 새 Claude child의 source policy는 부모를 고정하며 첫 키보다 앞서 출력 예약을 확정한다. | herdr_launch.rs:816–843, claude_thread.rs:59–135,173–249, first_input.rs:269–333 |
| C8 | 원본 identity별 첫 payload는 최대 1회. 불명 전송은 자동 재제출하지 않는다. | owner v2 attempt/retained, Q12 좌표 |
| C9 | 부팅 후 Bound를 처음 읽는 D 소비자보다 관계 복원·seed가 앞선다. | runtime_bootstrap.rs:302–340, o_writer_host.rs:523–562 — [w2-P1-5] 끝난 것이 확인된 행은 소비자 앞에서 exact retire, 미확인은 owner 단위 Hold. 전체 부팅을 막지 않는다. |
| C10 | 자동 idle 종료는 새 입력과 경합하면 중단하며 정확한 자식 nonce만 종료한다. | herdr_terminate.rs:498–581,617–642, 신규 idle.rs |
| C11 | 슬롯의 해제 가능성은 미래 호스트와 별개다. Retired 정산은 successor를 수정하지 않는다. | auto_queue/runtime.rs:176–191,498–590 — [w2-P1-6] 해제 판정(종료 + 정산 완료 + map 전이) 뒤에만 archive. |
| C12 | 부모 endpoint를 상속해도 custody·consent intent·HostedOwner identity는 바꾸지 않는다. | herdr_consent/admission.rs, intake_turn/adk_thread.rs |
| L1 | 지원되는 입력에서 부모·admission·O 준비가 회복되면 첫 입력이 진행된다. | configured_plan, owner wait, Pending attach |
| L2 | 종료가 허가되고 idle·증거 조건이 지속되면 자식은 Retired에 도달한다. | 기존 주기 pass → 신규 idle 종료 |
| L3 | L2 뒤 슬롯 reset이 공정하게 실행되면 슬롯 binding이 해제된다. | auto_queue/runtime.rs:387–414 |


C7의 구현에서는 첫 키 직전 body fence/source authority 아래 관계·owner·root·native ID·nonce·source policy와 이전 출력 정산을 다시 확인한다. 예약 실패는 키 0회, 원본 보존이다.

D1·switch off·지원 범위 밖 입력은 명시적 거절 또는 사유가 있는 보류다. 원격 장애가 영구 지속되거나 종료 권한을 계속 꺼둔 상태까지 L1~L3의 성공으로 주장하지 않는다.

4. E1~E93의 S2 행동과 소유

바꿈(공통)은 해당 caller의 의미가 공통 판정 변경을 통해 바뀐다는 뜻이다. 해당 caller에 분기 본체를 복제하지 않는다. 표의 기존 좌표는 
준비 문서 전수표와 고정 소스를 기준으로 한다.

E1~E31 — 판정·입력·생성

| E | 기존 경계 | S2 행동 / 소유 |
|---|---|---|
| E1 | thread_host.rs:24 | 바꿈(공통). O와 독립된 확인 관계를 소비. children.rs·`cutover.rs` 소유 |
| E2 | thread_host.rs:48 | 남김. 미확인 관계를 Herdr 이탈로 해석하지 않음 |
| E3 | thread_host.rs:87–117 | 바꿈. 검증된 현재 실행 기록, boot seed·retire 무효화 — [w2-P1-8] 캐시·seed·무효화 키는 (channel, provider, 정규 owner). 해당 nonce 변경만 반영. |
| E4 | thread_host.rs:137–159 | 바꿈. provider·selector·실행 증거에 따른 다음 호스트 |
| E5 | thread_host.rs:163 | 남김. 현재 실행만 읽음. E3·boot gate 소비 |
| E6 | turn_host.rs:157–187 | 바꿈. verified_bound와 다음 계획 구분 |
| E7 | turn_host.rs:191 | 바꿈(공통). 동일 관측, 기록·허가 소비 없음 |
| E8 | turn_host.rs:200–252 | 바꿈. Q1 상태표와 Pending 보류 |
| E9 | turn_host.rs:282–318 | 바꿈. 사전 eligibility·typed wait, first 소비 없음 |
| E10 | turn_host.rs:412–463 | 바꿈. child plan·late first permit·복구 입장 구분 |
| E11 | host_decision.rs:75–290 | 바꿈. 확인된 Nothing에서만 다음 호스트; probe 생략 제거 — [w2] rowless도 정규 tmux 이름으로 probe(`host_decision.rs:253-255`의 probe 전 Nothing 제거). |
| E12 | discord/host_defer_gate.rs:109,298,309 | 남김. 공통 판정을 소비 |
| E13 | intake_turn/host_refusal.rs:19 | 바꿈(공통). 새 자식 사전 허용·보류 |
| E14 | intake_turn.rs:1940 | 바꿈(공통). 최종 child 판정. giant에 새 본체 없음 |
| E15 | turn_teardown_clearance.rs:11 등 | 남김. keyed teardown 보호 유지 |
| E16 | provider_dispatch.rs:360–371 | 바꿈, 전달만. child·first·fresh를 HerdrTurn에 전달 |
| E17 | claude/tui_session_launch.rs:91–108 | 바꿈. provider-qualified 생성 gate; Herdr/Hold에서 tmux create 0 |
| E18 | claude.rs:2602 | 바꿈, 호출만. E17과 같은 생성 gate |
| E19 | Codex 생성 경계 6곳 | 바꿈, 호출 정합. provider 명시. 신규 Codex child 정책은 S4 |
| E20 | discord/codex_absence.rs:31 | 남김/S4. 분류와 생성 허가를 혼합하지 않음 |
| E21 | headless_turn/admission.rs:48–51, headless_turn.rs:993 | 바꿈. checked 관계 선행, Standing 범위 유지 |
| E22 | input_owner/claude_handoff.rs:261–286 | 바꿈 필수. ChildFresh journal 신규 개시 제외 |
| E23 | 같은 파일 :64,290,530 | 남김. 기존 root handoff와 left_herdr 복구 |
| E24 | intake_turn/owned_busy.rs:119,285,357 | 남김. Bound의 기존 owner busy 입력 |
| E25 | provider_dispatch/owner_input.rs:417,639,1159 | 바꿈(공통). Pending wait·정확한 현재 실행; 본체 추가 없음 |
| E26 | owner_input_start_wait.rs:50 | 바꿈(공통). 관측만 수행 |
| E27 | tui_followup/nameless_gate.rs:79,180 | 남김. boot gate 뒤 current 판정 소비 |
| E28 | unverified_thread_resume.rs:43–46 | 바꿈(공통). ChildFresh가 Tmux resume 보정에 진입하지 않음 |
| E29 | health/thread_turn.rs:148 | 남김. HTTP live-only 계약 유지 |
| E30 | turn_deliver/preflight.rs:20,29 | 바꿈(공통). authoritative host 판정 후 current 사용 |
| E31 | turn_deliver/inject.rs:312,440 | 남김. boot gate와 Bound 검증 뒤 주입 |

E32~E64 — 동의·관계·명령

| E | 기존 경계 | S2 행동 / 소유 |
|---|---|---|
| E32 | origin_admission.rs:19 | 남김. thread 인자가 있는 경로는 기존 prepared admission |
| E33 | herdr_consent/admission.rs:31–49 | 바꿈. 현재/다음 실행을 동일 사실로 판정, fresh child proof |
| E34 | herdr_consent.rs:31 | 바꿈. 확인된 부모 user-default endpoint |
| E35 | intake_dispatch.rs:587,617,896 | 바꿈(공통). child custody checkpoint |
| E36 | busy_inject.rs:86–92 | 남김. 보수적 제외와 owner/큐 drain 유지 |
| E37 | busy_inject.rs:103–109 | 바꿈, 조회 연결만. provider를 구별하는 next 판정 |
| E38 | busy_retry.rs:251,269 | 남김. 받은 queue/owner 계약 유지 |
| E39 | session_runtime.rs:487–638 | 남김. child 메모리 세션·worktree·cwd 준비 보존 |
| E40 | checked relation 기록 호출들 | 바꿈(공통). O candidate와 무관하게 관계 기록 |
| E41 | children.rs:476, cutover.rs:276, surface_policy.rs | 바꿈. relation accessor 사용, 기존 callback 경계 유지 |
| E42 | provider_hosting.rs:247 | 남김. 확인된 정책 채널 소비 |
| E43 | control/herdr_recovery.rs:30 | 남김. 부모 정책과 자식 canonical row |
| E44 | clear_teardown/resume_fence.rs:23–24 | 첫 커밋에서 바꿈 |
| E45 | clear_teardown.rs:72–81,148 | 첫 커밋에서 바꿈. 저장 행 우선, 미확인 fallback 금지 |
| E46 | clear_teardown.rs:126 | 남김. own canary 전용 |
| E47 | clear_report.rs:147 | 바꿈(공통). 정확한 child consent 정산 |
| E48 | execution_cleanup.rs:272 | 남김. 명령의 current/next 판정 소비 |
| E49 | provider_settings.rs:211 | 남김. Tmux에만 기존 relaunch |
| E50 | control.rs:396 | 남김. 동일 명령 판정 소비 |
| E51 | restart.rs:291 | 바꿈(공통). 빈 fresh child는 Herdr vacant 경로 — [w2-P1-7] vacant에서 `ingress::adopt` 뒤 seed. |
| E52 | admin_host_guard.rs:96–140 | 남김. Herdr/Hold의 tmux reset 금지 — [w2-P1-3] 확인된 vacant의 dispatch provider reset은 거절하지 않고 fresh 의도. |
| E53 | admin_host_guard.rs:18,29,289,322–325 | 남김. 행·marker·inflight 보호 |
| E54 | /stop → judged_stop.rs | 남김. spawn 때 정한 실제 Herdr cancel token; 실기 수용 필수 |
| E55 | root_replaced.rs:249–262 | 남김. 부모 endpoint와 행 위치 대조 |
| E56 | root_replaced/preclaim.rs:24,50 | 바꿈(공통). 부모 server-replaced 증거 |
| E57 | herdr_terminate.rs, restart_recovery.rs | 남김. exact 종료 엔진; idle 전용 요청은 child에서 추가 |
| E58 | effort.rs:135–142 | 남김. 같은 판정을 E59·E60에 전달 |
| E59 | effort.rs:146–205 | 첫 커밋에서 바꿈 |
| E60 | effort.rs:257–262, tui_passthrough.rs:94,98 | 첫 커밋에서 바꿈 |
| E61 | tui_passthrough/herdr_settings.rs:20–80 | 남김. 실제 Bound target 확인 |
| E62 | provider_settings.rs:194,237–263 | 남김. Tmux일 때만 기존 model relaunch |
| E63 | provider_settings.rs:119–131 | 남김. 결정된 실행 노드/명령 대상 사용 |
| E64 | fast_mode.rs, provider_settings.rs:321–335 | 남김. 기존 설정 저장·현재 실행 적용 계약 |

E65~E93 — 정리·복구·출력·배치

| E | 기존 경계 | S2 행동 / 소유 |
|---|---|---|
| E65 | session_idle_cleanup.rs:75–87 | Herdr skip 유지. Herdr 자식 해제는 주기 thread_lifecycle이 소유 |
| E66 | session_locator.rs:185 | 바꿈(공통). fresh child에서 old tmux 관측 생략 |
| E67 | tmux_watcher/host_gate.rs:153 | 남김. Herdr/Hold kill 금지 |
| E68 | watchers/lifecycle/watch_host.rs:30–77 | 남김. 행·marker 기준 |
| E69 | watchers/lifecycle/restore.rs:829 | 남김. keyed startup teardown |
| E70 | tmux_reaper/host_guard.rs:75,92 | 남김. keyed 보호 |
| E71 | session_resume.rs:406–413 | 남김. 기존 selector Tmux 보존, Herdr/Hold 차단 |
| E72 | herdr_reader.rs:184–218 | 바꿈. 복구 후 최신 행, seed·주기 idle 진행 — [w2-P1-5] 소비자 앞 exact retire·owner 단위 Hold. |
| E73 | herdr_reader.rs:363–393 | 바꿈. 부모 endpoint·기존 nonce 복구 |
| E74 | herdr_server_replaced.rs:27–44 | 바꿈(공통). 자식의 부모 서버 demand/hold |
| E75 | hosted_provider 소비자 | 남김. provider switch 의미 유지 |
| E76 | hosted thread GC·migration 0131 | 남김. Legacy/Retired만 삭제, live Pending/Bound 보존 |
| E77 | src/db/dispatched_sessions.rs:2086,2123 | 남김. 기존 Legacy selector 삭제 의미; 조회 정본은 Q2에서 확장 |
| E78 | auto_queue/runtime.rs:498–507 | 바꿈. future-host가 아닌 actual-release clearance — [w2-P1-6] 해제 판정 뒤 archive. |
| E79 | 같은 파일 :555–590 | 바꿈. Retired 전용 exact 정산, Legacy gate 유지 — [w2-P1-6] `managed_reset_verdict` 분기 포함. |
| E80 | tui_prompt_relay.rs:754 | 남김. boot seed 이후 current 소비 — [w2-P1-8] provider·source identity를 넘겨 provider별 현재 실행 조회. |
| E81 | synthetic_start/admission_fence.rs:168 | 남김. boot gate·source 보호 아래 동작 — [w2-P1-8] 같음. |
| E82 | tui_prompt_dedupe/hosted_source.rs:63 | 남김. 기존 복수 hosted 증거 유지 |
| E83 | unbound/herdr_bound.rs:19, completed_scheduled.rs:162 | current는 남김, next 호출은 provider를 구별하도록 연결 |
| E84 | unbound.rs:305,534 | 남김. 동적 자식은 앞선 binding 필터로 제외 |
| E85 | tui_o/writer/actor.rs:462–470 | 남김. boot gate 뒤 현재 실행, 미정산 출력 보호 — [w2-P1-8] 같음. |
| E86 | writer/input_notices/admission.rs:131 | 남김. boot gate 뒤 current fence — [w2-P1-8] 같음. |
| E87 | writer/rotation/readers.rs:806 | 남김/S4. Codex 전용 |
| E88 | discord/queue_dispatch.rs:253 | 남김. own canary 자동 개입 |
| E89 | herdr_launch.rs:550 | 바꿈. 자식 owner와 immutable child context로 Pending 설치 — [w2-P1-4] command 확정 미전송·불명 구분을 Pending 정산이 소비. |
| E90 | launch_host.rs:619, model.rs:238 | 바꿈. parent-workspace TabCreate — [w2-P1-4] `create_checked` 결과 구분은 S2b에서 먼저 착지. |
| E91 | herdr_launch/unplaced.rs:229,316 | 바꿈. tab marker·tab residue·기존 nonce 복구 |
| E92 | herdr_gate.rs:225 | 남김. 전달받은 endpoint·nonce의 기존 fence |
| E93 | first_input.rs:108–168,269–333 | 바꿈. owner effect 안에서 첫 키 전 보호 예약 — [w2-P1-1·2] 예약은 공통 write, 첫 키는 Bind 앞. |


E1~E93 모두 처분했다. S4로 남기는 것은 E20·E87의 Codex 신규 자식 관련 의미이며, E19의 공통 생성 경계 정합성과 E59·E60 수리는 S2에서 처리한다.

5. 조각표 — 파일·추정 줄·착지 순서

> **[w2]** 이 절의 추정은 w1 값이다. w2 재추정은 앞의 "조각·크기 재추정(w2)"와 "범위 선택지"를 따른다.

아래 수치는 설계 추정이다. 측정 결과가 아니다. 실제 파일 수·순증·누적치는 pr_cap_prod.py 출력으로 확정한다.

| 순서 | 조각 | 주 소유 파일 | prod 추정 | 경계 |
|---|---|---|---|---|
| 1 | S2a 첫 커밋 | clear_teardown.rs, resume_fence.rs, turn_host.rs, effort.rs, tui_passthrough.rs | 5파일, +70~120 | 요청된 세 수리와 필수 뮤턴트 |
| 2 | S2a 나머지: 현재 소비자·판정 정합 | DB hosted_execution.rs; thread_host.rs, host_decision.rs; 신규 session_host/thread_launch.rs; typed 생성 gate와 기존 Claude/Codex 호출부; channel_policy.rs, children.rs, cutover.rs; 공통 consent endpoint | 첫 커밋 포함 약 22~26파일, +350~600 | 현재 관측을 공통화. 새 fresh 분기는 아직 켜지 않음 |
| 3 | S2b: 부팅·복구·해제 | runtime_bootstrap.rs, runtime_bootstrap/o_writer_host.rs; herdr_reader.rs, 신규 herdr_reader/thread_lifecycle.rs; herdr_source.rs; herdr_terminate.rs, 신규 herdr_terminate/idle.rs; auto_queue/runtime.rs; 필요한 DB 후보 조회 | 약 9~12파일, +350~600 | 실제 시작 gate와 주기 생산 caller에 연결. 신규 활성화 없음 |
| 4 | S2c: 배치·첫 입력·활성화 | turn_host/`thread_launch`; herdr_launch.rs, prepared.rs, policy.rs, unplaced.rs; Herdr model.rs, contract.rs, launch_host.rs; herdr_gate residue 추출; Claude herdr_turn·`handoff`·`pending`·`first_input`; claude_thread.rs; 두 dispatch/handoff 전달부; redirect·headless admission | 약 25~29파일, +650~1100 | tab create부터 새 fresh 판정까지 원자 착지. 유일한 활성화 조각 |


누적 고유 prod 파일은 현재 소유 계획상 40대 후반 수준, 누적 순증은 약 +1350~2300으로 예상한다. 이는 기존 단일 조각 추정의 갱신이다. 중복 수정 파일과 순수 이동을 정리한 실제 누적값은 구현 manifest와 pr_cap_prod.py로 공개한다.

다음 경계를 지킨다.

S2a·S2b에서 “실행 없음 → Herdr fresh”나 first permit을 미리 켜지 않는다.
S2b는 부모 endpoint·owner wait·seed를 수리한다. 새 tab marker 해석은 S2c의 배치 변경과 함께 들어간다.
provider_dispatch.rs, owner_input.rs, 두 handoff giant, intake_turn.rs에는 필드 전달·호출 교체만 둔다.
herdr_gate residue는 기존 블록 이동 후 작은 child에서 확장한다.
S2c가 +1200에 도달하면 착지를 멈추고, 기존 생산 caller가 있는 수리를 선행 조각으로 재분리한다. 호출자 없는 tab-only 조각으로 나누지 않는다.
S2 kill-switch는 기존 herdr_turn_enabled다. 새 env 시험 스위치·prod mutant 함수·전환 전용 모듈은 만들지 않는다.
6. 수용 시험·뮤턴트

모든 시험은 아래 생산 경계를 통과해야 한다. helper의 반환 bool만 확인하는 시험으로 대체하지 않는다. 뮤턴트는 시험용 source patch 또는 모델 Mode로 만들며 제품 코드에 넣지 않는다.

T-S2-1~10 확정

| 시험 | 지키는 계약 | 수정 전 FAIL 조건 | 생산 진입 경계 | 원인 제거 뮤턴트 |
|---|---|---|---|---|
| T-S2-1 새 child | Herdr Pending→Bound→첫 제출 1회, tmux create 0 | 현재 turn_host:242가 Tmux 선택 | I1/I2 → intake → owner → for_turn → driver → socket create·first input | fresh 분기를 Tmux로 복원 — [w2-P1-1] 경로를 Pending(placed) → 예약 → 첫 제출 1회 → transcript → Bound로 고침. 첫 Enter 전 transcript 없음, 정상 시작·첫 키 전 재시작. 뮤턴트 Bound 선행 요구 |
| T-S2-2 FreshAfterKill | Legacy 잔존과 무관한 Herdr fresh, S3 0 | Legacy+tmux Missing+selector 없음에서 Tmux | 위 경로 + 실제 Legacy fixture·이전 binding log | Legacy면 Tmux / fresh 의도 제거 / child handoff 제외 제거 |
| T-S2-3 기존 selector | 기존 tmux 연속성 | base는 회귀 대조군으로 PASS. FreshSubstitution도 Tmux여야 함 | for_turn → tmux launch → prepared::resolve | selector 무시 / substitution 후 host 재판정 |
| T-S2-4 tmux 부모 | 기존 부모·자식 동작 회귀 0 | base는 회귀 대조군으로 PASS | 사람·봇·queue drain → 기존 driver | 부모 정책 구분 제거 |
| T-S2-5 첫 허가 | late first 진행, normal Pending은 ordinary readiness, 첫 키 전 출력 보호 | late child의 OWriterNotReady; child source policy 부재 | configured_plan → HerdrTurn → owner hook → first_input::write | permit 제거 / fresh Pending까지 예외 / reservation을 키 뒤로 이동 |
| T-S2-6 Pending 재시작 | 부모 endpoint 복구, 같은 원본 보류·재개 | 자기 boot만 읽어 복구 누락·terminal refusal | I7/I8/I10/I11 → owner wait → boot/주기 resolver → attach | own-boot 복원 / wait를 소비 / Adopted를 Bound로 seed |
| T-S2-7 create 응답 유실 | 기존 tab 채택, 두 번째 pane 0 | tab 경로·tab marker 투영 없음 | prepared::create → 응답 유실 → resolve_unplaced | workspace label만 검색 / create 재전송 |
| T-S2-8 idle·slot | 무입력·재시작 자식도 해제, Retired slot 회전 | Herdr skip, E78/E79의 영구 거절 | 주기 source tick → idle 종료 → slot reset | release 제거 / E79 legacy-only / exact nonce 조건 제거 — [w2-P1-6] successor 설치, Retired CAS 직후 정산 지연, Discord 조회 실패에서 map·출력 보존과 archive 0 |
| T-S2-9 boot seed | Bound가 첫 동기 읽기에서 Tmux가 되지 않음 | JUDGED가 비어 있음 | 공통 run_bot bootstrap → relay/presence/HTTP/worker | seed를 spawn 뒤로 이동 / 관계 복원보다 먼저 기록 — [w2-P1-5] 옛 root·사라진 pane 행 + 정상 tmux 채널 부팅 → 정상 채널 작업, 미확인 자식 키·create 0 |
| T-S2-10 첫 커밋 | clear·verified_bound·effort 세 계약 | Pending/Retired/Hold clear 누락, 거절을 Herdr 기록, Codex child effort 거절 | 실제 명령·`for_turn`·capability/passthrough | .is_herdr() gate 복원 / verified_bound 무시 / own-boot effort 복원 |


T-S2-10은 최소 다음 단언을 포함한다.

Pending clear: exact terminate 진입, guessed tmux 종료 0.
Retired+selector clear: 해당 selector 삭제, successor selector 보존.
Hold/Unknown clear: end_unconfirmed, 두 호스트의 destructive effect 0.
verified_bound=false gate refusal: JUDGED Herdr 기록 0.
자기 boot 없는 Bound Codex child: capability·저장·restart가 같은 현재 Herdr 판정 사용.
모델 밖과 새로 확인한 경계를 위한 추가 시험

| 시험 | FAIL을 만드는 조건 / 생산 경계 | 뮤턴트 |
|---|---|---|
| T-S2-11 배치·D1 | 부모와 자식 workspace 불일치, 부모 Missing/Unknown인데 create. 실제 SocketHerdrLaunchHost::create_checked까지 검사 | WorkspaceCreate 복원 / 부모 확인 삭제 |
| T-S2-12 공유 workspace 복구 | 부모에 다른 tab이 있을 때 자식 잔재를 해제하지 못함. 부모 이동 뒤 원래 tab을 못 찾음 | workspace pane_count=1 유지 / 현재 부모 workspace만 검색 |
| T-S2-13 네 crash 지점 | create 뒤, reserve 뒤, 첫 키 뒤, 성공 정산 뒤 재시작. I7/I8/I10/I11별 원본 payload count≤1 | 불명을 KnownNotSent로 바꿈 / terminal identity 무시 |
| T-S2-14 fresh successor | unplaced Pending 종료 후 late child가 영구 OWriterNotReady | 확정 미전송 successor 허가 제거 |
| T-S2-15 관계·headless | O 미선택·미기록 child가 Tmux로 새거나 조회 실패를 NotChild로 취급 | O-only recorder / candidate와 relation 재결합 |
| T-S2-16 redirect custody | 부모 Bound가 child proof를 대신함, 동의 후 새 original/thread 중복 | child 재판정 삭제 / custody channel 덮어쓰기 |
| T-S2-17 idle 경합 | idle 검사 뒤 새 front·native 입력·출력 custody가 생겼는데 종료 | transition/fence 안의 마지막 idle 검사 삭제 |
| T-S2-18 범위·스위치 | Codex 신규 활성화, Apart label 병합, switch off인데 create/terminate | provider·Standing·기존 권한 검사 제거 |


수정 전부터 존재하는 올바른 동작은 “수정 전 FAIL”로 꾸미지 않는다. T-S2-3·4와 기존 Codex 동작은 base 대조군 PASS, 회귀 뮤턴트 RED로 기록한다.

배포 수용 레그

구현 시험 통과 뒤 실제 Herdr+tui_o 부모 아래 새 Claude 스레드에서 확인한다.

첫 입력·후속 입력·취소;
동일 부모 workspace의 자식 tab;
10턴 동안 남은 자리표시 0, 답장 형태 0, 중복 0, 푸터 사용량;
해당 child의 legacy 본문 전송 0;
#7651 census의 새 legacy 본문 증가 0;
create 응답 유실 또는 재시작 후 동일 pane 복구;
idle 종료 허가가 켜진 상태에서 기존 1시간 기준의 해제·슬롯 회전.

DoD 3·4의 실측을 단위 시험이나 모델 GREEN으로 대체하지 않는다.

7. 상태 차트 갱신 지시
7.1 기존 모드와 기대 결과

> **[w2]** 모델의 `S2` 모드는 이제 w2 계약이다. w1 결함은 `MutW1*` 뮤턴트로 재현한다(앞의 "상태 차트(w2)" 표).

현재 보고서의 결과는 고정 SHA에 기록된 결과다. 이번 설계 작업에서 TLC를 재실행한 것은 아니다. 

다음 기대를 유지한다.

| cfg | 기대 |
|---|---|
| S2MainFresh | RED — NewThreadFollowsParent |
| S2MainAll | RED — NewThreadFollowsParent |
| S2NaiveAll | RED — NoInputRefused, FirstInputResolves, EventuallyReleased, SlotRotates |
| S2All | GREEN — 기존 6 invariant·3 liveness |
| MutS2NoPermit | RED — 첫 허가 |
| MutS2NoBootResolve | RED — Pending 복구·입력 진행 |
| MutS2NoRelease | RED — 해제·슬롯 회전 |
| MutS2TabNoMarker | RED — NoDuplicatePane |
| MutS2SelectorIgnored | RED — ExistingTmuxStaysTmux |
| MutS2ReplayResubmit | RED — SingleSubmit |
| S2SeedAll | GREEN — RestoredBeforeRead 포함 |
| MutSeedLate | RED — RestoredBeforeRead |


Main은 역사적 base 모드로 남긴다. 제품 수정 후 S2MainFresh를 GREEN으로 바꾸는 방식으로 증상 재현을 지우지 않는다.

7.2 S2 계약 모델 보강

실행 사실을 보강한다.
live tmux+selector 없음, selector 미확인, Herdr Retired+native ID를 구별한다. Selector 하나로 모든 native ID를 표현하지 않는다.

fresh 의도와 permit을 구별한다.
ordinary fresh adoption Pending과 late Deferred/parked를 나눈다. normal fresh는 예외 허가 없이 진행하며, 기존 hosted Pending에는 새 create 허가를 주지 않는다.

미전송 successor를 넣는다.
unplaced Pending→확정 Retired→다음 fresh nonce 경로를 모델링한다. 이를 제거한 MutS2NoFreshSuccessor는 FirstInputResolves에서 RED여야 한다.

Resolve 추상화를 명시한다.
현재 Resolve→Bound는 생산 코드의 “위치·증거 채택→own start/source→bind”를 묶은 것이다. 모델을 세분화하거나 설명과 대응표를 보강하고, 채택만으로 seed하는 뮤턴트를 별도로 둔다.

current seed와 launch 계획을 구별한다.
fresh 계획·미검증 gate refusal이 current Herdr를 기록하지 않도록 한다. verified_bound 무시 뮤턴트를 추가한다.

슬롯은 현재 release 상태를 본다.
Retired의 다음 호스트가 Herdr여도 슬롯 해제는 가능해야 한다. 미래 정책으로 다시 막는 뮤턴트는 SlotRotates에서 RED여야 한다.

재생 불명 구간은 별도 cfg로 다룬다.
실제 payload 이후 receipt/정산 전 crash를 Unconfirmed로 표현한다. 자동 재제출을 막는 safety와, 외부 확인 없이 성공을 확정할 수 없는 구간을 구별한다. 기존 S2All의 정상 복구 liveness를 약화해 뮤턴트를 통과시키지 않는다.

7.3 모델과 생산 시험의 경계

| 항목 | 검증 위치 |
|---|---|
| host 선택·Pending 재시작·입력 횟수·해제·slot | TLA와 생산 경계 시험 모두 |
| parent workspace·tab 응답·marker·공유 workspace 잔재 | protocol fixture 기반 생산 시험 |
| source policy·첫 키 전 reservation·자리표시 | 생산 writer/owner 시험과 배포 레그 |
| Codex | 기존 동작 회귀 시험. 신규 child 활성화는 S4 |
| routine Process·Apart identity | S1r 범위. S2는 경계와 원본 보존 시험 |
| 동의 카드와 redirect custody | 생산 consent/owner 시험 |


liveness cfg의 정상 운영 전제에는 local·허용 endpoint, 부모 배치 가능, 회복 가능한 O 준비, 공정한 worker 실행과 종료 권한을 적는다. 종료 권한 off는 pane·selector·slot 보존이라는 별도 safety 시험으로 검증한다.

8. [U] 미확인 목록

| 항목 | 미확인 내용 | 설계·착지 처리 |
|---|---|---|
| U1 실제 Herdr 실행 | protocol 22의 tab create·응답 유실·부모 이동 복구를 실제 서버에서 실행하지 않았다. wire 명세는 고정 fixture로 확인했다. | T-S2-7·11·12 및 배포 레그에서 확인 |
| U2 실제 diff 크기 | 구현 전이므로 prod 파일 수·순증은 추정이다. | 각 조각과 누적 범위를 pr_cap_prod.py로 측정. +1200 정지선 적용 |
| U3 모델 재실행 | cfg·TLA·보고서를 대조했으며 TLC를 이번 작업에서 재실행하지 않았다. | 모델 변경 뒤 전체 기존 cfg와 신규 cfg를 실행하고 expected/report 갱신 |
| U4 I20의 생산 연결 | Decision deliver_once API에서 thread first-start가 가능한 경로는 확인했지만, 고정 SHA의 실제 생산 Normalizer 호출부까지 확인하지 못했다. | “첫 입력 불가”로 제외하지 않는다. common headless 관계·판정 시험에 포함 |
| U5 운영 설정 | 실제 부모 O 선택, local endpoint, 기존 terminate 권한의 운영값은 읽지 않았다. | 배포 수용 전제 확인. 미충족 상태에서 liveness 완료를 주장하지 않음 |
| U6 출력 실측 | 실제 10턴 자리표시·답장·중복·푸터·census 수치는 측정하지 않았다. | DoD 3·4 배포 수용 레그로 남김 |


I21의 일반 handoff/recovery는 등록된 agent binding/catalog에서 대상을 고르므로 동적 새 스레드를 발견하는 별도 입구로 확대하지 않는다. 명시 binding된 thread가 있다면 기존 canonical 경로가 공통 판정을 소비한다(discord/agent_handoff.rs:544–586, agent_recovery/durable/operations.rs:51–55).

위 [U]는 구현·배포 증거로 닫을 항목이다. FreshAfterKill의 의미, S3 제외, 배치 방식, 복구·해제 책임, 범위와 분할에 남은 제품 결정은 없다.

VERDICT: DESIGN_READY
