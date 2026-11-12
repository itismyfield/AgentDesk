# 루틴 실행 방식 네 가지 설계 (#8117)

기준 main `5bcbb82297f5cc3ab8897b81ddb9891459e0321b`, worktree `inv/i8117-routine-modes`, 2026-11-12 KST. 조사·설계 산출물이며 **프로덕션 변경·cargo 빌드/시험·운영 실행은 하지 않았다.** 이슈의 「내용」·DoD가 수용 기준이다. 코드 좌표는 이 SHA 기준이다.

## 결론과 선행 조건

루틴 정의의 실행 방식은 `headless | thread | channel | script` 하나로 고른다. 실행 방식과 process/tmux라는 백엔드를 구별한다. 현재 process 실행도 Discord 스레드를 만들고 일반 relay와 진단 logger가 각각 게시한다. **현재 운영 7개는 모두 유효 방식 `thread`로 보존 매핑**하며 process 선호·고정 스레드·일정·paused 상태를 그대로 유지한다. 구형 정의의 null-mode provenance를 보존하여 동적 DM action 등 과거 계약을 자동 승격으로 깨뜨리지 않는다. 별도 운영자 전환 명령이나 실행 중지 절차는 필요 없다.

#8114의 `design-1.md`와 후속 `design-boundary-delta-1.md` 계약을 전제로 한다. `InputIdentity`, `CurrentSource::{Ready,AwaitedClear,Unknown}`, `ObserverOwnership`, 공통 ClaudeBusy operation과 기존 adapter의 key-time guard를 재설계하지 않는다. #8115의 기본 실행 노드 지정 에이전트 새 스레드 세션 수리가 완료된다는 것을 thread 방식의 전제로 둔다. 설계·fixture 준비는 병행 가능하고 channel 활성 호출은 #8114 착지 후 통합한다.

이미 main이 충족한 것은 다시 만들지 않는다: 동일 루틴 스레드 재사용(`agent_executor.rs:1010`), JS complete/skip/pause의 턴 0(`runtime.rs:675-785`), claim/in-flight 중복 방지(`store.rs:685-717,3168-3224`), owner original/ticket custody(`agent_executor/owner_input.rs:200-283`), process attempt 백엔드 복원(`agent_executor/execution.rs:19-29`), 실행 결과 API·대시보드 요약(`routes/routines/handlers.rs:178-199`, `RoutinesTimelineWidget.tsx:558-638`). 남은 것은 모드별 라우팅·게시 정책, channel Busy 참여/정산, 직접 command runner, 진단 가시성 및 호환 이전이다.

## 수용 계약과 턴 수의 의미

| 방식 | 실행·세션 | Discord에서 보임 | Discord에서 안 보임 | run 이력·대시보드 |
|---|---|---|---|---|
| headless | 매 실행 새로운 Claude/Codex process provider 세션. 기존 primary를 workspace/권한 문맥으로만 사용하고 synthetic session key로 분리 | 없음. 스크립트가 업무 목적으로 명시 호출하는 외부 전송(iMessage 등)은 별개 | 스레드 생성, 프롬프트·중간 문장·본문·배너·NO_REPLY·성공/실패 진단 전부 | 실행·응답·오류·전달 증거·실패/paused 알림 카드 |
| thread | 루틴당 지정 스레드 하나. 실행마다 새 provider 세션; 현재 process 백엔드도 유지 가능 | 읽을 최종 결과 한 번, 사람이 조치할 실패/일시정지 알림 | run/script/count/checkpoint/action/prompt/preview, 중간 진행, 새 세션 문구, 제어 토큰 NO_REPLY | 모든 진단과 원문, 기존 답의 링크·배달 상태 |
| channel | 에이전트 본 채널의 현재 세션. 없으면 기존 cold start; Busy면 같은 owner로 프롬프트 주입 | 본 채널 기존 답·작은 루틴 출처 표시·공유 처리 표시·실패/paused 알림 | 별도 routine thread/세션, prompt 재게시, 중복 preview, 새 세션 진단 | 같은 original/attempt의 수용·합류/다음 턴 이관·공유 답 참조·오류 |
| script | QuickJS의 종결 action 또는 직접 command 실행. AgentDesk provider 턴 0 | 없음 | agent/session/thread/성공 로그·stdout/stderr 원문 | 결과·stdout/stderr·종료 코드/신호·timeout·실패/paused 알림 카드 |

‘실행 한 번에 에이전트 턴 정확히 1’은 **agent action이 실제 수용되어 성공한 run의 원문 1회 실행**, 모든 run의 중복 실행 0으로 검증한다. JS complete/skip/pause, script 모드, 시작 전 실패·취소는 턴 0이며 성공한 agent 실행으로 세지 않는다. channel Busy는 한 프롬프트가 이미 진행 중인 native 턴에 합류할 수 있어 새 native 턴 수를 무조건 1로 강제할 수 없다. run에 `execution_count=0|1|unknown`, `completion_evidence`를 구별해 기록하고, unknown을 0/PASS로 표시하지 않는다. `Consumed` 명령 처리는 `outcome_kind=command_consumed`, agent count0인 별도 종결로 투영하며 agent 실행 1 성공과 구분한다.

main은 Started 후 timeout/provider 오류에도 같은 run에서 새 턴을 재실행한다(`agent_executor.rs:361-515,653-815`, `store.rs:2383-2429`). 새 모드에서는 **positive NotSent인 제출 전 재시도만 허용**하고, Started/MayHaveSent 이후 새 turn retry·다른 agent fallback을 금지한다. 실패 알림과 다음 정상 예약 슬롯으로 종결한다. 이미 진행 중인 구형 run은 최초 snapshot의 구형 정책으로 drain한다. 활성 6개는 현재 retry=0/fallback 없음이므로 정상 이전의 재시도 동작은 변하지 않는다. 이것을 빼면 모델의 ‘한 번’은 현재 코드를 검증한 것이 아니다.

## 현재 동작 진입점 전수표

표에서 `routines/`는 `src/services/routines/`, `discord/`는 `src/services/discord/`다. 아래 모든 게시 경로를 새 mode의 presentation 정책이 덮어야 한다.

