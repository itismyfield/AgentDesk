# 루틴 실행 방식 네 가지 설계 r2c 최종 (#8117)

기준 main `5bcbb82297f5cc3ab8897b81ddb9891459e0321b`, worktree `inv/i8117-routine-modes`, 2026-11-12 KST. 조사·설계 산출물이며 **프로덕션 변경·cargo 빌드/시험·운영 실행은 하지 않았다.** 이슈의 「내용」·DoD가 수용 기준이다. 코드 좌표는 이 SHA 기준이다. r1 문서 head는 `e69051beeb6a268aba286461c3fb67c641ffcc2c`이고 r2 head는 `a81f5c9fe8e844cdb02762f51fe5c821a76be230`이며 r2b head는 `067808b903ef3ba10df403e599baa4101d831c23`이며 r2c는 마지막 리뷰 P1 적용·P2 이관으로 설계를 닫는다. 사전 판정 API·키 조회도 실행하지 않았다.

## 변경 이력 r2

리뷰 1라운드 CLEAN(P0/P1 0), 마지막 2라운드 ISSUES(P1 1/P2 8)를 아래에 반영했다. 더 이상 설계 리뷰는 없다. r2c 뒤 바로 PR1 구현으로 진행하며 남은 P2는 지정 조각의 RED·수용 기준으로 검증한다.

- P2-1 → 「스택 PR·조각·크기」: 기능 전체 한 PR 문구를 없애고 10개 스택 PR, PR당 1~2조각·30파일/+1500·PLAN +1200 미만으로 고정했다. E는 마지막이다.
- P2-2 → 「headless」와 D: primary mailbox 양방향 대기, `primary_busy`, routine-owned thread/DM만 actor retire, 반대 방향 입력 RED를 추가했다.
- P2-3 → 「상태 차트」: owner Submit를 만료/복구와 독립시켜 first-key 경합 및 cancel_after_key mutant를 검사한다.
- P2-4 → 「진입점 전수표」의 모델 열, 「상태 차트」: TerminalLeavesNothingQueued·unconditional LateDispatch·3개 비공허 mutant와 Consumed 분류를 추가했다.
- P2-5 → 「사건×방식 알림」: script/interrupted dashboard-only·classification TTL을 고정했고, headless 정책은 아래 r2b 사용자 결정3으로 대체했다.
- P2-6 → 「레거시 계약 행렬」·「thread」: 시작 전 일시 오류 60/600초 같은 thread deferral, primary fallback0, current_attempt_started_at 기준을 명시했다.
- P2-7 → 「C1/C2와 본문 비교」: 현재6 실물 비교, legacy는 terminal NO_REPLY만 억제·그 외 full 유지, 7개 이전은 B+C1만으로 분리했다.
- P2-8 → 「channel」: Codex 기존 Busy routes와 하나의 busy_participation 훅, unsupported Held→idle/deadline 처분을 고정했다.
- P2-9 → 「단순 스크립트」: 기존 process group/identity lifecycle을 재사용하여 소유 PGID TERM→유예→KILL, 정상 종료 hook·강제 재시작 한계를 적었다.
- P2-10 → 「giant 배치」: headless_turn.rs·terminal_outcome_delivery.rs·inflight.rs를 책임 순증0 목록에 추가했다.
- P2-11 → 「확정 UX」: r2에서 비교한 채널 표시·스크립트 등록 선택지는 아래 r2b 사용자 결정1·2로 닫았다.
- 사용자 승인 댓글(11-12 15:58/16:59 KST) → 「후속 사전 판정」: 도입 목적·현재7 조사만 보존하고 키 없음은 Off로 호출0·warn0·기동1줄인 #8127 gate를 pin한 후속으로 이관했다.

### r2b 사용자 결정 1·2·3 적용

11-12 16:13 KST 사용자 답(이슈 댓글·`user-decisions-1613.md`)을 적용했다. 새 리뷰 라운드가 아니며 열린 결정은0개다.

- 결정1 → 「channel」「확정 UX」「사건×방식 알림」「E/이전」: 채널 전용 표시·라벨/O 편집·별도 스레드를 제거하고 기존 Discord 입력과 같은 owner·표시·순서를 유지한다. 결과 귀속은 run API/대시보드에만 둔다.
- 결정2 → 「단순 스크립트 등록」「API」「결정 완료」: 동일 루틴 폼/API의 executable·argv·cwd 직접 등록을 확정했다.
- 결정3 → 「헤드리스 실패 알림과 N+1 대응」「진입점 전수표」「사건표」「H/크기」「상태 차트」: 기본N3의 알림/명령봇 전이를 분리하고 기존 Announce 게시→intake 경로·전송 불확실 처분을 고정했다. E 표시 비용은 줄이고 H를 별도 PR로 분할했다.

### r2c: 2라운드 P1 적용·PJ 후속 이관·P2 이관

- P1-1·새 방향 채택 → 후속 PJ/스택 PR: PJ를 #8117 밖으로 옮겼다. #8127 gate 착지 뒤 ref/API·서버 키 경로와 Off/NoAnswer/Answer를 pin한다. 호출마다 CLI 키 조회·자체 timeout·run별 fallback class 계약을 삭제했다. A는11파일/+700이다.
- P2-A → 전수표 빈 줄 제거·연속 표 행 검사. P2-B → RelogRun/두 뮤턴트 보강 결과를 모델 보고서에 기록한다. DB/재시작 재진입 자체는 H RED다.
- P2-C/D/E/H → 사건표/구현 RED 이관: primary/thread 생성0, non-turn reason/사건 키, 주입 전 channel 임계·paused·stale 알림, ACK 미확인/held activation 차단을 H/E0/G에 이관했다.
- P2-F → H의 전역 ingress author_is_bot 수리를 제거하고 후속 이슈 후보로 남겼다. 기존 Announce와 동일 처리한다. P2-G →8KiB 경계 RED를 #8127 뒤 PJ 후속으로 이관했다.
- 최종: 열린 차단 결정0. P2-E의 최소 알림 기본값 진행·확인 안내1건은 코디네이터 브리핑에 맡긴다. 추가 설계 리뷰는 없다.

## 결론과 선행 조건

루틴 정의의 실행 방식은 `headless | thread | channel | script` 하나로 고른다. 실행 방식과 process/tmux라는 백엔드를 구별한다. 현재 process 실행도 Discord 스레드를 만들고 일반 relay와 진단 logger가 각각 게시한다. **현재 운영 7개는 모두 유효 방식 `thread`로 보존 매핑**하며 process 선호·고정 스레드·일정·paused 상태를 그대로 유지한다. 구형 정의의 null-mode provenance를 보존하여 동적 DM action 등 과거 계약을 자동 승격으로 깨뜨리지 않는다. 별도 운영자 전환 명령이나 실행 중지 절차는 필요 없다. 가장 먼저 기존 metadata provenance만으로 B+C1을 착지하여 7개의 진단 소음을 없애고, 방식 확장은 스택으로 뒤따른다. 채널은 사람이 Discord에 입력한 것과 동일하게 처리하고, 스크립트 직접 등록은 확정했다. 헤드리스 연속 실패1..N은 알림만, N+1은 기존 명령봇 경로로 별도 대응 턴을 요청한다.

#8114의 `design-1.md`와 후속 `design-boundary-delta-1.md` 계약을 전제로 한다. `InputIdentity`, `CurrentSource::{Ready,AwaitedClear,Unknown}`, `ObserverOwnership`, 공통 ClaudeBusy operation과 기존 adapter의 key-time guard를 재설계하지 않는다. #8115의 기본 실행 노드 지정 에이전트 새 스레드 세션 수리가 완료된다는 것을 thread 방식의 전제로 둔다. 설계·fixture 준비는 병행 가능하고 channel 활성 호출은 #8114 착지 후 통합한다.

이미 main이 충족한 것은 다시 만들지 않는다: 동일 루틴 스레드 재사용(`agent_executor.rs:1010`), JS complete/skip/pause의 턴 0(`runtime.rs:675-785`), claim/in-flight 중복 방지(`store.rs:685-717,3168-3224`), owner original/ticket custody(`agent_executor/owner_input.rs:200-283`), process attempt 백엔드 복원(`agent_executor/execution.rs:19-29`), 실행 결과 API·대시보드 요약(`routes/routines/handlers.rs:178-199`, `RoutinesTimelineWidget.tsx:558-638`). 남은 것은 모드별 라우팅·게시 정책, channel Busy 참여/정산, 직접 command runner, 진단 가시성 및 호환 이전이다.

## 수용 계약과 턴 수의 의미

| 방식 | 실행·세션 | Discord에서 보임 | Discord에서 안 보임 | run 이력·대시보드 |
|---|---|---|---|---|
| headless | 매 실행 새 process 세션·synthetic session key. primary mailbox는 공유하므로 양방향 입력이 대기할 수 있음 | 실패1..N은 primary 알림만, N+1은 명령봇 기존 intake의 대응 턴1회. paused/stale/classification은 알림만. 업무 iMessage는 별개 | 본문·배너·진단·NO_REPLY·routine thread 생성0 | 실행/대기/primary_busy·원인·실패 카드(사전 판정은 후속) |
| thread | 루틴당 지정 스레드 하나. 매 실행 새 provider 세션 | explicit C2=최종 결과, legacy C1=terminal NO_REPLY만0·그 외 full 유지; 조치 알림 | 공통 run/script/count/checkpoint/action/prompt/preview/banner0. 중간 진행 전체 억제는 explicit C2만 | 진단·원문·답 링크/배달 상태 |
| channel | 에이전트 본 채널의 현재 세션. 없으면 기존 cold start; Busy면 같은 owner로 프롬프트 주입 | 주입 뒤 사람 Discord 입력과 같은 답·표시·순서; 주입 전 실패 임계/paused/stale만 non-turn P | 루틴 전용 진행/출처/공유 라벨·O 편집·별도 thread·원문 재게시·추가 루틴 전용 새 세션 진단0 | 같은 original/attempt의 수용·합류/다음 턴 이관·공유 답 참조·오류 |
| script | QuickJS의 종결 action 또는 직접 command 실행. AgentDesk provider 턴 0 | 없음 | agent/session/thread/성공 로그·stdout/stderr 원문 | 결과·stdout/stderr·종료 코드/신호·timeout·실패/paused 알림 카드 |

후속 PJ가 도입되면 prejudge_no_work skip도 provider 턴0이며 현재 #8117 실행 경로에는 gate 호출이 없다. ‘실행 한 번에 에이전트 턴 정확히 1’은 **agent action이 실제 수용되어 성공한 run의 원문 1회 실행**, 모든 run의 중복 실행 0으로 검증한다. JS complete/skip/pause, script 모드, 시작 전 실패·취소는 턴 0이며 성공한 agent 실행으로 세지 않는다. channel Busy는 한 프롬프트가 이미 진행 중인 native 턴에 합류할 수 있어 새 native 턴 수를 무조건 1로 강제할 수 없다. run에 `execution_count=0|1|unknown`, `completion_evidence`를 구별해 기록하고, unknown을 0/PASS로 표시하지 않는다. N+1의 명령봇 대응은 실패한 원 run의 retry가 아니라 별도 기존 채널 입력이며 원 run의 execution_count를 늘리지 않는다. `Consumed` 명령 처리는 `outcome_kind=command_consumed`, agent count0인 별도 종결로 투영하며 agent 실행 1 성공과 구분한다.

main은 Started 후 timeout/provider 오류에도 같은 run에서 새 턴을 재실행한다(`agent_executor.rs:361-515,653-815`, `store.rs:2383-2429`). 새 모드에서는 **positive NotSent인 제출 전 재시도만 허용**하고, Started/MayHaveSent 이후 새 turn retry·다른 agent fallback을 금지한다. 실패 알림과 다음 정상 예약 슬롯으로 종결한다. 이미 진행 중인 구형 run은 최초 snapshot의 구형 정책으로 drain한다. 활성 6개는 현재 retry=0/fallback 없음이므로 정상 이전의 재시도 동작은 변하지 않는다. 이것을 빼면 모델의 ‘한 번’은 현재 코드를 검증한 것이 아니다.

## 현재 동작 진입점 전수표

표에서 `routines/`는 `src/services/routines/`, `discord/`는 `src/services/discord/`다. 아래 모든 게시 경로를 새 mode의 presentation 정책이 덮어야 한다.

