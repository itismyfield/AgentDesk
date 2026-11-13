# 외부 입력의 스레드 직접 전달 설계 — #8125

기준은 main `33d1f4b5a9dce7023e2d0014e45a05ed6c530bba`, 조사일은 2026-11-13 KST다. 좌표는 별도 표기가 없으면 이 SHA에 고정한다. 이 산출물은 설계·문서·실행 가능한 유한 모델이며 생산 Rust, 운영 설정·DB·프로세스, 저장소 밖 스킬을 변경하지 않았다. Rust 시험과 실제 아이메시지 수용은 구현·배포 뒤에 수행한다.

정본 입력은 [#8125](https://github.com/itismyfield/AgentDesk/issues/8125), 선행 [#8115](https://github.com/itismyfield/AgentDesk/issues/8115), [#8118](https://github.com/itismyfield/AgentDesk/issues/8118), 호스트 차이 추적 [#8128](https://github.com/itismyfield/AgentDesk/issues/8128) 및 [통합 설계](session-host-parity.design.md)의 T1/T2 계약이다. 이 문서는 그 계약을 현재 main에서 구체화한다. 스레드 호스트·출력 상속과 검증된 surface 생산 경계는 #8118 정본을 소비하며 별도 판정기를 만들지 않는다.

## 1. 결과와 경계

두 API가 현재 에이전트에 묶인 부모 아래의 실제 Discord 스레드를 목적지로 허용한다. provider·설정·런타임 선택은 부모, 세션·mailbox·original·origin receipt·물리 입력·답 게시 목적지는 실제 자식이다. 살아 있는 자식 실행은 기존 경로로 입력을 받는다. 자식 세션이 없으면 Discord 첫 스레드 메시지와 같은 복원·bootstrap·배치 경로를 거친 뒤 원본을 한 번 제출한다. bootstrap은 준비만 맡고 payload를 별도 제출하거나, 기존 시작 경로가 payload를 소비했다면 그 결과를 그대로 반환한다. 두 경우를 겹쳐 제출하지 않는다.

확정된 남의 채널·스레드, 실제 thread가 아닌 임의 대상, parent가 확정적으로 없는 대상은 403이며 입력·큐·GUID admission·bootstrap·unarchive·launch 부작용이 없다. 관계 조회 실패·timeout·상충은 통합 설계대로 `503 {"status":"thread_parent_unverified","retryable":true}`로 거절한다. 조회 실패를 부재로 바꾸어 허용하거나 403으로 압축하지 않는다. 후속 재시도는 같은 origin을 유지하며 새 자동 전송기는 만들지 않는다.

한 번 받아들인 논리 입력은 소유 경로를 하나만 갖는다. 전달되면 세션 입력 효과가 정확히 한 번이고, 전달할 수 없으면 사용자에게 보이는 실패로 종결한다. 성공 입력의 중복·유실은 없으며 실패를 성공으로 세지 않는다. 원본이 없는 상태에서 영구 대기를 PASS로 취급하지 않는다. 같은 문자열의 별개 입력은 별개이며, HTTP 재시도 dedupe에는 기존 origin 계약이 필요하다.

### 이미 main에 있는 것과 남은 것

| 항목 | main 증거 | 이번 처분 |
|---|---|---|
| #8115 preferred node의 새 스레드 배치 | `135a168e60`이 기준 main의 조상. `src/services/cluster/execution_capacity.rs:54,57,63`, `src/services/cluster/intake_router_hook/placement.rs:24,97` | 재구현하지 않음. 공개 headless 시작은 현 placement를 우회하므로 T2가 기존 admission/placement seam에 연결한 통합 시험으로 소비 |
| O-a child reader·policy parent 준비 | 기준 main의 `src/services/session_host/surface_policy.rs:6,27`, `src/services/tui_o/channel_policy/children.rs:1,30,152` | 준비 단계. production surface 생산·host 상속·출력 membership 완료로 읽지 않음 |
| 내부 owner-channel/actual-turn-channel 분리 시작 | `src/services/discord/health/headless_turn.rs:100,124,128` | 공개 API target에 연결해 재사용 |
| busy Herdr Claude 입력 | `src/services/discord/health/turn_deliver/inject/herdr.rs:41,79` | 기존 exact pane·receipt 경로 재사용 |
| 공개 API thread target | `src/server/routes/agents_turn_target.rs:65`에서 직접 bound 밖 403 | T2에서 남은 기능 구현 |
| external Herdr idle 시작 | `src/services/discord/router/message_handler/headless_turn.rs:159` → `src/services/turn_host.rs:191` | T1에서 기존 executor까지 연결 |

<!-- doc-path-refs: external 99_Skills/ -->

## 2. 진입점 전수표와 모델 값의 정본

이 절의 표가 값 범위의 유일한 설명 정본이다. 모델 config는 실행에 필요한 값을 선언하되 다른 보고서에서 독립적인 범위를 만들지 않는다. 모델의 공통 인계는 하나의 받아들인 ticket을 투영하며, 별개 요청 간 DB dedupe·다중 original 경합은 실제 진입점 시험으로 보완한다.

| ID / 진입점·file:line | 현재 판정·효과 | 목표 및 모델 값·행동의 코드 근거 |
|---|---|---|
| E01 실제 라우트 `src/server/routes/domains/runtime.rs:133,135`; `src/server/routes/agents.rs:658`; `src/server/routes/agents_turn_deliver.rs:133` | start와 deliver가 공통 target resolver에 진입 | 두 endpoint 공통 권한·actual child 인계. 단일 ticket은 두 route가 공유하는 하위 흐름의 투영 |
| E02 호출자·본문 `src/server/routes/agents_turn_deliver.rs:124,148,160,174`; `src/server/routes/agents.rs:659,742`; `src/server/routes/auth.rs:213,260` | deliver의 HumanInputCaller와 명시 author, start의 기존 서버 인증·metadata agent 일치 검사 | route마다 현재 인증 의미 보존. start에 deliver 전용 author/extractor가 이미 있다고 가정하지 않음. API source 문자열은 agent/parent 권한 증거가 아님 |
| E03 agent binding·provider `src/server/routes/agents_turn_target.rs:15,36,65,107,117` | agent 존재·DB bound 비교, provider mismatch 거절. binding 조회 오류도404로 합치는 기존 동작은 넓히지 않음 | `Target`은 boundChannel/boundThread/foreignThread/foreignChannel/parentUnavailable/parentMalformed. foreignThread는 wrong parent·확정 missing parent·invalid kind 거절을, parentMalformed는 손상·상충 관계의 미확정 거절을 대표. boundChannel은 기존 경로 제어군 |
| E04 thread 관계 `src/services/discord/session_runtime/channel_routing.rs:276,280,296,300`; `src/services/session_host/surface_policy.rs:6,27` | Option helper는 오류를 None으로 합침. checked helper는 오류 보존 | checked Discord 실제 thread kind+parent와 요청 agent의 현재 bound parent 비교. parentUnavailable은 조회 실패·timeout·상충. checked helper는 부모 객체의 이름 조회 성공을 보장하지 않으므로 추가 부모 조회가 필요하면 그 실패도 거절 |
| E05 runtime 선택 `src/services/discord/health/runtime_resolve.rs:150,167,188,191,200,233`; `src/services/discord/health/headless_turn.rs:100,124` | explicit child 우선, worker는 checked parent 후보, gateway는 live probe | 검증된 owner parent에서 runtime 선택, turn은 child 유지. 조회 실패를 임의 단일 runtime fallback으로 넘기지 않음 |
| E06 Discord 첫 메시지 `src/services/discord/router/intake_gate.rs:728,736,743,752` | child restore, 없으면 parent restore·path로 bootstrap | `InitialSession`은 live/absent/booting. `Authorize` 뒤 `Prepare`가 필요하면 이 기존 순서를 재사용. core 비어 있음만으로 absent 확정 금지 |
| E07 thread session 준비 `src/services/discord/session_runtime.rs:505,513,527,532,562,596,602,631` | actual child entry 재확인, 저장된 worktree 재사용 후 fresh 생성. worktree 실패 시 parent path fallback은 main 기존 동작 | 준비와 payload 효과 분리. 실제 native launch1은 core entry1만으로 증명하지 않음. #8118이 host 상속·Herdr 준비를 정본화. 새 cleanup/fallback0 |
| E08 dispatch thread 접점 `src/services/discord/router/message_handler/intake_turn/adk_thread.rs:4,61,126,166`; `src/services/discord/router/message_handler/intake_turn.rs:33` | target redirect, 접근성·bootstrap 접점, claim 뒤 route publish | adk_thread 단독 호출은 Discord 첫 메시지 전체가 아님. API 기존 thread 지정에 dispatch thread 생성·부모 queue edge를 불필요하게 합성하지 않음 |
| E09 child claim/transition `src/services/discord/session_transition.rs:165,198`; `src/services/discord/router/turn_start.rs:703`; `src/services/discord/router/message_handler/headless_turn.rs:226` | channel별 transition과 reserved mailbox claim | 같은 actual child로 serialize 후 세션·holder 재확인. lookup은 무거운 core lock 밖. race loser의 원본도 기존 queue/owner로 반환 |
| E10 human delivery `src/services/discord/health/turn_deliver.rs:36,70,129,151`; `src/server/routes/agents_turn_deliver.rs:236,268`; `src/services/discord/health/turn_deliver/prepared.rs:1,9,60`; `src/server/routes/agents_turn_deliver/legacy_origin.rs:20,31,37,54` | remote author preflight, exact origin admission, 효과 없는 prepared future 생성 후 실행 | child bootstrap/restore/worktree 효과는 GUID admission 뒤 returned future 또는 accepted owner에서만 실행. no-origin 호출은 별도 request identity이며 재전송 dedupe를 약속하지 않음 |
| E11 start·기계 입력 `src/services/discord/router/message_handler/headless_turn.rs:125,144,159,203,926,1067`; `src/services/discord/queue_io/input_owner/start.rs:18` | replay/owner admission 후 generic start는 자기 턴; iMessage identity는 사람 입력 분류 | API라는 이유로 전부 사람 입력으로 바꾸지 않음. 기존 origin/source/token/obligation 유지 |
| E12 tmux 입력 `src/services/discord/health/turn_deliver/inject.rs:309,312,385,427,644,656,694`; `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:533,579` | idle transcript fast path와 busy owner 입력, transition·row·mailbox 재관측 | `Submit`은 기존 actor/physical grant 아래 한 번. NotSent만 handback, 불명 effect는 blind replay 금지 |
| E13 Herdr 입력/시작 `src/services/discord/health/turn_deliver/inject/herdr.rs:41,47,79`; `src/services/turn_host.rs:151,177,191,198`; `src/services/discord/router/message_handler/provider_dispatch.rs:275,338,343,553,596` | busy Claude exact pane 주입 존재. idle start는 ExecutorNotWired 거절; 뒤 executor는 이미 있음 | `actualHost`/`desiredHost`는 tmux/herdr. 살아 있는 old tmux는 parent H라도 보존. 정책을 actual locator로 치환하지 않음 |
| E14 부팅·복원 `src/services/discord/recovery_engine/restore_inflight.rs:677,722,949`; `src/services/discord/watchers/lifecycle/restore.rs:198`; `src/services/discord/health/turn_deliver.rs:129`; `src/services/discord/session_runtime.rs:562` | DB/inflight/queue/source로 세션 복원, prior-process H는 hold | 모델의 bounded crash와 `Restart`/`ShowFailure`는 custody를 유지하거나 실제 보이는 실패로 종결. ready 재관측은 `Ready` 포트. 실제 큐 파일 원자성은 기존 입력 모델·시험 계약 |
| E15 깨우기·종결 `src/services/tui_o/writer/host.rs:1007`; `src/services/discord/runtime_bootstrap/o_writer_host.rs:397,402`; `src/services/discord/queue_io.rs:370,379`; `src/services/discord/runtime_bootstrap.rs:35,41`; `src/services/discord/session_transition.rs:132,145,153`; `src/services/discord/queue_io/input_owner/drive.rs:88,106,118`; `src/services/discord/queue_io/backstop.rs:672,707,733` | writer ready·active reconcile·transition release·turn event에서 actual child owner 깨움 | 모델 ready/submit/reply 공정성은 이 기존 owner 이벤트를 포트로 둠. 무작정 timer가 풀린다고 가정하지 않음. 원본 custody/queued 보장은 기존 InputOwnership/F7 모델과 연결 |
| E16 출력 `src/services/tui_o/channel_policy/children.rs:1`; `src/services/discord/router/message_handler/headless_turn.rs:1067` 이후 기존 producer | child의 writer/receipt/실제 native source로 게시 | `Reply`는 child에 한 번. #8118 O-b 생산 활성 전에는 이 포트 완료를 주장하지 않음. legacy 개별 수리0 |
| E17 stop·cancel `src/services/discord/commands/stop.rs:39,75,134`; `src/services/discord/herdr_terminate.rs:186`; `src/server/routes/agents_stop.rs:28,39,68,99`; `src/server/routes/queue_api.rs:209,226,230`; `src/services/discord/commands/control.rs:612,637,668,711,749` | 기존 cancel token·exact host settlement, waiting input 취소 | 보유 input이 기존 취소/실패로 끝날 수 있음. parent/sibling 취소0. agent stop에 새 thread override 권한0. 새 close/kill 권한0 |
| E18 clear·restart·queue reset `src/services/discord/commands/control/clear_teardown.rs:59,70,81,94`; `src/services/discord/commands/restart.rs:150,216,293`; `src/services/auto_queue/runtime.rs:485` | clear는 transition을 유지, restart는 seed 시작, H 일부 관리 거절 | shared target/host 정책과 경합 시험. 관리 동작 parity 자체는 #8128; thread actual identity는 #8118 H2 소비 |
| E19 결과 관측 `src/server/routes/agents_input_result.rs:56,71`; `src/services/agents/input_result.rs:69,79,90,102` | read 결과는 bound channel만 검색, delivery status는 소비 증거 아님 | 현 API의 child origin 404를 유실0 증거로 쓰지 않음. 본 설계는 read 권한 확장을 몰래 추가하지 않으며 child queue/inflight/native/posted body를 직접 join |
| E20 placement 실제 경로 `src/services/discord/router/intake_dispatch.rs:229,357,413,442`; `src/services/cluster/intake_router_hook.rs:388,392`; `src/services/cluster/intake_router_hook/placement.rs:97` | Discord는 IntakeSubmission을 admission하여 local permit 또는 durable routed outbox/worker로 보냄. #8115는 이 NoOwner 배치에서 소비 | cold/remote API도 기존 admission/placement를 지나야 첫 메시지와 같음. bootstrap→공개 headless 직결만으로 #8115 소비를 주장하지 않음. 준비 포트는 placement/forward/restore 성공 또는 visible failure를 추상화 |
| E21 routed provenance `src/services/discord/router/message_handler/intake_turn/worker_entry.rs:14,130`; `src/services/cluster/intake_router_hook/model.rs:132,161`; `src/db/intake_outbox.rs:30,76`; `src/services/cluster/intake_worker.rs:648`; `src/services/cluster/intake_worker/o_input.rs:80` | 현 DTO/outbox/worker는 Discord user_text/reply context 등을 운반하며 API headless source/origin/metadata/reserved ID를 위한 필드는 없음 | 기존 transport에 optional typed ingress context를 왕복 적응. 임의 text/reply_context/execution_requirements JSON을 API provenance로 해석하지 않음. absent는 현 Discord/legacy 의미 보존. 다중 node DB/네트워크 원자성은 모델 포트 밖 시험 |
| E22 현 headless placement 우회 `src/services/discord/health/headless_turn.rs:351`; `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:621,638,650,909,912`; `src/services/routines/agent_executor/launch.rs:198`; `src/services/scheduled_messages.rs:429` | 공개 start·context가 있는 owner 입력 및 기존 internal routine/scheduled 시작은 headless로 직접 감 | 본채널·internal routine/scheduled의 기존 경로 전체를 이 이슈에서 갈아엎지 않음. 신규 thread API의 cold/remote만 E20/E21로 연결 |
| M01 모델 제어 | 현재 거절과 제안 인계를 비교 | `Mutation`은 before/none/parentOpen/doubleSubmit/dropRestart/noReadyWake/policyOverridesLive. before는 현 target 거절, 나머지는 한 원인을 제거하는 검증용 변형이며 생산 설정이 아님 |
| M02 단일 ticket 모델의 내부 상태 / E06, E09–E16 | 요청, custody, 제출, 응답·실패와 crash의 투영 | `phase`는 request/authorized/held/queued/submitted/failurePending/uncertain/done/failed/denied/dropped. accepted/retained/visibleFailure/CanCrash는 BOOLEAN, delivered는0..2(2는 duplicate 변형만), reply는0..1, status는0/200/403/503(200은 custody 확보 뒤), session은 absent/booting/live, usedHost는 none/tmux/herdr, launches/crashes는0..1 |
| M03 동시 first-start 모델 / E06–E09, E14–E16 | 같은 child의 API start/deliver 및 Discord original 경합 | N은2/3, Ids는1..N, Mutation은 none/staleRead, phase[i]는 queued/peeked/preparing/starting/submitted/done/failed, snapshot[i]는 none/absent/live, owner는0..N, session은 absent/booting/live, launches는0..N, delivered[i]/reply[i]는0..1, failed는Ids의 부분집합. GUID 동일성은 이 대칭 ticket 모델 밖 |

### 입력 출처 분류표 — 전수표의 일부

| 출처 / 코드 좌표 | 소유·identity | 보존할 처리 |
|---|---|---|
| generic API start, routine/scheduled seed: `src/services/discord/router/message_handler/headless_turn.rs:125,203`; `src/services/agents/input_result.rs:21`; `src/services/discord/router/message_handler/headless_turn/replay_start.rs:203`; `src/services/discord/queue_io/input_owner/start.rs:18`; `src/services/routines/agent_executor.rs:129,268,582,731` | 기계가 요청한 자기 턴. 일반 start origin_id는 추적값이며 새 HTTP 호출의 reservation은 별개 | 사람 busy 입력으로 임의 승격하지 않음. 일반 origin_id만으로 재호출 dedupe 보장 없음. process fresh 및 scheduled snapshot scope 불변 |
| API deliver: `src/server/routes/agents_turn_deliver.rs:133,160,236,268`; `src/services/discord/health/turn_deliver.rs:59,70` | 인증된 remote caller + 명시 author의 사람 입력, source/origin/actual child | owner/allowed user preflight 유지. source가 imessage여도 caller 권한을 대신하지 않음 |
| iMessage 답장: `src/services/discord/router/turn_start.rs:81,88`; `src/services/discord/queue_io/input_owner/origin.rs:48,59,68`; `src/services/discord/queue_io/input_owner/start.rs:21,43,47`; 저장소 밖 `99_Skills/imessage-send/scripts/imsg_inbound.py:220,229` | source=imessage와 GUID, routine/scheduled 아님인 local TUI owner에서 child+GUID 안정 ticket·retry_original 사용 | deliver는 GUID를 전송. 조사한 start 호출은 GUID를 안 보내므로 외부 스킬 후속에서 target child 등록과 start origin 전달을 함께 맞춤. non-TUI replay는 activation/support 조건이 있어 전체 provider/host 재시도 보장으로 일반화하지 않음 |
| Discord: `src/services/discord/router/intake_gate.rs:728,764`; `src/services/discord/router/message_handler/intake_turn.rs:33` | 실제 child/message ID의 intake original; bot dispatch는 dispatch/message dedupe 별도 | 같은 child bootstrap을 공유하되 API GUID와 Discord message ID를 body 문자열로 합치지 않음 |
| TUI: `src/services/discord/router/message_handler/intake_turn.rs:1751,1754,1768`; `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:533,579`; `src/services/discord/health/turn_deliver.rs:129,151` | 터미널 사람이 쓴 native 턴과 외부 API 입력은 별개 소유 | busy/unknown을 무시해 새 native session을 만들지 않음. current intake는 H를 제외하는 조건이 있으므로 #8114 완결 계약 소비를 확인 |

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

### 단일 권위 대조 — #5071

[릴레이 상태 계약](relay-state-contract.md)의 mailbox active-turn lease가 실행 권위이고 row는 파생 증거라는 원칙을 유지한다. target 검증은 권한/목적지 선택이며 실행·배달 권위를 만들지 않는다.

| 판정/행동 | 재사용하는 기존 권위·코드 좌표 | 새로 만들지 않는 것 |
|---|---|---|
| 실행 claim | `src/services/discord/router/turn_start.rs:230,249,262`의 mailbox claim·episode nonce | session map/HTTP200만으로 별도 active 권위 |
| 원본 보유·retry | `src/services/discord/queue_io/input_owner/start.rs:43,47,51`, `src/services/discord/queue_io/input_owner/origin.rs:48` 및 기존 owner queue | parent별 dedupe cache·본문 fingerprint dedupe |
| 배치/원격 인계 | `src/services/discord/router/intake_dispatch.rs:229,442`, 기존 routed outbox/worker claim | 새 transport·비영속 전달 경로·local 선제 launch |
| 물리 입력 | `src/services/discord/health/turn_deliver/inject.rs:427,644,656`, `inject/herdr.rs:41,79`, 기존 provider dispatch owner | API direct pane key 전송·ACK 불명 blind retry |
| child 답 게시 | `src/services/tui_o/writer/host.rs:201,555,1007`의 기존 writer/adoption·lease, child 실제 source | parent writer alias·API 별도 본문 게시·legacy 수리 |

## 4. 호스트별 동작과 #8118/#8128 접점

| 사용자 동작 | tmux | Herdr | 담당과 수용 |
|---|---|---|---|
| API idle start / deliver의 idle 시작 | 기존 own-turn executor | 현재 일반 refusal로 차단, 뒤에는 기존 Herdr executor가 있음 | #8125 T1. intake와 같은 switched admission을 사용하되 O-ready·capacity·hosted row·provider switch 거절 보존. 단순 refusal 삭제만으로 완료하지 않음 |
| busy 사람 입력 | 기존 owner/physical input | 기존 exact Bound Claude pane injector 존재. 공통 operation 적용 범위는 #8114 이후 확인 | #8125는 재사용·route 배선과 결과 보존. Codex를 Claude injector로 우회시키지 않음; #8114/#8118 계약과 실제 provider별 시험 필요 |
| 새 thread 준비와 host 선택 | 기존 tmux bootstrap | 부모 host를 상속한 child의 기존 launch/admission | #8118 H1 정본. #8125에 별도 parent endpoint fallback을 만들지 않음 |
| 살아 있는 old tmux child, 부모 H | actual tmux 유지 | 정책만으로 live target 전환하지 않음 | #8118 계약 소비. 다음 새 native session부터 상속, live 이사는 #8077 별도 |
| child 답 게시 | child 기존 출력 | child 기존 출력 | O-a만으로 완료 아님. #8118 O-b 활성·child O-ready 통합 증거 필요 |
| clear/restart/진단·관리 reset | 현행 의미 | 기존 차이 추적 | #8128 src/server/routes/D/E 등 담당. 본 API 변경이 close·cleanup 권한을 넓히지 않음 |

`inject.rs:312`의 tmux 조건은 idle transcript 제출 fast path다. 이를 “Herdr 입력 없음”으로 표기하거나 단순 제거해 Herdr를 tmux driver로 보내지 않는다. busy H는 `inject/herdr.rs`의 실제 row/pane pin을 사용하고 idle H는 공통 headless own-turn start를 사용한다. T1의 시작·idle deliver 차이는 #8128이 #8125로 지정한 기능이므로 #8128 추적만 남겨 본 이슈 완료로 보고할 수 없다.

## 5. 부팅·재시작·취소·경합

재시작 후 메모리 map이 비어도 native 세션 부재가 아니다. DB hosted row·inflight·source·저장된 worktree와 기존 restore를 확인한다. Pending/unknown·old process row를 fresh 권한으로 승격하지 않는다. runtime 미준비 거절은 accepted가 아니며, accepted 뒤 준비/전환을 기다리는 입력은 기존 durable custody가 유지되거나 visible terminal 실패가 있어야 한다. 실제 wake 없는 `queued` 응답만으로 liveness를 주장하지 않는다.

동시 API 두 건과 Discord 첫 입력 한 건은 같은 child transition/owner admission을 사용한다. winner만 세션을 준비하고 race loser는 준비된 세션을 재사용하며 원본을 각각 보유한다. worktree/native 시작 계수와 입력 효과 계수를 분리한다. 같은 GUID retry는 같은 원본이고 서로 다른 GUID 세 건은 각기 보존된다. mailbox claim 이전에 dispatch parent edge를 publish하지 않는다.

clear·stop·HTTP cancel·restart가 끼면 existing token/transition/exact locator 판정을 재관측한다. 이미 취소된 원본에 늦게 키를 넣지 않고, effect 불명인 입력은 NotSent로 바꾸어 재전송하지 않는다. 기존 NotSent handback·Unconfirmed no-resend 의미를 그대로 유지한다. parent나 sibling의 queue/source/pane을 대신 지우지 않는다. 기존 관리 경로 결함의 처분은 E17/E18과 #8118 H2/#8128에 남기며 본 이슈에서 kill/reset/삭제를 추가하지 않는다.

## 6. 구현 조각과 예상 크기

새 trait·Core·session host 프레임워크·origin DB를 만들지 않는다. 기존 `AgentTurnTarget`, owner-channel starter, thread restore/bootstrap, owner actor, provider dispatch, Herdr operation, obligation을 연결한다. giant 파일에는 선언·호출 배선·거절 분기만 두고 정책/준비 본체는 기존 작은 소유 모듈 또는 그 child에 둔다. 현재 `agents.rs` 1,158줄, `headless_turn.rs` 1,545줄, `intake_gate.rs` 1,388줄, `session_runtime.rs` 1,172줄, `intake_turn.rs` 2,321줄이며 본문 블록 이동으로 수치를 맞추지 않는다.

| 리뷰 단위 | 생산 변경 후보 | 예상 prod | 선행·완료 경계 |
|---|---|---|---|
| T1 external Herdr start | `turn_host.rs`, headless turn의 admission 연결·작은 child, 기존 provider dispatch/health turn deliver 배선 등 | 6~10파일 / 순 +350~650 | 통합 설계대로 #8114 완결 사용자 계약과 #8118 O-b 배포·수용 뒤. 기존 bound channel 실제 route→기존 H executor까지 열기. provider switch·O-ready·held/cancel 거절과 tmux 회귀 보존 |
| T2a live local API thread | `agents_turn_target.rs`, 두 route 배선, health owner-channel/human request 적응, API 문서 계약 | 7~11파일 / 순 +350~600 | T1·#8118 H1 생산 surface/host 상속·O-b 배포·수용 뒤. 검증된 live local child에 실제 입력을 완결. cold/boot-unverified/foreign-owner는 mutation0·명시503으로 유지. bootstrap/transport dormant 코드0 |
| T2b cold/remote API thread | 기존 intake admission·DTO·routed outbox·worker의 typed context 왕복, child 준비 공통 함수·route 활성, 필요 시 nullable migration | 15~25파일 / 순 +800~1,200 추정 | T2a 뒤 E20/E21 placement/forward/restore까지 활성. #8115 preferred node 및 NoOwner·remote owner·worker retry/재시작 시험. 현재 source 포맷/SQL 적응량이 불확실하므로 착수 전 확정 산정 |
| 저장소 밖 수용 단계 | imessage-send 스킬의 발신 thread target 등록 및 start GUID 전달 | 이 저장소 prod 0 | API 배포 후 담당 코디네이터가 별도 수정·실제 답장 관측. 이 레그 수정0 |

초기 통합 설계 T1/T2만으로는 cold/remote의 provenance/placement 적응량을 과소평가하므로 T2를 live local과 cold/remote로 분리한다. 세 조각 각각 prod 30파일/순 +1,500 이내, 구현 전 확정 추정이 +1,200을 넘으면 PLAN_READY로 재분할한다. T2b의 다음 분할 경계는 기존 transport의 typed context 왕복과 실제 worker 소비를 포함한 기반 단위, 그 기반을 사용하는 공개 cold/remote route 활성 단위다. 기반을 무호출 dormant 코드로 두거나 경계만 맞추려고 필수 시험을 빼는 안은 허용하지 않는다. 실제 활성 소비자를 한 단위에 담을 수 없으면 별도 설계 결정이 필요하며 크기를 통과했다고 간주하지 않는다.

T2a만 착지한 동안 #8125는 부분 완료다. no-session/remote와 실제 iMessage 수용은 T2b 뒤에만 완료로 보고한다. T1이 H1 소유 정책을 고쳐야 하거나 T2가 #8118 계약을 바꿔야 하면 DESIGN_DELTA다. 구현 착수 때 #8118 정본의 현재 head와 핵심 심볼 드리프트를 확인해 중복 구현·상호 의존을 피한다.

## 7. 시험 계획과 실제 수용

아래는 구현 레그가 실행할 계획이다. 설계 모델의 GREEN을 Rust route·DB·실제 문자 GREEN으로 보고하지 않는다. fixture는 운영 설정/DB/파일을 물려받지 않는 격리 환경에 두며 첫 cargo 전에 lane headroom·seed_target·cargo-shim 규칙을 적용한다.

| 묶음 | 수정 전 RED / 수정 후 GREEN | 원인 제거 뮤턴트 |
|---|---|---|
| T1 실제 HTTP 진입 | 실제 runtime router의 start와 idle deliver가 H에서 executor_not_wired로 거절 → 같은 입력이 H executor1, tmux/process fallback0, child/parent 목적지 정확 | 일반 refusal 복원, provider switch/O-ready 거절 제거, H를 tmux driver로 전송 |
| T2 권한 | bound parent 아래 실제 public/private/news thread가 두 API에서403 → 허용 및 실제 child start/delivery. 기존 bound channel 응답 유지 | exact-bound-only 복원, payload parent 신뢰, Option 오류를 허용, provider를 child direct binding으로 찾기 |
| 거부 부작용 | foreign channel/thread·category·parent 없음403; parent timeout/REST Err/상충503; author·caller·metadata agent·provider mismatch 기존 거절 | parent lookup 오류 swallow, live child row로 agent binding 생략, 503을 403/성공으로 평탄화 |
| same-child 경합 | no-session child deliver2+Discord first1, barrier로 lookup/transition/claim 순서 제어 → worktree/native 시작1, distinct original 각 소비1, parent/sibling IO0 | child 재확인 제거, child→parent queue/source/turn key 치환, bootstrap과 start 둘 다 payload 제출 |
| placement·transport | 실제 API cold/remote→IntakeSubmission→NoOwner preferred placement/local permit 또는 durable outbox→worker→child 준비. #8115 슬롯 미광고 preferred 제어군, live foreign owner, forward/retry/restart 후 source/origin/metadata/reserved ID 그대로. absent legacy decode 유지, present malformed context는 효과0·visible failure | headless 직결로 placement 우회, context 누락·child→parent 치환·identity 재생성, API를 Discord source로 변환, malformed를 absent로 downgrade, 임의 reply_context JSON을 trusted context로 해석 |
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
| 권한 우회 | caller·author·agent binding·실제 checked parent를 모두 통과 후 준비. forged metadata/live row/dispatch map은 권한 아님. 두 route negative mutation0 시험 |
| 부모 조회 실패 | Result 오류 보존, 미확정503·effects0. parent 이름 조회와 관계 조회를 혼동하지 않음. #8118 provenance 경계의 생산 API는 구현 착수 시 재확인 |
| 경계 시각·경합 | actual child transition 뒤 존재/holder 재확인, native 시작과 original 소비 별도 계수. multi-original·same-GUID 실험은 단일-ticket 모델 밖이라 필수 route 시험 |
| 옛 형식·재시작 | 미기록 child checked REST, stored worktree/hosted row/inflight 복원. core None은 absent 아님. recorded/REST 충돌은 거절; 기존 queue/receipt 포맷을 바꾸지 않음 |
| 문서로만 선언한 가드 | surface API는 O-a에서 시험 전용임을 명시. T1 실제 executor·T2 실제 route·PG/native effect 계수로 연결. main 드리프트 시 핵심 심볼 변경 확인 |
| 새 권위·배달 경로 | #5071 대조표의 mailbox·owner·outbox·physical gate·writer만 소비. target/type context가 독자 실행 권위를 만들지 않음. source/GUID 보존을 transport 실제 왕복으로 확인 |
| 잘못된 0/PASS | 모델/route/운영 증거 분리, 미실행·0 selector·PG skip·0 request 불합격. H busy 기존 존재를 누락0으로 오기하지 않음. child input-result404에 기대지 않음 |
| 범위 확장 | 호스트 상속 정본 #8118, 관리 parity #8128, live migration #8077. parent/sibling kill·삭제·reset0. 이 설계에서 legacy 결함 수리0 |

## 9. 0단계 상태 차트 인계

[상태 차트 색인](statecharts/README.md)의 입력 큐 `InputPath`/`InputOwnership`, 세션 수명 `TmuxSession`, O-a thread reader 모델을 대조했다. 기존 모델은 queue/owner/ready·session restore와 child reader를 각각 다루며 공개 API 권한에서 thread 첫 시작으로 들어가는 접합은 없었다. 새 연결 모델은 그 포트 사이의 권한→준비→한 번 제출→child 답 게시를 검증한다. 기존 입력·수명 proof를 새 API의 운영 증거로 승격하지 않는다.

실행 결과·수정 전/후 표·반례와 코드 대조·재현 명령·모델 밖 항목은 [연결 모델 보고서](statecharts/input-single-path/thread-ingress.report.md)에 둔다. 수동 실행한 19개 cfg는 제안/제어군 12 GREEN, 수정 전/원인 변형 7 expected RED다. bound thread를 모두 거절하여 accepted가 비는 Before도 별도 기능 불변식으로 RED이며, 빈 liveness PASS를 성공으로 쓰지 않는다. healthy 경합은 서로 다른 원본의 actual child 인계를 검사하며 같은 GUID HTTP dedupe·다중 node/outbox 원자성은 구현 시험에 남긴다.

모델의 값 범위와 코드 출처는 §2가 정본이다. 모델의 fairness가 실제 ready/owner/writer 이벤트를 대신 구현하지 않으며, 관계 조회·host inheritance·durable queue/native IO의 내부 원자성은 기존 포트 계약 및 구현 시험으로 확인한다. `.cfg.example`은 실제 scratch `.cfg`로 복사해 수동 검증했다. 이 레그는 제한된 baseline 항목 변경을 피하려고 `expected.tsv`를 수정하지 않았으므로 CI 자동 등록/PASS를 주장하지 않는다. 코디네이터가 구현 조각에서 정식 cfg/expected 등록을 승인해야 한다.
