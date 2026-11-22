# 외부 입력의 스레드 직접 전달 설계 — #8125

델타 설계 2의 기준은 main `ad04f6f3ecc33e86e7dc0968822ce2ce3f087e90`(#8184 포함), 조사일은 2026-11-16 KST다. 설계 1 `2b142514b6399f6fecb9014711085ac4bf6ee299`의 계약을 유지하며 선행 조건과 실행 경로를 다시 고정했다. #8118 O-b는 `c7c57a73ce`를 읽기만 했고 병합하지 않았다. 좌표는 별도 표기가 없으면 main `ad04f6f3ec`에 고정한다. 이 산출물은 설계·문서·실행 가능한 유한 모델이며 생산 Rust, 운영 설정·DB·프로세스, 저장소 밖 스킬을 변경하지 않았다. Rust 시험과 실제 아이메시지 수용은 구현·배포 뒤에 수행한다.

정본 입력은 [#8125](https://github.com/itismyfield/AgentDesk/issues/8125), 선행 [#8115](https://github.com/itismyfield/AgentDesk/issues/8115), [#8118](https://github.com/itismyfield/AgentDesk/issues/8118), 호스트 차이 추적 [#8128](https://github.com/itismyfield/AgentDesk/issues/8128) 및 [통합 설계](session-host-parity.design.md)의 T1/T2 계약이다. 이 문서는 그 계약을 고정 main에서 구체화한다. T1의 선행을 bound channel에 필요한 main 계약으로 좁히며, T2a의 live local 실제 iMessage 수용을 T2b와 독립시킨다. 권한·출력 상속·exactly-once 계약 변경은 없다. 스레드 호스트·출력 상속과 검증된 surface 생산 경계는 #8118 정본을 소비하며 별도 판정기를 만들지 않는다.

델타 설계 3은 main `d40c06eb47a358373a740b8596144640a3b8ba63`, lane base `2c359680117efb2e898c50a26524cf570213b1c0`에 고정한다. 아래 N08–N14와 §4의 종료 계약은 이 main 좌표다. T1의 first-key 이후 종료 권한 누락을 보완한다. 이번 커밋은 문서·모델만 포함한다. 생산 초안 13파일은 보존 객체 `7047b81b7daa5c487e2f1b8f365f202b7a08913e`와 바이트 동일하게 작업 트리에 남긴다. 선택은 **G: 실제 bound-channel 전달 gateway + 기존 O 본문 소유권**이다. 종료 admission 완화는 하지 않는다.

## 1. 결과와 경계

두 API가 현재 에이전트에 묶인 부모 아래의 실제 Discord 스레드를 목적지로 허용한다. provider·설정·런타임 선택은 부모, 세션·mailbox·original·origin receipt·물리 입력·답 게시 목적지는 실제 자식이다. 살아 있는 자식 실행은 기존 경로로 입력을 받는다. T2b에서 자식 세션이 없으면 Discord 첫 스레드 메시지와 같은 복원·bootstrap·배치 경로를 거친 뒤 원본을 한 번 제출한다. bootstrap은 준비만 맡고 payload를 별도 제출하거나, 기존 시작 경로가 payload를 소비했다면 그 결과를 그대로 반환한다. 두 경우를 겹쳐 제출하지 않는다.

확정된 남의 채널·스레드, 실제 thread가 아닌 임의 대상, parent가 확정적으로 없는 대상은 403이며 입력·큐·GUID admission·bootstrap·unarchive·launch 부작용이 없다. 관계 조회 실패·timeout·상충은 통합 설계대로 `503 {"status":"thread_parent_unverified","retryable":true}`로 거절한다. 조회 실패를 부재로 바꾸어 허용하거나 403으로 압축하지 않는다. 후속 재시도는 같은 origin을 유지하며 새 자동 전송기는 만들지 않는다.

한 번 받아들인 논리 입력은 소유 경로를 하나만 갖는다. 전달되면 세션 입력 효과가 정확히 한 번이고, 전달할 수 없으면 사용자에게 보이는 실패로 종결한다. 성공 입력의 중복·유실은 없으며 실패를 성공으로 세지 않는다. 원본이 없는 상태에서 영구 대기를 PASS로 취급하지 않는다. 같은 문자열의 별개 입력은 별개이며, HTTP 재시도 dedupe에는 기존 origin 계약이 필요하다.

### 이미 main에 있는 것과 남은 것

| 항목 | main 증거 | 이번 처분 |
|---|---|---|
| #8115 preferred node의 새 스레드 배치 | `135a168e60`이 기준 main의 조상. `src/services/cluster/execution_capacity.rs:54,57,63`, `src/services/cluster/intake_router_hook/placement.rs:24,97` | 재구현하지 않음. 공개 headless 시작은 현 placement를 우회하므로 T2가 기존 admission/placement seam에 연결한 통합 시험으로 소비 |
| O-a child reader·policy parent 준비 | 기준 main의 `src/services/session_host/surface_policy.rs:6,27`, `src/services/tui_o/channel_policy/children.rs:1,30,152` | 준비 단계. production surface 생산·host 상속·출력 membership 완료로 읽지 않음 |
| 내부 owner-channel/actual-turn-channel 분리 시작 | `src/services/discord/health/headless_turn.rs:100,124,128` | 공개 API target에 연결해 재사용 |
| busy Herdr Claude 입력 | `src/services/discord/health/turn_deliver/inject/herdr.rs:41,79` | 기존 exact pane·receipt 경로 재사용 |
| #8184 Herdr Idle delegation·owned Busy probe | `src/services/claude_tui/host_input/busy/herdr.rs:210,253,277,288`; `src/services/discord/router/message_handler/intake_turn/owned_busy.rs:150,165,174,260` | public readiness proof를 잠시 예약·반환하는 Idle 판정과 exact source Busy/held probe를 재사용. 직접 idle key 주입·별도 wake 구현0 |
| 공개 API thread target | `src/server/routes/agents_turn_target.rs:65`에서 직접 bound 밖 403 | T2에서 남은 기능 구현 |
| external Herdr idle 시작 | `src/services/discord/router/message_handler/headless_turn.rs:159` → `src/services/turn_host.rs:191` | T1에서 기존 executor까지 연결 |

<!-- doc-path-refs: external 99_Skills/ imsg_send.py imsg_inbound.py imsg_fork.py -->

## 2. 진입점 전수표와 모델 값의 정본

이 절의 표가 값 범위의 유일한 설명 정본이다. 모델 config는 실행에 필요한 값을 선언하되 다른 보고서에서 독립적인 범위를 만들지 않는다. 모델의 공통 인계는 하나의 받아들인 ticket을 투영하며, 별개 요청 간 DB dedupe·다중 original 경합은 실제 진입점 시험으로 보완한다.

| ID / 진입점·file:line | 현재 판정·효과 | 목표 및 모델 값·행동의 코드 근거 |
|---|---|---|
| E01 실제 라우트 `src/server/routes/domains/runtime.rs:133,135`; `src/server/routes/agents.rs:658`; `src/server/routes/agents_turn_deliver.rs:133` | start와 deliver가 공통 target resolver에 진입 | 두 endpoint 공통 권한·actual child 인계. 단일 ticket은 두 route가 공유하는 하위 흐름의 투영 |
| E02 호출자·본문 `src/server/routes/agents_turn_deliver.rs:124,148,160,174`; `src/server/routes/agents.rs:659,742`; `src/server/routes/auth.rs:213,260` | deliver의 HumanInputCaller와 명시 author, start의 기존 서버 인증·metadata agent 일치 검사 | route마다 현재 인증 의미 보존. start에 deliver 전용 author/extractor가 이미 있다고 가정하지 않음. API source 문자열은 agent/parent 권한 증거가 아님 |
| E03 agent binding·provider `src/server/routes/agents_turn_target.rs:15,36,65,107,117` | agent 존재·DB bound 비교, provider mismatch 거절. binding 조회 오류도404로 합치는 기존 동작은 넓히지 않음 | `Target`은 boundChannel/boundThread/foreignThread/foreignChannel/parentUnavailable/parentMalformed. foreignThread는 wrong parent·확정 missing parent·invalid kind 거절을, parentMalformed는 손상·상충 관계의 미확정 거절을 대표. boundChannel은 기존 경로 제어군 |
| E04 thread 관계 `src/services/discord/session_runtime/channel_routing.rs:276,280,296,300`; `src/services/session_host/surface_policy.rs:6,27` | Option helper는 오류를 None으로 합침. checked helper는 오류 보존 | checked Discord 실제 thread kind+parent와 요청 agent의 현재 bound parent 비교. parentUnavailable은 조회 실패·timeout·상충. checked helper는 부모 객체의 이름 조회 성공을 보장하지 않으므로 추가 부모 조회가 필요하면 그 실패도 거절 |
| E05 runtime 선택 `src/services/discord/health/runtime_resolve.rs:150,167,188,191,200,233`; `src/services/discord/health/headless_turn.rs:100,124` | explicit child 우선, worker는 checked parent 후보, gateway는 live probe | 검증된 owner parent에서 runtime 선택, turn은 child 유지. 조회 실패를 임의 단일 runtime fallback으로 넘기지 않음 |
| E06 Discord 첫 메시지 `src/services/discord/router/intake_gate.rs:733,741,748,757` | child restore, 없으면 parent restore·path로 bootstrap | `InitialSession`은 live/absent/booting. `Authorize` 뒤 `Prepare`가 필요하면 이 기존 순서를 재사용. core 비어 있음만으로 absent 확정 금지 |
| E07 thread session 준비 `src/services/discord/session_runtime.rs:505,513,527,532,562,596,602,631` | actual child entry 재확인, 저장된 worktree 재사용 후 fresh 생성. worktree 실패 시 parent path fallback은 main 기존 동작 | 준비와 payload 효과 분리. 실제 native launch1은 core entry1만으로 증명하지 않음. #8118이 host 상속·Herdr 준비를 정본화. 새 cleanup/fallback0 |
| E08 dispatch thread 접점 `src/services/discord/router/message_handler/intake_turn/adk_thread.rs:4,61,126,166`; `src/services/discord/router/message_handler/intake_turn.rs:33` | target redirect, 접근성·bootstrap 접점, claim 뒤 route publish | adk_thread 단독 호출은 Discord 첫 메시지 전체가 아님. API 기존 thread 지정에 dispatch thread 생성·부모 queue edge를 불필요하게 합성하지 않음 |
| E09 child claim/transition `src/services/discord/session_transition.rs:165,198`; `src/services/discord/router/turn_start.rs:615`; `src/services/discord/router/message_handler/headless_turn.rs:226` | channel별 transition과 reserved mailbox claim | 같은 actual child로 serialize 후 세션·holder 재확인. lookup은 무거운 core lock 밖. race loser의 원본도 기존 queue/owner로 반환 |
| E10 human delivery `src/services/discord/health/turn_deliver.rs:36,70,129,151`; `src/server/routes/agents_turn_deliver.rs:236,268`; `src/services/discord/health/turn_deliver/prepared.rs:1,9,60`; `src/server/routes/agents_turn_deliver/legacy_origin.rs:20,31,37,54` | remote author preflight, exact origin admission, 효과 없는 prepared future 생성 후 실행 | child bootstrap/restore/worktree 효과는 GUID admission 뒤 returned future 또는 accepted owner에서만 실행. no-origin 호출은 별도 request identity이며 재전송 dedupe를 약속하지 않음 |
| E11 start·기계 입력 `src/services/discord/router/message_handler/headless_turn.rs:125,144,159,203,932,1073`; `src/services/discord/queue_io/input_owner/start.rs:18` | replay/owner admission 후 generic start는 자기 턴; iMessage identity는 사람 입력 분류 | API라는 이유로 전부 사람 입력으로 바꾸지 않음. 기존 origin/source/token/obligation 유지 |
| E12 tmux 입력 `src/services/discord/health/turn_deliver/inject.rs:309,312,385,427,644,656,694`; `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:561,607` | idle transcript fast path와 busy owner 입력, transition·row·mailbox 재관측 | `Submit`은 기존 actor/physical grant 아래 한 번. NotSent만 handback, 불명 effect는 blind replay 금지 |
| E13 Herdr 입력/시작 `src/services/discord/health/turn_deliver/inject/herdr.rs:41,47,79`; `src/services/turn_host.rs:151,177,191,198`; `src/server/routes/agents.rs:892`; `src/server/routes/agents_turn_deliver.rs:345`; `src/services/discord/router/message_handler/provider_dispatch.rs:275,338,343,549,592` | busy Claude exact pane 주입 존재. idle start는 ExecutorNotWired 거절; 뒤 executor는 이미 있음 | `actualHost`/`desiredHost`는 tmux/herdr. 살아 있는 old tmux는 parent H라도 보존. 정책을 actual locator로 치환하지 않음 |
| E14 부팅·복원 `src/services/discord/recovery_engine/restore_inflight.rs:684,729,956`; `src/services/discord/watchers/lifecycle/restore.rs:198`; `src/services/discord/health/turn_deliver.rs:129`; `src/services/discord/session_runtime.rs:562` | DB/inflight/queue/source로 세션 복원, prior-process H는 hold | 모델의 bounded crash와 `Restart`/`ShowFailure`는 custody를 유지하거나 실제 보이는 실패로 종결. ready 재관측은 `Ready` 포트. 실제 큐 파일 원자성은 기존 입력 모델·시험 계약 |
| E15 깨우기·종결 `src/services/tui_o/writer/host.rs:900`; `src/services/discord/runtime_bootstrap/o_writer_host.rs:397,402`; `src/services/discord/queue_io.rs:370,379`; `src/services/discord/runtime_bootstrap.rs:35,41`; `src/services/discord/session_transition.rs:132,145,153`; `src/services/discord/queue_io/input_owner/drive.rs:88,106,118`; `src/services/discord/queue_io/backstop.rs:672,707,733` | writer ready·active reconcile·transition release·turn event에서 actual child owner 깨움 | 모델 ready/submit/reply 공정성은 이 기존 owner 이벤트를 포트로 둠. 무작정 timer가 풀린다고 가정하지 않음. 원본 custody/queued 보장은 기존 InputOwnership/F7 모델과 연결 |
| E16 출력 `src/services/tui_o/channel_policy/children.rs:1`; `src/services/discord/router/message_handler/headless_turn.rs:1073` 이후 기존 producer | child의 writer/receipt/실제 native source로 게시 | `Reply`는 child에 한 번. #8118 O-b 생산 활성 전에는 이 포트 완료를 주장하지 않음. legacy 개별 수리0 |
| E17 stop·cancel `src/services/discord/commands/stop.rs:39,75,134`; `src/services/discord/herdr_terminate.rs:185`; `src/server/routes/agents_stop.rs:28,39,68,99`; `src/server/routes/queue_api.rs:209,226,230`; `src/services/discord/commands/control.rs:616,641,672,715,753` | 기존 cancel token·exact host settlement, waiting input 취소 | 보유 input이 기존 취소/실패로 끝날 수 있음. parent/sibling 취소0. agent stop에 새 thread override 권한0. 새 close/kill 권한0 |
| E18 clear·restart·queue reset `src/services/discord/commands/control/clear_teardown.rs:59,70,81,94`; `src/services/discord/commands/restart.rs:150,216,293`; `src/services/auto_queue/runtime.rs:485` | clear는 transition을 사용하되 `control.rs:524` timeout overtake와 `clear_teardown.rs:191` guard 해제→terminate 재획득이 있음. restart는 seed 시작, H 일부 관리 거절 | shared target/host 정책과 경합 시험. 관리 동작 parity 자체는 #8128; thread actual identity는 #8118 H2 소비 |
| E19 결과 관측 `src/server/routes/agents_input_result.rs:56,71`; `src/services/agents/input_result.rs:69,79,90,102` | read 결과는 bound channel만 검색, delivery status는 소비 증거 아님 | 현 API의 child origin 404를 유실0 증거로 쓰지 않음. 본 설계는 read 권한 확장을 몰래 추가하지 않으며 child queue/inflight/native/posted body를 직접 join |
| E20 placement 실제 경로 `src/services/discord/router/intake_dispatch.rs:229,357,413,442`; `src/services/cluster/intake_router_hook.rs:388,392`; `src/services/cluster/intake_router_hook/placement.rs:97` | Discord는 IntakeSubmission을 admission하여 local permit 또는 durable routed outbox/worker로 보냄. #8115는 이 NoOwner 배치에서 소비 | cold/remote API도 기존 admission/placement를 지나야 첫 메시지와 같음. bootstrap→공개 headless 직결만으로 #8115 소비를 주장하지 않음. 준비 포트는 placement/forward/restore 성공 또는 visible failure를 추상화 |
| E21 routed provenance `src/services/discord/router/message_handler/intake_turn/worker_entry.rs:14,130`; `src/services/cluster/intake_router_hook/model.rs:132,161`; `src/db/intake_outbox.rs:30,76`; `src/services/cluster/intake_worker.rs:648`; `src/services/cluster/intake_worker/o_input.rs:80` | 현 DTO/outbox/worker는 Discord user_text/reply context 등을 운반하며 API headless source/origin/metadata/reserved ID를 위한 필드는 없음 | 기존 transport에 optional typed ingress context를 왕복 적응. 임의 text/reply_context/execution_requirements JSON을 API provenance로 해석하지 않음. absent는 현 Discord/legacy 의미 보존. 다중 node DB/네트워크 원자성은 모델 포트 밖 시험 |
| E22 현 headless placement 우회 `src/services/discord/health/headless_turn.rs:351`; `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:649,666,678,937,940`; `src/services/routines/agent_executor/launch.rs:198`; `src/services/scheduled_messages.rs:429` | 공개 start·context가 있는 owner 입력 및 기존 internal routine/scheduled 시작은 headless로 직접 감 | 본채널·internal routine/scheduled의 기존 경로 전체를 이 이슈에서 갈아엎지 않음. 신규 thread API의 cold/remote만 E20/E21로 연결 |
| M01 모델 제어 | 현재 거절과 제안 인계를 비교 | `Mutation`은 legacy projection에서 before/none/parentOpen/doubleSubmit/dropRestart/noReadyWake/policyOverridesLive, 새 Herdr projection에서 none/herdrBefore/herdrBoot/herdrBusy/herdrMachineBusy/herdrFailure/herdrDoubleSubmit/herdrKeepReservation/herdrNoWake/herdrReclassify/herdrStaleEpoch 및 P2 변형 herdrNoIdleRecheck/herdrMachineJoinsBusy다. before는 현 target 거절, 나머지는 한 원인을 제거하는 검증용 변형이며 생산 설정이 아님 |
| M02 단일 ticket 모델의 내부 상태 / E06, E09–E16 | 요청, custody, 제출, 응답·실패와 crash의 투영 | `phase`는 request/authorized/held/queued/submitted/failurePending/uncertain/done/failed/denied/dropped. accepted/retained/visibleFailure/CanCrash는 BOOLEAN, delivered는0..2(2는 duplicate 변형만), reply는0..1, status는0/200/403/503(200은 custody 확보 뒤), session은 absent/booting/live, usedHost는 none/tmux/herdr, launches/crashes는0..1 |
| M03 동시 first-start 모델 / E06–E09, E14–E16 | 같은 child의 API start/deliver 및 Discord original 경합 | N은2/3, Ids는1..N, Mutation은 none/staleRead, phase[i]는 queued/peeked/preparing/starting/submitted/done/failed, snapshot[i]는 none/absent/live, owner는0..N, session은 absent/booting/live, launches는0..N, delivered[i]/reply[i]는0..1, failed는Ids의 부분집합. GUID 동일성은 이 대칭 ticket 모델 밖 |
| M04 Herdr 단일 external ticket / E10–E15, N02–N07 | 실제 first-key 효과와 후속 ACK, Idle read proof 반환·Busy 합류·boot/재시작을 분리 | 새 cfg의 `Target`은 boundChannel/foreignThread/runtimeUnavailable(E03/E10), `hphase`는 request/checked/denied/held/queued/probed/delegated/driverReserved/started/submitted/uncertain/failed/done, `hactivity`는 idle/busy/booting, `hprobe`는 none/idle/busy. accepted/retained/reserved/started/staleEffect/failure는 BOOLEAN, `hstatus`는0/200/403/422/503, effect는0..2(2는 duplicate mutant), reply는0..1, crashes는0..1. `hepoch`1/2와 readEpoch0/1/2는 N03/N07의 prior/current nonce·process generation을 한 번 교체하는 bounded projection이며 실제 generation 숫자가 아님. `horigin` externalAPI는 source/GUID를 보존한 API provenance 추상값이며 discordPerson은 reclassify mutant만. P2의 nativeChanged/machine/joinedBusy/idleWhileBusy는 BOOLEAN이며 N02/N04/N05의 same-incarnation 재관측·사람/machine 정책을 투영 |
| M05 Herdr same-channel 경합 / E09, N02–N07 | 이미 accepted된 imessageStart/humanApiDeliver/discordPerson 서로 다른 original3개가 같은 owner를 경합 | `N=3`, owner0..N, phase[i]는 queued/observe/idleProbe/busyProbe/delegated/driverReserved/begun/submitted/uncertain/done/failed. activity idle/busy, epoch1/2·readEpoch0/1/2, reservation·begun[i]·staleEffect·nativeChanged·idleWhileBusy BOOLEAN, restarts0..1. first-key에서 delivered[i]를 올리고 ACK는 카운트0. effect 뒤 crash는 uncertain→visible failed로만 종결. generic HttpStart Busy 대기는 별도 M04 MachineBusy cfg에서 검사 |

### 설계 1 → main 진입점 델타

E01–E22를 모두 다시 읽었다. 행 삭제는 0이며 아래 표 밖 E01–E05/E07–E10/E15/E17–E21은 위 전수표의 좌표·계약이 동일하다. E13은 좌표가 같지만 새 N01–N06을 함께 소비해야 한다.

| 바뀐 행 | 설계 1 좌표 → main 좌표 | 의미 |
|---|---|---|
| E06 | `intake_gate.rs:728,736,743,752` → `733,741,748,757` | restore→bootstrap 순서 유지 |
| E11/E16 | `headless_turn.rs:926,1067` → `932,1073` | routine presentation 초기화 추가, 외부 시작 차단159는 그대로 |
| E12 | `provider_dispatch/owner_input.rs:533,579` → `561,607` | common BusyOperation·owned probe 소비 |
| E13 | 기존 좌표 동일 | 새 Idle delegation·Busy observation·same-front wake를 N01–N06으로 추가 |
| E14 | `restore_inflight.rs:677,722,949` → `684,729,956` | routine_no_visible 분류 추가. 외부 입력을 routine으로 바꾸지 않음 |
| E22 | `provider_dispatch/owner_input.rs:621,638,650,909,912` → `649,666,678,937,940` | headless 우회 범위 그대로 |

| 새 ID | main 코드 좌표 | 값/행동의 실제 근거 |
|---|---|---|
| N01 early owned Busy | `src/services/discord/router/message_handler/intake_turn.rs:983`; `intake_turn/owned_busy.rs:9,18,29,36,38,46,89` | fresh·non-Claude·non-person 제외. observation Legacy/Idle/Busy/Held로 기존 owner를 분기. Idle은 legacy busy diagnostic을 건너뛰고 idle driver에 위임; Busy는 준비된 exact 관측으로 paste; Held는 front를 보유·watch |
| N02 late owned Busy | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:1092`; `intake_turn/owned_busy.rs:150,157,173,190,223` | early 이후 상태가 바뀌어도 같은 supplied host를 다시 관측. Busy 등록의 phase·target·source·transcript·native·Herdr writer 일치 후만 실행. Held는 NotSent |
| N03 observed host/identity | `src/services/discord/queue_io/input_owner/claude_handoff.rs:442,448,459,464,475,485` | Tmux→Legacy; Return/Refused·row 없음·비Bound→Idle(기존 driver의 수명 정책으로 위임); Unknown/owner·provider·root·channel 불일치→Held. Bound는 exact endpoint target, O owns+writer accepts 필요 |
| N04 Idle delegation | `src/services/claude_tui/host_input/busy/herdr.rs:211,215,218,223,252,258,277,280,288,292` | execution_alive, awaited_clear, JSONL Idle 또는 public readiness proof+current spawn nonce·runtime binding의 temporary reservation을 읽고 반환(not_sent). 위임 자체는 키0; 실제 Idle driver가 다시 reservation/first-key 검사 |
| N05 Busy exact write | `src/services/claude_tui/host_input/busy/herdr.rs:25,71,75,90,96,106,111`; `provider_dispatch/owner_input.rs:1470` | observation.admits를 첫 키 전/후 재관측. no-key 거절은 history withdraw; keyed 이후 불확실은 Uncertain, 재전송 금지 |
| N06 same-front watch | `src/services/discord/router/message_handler/intake_turn/owned_busy.rs:260,273,282,299,306,314`; `src/services/discord/queue_io/input_owner/held_watch.rs:62,88,97,120,138` | owner front message+queued_generation 재관측; 사라지면 Gone. unwind 후 mailbox active/자기 inflight 부재에서만 Ready. channel별 watch generation 검증 후 wake1 |
| N07 boot/restart owner | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:690,701,707`; `src/services/discord/runtime_bootstrap.rs:35,41` | durable V2 queue/history 복구·owed notice 뒤 기존 owner wake. map 없음은 fresh 실행 증거 아님 |
| 폐기한 설계1 하위 좌표 | old `src/services/discord/router/message_handler/intake_turn.rs:1751,1754,1768` → main `intake_turn/owned_busy.rs:100,115,138,145` | old inline Claude/tmux-only busy handoff는 helper로 대체. 설계1 입력 출처표의 이 좌표를 그대로 두면 전혀 다른 queue diagnostics를 가리킨다. H를 제외한 old inline 조건을 신규 H 미지원 근거로 재사용하지 말 것 |


### T1 종료·출력 진입점 — 설계 3 추가, main d40c06eb47

| ID | main file:line | 모델 포트·값 근거 |
|---|---|---|
| N08 gateway 선택·출처 | `src/services/discord/router/message_handler/headless_turn/replay_start.rs:68-97,420-455,487-527`; `src/services/discord/gateway/headless.rs:5-50,93`; `src/services/discord/gateway/headless_exact.rs:7-23,76-84,103` | 외부 start는 Headless/HeadlessExact, 직접 전달 false. source/metadata/origin ID/author/reply/sources/receipt를 같은 요청의 재시작 closure로 보존. 모델 Contract=headless/boundGateway, provenance=imessage/externalAPI; discordPerson은 오분류 변형만. |
| N09 기존 직접 전달 수단 | `src/services/discord/gateway.rs:251-289,767-773,909-932`; `src/services/discord/turn_bridge/finalize_epilogue.rs:69-88,205-223` | DiscordGateway의 live_turn=None도 direct=true, chain=false, bot owner=None. presentation과 실행 provenance를 분리할 기존 포트. 다음 입력은 deferred idle kickoff. |
| N10 native terminal 수락 | `src/services/claude/herdr_turn/live_reader.rs:25-33`; `src/services/claude/herdr_turn.rs:758`; `src/services/discord/inflight/save_store/identity_gate/runtime_stamp.rs:1330-1385`; `runtime_stamp/herdr_terminal_admission.rs:54-74`; `src/services/discord/turn_bridge/stream_loop.rs:296-314`; `turn_bridge/mod.rs:675-692` | H는 bare Done을 거르고 typed terminal만 발행. direct=false면 actor AuthorityPinned, bridge AuthorityLost로 후처리 전 반환. 모델 running→terminal→admitted/rejected. same actor/mailbox allocation/nonce/source 증거는 기존 exact gate이며 완화0. |
| N11 본문 단일 게시자 | `src/services/tui_o/cutover/channel_gate.rs:155-162`; `src/services/discord/turn_bridge/terminal_controller_cutover/o_body.rs:9-44`; `src/services/discord/turn_bridge/terminal_outcome_delivery.rs:437-443,522-547,878-943`; `turn_bridge/headless_delivery.rs:524-553,588-608` | O-owned이면 gateway 종류와 무관하게 O가 본문 소유. 비 O의 headless는 outbox channel:<actual id>, bridge는 기존 claim/receipt 경로. 모델 BodyOwner=O/bridge, posted[i]=0..2(2는 중복 변형); native body와 terminal 수락은 독립 관측. T1 H의 수용 cfg는 O만 사용하며 bridge cfg는 기존 전달 포트 제어군. |
| N12 종결·정리·wake | `src/services/discord/turn_bridge/completion_postlude.rs:767-805,932-960,1033-1040`; `src/services/discord/turn_bridge/finalize_epilogue.rs:69-88,205-223`; `src/services/discord/queue_io/input_owner/held_watch.rs:97-138` | receipt/현재 allocation 소유권을 확인한 guarded clear, successor 침범0, 기존 completion과 deferred kickoff. 모델 receipt BOOLEAN, inflight/token=0(없음)/1/2(ticket), active=0/1/2, retained BOOLEAN, wake BOOLEAN. 종료는 terminal receipt+guarded clear+token 반환 뒤 done. O 게시는 독립적으로 먼저/나중에 가능하며 canonical native source의 O custody를 유지한다. terminal receipt를 Discord 게시 ACK로 부르지 않는다. |
| N13 이미 수락된 H terminal 복원 | `src/services/discord/recovery_engine/herdr_admitted_restart.rs:23-43,90-113,124-179`; `src/services/discord/recovery_engine/herdr_row_end.rs:53-72`; `herdr_row_end/owner.rs:113-136` | prior-process durable terminal만 기존 settlement 사용. current live actor를 recovery로 우회하지 않음. 모델 epoch=1/2, captured[i]=0/1/2, restart 횟수0/1. 재시작은 admitted 이후에만 투영하며 token 공백을 fresh start 권한으로 쓰지 않음. |
| N14 tmux 외부 종료 제어군 | `src/services/claude_tui/hosting/warm_followup.rs:418-443`; `src/services/session_backend.rs:935-955,987-997`; `src/services/session_backend/stream_line.rs:344,477`; `runtime_stamp/captured_terminal_admission.rs:9-18,180-198` | tmux Claude 일반 transcript의 Done은 captured H admission 대상이 아니며 기존 headless outbox로 끝난다. captured tmux terminal도 direct=false면 거절하므로 모든 tmux 경로가 성공한다고 일반화하지 않음. 기존 결함은 넓히지 않음. |
| M06 종료 동반 모델 / N08–N14 | `ThreadIngressTerminal.tla`, accepted original 두 개(1=iMessage 사람, 2=외부 API machine), Ids=1..2 | phase=queued/running/terminal/admitted/rejected/done, effects=0..1, published=0..2, receipt/retained/bodyReady/lateCleanupTried/wake BOOLEAN. Contract와 BodyOwner는 위 실제 gateway/body owner의 추상값. Mutation=none/doublePublish/dropCustody/keepInflight/noWake/headlessAuthority/reclassify/staleCleanup. CanRestart BOOLEAN. 숫자2는 실제 queue 크기 제한이 아닌 현재 입력+다음 입력의 유한 투영이다. |
| N15 current-process execution-gone / M07 live-end 모델 | `src/services/discord/recovery_engine/herdr_row_end.rs:120-136`; `herdr_row_end_live.rs:10-25,30-88,100-147` | current-generation/no terminal/no stop receipt의 같은 actor를 transition·fresh row generation·nonce로 재관측. pinned execution E 뒤 자기 source가 terminal이면 reader admission에 양보, unread/unknown 또는 successor면 hold. terminal이 없으면 retire→body-free notice Delivered→captured settlement→completion cleanup/reader wake. 모델 Source=terminal/absent/unknown, Exact/ExecutionEnded BOOLEAN, phase=active/reader/notice/settle/settled/done/held, posted/finished=0..1, token/inflight=0..1, readerWoken/readerOwns BOOLEAN. Mutation=none/omitLiveEnd/stealTerminal/noSettlement/noWake/ignoreUnknown. 숫자1은 한 captured episode 투영이며 실제 generation 아님. |

### 입력 출처 분류표 — 전수표의 일부

| 출처 | main 좌표 | 분류/identity/소유 |
|---|---|---|
| Discord 사용자 직접 입력 | `src/services/discord/router/intake_gate.rs:733,769`; `src/services/discord/router/message_handler/intake_turn.rs:33,983`; `src/services/discord/queue_io/input_owner/origin.rs:5,26,31` | `Discord`, is_person=true. 실제 child/message original. #8184 early/late 관측 대상. bot 여부/allowed automation은 intake 분류 유지 |
| iMessage 답장 | `src/services/discord/queue_io/input_owner/origin.rs:49,55,59,62,67`; `src/services/discord/queue_io/input_owner/start.rs:21,32,43,47`; `src/services/discord/router/turn_start.rs:29,36` | source=imessage, routine_id/scheduled_message_id 없음. metadata.human_input.origin_id GUID가 있으면 child+GUID가 stable message/ticket. is_person=true. source 문자열만으로 caller/parent 권한 생기지 않음 |
| generic API start | `src/services/discord/router/message_handler/headless_turn.rs:125,144,159,203`; `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:649,676,887`; `src/services/discord/queue_io/input_owner/origin.rs:42` | `HttpStart`, is_person=false. machine own-turn으로 busy 끝날 때까지 wait; origin_id 추적값만으로 일반 HTTP retry dedupe를 추가하지 않음. `source=imessage`+사람 계약을 갖춘 경우만 별도 person 정책 |
| API deliver | `src/server/routes/agents_turn_deliver.rs:124,160,236,268`; `src/services/discord/health/turn_deliver.rs:36,59,70` | remote caller+명시 author 검증된 사람 입력. source/origin/actual child 유지. API라는 transport 이름을 Discord original로 위장하지 않음 |
| routine/scheduled 자동 입력 | `src/services/routines/agent_executor/launch.rs:198`; `src/services/scheduled_messages.rs:429`; `src/services/discord/queue_io/input_owner/origin.rs:31,42,59`; `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:887` | `Routine`/own turn; imessage source여도 routine/scheduled metadata가 있으면 imessage_identity 제외. #8184 early/late person 관측으로 강제 승격 금지 |
| EffortContinue | `src/services/discord/queue_io/input_owner/origin.rs:9,32`; `src/services/discord/queue_io/input_owner/start.rs:18` | continuation이 최우선; 기존 reservation/original 복원. 이 레그에서 API 입력으로 재분류하지 않음 |
| native TUI 사용자 입력 | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:723,727,734,887`; `src/services/discord/turn_presence/activity.rs` | native turn은 외부 API original과 별개. unmanaged/durable holder Busy/Unknown에서 자동 새 session 생성 금지 |


owner 출처는 `InputOrigin::from_context`가 복원하고, 사람 판정은 `src/services/turn_orchestrator/queued_head/v2.rs:61`의 기존 origin/headless+source·routine/scheduled 제외 조건을 따른다. GUID는 사람 판정과 별개인 `imessage_identity`의 stable ticket 조건이다. 검증된 thread target은 목적지만 고르고 출처를 바꾸지 않는다. `source=imessage`는 caller·author·agent·parent 권한을 대체하지 않는다. API를 Discord로 포장하거나 routine을 person으로 바꾸는 적응은 금지한다.

## 3. 권한과 준비 경계

`AgentTurnTarget`의 현재 provider/primary/channel 결과를 확장하여 검증된 runtime owner와 actual destination을 구별한다. 기존 `channel_id`를 부모로 덮어쓰지 않는다. 이름·alias는 기존 bound 해석을 보존하고, 직접 bound가 아닌 대상만 실제 Discord thread 관계를 확인한다. 자식은 부모 binding의 provider를 선택하고 provider override 불일치는 기존 422다. 임의 title, `metadata.parent_channel_id`, caller의 agent 이름, live child row, dispatch map만으로 권한을 만들지 않는다.

현재 resolver는 PgPool만 받으므로 route의 HealthRegistry에서 검증할 부모 runtime/HTTP를 가져오는 접점을 추가한다. DB binding을 먼저 읽고 기존 owner-channel runtime 해석을 재사용한다. HTTP/runtime가 준비되지 않았거나 관계를 검증할 수 없으면 부작용 전에 503이다. `checked_thread_parent`의 `Result` 구분과 실제 kind 검사를 #8118 생산 surface 경계에 연결한다. O-a의 `VerifiedSurface::from_discord`는 아직 시험 전용이므로 지금 호출 가능한 생산 API인 것처럼 설계하지 않는다. #8118 H1에서 정한 공통 경계를 T2가 소비한다.

서버가 보관한 검증된 parent provenance가 있으면 #8118/#8119의 기록 계약대로 소비하되 현재 agent binding과 다시 대조한다. 미기록 옛 스레드는 checked REST를 쓴다. 향후 S6 BindingContext를 이미 main에 있는 것으로 가정하지 않는다. 기록이 상충하면 거절하며 parent 조회 실패를 임의 parent 채널 실행으로 바꾸지 않는다. 부모 이름은 표시/경로 선택의 보조값이고 관계 권한의 대체 증거가 아니다.

deliver의 author preflight까지 완료하기 전에는 bootstrap·접근성 변경을 하지 않는다. 특히 Legacy origin 경로는 `legacy_origin.rs:20,31,37,54`의 preflight/replay/prepare/admission 순서를 유지한다. 이때 `prepare_external_human_input`은 효과 없는 캡처이고, 실제 child restore/worktree/bootstrap은 GUID admission 뒤 실행되는 returned future 또는 accepted owner 안에만 둔다. 같은 GUID replay는 기존 결과를 반환하며 준비 과정이 새 효과를 내지 않는다. origin admission 뒤 준비 실패는 기존 obligation/terminal 실패로 종결한다. start는 현행 서버 인증·owner/source 정책을 보존하고 deliver 전용 author 규칙을 임의로 덧씌우지 않는다.

살아 있는 검증된 local child는 기존 owner 입력을 재사용한다. cold/remote child는 E20의 기존 `admit_text_intake` 경계를 재사용하여 owner/NoOwner placement를 먼저 해결하고 local permit 또는 durable forwarding을 받는다. API용 Discord Message를 합성하거나 dispatch redirect 전체를 재호출하지 않는다. E22의 headless 직결은 현재 placement를 우회하므로 bootstrap만 호출한 뒤 headless로 보내면 #8115를 소비한 것이 아니다.

E21의 기존 DTO/outbox/worker에 source·origin·start own-turn/iMessage person·metadata·reserved ID·검증된 owner/destination 관계를 담는 작은 optional typed context를 연결한다. API 경계에서 검증한 context만 서버가 생성하며 worker는 기존 agent/owner/claim fence를 통과한 그 context로 기존 headless owner admission을 복원한다. canonical source의 선택부터 API provenance를 유지한다(`intake_dispatch.rs:243`, `intake_turn/worker_entry.rs:130`의 현재 Discord source 구성 접점). 첫 replay lookup/preflight 이전부터 이 typed 분기를 적용하고, context를 기존 `o_input.rs:80`의 take-before-effects exact request 비교에도 포함한다. API를 Discord로 강제 변환하여 replay/dedupe하는 경로를 만들지 않는다.

요청 text·reply_context·execution_requirements에 몰래 JSON을 끼우거나 Discord 입력의 source를 API로 재분류하지 않는다. context가 없는 옛 row만 기존 해석을 사용한다. context가 존재하지만 손상되거나 알 수 없는 tag, owner/child/provider 불일치인 경우에는 legacy absent로 downgrade하지 않고 물리 효과 없이 거절하며, 이미 수락된 원본이면 기존 failed/owed notice로 사용자에게 보이는 실패를 남긴다. outbox 스키마에 필드가 필요하면 nullable migration·옛 row decode·재시작 왕복을 같은 조각에서 검증한다. GUID와 reserved identity는 forward/retry에서 다시 만들지 않는다.

선택된 실제 실행 노드에서는 child restore → 기존 child/hosted row·inflight 확인 → 필요 시 parent path restore → thread bootstrap → 기존 reserved/owner start로 이어진다. 이 준비를 Discord와 API가 같이 쓰는 기존 함수 경계로 묶고, child transition 아래 current child를 재확인한다. transition의 비재진입 락을 잡은 상태로 같은 락을 다시 잡는 start를 호출하지 않도록 guard 보유 범위를 분리한다. core entry가 생겼다는 사실과 native session 시작 완료는 별도다. local gateway가 remote 배치 전에 worktree를 만들거나 prompt를 제출하지 않는다.

runtime owner의 parent 이름을 actual child의 `channel_name_hint`로 무조건 넘기면 기존 이름 해석이 부모 세션을 가리킬 수 있다. parent는 runtime/settings 선택에만 쓰고 actual 이름·session key·BindingContext·worktree는 공통 bootstrap이 만든 child 값을 사용한다. 준비 단계는 prompt를 제출하지 않는 계약을 우선하며, 이미 제출한 기존 경로의 결과는 재주입 없이 반환한다.

### 거절 효과0와 수락 뒤 상태 변화

`agents_turn_deliver.rs:256-278`는 `deliver_once(authorize=preflight_human_input)`를 쓴다. `health/turn_deliver.rs:70-87` 현재 preflight는 runtime resolve와 author 허용만 본다. `imsg_reply/admission.rs:140-159` authorize 성공 후 origin INSERT; `:172` backend; backend AuthorNotAllowed 외 Err는 `:195-197 unknown`을 저장한다. 따라서 host/live/runtime deterministic refusal을 **origin reserve 전 effect-free preflight**에서 확인해야 거절 effects0·lease0를 주장할 수 있다. Legacy는 `agents_turn_deliver/legacy_origin.rs:20-54` preflight→replay→effect-free prepared→admit 순서를 유지하며 `prepared.rs:10-17` returned future를 실행할 때만 delivery한다. T2a 관계 확인/owner runtime 선택/live gate를 preflight에 넣되 bootstrap은 하지 않는다. admission 후 late host/incarnation 변화는 기존 unknown/owed custody·visible terminal로 처리하며 raw rejection/effects0로 쓰지 않는다. replay를 다시 실행하는 대신 기존 결과를 돌려준다.


정적 target/관계/host/provider/runtime/live 거절은 origin row·mailbox lease·queue·source 등록·worktree/native IO 모두 0이어야 한다. admission 뒤 경합으로 Held/Unconfirmed가 된 입력은 이미 채택된 입력이다. 기존 durable custody·owed/visible terminal로 책임을 유지하며 이를 효과0 거절이라고 재명명하거나 새 삭제·rollback을 추가하지 않는다.

### 단일 권위 대조 — #5071

[릴레이 상태 계약](relay-state-contract.md)의 mailbox active-turn lease가 실행 권위이고 row는 파생 증거라는 원칙을 유지한다. target 검증은 권한/목적지 선택이며 실행·배달 권위를 만들지 않는다.

| 판정/행동 | 재사용하는 기존 권위·코드 좌표 | 새로 만들지 않는 것 |
|---|---|---|
| 실행 claim | `src/services/discord/router/turn_start.rs:142,161,174`의 mailbox claim·episode nonce | session map/HTTP200만으로 별도 active 권위 |
| 원본 보유·retry | `src/services/discord/queue_io/input_owner/start.rs:43,47,51`, `src/services/discord/queue_io/input_owner/origin.rs:48` 및 기존 owner queue | parent별 dedupe cache·본문 fingerprint dedupe |
| 배치/원격 인계 | `src/services/discord/router/intake_dispatch.rs:229,442`, 기존 routed outbox/worker claim | 새 transport·비영속 전달 경로·local 선제 launch |
| 물리 입력 | `src/services/discord/health/turn_deliver/inject.rs:427,644,656`, `inject/herdr.rs:41,79`, 기존 provider dispatch owner | API direct pane key 전송·ACK 불명 blind retry |
| child 답 게시 | `src/services/tui_o/writer/host.rs:201,555,1007`의 기존 writer/adoption·lease, child 실제 source | parent writer alias·API 별도 본문 게시·legacy 수리 |

## 4. 호스트별 동작과 #8118/#8128 접점

**T1은 O-b 없이 main만으로 구현·착지 가능하다(`t1_needs_ob=no`).** T1은 기존 bound channel의 switched admission을 기존 실행기로 잇는 범위다. bound writer·O-ready·mailbox/owner는 이미 main에 있으며 O-b가 추가하는 dynamic child membership·adoption·boot reservation을 요구하지 않는다. T1 구현·운영 배포와 provider switch/admission/O-ready 수용은 여전히 필요하다. #8184 머지만으로 외부 API 기능까지 완료됐다는 뜻은 아니다.

### main의 실제 차단과 재사용할 실행기

| 단계 | 좌표·의미 |
|---|---|
| 현재 차단 | `src/services/discord/router/message_handler/headless_turn.rs:147-165`는 claim/reset/clear 전 generic `turn_host::refusal_before_turn`을 부른다. `src/services/turn_host.rs:177-193`의 `TurnHost::Herdr(_)`→`:191 ExecutorNotWired`가 여전히 실제 차단이다. 현재 HTTP는 `src/server/routes/agents.rs:892`/`src/server/routes/agents_turn_deliver.rs:345`의 InvalidTarget 매핑으로 **422**다. 관계/boot/runtime 미확정의503과 구별한다. |
| 기존 intake 개방 | `src/services/turn_host.rs:198-210 intake_refusal_before_turn`은 **그 refusal만** provider switch가 켜졌을 때 없앤다. provider별 switch는 `:220-246`, configured guard는 `:281-312`(지원 provider·local endpoint·hosted read·admission·O-ready)다. refusal 전부를 없애면 안 된다. |
| owner admission | headless `:203 owner_start`; `provider_dispatch/owner_input.rs:649-685 admit_start`는 execution 확인·owner adopt·stable original·context provenance·owner drive를 재사용한다. |
| owner idle | `provider_dispatch/owner_input.rs:863-954 start_idle`: root preclaim `:881`, synthetic wait `:887-902`, owner front claim `:904`, kept headless context→`:939-943 start_owner_headless`, Discord original→`:949-952 start_owner_front`. kept headless는 다시 headless 차단을 지나므로 입구 하나의 임시 bypass로 끝내면 안 된다. |
| Busy 재관측 | `message_handler/intake_turn/owned_busy.rs:9-98 OwnerTurn::early` / `:150-195 late`: 사람+Claude+nonfresh에서 exact host 관측, Busy→prepare/paste, Held→custody/held watch, Idle→idle 실행. **early 생산 호출은 intake_turn.rs:983-991에만 있고 headless에는 없다.** 공통 late는 `provider_dispatch/owner_input.rs:1092`에서 실행된다. 외부 H start를 열 때 early/late 동등성·경합을 실제 HTTP 시험으로 증명해야 한다. |
| #8184 Idle delegation | `input_owner/target.rs:76-88` machine Busy=Held, person Busy=Claude/Codex Herdr adapter, Herdr Idle=HerdrIdle. `provider_dispatch/owner_input.rs:1303-1318` Cold→Created/HerdrIdle→Existing launch hook 설치, `:1282` driver 재진입. `:1557 idle_unowned`는 이 hook을 무시한 별도 직접 leaf가 거절되는 경계다. |
| executor | `provider_dispatch.rs:305-323` replay permit spend 뒤 owner execute; `:338-346 drive` Herdr는 `exact_submission::dispatch`로만 `herdr_turn`. `:596-647`는 execution capacity를 취득하고 Codex `:630`/Claude `:647` 기존 실행기. H→tmux/process fallback 없음. |
| GUID/출처 | `input_owner/origin.rs:25-45` person=Discord/Imessage, HttpStart/Routine 별도; `:48-72` child+GUID→stable identity. `start.rs:21-51` imessage original/retry_original. source=API라는 이유로 HttpStart를 person으로 바꾸지 않는다. |
| boot/prior process | `health/turn_deliver.rs:129-143 external_turn_holds_channel`, `:146-182 external_turn_hold(_for_start)` prior-process H/nonmanaged inflight/unknown transcript는 hold. owner recovery `provider_dispatch/owner_input.rs:688-708` 재adopt/reconcile/owed/wake. 메모리 None을 fresh로 해석하지 않는다. |


허용 범위는 외부 start/idle deliver와 그 kept-context owner 재진입이다. 관리 경로도 사용하는 `refusal_before_turn` 자체를 전역 완화하지 않는다. intake의 switched prestart 계약을 이 두 외부 접점에서 재사용하고, 실제 spawn 직전 `for_turn`과 exact submission gate를 다시 소비한다. generic start는 own-turn, iMessage person은 Busy join/Idle 위임이라는 출처 차이를 유지한다.

### 설계 3: 외부 own-turn의 출력·종료 권한

설계 2의 switched admission은 첫 키까지만 연결했다. 격리된 실제 HTTP 초안에서는 start와 idle deliver 모두 H paste+Enter1 뒤 typed terminal이 N10에서 axis=actor로 거절되어, 30초 뒤에도 mailbox token과 해당 original inflight가 남았다. `can_deliver_directly=false`는 전달 능력이 없는 HeadlessGateway의 실제 계약이다. 이를 true로만 바꾸거나 native Done을 합성하면 안 된다. `turn_bridge/mod.rs:675-682`는 AuthorityLost에서 completion·inflight guard를 defuse하고 바로 반환하므로 outbox나 후처리가 대신 끝내지 않는다.

| 같은 bound channel의 외부 시작 | tmux Claude 일반 transcript 경로(main) | Herdr typed terminal 경로(main 및 초안) | 선택 G 이후 |
|---|---|---|---|
| 실행·gateway | E11 own-turn, N08 Headless/HeadlessExact | switched admission 뒤 기존 H own-turn, gateway는 여전히 N08 | 같은 H executor, bound delivery presentation을 가진 기존 headless adapter |
| 종료 권위 | mailbox의 captured owner/token. N14의 일반 Done은 captured frame admission을 거치지 않고 bridge completion으로 감 | N10 exact actor+mailbox allocation+nonce+canonical source+file+terminal replay와 direct 능력. bare Done 대체 없음 | N10 조건 전부 유지. 실제 DiscordGateway 전달 수단이 direct 능력을 제공 |
| 게시 경로 | 비 O는 N11 headless outbox→동일 channel, visible delivery 확인 뒤 commit. O-owned는 O writer가 source에서 게시 | O writer는 독립적으로 게시할 수 있으나 terminal 거절은 live actor를 끝내지 못함 | O-owned 본문은 기존 O writer만 게시. 일반 gateway의 body claim은 O-owned에서 send0; 비 O 제어군은 기존 bridge lease/receipt만 소비 |
| 종결·다음 입력 | N12 receipt·guarded clear·completion, live context 없으면 deferred drain | AuthorityLost 조기 반환으로 N12 도달 못함. current live row를 N13 prior-process recovery에 맡길 수 없음 | terminal admission→기존 receipt·guarded clear·captured token 반환→deferred owner wake. 다음 machine own-turn은 이 반환 뒤 claim |
| 경계 | captured tmux typed terminal도 direct=false면 거절(N14). 모든 tmux 경로가 완료된다는 주장은 하지 않음 | #8184 Busy probe/Idle delegation은 입력 권한이며 terminal 권한이 아님 | tmux·관리·routine·remote 동작 변경0; T1의 checked bound H own-turn만 선택 |

| 대안 | 흐름·비용 | 위험·결정 |
|---|---|---|
| **G: bound channel delivery gateway (채택)** | checked bound H own-turn에서 기존 DiscordGateway(http, shared, provider, live_turn=None)를 실제 presentation delegate로 사용. existing headless exact adapter가 전달 메서드와 direct 능력만 위임. 작은 gateway child/adapter와 replay_start 선택 배선, HTTP 종료 시험 추가 | O 본문 단일 권위·exact terminal guard 유지. presentation만 전체 Discord intake로 교체하면 provenance/재시도 정책이 바뀌므로 아래 보존 계약을 필수화. same bound channel에 답한다는 기존 제품 목적을 유지하므로 사용자 선택 불필요 |
| A: headless exact terminal admission 완화 (비채택) | N10에서 same actor/incarnation/nonce/source가 exact일 때 direct=false도 admit. 본문은 O-owned면 O, 비 O면 기존 headless outbox. terminal 저장·복원·receipt 소비와 headless variant 시험을 모두 확장해야 함 | 직접 전달 없는 actor에 새 종료 능력을 부여. O-ready라는 사전 관측만으로 늦은 writer 중단/receipt를 대신할 수 없음. 아래 소비자마다 새 계약 필요하며 G보다 변경 경계가 큼 |
| 전체 일반 intake 재진입 (비채택) | `intake_turn/worker_entry.rs:121-175` 같은 Discord exact retry 재진입을 외부 시작에도 사용 | 별도 IntakeRequest로 source/metadata를 재구성하면 API original·machine 정책·GUID가 바뀔 위험. 기존 external OwnedRequest closure보다 크며 이 조각에서 필요 없음 |
| HeadlessGateway bool만 true 또는 Done 합성 (기각) | 가장 적은 줄 수지만 실제 send/edit는 no-op(N08), typed terminal의 source 증거를 버림 | 본문 유실·가짜 receipt·중복 게시를 가릴 수 있음. companion의 headlessAuthority 변형은 종료 권한 불변식 RED |

**G의 구현 계약:** 신규 trait/Core/origin DB 없이 existing `HeadlessExactGateway`의 작은 delivery delegate 확장(또는 그 child)을 사용한다. 기존 constructor/호출자의 기본 presentation은 Headless 그대로다. T1에서만 실제 `DiscordGateway(..., None)`를 선택하고 send/edit/delete/long/replace와 `can_deliver_directly`를 그 수단에 위임한다. synthetic headless placeholder를 실제 Discord anchor로 오인하지 않고 `turn_bridge/current_message_anchor.rs:8-29,96-122`의 기존 anchorless/current-identity 경로를 소비한다. protected retry는 N08의 기존 OwnedRequest closure를 그대로 사용한다. receipt 없는 요청은 exact permit을 만들지 않고 기존 Refused를 유지한다. 일반 retry는 기존 headless suppression, `can_chain_locally=false`, `bot_owner_provider=None`, requester mention 없음, local queue dispatch 금지를 유지한다. DiscordGateway의 `schedule_exact_retry_with_history`를 외부 adapter에서 호출하지 않는다. 이를 통해 output transport 선택이 API 입력을 Discord 사람 입력으로 바꾸지 않는다. giant gateway/headless 파일은 선언·factory 호출만, 본체는 작은 모듈에 둔다.

선택 조건은 **기존 bound channel + 실제 Herdr host + switched admission/O-ready 승인 + 기존 own-turn 실행**이다. target/source 문자열만으로 direct 권한을 만들지 않는다. machine은 Busy 사람 합류 경로로 보내지 않고 자기 차례의 동일 gateway 선택을 사용한다. 기존 silent_turn·delivery_bot·reply obligation 의미를 그대로 보존하며, 답이 필요한 일반 입력의 수용에 게시1을 적용한다. 사람 Busy 합류는 이미 있는 owner의 gateway·actor를 그대로 사용한다. late host/switch/O 변화는 기존 admitted custody/visible hold이며, 다른 host로 fallback하거나 delegate를 뒤늦게 no-op으로 바꾸지 않는다. current actor·incarnation/source가 바뀌면 N10 exact refusal을 유지한다. 선택한 실제 delivery delegate는 HTTP 재시작 시 persisted origin/receipt와 현 host를 다시 확인해 만들며, 예전 actor proof는 재사용하지 않는다.

`source=imessage` 또는 기존 generic API source/absent 등 입력 값, author/person 판정, source message IDs, origin GUID, ReplyObligation, reply context, metadata, prepared side inputs, receipt/turn ID는 owner admission과 `OwnedRequest`·inflight에 원래 값으로 남긴다(N08, 기존 출처표). gateway 생성은 이 필드를 쓰거나 새 owner를 claim하지 않는다. source가 없는 과거 호출에 새 GUID나 Discord source를 합성하지 않는다. 자동·routine/scheduled는 본 조각의 switched admission 대상이 아니며 기존 refusal 유지. generic API machine과 iMessage 사람 입력의 terminal 능력은 같지만 Busy 정책은 서로 다르다.

O가 소유한 본문은 직접 전달 가능한 gateway에서도 O가 게시한다(N11). bridge의 terminal receipt는 실행 종료 기록이며 Discord 게시 ACK가 아니다. O의 canonical native source·writer checkpoint가 본문 custody를 유지하므로 O 게시와 bridge cleanup 순서는 독립이다. `outbound/completed_turn_ledger/o_pending.rs:135-146`의 capture는 best-effort 반환이며 새 durable ACK로 간주하지 않는다. 기존 native source를 지우거나 pending ledger를 새로운 실행 권위로 만들지 않는다. 실제 HTTP 시험은 O 게시1과 terminal 수락·token/inflight0을 각각 관측해야 한다. 구현에서 이를 충족하지 못하면 G가 완료됐다고 보고하지 않고 반례를 다시 올린다.

### 종료 권한 소비자 전수 — G 보존 / A였을 때 필요한 변경

| 소비자(main d40c06eb47) | 현재 계약 및 G | A 완화 시 검증해야 할 영향 |
|---|---|---|
| live terminal admission `runtime_stamp.rs:1330-1385`, `herdr_terminal_admission.rs:54-74,94-120` | concrete direct+same allocation/actor/nonce/source/replay 유지. 새 capability0 | exact headless를 허용할 별도 능력과 기존 false refusal 소비자를 구별해야 함 |
| bridge loop/exit `stream_loop.rs:296-314`, `turn_bridge/mod.rs:675-692` | typed terminal 수락 뒤 기존 후처리. AuthorityLost를 성공으로 바꾸지 않음 | non-direct admitted를 live 완료로 처리해도 output custody가 남는지 새 증명 필요 |
| 본문 gate/relay `terminal_controller_cutover/o_body.rs:9-44`, `tui_o/cutover/channel_gate.rs:155-162`, `terminal_outcome_delivery.rs:437-443,522-547` | O-owned body send0, O writer source/lease만 게시. delegation이 body 권위 중복0 | non-direct terminal이 O-ready 상실 후 outbox/relay와 중복하지 않는지 필요 |
| headless outbox `turn_bridge/headless_delivery.rs:524-553,588-608`, `terminal_outcome_delivery.rs:878-943` | tmux/internal 기존 경로 유지. T1 O body를 outbox로 이중 enqueue0 | 새 accepted headless terminal의 payload·provider bot·idempotency·visible commit과 O 선택을 모두 확인 |
| detached relay/custody `terminal_outcome_delivery/foreign_terminal_handoff.rs:13-95,122-146,311-316` | detached episode의 기존 receipt/source/owner 판정 보존; 새 publisher0 | 새 headless admission latch와 foreign output owner가 이중 publish/cleanup하지 않는지 필요 |
| prior admitted restart `recovery_engine/herdr_admitted_restart.rs:23-43,90-113,124-179` | persisted terminal kind만 exact settlement; O-body는 O에 남기고 receipt commit 뒤 clear. current actor는 침범0 | headless에서 새로 저장한 kind도 같은 cleanup 권한을 갖게 되므로 restart-before/after-ACK 추가 증명 필요 |
| pathless row-end `recovery_engine/herdr_row_end.rs:53-72`, `herdr_row_end/owner.rs:113-136` | prior-process pathless/receipt 없는 row 조건 유지. 현재 살아 있는 headless leak의 대리 finalizer 아님 | terminal kind가 추가되면 row-end 제외로 새 orphan이 없는지 확인 필요 |
| current-process execution-gone `recovery_engine/herdr_row_end.rs:120-136`, `herdr_row_end_live.rs:10-25,30-88,100-147` | N15: 같은 actor·fresh row·pinned execution E 뒤 source 재읽기. 자기 terminal 있음→reader typed admission에 양보(별도 실패/본문 게시0), source 불명·교체 actor→hold. terminal 없음+확정 E→기존 retire, body-free notice Delivered, captured settlement, completion cleanup·reader wake. G external own-turn도 같은 계약 | non-direct admitted terminal의 존재가 live 선택을 막는 시점 및 terminal/retire 경합에서 reader 양보·notice1·successor 보호 재검증 필요 |
| completion·다음 입력 `completion_postlude.rs:767-805,880-895,932-960`, `finalize_epilogue.rs:69-88,205-223` | terminal receipt/guarded clear/captured allocation/deferred wake 유지. 늦은 이전 턴 정리가 successor token/inflight를 지우지 못함 | non-direct terminal 권한을 cleanup·queue wake까지 전달하는 전체 계약 필요 |
| Stop/Cancel/clear/Terminate | §5 기존 전수표 및 `turn_bridge/cancel_finalize_policy.rs:85` 유지. Completed/Aborted 기존 의미, 새 kill/reset/delete0 | 새 headless admission이 취소된 actor를 완료시키거나 다른 원본을 정리하지 않는지 필요 |

### O-b 의존성의 정확한 경계

| 계약 | O-b 좌표 | T1/T2a 영향 |
|---|---|---|
| thread surface 생산 | `src/services/session_host/surface_policy.rs:31-48 VerifiedSurface::from_discord`; main 대응 `:27-29`는 cfg(test,unix) | T1 bound 불필요. T2a checked relationship에서 기존 생산 surface 소비 필요. |
| checked child 등록/예약 해소 | `src/services/tui_o/cutover.rs:130-176 resolve_reserved_child/record_thread_child/record_thread_child_by_id`, `:179-192 record_in` | T2a는 검증된 실제 child ID에서 이 계약을 소비; parent metadata만으로 등록 금지. |
| 독립 child adoption | `src/services/tui_o/channel_policy/children.rs:29-45 body_owner`, `:63-85 read`, `:138-160 resolve` | child own candidate, unresolved/conflict Held, parent writer alias 금지. bound를 child로 중복등록하지 않는 guard `:147-150`. |
| boot reservation | `src/bootstrap.rs:74-76 reserve_boot_children(open_tui_thread_hints)` | thread boot 직후 옛 Legacy send와 O init 동시성을 막는 O-b 소유 계약. T1에 의존 없음. |
| child output readiness | `cutover.rs:106-107 thread_child`, `:242-244 policy_channel`; writer host/child_retirement 및 runtime_bootstrap/o_writer_host 배선 | parent ready를 child ready로 쓰면 안 됨. API를 열기 전에 child source/writer/adoption readiness를 실제 확인. |

main `src/services/discord/session_runtime.rs:512-514`는 thread ID exact `herdr_endpoint`로 bootstrap 거절 여부를 판단한다. O-b의 surface production을 H1 host inheritance 완료로 잘못 읽으면 안 된다. T2a의 새 H child 지원에는 H1 정본 생산 계약이 별도 필요하며, live old tmux child는 H parent 정책 때문에 실제 locator를 H로 덮지 않는다.



| 사용자 동작 | tmux | Herdr | 담당과 수용 |
|---|---|---|---|
| API idle start / deliver의 idle 시작 | 기존 own-turn executor | 현재 일반 refusal로 차단, 뒤에는 기존 Herdr executor가 있음 | #8125 T1. intake와 같은 switched admission을 사용하되 O-ready·capacity·hosted row·provider switch 거절 보존. 단순 refusal 삭제만으로 완료하지 않음 |
| busy 사람 입력 | 기존 owner/physical input | 기존 exact Bound Claude pane injector와 #8184 공통 BusyOperation·early/late owned probe 존재 | #8125는 재사용·route 배선과 결과 보존. Codex를 Claude injector로 우회시키지 않음; main의 provider별 switch·실행기·receipt 시험을 그대로 소비 |
| 새 thread 준비와 host 선택 | 기존 tmux bootstrap | 부모 host를 상속한 child의 기존 launch/admission | #8118 H1 정본. #8125에 별도 parent endpoint fallback을 만들지 않음 |
| 살아 있는 old tmux child, 부모 H | actual tmux 유지 | 정책만으로 live target 전환하지 않음 | #8118 계약 소비. 다음 새 native session부터 상속, live 이사는 #8077 별도 |
| child 답 게시 | child 기존 출력 | child 기존 출력 | O-a만으로 완료 아님. #8118 O-b 활성·child O-ready 통합 증거 필요 |
| clear/restart/진단·관리 reset | 현행 의미 | 기존 차이 추적 | #8128 src/server/routes/D/E 등 담당. 본 API 변경이 close·cleanup 권한을 넓히지 않음 |

`inject.rs:312`의 tmux 조건은 idle transcript 제출 fast path다. 이를 “Herdr 입력 없음”으로 표기하거나 단순 제거해 Herdr를 tmux driver로 보내지 않는다. busy H는 `inject/herdr.rs`의 실제 row/pane pin과 #8184의 공통 BusyOperation·owned probe를 사용하고 idle H는 공통 headless own-turn start와 기존 Herdr Idle launch hook을 사용한다. T1의 시작·idle deliver 차이는 #8128이 #8125로 지정한 기능이므로 #8128 추적만 남겨 본 이슈 완료로 보고할 수 없다.

## 5. 부팅·재시작·취소·경합

재시작 후 메모리 map이 비어도 native 세션 부재가 아니다. DB hosted row·inflight·source·저장된 worktree와 기존 restore를 확인한다. Pending/unknown·old process row를 fresh 권한으로 승격하지 않는다. runtime 미준비 거절은 accepted가 아니며, accepted 뒤 준비/전환을 기다리는 입력은 기존 durable custody가 유지되거나 visible terminal 실패가 있어야 한다. 실제 wake 없는 `queued` 응답만으로 liveness를 주장하지 않는다.

동시 API 두 건과 Discord 첫 입력 한 건은 같은 child transition/owner admission을 사용한다. winner만 세션을 준비하고 race loser는 준비된 세션을 재사용하며 원본을 각각 보유한다. worktree/native 시작 계수와 입력 효과 계수를 분리한다. 같은 GUID retry는 같은 원본이고 서로 다른 GUID 세 건은 각기 보존된다. mailbox claim 이전에 dispatch parent edge를 publish하지 않는다.

clear·stop·HTTP cancel·restart가 끼면 existing token/transition/exact locator 판정을 재관측한다. 이미 취소된 원본에 늦게 키를 넣지 않고, effect 불명인 입력은 NotSent로 바꾸어 재전송하지 않는다. 기존 NotSent handback·Unconfirmed no-resend 의미를 그대로 유지한다. parent나 sibling의 queue/source/pane을 대신 지우지 않는다. 기존 관리 경로 결함의 처분은 E17/E18과 #8118 H2/#8128에 남기며 본 이슈에서 kill/reset/삭제를 추가하지 않는다.

N15의 current-process execution-gone도 아래 복원 경로와 별개로 소비한다. 외부 own-turn row의 자기 terminal은 reader에 남기며, terminal 없는 확정 E만 기존 안내·captured settlement·wake로 끝낸다. unread/unknown/source 교체는 hold한다.

### 부팅·incarnation 경로별 소비

| 경로 | main 좌표 | 반드시 지킬 소비 조건 |
|---|---|---|
| cold/restored native state | `src/services/discord/session_runtime.rs:527,532,562,596`; `src/services/discord/watchers/lifecycle/restore.rs:198`; `src/services/discord/recovery_engine/restore_inflight.rs:684,729,956` | DB selector/stored worktree/hosted row/inflight를 먼저 확인. core map 공백은 native absent 아님 |
| process epoch recovery hold | `src/services/discord/recovery_engine/restore_inflight.rs:128,140,149,153,204,210` | generation 할당 실패는 의도적 보수 hold(기존 #5482); admitted stop receipt/replay hold/held Herdr를 wrapper fresh launch로 바꾸지 않음 |
| prior admitted terminal | `src/services/discord/recovery_engine/herdr_admitted_restart.rs:23,30,90,102,113,159,176` | current generation은 prior terminal settlement 대상 아님. hot-swap·새 actor 보호. 정확한 row/nonce/generation을 담은 기존 terminal 계약 소비 |
| external owner re-adopt | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:690,701,707`; `src/services/discord/runtime_bootstrap.rs:35,41` | accepted original은 durable queue/history/notice를 회복하고 실제 owner wake가 있어야 함 |
| exact H Busy and Idle proof | `src/services/discord/queue_io/input_owner/claude_handoff.rs:464,475`; `src/services/claude_tui/host_input/busy/herdr.rs:258,280,297`; `src/services/discord/router/message_handler/intake_turn/owned_busy.rs:306,326` | node/token/root/provider/logical/channel/execution nonce/source inode·session 및 same-front generation을 보존. old incarnation proof는 새로운 target을 찾는 권한이 아님 |


### 같은 상태를 소비하는 인접 경로 전수

| 경로 | main 좌표 | 소비 상태/본 변경 경계 |
|---|---|---|
| Slash /stop | `src/services/discord/commands/control.rs:617,637,668`; `src/services/discord/commands/stop.rs:84,91`; `src/services/discord/turn_bridge/tmux_runtime/judged_stop.rs:345,358,389,399` | 실제 child/current owner revalidation, Herdr intent/Escape의 기존 계약. 새 thread override 권한 없음 |
| HTTP nonforced cancel | `src/server/routes/queue_api.rs:209,226,230`; `src/services/queue.rs:337,347,365,375`; `src/services/queue/herdr_stop.rs:13,19,24`; `src/services/discord/commands/stop.rs:98,107` | forward/receiver fence 뒤 judged live Herdr stop. provider end까지 queue·dispatch·inflight를 유지; cancel success를 completion으로 바꾸지 않음 |
| HTTP force cancel | `src/services/queue.rs:365`; `src/services/queue/herdr_stop.rs:19`; `src/services/discord/herdr_terminate.rs:436,452,481,547` | force는 nonforced adapter를 건너 기존 selection/terminate exact fence 경로. 새 parent/sibling 정리 없음 |
| Agent stop | `src/server/routes/agents_stop.rs:28,39,68,99` | 기존 bound agent session 선택 및 host refusal 우선, queue preserving lifecycle; 새 child 대상 override 없음 |
| waiting cancel | `src/services/discord/commands/control.rs:711,749`; `src/services/discord/router/message_handler/intake_turn/owned_busy.rs:306,314,326`; `src/services/discord/queue_io/input_owner/held_watch.rs:62,151` | front 폐기/교체 뒤 watch Gone/End, 옛 original 재생성·부활 금지 |
| operator Terminate | `src/server/routes/domains/runtime.rs:164`; `src/server/routes/herdr_terminate.rs:40,48,68,88`; `src/services/discord/herdr_terminate.rs:400,436,452,481,518,547,678` | local operator/server auth/switch 및 exact session+execution nonce·current actor·row fence. 입력 허용이 terminate 권한을 만들지 않음 |
| clear | `src/services/discord/commands/control.rs:490,510,521,540,552,561,599`; `src/services/discord/commands/control/clear_teardown.rs:61,70,137,155,179,191,218` | transition(기존 timeout overtake 포함), existing queue discard/current stop·selector clear·exact H terminate. 기존 best-effort/경고·fallback 의미를 넓히지 않음 |
| awaited clear / source retirement | `src/services/claude_tui/host_input/busy/herdr.rs:215,224,292`; `src/services/discord/session_transition.rs:132,145,153` | clear된 같은 incarnation proof만 Idle 위임, completion은 guard release 후 owner wake; 이전 watch가 discarded front를 재생시키지 않음 |
| restart | `src/services/discord/commands/restart.rs:150,216,293`; `src/services/auto_queue/runtime.rs:485` | 기존 seed/host guard 거절 의미 유지; T1로 관리 경로 parity를 몰래 열지 않음 |


기존 관리 경로의 timeout overtake·best-effort clear·bound-only agent stop 차이는 위 main 좌표에서 그대로 남긴다. T1/T2a가 관리 API에 새 child override·kill·삭제·retire 권한을 더하지 않는다. cancel 전에 효과가 없으면 기존 NotSent handback, 첫 키 이후 효과가 불명이면 Unconfirmed와 원본 보유로 남긴다.

## 6. 구현 조각과 예상 크기

새 trait·Core·session host 프레임워크·origin DB를 만들지 않는다. 기존 `AgentTurnTarget`, owner-channel starter, thread restore/bootstrap, owner actor, provider dispatch, Herdr operation, obligation을 연결한다. giant 파일에는 선언·호출 배선·거절 분기만 두고 정책/준비 본체는 기존 작은 소유 모듈 또는 그 child에 둔다. giant 파일을 줄 수만으로 막지 않으며 본문 블록 이동으로 수치를 맞추지 않는다. 실제 리뷰 상한은 `pr_cap_prod.py`의 prod 30파일/순 +1,500, PLAN 정지선은 순 +1,200이다.

| 조각 | 파일 후보 | 예상 prod | 필수 수용/뮤턴트/선행 |
|---|---|---|---|
| T1 단일 활성 조각 | `services/turn_host.rs`; `discord/router/message_handler/headless_turn.rs` 및 작은 admission child; `discord/health/turn_deliver.rs`, `turn_deliver/prepared.rs`; `server/routes/agents_turn_deliver.rs`/`legacy_origin.rs`의 shared preflight 배선; `headless_turn/replay_start.rs`, `gateway/headless_exact.rs` 및 작은 delivery child; 필요시 기존 owner dispatch 작은 child; 실제 HTTP/owner fixture 시험 파일 | 11~14 prod / 순+250~550; 현재 초안은 main 대비 prod10/순+104 code, delivery adapter·종료 HTTP 시험을 포함해 재추정; 기존 executor 재구현0 | bound H start/idle deliver 실제 HTTP RED→H executor1/no tmux, typed terminal 수락·해당 token/inflight0·다음 distinct original 진행·O body 게시1; busy person joins once vs machine waits own turn; early/late idle→busy barrier; same GUID retry/restart1; disabled/O-unready/unknown host rejects0; admission 후 late change retained+visible. mutants generic refusal restore, switch/O-ready 제거, launch hook 제거, person/synthetic 오분류, stale locator 허용, headless presentation 복원, terminal guard 우회, cleanup/wake 제거. **O-b 불필요**, main #8184 소비; 운영 T1 switch·O-ready 별도 확인. |
| T2a 단일 활성 조각 | `server/routes/agents_turn_target.rs`, `agents.rs`, `agents_turn_deliver.rs`; `discord/health/headless_turn.rs`, `turn_deliver.rs`, `turn_deliver/prepared.rs`, 필요시 `runtime_resolve.rs`; target/live gate 작은 child·API 문서·route tests | 8~12 prod / 순+400~750 | 두 endpoint×T/H×public/private/news live local; wrong kind/parent403, REST/timeout/conflict503·mutation0; actual child queue/native/reply1, parent/sibling0; current incarnation 확인; old tmux actual 유지; GUID retry stable. mutants exact-bound restore, payload parent trust, child direct provider lookup, parent source/queue alias, boot current check 제거. **T1+H1+O-b 배포/수용**, 새 bootstrap/outbox dormant0. |
| 외부 스킬 수용 | imsg_send.py + imsg_inbound.py (+imsg_fork.py helper 또는 inbound API-first 분기), 등록·fork·GUID fixture | repo prod0 | child registration/host-aware routing/GUID 전송 모든 필요 수정 후 실제 iMessage1건. T1/T2a 배포 전 H 제외만 먼저 제거 금지. 스킬·환경 동기화는 담당 코디네이터가 별도 실행. |


T2b는 기존 intake admission·DTO·routed outbox·worker의 typed context 왕복과 실제 child bootstrap 활성 조각이다. 예상 prod 15~25파일/순 +800~1,200이며 이번 델타에서 구현 경계를 변경하지 않는다. T2a가 cold/remote bootstrap·transport의 미호출 scaffold를 포함하지 않도록 이 경계를 유지한다.

초기 통합 설계 T1/T2만으로는 cold/remote의 provenance/placement 적응량을 과소평가하므로 T2를 live local과 cold/remote로 분리한다. T1 1조각·T2a 1조각과 후속 T2b는 각각 prod 30파일/순 +1,500 이내, 구현 전 확정 추정이 +1,200을 넘으면 PLAN_READY로 재분할한다. T2b의 다음 분할 경계는 기존 transport의 typed context 왕복과 실제 worker 소비를 포함한 기반 단위, 그 기반을 사용하는 공개 cold/remote route 활성 단위다. 기반을 무호출 dormant 코드로 두거나 경계만 맞추려고 필수 시험을 빼는 안은 허용하지 않는다. 실제 활성 소비자를 한 단위에 담을 수 없으면 별도 설계 결정이 필요하며 크기를 통과했다고 간주하지 않는다.

T2a만 착지한 동안 #8125 전체는 부분 완료다. 다만 살아 있는 검증된 local 스레드에서 시작한 iMessage의 직접 답장 수용은 T1·T2a·H1·O-b 배포·외부 스킬 수정 뒤에 완결할 수 있으며 T2b를 기다리지 않는다. no-session/remote 스레드의 동일 보장은 T2b에 남긴다. T1이 H1 소유 정책을 고쳐야 하거나 T2가 #8118 계약을 바꿔야 하면 DESIGN_DELTA다. 구현 착수 때 #8118 정본의 현재 head와 핵심 심볼 드리프트를 확인해 중복 구현·상호 의존을 피한다.

## 7. 시험 계획과 실제 수용

### live local 사용 요청을 만족시키는 최소 경로

1. 두 공개 route의 `agents_turn_target.rs:15-49 resolve_agent_turn_target`를 확장한다. 현 `:65-71` exact-bound-only가 thread403의 원인. payload parent가 아니라 checked Discord thread kind(Public/Private/News)+parent를 agent의 현재 bound channel과 비교한다. owner parent로 provider/settings/runtime을 선택하고 destination child는 변경하지 않는다.
2. 현재 `AgentTurnTarget :8-12`에 owner/runtime parent와 actual destination 역할을 분리. start의 `health/headless_turn.rs:100-140 start_reserved_headless_agent_turn_with_owner_channel` 재사용; deliver의 `HumanInputRequest`/preflight/prepared/resolve에 같은 owner parent를 명시적으로 전달. `runtime_resolve.rs:167-205` worker parent 후보·`:208-230` gateway live probe는 기존 계약을 소비하되 실패를 fallback 성공으로 축소하지 않는다.
3. current-process verified live local child와 actual native/hosted locator·child ready만 허용. cold/remote/boot unresolved는 명시503+mutation0. parent/sibling queue/source/native IO0. target validation은 실행 권위를 만들지 않는다.
4. GUID admission→existing owner queue/front→#8184 actual Busy/Idle adapters→native transcript input1→O-b child writer에 reply1. parent/sibling reply0. cancel/clear/restart/stale nonce는 existing transition/token/physical grant로 revalidate.
5. 스킬은 발신 GUID→actual child registration을 만들고 답장 원본 GUID를 두 endpoint 모두 전달한다. start409일 때 deliver로 갈 때 같은 GUID를 유지한다. 실제 수용은 발신 GUID/inbound GUID→actual API child→owner original→native transcript→child reply message ID join, 한 실제 답장 소비1, parent/sibling0이다. HTTP200/echo/receipt만으로 PASS 금지.


### 저장소 밖 스킬의 후속 수정

정본 경로 `/Users/itismyfield/ObsidianVault/RemoteVault/99_Skills/imessage-send/scripts/`.

- `imsg_send.py:227-272 caller_target`: 현재 `:239-240` thread suffix를 부모 이름으로 제거하고 `:265` bound를 그대로 target으로 쓰며 `:266-268` Herdr binding은 target을 비운다. T1 배포·수용 뒤 Herdr 제외를 해제하고, agent/provider 권한 발견용 parent와 실제 `context.channel_id` child를 분리해 `register_thread :288-294`에 **child channel_id**를 저장한다. 설정의 parent H 여부는 actual native host 증거가 아니며 스레드 이름 파싱은 권한 증거가 아니다.
- `imsg_inbound.py:220-226 start_turn`: 현재 `origin`/`person` 인자를 받지만 body에 GUID가 없다. `origin_id=origin`을 추가해 main `agents.rs:673 start_metadata`가 stable imessage identity를 만들도록 한다. `deliver_turn :229-238`는 GUID·author를 이미 보낸다. 두 endpoint에서 child/provider/source/GUID를 유지한다.
- **빠뜨리기 쉬운 fork 경로**: `imsg_inbound.py:404-407`는 Claude(provider_session_id 있음) 또는 Codex에서 `imsg_fork.tmux_alive`가 false면 headless fork로 API를 우회한다. `imsg_fork.py:62-77`는 실제 tmux만 검사한다. Herdr target 등록 제외만 지우면 live H를 dead tmux로 오판해 여전히 thread API로 안 간다. 최소안은 등록된 AgentDesk Herdr target을 API로 우선 전달하고 tmux absent만으로 fork하지 않게 한다; host-aware 검증을 쓰더라도 unknown→fork 금지. `SESSION_KEYS imsg_inbound.py:308-313`에 필요한 routing/host discriminator를 보존한다. BindingContext.host(main `binding_context.rs:29`, `herdr_launch.rs:750`)는 **node ID**이므로 H/T discriminator로 오용하지 않는다.
- watcher `imsg_inbound.py:320-361`는 start409만 deliver로 넘기며, timeout/restart-midPOST `:384-388`는 unknown·no resend를 지킨다. 이를 blind retry/fork로 바꾸지 않는다. `:432-434` fallback도 child/GUID 그대로다.
- `imsg_send.py:463-469`의 자동 send registration과 inbound `:151-165 resolve_target`를 함께 fixture 시험한다. bubble 답장은 발신 GUID registered target, 없으면 default인 현재 의미를 명확히 한다. 운영 기존 `threads.jsonl`을 무단 rewrite하지 않는다.
- main `agents_input_result`는 bound channel만 찾으므로 child404를 실패나 성공으로 쓰지 않는다. watcher는 현재 이 endpoint를 사용하지 않아 T2a 필수 수정 아님; 실제 custody/native/reply join으로 검증하고 read API 확장은 별도 계약으로 남긴다.


등록 대상은 발신 스레드로 명시된 iOS inline reply이며, 일반 새 문자에 기존 default를 쓰는 계약까지 이 설계로 바꾸지 않는다. 전환 후 새 발신부터 정확한 child를 등록하고 옛 등록을 운영에서 일괄 rewrite하지 않는다. 스킬은 이번 레그 수정0이며 담당 코디네이터의 별도 수용·동기화 단계다.

### 구현 시험과 운영 증거

아래는 구현 레그가 실행할 계획이다. 설계 모델의 GREEN을 Rust route·DB·실제 문자 GREEN으로 보고하지 않는다. fixture는 운영 설정/DB/파일을 물려받지 않는 격리 환경에 두며 cargo-shim과 해당 호스트의 검증 환경을 적용한다(이 WSL 레인은 headroom/seed 없음, 기본 병렬).

| 묶음 | 수정 전 RED / 수정 후 GREEN | 원인 제거 뮤턴트 |
|---|---|---|
| T1 실제 HTTP 진입 | 실제 runtime router start/idle deliver의 수정 전 executor_not_wired RED와, 초안의 typed terminal actor refusal·30초 token/inflight 잔존 RED를 따로 보존. G 뒤 H executor1, tmux/process fallback0, exact typed terminal 수락, 해당 original token/inflight0, 다음 distinct GUID 진행, 실제 bound channel O 답 게시1(다른 채널0). 완료/게시를 각각 관측하며 terminal receipt를 게시 ACK로 세지 않음 | 일반 refusal 복원, provider switch/O-ready 거절 제거, H를 tmux driver로 전송, headless delivery delegate 복원, idle→busy 재검사 제거, exact terminal proof/guarded clear/wake 제거 |
| T1 종료·보관·다음 입력 | terminal 전에 O 게시 / terminal 뒤 O 게시 모두 답1. 같은 GUID HTTP retry/restart는 native1·logical reply1; distinct next GUID는 cleanup 뒤 native1. admission 뒤 writer hold는 source custody·visible hold, 실패를 완료로 세지 않음. admitted terminal 후 재시작은 기존 prior-process settlement, 새 actor가 들어온 late cleanup은 successor token/inflight 보존. machine Busy join0 | 두 게시자 활성, source/receipt를 Discord로 재분류, 이전 actor cleanup 허용, no-op send에 direct=true, O-ready/late hold 제거 |
| T2 권한 | bound parent 아래 실제 public/private/news thread가 두 API에서403 → 허용 및 실제 child start/delivery. 기존 bound channel 응답 유지 | exact-bound-only 복원, payload parent 신뢰, Option 오류를 허용, provider를 child direct binding으로 찾기 |
| 거부 부작용 | foreign channel/thread·category·parent 없음403; parent timeout/REST Err/상충503; author·caller·metadata agent·provider mismatch 기존 거절 | parent lookup 오류 swallow, live child row로 agent binding 생략, 503을 403/성공으로 평탄화 |
| same-child 경합(T2b) | no-session child deliver2+Discord first1, barrier로 lookup/transition/claim 순서 제어 → worktree/native 시작1, distinct original 각 소비1, parent/sibling IO0 | child 재확인 제거, child→parent queue/source/turn key 치환, bootstrap과 start 둘 다 payload 제출 |
| placement·transport(T2b) | 실제 API cold/remote→IntakeSubmission→NoOwner preferred placement/local permit 또는 durable outbox→worker→child 준비. #8115 슬롯 미광고 preferred 제어군, live foreign owner, forward/retry/restart 후 source/origin/metadata/reserved ID 그대로. absent legacy decode 유지, present malformed context는 효과0·visible failure | headless 직결로 placement 우회, context 누락·child→parent 치환·identity 재생성, API를 Discord source로 변환, malformed를 absent로 downgrade, 임의 reply_context JSON을 trusted context로 해석 |
| origin·busy·불명 | 같은 GUID 동시/재시도·재시작은 original1; distinct same-text는 각각1; busy T/H는 기존 owner 경로; 불명 effect 재전송0 | origin 재생성, body dedupe, admission 우회, Unconfirmed→NotSent, handback 원본 버리기 |
| boot/transition/cancel | runtime 없음은 명시503·미수락; accepted 뒤 재시작/boot-ready/clear/stop/restart는 각 delivered 또는 visible failure, stale native 중복 시작0 | core None→fresh, retained custody drop, wake 제거, cancel 뒤 first key 허용 |
| 출력·회귀 | 실제 child에 응답1, parent/sibling 본문0. old live tmux+H parent는 tmux 유지, cold new child는 H1 정책. tmux bound 회귀·기존 author/auth/DM 의미 유지 | policy host로 live locator 덮기, parent pane/source/post 대상 대체, O 미준비를 완료로 표시 |

진입점 시험은 `src/server/routes/domains/runtime.rs`의 실제 router와 extractor·target·preflight·claim을 통과시킨다. fake는 Discord 관계 조회·native driver·출력 효과와 같은 외부 경계에서만 사용하고 테스트 helper만 직접 호출해 라우트 검증을 대신하지 않는다. 403/503 시험은 HTTP 코드와 함께 origin admission·queue append·bootstrap·worktree/native launch·pane keys·posted reply가 모두 0임을 확인한다. 읽기 요청·로그는 이 mutation 계수에서 제외한다.

기존 `src/server/routes/agents_turn_deliver/pg_tests.rs`, `src/server/routes/imessage_input_notice_tests.rs`, `src/services/discord/router/message_handler/provider_dispatch_herdr_tests.rs`와 owner/stop/boot 시험을 확장한다. DB가 필요한 시험은 PG 실제 실행 여부를 남긴다. selector가 0건이거나 PG 없어서 skip된 결과는 PASS가 아니다. 각 실행은 head·시험 binary 경로/SHA256·필터·실행/실패/skip·원시 로그·SHELL/TMPDIR/profile를 보고한다. 원인 뮤턴트마다 해당 시험 RED, 원복 후 바이트 동일을 증명한다. 모델 변형의 RED는 실제 생산 뮤턴트 RED를 대체하지 않는다.

실제 수용은 API·#8118 필요 조각 배포 뒤 스레드 세션에서 보낸 iMessage 한 통에 실제 답장 한 건을 받아 확인한다. 운영 단계의 담당자가 배포 SHA·시각·agent·parent·child·actual host/native session·발신 대상 등록·원본 GUID를 연결하고, inbound 등록→실제 API target child→admission/owner original→native transcript 소비→child 답 게시를 join한다. 분모는 실제 답장 1, 해당 입력 소비1, parent/sibling 입력0이며 답 게시 message ID와 원본/응답 대조를 남긴다. echo/HTTP2xx/receipt만으로 모델이 읽었다고 판정하지 않는다. `input-result`는 현재 bound만 검색하므로 child404는 실패·유실 증거도 성공 증거도 아니다. 원본 iMessage 본문/개인 식별값을 공개 문서·커밋에 넣지 않는다.

실제 답장 없는 합성 HTTP·관측 창 요청0·native transcript0을 수용 PASS로 쓰지 않는다. 이 레그는 운영 실행이 금지되어 실제 답장을 보내거나 사용자에게 답장을 요구하지 않았으며 실제 수용은 미실행이다. 외부 스킬의 GUID 미전송 start를 그대로 두고 API exactly-once 완료를 선언하지 않는다.

## 8. 리뷰 집중 항목

| 집중 항목 | 막을 방법 / 남은 반례 |
|---|---|
| #8184 Idle/Busy와 외부 시작 겹침 | N01/N02 actual 관측, N04 Idle 예약 반환 후 driver 재검증, N06 same-front generation wake. person Busy join과 machine own-turn 대기를 실제 HTTP barrier 시험에서 구별. 외부 headless에 intake early가 이미 있다고 가정하지 않음 |
| 부팅 직후 옛 incarnation | E14/N03/N07와 exact root/provider/node/channel/nonce/source 및 same-front generation을 재관측. map None·새 parent 정책으로 옛 proof를 재활용하지 않음 |
| 출처 재분류 | queued_head/v2 person 판정과 origin/from_context·GUID stable ticket을 분리. routine/scheduled·generic HTTP는 synthetic, iMessage만 기존 조건으로 person. child target은 출처 변환0 |
| 거절 effects0 | target/host/live/runtime·author 검사를 origin reserve 앞의 효과없는 preflight에 배치. admission 후 경합은 custody/visible terminal이며 거절0으로 보고하지 않음 |
| 바꾼 cfg의 잘못된 0/PASS | 변경 모델의 모든 cfg.example을 scratch cfg로 materialize해 statecharts_tlc.py manifest+source hash+선택 수를 남김. 기대 위반집합과 실제 liveness 검사 수를 함께 비교 |
| 권한 우회 | 각 route의 기존 caller/author 규칙과 agent binding·실제 checked parent를 통과 후 준비. forged metadata/live row/dispatch map은 권한 아님. 두 route negative mutation0 시험 |
| 부모 조회 실패 | Result 오류 보존, 미확정503·effects0. parent 이름 조회와 관계 조회를 혼동하지 않음. #8118 provenance 경계의 생산 API는 구현 착수 시 재확인 |
| 경계 시각·경합 | actual child transition 뒤 존재/holder 재확인, native 시작과 original 소비 별도 계수. multi-original·same-GUID 실험은 단일-ticket 모델 밖이라 필수 route 시험 |
| 옛 형식·재시작 | 미기록 child checked REST, stored worktree/hosted row/inflight 복원. core None은 absent 아님. recorded/REST 충돌은 거절; 기존 queue/receipt 포맷을 바꾸지 않음 |
| 문서로만 선언한 가드 | surface API는 O-a에서 시험 전용임을 명시. T1 실제 executor·T2 실제 route·PG/native effect 계수로 연결. main 드리프트 시 핵심 심볼 변경 확인 |
| 새 권위·배달 경로 | #5071 대조표의 mailbox·owner·outbox·physical gate·writer만 소비. target/type context가 독자 실행 권위를 만들지 않음. source/GUID 보존을 transport 실제 왕복으로 확인 |
| 잘못된 0/PASS | 모델/route/운영 증거 분리, 미실행·0 selector·PG skip·0 request 불합격. H busy 기존 존재를 누락0으로 오기하지 않음. child input-result404에 기대지 않음 |
| 범위 확장 | 호스트 상속 정본 #8118, 관리 parity #8128, live migration #8077. parent/sibling kill·삭제·reset0. 이 설계에서 legacy 결함 수리0 |

설계 3의 종료 모델·수정 전/후 cfg 표·fairness 한계는 [종료 모델 보고서](statecharts/input-single-path/thread-ingress-terminal.report.md)를 따른다. 기존 first-key 모델의 reply 포트는 더 이상 실제 종료 계약의 증거로 쓰지 않는다.

## 9. 0단계 상태 차트 인계

[상태 차트 색인](statecharts/README.md)의 입력 큐 `InputPath`/`InputOwnership`, 세션 수명 `TmuxSession`, O-a thread reader 모델을 대조했다. 기존 모델은 queue/owner/ready·session restore와 child reader를 각각 다루며 공개 API 권한에서 thread 첫 시작으로 들어가는 접합은 없었다. 새 연결 모델은 그 포트 사이의 권한→준비→한 번 제출→child 답 게시를 검증한다. 기존 입력·수명 proof를 새 API의 운영 증거로 승격하지 않는다.

실행 결과·수정 전/후 표·반례와 코드 대조·재현 명령·모델 밖 항목은 [연결 모델 보고서](statecharts/input-single-path/thread-ingress.report.md)에 둔다. 설계 2의 공식 실행은 **36cfg(22 GREEN·14 expected RED)**였다. 이후 P2 첫 커밋 `3d458bd65`에서 driver 재검사·machine Busy invariant의 cfg3개를 더했고 **39cfg(22 GREEN·17 expected RED)**가 됐다. 설계 3은 기존39를 바꾸지 않고 종료 동반11cfg를 추가했으며 공식 harness로 전체 **50cfg(25 GREEN·25 expected RED)**를 다시 실행했다. 종료11의 before도 전부 기대 RED로 별도 실행했다. 기대값 불일치·도구 오류·skip·미선택은0이다. 기존19cfg는 설계1 원본 bytes의 before와 새 bytes의 after가 같은 위반집합이다. 신규 단일 Herdr14cfg는 main의 닫힌 gate만 투영한 동일 fixture before도 실제 실행(2 GREEN·12 expected RED)했으며, accepted0의 vacuous liveness와 기능 거절을 분리한다. 새 Race3cfg는 이미 채택된 사람 original의 조건부 owner 포트이며 main이 API를 이미 수락한다는 증거가 아니다.

건강한 cfg는 accepted→실제 native first-key 시작(`HerdrAcceptedStarts`/`EachAcceptedStarts`)을 PROPERTY로 검사한다. Busy 사람 입력은 이미 시작된 native turn 합류, non-person HttpStart는 Busy 종료 뒤 Idle own-turn이다. `HBegin`에서 물리 효과를 세고 ACK는 다시 세지 않으며, 효과 뒤 재시작은 uncertain→보이는 실패로만 종결한다. same-GUID HTTP/DB dedupe·다중 node/outbox 원자성·실제 native source/receipt는 구현 시험에 남긴다.

모델의 값 범위와 코드 출처는 §2가 정본이다. 모델의 fairness가 실제 ready/owner/writer 이벤트를 대신 구현하지 않으며, 관계 조회·host inheritance·durable queue/native IO의 내부 원자성은 기존 포트 계약 및 구현 시험으로 확인한다. `.cfg.example`은 실제 scratch `.cfg`로 materialize해 공식 harness로 검증했다. 이 레그는 제한된 baseline 항목 변경을 피하려고 `expected.tsv`를 수정하지 않았으므로 CI 자동 등록/PASS를 주장하지 않는다. 코디네이터가 구현 조각에서 정식 cfg/expected 등록을 승인해야 한다.