| 진입점·좌표 | 현재 실행/상태/소유 처리 | 현재 Discord 게시물 | 설정 영향  | 모델 action / 범위 밖 사유 |
|---|---|---|--- | --- |
| `src/server/mod.rs:2630-2680` runtime 구성 | script registry, store, logger, executor 생성. timeout·headless_fresh_agents는 boot-bound | 구성 자체 없음 | flag 변경은 재시작 전 executor에 반영 안 됨  | 범위 밖: runtime 설정 생성; Mode/Policy 상수의 코드 출처 |
| `src/server/mod.rs:2701-2707,2830-2852`; `routines/runtime.rs:115-123` 시작 유예 | startup grace 동안 due claim만 보류, poll/recovery 지속. 지나간 slot은 due에 남음 | claim 전 없음 | startup_grace_secs live; 0이면 유예 없음  | 범위 밖: scheduler 부팅 유예; Admit 전 in-flight fence를 전제 |
| `routines/store.rs:685-717,3168-3224` 예정 시작 | enabled·due·in_flight=NULL 행을 FOR UPDATE SKIP LOCKED, run UUID 생성+in_flight CAS. lease 1800초(:20) | 이후 runtime.rs:44-45 시작 로그 | strategy는 claim snapshot에 복사  | Init → Admit (정확한 run claim을 전제) |
| `src/server/routes/routines/handlers.rs:400-491`; `routines/store.rs:2197-2221` run-now | 동일 in_flight guard, loader/이전 검증 후 claim, execute_claimed_script_run 공유 | :465 시작, :483 outcome, :485 failure streak | helpers.rs:99가 boot config flag 전달  | Init → 동일 공통 action; HTTP 경로 차이는 범위 밖 |
| `routines/runtime.rs:47-84,126-151` launchd 이전 검증 실패 | failed+paused(migration_invalid), 닫힌 run만 결과 반환 | :75 outcome 진단 | strategy/flag 무관, agent 턴 없음  | FailBeforeSubmit(migration_invalid), paused 표시는 SkipOrPause 대표 |
| `routines/runtime.rs:165-272` JS 입력 구성 | agent context·observations·automation inventory·checkpoint 수집 | :242-251 logger js_inputs, observations/inventory 개수·checkpoint 요약 | strategy가 JS context에 전달됨(:259)  | 후속 PJ 자료 출처(현재 gate 호출 없음); 관측 payload/개수는 범위 밖 |
| `routines/runtime.rs:274-315,656-663` JS 평가 에러 | heartbeat 후 execute_tick, 공개 error는 고정 안전 사유, 상세는 tracing/result_json | due/manual 호출자가 outcome 로그 | 실패 pause knob가 종결 시 pause 여부 결정  | FailBeforeSubmit(js_error), terminal 기록/표시 검사 |
| `routines/runtime.rs:316-336,338-495` JS 결과 | action명·detail·prompt·checkpoint 변경 logger 호출 후 close_action | js_action: action·요약·프롬프트 최대 900자 | strategy/flag 무관  | CompleteScript / SkipOrPause / Admit 선택; prompt 내용은 범위 밖 |
| `routines/runtime.rs:675-709` complete | finish_run→fresh teardown→succeeded | caller outcome 로그 | JS만이면 agent 턴 0; fresh teardown은 persisted attempt 기반  | CompleteScript (agent0) |
| `routines/runtime.rs:711-748` skip | skip_run→fresh teardown→skipped | caller outcome 로그 | 동일  | SkipOrPause(skipped, agent0) |
| `routines/runtime.rs:750-785` pause | pause_after_run→fresh teardown→paused | caller outcome 로그 | 동일, 사람 조치할 pause 알림과 진단 결과 현재 혼재  | SkipOrPause(paused, agent0) |
| `routines/runtime.rs:787-829`; `routines/agent_executor.rs:110-195` agent | agent_id 필요, start_turn→consume_start; 오류는 transient defer 후 retry/fallback/fail | caller outcome 로그(실제 started도 status=running 로그) | execution.rs:11-17 backend 선택  | Admit → OwnerAccept → Submit; 후속 PJ는 #8127 뒤에만 접목 |
| `routines/agent_executor/execution.rs:11-46` backend 선택 | fresh+flag=true만 Process, 그 외 Tmux. Process Claude/Codex만 허용 | backend가 Discord 유무를 결정하지 않음 | **process→tmux 자동 fallback 없음**. 미지원 provider는 에러, 결함/실패시 flag fallback 안 함  | agent3 cfg 동일 상태 공간; backend/Discord 표면 차이는 범위 밖 |
| `routines/agent_executor/launch.rs:30-56`; `start_deferral.rs:41-68` 사전 검증 | binding/provider/primary channel/DM id 검증, runtime 등록을 thread·reservation 전 확인 | 사전 검증 자체 없음 | provider_not_ready/mailbox_busy만 transient  | FailBeforeSubmit 또는 positive NotSent; provider 구체성은 범위 밖 |
| `routines/agent_executor/launch.rs:58-108` 표면 라우팅 | dmUserId면 DM reserve; 아니면 routine thread resolve, 실패하면 primary channel fallback + notify bot | process든 tmux든 DM/thread/primary에서 답이 게시될 수 있음 | **headless_fresh_agents는 silent가 아니다**. flag와 무관하게 thread를 잡음  | thread 영구 실패=FailBeforeSubmit(thread_setup_permanent); transient=NotSent/RearmKnownUnsent |
| `routines/agent_executor.rs:1010-1079,1108-1209` 실행 스레드 재사용/생성 | saved id valid+권한 확인이면 재사용; invalid/권한 실패면 replacement create; id update 실패는 warning만 저장 | 새 routine thread 자체와 이후 turn 본문. 제목 routine <name> - <agent> | fresh/persistent 양쪽 같은 스레드 resolver  | thread_setup_permanent 실패 action; Discord create/update 원자성은 범위 밖 |
| `routines/agent_executor/launch.rs:111-160` intent metadata | run/turn/agent/provider/channel/parent/thread/dm/prompt/strategy/backend 저장, planned에 synthetic label | 이 객체 자체는 API/run result; logger가 일부 진단/preview를 재게시 | fresh-context 보장은 시작 확인 후만 true  | Admit의 같은 original/ticket 보관 전제; metadata bytes는 범위 밖 |
| `routines/agent_executor/launch.rs:161-163`; `owner_input.rs:165-190,222-283`; `store/agent_start.rs` owner admission | TUI owned target이면 durable run intent 먼저 기록→같은 original/ticket admission→accepted/pending. 프로세스·비owner는 legacy start로 | admission 자체는 owner 기존 표면/수명 계약. routine 진단은 별도 logger | process는 legacy; owner 경로는 새 세션 소유권을 routine에 주지 않음  | Admit → OwnerAccept (admission ACK≠native accept) |
| `routines/agent_executor/launch.rs:164-212` legacy start | turn_id를 run에 먼저 기록, DM 또는 owner_channel start_reserved 호출 | provider streaming·terminal relay로 중간 진행과 결과 | Process/Tmux를 metadata로 start에 전달  | Submit → NativeAccept → ObserveAccepted |
| `discord/router/message_handler/headless_turn/routine_metadata.rs:36-85`; `headless_turn.rs:487-525` fresh 경계 | fresh는 synthetic session-key basis, persist context boundary→provider clear→cold start. persistent만 continuity 유지 | 첫 답에 새 세션 banner 가능(아래 행) | missing strategy도 valid routine이면 fresh. Process는 fresh metadata에서만 인정  | 범위 밖: provider/session fresh isolation. PR3a/D RED 대상 |
| `routines/agent_executor/launch.rs:214-271` 시작 확정 | reserved turn mismatch 거절. actual Started+durable confirm 둘 다 성공해야 fresh_context_guaranteed | 자체 게시 없음 | Accepted/Pending/Consumed를 Started로 오인 안 함  | NativeAccept → ObserveAccepted (transport ACK를 success로 세지 않음) |
| `routines/agent_executor/launch.rs:273-285`; `owned_session.rs:13-90` fresh ownership | fresh non-process non-DM만 실제 receipt/session-key를 기록. 이름 추측 안 함 | 자체 게시 없음 | persistent/process/DM 제외  | SharedSessionPreserved; 실제 ownership stamp/OS teardown은 범위 밖 |
| `routines/agent_executor/owner_input.rs:95-137` start outcome | Pending/Accepted/Started는 running, Consumed만 complete_agent_run+succeeded(에이전트 턴 0 가능) | caller outcome 진단 | 후속 실제 observer가 필요  | ConsumeCommand / ObserveAccepted (Consumed agent0 별도 분류) |
| `routines/agent_executor/start_deferral.rs:6-9,105-125,143-235` transient 시작 재시도 | 60초 뒤, 최초 600초 window. turn=NULL·deferred metadata·attempt event, retry_count 안 씀. window 이후 일반 policy | caller outcome에 deferred reason/요약 진단 | recorded backend 유지; deferred는 원 attempt kind·DM target 유지  | NotSent / RearmKnownUnsent, 소진 FailBeforeSubmit(deferral_exhausted); 60/600 수치는 Expire로 축약 |
| `routines/agent_executor.rs:199-334,1561-1583` 최초 시작 실패 | max_retries>0면 retry 먼저, 아니면 다른 fallback agent 한 번, 마지막 failed | caller outcome 진단 | backend는 최초 for_new 결과, backend 전환 아님  | FailBeforeSubmit, positive NotSent 재시도만 |
| `routines/store.rs:2383-2429`; `agent_executor.rs:1282-1287` retry 예약 | 같은 run status=running, turn=NULL, retry_count++, owned session NULL; 60·120·…900초 cap | caller outcome 진단(run id·retry_count 등) | 결과에 backend 유지  | ProviderFailure → RetryStarted (BeforeRetry countercfg만 post-start retry) |
| `routines/agent_executor.rs:521-648`; `store.rs:1073-1111` retry due | prompt/agent 없으면 failed, due retry는 start_turn 재호출 | caller outcome 진단 | from_result가 Process 보존, 옛 missing field는 Tmux(:execution.rs:19-29). deferred만 원 kind·DM 보존  | RetryStarted; after는 positive NotSent RearmKnownUnsent만 |
| `routines/agent_executor.rs:653-815,1586-1606` 실행 중 실패 fallback | retry budget 후 fallback 다른 agent 한 번, prompt 없거나 fallback 실패하면 failed/pause | caller outcome 진단 | running fallback도 recorded mode 유지(:741). :738 dm_user_id=None은 기존 정책/결함이며 이번 확장 금지  | ProviderFailure; BeforeRetry만 새 input 허용, after는 terminal |
| `routines/agent_executor.rs:336-359`; `store.rs:1114-1130` poll | 모든 running agent lease heartbeat, owner reconcile, turn=NULL이면 retry 처리 | 결과 생길 때 server/mod.rs:2820 outcome | heartbeat는 Held까지 살아 있게 할 수 있음; 실행시각 deadline 별도 필요  | ObserveAccepted / ObserveNotSent / Expire; 실제 heartbeat는 범위 밖 |
| `routines/agent_executor/owner_input.rs:37-85,286-347` owner poll gate | exact run/turn/context 확인→start rejection 실패→unbound attempt observer 먼저 연결. bind 전 completion/timeout/retry 관측 보류; admission projection 실패면 같은 ticket 복원 | 직접 게시 없음 | bind/retry 확인 오류는 held이며 새 launch 금지  | OwnerAccept / ObserveAccepted; BeforeHeld 미종결 경로 |
| `routines/agent_executor/owner_input.rs:350-389` owner retry·bind | may_retry 기존 owner 판단만 사용, bind_observer에 exact attempt/source/logical turn 연결 후 projection | 직접 게시 없음 | timeout만으로 pending을 새 입력으로 복사하면 중복 위험  | ObserveAccepted / ObserveNotSent; exact source/attempt는 외부 전제 |
| `routines/agent_executor/reliability.rs:74-155` 완료 증거 | 해당 turn·since 이후 첫 nonblank session_transcripts, NO_REPLY면 NoReplyTranscript. 없으면 exact correlation/source turn의 empty_response quality event | routine logger가 preview 재게시; actual relay와 독립 | transcript/empty response가 완료 판단, headless 전달 여부는 bridge 권위  | CompleteAgent / ConsumeCommand; 본문/NO_REPLY 분류는 범위 밖 |
| `routines/agent_executor.rs:406-443` 정상 완료 | checkpoint/next_due 반영 complete_agent_run 후 teardown | server/mod.rs:2820 outcome + failure streak 검사(성공은 streak reset) | fresh만 자동 세션 teardown  | CompleteAgent (저장+NoSilentLoss) |
| `routines/agent_executor.rs:361-404`; `reliability.rs:26-31` provider error 응답 | owner may_retry gate 후 error-only transcript면 teardown→retry/fallback/fail | 이미 relay된 에러 응답+outcome 진단 둘 다 가능 | 실제 Started 이후에도 새 attempt 가능  | ProviderFailure |
| `routines/agent_executor/reliability.rs:33-71,157-218`; `agent_executor.rs:449-478` 조기 실패 | fresh만 cancelled/transport_error/prompt_too_long 30초 settle, exact turn_complete 있으면 실패 안 함 | caller outcome 진단 | persistent는 transcript/timeout까지 기다림  | ProviderFailure / CloseUncertainAtDeadline; 30/120초는 시간 추상화 |
| `routines/agent_executor/reliability.rs:220-289` pane liveness | fresh nonowner nonprocess만 started 120초 뒤, DeadOrAbsent/Qwen 확정 API 오류를 host guard 승인 시 실패 | 자체 게시 없음→poll 결과 진단 | Herdr/Process/Unknown은 이 tmux probe 사용 안 함  | ProviderFailure; provider별 probe/host guard는 범위 밖 |
| `routines/agent_executor.rs:481-515,991-1007,1228-1232` timeout | per-routine positive timeout 또는 default, latest started attempt 시각+timeout<=DB NOW. teardown→retry/fallback/fail | caller outcome timeout 진단 | owner retry 불허면 :446-448 continue로 timeout도 보류  | Expire → CancelUnsentAtDeadline / CloseUncertainAtDeadline |
| `routines/agent_executor.rs:818-883`; `execution.rs:101-159` teardown 라우팅 | owner_input 있으면 no-op; Process면 recorded provider/channel/turn만 cancel; 그 외 current routine fresh 여부 확인→controller | 자체 게시 없음, 기존 session/turn 종료 relay는 별도 | script close는 attempts에서 latest started/input_pending routing 복원; arbitrary JS result routing 금지  | SharedSessionPreserved; channel_teardown mutant로 금지 처분 검사 |
| `routines/session_control.rs:155-190,236-295` fresh tmux teardown | exact resolved target→기존 force_kill_turn/row host guard→session disconnect. recovery token remote-owned면 local kill 안 함 | 자체 게시 없음 | fresh-only; persistent 거절  | 범위 밖: OS exact kill; SharedSessionPreserved는 channel에만 |
| `routines/runtime.rs:834-878`; `execution.rs:101-124` JS 종결 teardown | Complete/Skip/Pause도 persisted owning attempt 조회, routing metadata는 JS 결과를 신뢰하지 않음 | 자체 게시 없음 | owner/process/newer work 보존  | CompleteScript / SkipOrPause / Admit 선택; prompt 내용은 범위 밖 |
| `src/server/mod.rs:2681-2696`; `routines/store.rs:3057-3132` boot recovery | expired running lease를 row-lock+재검사해 interrupted, matching in_flight만 clear | stored thread 있으면 recovery 로그. 없으면 tracing만(discord_log.rs:242-271) | strategy snapshot 아닌 현재 r join; backend 자체는 recovery 안 바꿈  | BeginRecovery → RecoverWithdraw → RecoverUnsent / RecoverUncertain |
| `routines/agent_executor.rs:908-987`; `store.rs:597`; `owned_session.rs` boot reap | positive owned_tmux_session+fresh only, other running run 있으면 skip, exact namespace token teardown | 자체 게시 없음 | Process/DM/owner ownership 없으면 reap 대상 아님  | 범위 밖: 기존 exact owned-session reap; 권한 확대0 |
| `src/server/mod.rs:2707-2720` periodic recovery | 동일 expired lease 종결. 동시 reclaim 때문에 **fresh reap 하지 않음** | recovery 로그 | 자동 kill 권한 추가 금지  | BeginRecovery → RecoveryFinish, known-unsent terminal queue0 |
| `routines/fresh_session_reaper.rs:25-59,74-96,124-153`; `discord/tmux_reaper.rs:620-682` orphan backstop | fresh+agent+no inflight의 primary/fallback deterministic names만 후보. kill 직전 row+pane 재조회, live/unknown/reclaimed면 preserve | routine 자체 게시 없음. 일반 reaper의 session 종료 알림과 혼동 여지 | persistent·DM·process 전용 label은 후보 아님; 현재 이름 기반 잔여를 확장하지 않음  | 범위 밖: 기존 reaper; 새 mode의 kill 권위로 사용0 |
| `src/server/routes/routines/handlers.rs:493-575`; `routines/session_control.rs:84-153` 수동 reset/kill | persistent만 허용. reset 기존 guarded clear; kill 기존 turn lifecycle. 실제 변경이면 run interrupted | API 직접 routine log는 없음; 기존 runtime/provider UI의 상태 변화는 별도 | fresh API는 conflict. 채널 모드에 이 권한을 이동시키면 사용자 세션 파괴 권한 신설  | SharedSessionPreserved; control API/legacy persistent는 범위 밖 |
| `src/server/routes/routines/handlers.rs:202-248,294-375` attach/pause/resume/detach | attach default=fresh, pause/resume/detach store 변경. detach는 in-flight 거절 | RoutineLifecycleEvent 로그: 부착·일시정지·재개·분리 | agent 없어도 script 루틴 가능; logger agent 없음이면 skip  | 범위 밖: definition CRUD; paused 종결 표시는 SkipOrPause로 대표 |
| `src/server/mod.rs:2739-2764`; `routines/discord_log.rs:286-324` stale paused | threshold>0만 scan, routine id+TTL dedupe | thread 있으면 일시정지 장기 알림, 없으면 tracing warn만 | stale_paused_alert_secs=0 disable  | NoSilentLoss의 표시 전제; TTL/알림 표면은 범위 밖 |
| `src/server/mod.rs:2766-2807` failure auto-resume | failure pause만 cutoff 후 자동 resume, manual/migration-invalid 제외 | tracing info만, lifecycle Discord log 호출 없음 | failure_pause_auto_resume_secs>0이면 terminal failure pause+후일 resume; 0이면 자동 pause 없음  | 범위 밖: 다음 definition state; 현재 run 재실행0 |
| `routines/discord_log/failure_alert.rs:47-104` 연속 실패 | latest stored terminal failed streak 정확히 threshold에서 한 번, interrupted 무시, nonfailed reset | routine·id·script·last_error·next_slot actionable 알림 | max_consecutive_failures 기본3(문서),0 disable; pause knob와 독립  | escalation RecordRun의 이력 count/reset 출처. 현행 exactly-threshold 알림과 새 N+1은 구별; TTL 자체는 모델 밖 |
| `src/cli/direct.rs:327-378`; `src/server/routes/agents.rs:1027-1064`; `discord/agent_handoff.rs:285-370` 명령봇 실제 게시 | send-to-agent 기본 start_turn=false 및 POST agents/{id}/message가 Announce backend로 실제 메시지 게시; --start-turn/handoff는 direct start로 별개 | bot=announce 실제 본 채널 메시지. 이번 N+1은 이 backend의 primary target/기존 권한을 재사용 | runtime 쓰기0인 조사; incident source는 이미 등록된 routine-runtime 유지 | escalation: RecordRun → LoadOutbox → PostOutbox; 실제 전달을 실행한 증거는 아님 |
| `discord/outbound/send_gate.rs:30-68,220-239,279-294,404-433`; `src/server/outbox_actionable_delivery.rs:22-38,117-133,158-192`; `discord/outbound/source_registry.rs:112` | 기존 source 권한·bot resolution·outbox→manual delivery. generic retry는 post-accept Unknown을 구별하지 못함 | 실제 Discord post. routine_consecutive_failures는 OperationalAlert 표지가 붙어 턴0 | N+1의 새 routine_failure_incident 목적 키만 분리; source/권한 확대0, Notify fallback0 | escalation: PostOutbox(ACK/Unknown); positive NotSent 분기는 구현 RED, 모델 생략. H의 같은-row 상태 확장이 필요한 현행 틈 |
| `discord/router/intake_gate/gate.rs:84-120`; `discord/dispatch_policy.rs:226-263`; `discord/router/intake_gate.rs:527,830-832,1325-1329` 봇 intake | 실제 Announce ID는 기존 예외로 허용, OperationalAlert/monitor는 거절; 실제 message/author id와 봇 취소/merge 정책 유지 | 기존 사람 입력과 같은 owner 응답 표면; 새 루틴 표시0 | 허용목록 추가·사람 origin 위장0; bot author는 실제 flag로 유지 | escalation: ExistingBotIntake; Notify0/Announce1의 코드 근거 |
| `discord/router/intake_dispatch.rs:784-824`; `discord/queue_io/input_owner/ingress.rs:395-473`; `discord/router/message_handler/input_payload.rs:92-113` | permit→기존 original admission/wake. :407-413의 bot 추론은 announce가 whitelist 밖이면 실제 author.bot를 잃는 좁은 결함 | 별도 routine 게시 없음 | H는 다른 Announce 메시지와 동일 처리; 전역 author_is_bot 수리는 후속 후보(P2-F), 기존 경로/권한 불변 | escalation: ExistingBotIntake; 기존 ingress 의미는 외부 전제이며 전역 수리는 범위 밖 |
| `discord/router/message_handler/provider_dispatch/owner_input.rs:831-920`; `discord/router/intake_dispatch/queued.rs:235-260`; `discord/router/message_handler.rs:310-329`; `discord/router/message_handler/intake_turn.rs:2173-2212`; `discord/router/message_handler/provider_dispatch.rs:465-510` | 같은 front/original→start_owner_front→handle_text_message→기존 provider execute | 기존 채널 답·표시·Busy/queue 순서 | N+1은 기존 명령봇 입력이며 routine-start로 복제0 | escalation: ExistingOwnerStarts; 실패 원 run의 Submit/RetryStarted와 별개 |
| `routines/discord_log.rs:90-231,407-457` 진단 로그 조립 | started/js_inputs/js_action/outcome을 routine_runs.discord_log_sections에 각각 저장, 저장 message id로 edit | `루틴 실행 로그` 코드블록: run/script/count/checkpoint/action/prompt/result/error/agent/retry/response preview | strategy/flag에 따른 suppress 없음; **process도 동일**  | Finish의 recorded/notified; Discord 문자열은 범위 밖 |
| `routines/discord_log.rs:545-650` 진단 실제 transport | 직접 outbound HTTP edit/send, without_idempotency; transient edit 실패는 error, permanent/confirmed missing은 replacement POST | 시작 summary가 편집되며 재게시될 수 있음 | agent 본문 relay와 별개 책임이며 삭제할 수용 대상 진단 경로  | 범위 밖: HTTP/outbound-port 기존 모델; run 원문 재실행과 분리 |
| `routines/discord_log.rs:460-542,652-720` log target/thread | saved thread 있으면 사용; 없으면 store 최신 thread 재확인→create+persist; 실패면 primary fallback | JS-only여도 agent 연결돼 있으면 log thread가 생김 | 새 headless/script는 이 logger 경로까지 막아야 Discord 0 가능  | 범위 밖: surface 생성; thread create 실패는 FailBeforeSubmit로 대표 |
| `routines/discord_log.rs:723-782` lifecycle/actionable transport | message_outbox_pg 및 TTL shared 경로 사용 | lifecycle/recovery/stale/failure notice | 새로운 알림 transport/dedup 만들지 않음  | NoSilentLoss의 notified; 실제 outbox 배달은 범위 밖 |
| `routines/discord_log.rs:95,123,157,204-208`; `discord_log/census.rs:4-85` 선례 | legacy-body-census는 시작·JS 진단 생략, outcome native verdict만 | `Legacy body census PASS/FAIL/INCONCLUSIVE...` 한 줄 | 이미 main에 사용자 결과만 남기는 특수 예 있음. 일괄 logger 삭제로 native verdict까지 버리지 말 것  | 범위 밖: census public-result 특례; 기록 보존은 NoSilentLoss |
| `discord/router/message_handler/headless_turn/replay_start.rs:87-96`; `provider_isolation.rs:594` silent metadata | silent_turn=metadata silent flag. routine launch metadata는 silent를 넣지 않음 | silent flag가 없으므로 일반 relay 게시 | backend=process와 silent는 직교  | 범위 밖: Discord presentation; agent3 cfg 동일 상태 공간 |
| `discord/router/message_handler/headless_turn.rs:1121-1144`; `turn_bridge/stream_tick/output.rs:403-425` 중간 진행 | typing indicator, provider stream→turn_bridge 기존 emitter | 에이전트 진행 문장·상태 panel/streaming text | logger 진단 제거만 해도 중간 진행은 남음. 결과만 표면 정책은 relay 쪽 metadata로 해결해야 함  | 범위 밖: C1/C2 presentation, 원문 수용 count와 구별 |
| `discord/session_banner.rs:107-125`; `tmux_watcher/streaming_session_banner.rs:20-37,66-87`; `turn_bridge/stream_tick/output.rs:414-423` 새 세션 | first logical answer chunk에 shared banner claim prefix | `🆕 새 세션 시작...` | 루틴 metadata별 suppress가 현재 없음  | 범위 밖: banner 억제 C1/C2, provider fresh와 구별 |
| `routines/agent_executor/reliability.rs:96-108`; `agent_executor.rs:1707-1722`; `discord_log.rs:1437-1441` NO_REPLY | no-reply는 완료 성공으로 분류되며 last_result=NO_REPLY; preview가 있으면 outcome도 재게시 | relay가 텍스트 NO_REPLY를 내고 logger preview도 재게시 가능 | 프로세스 bridge_tests.rs:199-211은 ordinary NO_REPLY transport 도달을 명시 단언  | 범위 밖: body projection. run terminal은 CompleteAgent |
| `discord/turn_bridge/headless_delivery.rs:272-323`; `terminal_outcome_delivery.rs:856-878` agent terminal | 기존 body source→lease/cutover→headless outbox, committed/ambiguous를 기존 권위로 정산 | user-facing assistant body. NO_REPLY 전용 제외가 여기 없음 | routine 결과 preview logger와 별개; 새 본문 emitter 금지  | Admit → OwnerAccept → Submit; 후속 PJ는 #8127 뒤에만 접목 |
| `discord/router/message_handler/headless_turn/process_mailbox.rs:35-62,72-109` process 완료 후 actor retire | captured actor/claim, bridge Finalized+producer exit 후 safe idle이면 retire; unresolved/cancel retain | 자체 게시 없음 | mode fresh process만; 종료 증거 없는 cancel을 cleanup 성공으로 오인 금지  | 범위 밖: ProcessMailboxScope/actor retirement, D RED 대상 |
| `routines/runtime.rs:275,328,787-829` 후속 예정 PJ 접점 | #8127 착지 뒤 Agent arm consumer만 추가; 현재 스택 호출0 | 후속 진단 API만, Discord 판정 본문0 | Off/NoAnswer/Answer·script 제외는 후속 RED | 보존된 미래 Prejudge 추상조건; 키 없음은 PrejudgeEnabled=FALSE |
| 신규 command runner(현재 없음), 재사용 `src/services/process.rs:82-95` | command 비0/signal/spawn 오류는 agent 없이 failed | 대시보드 결과, Discord0 | script mode만 | FailBeforeSubmit(command_nonzero) 대표 실패; OS child/PGID는 모델 밖, F 실제 격리 시험 |