| 진입점·좌표 | 현재 실행/상태/소유 처리 | 현재 Discord 게시물 | 설정 영향 |
|---|---|---|---|
| `src/server/mod.rs:2630-2680` runtime 구성 | script registry, store, logger, executor 생성. timeout·headless_fresh_agents는 boot-bound | 구성 자체 없음 | flag 변경은 재시작 전 executor에 반영 안 됨 |
| `src/server/mod.rs:2701-2707,2830-2852`; `routines/runtime.rs:115-123` 시작 유예 | startup grace 동안 due claim만 보류, poll/recovery 지속. 지나간 slot은 due에 남음 | claim 전 없음 | startup_grace_secs live; 0이면 유예 없음 |
| `routines/store.rs:685-717,3168-3224` 예정 시작 | enabled·due·in_flight=NULL 행을 FOR UPDATE SKIP LOCKED, run UUID 생성+in_flight CAS. lease 1800초(:20) | 이후 runtime.rs:44-45 시작 로그 | strategy는 claim snapshot에 복사 |
| `src/server/routes/routines/handlers.rs:400-491`; `routines/store.rs:2197-2221` run-now | 동일 in_flight guard, loader/이전 검증 후 claim, execute_claimed_script_run 공유 | :465 시작, :483 outcome, :485 failure streak | helpers.rs:99가 boot config flag 전달 |
| `routines/runtime.rs:47-84,126-151` launchd 이전 검증 실패 | failed+paused(migration_invalid), 닫힌 run만 결과 반환 | :75 outcome 진단 | strategy/flag 무관, agent 턴 없음 |
| `routines/runtime.rs:165-272` JS 입력 구성 | agent context·observations·automation inventory·checkpoint 수집 | :242-251 logger js_inputs, observations/inventory 개수·checkpoint 요약 | strategy가 JS context에 전달됨(:259) |
| `routines/runtime.rs:274-315,656-663` JS 평가 에러 | heartbeat 후 execute_tick, 공개 error는 고정 안전 사유, 상세는 tracing/result_json | due/manual 호출자가 outcome 로그 | 실패 pause knob가 종결 시 pause 여부 결정 |
| `routines/runtime.rs:316-336,338-495` JS 결과 | action명·detail·prompt·checkpoint 변경 logger 호출 후 close_action | js_action: action·요약·프롬프트 최대 900자 | strategy/flag 무관 |
| `routines/runtime.rs:675-709` complete | finish_run→fresh teardown→succeeded | caller outcome 로그 | JS만이면 agent 턴 0; fresh teardown은 persisted attempt 기반 |
| `routines/runtime.rs:711-748` skip | skip_run→fresh teardown→skipped | caller outcome 로그 | 동일 |
| `routines/runtime.rs:750-785` pause | pause_after_run→fresh teardown→paused | caller outcome 로그 | 동일, 사람 조치할 pause 알림과 진단 결과 현재 혼재 |
| `routines/runtime.rs:787-829`; `routines/agent_executor.rs:110-195` agent | agent_id 필요, start_turn→consume_start; 오류는 transient defer 후 retry/fallback/fail | caller outcome 로그(실제 started도 status=running 로그) | execution.rs:11-17 backend 선택 |
| `routines/agent_executor/execution.rs:11-46` backend 선택 | fresh+flag=true만 Process, 그 외 Tmux. Process Claude/Codex만 허용 | backend가 Discord 유무를 결정하지 않음 | **process→tmux 자동 fallback 없음**. 미지원 provider는 에러, 결함/실패시 flag fallback 안 함 |
| `routines/agent_executor/launch.rs:30-56`; `start_deferral.rs:41-68` 사전 검증 | binding/provider/primary channel/DM id 검증, runtime 등록을 thread·reservation 전 확인 | 사전 검증 자체 없음 | provider_not_ready/mailbox_busy만 transient |
| `routines/agent_executor/launch.rs:58-108` 표면 라우팅 | dmUserId면 DM reserve; 아니면 routine thread resolve, 실패하면 primary channel fallback + notify bot | process든 tmux든 DM/thread/primary에서 답이 게시될 수 있음 | **headless_fresh_agents는 silent가 아니다**. flag와 무관하게 thread를 잡음 |
| `routines/agent_executor.rs:1010-1079,1108-1209` 실행 스레드 재사용/생성 | saved id valid+권한 확인이면 재사용; invalid/권한 실패면 replacement create; id update 실패는 warning만 저장 | 새 routine thread 자체와 이후 turn 본문. 제목 routine <name> - <agent> | fresh/persistent 양쪽 같은 스레드 resolver |
| `routines/agent_executor/launch.rs:111-160` intent metadata | run/turn/agent/provider/channel/parent/thread/dm/prompt/strategy/backend 저장, planned에 synthetic label | 이 객체 자체는 API/run result; logger가 일부 진단/preview를 재게시 | fresh-context 보장은 시작 확인 후만 true |
| `routines/agent_executor/launch.rs:161-163`; `owner_input.rs:165-190,222-283`; `store/agent_start.rs` owner admission | TUI owned target이면 durable run intent 먼저 기록→같은 original/ticket admission→accepted/pending. 프로세스·비owner는 legacy start로 | admission 자체는 owner 기존 표면/수명 계약. routine 진단은 별도 logger | process는 legacy; owner 경로는 새 세션 소유권을 routine에 주지 않음 |
| `routines/agent_executor/launch.rs:164-212` legacy start | turn_id를 run에 먼저 기록, DM 또는 owner_channel start_reserved 호출 | provider streaming·terminal relay로 중간 진행과 결과 | Process/Tmux를 metadata로 start에 전달 |
| `discord/router/message_handler/headless_turn/routine_metadata.rs:36-85`; `headless_turn.rs:487-525` fresh 경계 | fresh는 synthetic session-key basis, persist context boundary→provider clear→cold start. persistent만 continuity 유지 | 첫 답에 새 세션 banner 가능(아래 행) | missing strategy도 valid routine이면 fresh. Process는 fresh metadata에서만 인정 |
| `routines/agent_executor/launch.rs:214-271` 시작 확정 | reserved turn mismatch 거절. actual Started+durable confirm 둘 다 성공해야 fresh_context_guaranteed | 자체 게시 없음 | Accepted/Pending/Consumed를 Started로 오인 안 함 |
| `routines/agent_executor/launch.rs:273-285`; `owned_session.rs:13-90` fresh ownership | fresh non-process non-DM만 실제 receipt/session-key를 기록. 이름 추측 안 함 | 자체 게시 없음 | persistent/process/DM 제외 |
| `routines/agent_executor/owner_input.rs:95-137` start outcome | Pending/Accepted/Started는 running, Consumed만 complete_agent_run+succeeded(에이전트 턴 0 가능) | caller outcome 진단 | 후속 실제 observer가 필요 |
| `routines/agent_executor/start_deferral.rs:6-9,105-125,143-235` transient 시작 재시도 | 60초 뒤, 최초 600초 window. turn=NULL·deferred metadata·attempt event, retry_count 안 씀. window 이후 일반 policy | caller outcome에 deferred reason/요약 진단 | recorded backend 유지; deferred는 원 attempt kind·DM target 유지 |
| `routines/agent_executor.rs:199-334,1561-1583` 최초 시작 실패 | max_retries>0면 retry 먼저, 아니면 다른 fallback agent 한 번, 마지막 failed | caller outcome 진단 | backend는 최초 for_new 결과, backend 전환 아님 |
| `routines/store.rs:2383-2429`; `agent_executor.rs:1282-1287` retry 예약 | 같은 run status=running, turn=NULL, retry_count++, owned session NULL; 60·120·…900초 cap | caller outcome 진단(run id·retry_count 등) | 결과에 backend 유지 |
| `routines/agent_executor.rs:521-648`; `store.rs:1073-1111` retry due | prompt/agent 없으면 failed, due retry는 start_turn 재호출 | caller outcome 진단 | from_result가 Process 보존, 옛 missing field는 Tmux(:execution.rs:19-29). deferred만 원 kind·DM 보존 |
| `routines/agent_executor.rs:653-815,1586-1606` 실행 중 실패 fallback | retry budget 후 fallback 다른 agent 한 번, prompt 없거나 fallback 실패하면 failed/pause | caller outcome 진단 | running fallback도 recorded mode 유지(:741). :738 dm_user_id=None은 기존 정책/결함이며 이번 확장 금지 |
| `routines/agent_executor.rs:336-359`; `store.rs:1114-1130` poll | 모든 running agent lease heartbeat, owner reconcile, turn=NULL이면 retry 처리 | 결과 생길 때 server/mod.rs:2820 outcome | heartbeat는 Held까지 살아 있게 할 수 있음; 실행시각 deadline 별도 필요 |
| `routines/agent_executor/owner_input.rs:37-85,286-347` owner poll gate | exact run/turn/context 확인→start rejection 실패→unbound attempt observer 먼저 연결. bind 전 completion/timeout/retry 관측 보류; admission projection 실패면 같은 ticket 복원 | 직접 게시 없음 | bind/retry 확인 오류는 held이며 새 launch 금지 |
| `routines/agent_executor/owner_input.rs:350-389` owner retry·bind | may_retry 기존 owner 판단만 사용, bind_observer에 exact attempt/source/logical turn 연결 후 projection | 직접 게시 없음 | timeout만으로 pending을 새 입력으로 복사하면 중복 위험 |
| `routines/agent_executor/reliability.rs:74-155` 완료 증거 | 해당 turn·since 이후 첫 nonblank session_transcripts, NO_REPLY면 NoReplyTranscript. 없으면 exact correlation/source turn의 empty_response quality event | routine logger가 preview 재게시; actual relay와 독립 | transcript/empty response가 완료 판단, headless 전달 여부는 bridge 권위 |
| `routines/agent_executor.rs:406-443` 정상 완료 | checkpoint/next_due 반영 complete_agent_run 후 teardown | server/mod.rs:2820 outcome + failure streak 검사(성공은 streak reset) | fresh만 자동 세션 teardown |
| `routines/agent_executor.rs:361-404`; `reliability.rs:26-31` provider error 응답 | owner may_retry gate 후 error-only transcript면 teardown→retry/fallback/fail | 이미 relay된 에러 응답+outcome 진단 둘 다 가능 | 실제 Started 이후에도 새 attempt 가능 |
| `routines/agent_executor/reliability.rs:33-71,157-218`; `agent_executor.rs:449-478` 조기 실패 | fresh만 cancelled/transport_error/prompt_too_long 30초 settle, exact turn_complete 있으면 실패 안 함 | caller outcome 진단 | persistent는 transcript/timeout까지 기다림 |
| `routines/agent_executor/reliability.rs:220-289` pane liveness | fresh nonowner nonprocess만 started 120초 뒤, DeadOrAbsent/Qwen 확정 API 오류를 host guard 승인 시 실패 | 자체 게시 없음→poll 결과 진단 | Herdr/Process/Unknown은 이 tmux probe 사용 안 함 |
| `routines/agent_executor.rs:481-515,991-1007,1228-1232` timeout | per-routine positive timeout 또는 default, latest started attempt 시각+timeout<=DB NOW. teardown→retry/fallback/fail | caller outcome timeout 진단 | owner retry 불허면 :446-448 continue로 timeout도 보류 |
| `routines/agent_executor.rs:818-883`; `execution.rs:101-159` teardown 라우팅 | owner_input 있으면 no-op; Process면 recorded provider/channel/turn만 cancel; 그 외 current routine fresh 여부 확인→controller | 자체 게시 없음, 기존 session/turn 종료 relay는 별도 | script close는 attempts에서 latest started/input_pending routing 복원; arbitrary JS result routing 금지 |
| `routines/session_control.rs:155-190,236-295` fresh tmux teardown | exact resolved target→기존 force_kill_turn/row host guard→session disconnect. recovery token remote-owned면 local kill 안 함 | 자체 게시 없음 | fresh-only; persistent 거절 |
| `routines/runtime.rs:834-878`; `execution.rs:101-124` JS 종결 teardown | Complete/Skip/Pause도 persisted owning attempt 조회, routing metadata는 JS 결과를 신뢰하지 않음 | 자체 게시 없음 | owner/process/newer work 보존 |
| `src/server/mod.rs:2681-2696`; `routines/store.rs:3057-3132` boot recovery | expired running lease를 row-lock+재검사해 interrupted, matching in_flight만 clear | stored thread 있으면 recovery 로그. 없으면 tracing만(discord_log.rs:242-271) | strategy snapshot 아닌 현재 r join; backend 자체는 recovery 안 바꿈 |
| `routines/agent_executor.rs:908-987`; `store.rs:597`; `owned_session.rs` boot reap | positive owned_tmux_session+fresh only, other running run 있으면 skip, exact namespace token teardown | 자체 게시 없음 | Process/DM/owner ownership 없으면 reap 대상 아님 |
| `src/server/mod.rs:2707-2720` periodic recovery | 동일 expired lease 종결. 동시 reclaim 때문에 **fresh reap 하지 않음** | recovery 로그 | 자동 kill 권한 추가 금지 |
| `routines/fresh_session_reaper.rs:25-59,74-96,124-153`; `discord/tmux_reaper.rs:620-682` orphan backstop | fresh+agent+no inflight의 primary/fallback deterministic names만 후보. kill 직전 row+pane 재조회, live/unknown/reclaimed면 preserve | routine 자체 게시 없음. 일반 reaper의 session 종료 알림과 혼동 여지 | persistent·DM·process 전용 label은 후보 아님; 현재 이름 기반 잔여를 확장하지 않음 |
| `src/server/routes/routines/handlers.rs:493-575`; `routines/session_control.rs:84-153` 수동 reset/kill | persistent만 허용. reset 기존 guarded clear; kill 기존 turn lifecycle. 실제 변경이면 run interrupted | API 직접 routine log는 없음; 기존 runtime/provider UI의 상태 변화는 별도 | fresh API는 conflict. 채널 모드에 이 권한을 이동시키면 사용자 세션 파괴 권한 신설 |
| `src/server/routes/routines/handlers.rs:202-248,294-375` attach/pause/resume/detach | attach default=fresh, pause/resume/detach store 변경. detach는 in-flight 거절 | RoutineLifecycleEvent 로그: 부착·일시정지·재개·분리 | agent 없어도 script 루틴 가능; logger agent 없음이면 skip |
| `src/server/mod.rs:2739-2764`; `routines/discord_log.rs:286-324` stale paused | threshold>0만 scan, routine id+TTL dedupe | thread 있으면 일시정지 장기 알림, 없으면 tracing warn만 | stale_paused_alert_secs=0 disable |
| `src/server/mod.rs:2766-2807` failure auto-resume | failure pause만 cutoff 후 자동 resume, manual/migration-invalid 제외 | tracing info만, lifecycle Discord log 호출 없음 | failure_pause_auto_resume_secs>0이면 terminal failure pause+후일 resume; 0이면 자동 pause 없음 |
| `routines/discord_log/failure_alert.rs:47-104` 연속 실패 | latest stored terminal failed streak 정확히 threshold에서 한 번, interrupted 무시, nonfailed reset | routine·id·script·last_error·next_slot actionable 알림 | max_consecutive_failures 기본3(문서),0 disable; pause knob와 독립 |
| `routines/discord_log.rs:90-231,407-457` 진단 로그 조립 | started/js_inputs/js_action/outcome을 routine_runs.discord_log_sections에 각각 저장, 저장 message id로 edit | `루틴 실행 로그` 코드블록: run/script/count/checkpoint/action/prompt/result/error/agent/retry/response preview | strategy/flag에 따른 suppress 없음; **process도 동일** |
| `routines/discord_log.rs:545-650` 진단 실제 transport | 직접 outbound HTTP edit/send, without_idempotency; transient edit 실패는 error, permanent/confirmed missing은 replacement POST | 시작 summary가 편집되며 재게시될 수 있음 | agent 본문 relay와 별개 책임이며 삭제할 수용 대상 진단 경로 |
| `routines/discord_log.rs:460-542,652-720` log target/thread | saved thread 있으면 사용; 없으면 store 최신 thread 재확인→create+persist; 실패면 primary fallback | JS-only여도 agent 연결돼 있으면 log thread가 생김 | 새 headless/script는 이 logger 경로까지 막아야 Discord 0 가능 |
| `routines/discord_log.rs:723-782` lifecycle/actionable transport | message_outbox_pg 및 TTL shared 경로 사용 | lifecycle/recovery/stale/failure notice | 새로운 알림 transport/dedup 만들지 않음 |
| `routines/discord_log.rs:95,123,157,204-208`; `discord_log/census.rs:4-85` 선례 | legacy-body-census는 시작·JS 진단 생략, outcome native verdict만 | `Legacy body census PASS/FAIL/INCONCLUSIVE...` 한 줄 | 이미 main에 사용자 결과만 남기는 특수 예 있음. 일괄 logger 삭제로 native verdict까지 버리지 말 것 |
| `discord/router/message_handler/headless_turn/replay_start.rs:87-96`; `provider_isolation.rs:594` silent metadata | silent_turn=metadata silent flag. routine launch metadata는 silent를 넣지 않음 | silent flag가 없으므로 일반 relay 게시 | backend=process와 silent는 직교 |
| `discord/router/message_handler/headless_turn.rs:1121-1144`; `turn_bridge/stream_tick/output.rs:403-425` 중간 진행 | typing indicator, provider stream→turn_bridge 기존 emitter | 에이전트 진행 문장·상태 panel/streaming text | logger 진단 제거만 해도 중간 진행은 남음. 결과만 표면 정책은 relay 쪽 metadata로 해결해야 함 |
| `discord/session_banner.rs:107-125`; `tmux_watcher/streaming_session_banner.rs:20-37,66-87`; `turn_bridge/stream_tick/output.rs:414-423` 새 세션 | first logical answer chunk에 shared banner claim prefix | `🆕 새 세션 시작...` | 루틴 metadata별 suppress가 현재 없음 |
| `routines/agent_executor/reliability.rs:96-108`; `agent_executor.rs:1707-1722`; `discord_log.rs:1437-1441` NO_REPLY | no-reply는 완료 성공으로 분류되며 last_result=NO_REPLY; preview가 있으면 outcome도 재게시 | relay가 텍스트 NO_REPLY를 내고 logger preview도 재게시 가능 | 프로세스 bridge_tests.rs:199-211은 ordinary NO_REPLY transport 도달을 명시 단언 |
| `discord/turn_bridge/headless_delivery.rs:272-323`; `terminal_outcome_delivery.rs:856-878` agent terminal | 기존 body source→lease/cutover→headless outbox, committed/ambiguous를 기존 권위로 정산 | user-facing assistant body. NO_REPLY 전용 제외가 여기 없음 | routine 결과 preview logger와 별개; 새 본문 emitter 금지 |
| `discord/router/message_handler/headless_turn/process_mailbox.rs:35-62,72-109` process 완료 후 actor retire | captured actor/claim, bridge Finalized+producer exit 후 safe idle이면 retire; unresolved/cancel retain | 자체 게시 없음 | mode fresh process만; 종료 증거 없는 cancel을 cleanup 성공으로 오인 금지 |


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