## 운영 API 실물과 기존 7개

1. `GET /api/docs` → `logs/api-docs.json` (200).
2. `GET /api/docs/automation/routines` → `logs/api-docs-routines.json` (200; 14 endpoints). 목록·단일·runs API가 정의됨; 별도 run-detail endpoint는 없고 runs row에 result/log sections 포함.
3. `GET /api/routines` → `logs/api-routines.json` (200; 7 rows, 6 enabled + 1 paused, default_timeout_secs=1800).
4. 각 `GET /api/routines/{id}/runs?limit=10` → `logs/runs-{id}.json` (모두 200; 총 61개 row).
5. `GET /api/docs/integrations/discord` → `logs/api-docs-discord.json` (200). 명시된 read-only proxy `GET /api/discord/channels/{id}/messages`로 thread 실물 확인.
6. `GET /api/discord/channels/1552549227018850346/messages?limit=30` → `logs/thread-1552549227018850346.json`.
7. `GET /api/discord/channels/1521711275732631663/messages?limit=50` → `logs/thread-1521711275732631663.json` (실패 run의 진단 본문 관측).

## 루틴 7개 현재와 이전 기본값

모든 row가 `execution_strategy=fresh`, 고정 `discord_thread_id` 있음. `fallback_agent_id=null`; paused 1개만 max_retries=1, 나머지 0. 활성 6개의 최신 run은 모두 `execution_mode=process`, `fresh_context_guaranteed=true`, `completion_evidence=session_transcripts`. **process는 헤드리스 backend 의미이며 Discord 표면 없음이라는 새 headless 의미와 다르다. 현재 실제 표면은 thread이므로, 무중단 migration의 유효 새 mode 기본값은 7개 모두 `thread`, backend는 기존 선택을 유지한다. 정의의 legacy provenance는 남기고 실행 snapshot의 표면을 확정한다.** 채널/단순스크립트로 자동 재분류하면 동작 변경이다.

| 루틴 | id | 현재 상태·일정(KST) | 최신 run(KST) / run id | backend / thread id | 새 mode 기본값 |
|---|---|---|---|---|---|
| memento-daily-report | `83c4ce79-cd7a-4cf9-999b-4f90397a8302` | enabled; `0 9 * * *` | 11-12 09:00:22 KST; `186e74e8-990b-4c52-9fb0-764725f18adc` | process; `1565517284653670450` | thread |
| family-morning-briefing-yohoejang | `c7f218a2-cba5-4a80-a578-da22240b6f37` | enabled; `31 6 * * *` | 11-12 06:31:16 KST; `ed135c22-bac4-4d08-89d0-864714e70a24` | process; `1552549229782634537` | thread |
| family-morning-briefing-obujang | `5dfe0313-30f4-4cfc-8072-6b45d2f516b5` | enabled; `30 6 * * *` | 11-12 06:30:16 KST; `e4f43597-3513-48b3-8a0c-3e1fe00c33ad` | process; `1552549227018850346` | thread |
| memento-intake-check-20260925 | `9dd41ab3-23d7-4c47-af3f-176f38225b11` | paused; `57 8 25 9 *` | 09-25 08:57:06 KST; `e38799e7-5006-4287-a137-97ab2fac2898` | 미기록(구형 row); `1552466264629444610` | thread |
| memento-hygiene | `59883feb-9365-4367-8bee-4d1a1b061efa` | enabled; `0 3 * * *` | 11-12 03:00:01 KST; `debf55f5-de71-46e6-bff0-73f3ce2d6a08` | process; `1521711275732631663` | thread |
| memento-scope-audit | `d3c422dc-6d42-47a7-99e2-49420cc6bfc4` | enabled; `0 9 * * 0` | 11-08 09:00:04 KST; `9d392ed9-d150-44db-b6af-35256dc3780e` | process; `1568777096606785608` | thread |
| dependency-update-watcher | `12d15a46-8053-4ac9-94f4-079c9a1549c5` | enabled; `0 10 * * 1` | 11-09 10:00:15 KST; `a9c7e97a-9c03-4563-ba54-d1f0d1e64059` | process; `1566617955826012162` | thread |

`memento-intake-check-20260925`는 09-25 08:57 단 1회 succeeded이고 paused 상태다. 구형 run에는 `execution_mode`가 없으므로 process라고 단정할 수 없다(이슈의 “최근 실행 전부 headless” 일반화에 예외). 새 기본값은 thread·fresh, paused·next_due_at 등 원상 보존. `memento-scope-audit`/`dependency-update-watcher`는 과거 run의 thread id와 현재 row가 다르므로 migration 때 현재 row의 id를 보존하고 과거 run id를 덮어쓰지 않는다.

초기 보존 원칙(구체적 착지 순서는 아래 스택 PR/이전 절): (0) additive schema/read compatibility+진단 저장·노출을 먼저 출시, scheduler를 멈추지 않고 각 새 run이 claim 시 모드를 snapshot; (1) enabled 6개와 paused 1개를 자동 보존 매핑(effective thread/fresh, legacy provenance 유지)하되 due·status·lease·thread id 불변; (2) 진단 Discord 억제를 전부 활성화; (3) 낮은 빈도 dependency-update-watcher → memento-scope-audit → memento-daily-report → memento-hygiene → family-morning-briefing-yohoejang → family-morning-briefing-obujang 순으로 기존 다음 정기 실행을 관측. paused intake는 재개/실행하지 않고 API read compatibility로 확인. 관측 순서가 배치별 수동 설정 변경을 뜻하지 않으며 자동 일괄 보존 mapping이 기본이다. headless/channel/script 전환은 새 선택 기능 사용자의 명시적 편집일 때만.

## Discord 실물: 11-12 family-morning-briefing-obujang

run `e4f43597-3513-48b3-8a0c-3e1fe00c33ad`, 06:30:16 시작→06:34:16 succeeded. thread `1552549227018850346`, turn `discord:1552549227018850346:9100096699003098956`. run API는 `discord_log_status=ok`, `discord_log_error=null`, `discord_message_id=1570189955823837278`. result는 provider=claude, execution_mode=process, fresh_context_guaranteed=true, assistant_message_chars=353, completion_evidence=session_transcripts.

| 시각 KST | Discord message id | 실제 종류·크기 | 본문에서 확인한 요소 |
|---|---|---|---|
| 11-12 06:30:17.087 | `1570188944576880711` | 루틴 실행 로그 코드블록 1,912자 | 시작·JS 입력·JS action·프롬프트: run id/script ref, observations/inventory/checkpoint, action, [프롬프트] 원문 |
| 11-12 06:33:59.982 | `1570189879466393641` | 에이전트 relay 364자 | `🆕 새 세션 시작`, 중간 진행 문장들, 말미 `NO_REPLY` |
| 11-12 06:34:18.187 | `1570189955823837278` | 누적 루틴 실행 로그 코드블록 1,900자 | 앞의 시작·JS 입력·JS action·프롬프트를 다시 포함하고 outcome 추가 |
| 11-12 06:34:18.634 | `1570189957698682892` | chunk continuation/응답 preview 412자 | `[에이전트 응답] preview:`가 relay 본문의 진행 문장들과 NO_REPLY를 다시 게시 |

첫 진단 메시지의 본문 안 시각은 06:30:23인데 Discord 생성 시각은 06:30:17이다. 메시지 편집으로 누적 로그를 갱신하는 현재 형태이므로 “본문 시각=message 생성 시각”으로 정렬하면 틀린다. 종료 로그도 새 메시지 묶음이며 이전 진단 내용을 재포함한다. 프롬프트는 시작·종료 진단에 한 번씩 총 두 번 게시된다.

run API의 `discord_log_sections`는 `started` 154자, `js_inputs` 141자, `js_action` 1,401자, `outcome` 565자다. 원문을 설계서에 복사할 필요 없이 이 API에서 각 진단 구간을 볼 수 있다. 새 thread 계약에서는 시작·종료 진단과 preview 재게시 3개는 없어져야 한다. r1 당시에는 relay 말미만 확인하고 native final을 읽지 않았다. r2 하단의 exact turn transcript API 대조로 terminal=NO_REPLY를 확인했으므로 이 run의 relay까지 포함한 4개 모두 억제하는 fixture 전제가 충족됐다. 집계 preview를 native 최종 결과로 추정한 증거는 쓰지 않는다.

## 실패 원인 진단 실물

`memento-hygiene` run `51bbf441-1d06-4482-a2fa-c1d92bcf8d9b`: 11-06 03:00:24 시작→03:30:53 failed, `error="routine agent turn timed out after 1800 seconds"`, result.status=timeout, execution_mode=process, retry_count=0. API row는 `discord_log_status=ok`; thread message `1567961801386364980`의 진단 코드블록(1,531자)에 timeout 원문이 실제 포함된다. 이 message는 시작 때 생성되어 종료에 편집된 것이므로 Discord 생성 시각은 03:00:24.772다.

최근 10개씩 총 61 runs에서 56 succeeded·5 failed(전부 timeout). paused 상태인 routine이 있다는 것과 paused 결과 run을 관측했다는 것은 다르다: 이번 샘플에 paused run은 0건. 부모 채널의 failure/일시정지 전용 알림까지 도착했음은 이번 관측으로 입증하지 않았다. 11-06 03:32 이전 parent 채널 30개 read-only 조회도 해당 routine 알림을 찾지 못했으므로 알림을 “실물 확인 완료”라고 쓰면 안 된다. 기존 failure-alert 코드는 별도 코드 근거로 다뤄야 한다.


r1 당시 운영 설정 파일은 읽지 않았으므로 `headless_fresh_agents` 실제 값은 미확인이다. API에서 확인한 process 실물과 기본 설정값을 혼동하지 않는다. 위의 기존 게시물은 실제 관측이고 아래 신규 방식 예시는 구현 후 만족해야 할 설계 예시다.

## 방식별 흐름과 모듈 경계

### 공통 진단과 claim snapshot

현재 `discord_log_sections` 저장과 Discord 전송이 같은 함수에 묶여 있다(`discord_log.rs:90-231,407-457`). 작은 `routines/run_diagnostics.rs`로 구조 진단 저장을 분리하고 started/js_inputs/js_action/outcome 호출은 항상 저장만 한다. logger가 없거나 agent_id가 없어도 저장한다. `RoutineDiscordLogger`에는 **public result와 actionable notice만** 남긴다. JS 평가 실패의 원래 안전 진단도 run의 진단 필드로 남기되 공개 알림은 현재의 안전 오류 요약을 쓴다. 메모리/비밀 전체 덤프나 임의 환경변수 수집은 추가하지 않는다.

run 최초 claim에서 mode, runner, session backend, target thread/channel, legacy policy 등 실행 정의를 `run_spec`으로 고정한다. mutable routine row를 poll/recovery 때 다시 JOIN하여 새 모드의 teardown을 적용하지 않는다. 진행 중 PATCH는 다음 run부터 적용하고 in-flight의 target·backend·mode·timeout 예산은 불변이다. 완료 deadline은 기존 `current_attempt_started_at`에서 계산하며 claim 시각으로 고정하지 않는다. 시작 전 deferral600초와 owner input delivery deadline은 별도 시계다. 예전 run의 `result_json.execution_mode=process|tmux`는 백엔드 필드이므로 새 product mode로 재해석하지 않는다. 신규 `run_spec.execution_mode`와 `execution_backend`를 명시하며 구형 키는 읽기 alias로만 남긴다.

실행/게시 권위는 #5071 그대로다. run row는 스케줄·진단·관측 projection이고 입력 수용 권위가 아니다. 원문/ticket admission과 FrontClaim/physical writer는 기존 owner, 실행 여부는 mailbox active-turn lease, 본문은 기존 relay owner와 shared delivery lease/O frontier다. run succeeded나 HTTP ACK를 native 수용/Discord 배달의 증거로 승격하지 않는다. 새 큐·전송기·본문 dedup·영구 receipt ledger를 만들지 않는다. 신규 DB 필드는 선택/진단 snapshot이지 두 번째 입력 권위가 아니다.

### headless

QuickJS agent action → 기존 provider 준비 확인 → thread/DM resolver를 건너뜀 → primary channel을 기존 runtime/권한/workspace 문맥으로 사용 → 기존 reservation/mailbox에서 process 실행 → 새로운 synthetic session key와 fresh provider 경계 → 기존 완료 증거 → run 종결. `metadata.silent=true`와 persisted presentation policy를 시작 전에 넣는다. 이미 있는 `replay_start.rs:87-96`, `health/recovery.rs:400-402`, process silent bridge 시험 선례를 재사용하고 복구에서도 출력 0을 유지한다.

기존 primary mailbox가 바쁘면 기존 start-deferral을 사용하며 사용자 TUI를 reset/stop하거나 대체하지 않는다. 새 synthetic Discord 채널 번호·가짜 스레드·별도 mailbox registry를 만들지 않는다. **synthetic session key만 전달해서는 primary를 보호하지 못한다.** 현재 `headless_turn.rs:242-334`는 primary core session/uploads/cleared를 읽고 변경하며 `:337-360,488-518`은 channel clear boundary와 provider 상태를 지운다. 새 headless를 여기에 그대로 통과시키지 않는다.

작은 `headless_turn/isolated_context.rs`가 기존 scheduled snapshot의 turn-local cold-start 의미(`headless_turn.rs:549-564`)를 재사용하는 `IsolatedRoutineProcess` 문맥을 만든다. 실제 agent binding에서 workspace/role만 해석하고 session_id=None·uploads/history/retry-context 없음·memento_context_loaded=false로 시작한다. 이 branch는 primary `core.sessions` 생성/수정·uploads 소비·cleared reset·routine identity channel boundary·provider shared clear·live restore를 모두 건너뛴다. 새 per-run session key만 status/session writeback 대상으로 삼고 channel recent-context 주입도 억제한다(`:687-698`). 일반 thread fresh의 기존 channel 경계는 유지한다.

완료 writeback도 격리한다. 기존 `scheduled_turn::ChannelEffects` 판정(`scheduled_turn.rs:100-143`)에 persisted routine-isolation 관측을 연결해 completion postlude의 같은 suppress 결정을 재사용한다(`turn_bridge/completion_postlude/channel_writeback.rs:84-105,198`, `completion_postlude.rs:322,406,609`). routine을 ScheduledTurnMark로 사칭하지 않고 inflight의 routine run-spec/presentation에 isolation을 pin한다. 복구·provider 재시도·memory/feedback postlude도 이 관측으로 같은 결정을 내린다. process cancel은 기존 exact episode/token을 쓴다. actor retirement는 아래 소유 surface 가드를 추가한다. D0의 RED는 primary session id/history/uploads/clear boundary/runtime binding이 시작·완료·재시작 뒤 보존되고 별도 per-run key에만 새 provider 세션이 기록되는 것이다.

백엔드 미지원은 명시 실패이며 tmux/채널로 조용히 fallback하지 않는다. 새 headless는 Claude/Codex process 지원 범위부터 제공한다. 기존 thread는 현 지원 provider를 유지한다. 외부 업무 전송은 routine의 명시 도구 호출 범위이며 이 게시 억제가 업무 iMessage 발송까지 막지 않는다. 실패1..N은 primary에 알림만 보내고 정확히N+1에서 기존 명령봇을 통해 별도 대응 턴을 요청한다. paused·stale-paused·classification 알림 자체는 턴을 만들지 않는다. 아래 사건표와 전송 계약을 따른다. 어느 경우에도 routine thread 생성 경로는 호출하지 않는다.


**mailbox 공유는 세션 격리와 별개 계약이다.** `router/turn_start.rs:230-266`과 `headless_turn/process_mailbox.rs:35-62`는 channel 단위 mailbox를 쓴다. 헤드리스가 돌면 본 채널 사람 입력은 기존 Busy/queue 정책으로 그 작업 뒤에서 대기한다(`health/turn_deliver.rs:199-284`). 반대로 사람이 먼저 600초 넘게 일하면 루틴은 provider spawn0인 채 deferral을 소진하고 `primary_busy`로 끝난다. UI mode 설명에 이 두 방향을 적는다. run 전용 mailbox key는 후속 검토이며 가짜 Discord channel id로 우회하지 않는다.

`process_mailbox` capture/retire에는 검증된 `ProcessMailboxScope::{RoutineOwnedThread,RoutineOwnedDm,SharedPrimary}`를 전달한다. **retire는 routine-owned thread/DM일 때만**, SharedPrimary는 queue가 모두 비고 idle이어도 retire하지 않는다. scope는 실제 thread resolver/DM reservation 증거에서 만들고 외부 metadata bool을 신뢰하지 않는다. 그 뒤의 actor/claim/token/successor/queue/recovery/watcher 가드는 그대로 유지한다(`process_mailbox.rs:138-240`). D의 반대 방향 RED는 headless 진행 중 사람 입력 둘이 도착해 각각1회 처리되고 drain 뒤에도 primary actor identity가 같다는 것이다. scope 가드만 제거한 mutant가 이 단언에서 RED여야 한다. routine-owned thread/DM의 기존 retire 성공은 positive control로 남긴다.

### thread

기존 saved thread를 검증·재사용하고 없으면 한 번 생성/저장한 뒤 같은 스레드를 쓴다. 순차 실행은 기존 in-flight fence로 충분하다. 생성 성공 후 저장 실패는 결과를 실패로 기록하고 **이번 실행은 생성한 thread id를 run snapshot으로 유지**한다. 이후 run의 재조회·재사용에는 기존 thread 복구 증거만 사용하며 이름 검색으로 임의 thread를 채택하지 않는다. PR3b(D) 이후 새 claim은 legacy/명시 모드 모두 본 채널 fallback0이다. resolve/validate/create의 5xx가 provider 제출 전임이 확인되면 positive NotSent의 `thread_unavailable`로 기존 `start_deferral` 60초/최초600초 창에서 같은 run·같은 저장 thread를 재시도한다. 403/invalid parent 등 영구 오류는 명시 실패이고, 창을 소진하면 `thread_unavailable` 실패와 사건 알림1건이다. 생성 응답/저장이 불확실하면 nonce/id 증거 없이 새 스레드를 반복 생성하지 않는다. validate/create5xx→둘째 시도 같은 thread 성공, primary 게시0, 실제 Started 이후 완료예산 전체 보존을 RED로 둔다. 이미 시작된 구형 in-flight만 pin된 구형 정책으로 drain한다.

실행마다 fresh provider severance와 확인된 start를 요구한다. UI의 새 세션 진단은 게시하지 않는다. process 또는 interactive fresh backend는 내부 선택이며 지정 스레드/항상 fresh라는 제품 의미는 같다. script의 `dmUserId`가 새 explicit mode를 override하지 못하게 action boundary에서 부작용 전에 거부한다. 구형 null-mode DM 정의만 기존 adapter로 유지한다.

게시 정책은 **기존 7개 이행 C1과 명시 thread FinalOnly C2를 구별**한다. C1은 qualifying Claude process의 wire만 terminal까지 보류하며 raw full_response/transcript는 계속 쌓는다. exact terminal=NO_REPLY이면 앞 진행까지 본문0·배너0, 그 외는 기존 full body를 기존 owner에 한 번 넘긴다. 이미 게시한 old inflight는 serde-default Legacy로 drain하고 새 start에만 policy를 pin한다. 7개 소음 제거는 중간 진행을 모두 지우는 계약으로 넓히지 않는다. 명시 thread의 FinalOnly는 final record만 남기는 별도 opt-in 의미이고, 스트리밍/TUI/Codex 분류는 C2다. 두 정책 모두 run/script/count/checkpoint/action/prompt/preview 재게시·새 세션 진단은0이다.

`full_response`/`assistant_message_preview`는 final record가 아니다. C1은 같은 exact turn의 `SessionTranscriptEventKind::Result`/Claude terminal result를 사용한다(`turn_bridge/stream_loop/content_arms.rs:381-396`, `claude/stream_result.rs:5-21`). terminal이 없거나 여러 개여서 attribution이 불명확하면 raw preview의 꼬리 NO_REPLY로 지우지 않고 기존 full을 보존·classification 사건을 기록한다. C2는 Codex `final_answer`/TUI O의 structured native record와 기존 lease/frontier를 사용하며, commentary를 합치는 기존 `rollout_tail.rs:3199-3204`·phase 없는 `StreamMessage::Text`(`agent_protocol.rs:178-187`)를 그대로 final로 취급하지 않는다. source record를 원래 수집 경계에서 presentation projection으로 보존하고 새 reader/본문 emitter는 만들지 않는다.

정확한 final control token `NO_REPLY`(trim·기존 대소문자 호환)는 successful-no-visible-result로 정산하며 앞 진행문을 대신 게시하지 않는다. 일반 결과 안의 인용된 문자열은 지우지 않는다. raw transcript/full_response와 원래 delivery identity/range는 보존하고 화면 문자열 길이로 frontier를 다시 계산하지 않는다. C1은 legacy `[보고 메시지, 도구, "완료"]` fixture의 보고 메시지를 보존하고 명시 FinalOnly는 final="완료"만 남긴다. terminal=NO_REPLY이면 둘 다 본문0이다. 진단은 run API에 남으며 body0을 미배달로 잘못 복구 재게시하지 않는다.

기존 census의 native verdict(`discord_log/census.rs:4-85`)처럼 JS만으로 내는 사용자 결과는 명시 public-result adapter로 보존한다. 진단 logger를 통째로 삭제하여 이 결과까지 잃지 않는다. preview를 다시 보내는 logger transport는 제거한다.

### channel과 #8114의 입력 계약

`routines/agent_executor/channel.rs`가 실제 agent primary channel을 해석한 뒤 기존 owner custody/admission을 호출한다. `/api/agents/{id}/handoff`의 underlying start service와 `/turn/deliver`가 도달하는 owner drive를 재사용하며, 내부 HTTP 요청·announce trigger·가짜 HumanInputCaller를 만들지 않는다. cold/idle는 해당 채널의 기존 session을 만들거나 이어 쓰고 synthetic routine label/fresh reset은 사용하지 않는다. shared session은 routine이 소유하지 않는다.

현재 machine-origin Routine는 Busy에서 대기한다. #8114가 보존하는 그 정책을 몰래 바꾸지 않고 **#8117이 channel routine에만 typed Busy participation capability를 추가**한다. 최초 검증된 stored run mode에서 capability를 만들고 admission provenance에 pin한다. `is_person`은 그대로 두며 임의 source/body/metadata bool만으로 승인하지 않는다. 의존 API를 `busy_participation(origin, capability)` 한 훅으로 고정한다. 현재 코드에서는 `origin.rs`, `target.rs:78-80`, `provider_dispatch/owner_input.rs:193-213,855-869,1027-1053`의 managed-active Handoff·선행·late 판정과 target route가 함께 소비한다. `:193-213`의 handoff prepare/external active 관측도 같은 capability를 써야 native active에서 기존 `hand_busy(:232)`에 도달한다. fresh/thread/HTTP/scheduled의 own-turn 대기는 유지한다. target 조건 하나만 고치는 수리는 실제 선행 veto를 통과하지 못한다. 이 네 veto 접점을 E 통합 fixture가 각각 거친다.

#8114가 제공하는 operation 순서 그대로: actual operation 취득 → `InputIdentity`와 `CurrentSource`/observer 증거 → original/frame+history registration → owner ready → optional draft park → payload-start → host-gated paste+Enter → Enter-terminal Completion → 기존 settlement와 receipt diagnostic. tmux의 pinned if-shell/first-key stop 및 Herdr per-mutation pin/physical lease는 adapter 내부다. 새 공통 caller에 Herdr/tmux endpoint 분기나 raw send를 넣지 않는다. RemoteTui는 기존 placement로 해당 node owner에 보낸다. 같은 participation 결과는 기존 `CodexBusy`/`CodexHerdrBusy`에도 적용한다(`input_owner/target.rs:87-89`, `provider_dispatch/owner_input.rs:1504-1540`). Claude만 허용하는 predicate로 Codex를 차단하지 않는다. 미지원 host/capability는 즉시 실패 대신 기존 `synthetic_waits_idle` same-original Held로 둔다. idle로 회복되어 기존 idle path가 가능하면1회 제출하고, 계속 미지원이면 delivery deadline에서 exact-withdraw ACK 후 `channel_input_unavailable` failed로 닫는다. fresh fallback/새 leaf는 없다. #8114 착지 뒤 파일 이동된 `busy_target.rs`/`owned_busy.rs`와 공통 operation API를 먼저 읽고 이 훅 한 곳에 연결하며 옛 네 좌표의 host 분기를 다시 복제하지 않는다.

Busy의 `Submitted`는 transport ACK이고 run 성공이 아니다. 기존 `tui_prompt_dedupe/injected_steer.rs:198-222,243-249`의 동일 frame/attempt `Joined|Handed` 및 연결 native terminal/O 결과를 run의 읽기 projection으로 연결한다. 현재 routine observer의 자기 logical turn 기대(`input_owner/routine.rs:216-247`)를 shared native turn attribution에 맞게 작은 child에서 적응한다. 최근 채널 answer 복사나 별도 routine reader는 금지한다. 답이 사용자와 routine 여럿을 함께 처리할 수 있으며 같은 Discord 메시지 링크를 여러 run이 참조할 수 있다. 이 귀속은 API/대시보드의 내부 기록에만 표시한다. 기존 입력 경로에 프롬프트를 넘긴 뒤에는 사람 Discord 입력과 동일하게 돌며 루틴 전용 진행/출처/공유 라벨, O 편집, 답 복사·별도 스레드를 추가하지 않는다. 내부 `RoutineChannel` origin은 권한/소유 증거로만 쓰고 화면 라벨로 투영하지 않는다. 사람 입력으로 origin을 위장하지 않는다.

### 주입 실패·시간 경계·복구

| 시점 | 처리·자동 진행 | 종결·가시성 | 금지 |
|---|---|---|---|
| key=0이며 source/modal/writer 원인 거절 | 같은 original/ticket Held, #8114 reobserve/기존 held_watch가 실제 회복 때 wake | 대시보드 전달 대기+원인. 원래 run deadline 안에서만 재시도 | 새 ticket·timer paste·idle 기다리기만 하는 가짜 Busy 지원 |
| 확정 NotSent이며 provider 준비/예약 충돌 | 기존 60초/600초 start-deferral 및 남은 pre-start retry budget, target/mode snapshot 유지 | timeout이 되면 같은 취소/실패 경로 | backend 전환·deadline 재설정 |
| Held deadline 경합 | owner actor 안에서 exact namespace+ticket+original/context 확인 후 기존 cancel/first-key fence로 **해당 원문만** 철회 | 철회 commit 확인 뒤 failed. 이미 key 가능하면 다음 행으로 정산 | run row만 failed로 바꾸고 원문이 나중에 실행되게 남기기 |
| paste/Enter가 도달했을 가능성 | 기존 MayHaveSent/Unknown 정산만; Enter-terminal 뒤 재전송 없음 | deadline까지 증거가 없으면 failed(`completion_unconfirmed`)와 사건표 가시성(channel 추가 알림0·기존 owner 표시). 늦은 증거는 같은 run에 보충하며 성공/실행을 새로 만들지 않음 | Unknown→Held downgrade, 다른 agent로 재실행 |
| 수용/합류 후 provider 오류·timeout | 같은 native episode의 기존 정산, 새 turn을 시작하지 않음 | failed와 원인·증거. 채널 답은 기존 owner가 필요하면 계속 처리 | 사용자 대화/채널 session reset·kill·clear |
| 서버 재시작/lease 만료 | 새-mode owner run은 같은 ticket 복원 후 제출 전이면 exact-withdraw ACK를 먼저 받음. 그동안 running/recovering 유지. post-key는 기존 정산/no resend | 철회 확인 뒤 interrupted; post-key는 interrupted/unconfirmed와 보충 증거. 다음 정기 슬롯은 기존 in-flight 해제 뒤 별도 run | interrupted만 쓰고 known-unsent queued 원문을 남겨 late dispatch하게 함·현재 설정으로 target 재추론·매칭 안 된 세션 reap |

main의 owner poll은 observer bind 전 timeout도 보류한다(`owner_input.rs:37-55,286-347`), heartbeat는 계속 연장된다. 새 `agent_executor/settlement.rs`는 poll의 owner gate **앞**에서 run deadline을 확인하고 기존 owner 취소/정산을 요청한다. `queued_head/v2/control.rs:206-260`의 ambiguous-ID 거절·revoke_front·Cancelled owe+commit을 재사용하며 exact ticket/context 검사를 같은 actor 요청에 붙인다. 기존 `cancel_queued_primary_message`를 모호한 ID로만 호출하지 않는다. first-key 전에 철회가 linearize되면 key=0, writer가 이미 앞섰으면 Unknown/no-resend다. 새 cleanup 권위나 프로세스 전체 kill을 만들지 않는다. 이 최소 exact-withdraw 적응은 #8117 구현 조각에 포함한다. boot/periodic `recover_stale_runs`도 새-mode owner intent가 있는 run은 generic interrupted UPDATE 전에 같은 복구 coordinator로 넘긴다. known-unsent의 철회/종결 저장이 실패하면 running+recovering으로 보존하고 기존 recovery tick에서 같은 원문을 이어받는다. 전역 저장소 장애 중 강제 종결하지 않는다. process/legacy 구형 run은 기존 recovery 정책을 유지한다.

liveness의 환경 가정은 기존 tick/owner actor/저장소가 결국 다시 응답하고 deadline 판정 기회가 있다는 것이다. 저장소 영구 장애·서버 영구 정지 중 run 종결을 증명한다고 주장하지 않는다. 실패·불확실성을 run에서 보이게 하는 것은 성공 전달과 다르다. 기존 owner custody의 전역 replacement-safe 보장을 이 설계 모델 하나로 완료했다고 주장하지 않는다.


### 시계와 레거시 계약 행렬