권고 이전 순서: (0) additive schema/read compatibility+진단 저장·노출을 먼저 출시, scheduler를 멈추지 않고 각 새 run이 claim 시 모드를 snapshot; (1) enabled 6개와 paused 1개를 자동 보존 매핑(effective thread/fresh, legacy provenance 유지)하되 due·status·lease·thread id 불변; (2) 진단 Discord 억제를 전부 활성화; (3) 낮은 빈도 dependency-update-watcher → memento-scope-audit → memento-daily-report → memento-hygiene → family-morning-briefing-yohoejang → family-morning-briefing-obujang 순으로 기존 다음 정기 실행을 관측. paused intake는 재개/실행하지 않고 API read compatibility로 확인. 관측 순서가 배치별 수동 설정 변경을 뜻하지 않으며 자동 일괄 보존 mapping이 기본이다. headless/channel/script 전환은 새 선택 기능 사용자의 명시적 편집일 때만.

## Discord 실물: 11-12 family-morning-briefing-obujang

run `e4f43597-3513-48b3-8a0c-3e1fe00c33ad`, 06:30:16 시작→06:34:16 succeeded. thread `1552549227018850346`, turn `discord:1552549227018850346:9100096699003098956`. run API는 `discord_log_status=ok`, `discord_log_error=null`, `discord_message_id=1570189955823837278`. result는 provider=claude, execution_mode=process, fresh_context_guaranteed=true, assistant_message_chars=353, completion_evidence=session_transcripts.

| 시각 KST | Discord message id | 실제 종류·크기 | 본문에서 확인한 요소 |
|---|---|---|---|
| 11-12 06:30:17.087 | `1570188944576880711` | 루틴 실행 로그 코드블록 1,912자 | 시작·JS 입력·JS action·프롬프트: run id/script ref, observations/inventory/checkpoint, action, [프롬프트] 원문 |
| 11-12 06:33:59.982 | `1570189879466393641` | 에이전트 relay 364자 | `🆕 새 세션 시작`, 중간 진행 문장들, 말미 `NO_REPLY` |
| 11-12 06:34:18.187 | `1570189955823837278` | 누적 루틴 실행 로그 코드블록 1,900자 | 앞의 시작·JS 입력·JS action·프롬프트를 다시 포함하고 outcome 추가 |
| 11-12 06:34:18.634 | `1570189957698682892` | chunk continuation/응답 preview 412자 | `[에이전트 응답] preview:`가 relay 본문의 진행 문장들과 NO_REPLY를 다시 게시 |