완료 timeout은 `reliability.rs:6-24`의 **current_attempt_started_at**(마지막 started event, 없으면 기존 run.started_at)을 유지한다. provider/thread 예약 전 deferral은 `deferred_since`부터600초이며 실제 provider Started 후 기존 timeout 전체를 받는다. claim 시각으로 완료예산을 깎지 않는다. owner pending/Held는 기존 intent 시각(`owner_input.rs:53-69`)에 고정한 delivery deadline을 E0에서 별도로 관측한다. 같은 원문 NotSent rearm/heartbeat는 이 시계를 새로 시작하지 않으며 key가 먼저면 Unknown/no-resend다.

표는 **해당 PR 착지 뒤 새 claim**의 최종 계약이다. 실행 중 구형 run은 pin된 정책을 drain하고 현재 row 변경을 소급하지 않는다. B/C1은 PR1에서 기존 metadata로, Started/retry·thread deferral은 A 이후 D에서 적용된다. mode 필드가 null이어도 정책 적용 여부는 claim snapshot으로 구별한다.

| 레거시 종류 | 진단 억제 | 본문 projection | Started 뒤 새 턴 retry | thread 실패 처분 | deadline | DM override |
|---|---|---|---|---|---|---|
| fresh-thread(현재7 기본) | B: 진단/preview/banner0 | C1: terminal NO_REPLY만0, 그 외 full 유지 | D: 금지; positive NotSent만 재시도 | D: transient60/600 같은 thread, 소진/영구오류 failed; primary fallback0 | current_attempt_started_at 유지, deferral별도 | 동적 dmUserId가 나오면 legacy DM provenance로 고정 |
| persistent 복원 | B: 진단0 | C1 process 적용 전제 없음; 기존 본문 보존·확정 NO_REPLY만0, TUI는 C2 계약 확인 | D 이후 새 claim도 금지; 세션 continuity 유지 | D: 동일 deferral/primary fallback0 | 기존 current_attempt_started_at; shared 채널로 자동 변환0 | 기존 동작 유지 |
| 동적 DM | B: 진단0·DM 대상 노출0 | Claude process면 C1, 그 외 기존 full·확정 NO_REPLY만0 | D: 금지; 제출 전만 | thread resolve 없음; DM 예약의 기존 transient60/600, primary fallback0 | 실제 Started 기준, deferral별도 | legacy action override 유지; 명시 네 mode로 사용자 편집한 뒤는 거부 |

새 explicit thread/headless는 항상 fresh, channel은 자기 채널 continuity다. null-mode 구형 definition을 자동 explicit thread로 승격해 동적 DM을 깨뜨리지 않는다. 정책 강화는 mode 재분류와 구별하며 checkpoint/next_due/status/thread id는 보존한다.

## 확정 UX (11-12 16:13 KST)

### 채널: 결정됨 — Discord 입력과 동일

프롬프트 주입 뒤 답의 소유·표시·순서는 기존 입력 경로 그대로다. granted physical writer는 비선점이고 그 다음은 기존 owner priority와 원문 순서를 따른다(`queued_head/v2.rs:122`). 입력의 내부 origin/capability는 유지하지만 루틴 전용 진행·‘함께 처리’·O 라벨 편집·별도 스레드를 만들지 않는다. 사람 본문에는 routine-only NO_REPLY/FinalOnly 필터를 적용하지 않는다. 어떤 run이 같은 답을 참조하는지는 run API/대시보드에서만 확인한다. r2의 A/A′/B 표시 비교는 종료했고 모두 구현 범위에서 제외했다. E는 정확한 입력·결과 귀속만 구현한다.

### 단순 스크립트: 직접 등록 A 확정

같은 루틴 생성 폼/API에서 executable path·argv 배열·cwd를 직접 등록한다. 예: `{"execution_mode":"script","runner":{"kind":"command","program":"/bin/bash","args":["scripts/report.sh"],"cwd":"/workspace/reports"}}`. 셸 문법은 사용자가 `/bin/bash` 등의 인자로 명시했을 때만 해석된다. agent_id는 선택적 귀속이며 provider 사용0이다. 기존 JS도 `runner.kind=quickjs`로 등록한다. 실행 노드 파일 존재·셸 내부 side effect의 exactly-once 한계는 F의 등록 검증·출력 이력에서 드러낸다. 비교했던 JS wrapper/spawn 권한 확대 B는 채택하지 않는다. 새 원격 실행기·관리자 allowlist·수동 launchd 정리를 필수로 붙이지 않는다.

## 단순 스크립트 범위와 실행 결과

현재 `action.rs:6-32` 및 `runtime.rs:675-785`의 complete/skip/pause는 이미 provider 없이 결과 JSON/checkpoint/next_due를 저장한다. `loader.rs:706-748,790-807`는 격리 QuickJS이며 Node shell API가 아니다. `migrated.rs:13-24,27-56,68-99`는 portable entrypoint/required path/connector 전제를 검증할 뿐 실행기가 아니다. `docs/launchd-to-routine-migration-plan.md:10-34`의 tick은 agent prompt를 반환해 agent가 셸을 호출한다. 따라서 임의 셸 직접 등록·stdout/stderr·exit code는 이번에 추가할 기능이다. 이전 문서의 과거 parallel/cutover 운영 절차를 이번 기본값의 전제로 삼지 않는다.

`script+quickjs`에서 agent action을 반환하면 명시 `agent_action_forbidden` 실패이며 agent launch 0이다. command는 기존 scheduler 실행 노드에서 argv로 직접 spawn, stdout/stderr를 동시에 drain하여 각각 64KiB까지 capture하고 초과 byte 수/잘림 표식을 남긴다(초과분도 drain). 정확한 child 종료/timeout 뒤 pipe drain은 최대 1초의 별도 유예만 둔다. 하위 child가 FD를 보유해 EOF가 오지 않으면 reader task를 중단하고 pipe를 닫아 `capture_incomplete=true`로 기록한다. byte cap과 열린 pipe 종결 한도를 구별한다. UTF-8 불량은 대체 문자와 원래 byte count를 보인다. 결과는 `exit_code|signal`, `duration_ms`, `timed_out`, stdout/stderr, started/finished, runner snapshot이다. exit0은 stderr가 있어도 succeeded; nonzero/signal/spawn 오류/timeout은 failed다. command는 agent retry/fallback을 사용하지 않으며 기본 재실행 0이다.

spawn은 기존 `services/process.rs:82-95 configure_child_process_group`의 Unix `process_group(0)`을 사용하여 **이 run이 직접 만든 PGID**로 격리한다. `process/stream_child.rs:24-40,51-87`의 owned-group 확인·leader WNOWAIT→TERM→기존200ms 유예→identity 확인 KILL→reap 순서를 재사용한다. stdout/stderr는 위 bounded capture에 연결하고 leader를 먼저 reap한 뒤 오래된 PGID 정수로 신호를 보내지 않는다. timeout/명시 취소/정상 exit에 남은 같은 그룹 child까지 정리하되 foreign group·0·소유 증거 없는 그룹에는 신호0이다. process helper를 복제하거나 패턴 kill·사용자 tmux kill은 추가하지 않는다. 그룹에서 setsid로 이탈한 daemon 추적·daemon supervisor·LaunchAgent 설치/삭제·전역 env 수정은 범위 밖이다.

정상 dcserver 종료는 runner가 보유한 live handle/PGID만 idempotent하게 회수한다. 종료 admission을 닫고 TERM/KILL/reap·결과 저장 뒤 process exit한다. 기존 `runtime_bootstrap/shutdown.rs:20-53`의 SIGTERM hook, `runtime_bootstrap/spawns.rs:365-394`의 managed deferred restart, `discord/mod.rs:696-729`의 legacy deferred exit를 같은 작은 runner shutdown adapter에 연결한다. generic 영구 PID registry는 만들지 않으며 새 server가 DB의 옛 PGID 숫자만 보고 kill하지 않는다. hook이 없는 SIGKILL/OS crash에서는 child group이 생존할 수 있고 다음 boot는 interrupted/unknown으로 종결한다. 같은 run 자동 spawn0이며 다음 cron과 외부 side effect가 겹칠 수 있다는 trade-off를 결과에 드러낸다. 정상 종료 hook의 회수 성공과 강제 crash 생존을 혼동하지 않는다.

F의 실제 격리 RED는 `sleep 600 & sleep 600`을 timeout2초로 실행해 두 PID와 group이 Gone·capture_incomplete=false인지, foreign group은 살아 있는지 검사한다. 정상 종료 세 hook의 중복 호출은 그룹당 정리1회이고 leader-exit/child-alive·PID identity 불일치·spawn 실패는 별도로 검증한다. AgentDesk provider 턴0은 보장하지만 외부 command가 자체 AI를 쓰는지/셸 내부 side effect의 exactly-once까지 보장하지 않는다.

## 후속 사전 판정 — #8127 gate 착지 뒤 (현재 #8117 스택 밖)

사용자 승인(11-12 15:58/16:59 KST)은 자료 수집 뒤 닫힌 판단만 남으면 agent 세션을 절약하되 키 없음을 오류로 취급하지 않는다는 것이다. PJ는 현재7의 즉시 적용 대상0/7이고 기본off이므로 #8117 구현/이전의 의존이 아니다. 별도 PR2c를 현재 스택에 만들지 않는다. #8127이 공통 gate와 서버 키 경로를 착지한 뒤 정확한 ref/API/키 가용성·캐시·기동 로그 계약을 pin한 후속 PR에서만 소비한다. routine 전용 REST client·키 조회·model 선택·공통 gate 파일을 새로 만들지 않는다.

후속 gate 결과는 `Off(no_key|disabled) / NoAnswer(class) / Answer{answer,confidence,p_yes}` 세 상태다. Off는 CLI spawn·네트워크 호출0, 기존 agent 흐름 그대로이며 실패/fallback class나 run별 경고가 아니다. 키 없음은 기동 때 ‘판정 API 꺼짐(키 없음)’1줄만 남기고 run outcome에는 `prejudge=off(no_key)`를 투영한다. 키 가용성 시점/캐시/timeout은 #8127 정본을 쓰며 호출마다 CLI로 금고/agent-op을 조회하지 않는다. NoAnswer(rc2/3·timeout 등)는 답을 쓰지 않고 원래 규칙으로 진행한다. Answer의 정상 no+confidence 충족만 기존 skip으로 종결해 reason=prejudge_no_work·provider 세션0, yes는 agent1이다. 공통 정본 외에 routine run마다 fallback class를 쌓지 않는다. 이 레그의 실제 판정 API·키 조회0이다.

적용은 후속의 validated QuickJS Agent arm(`runtime.rs:787-829`) 하나로 due/manual이 공유한다. headless/thread/channel에만 opt-in하고 script mode·complete/skip/pause에는 호출하지 않는다. 질문 틀은 ‘이번 실행에 에이전트가 할 새 작업이나 보고할 유의미한 변화가 있습니까? 자료가 불완전하거나 수집이 실패했으면 예로 답하세요.’다. 숫자·결정적 규칙·감사/조사 자체가 일인 업무·이미 agent가 내린 판단·삭제/배포/권한 결정에는 쓰지 않는다.

입력은 script의 안전한 수집 사실·변화·완전성 요약 prejudge_input만 쓰고 prompt 전체/환경/키/가족 프로필/기억 원문을 보내지 않는다. P2-G 후속 계약은 UTF-8 자료8KiB 이하이며 초과·없음·불완전·수집 실패면 호출0·agent1이다(임의 truncate로 no를 만들지 않음). 정의/API/UI opt-in·입력 hash/byte·typed outcome/skip 사유는 후속에만 추가하며 A/G에는 넣지 않는다. checkpoint/next_due·실제 agent 시작 이후 timeout을 보존하고 deferral/recovery에서 같은 run을 다시 판정하지 않는다. 비용/지연은 #8127 관측과8KiB 경계로 확인하며 자체 전체 호출 상한을 약속하지 않는다.

후속 RED(P1-1·P2-G): 키 없는 fixture에서 enabled 루틴10회 각각 CLI spawn0·네트워크0·agent1(총10)·추가 지연0·warn0, 기동 info1줄, outcome=off(no_key). disabled/script/complete/skip/pause 호출0. 정상 Answer yes→agent1, no/.95→skip0, NoAnswer/임계미달/invalid→agent1, 누적요청1·skip 사유·checkpoint/next_due 보존.8192bytes/8193bytes 경계에서 초과 spawn0·agent1이다. 실제 API/키 없이 fake #8127 gate로 시험하고 CI에서도 키 없는 빌드·시험이 통과해야 한다.

원인 제거 뮤턴트① Off/no_key의 호출 생략 가드를 제거하거나 NoAnswer 오류 경로로 보내면 실제 요청수/추가 지연 또는 outcome=off assertion에서 RED다(단순 enum 변경이 spawn을 만든다고 가정하지 않음). ② 매 run warn!을 남기면 로그 개수로 RED다. 초과입력 호출·rc2 skip·attempt 재호출·기본on·키/raw자료 argv 노출도 RED다. 모델 해석은 키 없음 ≡ PrejudgeEnabled=FALSE이며 기존 false cfg 증거만 재사용한다. 후속 코드가 생기면 그 head에서 RED/GREEN을 실행한다.

## 저장 형식·API·대시보드

### 정의와 구형 복원

새 정의의 정본은 `routines.execution_mode` enum과 discriminated `runner` JSON이다. prejudge 정의/API/UI는 #8127 뒤 후속에만 추가한다. 루틴별 `headless_failure_notice_limit`은 정수1..100, 기본3이다(0으로 사용자 정책을 끄지 않음). 기존 전역 `max_consecutive_failures`는 thread/legacy 알림의 이전 의미로 유지하고, 새 N 필드와 같은 의미라고 해석하지 않는다. 구형 row/누락 client에는3을 투영한다. 값 변경은 다음 streak부터 적용하며 첫 failed run의 run_spec에 N을 pin한다. 성공 등으로 reset되기 전에는 그 N을 쓴다.

```json
{"execution_mode":"thread","runner":{"kind":"quickjs","script_ref":"family.js"},"agent_id":"family","discord_thread_id":"existing-thread","timeout_secs":1800}
```

mode=script만 command runner를 허용하고 다른 셋은 quickjs agent 판단을 사용한다. routine의 일정/checkpoint/last_result/in_flight 필드는 유지한다. `script_ref`는 quickjs에만 쓰는 compatibility alias이며 command는 null로 바꾸는 additive migration을 한다. mode와 runner 조합 검증은 API 진입 한 곳에 두고 DB CHECK는 저장 무결성을 지킨다. command의 program/cwd 접근 가능성은 등록 시 검증하고 실행 때 spawn 실패는 run에 기록한다. script가 runner id/경로를 매 tick 임의 변경하게 하지 않는다.

`execution_strategy`는 신규 정의에서 쓰지 않는다. thread/headless는 항상 fresh, channel은 기존 채널 continuity를 쓰므로 persistent bool을 두지 않는다. channel에 `persistent`를 투영하더라도 예전 루틴 전용 session/reset·kill 권한을 뜻하지 않는다. 새 session control API는 네 모드 전부 routine-owned persistent 제어가 없으므로 명시 conflict이며 기존 legacy persistent만 기존 제어를 유지한다. UI 새 선택지는 정확히 넷이다.

무중단 expand/contract 중 `execution_mode=null`인 구형 정의/이미 진행 중 run은 기존 dispatcher로 읽는다. 운영 7개 fresh의 effective mode는 thread+기존 backend로 자동 투영하되 **정의 행의 null-mode provenance를 자동 삭제하지 않는다.** `dmUserId`는 정의에 없고 JS가 실행 때 동적으로 반환하므로 claim 시 ‘DM 정의 아님’을 판별할 수 없다(`action.rs:26-32`, `launch.rs:51-72`). null-mode run은 최초 claim에 legacy 계약을 pin하고 JS action 평가 뒤 그 run의 실제 thread/DM surface를 고정한다. 현재 7개 API 실물은 thread이며 표의 기본 매핑을 따른다. 다음 실행에서 DM을 반환하는 구형 script도 기존 동작을 유지한다. 새 explicit-mode 검증을 자동 승격한 구형 row에 적용하지 않는다. 과거 persistent/DM 정의가 복원되면 자동 channel/thread 치환하지 않고 **기존 동작의 read-only ‘구형 정의 · 현재 동작 유지’ 호환 상태**로 실행한다. 이는 신규 다섯째 선택지가 아니고 기존 코드를 제거하지 않는 backward reader다. 사용자가 네 방식 중 하나로 편집할 때부터 명시 새 계약을 적용하며 운영자 일괄 수리/재개는 필요 없다. 기존 persistent를 channel로 몰래 바꾸면 전용 스레드 세션을 사용자 본 채널로 옮기는 변경이므로 금지한다. 구형 코드 최종 제거는 복원 정책을 포함한 후속 명시 범위이며 이 설계의 7개 이전을 막지 않는다.

`headless_fresh_agents`는 mode 미지정 구형 정의의 backend 해석 및 보존 snapshot에만 사용하고 explicit mode에는 무시한다. 지금 executor boot-bound인 점은 유지하며 hot reload로 바꾼다고 가정하지 않는다. 새 정의는 provider capability에 따라 thread backend(process 지원 Claude/Codex면 process, 그렇지 않으면 기존 interactive)를 정해 저장한다. 구형 정의의 각 run은 당시 로드된 flag가 선택하던 backend를 snapshot하므로 구형 flag=false의 tmux도 갑자기 process로 바뀌지 않는다. 사용자가 명시 모드로 전환할 때만 새 선택을 저장한다. headless는 process, channel은 실제 채널 target이다. backend를 사용자 모드 선택지로 추가하지 않는다.

### API 계약

| 현재 API | 추가/변경 |
|---|---|
| POST `/api/routines` | mode·runner·headless_failure_notice_limit(default3), schedule/timeout/name 선택 폼과 동일. 기존 body mode 누락은 구형 quickjs fresh 호환 생성, response에 effective mode/호환 상태 표시 |
| PATCH `/api/routines/{id}` | mode/runner 변경은 다음 claim부터. N omitted는 유지, null/범위 밖은422이며 N 변경은 다음 streak부터. omitted 유지·명시 null 의미 구분. mode를 초기화하는 null은 새 정의에서 422. mode/strategy 동시 상충은 422 |
| GET 목록·단일 | explicit/effective mode, legacy compatibility, runner, N과 유효 streak N, 현재 thread/channel 링크, existing last_run. command에 가짜 script_ref를 만들지 않음 |
| GET `/api/routines/{id}/runs` | 기존 result/attempts/log sections + immutable run_spec·diagnostics·실행/수용/배달 상태·실패 streak/notice/escalation 상태와 기존 outbox id. 구형 rows는 없음을 unknown/legacy로 표시 |
| GET `/api/routines/runs/search`, metrics | 기존 검색에 mode/오류 class 활용. 0 output과 unconfirmed 구분, script exit 요약 포함 |
| 신규 GET `/api/routines/{id}/runs/{run_id}` (G) | PR1은 기존 runs API를 재사용하며 G에서 같은 store 권한으로 해당 run 상세만 반환. 현재 API에는 별도 detail 없음. 대시보드·실패 알림 안정 링크를 위해 추가 |
| run-now·pause/resume/detach | 기존 공통 claim/close 사용. 새 모드가 별도 실행 진입점을 만들지 않음 |
| routine session/reset·kill | explicit mode는 409. channel 공유 세션 제어로 우회 연결하지 않음 |

구형 client가 새 explicit mode에 strategy만 PATCH하면 동일한 의미의 호환 값만 허용하고 반대값은 422, 누락하면 mode 보존이다. legacy null-mode에는 기존 API를 유지한다. deprecated field는 response에서 호환 projection임을 docs에 밝히고 legacy API 문서에 새 mode 선택을 권한다. 삭제/rename로 구형 run의 process 필드를 새 enum 파싱에 넣지 않는다.

### 화면

현재 `RoutinesPageView.tsx:27-29`는 모든 row에 Discord thread가 있다고 설명하고 `RoutinesTimelineWidget.tsx:925-1027`는 결과 drawer를 갖는다. 새 화면은 mode badge/생성·편집 폼, thread/channel만 Discord 링크, headless/script는 이력 링크를 보여 준다. 새 책임은 `RoutineExecutionModeEditor.tsx`, `RoutineRunDiagnostics.tsx`, `RoutineScriptOutput.tsx`로 분리한다. giant widget은 badge/link/open glue만 변경한다.

run drawer는 ‘결과’와 ‘진단’ 탭으로 구분한다. 결과에는 최종 결과·공유 답 링크·exit 및 실패 요약, 진단에는 run id·script/runner·관측 개수·checkpoint/action·prompt 원문·attempt·Held 원인·receipt/O 정산·backend·Discord 배달 상태을 둔다. prompt는 접힌 상세이며 기존 UI가 갖는 데이터 접근 범위를 그대로 따른다. headless/script 실패·paused는 루틴 목록의 붉은 상태/알림 카드와 run deep link로 볼 수 있다. headless mode 설명에는 primary mailbox 공유·N 설정(기본3)·1..N 알림/N+1 별도 대응 턴을 명시한다. thread 알림은 기존 message_outbox로 링크+루틴명+안전 원인 한 줄을 게시한다. channel은 주입 뒤 기존 입력/owner 표시 그대로이며 주입 전 실패 임계/paused/stale만 non-turn primary 알림(thread 생성0)을 사용한다. headless 상세에는 streak N과 notice/command 전송·불확실 상태를 각각 표시한다. P2-H: ACK만 있으면 ‘게시됨 · 대응 턴 미확인’, 기존 owner 수용/합류 증거가 있어야 ‘턴 연결됨’이다. status=failed와 notice delivery 실패를 각각 남겨 알림이 실패했다고 본문을 재실행하지 않는다. 새 모드에서 진단 기록의 실패를 ‘Discord가 조용해서 정상’으로 감추지 않는다.

## 진단 제거 뒤 어디서 보는가: 실물 기반 수용 예

다음은 위 실물 run을 입력 fixture로 삼는 **미구현 수용 예시**다. 실제 신규 모드 운영 성공을 주장하지 않는다.

| 방식·상황 | 사용자가 보는 예 | 보이지 않아야 하는 예 | 원인 확인 |
|---|---|---|---|
| headless 아침 브리핑 성공 | Discord 메시지 0. 대시보드 ‘성공 · 외부 전달 작업 완료’, 응답 진단/완료 증거 | e4f43597 run의 위 4개 메시지와 thread 생성 0 | run 상세에 prompt/attempt/process 및 실제 외부 전달 주장은 도구/결과 증거와 구분 |
| thread 동일 e4f43597, final=NO_REPLY | Discord 메시지 0, 이력 ‘성공 · 표시할 결과 없음’ | 진단 1,912/1,900자, 364자 진행+배너, 412자 preview 모두 0 | run 상세에서 기존 started/js_inputs/js_action/outcome 정보 유지 |
| thread 결과 있는 실행 | 기존 지정 thread에 결과 한 번. legacy C1은 non-NO_REPLY full 보존, explicit C2는 최종 결과 | prompt/response preview 재게시/새 세션 시작0(중간 진행 전체 억제는 C2) | result와 같은 turn의 배달 링크·진단 탭 |
| channel 사용자 조사 중 작업 감시 | 원래 사람이 Discord에 입력한 것과 동일한 기존 채널 답/표시 | 루틴 전용 진행/출처/공유 라벨·O 편집·별도 thread·원문 재게시0 | 해당 run의 same ticket/frame Joined→native terminal/O 링크. 사용자 입력과의 공유 귀속은 run API/대시보드에만 표시 |
| channel source 불명/Held | 대시보드 ‘전달 대기 · 현재 입력 대상 확인 중’→회복 시 1회, deadline 시 ‘실패 · 제출 확인 불가’ | key0 거절 중 원문 유실/새 input 복사/채널 session kill 0 | exact target/current-source/NotSent 또는 Unknown·철회 증거. Discord는 주입 뒤 기존 owner 표시만; known-unsubmitted 종결의 임계/paused/stale는 사건표 P 예외 |
| script `/bin/bash report.sh`, exit2 | 대시보드 ‘실패 · 종료 코드 2’, stdout `검사 3건`, stderr `입력 파일 없음` | Discord 성공/진단 0, provider turn0 | runner/argv/cwd·출력·exit2·시간·잘림·재시작 여부 |
| 실패 실제 51bbf441 timeout | thread: ‘Memento 정리가 30분 안에 끝나지 않았어요. [실행 보기]’; channel: 기존 입력 표시; headless N3: 실패1/2/3 알림만, 4회차 명령봇 대응 요청; script: 카드만 | 1,531자 진단 코드블록·script/prompt 노출 0 | API error=timed out after 1800 seconds, backend, exact attempt·last evidence |

## 사건×방식 알림 (사용자 결정·P2-E 기본값)

모든 사건은 run API·대시보드에 남는다. `P=primary 기존 non-turn outbox 알림만(턴0; 기존 Announce+OperationalAlert, 정책상 Notify fallback 가능)`, `A=primary 명령봇 Announce→기존 intake 대응 턴`, `T=기존 지정 thread outbox`, `—=대시보드만`이다. channel은 주입 뒤 기존 입력/owner의 답·오류 표시 그대로다. 주입 전 실패 임계·paused·stale만 P이고 진행/출처/공유 라벨은 추가하지 않는다. 진단 코드블록·prompt·stdout/stderr를 알림에 넣지 않는다. headless의 모든 행은 thread 생성0이다.

| 사건 | headless | thread | channel 추가 게시 | script |
|---|---|---|---|---|
| terminal failed run, streak1..N (primary_busy/소진 포함) | P 각 failed run1, 턴0 | T 안전 요약1 | — | — |
| 주입 전 terminal 실패의 기존 임계 도달(JS 오류·channel_input_unavailable·deferral 소진) | headless는 k정책 | T 기존 임계1 | P 임계1, thread 생성0 | — |
| 정확히N+1회 연속 실패 | A streak당1, 원 run retry0 | 기존 threshold 정책대로 T; 새 escalation0 | — | — |
| N+1 초과 연속 실패 | P 알림만(기존 TTL 중복억제), 추가 대응 턴0 | 기존 threshold 정책 유지 | — | — |
| paused(JS/자동/migration_invalid) | P 전이1, 턴0 | T 전이1 | P 전이1, thread 생성0 | — |
| stale-paused | P 기존 TTL1, 턴0 | T 기존 TTL1 | P 기존 TTL1, thread 생성0 | — |
| interrupted(boot/lease/재시작) | —, streak 중립 | — | — | — |
| completion_unconfirmed | terminal failed면 같은 실패 행에 합산, 별도 trigger0 | T 실패 요약1 | — | — |
| result_classification_unavailable | P routine당24h1, 이 사건 자체는 턴0 | T routine당24h1 | — | — |
| auto-resume | — | — | — | — |

channel 주입 전 실패 임계는 기존 global threshold(기본3/0disable)다. leading terminal 실패 중 입력 수용0이 확정된 run만 연속으로 세며 post-injection/unknown은 추가 P 근거가 아니다. JS 오류3회면 P1·thread 생성0, 주입 뒤 결과는 routine 알림으로 재게시하지 않는다.

thread의 기존 연속실패 임계는 전역 threshold 기본3/0disable이며 정확히임계 도달1이라는 이전 의미를 유지한다. 새 headless N과 다르다. classification은 C1에서 full 보존+degraded, explicit FinalOnly에서 확정불가 failed다. 반복 classification notice는 기존 outbox routine당24h TTL1로 합치되 **실제 failed 카운트와 N+1 escalation을 억제하지 않는다.** 같은 failed run의 일반 notice/paused/classification은 안전 요약1건으로 합칠 수 있지만 N+1 command는 별도 목적 키로 반드시 기록한다. 새 dedup store를 만들지 않는다.

### 헤드리스 실패 알림과 N+1 대응

P2-C/D: headless의1..N·paused·stale·임계와 channel의 허용된 주입 전 알림은 primary만 해석하며 routine target/thread 생성 resolver(`discord_log.rs:460-542`)를 호출하지 않는다. 기존 exactly-threshold headless 알림은 억제/같은 사건으로 병합한다. P용 reason은 is_non_turn_operational_alert(`message_outbox/identity.rs:36-45`)에 등록해 실제 bot=Announce여도 OperationalAlert로 intake0이다. routine_failure_incident는 actionable/non-turn 목록 둘 다 제외해 표지·Notify fallback0이다.1..N P 키는 current run id, N+2 이후 P 키는 `routine:{id}:after_incident:{first_failed_run_id}`+24h TTL, incident는 first failed id 영구 키다.

카운터 권위는 기존 run 이력이다(`discord_log/failure_alert.rs:19-40`): terminal failed만 세며 attempt 실패/NotSent deferral은 세지 않는다. running/interrupted는 건너뛰고 succeeded/skipped/paused는 기존 의미대로0으로 reset한다. 재시도 뒤 succeeded인 run은0이다. 별도 연속실패 DB 카운터를 만들지 않고 failed streak의 첫 run id와 pin된 N을 projection한다. 현행 LIMIT=N+1로는 긴 streak의 첫 ID를 못 찾으므로 마지막 nonfailed 경계와 첫 failed ID까지 읽는 SQL helper를 child에 둔다. N 변경은 reset 후 새 streak부터이며 같은 streak 중 PATCH로 두 번째 escalation을 만들지 않는다. 이력 보존/삭제 경계에서는 이미 존재하는 outbox 사건 키와 run snapshot을 보존하고 과거 실패를 새 사건으로 재구성하지 않는다.

N=3 예: 실패1·2·3은 non-turn P만, 실패4는 Announce 명령 메시지1건, 실패5부터는 after_incident TTL 키의 P만이다. 성공 후 실패1부터 다시 센다. 기존 pause/일정/auto-resume 규칙은 유지하므로 paused 동안에는 다음 실패 run을 만들어 N+1을 억지로 채우지 않는다. JS paused terminal reset과 failed run 뒤 routine만 자동paused인 경우를 구별한다(후자는 실제 stored failed로 카운트). 대응 메시지는 `[실행 보기](run deep link) · <루틴명> · <허용목록의 안전 원인 한 줄> · 4회 연속 실패(N+1)`이다. 동적 원문/셸 출력/secret/프롬프트를 넣지 않는다. 대응은 진단·조치 요청이고 실패한 routine prompt 재실행이 아니다. 해당 Announce intake 자체는 routine_run이 아니므로 이 카운터를 올리지 않아 재귀 escalation이 없다. 봇 턴도 기존 입력의 Busy/queue·권한·순서 정책을 그대로 따른다.

경로는 `agentdesk send-to-agent`의 **기본 실제 Discord post**가 쓰는 기존 Announce backend와 같은 입력 provenance를 재사용한다. `--start-turn`/handoff의 direct health start로 우회하지 않는다. P 알림은 non-turn OperationalAlert로 유지하고 N+1 Announce는 실제 bot author/message identity를 쓰는 기존 agent-message 게시 계약을 따른다(`/message` body에 /handoff metadata/source가 전달된다고 가정하지 않음). 같은 등록 source=`routine-runtime`, bot=`announce`, 새 reason=`routine_failure_incident`이며 기존 non-turn `routine_consecutive_failures` reason을 재사용하지 않는다. OperationalAlert로 표기해 intake가 차단하거나 새로운 bot allowlist/권한을 추가하면 수용 실패다. 실제 Announce가 allowed_bot_ids 바깥이어도 기존 gate 예외가 허용한다. 기존 ingress를 다른 Announce 메시지와 똑같이 쓴다. 전역 author_is_bot 재추론 수리는 P2-F 후속 이슈 후보로 남기고 H에서는 바꾸지 않는다. 허용목록 추가·본문 marker를 권한으로 쓰는 해법은 없다. 자세한 현행 좌표는 위 전수표에 고정한다.

streak 첫 failed run id로 기존 message_outbox 사건 키 `routine:{id}:failure_incident:{first_failed_run_id}`를 고정하고 기존 persistent dedupe transaction으로 terminal 저장/incident 예약을 결합한다. incident row에는 만료 없는 기존 보존 계약을 사용한다. **현행 generic outbox는 post-accept Unknown에서 재시도할 수 있어 그대로 exactly1이라고 주장하지 않는다.** H는 같은 outbox row의 기존 전송 경계에 stable Discord nonce·durable posting/ACK/unknown 상태를 최소 확장한다. 기존 source/bot/target 검증 뒤 post 전에 posting을 commit하고 ACK는 같은 row에 기록한다. 작은 incident 본문에 한해 기존 manual v3의 `with_create_nonce` 경계(`outbound/manual_delivery.rs:606-613`)를 typed purpose로 확장한다. headless_nonce의 기존 source=headless_turn 제한(`outbound/manual_delivery/headless_nonce.rs:71-97`)을 임의 source 전체로 넓히지 않는다. HTTP200/in-flight/skipped만으로는 ACK가 아니며 nonempty 실제 Discord message ID와 nonce/claim CAS가 필요하다. positive NotSent만 같은 사건 재시도, MayHaveSent/ACK 저장 실패·post 뒤 재시작은 `completion_unconfirmed`로 종결해 대시보드에 남기고 자동 재게시0이다. transport row는 기존 held+NULL expiry로 계속 dedupe fence를 보존하고, 진단 projection만 completion_unconfirmed terminal로 닫는다. failed로 내려 partial unique key를 풀거나 GC로 지우지 않는다(`message_outbox.rs:293,766-788`). nonce가 보존된 같은 기존 전송기의 권위 있는 확인만 보충 가능하며 새 receipt store/입력 owner/별도 sender를 만들지 않는다. 기존 routing_notice의 다른 owner pointer는 가져다 쓰지 않는다. ACK는 Discord 게시 증거이지 native 대응 턴 완료 증거가 아니다. P2-H: incident held row는 staging activation(`message_outbox.rs:713-722`)·activate_or_confirm·circuit/recovery에서 purpose guard로 제외한다. require-mention/거절 fixture는 ACK1·owner효과0을 ‘게시됨 · 대응 턴 미확인’으로 보이며 ACK만으로 ‘턴 연결됨’을 만들지 않는다. 원 run은 이미 failed이며 대응 턴 성공까지 run liveness를 연장하지 않는다. 불확실 전송에서 대응 턴이 생긴다고 보장하지 않는다.

RED(P2-C/D/H): N3에서 failure1/2/3 non-turn P만·4 Announce1·5추가turn0, N1 경계, outcome 재로그/동시 poll/재시작 중복0, success/reset·interrupted중립·N변경streak고정, paused/stale/classification 자체command0. 실제 Announce 메시지가 기존 intake를 통과해 owner/native1로 가는 fixture와 Announce+OperationalAlert 차단 positive control이 필요하다. Unknown은 completion_unconfirmed/재게시0, outbox 전송오류와 원 run 실패를 각각 남긴다. mutant는 off-by-one·non-turn P도trigger·OperationalAlert provenance·--start-turn우회·post후blind retry·같은streak반복·성공reset누락이다. 모델은 이 사건 전이를 검사하며 실제 Discord 배달은 구현 시험/자연 슬롯 후속 증거가 필요하다.

## 스택 PR·조각·크기·RED 계약