첫 진단 메시지의 본문 안 시각은 06:30:23인데 Discord 생성 시각은 06:30:17이다. 메시지 편집으로 누적 로그를 갱신하는 현재 형태이므로 “본문 시각=message 생성 시각”으로 정렬하면 틀린다. 종료 로그도 새 메시지 묶음이며 이전 진단 내용을 재포함한다. 프롬프트는 시작·종료 진단에 한 번씩 총 두 번 게시된다.

run API의 `discord_log_sections`는 `started` 154자, `js_inputs` 141자, `js_action` 1,401자, `outcome` 565자다. 원문을 설계서에 복사할 필요 없이 이 API에서 각 진단 구간을 볼 수 있다. 새 thread 계약에서는 시작·종료 진단과 preview 재게시 3개는 없어져야 한다. relay 본문 말미의 NO_REPLY는 확인했지만 native final phase는 이 API 조사에서 읽지 않았다. 별도 terminal fixture가 final=NO_REPLY임을 확정하면 relay까지 포함한 4개 모두 없어져야 한다. 집계 preview를 native 최종 결과로 단정하지 않는다.

## 실패 원인 진단 실물

`memento-hygiene` run `51bbf441-1d06-4482-a2fa-c1d92bcf8d9b`: 11-06 03:00:24 시작→03:30:53 failed, `error="routine agent turn timed out after 1800 seconds"`, result.status=timeout, execution_mode=process, retry_count=0. API row는 `discord_log_status=ok`; thread message `1567961801386364980`의 진단 코드블록(1,531자)에 timeout 원문이 실제 포함된다. 이 message는 시작 때 생성되어 종료에 편집된 것이므로 Discord 생성 시각은 03:00:24.772다.

최근 10개씩 총 61 runs에서 56 succeeded·5 failed(전부 timeout). paused 상태인 routine이 있다는 것과 paused 결과 run을 관측했다는 것은 다르다: 이번 샘플에 paused run은 0건. 부모 채널의 failure/일시정지 전용 알림까지 도착했음은 이번 관측으로 입증하지 않았다. 11-06 03:32 이전 parent 채널 30개 read-only 조회도 해당 routine 알림을 찾지 못했으므로 알림을 “실물 확인 완료”라고 쓰면 안 된다. 기존 failure-alert 코드는 별도 코드 근거로 다뤄야 한다.


운영 설정 파일은 읽지 않았으므로 `headless_fresh_agents` 실제 값은 미확인이다. API에서 확인한 process 실물과 기본 설정값을 혼동하지 않는다. 위의 기존 게시물은 실제 관측이고 아래 신규 방식 예시는 구현 후 만족해야 할 설계 예시다.

## 방식별 흐름과 모듈 경계

### 공통 진단과 claim snapshot

현재 `discord_log_sections` 저장과 Discord 전송이 같은 함수에 묶여 있다(`discord_log.rs:90-231,407-457`). 작은 `routines/run_diagnostics.rs`로 구조 진단 저장을 분리하고 started/js_inputs/js_action/outcome 호출은 항상 저장만 한다. logger가 없거나 agent_id가 없어도 저장한다. `RoutineDiscordLogger`에는 **public result와 actionable notice만** 남긴다. JS 평가 실패의 원래 안전 진단도 run의 진단 필드로 남기되 공개 알림은 현재의 안전 오류 요약을 쓴다. 메모리/비밀 전체 덤프나 임의 환경변수 수집은 추가하지 않는다.

run 최초 claim에서 mode, runner, session backend, target thread/channel, legacy policy 여부를 `run_spec`으로 고정한다. mutable routine row를 poll/recovery 때 다시 JOIN하여 새 모드의 teardown을 적용하지 않는다. 진행 중 PATCH는 다음 run부터 적용하고 in-flight의 target·backend·mode·deadline은 불변이다. 예전 run의 `result_json.execution_mode=process|tmux`는 백엔드 필드이므로 새 product mode로 재해석하지 않는다. 신규 `run_spec.execution_mode`와 `execution_backend`를 명시하며 구형 키는 읽기 alias로만 남긴다.

실행/게시 권위는 #5071 그대로다. run row는 스케줄·진단·관측 projection이고 입력 수용 권위가 아니다. 원문/ticket admission과 FrontClaim/physical writer는 기존 owner, 실행 여부는 mailbox active-turn lease, 본문은 기존 relay owner와 shared delivery lease/O frontier다. run succeeded나 HTTP ACK를 native 수용/Discord 배달의 증거로 승격하지 않는다. 새 큐·전송기·본문 dedup·영구 receipt ledger를 만들지 않는다. 신규 DB 필드는 선택/진단 snapshot이지 두 번째 입력 권위가 아니다.

### headless

QuickJS agent action → 기존 provider 준비 확인 → thread/DM resolver를 건너뜀 → primary channel을 기존 runtime/권한/workspace 문맥으로 사용 → 기존 reservation/mailbox에서 process 실행 → 새로운 synthetic session key와 fresh provider 경계 → 기존 완료 증거 → run 종결. `metadata.silent=true`와 persisted presentation policy를 시작 전에 넣는다. 이미 있는 `replay_start.rs:87-96`, `health/recovery.rs:400-402`, process silent bridge 시험 선례를 재사용하고 복구에서도 출력 0을 유지한다.

기존 primary mailbox가 바쁘면 기존 start-deferral을 사용하며 사용자 TUI를 reset/stop하거나 대체하지 않는다. 새 synthetic Discord 채널 번호·가짜 스레드·별도 mailbox registry를 만들지 않는다. **synthetic session key만 전달해서는 primary를 보호하지 못한다.** 현재 `headless_turn.rs:242-334`는 primary core session/uploads/cleared를 읽고 변경하며 `:337-360,488-518`은 channel clear boundary와 provider 상태를 지운다. 새 headless를 여기에 그대로 통과시키지 않는다.

작은 `headless_turn/isolated_context.rs`가 기존 scheduled snapshot의 turn-local cold-start 의미(`headless_turn.rs:549-564`)를 재사용하는 `IsolatedRoutineProcess` 문맥을 만든다. 실제 agent binding에서 workspace/role만 해석하고 session_id=None·uploads/history/retry-context 없음·memento_context_loaded=false로 시작한다. 이 branch는 primary `core.sessions` 생성/수정·uploads 소비·cleared reset·routine identity channel boundary·provider shared clear·live restore를 모두 건너뛴다. 새 per-run session key만 status/session writeback 대상으로 삼고 channel recent-context 주입도 억제한다(`:687-698`). 일반 thread fresh의 기존 channel 경계는 유지한다.

완료 writeback도 격리한다. 기존 `scheduled_turn::ChannelEffects` 판정(`scheduled_turn.rs:100-143`)에 persisted routine-isolation 관측을 연결해 completion postlude의 같은 suppress 결정을 재사용한다(`turn_bridge/completion_postlude/channel_writeback.rs:84-105,198`, `completion_postlude.rs:322,406,609`). routine을 ScheduledTurnMark로 사칭하지 않고 inflight의 routine run-spec/presentation에 isolation을 pin한다. 복구·provider 재시도·memory/feedback postlude도 이 관측으로 같은 결정을 내린다. process cancel/actor retirement는 기존 exact episode/token을 쓴다. D0의 RED는 primary session id/history/uploads/clear boundary/runtime binding이 시작·완료·재시작 뒤 보존되고 별도 per-run key에만 새 provider 세션이 기록되는 것이다.

백엔드 미지원은 명시 실패이며 tmux/채널로 조용히 fallback하지 않는다. 새 headless는 Claude/Codex process 지원 범위부터 제공한다. 기존 thread는 현 지원 provider를 유지한다. 외부 업무 전송은 routine의 명시 도구 호출 범위이며 이 게시 억제가 업무 iMessage 발송까지 막지 않는다. 실패·paused는 전용 대시보드 알림으로 보이며 Discord thread 생성 경로를 호출하지 않는다.

### thread

기존 saved thread를 검증·재사용하고 없으면 한 번 생성/저장한 뒤 같은 스레드를 쓴다. 순차 실행은 기존 in-flight fence로 충분하다. 생성 성공 후 저장 실패는 결과를 실패로 기록하고 **이번 실행은 생성한 thread id를 run snapshot으로 유지**한다. 이후 run의 재조회·재사용에는 기존 thread 복구 증거만 사용하며 이름 검색으로 임의 thread를 채택하지 않는다. 신규 모드에서는 thread 설정 실패를 본 채널 실행으로 폴백하지 않는다. 모드가 바뀌는 자동 실행은 원문 요구와 충돌하기 때문이다. 기존 구형 in-flight fallback은 그대로 drain한다.

실행마다 fresh provider severance와 확인된 start를 요구한다. UI의 새 세션 진단은 게시하지 않는다. process 또는 interactive fresh backend는 내부 선택이며 지정 스레드/항상 fresh라는 제품 의미는 같다. script의 `dmUserId`가 새 explicit mode를 override하지 못하게 action boundary에서 부작용 전에 거부한다. 구형 null-mode DM 정의만 기존 adapter로 유지한다.

게시 정책 `FinalOnly`를 시작 전에 pin하여 stream/status panel/배너 생성을 억제하고, 기존 terminal body owner가 사용자 최종 결과만 한 번 게시한다. **`full_response`나 `assistant_message_preview`는 최종 결과가 아니다.** Codex `rollout_tail.rs:3199-3204`는 commentary+final을 합치는 계약이고 `StreamMessage::Text`는 phase를 잃는다(`agent_protocol.rs:178-187`). 별도 presentation projection이 provider의 terminal/final record를 원래 수집 경계에서 보존해야 한다(Claude `stream_result.rs:5-21`, Codex/native final phase). raw transcript/full_response와 원래 delivery identity/range는 보존하고 화면에 투영할 본문만 고른다. 기존 lease/frontier를 화면 문자열 offset으로 다시 계산하지 않는다.

Claude는 terminal result/마지막 완료 assistant record, Codex는 `final_answer` record의 완료 본문을 사용하고 commentary/tool/thinking은 진단에만 남긴다. final 분류 근거가 없는 adapter는 원문을 추측하거나 부분 문자열로 지우지 말고 `result_classification_unavailable`로 기록·알림한다. provider 지원을 줄이는 방식으로 현재 7개를 이전하지 않도록 Claude process final/NO_REPLY fixture를 첫 통합 시험으로 둔다. TUI O의 structured native record도 같은 presentation contract를 소비하고 별도 reader를 만들지 않는다.

정확한 final control token `NO_REPLY`(trim 후 대소문자 호환)는 terminal successful-no-visible-result로 정산하고 body transport 0이다. 일반 결과 안의 인용된 `NO_REPLY` 문자열은 지우지 않는다. 최종 결과가 NO_REPLY이면 앞의 commentary를 대신 게시하지 않는다. tombstone/receipt 정산은 기존 owner에서 수행하고 ‘본문이 없으니 delivery obligation이 남음’으로 복구 재게시하지 않게 해야 한다. 이 정책은 routine provenance에만 적용하며 사람의 일반 답·기존 ordinary NO_REPLY 의미는 바꾸지 않는다.

기존 census의 native verdict(`discord_log/census.rs:4-85`)처럼 JS만으로 내는 사용자 결과는 명시 public-result adapter로 보존한다. 진단 logger를 통째로 삭제하여 이 결과까지 잃지 않는다. preview를 다시 보내는 logger transport는 제거한다.

### channel과 #8114의 입력 계약

`routines/agent_executor/channel.rs`가 실제 agent primary channel을 해석한 뒤 기존 owner custody/admission을 호출한다. `/api/agents/{id}/handoff`의 underlying start service와 `/turn/deliver`가 도달하는 owner drive를 재사용하며, 내부 HTTP 요청·announce trigger·가짜 HumanInputCaller를 만들지 않는다. cold/idle는 해당 채널의 기존 session을 만들거나 이어 쓰고 synthetic routine label/fresh reset은 사용하지 않는다. shared session은 routine이 소유하지 않는다.

현재 machine-origin Routine는 Busy에서 대기한다. #8114가 보존하는 그 정책을 몰래 바꾸지 않고 **#8117이 channel routine에만 typed Busy participation capability를 추가**한다. 최초 검증된 stored run mode에서 capability를 만들고 admission provenance에 pin한다. `is_person`은 그대로 두며 임의 source/body/metadata bool만으로 승인하지 않는다. 하나의 공통 predicate를 `origin.rs`, `target.rs:78-80`, `provider_dispatch/owner_input.rs:193-213,855-869,1027-1053`의 managed-active Handoff·선행·late 판정과 target route가 함께 소비한다. `:193-213`의 handoff prepare/external active 관측도 같은 capability를 써야 native active에서 기존 `hand_busy(:232)`에 도달한다. fresh/thread/HTTP/scheduled의 own-turn 대기는 유지한다. target 조건 하나만 고치는 수리는 실제 선행 veto를 통과하지 못한다. 이 네 veto 접점을 E 통합 fixture가 각각 거친다.

#8114가 제공하는 operation 순서 그대로: actual operation 취득 → `InputIdentity`와 `CurrentSource`/observer 증거 → original/frame+history registration → owner ready → optional draft park → payload-start → host-gated paste+Enter → Enter-terminal Completion → 기존 settlement와 receipt diagnostic. tmux의 pinned if-shell/first-key stop 및 Herdr per-mutation pin/physical lease는 adapter 내부다. 새 공통 caller에 Herdr/tmux endpoint 분기나 raw send를 넣지 않는다. RemoteTui는 기존 placement로 해당 node의 owner에 보낸다. 미지원 provider/host는 `channel_input_unavailable`로 명시 실패하며 fresh 새 세션으로 대신 실행하지 않는다.

Busy의 `Submitted`는 transport ACK이고 run 성공이 아니다. 기존 `tui_prompt_dedupe/injected_steer.rs:198-222,243-249`의 동일 frame/attempt `Joined|Handed` 및 연결 native terminal/O 결과를 run의 읽기 projection으로 연결한다. 현재 routine observer의 자기 logical turn 기대(`input_owner/routine.rs:216-247`)를 shared native turn attribution에 맞게 작은 child에서 적응한다. 최근 채널 answer 복사나 별도 routine reader는 금지한다. 답이 사용자와 routine 여럿을 함께 처리할 수 있으며 같은 Discord 메시지 링크를 여러 run이 참조할 수 있다.

### 주입 실패·시간 경계·복구

| 시점 | 처리·자동 진행 | 종결·가시성 | 금지 |
|---|---|---|---|
| key=0이며 source/modal/writer 원인 거절 | 같은 original/ticket Held, #8114 reobserve/기존 held_watch가 실제 회복 때 wake | 대시보드 전달 대기+원인. 원래 run deadline 안에서만 재시도 | 새 ticket·timer paste·idle 기다리기만 하는 가짜 Busy 지원 |
| 확정 NotSent이며 provider 준비/예약 충돌 | 기존 60초/600초 start-deferral 및 남은 pre-start retry budget, target/mode snapshot 유지 | timeout이 되면 같은 취소/실패 경로 | backend 전환·deadline 재설정 |
| Held deadline 경합 | owner actor 안에서 exact namespace+ticket+original/context 확인 후 기존 cancel/first-key fence로 **해당 원문만** 철회 | 철회 commit 확인 뒤 failed. 이미 key 가능하면 다음 행으로 정산 | run row만 failed로 바꾸고 원문이 나중에 실행되게 남기기 |
| paste/Enter가 도달했을 가능성 | 기존 MayHaveSent/Unknown 정산만; Enter-terminal 뒤 재전송 없음 | deadline까지 증거가 없으면 failed(`completion_unconfirmed`)와 알림. 늦은 증거는 같은 run에 보충하며 성공/실행을 새로 만들지 않음 | Unknown→Held downgrade, 다른 agent로 재실행 |
| 수용/합류 후 provider 오류·timeout | 같은 native episode의 기존 정산, 새 turn을 시작하지 않음 | failed와 원인·증거. 채널 답은 기존 owner가 필요하면 계속 처리 | 사용자 대화/채널 session reset·kill·clear |
| 서버 재시작/lease 만료 | 새-mode owner run은 같은 ticket 복원 후 제출 전이면 exact-withdraw ACK를 먼저 받음. 그동안 running/recovering 유지. post-key는 기존 정산/no resend | 철회 확인 뒤 interrupted; post-key는 interrupted/unconfirmed와 보충 증거. 다음 정기 슬롯은 기존 in-flight 해제 뒤 별도 run | interrupted만 쓰고 known-unsent queued 원문을 남겨 late dispatch하게 함·현재 설정으로 target 재추론·매칭 안 된 세션 reap |

main의 owner poll은 observer bind 전 timeout도 보류한다(`owner_input.rs:37-55,286-347`), heartbeat는 계속 연장된다. 새 `agent_executor/settlement.rs`는 poll의 owner gate **앞**에서 run deadline을 확인하고 기존 owner 취소/정산을 요청한다. `queued_head/v2/control.rs:206-260`의 ambiguous-ID 거절·revoke_front·Cancelled owe+commit을 재사용하며 exact ticket/context 검사를 같은 actor 요청에 붙인다. 기존 `cancel_queued_primary_message`를 모호한 ID로만 호출하지 않는다. first-key 전에 철회가 linearize되면 key=0, writer가 이미 앞섰으면 Unknown/no-resend다. 새 cleanup 권위나 프로세스 전체 kill을 만들지 않는다. 이 최소 exact-withdraw 적응은 #8117 구현 조각에 포함한다. boot/periodic `recover_stale_runs`도 새-mode owner intent가 있는 run은 generic interrupted UPDATE 전에 같은 복구 coordinator로 넘긴다. known-unsent의 철회/종결 저장이 실패하면 running+recovering으로 보존하고 기존 recovery tick에서 같은 원문을 이어받는다. 전역 저장소 장애 중 강제 종결하지 않는다. process/legacy 구형 run은 기존 recovery 정책을 유지한다.

liveness의 환경 가정은 기존 tick/owner actor/저장소가 결국 다시 응답하고 deadline 판정 기회가 있다는 것이다. 저장소 영구 장애·서버 영구 정지 중 run 종결을 증명한다고 주장하지 않는다. 실패·불확실성을 run에서 보이게 하는 것은 성공 전달과 다르다. 기존 owner custody의 전역 replacement-safe 보장을 이 설계 모델 하나로 완료했다고 주장하지 않는다.

## 선례 없는 UX 두 안 비교 (각 한 라운드)

### 채널 표시·순서