**스택 PR, PR당 조각1~2개, 각 PR prod≤30파일/순증+1500. PLAN 예상+1200 도달 전에 분할한다.** 이 사용자 지시가 r1의 기능 전체 한 PR 지침을 대체한다. PR-CAP-EXEMPT·baseline/allow 인상은 쓰지 않는다. 이 설계 레인은 PR을 만들지 않는다. 아래는 `pr_cap_prod.py <stack-parent> <head>` 기준 FILES 합집합/NET 보수 추정이며 실측이 아니다. 공통 파일은 FILES만 합집합으로 세고 독립 새 기능의 NET을 임의 상쇄하지 않았다.

| 스택 순서 / 조각 | 주요 prod 파일·책임 | 조각 추정 FILES / NET | PR 합집합 FILES / NET | 수정 전 RED와 원인 제거 mutant |
|---|---|---:|---:|---|
| PR1 B+C1 | B: run_diagnostics.rs·discord_log/presentation.rs·runtime/discord_log/failure_alert·server poll/manual glue·non-turn identity. C1: routine_presentation.rs·inflight typed policy/presence_identity·headless start/bridge Result/stream wire/terminal child glue. 기존 store/RunRecord/runs API 재사용; 신규 schema/detail/UI 없음 | B9, C1 8; 합집합17/+1050 목표 | **17/+1050** (추출 필요 시18–19) | logger/agent 없어도 기존 runs API 진단; 진단/preview/banner0·실행단위 안전 알림/census 보존·C1 wirehold→NO_REPLY0/non-NO_REPLY full. P2-D: 새 notice reason이면 non-turn 등록·Announce intake0. 저장 생략/stream hold 제거/preview복원/NO_REPLY진행fallback/알림누락 mutant |
| PR2 A | migration·execution_mode.rs·N 필드/snapshot/검증·store/execution_modes.rs·store/agent_start.rs·loader/context·API/helpers/part_06; PJ 파일/호출 없음 | 11/+700 | **11/+700** | nullable PATCH·old process/persistent/DM·in-flight snapshot·N누락3/범위/다음streak. current row JOIN·backend 자동변경 mutant. PJ는 #8127 뒤 후속 |
| PR2b H(실패 대응) | discord_log/failure_escalation child·기존 failure_alert/store 이력projection·message_outbox 기존 전송 child/nonce상태·agent_handoff Announce backend glue·기존 Announce ingress 그대로·detail docs | 14/+1100(상향1180 전 분할) | **14/+1100** | P2-B/C/D/H: N3의1..3non-turnP/4Announce/5턴0·primary/thread생성0·임계중복0·paused/stale primary1·reason/after_incident TTL·held activation/circuit0·ACK미확인 UI·N1·성공reset·interrupted중립·streak N pin·재로그/재시작/동시성·Unknown no-repost·실제bot intake. threshold조기/Notifytrigger/OperationalAlert/dropACK후retry/reset누락 mutant |
| PR3a D0 | headless_turn/isolated_context.rs·headless_turn.rs glue·routine_metadata·inflight isolation child·scheduled_turn ChannelEffects·completion_postlude/channel_writeback·memory_lifecycle | 8–10/+750 | **8–10/+750** | primary session/history/uploads/boundary/binding 시작·완료·복구 불변, per-run key만 새 session. shared clear/writeback·memory effect 복원 mutant |
| PR3b D | agent_executor/launch.rs·execution.rs·surface.rs·run_policy.rs·owner_input/session_control/runtime glue·fresh_session_reaper·start_deferral·headless_turn/process_mailbox | 12–14/+1000 | **12–14/+1000** | headless↔user 양방향 queue·primary actor idle후 보존, owned thread/DM retire positive; primary_busy600·thread5xx→same thread60/600·full actual-start timeout·primaryfallback0·Started후retry0. retire scope 제거/claimdeadline/primaryfallback/Startedretry mutant |
| PR4 E0 | agent_executor/settlement.rs·store/recovery.rs·agent_executor/owner_input/store/server glue·input_owner/routine settlement·queued_head/v2 exact withdrawal | 10/+800 | **10/+800** | first-key↔deadline/recovery 양 순서, ACK 전running·terminal queue0. cancel_after_key·row만terminal·ambiguousID·withdraw전inflight해제 mutant |
| PR5 F | command_runner.rs/output·runner_spec·runtime/loader/API glue·migrated validator·기존 process/stream_child lifecycle 재사용·runner owned group child·SIGTERM/managed/legacy exit hook | 13–16/+1100 | **13–16/+1100** | 실제 child exit/large output/timeout2초의 두 sleep PID와 groupGone·foreign alive·정상shutdown1·crash no-respawn. child-onlykill/먼저reap/재시작spawn/agentfallback/pipe무한drain mutant |
| PR6a C2 | streaming FinalOnly·Codex final_answer·TUI O presentation child, stream_tick/terminal/session banner의 기존 owner 연결, source phase 보존 | 10–12/+650 | **10–12/+650** | process/TUI/Codex final vs commentary·lease/frontier/restart identity·ordinary human 보존. full_response=final/raw range화면offset/새reader mutant |
| PR6b G | api/routines.ts·barrel·RoutinesPageView/widget/Home glue·ModeEditor/RunDiagnostics/ScriptOutput·run deep-link/detail API·N 설정/대응상태·docs·보존 projection | 13–16/+1100 | **13–16/+1100** | 7개 thread/status/due/backend/paused 보존·4선택·legacy읽기·PJ폼미노출·mode별링크/알림표·N3→4대응/unknown 표시(P2-H ACK미확인/턴연결됨 분리). process→headless/pausedresume/unknown=0 mutant |
| PR7 E(#8114 뒤 마지막) | agent_executor/channel.rs/channel_settlement·owner_input glue·origin/target/routine·공통 busy_participation 훅·injected_steer 내부 결과귀속 projection(표시/O 편집 없음) | 8–10/+850 | **8–10/+850** | Claude/Codex managed/external/late Busy Joined/Handed1·unsupportedBusy→idle1/Helddeadline·공유 답1·주입뒤 전용표시/게시/O편집0·주입전 임계/paused/stale P1/thread생성0(P2-C/E)·세션kill0. Claude-only predicate/선행veto/Submitted성공/Unknown재주입/channel_teardown mutant |

review 제안 순서 B+C1→A→H→D0+D→E0→F→G+C2→E를 유지하되 **D0+D의 기존 +1650은 PR3a/3b로, G+C2의 +1650(상세화 전 약1450)도 PR6a/6b로 분할**했다. UI가 약속하는 C2가 먼저 착지하도록 PR6 그룹 안에서 C2→G로 둔다. 파일 겹침만으로 NET이 줄었다고 꾸미지 않는다. PJ를 스택 밖 후속으로 빼 A는+700, H는 전역 author_is_bot 수리를 빼+1100이다(incident transport 포함, 상향1180 전 H1/H2로 분할; H2 전에는 N+1 미완료 기능을 노출하지 않음). E는 표시/O 편집 제거로+1050→+850이다. PJ UI 제거로 G는 일단+1000이고 PR1에서 뺀 run-detail API/문서 약+100을 G에 합쳐13–16파일/+1100으로 잡았다. 최대 계획+1100이며 구현 중 상향되면 추가 분할한다. F의 shutdown이 기존 helper 재사용 범위를 넘으면 lifecycle child를 별도 PR로 떼고 cap을 올리지 않는다. E가 #8114를 기다려도 PR1의 기존7 소음 제거는 지연되지 않는다.

PR1의 알림 이력 링크는 현 `/routines` 화면(`dashboard/src/app/AppShellRoutes.tsx:116`)과 기존 runs API를 사용하며 존재하지 않는 run-detail endpoint/자동 drawer 선택을 약속하지 않는다. G에서 exact run deep-link를 추가한다.

PR1은 `valid_routine_metadata`와 기존 execution_mode process를 presentation 함수 한 곳에서 읽으며 schema/run_spec을 기다리지 않는다. A가 착지하면 같은 함수가 run_spec 우선·legacy metadata fallback으로 흡수한다. PR3a/b와 PR6a/b는 부모/선행 head에서 검증한 스택이고 활성화는 구현된 capability만 자동 적용한다. 운영자 수동 switch는 없다.

### giant 배치와 검증

새 독립 책임 순증0 대상: `agent_executor.rs`2326, runtime1144, store4207, discord_log2101, session_control1157, loader3266 외에 **headless_turn.rs1545, terminal_outcome_delivery.rs1016, inflight.rs6314, message_outbox.rs1459, intake_gate.rs1388**도 포함한다(현재 source 실측; 과거 baseline 숫자를 현재 줄수로 쓰지 않음). parent에는 필드/mod/호출 교체만 두고 정책·분류·새 branch는 child에 둔다. 각 PR의 maintainability/giant ratchet을 통과해야 하며 giant를 예산 회피용으로 키우지 않는다. API 문서 경로는 `src/server/routes/docs/inventory/endpoints/part_06.rs:405-648`, taxonomy, `routes/domains/ops.rs`다.

H의 새 독립 책임은 `src/services/routines/discord_log/failure_escalation.rs`, `src/services/message_outbox/routine_incident.rs`, `src/server/outbox_actionable_delivery/routine_incident.rs`, `src/services/discord/outbound/manual_delivery/routine_incident_nonce.rs`로 분리한다. 기존 parent에는 mod/typed 호출만 둔다. H의 +1100은 정책/이력 child2개+250, snapshot/API/result4개+135(선행 A와 겹쳐도 상쇄하지 않은 상한), 같은 outbox row migration/helper3개+330, delivery/worker/typed result3개+205, 기존 writer nonce2개+180의 보수 합계다. N+1을 예약하는 producer는 terminal 저장 경계 한 곳이며 logger 재진입은 동일 사건 키를 읽기만 한다. H1/H2로 나눌 경우 H2가 준비될 때 자동 capability로 함께 노출하며 운영자 수동 스위치는 없다.

각 PR은 공식 `pr_cap_prod.py <parent> <head>`와 `pr_cap_check.sh`로 크기를 측정하고 대상 RED→GREEN·caller-cut mutant·원복 바이트 동일·static-only를 남긴다. 새로운 Rust test id의 CI 선택을 확인하고 구현 묶음 끝 전체 clippy/필수 native gate를 수행한다. 이 설계 레인은 Rust/PG/실제 command·Gemini를 실행하지 않았다. TLC GREEN은 구현 GREEN이 아니다.

## 기존 7개 무중단 이전 순서 (DoD 3)

1. **PR1 B+C1**: 기존 metadata provenance만으로 진단 저장/공개 결과 분리·실행단위 실패 알림·Claude process terminal NO_REPLY0/banner0를 함께 착지한다. 7개 mode/row/schedule을 바꾸지 않는다. active6의 아래 exact turn full/final 비교를 활성화 전 증거로 쓴다. 기본 legacy는 non-NO_REPLY full 보존이라 표본 밖 보고를 버리지 않는다. #8114·A·D·C2를 기다리지 않는다.
2. **PR2 A**: additive mode/runner/run_spec만 도입한다. PJ 정의/호출/UI는 포함하지 않는다. 기존 fresh는 effective thread, null-mode provenance·동적DM/persistent·현재 thread·status/next_due/checkpoint를 보존하고 정의를 복제하지 않는다. current7 PJ는 미구현·자동활성화0/7이며 #8127 뒤 후속의 기본off이다. headless N은 기존 thread 동작을 바꾸지 않는 additive 기본3이다. 진행 중 run은 원 snapshot으로 drain한다.
3. **PR2b H→PR3a→3b→4→5**: H가 N+1 기존 명령봇 대응·불확실 전송 종결을 먼저 준비한다. 현재7은 thread라 새 headless escalation이 켜지지 않는다. headless context isolation/양방향 mailbox 계약·thread transient deferral/no-primary-fallback·Started 재실행 금지·owner exact withdrawal·command group lifecycle을 차례로 착지한다. 새 정책은 각 PR 이후 새 claim부터이며 실행 중 run의 target/시계를 바꾸지 않는다.
4. **PR6a C2→6b G**: provider별 FinalOnly를 검증한 뒤 explicit mode 선택 UI와 진단/출력 폼을 노출한다. headless·script의 준비된 기능과 channel 미지원 capability 표시는 서버 응답에 따라 자동이며 운영 수동 enable 절차는 없다. 기존7에 C2를 강제하지 않는다.
5. **PR7 E**: #8114 head/API를 pin한 뒤 Claude/Codex Busy participation·정확한 shared answer 귀속만 기존 owner에 연결한다. 표시는 사람 Discord 입력과 동일하고 루틴 전용 표시/O 편집/별도 게시0이다.
6. 구현 후 자연 슬롯 관측 우선순위는 dependency-update-watcher→memento-scope-audit→daily-report→hygiene→family-yohoejang→family-obujang이다. 실제 시각을 옮기거나 run-now를 강제하지 않는다. paused intake는 **API 읽기 검증만** 하고 resume/실행0이다. 가족의 업무 iMessage와 Discord 본문0은 별개 증거로 확인한다. 신규 script 등록이 기존 launchd를 자동 삭제/병행 실행하지 않는다.

## C1/C2 활성화 전 실물 대조와 후속 PJ 근거

활성화 전 절차는 `GET /api/docs`→routine docs→목록/각 runs?limit=1→agents docs가 정의한 `/api/agents/{id}/transcripts?limit=100`이다. **run.turn_id exact equality로 1행·events[kind=result] 1개**를 요구하고 full `assistant_message`와 terminal `result.content`를 비교한다(`src/services/agents/turn.rs:313-365`, `turn_bridge/stream_loop/content_arms.rs:381-396`). prefix/body/시각 유사 매칭은 금지한다. final 밖 독립 결과가 있거나 비교불가이면 legacy는 NO_REPLY-only를 유지한다. 원시 개인정보 본문은 문서에 복사하지 않는다.

| 루틴 / run id | full chars | terminal result chars / event index | final밖 assistant 내용 | NO_REPLY | 비교 / source API |
|---|---:|---:|---|---|---|
| memento-daily-report / `186e74e8-990b-4c52-9fb0-764725f18adc` | 975 | 975 / `9` | 없음(full=final) | 아님 | COMPARABLE / `/api/agents/personal-obiseo/transcripts?limit=100`; transcript `32951` |
| family-morning-briefing-yohoejang / `ed135c22-bac4-4d08-89d0-864714e70a24` | 239 | 8 / `34` | 4개/223자: 중복 가드·브리프/발송 경로 확인·발송 진행. 별도 완성 보고 없음 | 정확한 terminal=NO_REPLY(8자) | COMPARABLE / `/api/agents/family-counsel/transcripts?limit=100`; transcript `32942` |
| family-morning-briefing-obujang / `e4f43597-3513-48b3-8a0c-3e1fe00c33ad` | 353 | 8 / `46` | 6개/333자: 중복 가드·브리프/발송 경로·캘린더 수집 오류 확인·발송 진행. 업무 진단은 있으나 완성 브리핑은 아님 | 정확한 terminal=NO_REPLY(8자) | COMPARABLE / `/api/agents/family-counsel/transcripts?limit=100`; transcript `32943` |
| memento-hygiene / `debf55f5-de71-46e6-bff0-73f3ce2d6a08` | 260 | 260 / `14` | 없음(full=final) | 아님 | COMPARABLE / `/api/agents/project-agentmanager/transcripts?limit=100`; transcript `32935` |
| memento-scope-audit / `9d392ed9-d150-44db-b6af-35256dc3780e` | 1583 | 1583 / `65` | 없음(full=final) | 아님 | COMPARABLE / `/api/agents/project-agentdesk/transcripts?limit=100`; transcript `32560` |
| dependency-update-watcher / `a9c7e97a-9c03-4563-ba54-d1f0d1e64059` | 1475 | 1437 / `33` | 1개/36자: 점검 완료 후 iMessage 발송 진행 알림. 상세 결과는 final에 있음 | 아님 | COMPARABLE / `/api/agents/project-agentdesk/transcripts?limit=100`; transcript `32670` |


이번 r2 실제 비교는 **6/6 COMPARABLE, NOT_COMPARABLE0**이다. daily/hygiene/scope full=final, dependency는 final 밖36자의 발송 진행, 가족 둘은 terminal 정확NO_REPLY다. 최신 표본에 final 밖 독립 결과 보고 반례는 없지만 미래 전체 무손실 증거는 아니다. 그래서 **기존7 C1은 항상 보수적 NO_REPLY-only, 그 외 full 유지**를 기본으로 택한다. 명시적 thread FinalOnly는 별도 계약이다. r1의 native final 미확인은 이 새 증거로 보완되며 e4의 본문4개 억제 fixture 전제가 확인됐다. iMessage 실제 외부 도착은 이번 비교로 확인하지 않았다. 원시 `logs/r2-comparison-summary.json`과 `r2-transcripts-*.json`에 exact id/hash/길이를 남겼다.

### 후속 PJ 근거: 현재7의 사전 판정 적용 가능 수

운영 API의 script_ref가 지정하는 `~/.adk/release/routines/{script_ref}` 7개와 wrapper 셸 소스를 읽었다(실행0). API에 loaded-source hash가 없어 서버가 현재 읽은 바이트까지 동일하다는 주장은 하지 않는다. source snapshot·SHA는 `logs/r2-source-*.log`, 자세한 분류는 lane-runs `evidence-r2-notes.md`다.

| 루틴 | 현재 실제 tick/작업 근거 | 판정 전 자료수집·단순 판단 전환 판정 |
|---|---|---|
| memento-daily-report | JS26–37 agent wrapper. shell18–19가 memory_stats/search_traces 수집을 agent에게 지시; shell54는 **NO_REPLY 금지·항상 보고** | 제외. 현재 collector 없고 no-change여도 일일 보고 자체가 의무. “판정없음→skip”을 붙이면 보고 계약을 바꿈 |
| family-morning-briefing-yohoejang | JS35–44: marker 중복가드, 스크립트 브리프, 관심사 뉴스 검색, **본문 작성·발송** | 제외. 정기 콘텐츠 작성/발송 자체가 일. marker DONE은 기존 deterministic skip으로 처리할 문제이며 확률 판정이 불필요 |
| family-morning-briefing-obujang | JS35–44: 위와 동일한 작성/발송 흐름 | 제외. 자료를 모은 뒤 yes/no만 남는 업무가 아님 |
| memento-intake-check-20260925 | JS15–17 cp.done skip,27–39 신규 유입 집계·재발 출처 조사. API paused | 제외. 감사/조사 자체가 일인 일회성 업무. cp.done은 deterministic skip. 실행/재개하지 않음 |
| memento-hygiene | JS26–37 agent wrapper. shell52–69 consolidation·분류/정리·건강 모니터,82 결과없으면NO_REPLY | 제외. 사전 자료수집이 아니라 정리/감사 자체가 업무. 조사하지 않고 skip하면 일을 누락함 |
| memento-scope-audit | JS35–59 시간 slice 파편 수집·분류·de-anchor/forget/rescope/보고 | 제외. 명시한 감사 자체가 일인 사례. 단순 통계 판정으로 대체 불가 |
| dependency-update-watcher | JS31–47 CLI/npm 버전, cargo-outdated/audit 등 결정론적 자료 수집 지시;53 변경없음 한줄결과,56–63 **변경없음·부분실패에도 매실행 iMessage 필수** | **기술적 조건부 후보1개**. 명령 수집을 agent 전 shell collector로 분리하면 변경/취약점 판단만 남길 수 있음. 그러나 지금 그대로 no-change→skip이면 필수 iMessage가 사라짐. 무변경 통지를 collector에서 유지하거나 보고 계약을 명시 변경하기 전 opt-in 금지 |


**현재 계약 그대로 즉시 적용0/7, collector 분리·필수 통지 의무 보존 뒤 검토할 조건부 기술 후보1/7(dependency watcher), 제외6/7**이다. 현재7 JS는 모두 agent prompt를 만들고 실제 collector가 없어 defaultoff를 유지한다. 감사/조사 자체가 일인 memento류와 매일 작성/발송/보고 의무를 no-change→skip으로 바꾸지 않는다. 단순 숫자·DONE 중복 가드는 기존 결정적 skip으로 처리한다. 이 조사에서 collector·Gemini API·운영 루틴 실행은0이다.


## 상태 차트와 검증 범위

r1의 실제 RoutineRun 모델군은 outbound-port 아래였다. r2 브리프 경로에 맞춰 **`docs/statecharts/routine-run/`으로 RoutineRun군만 이동**하고 기존 OutboundCallerFlow/OutboundPort transport 모델을 유지했다. README/expected.tsv는 새 경로와 cfg별 기대 위반만 연결하며 기존 unrelated 기대는 약화하지 않는다.

r2는 owner Submit에서 expired/recovering 가드를 없애 루틴 timer/recovery와 독립시킨다. RearmKnownUnsent는 루틴의 결정이므로 기존 guard를 유지한다. TerminalLeavesNothingQueued를 추가하고 LateDispatch는 Policy와 무관하게 검사한다. channel_teardown/script_agent/consumed_as_agent/cancel_after_key의 RED로 종전 공허한 규칙을 감시한다. 후속 PJ의 추상조건 cfg는 기존 기본off와 enabled no→prejudge_no_work skip0·rc2/3fallback 결과를 보존한다. 키 없음 ≡ PrejudgeEnabled=FALSE이며 현 스택의 판정 구현/키 경로 증거가 아니다. Headless/Thread/Channel의 **agent 세 cfg는 같은 상태 공간**이며 Discord 표면 차이는 모델 밖이다. source/actor 정확성은 기존 권위를 외부 전제로 두고 추가 ledger를 만들지 않는다.

r1(off agent49/127·script15/33) 뒤 수정한 같은 r2 source에서 18cfg를 새로 실행했다. off agent3는 각58states/158steps, enabled prejudge agent3는 각196/524, script off/강제enabled 제외는 각25/53이다. 총 **8 GREEN·10 기대 RED**, 공식 checker도 같은 바이트18cfg/실제TLC50회/53.6초/expected집합18/18일치·exit0이다. BeforeHeld는 미종결 RED를 유지하며 noDeadline/noRecovery는 TerminalLeavesNothingQueued와 NoLateDispatch 모두 RED, ResendUnknown은 중복·성공1·late 모두 RED다. 신규 cancel_after_key는 6step NoLateDispatch RED, channel_teardown은 SharedSessionPreserved, script_agent는 ScriptHasNoAgentTurn, consumed_as_agent는 SuccessfulAgentExactlyOne을 각각 깬다. r2 모델 SHA256 `1afd1bc11f42c7909d7490e1b719096c53f8eaf526309f0b1d5df26d8800b5b3`이며 원시 `sc-out-r2/<cfg>/`·`official-checker.stdout.log`·`results.json`에 실제 결과를 남겼다. 전체18cfg 표와 수정 전/후는 모델 보고서를 따른다.

모든 변수/정규화된 failure kind는 위 진입점 표의 값/분기에서 유래한다. 표의 모델 action 열에 미모델링 사유를 적었고, command 비0 종료는 provider 없는 FailBeforeSubmit 대표 실패로만 검사하여 실제 OS process group을 모델링했다고 주장하지 않는다. 세션/본문/알림/OS 검증은 각 PR RED에서 필요하다. 수정 전 r1 결과와 r2 결과·뮤턴트 원시 로그·재현 명령·공정성/생략 범위는 [모델 보고서](../statecharts/routine-run/routine-run.report.md)에 있다.

### r2b→r2c: 여러 run 사이의 N/N+1 모델과 공허 가드 보강

연속 실패·명령봇은 원 run exactly1과 다른 수명이라 RoutineFailureEscalation 모델로 분리했다. r2b는10cfg·5 GREEN/5 기대 RED(source SHA511bbb35…)였으나 동일 run 재평가가 없어 event-key guard와 intake 불변식이 공허했다(P2-B). r2c는 cursor-1의 terminal run을 다시 평가하는 **RelogRun**을 추가해 카운터는 재증가시키지 않고 같은 사건 키를 재사용한다. 생산 코드의 실제 재로그/재시작·DB/CAS까지 모델로 검증했다고 주장하지 않는다.

같은 새 source에서 지정 CLI12cfg를 모두 다시 실행했다: **5 GREEN/7 기대 RED**, 공식 checker12/12 기대집합 일치·44 TLC·45.9초. 정상 ACK+허용 bot은 N1=117states/248steps, N3=145/310 GREEN이며 ConfirmedIncidentsReachExistingOwner를 검사한다. Unknown N1=225/490, N3=265/582와 IntakeRefused=97/203은 안전/typed outcome 종결만 GREEN이고 실제 대응 턴 성공은 아니다. source SHA256 `90af6e1d8903701c869f1aad2dcf71069405b6583fd33c317425eaee96cc7f22`; 원시 sc-out-r2c/를 보존한다.

신규 **DropEventKey**는 RelogRun 경로3step에서 AtMostOneTriggerPerStreak RED(총 요청수/owner효과 중복도 RED), **BypassIntake**는6step에서 OnlyExistingAcceptedBotIntake RED다. 기존 NotifyTriggers/OffByOne/Duplicate/NoReset/BlindUnknown도 기대 RED를 유지한다. r2b 수치는 새 source의 증거로 재사용하지 않았다. 기존 RoutineRun source+18cfg19파일은 그대로여서 같은 바이트의 r2 증거를 승계한다. **키 없음 ≡ PrejudgeEnabled=FALSE**이고 enabled PJ 추상조건은 #8127 뒤 후속용이다.

WF(history read/기존 delivery)는 계속 가능한 단계의 진행을 가정한다. 모델은 fixed history의 failed N+2→interrupted→succeeded→다음 failed N+1→skipped→failed→paused 및 producer/worker interleaving을 검사한다. N은1/3, 카운터는N+2에서 포화한다. 실제 first failed UUID SQL·snapshot N·N PATCH·nonce/CAS·GC·held activation·provider callback 재귀는 H RED, 기존 Announce ingress 의미는 외부 전제다. Unknown에는 자동 probe/recovery 가정을 넣지 않고 completion_unconfirmed로 종결하며 원 run은 이미 failed다. [cfg별 수치·반례·코드 대조](../statecharts/routine-run/routine-failure-escalation.report.md)·[기본 차트](../statecharts/routine-run/routine-failure-escalation.chart.mmd)·[event-key mutant](../statecharts/routine-run/routine-failure-escalation-event-key.chart.mmd)·[intake mutant](../statecharts/routine-run/routine-failure-escalation-intake-gate.chart.mmd)를 따른다.

## 마지막 리뷰 P2 → 구현 RED·수용 기준 이관

추가 설계 리뷰는 없다. 아래 RED는 계획이며 Rust 시험/production mutant가 통과했다는 뜻이 아니다. 이번 문서·모델 검증과 구현 head의 RED→GREEN→mutant RED를 구별한다.

| 리뷰 항목 | 담당 조각 / 수정 전 RED·수용 계약 | 원인 제거 뮤턴트·이번 처분 |
|---|---|---|
| P2-A 표 갈라짐 | 문서 즉시: 전수표 header 이후 마지막 행까지 연속 표·5열 유지, 약15행 포함 | 중간 빈 줄 재삽입 시 연속 행 검사 실패. 문서 검사 결과는 보고서 |
| P2-B 사건 키/허용 bot | H: 같은 terminal run 재로그·재시작·동시 poll→사건 row1/Discord post1/기존 owner original1. 허용 bot 아닌 입력0 | drop_event_key/bypass_intake_gate 모델 mutant 및 실제 H key/allow guard 제거 mutant RED. 모델 범위 보강 결과는 별도 보고서, DB/CAS/재시작 실물은 H 전용 |
| P2-C primary/thread0 | H(+channel 연결 E0/E): headless N3의 모든 알림/incident target=primary·thread create0, 기존 exactly-threshold 중복0, paused/stale primary1. channel 주입전 임계/paused/stale P1·thread0 | 기존 routine target resolver로 복원하면 create/target assertion RED. 기존 thread7 정책과 혼합하지 않음 |
| P2-D non-turn/사건 키 | H, PR1의 새 안전 알림 reason도 적용: P reason non-turn 등록·Announce+OperationalAlert intake0; incident는 actionable/non-turn 둘다 제외·표지0/Notify fallback0·intake1.1..N run-id, N+2 after_incident:firstfailed+24h TTL | identity 표+Announce notice 거절/incident 허용 쌍 fixture. reason 등록 제거/incident 표지 추가/after 키를 run별로 변경 시 RED |
| P2-E channel 주입전 가시성 | H/E0/E: JS 오류3회→P1·thread0; known-unsubmitted Held deadline(channel_input_unavailable)/deferral 소진 임계도 동일. paused/stale P1. 주입 뒤 기존 owner 표시·추가 진행/라벨0 | post-injection 실패에도 routine notice 추가하거나 전부 dashboard-only면 RED. 기본값으로 진행·확인 안내1건을 결정 완료에 기록 |
| P2-F 전역 author_is_bot | H에서 제거. 다른 Announce 입력과 동일 경로/동일 의미; 후속 이슈 후보로만 기록 | `ingress.rs:407-413`의 전역 수리를 이 스택에 넣지 않음. 후속이면 merge/prefix/render 영향 별도 RED |
| P2-G 입력 상한 | #8127 뒤 후속 PJ: UTF-8 8192/8193bytes·한글 경계, 초과 gate/CLI/network0·agent1 | cap 제거/임의 truncate→no 또는 초과 호출이면 RED. 키 없음 RED/뮤턴트①②도 같은 후속 |
| P2-H ACK/held | H/G: require-mention fixture ACK1·native/owner효과0은 ‘게시됨 · 대응 턴 미확인’; 수용 증거 후만 ‘턴 연결됨’. incident held는 staging activate/activate_or_confirm/circuit/recovery/GC로 재게시0 | ACK→turnlinked, held activation 허용, unknown→failed(unique key 해제) mutant 각각 RED. transport held와 UI terminal outcome 분리 |

PR1은 P2-C/H의 새 mode/incident 기능을 선행 구현하지 않는다. 현재7 thread의 진단 저장·본문 projection·실행단위 안전 실패 알림만 처리한다. 기존 run API/section 저장/row-lock merge는 main의 충족 기능을 재사용한다. 상세 파일/RED·뮤턴트·의존은 최종 보고서의 ‘PR1 조각 계약’이 구현 브리프 입력이다.

## 결정 완료

16:13 사용자 답3건 완료,16:59 키 없음 결정도 후속 PJ에 반영했다. 열린 차단 결정0, P2-E 기본값 진행·확인 안내1건이다.

1. 채널 표시: A/B 모두 채택하지 않고 프롬프트 주입 뒤 사람이 Discord에 입력한 것과 동일 처리. 루틴 전용 진행/출처/공유 라벨·O 편집·별도 thread0, 귀속은 run API/대시보드만. P2-E 보완으로 주입 전 실패 임계·paused·stale의 non-turn primary 알림(thread 생성0)을 기본값으로 진행하며 코디네이터 브리핑에서 사용자 확인을 요청한다(진행 차단 아님).
2. 단순 스크립트: A 확정. 같은 루틴 폼/API에서 executable·argv·cwd 직접 등록.
3. 조용한 headless: N(설정 가능, 기본3)회까지 primary 알림만, N+1에 기존 명령봇 Announce→intake 대응 턴1회. paused/stale/classification 자체는 알림만이며 성공 시 카운터0.

사전 판정 도입은 승인됐으나 현재7을 켜라는 승인은 아니다. 키 없음은 호출0·warn0·기동1줄 Off이며 PJ는 #8127 뒤 후속이다. 마지막 리뷰를 r2c에 반영했으며 더 이상 설계 리뷰는 없다. 남은 P2는 아래 구현 RED로 이관한다.

## 리뷰 집중 항목과 남는 한계

- 경계 시각: claim snapshot/PATCH, first-key↔deadline cancel, terminal↔timeout 동시성을 같은 actor/기존 row fence로 판정한다. 단순 시각 비교로 원문을 버리지 않는지 E0/E 시험이 담당한다.
- 동시성 경합: machine 정책 managed-active/early/late/route 네 접점이 같은 capability를 소비하는지, shared answer attribution과 O lease가 하나인지 검토한다. 중복된 frame registration/본문 emitter가 생기면 차단이다.
- 옛 형식 복원: legacy process 키·missing metadata·persistent/DM·과거 thread id·in-flight mode edit를 A/D/G에서 고정한다. 현재 row를 과거 run에 투영하지 않는다. 동적 DM 구형 정의는 자동 explicit-mode 승격 없이 유지한다.
- N+1 전송: generic outbox의 Unknown 보존은 현재 미충족이다. H의 같은-row posting/ACK/unknown·activation/circuit 차단 RED로 막고 전역 author_is_bot 수리는 후속 후보로 둔다. 모델 GREEN을 실제 native 대응/Discord 도착 증거로 쓰지 않는다.
- 문서로만 선언한 가드: C1/C2 source projection과 exact-withdraw는 현 main에 없는 구현 책임이며 PR1/C2/E0의 RED·뮤턴트로 검증해야 한다. #8114 약속을 구현 완료 증거로 쓰지 않는다.
- 잘못된 0/PASS: script0·start-failure0·unknown·actual accepted1·Discord no-visible-result를 구별한다. O/transport ACK만으로 succeeded로 올리지 않는다. 이 조사에서 알림 도착·구형 paused backend는 미확인이다. r2의 exact terminal 비교6/6과 현재 prejudge 대상0/7을 성공 전달/실행 증거와 구별한다.
- 기존 결함 범위: 이름 기반 orphan reaper(`fresh_session_reaper.rs:25-153`)와 best-effort session ownership stamp(`launch.rs:273-285`)를 더 넓히지 않는다. channel에는 reset/kill/reap 권한을 주지 않으며 process cancel은 기존 exact turn만 사용한다.
- 임의 셸 내부 side effect/group을 이탈한 하위 daemon의 exactly-once, 전역 relay replacement-safe 내구성은 이 모델의 보장 밖이다. 사용자의 데이터/세션 삭제·kill 권한을 성공률 명목으로 추가하지 않는다.

<!-- doc-path-refs: proposed src/services/discord/router/message_handler/headless_turn/isolated_context.rs src/services/routines/agent_executor/channel.rs src/services/routines/agent_executor/settlement.rs scripts/report.sh -->
<!-- doc-path-refs: proposed src/services/routines/store/diagnostics.rs -->
<!-- doc-path-refs: proposed src/services/routines/agent_executor/surface.rs src/services/routines/agent_executor/run_policy.rs src/services/routines/store/recovery.rs src/services/routines/agent_executor/channel_settlement.rs src/services/routines/command_runner.rs src/services/routines/command_runner/output.rs src/services/routines/runner_spec.rs -->

<!-- doc-path-refs: proposed src/services/routines/prejudge.rs -->

<!-- doc-path-refs: proposed src/services/discord/router/message_handler/provider_dispatch/owner_input/busy_target.rs src/services/discord/router/message_handler/intake_turn/owned_busy.rs -->

<!-- doc-path-refs: proposed src/services/routines/discord_log/failure_escalation.rs -->