A(권고): 본 채널의 기존 답을 공유한다. 실제 입력 제출이 확인되면 `⏱ 루틴 · 작업 감시` 출처 한 줄, cold/idle 답에는 루틴명, Busy 공유 답에는 `함께 처리: 작업 감시`만 붙인다. 원문/프롬프트·진단은 노출하지 않는다. 표시도 기존 notice/O formatter를 통해 보내고 새 본문 emitter를 만들지 않는다. 표시 전송 실패는 입력을 다시 보내는 사유가 아니다. 비용은 provenance 표시와 정확한 run→native attribution이다. 위험은 사용자 대화와 루틴 결과가 한 답에 섞이는 것이며 선택 UI에 이를 미리 적는다.

B: 같은 채널 세션에서 일하되 결과를 루틴별 스레드로 복사한다. 시야는 분리되지만 공유 native answer에서 루틴 부분을 떼는 근거가 없고 새 배달·중복 위험, 구조 응답 계약 비용이 크다. 요구한 채널 실행의 입력/출력 소유와 가장 잘 맞는 A를 권한다.

순서는 wall-clock/예약 시간으로 재정렬하지 않는다. 이미 granted된 physical writer는 비선점, 그 다음은 기존 owner priority(`queued_head/v2.rs:122`의 사람 입력 우선)와 원문 순서를 쓴다. 루틴끼리는 기존 큐 순서이며 합쳐서 새 원문을 만들지 않는다. 화면의 출처 표시는 실제 제출/수용 순서이고 입력 도착 시각과 다를 수 있다. O가 이미 열어둔 답을 편집하면 기존 내용을 재게시하지 않고 공유 처리 라벨만 갱신한다. 사용자의 본문은 routine-only NO_REPLY/FinalOnly 필터로 지우지 않는다.

### 단순 스크립트 등록

A(권고): 루틴 생성 폼/API에서 executable path·argv 배열·cwd를 직접 등록한다. 예: `{"execution_mode":"script","runner":{"kind":"command","program":"/bin/bash","args":["scripts/report.sh"],"cwd":"/workspace/reports"}}`. 셸 문법은 사용자가 `/bin/bash` 등의 인자로 명시했을 때만 해석된다. agent_id는 선택적 귀속이며 provider 사용 0이다. 기존 JS도 `runner.kind=quickjs`로 등록할 수 있다. 비용은 작은 command runner·출력 UI, 위험은 실행 노드에 파일이 있어야 한다는 것과 셸 내부 side effect의 exactly-once 한계다.

B: JS `portable.entrypoint`를 일반화해 셸마다 JS wrapper를 요구한다. 기존 migration 검증을 재사용하기 쉽지만 등록할 파일이 하나 늘고 agent 실행/직접 실행이 혼동된다. 임의 셸 등록이라는 요구에 맞는 A를 권한다. 새 원격 실행기·관리자 allowlist·운영자 수동 launchd 정리를 필수로 붙이지 않는다.

## 단순 스크립트 범위와 실행 결과

현재 `action.rs:6-32` 및 `runtime.rs:675-785`의 complete/skip/pause는 이미 provider 없이 결과 JSON/checkpoint/next_due를 저장한다. `loader.rs:706-748,790-807`는 격리 QuickJS이며 Node shell API가 아니다. `migrated.rs:13-24,27-56,68-99`는 portable entrypoint/required path/connector 전제를 검증할 뿐 실행기가 아니다. `docs/launchd-to-routine-migration-plan.md:10-34`의 tick은 agent prompt를 반환해 agent가 셸을 호출한다. 따라서 임의 셸 직접 등록·stdout/stderr·exit code는 이번에 추가할 기능이다. 이전 문서의 과거 parallel/cutover 운영 절차를 이번 기본값의 전제로 삼지 않는다.

`script+quickjs`에서 agent action을 반환하면 명시 `agent_action_forbidden` 실패이며 agent launch 0이다. command는 기존 scheduler 실행 노드에서 argv로 직접 spawn, stdout/stderr를 동시에 drain하여 각각 64KiB까지 capture하고 초과 byte 수/잘림 표식을 남긴다(초과분도 drain). 정확한 child 종료/timeout 뒤 pipe drain은 최대 1초의 별도 유예만 둔다. 하위 child가 FD를 보유해 EOF가 오지 않으면 reader task를 중단하고 pipe를 닫아 `capture_incomplete=true`로 기록한다. byte cap과 열린 pipe 종결 한도를 구별한다. UTF-8 불량은 대체 문자와 원래 byte count를 보인다. 결과는 `exit_code|signal`, `duration_ms`, `timed_out`, stdout/stderr, started/finished, runner snapshot이다. exit0은 stderr가 있어도 succeeded; nonzero/signal/spawn 오류/timeout은 failed다. command는 agent retry/fallback을 사용하지 않으며 기본 재실행 0이다.

timeout 정리는 이 run이 spawn하여 보유한 정확한 child handle에만 적용한다. 하위 프로세스 전체 종료·daemon supervisor·login shell·LaunchAgent 설치/삭제·전역 env 수정은 범위 밖이다. child가 자식을 남기면 종료 보장 범위를 대시보드 결과에 드러내며 패턴 kill/새 tmux kill을 추가하지 않는다. 새 runner가 env를 DB에 저장하거나 셸 startup 파일을 읽어 인증을 수집하지 않는다. 기존 서비스 실행 환경을 상속하고 별도 secret 입력 폼은 이번 범위에 넣지 않는다.

서버 재시작 중 child의 실제 side effect는 확인 불가할 수 있다. 기존 run을 interrupted/unknown으로 종결하고 같은 run을 자동 spawn하지 않는다. 다음 정상 cron 슬롯은 계속 온다. 자의적인 셸 내부 exactly-once/외부 AI 사용 0까지 보장하지 않으며 AgentDesk provider 턴 0만 보장한다. 주기 서비스·항상 살아 있는 daemon은 대체 대상이 아니다. 기존 launchd 전체 검색·등록·비활성화도 이번 작업에서 하지 않는다.

## 저장 형식·API·대시보드

### 정의와 구형 복원

새 정의의 정본은 `routines.execution_mode` enum과 discriminated `runner` JSON이다.

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
| POST `/api/routines` | mode·runner, schedule/timeout/name 선택 폼과 동일. 기존 body mode 누락은 구형 quickjs fresh 호환 생성, response에 effective mode/호환 상태 표시 |
| PATCH `/api/routines/{id}` | mode/runner 변경은 다음 claim부터. omitted 유지·명시 null 의미 구분. mode를 초기화하는 null은 새 정의에서 422. mode/strategy 동시 상충은 422 |
| GET 목록·단일 | explicit/effective mode, legacy compatibility, runner, 현재 thread/channel 링크, existing last_run. command에 가짜 script_ref를 만들지 않음 |
| GET `/api/routines/{id}/runs` | 기존 result/attempts/log sections + immutable run_spec·diagnostics·실행/수용/배달 상태. 구형 rows는 없음을 unknown/legacy로 표시 |
| GET `/api/routines/runs/search`, metrics | 기존 검색에 mode/오류 class 활용. 0 output과 unconfirmed 구분, script exit 요약 포함 |
| 신규 GET `/api/routines/{id}/runs/{run_id}` | 같은 store 권한으로 해당 run 상세만 반환. 현재 API에는 별도 detail 없음. 대시보드·실패 알림 안정 링크를 위해 추가 |
| run-now·pause/resume/detach | 기존 공통 claim/close 사용. 새 모드가 별도 실행 진입점을 만들지 않음 |
| routine session/reset·kill | explicit mode는 409. channel 공유 세션 제어로 우회 연결하지 않음 |

구형 client가 새 explicit mode에 strategy만 PATCH하면 동일한 의미의 호환 값만 허용하고 반대값은 422, 누락하면 mode 보존이다. legacy null-mode에는 기존 API를 유지한다. deprecated field는 response에서 호환 projection임을 docs에 밝히고 legacy API 문서에 새 mode 선택을 권한다. 삭제/rename로 구형 run의 process 필드를 새 enum 파싱에 넣지 않는다.

### 화면

현재 `RoutinesPageView.tsx:27-29`는 모든 row에 Discord thread가 있다고 설명하고 `RoutinesTimelineWidget.tsx:925-1027`는 결과 drawer를 갖는다. 새 화면은 mode badge/생성·편집 폼, thread/channel만 Discord 링크, headless/script는 이력 링크를 보여 준다. 새 책임은 `RoutineExecutionModeEditor.tsx`, `RoutineRunDiagnostics.tsx`, `RoutineScriptOutput.tsx`로 분리한다. giant widget은 badge/link/open glue만 변경한다.

run drawer는 ‘결과’와 ‘진단’ 탭으로 구분한다. 결과에는 최종 결과·공유 답 링크·exit 및 실패 요약, 진단에는 run id·script/runner·관측 개수·checkpoint/action·prompt 원문·attempt·Held 원인·receipt/O 정산·backend·Discord 배달 상태를 둔다. prompt는 접힌 상세이며 기존 UI가 갖는 데이터 접근 범위를 그대로 따른다. headless/script 실패·paused는 루틴 목록의 붉은 상태/알림 카드와 run deep link로 볼 수 있다. thread/channel 알림은 기존 message_outbox를 쓰고 링크+루틴명+안전 원인 한 줄만 게시한다. status=failed와 notice delivery 실패를 각각 남겨 알림이 실패했다고 본문을 재실행하지 않는다. 새 모드에서 진단 기록의 실패를 ‘Discord가 조용해서 정상’으로 감추지 않는다.

## 진단 제거 뒤 어디서 보는가: 실물 기반 수용 예

다음은 위 실물 run을 입력 fixture로 삼는 **미구현 수용 예시**다. 실제 신규 모드 운영 성공을 주장하지 않는다.

| 방식·상황 | 사용자가 보는 예 | 보이지 않아야 하는 예 | 원인 확인 |
|---|---|---|---|
| headless 아침 브리핑 성공 | Discord 메시지 0. 대시보드 ‘성공 · 외부 전달 작업 완료’, 응답 진단/완료 증거 | e4f43597 run의 위 4개 메시지와 thread 생성 0 | run 상세에 prompt/attempt/process 및 실제 외부 전달 주장은 도구/결과 증거와 구분 |
| thread 동일 e4f43597, final=NO_REPLY | Discord 메시지 0, 이력 ‘성공 · 표시할 결과 없음’ | 진단 1,912/1,900자, 364자 진행+배너, 412자 preview 모두 0 | run 상세에서 기존 started/js_inputs/js_action/outcome 정보 유지 |
| thread 결과 있는 실행 | 기존 지정 thread에 ‘의존성 업데이트 2건을 찾았어요…’ 최종 결과 한 번 | prompt/response preview 재게시/새 세션 시작/중간 진행 0 | result와 같은 turn의 배달 링크·진단 탭 |
| channel 사용자 조사 중 작업 감시 | 본 채널 `⏱ 루틴 · 작업 감시`; 원래 답에 `함께 처리: 작업 감시` | 별도 routine thread·원문 재게시·공유 답 복사 0 | 해당 run의 same ticket/frame Joined→native terminal/O 링크. 사용자 입력과 공유됨 표시 |
| channel source 불명/Held | 대시보드 ‘전달 대기 · 현재 입력 대상 확인 중’→회복 시 1회, deadline 시 ‘실패 · 제출 확인 불가’ | key0 거절 중 원문 유실/새 input 복사/채널 session kill 0 | exact target/current-source/NotSent 또는 Unknown·철회 증거, 실패 알림 한 줄 |
| script `/bin/bash report.sh`, exit2 | 대시보드 ‘실패 · 종료 코드 2’, stdout `검사 3건`, stderr `입력 파일 없음` | Discord 성공/진단 0, provider turn0 | runner/argv/cwd·출력·exit2·시간·잘림·재시작 여부 |
| 실패 실제 51bbf441 timeout | thread/channel: ‘Memento 정리가 30분 안에 끝나지 않았어요. [실행 보기]’; headless/script: 같은 대시보드 실패/paused 카드 | 1,531자 진단 코드블록·script/prompt 노출 0 | API error=timed out after 1800 seconds, backend, exact attempt·last evidence |

실패/일시정지의 의미 있는 알림은 남긴다. 기존 ‘3회 연속 실패 때 한 번’과 stale-paused/자동-resume knobs는 정의와 threshold 의미를 보존한다. thread/channel의 run 실패/새 paused 전이는 안전 요약 알림으로 투영하고 기존 streak 알림과 같은 원인/전이를 한 outbox 사건으로 정리하여 두 실패 본문을 새로 만들지 않는다. 단순 성공/skip/lifecycle 진단은 Discord에 올리지 않는다. headless/script는 Discord 목적지를 새로 만들지 않고 대시보드에서 같은 사건을 본다. 이번 조사에서 별도 실패/paused 알림의 실제 도착은 미확인이므로 구현 후 대상 시험과 전송 증거로 검증해야 한다.

## 구현 조각·크기·RED 계약

조각은 커밋/리뷰 단위다. 하나의 기능으로 묶어서 마지막에 PR 하나로 올린다(이 레인에서는 PR 생성 안 함). 각 추정은 prod 30파일/+1500 아래이며 **예측 +1200 도달 전에 PLAN 정지**한다. 시험/문서는 prod cap과 별도로 아래 fixture 비용을 계획한다. 명시하지 않은 대형 refactor·새 guardrail·운영 수동 스위치는 없다.

| 조각(순서/의존) | 대상 prod 파일·새 child (경로 축약) | prod 추정(파일/순증) | 수정 전 실패해야 할 계약(RED) | 원인 제거 뮤턴트 |
|---|---|---:|---|---|
| A 저장·호환·snapshot | 새 postgres migration, routines/execution_mode.rs, store/execution_modes.rs, store.rs(필드/위임), store/agent_start.rs, loader.rs(context 필드), routines/mod.rs, routes/routines.rs 및 handlers/helpers, routes/docs/inventory/endpoints/part_06.rs | 11 / +650 | mode/runner 조합·nullable PATCH·old persistent/DM 복원·legacy process 읽기·in-flight 중 mode edit 후 원래 target 유지 PG/API fixture | snapshot 대신 현재 routine JOIN; old process를 product mode로 파싱; omitted PATCH를 fresh로 초기화 |
| B 진단 저장/알림 분리(A) | run_diagnostics.rs, discord_log/presentation.rs, discord_log.rs(위임/transport 축소), discord_log/failure_alert.rs, runtime.rs·server/mod.rs(저장 호출), routes/routines/handlers.rs, store/diagnostics.rs, 기존 census child, api detail handler child | 10 / +700 | logger/agent가 없어도 네 진단 section API에 남음; e4 실물 진단 POST0; failure/pause 안전 알림과 census verdict 보존 | logger None이면 저장 생략; outcome preview POST 복원; census public result 지움; headless failure가 thread 생성 |
| C 최종 결과 projection(B) | discord/routine_presentation.rs, turn_bridge/stream_tick/output.rs, terminal_outcome_delivery.rs(glue), session_banner.rs, tmux_watcher/streaming_session_banner.rs, tui_o/writer presentation child 및 기존 render 연결, claude/stream_result.rs child, codex final projection child/호출 연결, agent_protocol 관련 좁은 자료 연결 | 12–15 / +850 | Claude/Codex process+TUI fixture에서 progress+final/NO_REPLY/인용 토큰 분리, ordinary human 동일, lease/frontier·restart replay 한 본문 | full_response를 final로 사용; NO_REPLY 전 commentary fallback; routine policy 복구 누락; raw range를 화면 길이로 advance |
| D0 headless turn-local 문맥(A,B) | 새 headless_turn/isolated_context.rs, headless_turn.rs(기존 snapshot helper 호출 교체), routine_metadata.rs, inflight.rs의 routine isolation 자료 child/glue, scheduled_turn.rs(기존 ChannelEffects 재사용), completion_postlude/channel_writeback.rs 및 completion_postlude.rs(glue), memory_lifecycle.rs(같은 suppress 소비) | 8–10 / +750 | primary session/history/uploads/clear boundary·live binding 시작/완료/복구 불변, per-run key만 새 session; scheduled snapshot 보존 | isolated flag 유실; shared provider clear 복원; 완료 core.sessions writeback 허용; memory/feedback channel effect 복원 |
| D headless/thread launch·종결(A,B,C,D0) | agent_executor/launch.rs, execution.rs, 새 surface.rs·run_policy.rs, agent_executor.rs(glue), owner_input.rs, session_control.rs(glue), runtime.rs(glue), health/headless_turn 관련 작은 연결, router/headless_turn/routine_metadata.rs, fresh_session_reaper.rs | 12–14 / +900 | silent process thread/DM/API Discord0; busy primary/user session 보존; 고정 thread 여러 run fresh; thread 설정 실패 primary fallback0; post-start retry/fallback0; old run 복원 | silent metadata 제거; thread error→primary fallback 복구; Started 뒤 retry 허용; channel/shared 대상으로 teardown 호출 |
| E0 owner deadline·복구 철회(A,D) | 새 agent_executor/settlement.rs·store/recovery.rs, agent_executor.rs·owner_input.rs(glue), store.rs·server/mod.rs(recovery 위임), input_owner/routine.rs와 routine/settlement child, queued_head/v2/control.rs와 exact withdrawal child | 10 / +800 | Held deadline↔first-key 경합·boot/periodic recovery same ticket; exact withdrawal ACK 전 running 유지; interrupted 뒤 known-unsent late dispatch0 | run row만 terminal; ambiguous ID로 취소; first-key fence 생략; 복구 취소 ACK 전에 in-flight 해제 |
| E channel Busy 참여와 정확한 정산(D,E0,#8114) | 새 agent_executor/channel.rs·channel_settlement.rs, owner_input.rs/glue, input_owner/origin.rs·target.rs·routine.rs, provider_dispatch/owner_input.rs(glue), tui_prompt_dedupe/injected_steer.rs(기존 결과 projection hook), tui_o 표시 child | 9–11 / +950 | cold/idle/managed-active/external-active/late Busy native-end 전1, RoutineChannel만 Busy; exact frame Joined/Handed/foreign answer 구별; unknown no resend; 사용자 reset/kill0 | managed-active 등 선행 veto 하나 복원; capability를 body 문자열에서 승인; Submitted 즉시 succeeded; Unknown 재주입 |
| F command 직접 실행(A,B) | 새 command_runner.rs·command_runner/output.rs, runner_spec.rs, runtime.rs·loader.rs(위임), migrated.rs(validator 재사용), API helpers/handlers child, store runner snapshot 연결 | 8–10 / +850 | 실제 격리 child exit0+stderr/nonzero/signal/spawn/timeout; stdout/stderr 동시 대량 drain·64KiB 잘림; quickjs script agent 차단; 재시작 interrupted no respawn | command→agent fallback; stderr 있으면 실패; 출력 cap에서 drain 중단; restart 자동 spawn; script+agent 허용 |
| G 대시보드·API 문서·자동 이전(A–F) | dashboard api/routines.ts·api barrel, RoutinesPageView.tsx·widget glue·Home timeline glue, 새 ModeEditor·RunDiagnostics·ScriptOutput, router run deep-link glue, 기존 store/execution_modes.rs의 다음 claim 호환 projection, docs runtime reliability/launchd migration | 9–11 / +950 | 7개 fixture 모두 thread·기존 thread/status/due/backend 유지; paused intake 실행0; mode4선택/legacy read-only; headless/script Discord link0; output/unconfirmed 표시; 실패 deep link 실제 run | 모든 process→headless 매핑; paused를 resume; historical thread 덮기; mode 없는 run을 성공/0으로 표시 |

추정 최대인 G/E도 +950으로 PLAN 선에 250 여유를 둔다. headless 문맥 분리(D0)와 owner 복구 철회(E0)를 별도 리뷰 조각으로 떼어 새로 확인한 비용을 숨기지 않았다. #8114 typed capability가 실제 구현에서 바뀌면 E 시작 전 dependency head와 API를 대조하고 접착 변경만 산정한다. 30파일 안에 맞추려고 giant에 새 책임을 합치지 않는다. `agent_executor.rs` 2326, runtime 1144, store 4207, discord_log 2101, session_control 1157, loader 3266줄의 독립 책임 순증 목표는 0이며 child 선언·필드·호출 교체만 허용한다. API docs 파일의 실제 소유 경로는 구현 시작 시 `src/server/routes/docs/inventory/endpoints/part_06.rs:405-648`와 taxonomy이며 endpoint 추가 배선은 `routes/domains/ops.rs`다. 구현 시작 시 이 실제 경로를 목록에 고정한다.

각 조각은 운영 없이 격리 PG/API 및 fake provider/native-source fixture에서 RED→GREEN을 만든다. 뮤턴트는 생산 호출부를 끊었을 때 해당 계약이 RED인지 확인하고 원복 바이트 동일/SHA를 남긴다. 통합된 F child는 임시 경로에서 실제 프로세스로 실행한다. CI는 새 시험 id가 paths-filter→job→recipe에 선택되는지 확인한다. 전체 clippy/일괄 mutant/native prepush는 구현 묶음 끝 한 번, 조각마다 대상 시험+static-only다. 이 설계 레인은 이 시험들을 실행하지 않았으며 모델 GREEN을 Rust GREEN으로 대체하지 않는다.

## 기존 7개 무중단 이전 순서 (DoD 3)

1. A/B를 additive로 배포할 수 있는 형태로 만든다. 현재 scheduler·lease·run/turn id·thread id·next_due/status를 쓰는 코드는 유지하고, 기존 run은 legacy snapshot으로 끝낸다. mode를 바꿔 현재 실행을 다시 claim하지 않는다.
2. 다음 claim 시 기존 fresh 정의를 effective thread+당시 기존 backend로 투영한다. 정의의 null-mode provenance를 보존하고 JS action 뒤 actual surface를 run에 고정한다. 기존 일정 7개를 새 row로 복제/재등록하지 않는다. 완료한 슬롯과 missed slot 수를 변경하지 않는다. old persistent/DM는 위의 backward reader로 보존한다.
3. B/C/D0/D/G의 저장·조회·최종 결과 정책이 준비된 같은 기능 묶음에서 자동 보존 projection과 조용한 게시를 함께 활성화한다. 수동 switch/canary 명령·운영 DB 수정·강제 run-now 없이 자연 예약 실행을 관측한다. 장애가 나도 mode를 바꾸거나 같은 run을 재실행하는 자동 fallback은 두지 않는다.
4. 구현 후 관측 우선순위는 dependency-update-watcher → memento-scope-audit → memento-daily-report → memento-hygiene → family-morning-briefing-yohoejang → family-morning-briefing-obujang. 모두 thread를 유지하며 이름/시간이 빠른 루틴을 대기시키는 배포 순서가 아니다. 기존 다음 정기 슬롯대로 실행되고 희소 주간 루틴은 fixture+API compatibility로 먼저 검증한다.
5. paused memento-intake-check-20260925는 마지막에 **읽기 검증만** 한다. paused/next_due/이력 미기록 backend를 보존하며 resume나 시험 실행하지 않는다. family 둘은 iMessage 업무 효과와 Discord 침묵을 구분해 검증한다.
6. 사용자가 이후 명시 선택할 때만 개별 루틴을 headless/channel/script로 바꾼다. 이번 ‘현재와 같은 기본값’ 이전은 그 재분류를 포함하지 않는다. 단순 스크립트 신규 등록은 기존 launchd를 자동 제거하거나 병행 실행하지 않는다.

## 상태 차트와 검증 범위

루틴 관련 기존 모델은 `docs/statecharts/outbound-port/OutboundCallerFlow.tla`의 routine log edit/send 순서뿐이다. 해당 area에 `RoutineRun.tla`를 추가하여 run 수명 모델을 확장하고 기존 logger 모델의 기대를 바꾸지 않는다. 새 입력 변수 값은 위 진입점 표의 claim/action/backend/start/owner/Held/submission/completion/timeout/retry/recovery 값에서만 가져온다. 설계 mode는 이 값들의 새 라우팅 상수다. 상태/실행 카운트는 모델 관측값이며 새 운영 ledger가 아니다.

지정 CLI로 동일 최종 모델 10cfg를 실행했다. Headless/Thread/Channel은 각 49states/127steps, Script는 15/33으로 GREEN이다. BeforeRetry·BeforeHeld·ResendUnknown·NoDeadlineCancel·NoRecoveryCancel·DropHeld 6cfg는 명시한 중복/미종결/late dispatch/유실 규칙에 기대 RED다. 결과·재현 명령·생략 범위·cfg별 상태수는 [상태 모델 보고서](../statecharts/outbound-port/routine-run.report.md)에 기록한다. 모델의 GREEN은 제안한 전이의 safety/liveness이며 아직 구현되지 않은 수리를 main PASS로 표시하지 않는다. main 반례는 코드 전이별 대조만 했고 Rust/운영 재현은 이 레인의 빌드·실행 금지로 NOT_RUN이다.

## 사용자 결정 필요

1. 채널 방식에서 사용자 대화와 루틴 결과가 섞일 때의 표시를 정한다.
   - A: 본 채널 기존 답을 공유하고 ‘함께 처리: 루틴명’을 표시한다.
   - B: 같은 세션에서 일하되 별도 스레드로 루틴 결과를 복사한다(구조 결과 계약 추가 필요).
   - 추천 A: 기존 답 소유와 실제 native 처리 단위를 보존하며 중복 게시·잘못된 분리 위험이 작다.
2. 단순 스크립트의 등록 형태를 정한다.
   - A: executable·argv 배열·cwd를 루틴 폼/API에서 직접 등록한다.
   - B: 셸마다 작은 JS wrapper를 작성하여 등록한다.
   - 추천 A: launchd 대체 목적에 맞고 agent 턴 없이 임의 셸을 등록하는 흐름이 짧다.

이 두 항목은 제품 UX 선택이며 이 설계에서 권고안을 구체화했다. 코디네이터가 독립 리뷰·사용자 선택을 받은 뒤 구현 범위를 고정한다. 이 레인은 승인 대기용 운영 변경이나 수동 가드레일을 만들지 않는다.

## 리뷰 집중 항목과 남는 한계

- 경계 시각: claim snapshot/PATCH, first-key↔deadline cancel, terminal↔timeout 동시성을 같은 actor/기존 row fence로 판정한다. 단순 시각 비교로 원문을 버리지 않는지 E0/E 시험이 담당한다.
- 동시성 경합: machine 정책 선행/late/route 세 지점이 같은 capability를 소비하는지, shared answer attribution과 O lease가 하나인지 검토한다. 중복된 frame registration/본문 emitter가 생기면 차단이다.
- 옛 형식 복원: legacy process 키·missing metadata·persistent/DM·과거 thread id·in-flight mode edit를 A/D/G에서 고정한다. 현재 row를 과거 run에 투영하지 않는다. 동적 DM 구형 정의는 자동 explicit-mode 승격 없이 유지한다.
- 문서로만 선언한 가드: FinalOnly source projection과 exact-withdraw는 현 main에 없는 구현 책임이며 C/E RED·뮤턴트로 검증해야 한다. #8114 약속을 구현 완료 증거로 쓰지 않는다.
- 잘못된 0/PASS: script0·start-failure0·unknown·actual accepted1·Discord no-visible-result를 구별한다. O/transport ACK만으로 succeeded로 올리지 않는다. 이 조사에서 알림 도착·구형 paused backend는 미확인이다.
- 기존 결함 범위: 이름 기반 orphan reaper(`fresh_session_reaper.rs:25-153`)와 best-effort session ownership stamp(`launch.rs:273-285`)를 더 넓히지 않는다. channel에는 reset/kill/reap 권한을 주지 않으며 process cancel은 기존 exact turn만 사용한다.
- 임의 셸 내부 side effect/하위 daemon의 exactly-once, 전역 relay replacement-safe 내구성은 이 모델의 보장 밖이다. 사용자의 데이터/세션 삭제·kill 권한을 성공률 명목으로 추가하지 않는다.
