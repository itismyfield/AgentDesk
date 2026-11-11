# Claude Idle 입력 준비 확정 — #8080 설계 d2

구현 A source SHA: be346e01c94fa32a37d13d05d01f7d3ab656bd72. B compact pending은 미구현이며 기존 compact 동작을 유지한다.

설계 d2 시점 Prod 기준 SHA: 574af580b0c5d6e6ce280994f4ed243463de5d2d.
main: 4f30c67d9a48eacad96a99faff9fd442d0d4dd3c.
선행 #8077 head: 08d5ac88be45e5e1af7d46696c69b2ee02def1b6.
r2 문서 기준: 0b0f39c582a9073fdb20b62512bc7794a18d1281. 리뷰 P1/P2와 코디네이터의 busy 불변 결정을 반영했다.
설계 d2 레그는 문서·모델만 변경했다. 구현 A 보완은 문서 끝 절을 따른다. [이슈와 DoD](https://github.com/itismyfield/AgentDesk/issues/8080)를 gh issue view로 읽었다.
아래 코드 표에서 src/services/ 접두어는 생략한다.

## 0. 운영 전제와 상태 차트

[C] 코드 확인, [D] 공식 문서 확인, [U] 이 레그에서 확인하지 못한 운영 전제다.
실제 운영 argv/settings·DB·store·pane·Discord에는 접근하지 않았다. 코드가 설치하는 훅과 실제 운영 수신을 혼동하지 않는다.

| 운영 전제 | 코드 좌표/증거 | 판정 및 의존 정도 |
|---|---|---|
| tmux Claude launch가 공통 builder를 지난다 | claude/tui_session_launch.rs:12, :44, :69 → claude_tui/session.rs:326 | [C] prepare 후 create/respawn. 실제 운영 개별 argv는 [U]; 대상이 이 immutable launch context를 가진다는 전제에 의존 |
| Herdr도 같은 settings를 전달한다 | claude/tui_session_launch.rs:126, :147, :158; claude_tui/session.rs:343, :367, :379 | [C] fresh --session-id / resume --resume 및 --settings. 기존 nonce/launch 소유권 재사용 |
| SessionStart·Stop 실제 설치 코드 | claude_tui/hook_bundle.rs:16, :39, :147 | [C] Claude renderer에는 SessionStart matcher 제한이 없다. :201의 startup/resume/clear 목록은 Codex renderer이며 Claude compact 누락 증거가 아님 |
| relay가 payload와 launch 신원을 전달한다 | claude_tui/hook_relay.rs:95, :187, :578, :602; tui_prompt_dedupe/binding_context.rs:312, :332 | [C] stdin JSON, 원 발행 시각/요청 ID/binding header. 도착 시각만으로 turn을 식별하지 않음 |
| 훅 버퍼 수신 | claude_tui/hook_server.rs:304, :395, :459, :465, :495, :531; hook_registry.rs:276, :443 | [C] ingress 뒤 event, 계속 실행되는 Stop은 notify/registry 제외. 기존 필터는 유지하며 새 continuation fence만 제외 전에 기록; 정상 ready wake는 제외 뒤 관측 |
| transcript 경로 | claude_tui/transcript_tail.rs:23, :44, :72, :139; source_verify.rs:39, :223, :332; tui_prompt_dedupe/binding_events.rs:498 | [C] source pin/UUID/path/dev/ino 우선. cwd newest/mtime는 신원 증거 아님. 복수 후보 임의 선택 금지 |
| transcript Idle 생산 | claude_tui/transcript_tail.rs:394 → tui_turn_state.rs:339 → tui_turn_state/claude_meta.rs:6, :17; tui_turn_state.rs:574 | [C] 빈/init-only도 Idle일 수 있고 malformed를 넘길 수 있어 준비 확정에는 추가 경계 검증 필요 |
| 운영 Claude Code 2.1.322 | 코디네이터 brief. r1에서 확인한 로컬 claude --version은 2.1.316 | [U] 운영 바이너리/이벤트를 실측하지 않음. 로컬 version을 운영 증거로 대체하지 않음 |
| source 값 | [Anthropic hooks reference](https://code.claude.com/docs/en/hooks#sessionstart) | [D] startup/resume/clear/compact 구별. 현재 문서에는 fork도 존재; unknown/fork를 startup으로 처리하지 않음. 2.1.322 payload는 [U] |
| SessionStart와 composer 순서 | source_verify.rs:376은 startup/clear background start의 늦은 회귀를 거절. 공식 문서는 interactive startup/launch resume/clear의 훅 실행 중 입력을 받고 응답은 훅 종료를 기다린다고 설명 | [D/U] 화면 marker보다 먼저 Start를 쓸 근거. 정확히 2.1.322에서 조기 paste+Enter 한 번이 수용되는지는 구현 격리 fixture로 확인해야 함. settle 시간은 증명이 아님 |

공식 문서는 버전 고정 자료가 아니다. 첫 부팅 훅 영구 유실+빈 transcript에서 화면 marker로 탈출하는 fallback은 만들지 않는다.
Start 수신 시점에 input transport가 아직 수용하지 않는다는 2.1.322 반례가 나오면 첫 입력 설계를 다시 검토한다.

### 보존 계약·이미 충족된 부분

main SHA 4f30c67d9a48eacad96a99faff9fd442d0d4dd3c의 claude_tui/hook_bundle.rs:39,
session.rs:338, :367, transcript_tail.rs:394는 이미 hook 설치·mapping·observer를 제공한다.
main claude_tui/owner_input.rs:75, :88은 first-key 권한/지속 경계이고
discord/queue_io/input_owner/held_watch.rs:82는 Held 재관측이다. 새 큐/receipt/실행 소유자로 재구현하지 않는다.
원시 main 좌표 증거는 logs/main-evidence.txt다.

#8077의 exact pane close, 옛 pane fallback 0, typed Held → actual NotSentProof → hold_not_sent → wake,
MayHaveSent/Unknown 정산을 유지한다. 현재 좌표는 claude/herdr_turn/first_input.rs:97, :117,
discord/queue_io/input_owner/effect.rs:122, :131, turn_orchestrator/queued_head/v2/retained.rs:142다.
새 Herdr 첫 입력은 기존 composer draft admission을 재사용하고 준비 증거만 바꾼다.
proof 발급 권한/자료형, close, crash 정산 계약 변경이 필요하면 DESIGN_DELTA로 중단한다.

### 기존 모델 갱신을 먼저 수행

README 색인의 input-single-path, session-lifecycle를 읽었다.
InputPath.Prepare의 read/unread, HerdrBusyHandoff.Reserve/WakeFirst의 Blocked는 준비·draft·관측 성공을 접은 값이다.
TmuxSession.TurnEnds도 완료/wake를 추상화한다. 이 추상 가정을 구현 증거로 간주하지 않는다.

InputPath.tla에 ReadinessSpec을 추가하고 ClaudeReadiness.tla를 instance했다. 이전 custody/boot/stop spec에서는
새 record를 고정하고 새 spec에서는 이전 상태를 고정한다. 과거 RED를 새 기능으로 지우지 않는다.
HerdrBusyHandoff의 guard를 ComposerAdmitted(i)로 명시하고 세부 readiness refinement에 연결했다.
close/Held/Unknown 전이와 기존 guard의 참/거짓은 동일하다.

INVARIANT는 입력 실행/주입 각각≤1, done이면 실행=1, **Idle 준비 경로에서 Running 관측 중 키0**,
사람 초안 실행0, 첫 입력 조기 실행0, compact는 승인된 terminal 이후 Idle만 허용이다.
busy 입력은 기존 busy 경로로 Claude 자체 대기열에 들어가며 Running 중 paste+Enter를 금지하지 않는다.
모델은 BusyInject/BusyConsume과 Idle Submit을 분리하고 BusyEventuallyQueues로 busy 행동의 실제 도달을 검사한다.
PROPERTY는 취소/영구 인프라 장애 없는 원본의 eventual effect다.
source epoch/turn floor/Start/Stop/주기 read/draft/최종 재검증/receipt/N=2를 분리했다.
fairness는 실행 성공을 가정하지 않고 SessionStart·현재 terminal 관측, 계속 가능한 제출/receipt,
일시 draft read 복구·사람의 draft 비우기를 각 WF로 둔다.
PermanentUnread는 복구 행동을 꺼 진행성 RED를 남긴다. unread를 Empty로 바꾸는 가정은 없다.

정확히 한 번의 범위: 취소 없는 crash-free 수용 입력은 한 번 실행한다. pre-key restart는 같은 원본을 보존한다.
post-key crash/응답 유실은 #8077의 Unknown retention을 유지한다. 그 경우 실행 성공의 unconditional liveness를
주장하지 않는다. readiness 모델의 restart도 pre-key 경계이며 기존 모델이 post-key custody를 계속 검사한다.

## 1. 현황·진입점 전수표

R=준비 증거, D=draft/사용자 입력 focus veto, W=관측/wake, K=first-key/receipt, X=source/초기화.
이슈의 1fe3a57d 좌표가 아니라 현재 head 좌표다.

| file:line | 현재 판정·46열/resize 실패 | 제안 모델/행동 |
|---|---|---|
| claude_tui/input.rs:315 | snapshot이 marker/draft/alive/capture/tail을 합침 | R을 별도 evidence로 분리; snapshot은 D/진단 |
| input.rs:613, :621, :643 | fresh/followup 공통 wait | 공통 current R resolver/wait |
| input.rs:660 | cold MCP 배너를 재캡처해 영구형 오류 | 배너를 R/재생성 근거에서 제거. 실제 input focus 안전 veto 유지 |
| input.rs:685 | buffered Stop 뒤 marker가 있어야 ready | W 뒤 현재 R 재관측 |
| input.rs:709 | Idle도 pane busy/draft/auth가 거절 | transcript R과 D 분리; screen busy/auth는 Idle을 뒤집지 못함 |
| input.rs:744, :863 | global Notify 전후 snapshot, 5/10초 예산 | scoped subscribe-before-observe; global Notify는 힌트 |
| input.rs:772, :790 | event 뒤 marker 또는 Idle+screen | 현재 source R만 준비 확정 |
| input.rs:990, :1006, :1028, :1073 | polling도 marker/MCP/busy를 요구 | bounded transcript read. 좁은 pane를 준비 timeout 이유로 만들지 않음 |
| input.rs:1034 | startup dialog 자동 응답 | 기존 trust/focus 보호만; 새 auto-trust/ready 근거 아님 |
| input.rs:1086, :1149 | timeout과 stranded draft 정리 | pre-key 대기는 Held/wake. 정리는 기존 AgentDesk 귀속 proof만 |
| input.rs:1232, :1238, :1248, :1266 | marker/final_prompt_ready/foreground busy/MCP | 일반 입력 R 호출 제거. 남는 reverse/control/presence는 아래 예외표 |
| input.rs:1414, :1456, :1499 | recorded-turn이 없으면 marker 대기 | empty/init-only는 current Start, 완료 턴은 strict current Idle |
| input/submission.rs:17, :27, :137, :153 | proven-warm은 wait 생략, composer/fence | Idle 전용 submission kind만 최종 R 재검증. Busy leaf에 R을 끼우지 않음; 기존 composer/fence/one Enter |
| hosting/warm_followup.rs:238, :248, :303 | reuse recheck/wait/post-wait screen | Idle 재사용 확인만 R 공유. 기존 Busy selection/leaf를 readiness wait로 치환하지 않음 |
| hosting/followup_support.rs:152, :164 | Idle 통과 뒤 draft snapshot veto | 공통 R + 기존 D |
| followup_support.rs:228 | stranded draft를 transcript+screen으로 판정 | D/귀속 정리 정책이며 ready로 승격하지 않음 |
| followup_support.rs:263 | zero-advance 뒤 input buffer에 prompt가 남았는지 화면 진단 | D/own-draft 진단. 성공 proof/자동 replay 권한 아님 |
| followup_support.rs:352; hosting/host_draft.rs:209 | Unknown에서 Baked/Tools 문구로 draft recreate 후보 | 보류 정책5의 실제 caller. 삭제/권한 확대하지 않음 |
| claude_tui/host_input.rs:704, :737 | recovery classify가 marker+busy/auth | Claude consumer에 R+D 전달. liveness/identity refusal 유지 |
| claude/herdr_turn/composer.rs:50, :73, :125 | first input도 final_prompt_ready | #8077 D 재사용, first variant만 R 교체. exit variant 별도 유지 |
| herdr_turn/first_input.rs:97, :117, :122 | run과 Held probe 둘 다 화면 wait | K/custody 동일, run/probe가 같은 R+D를 관측 |
| discord/recovery_engine/herdr_row_end.rs:450, :557, :576, :582, :608 | exact exec→screen classify→closed_turn_read. marker 실패면 닫힌 턴도 보류 | current source R+own closed_turn_read, D 별도. Codex 불변 |
| discord/router/message_handler/intake_turn.rs:1618, :1625, :1637, :1676 | Idle 준비 wait 성공 뒤 diagnostic이 다시 busy 거절 가능 | 이 Idle 쪽 post-wait만 동일 source generation R로 교체 |
| router/message_handler/tui_followup.rs:384, :433, :442, :447 | 파일/경로 생성 전 ClaudePromptMarkerOnly | PendingSource/Start 상태로 교체, screen fallback 제거 |
| tui_followup.rs:534, :558, :585 | Idle 우선, UserSubmitted+mtime+marker stop 예외, Unknown 화면 | 기존 Busy selection/stop 예외는 #8081로 유지. Idle 준비 증명만 R로 교체하며 mtime+marker는 IdleReadyProof가 아님 |
| provider_dispatch/owner_input.rs:193, :211, :232, :309 | active person은 hand_busy 주입, synthetic은 기존 큐 대기 | **불변(busy 주입 유지)**. #8077 forward preflight 순서 및 busy/idle 소유권 분기를 바꾸지 않음 |
| discord/router/message_handler/intake_turn.rs:1558 → :1763 into_busy_pane | native Running이면 readiness wait를 건너뛰고 busy queue | **불변(busy 주입 유지)**. 새 Idle R wait나 hold_front를 이 분기 앞에 넣지 않음 |
| provider_dispatch/owner_input.rs:1065 → :1076 Registration::observed → input_owner/effect.rs:212 | native Running 발견 뒤 Busy leaf → busy_inject::inject_owned | **불변(busy 주입 유지)**. stash.rs:62/:97/:103의 gate/paste/Enter, existing receipt/observer 불변 |
| provider_dispatch/owner_input_busy_gate_tests.rs:7, :65 | Running 입력의 native root와 paste,Enter를 기대 | 기존 기대 유지. 이 설계에서 제품 시험을 실행했다고 주장하지 않음 |
| tui_input/actor/gate.rs:31, :38, :61, :74; actor/pane.rs:216 | generic gate도 marker/busy/MCP | Claude **Idle prompt branch만** R proof; Busy context/명시 UI control/Codex 분기는 기존 동작 |
| tui_input/submission.rs:59, :77, :80 | capture+submission_admits가 독립 ready 권한 | Idle caller의 proof 없는 PASS만 제거. SubmitContext::Busy/Busy leaf는 R 요구 없이 기존 판정·키열 유지 |
| input.rs:352, :372, :382, :410, :452 | busy compact literal/Enter 및 화면 확인 | pending compact를 terminal 이후 Idle에만 한 번. D 유지 |
| claude_compact_trigger.rs:278, :430, :440, :447 | usage crossing 직후 busy worker | crossing은 pending만, 이후 source/latch/turn 재검증 |
| tmux_common.rs:323, :434 | marker와 그 함수를 쓰는 actively_streaming; wrapped chrome false busy | 일반 input R 용도 제거. 남는 production caller 때문에 전역 삭제 금지 |

input.rs 및 input/hosting 경로의 생략된 부모는 claude_tui/다.
ready_for_input, final_prompt_ready, wait_for_prompt_ready, prompt_marker_confirms_prompt_ready, idle_read, /compact를
git grep/rg로 검색했다. logs/all-entry-symbols.txt와 readiness-search.txt가 원시 검색이다.
embedded cfg(test)와 타입/주석만 있는 행은 새 production 진입점으로 세지 않았다.

### 신호 생산·소비·초기화

| file:line | 역할 | 모델 연결 |
|---|---|---|
| hook_bundle.rs:39, :147; session.rs:338, :343, :379 | 설치/mapping/launch | X: launch incarnation별 관측 상태 생성 |
| hook_relay.rs:187, :602 | payload/envelope 전달 | W/R: UUID/path/source/원 context 보존 |
| hook_server/observation_ingress.rs:196, :205, :229 | launch/resume 기록·source adoption | X: accepted current source만 연결, Pending≠Ready |
| source_verify.rs:332, :353, :376; binding_events.rs:498 | source/history fence | X: 회귀/뒤늦은 Start/file identity 검사 재사용 |
| hook_server.rs:420, :423, :426, :459, :465, :531 | Stop/PromptSubmit/Start, suppress/registry | :459에서 continuation disposition을 atomic note한 뒤 기존 notify/registry 필터 유지. Suppressed Stop은 ready wake로 재발행하지 않음 |
| hook_registry.rs:276, :383, :403, :443 | TTL/claim/drop | W: 단회 소비 유지. TTL만으로 turn 신선도를 증명하지 않음 |
| transcript_tail.rs:394; tui_turn_state/claude_meta.rs:6, :26 | 상태 관측 | R: Idle에 current identity/floor/torn-tail guard 추가; 공유 observer 의미 불변 |
| input.rs:144, :685, :777 | Stop 소비 | W: 소비는 ready 권한 소비가 아님. 다른 waiter가 먼저 가져가도 timer read |
| tui_prompt_dedupe/runtime_binding/binding_access.rs:97, :150; runtime_binding.rs:162 | UUID mapping/current binding | X: logical 이름만 같은 proof 재사용 금지 |
| runtime_binding/pane_registration.rs:52, :88, :340 | launch/restore/incarnation 교체 | X: 새 key면 ready/compact 폐기 |
| tui_prompt_dedupe/runtime_binding.rs:927, :946 | clear/evict | X: 해당 key readiness invalidate. 옛 watch가 새 generation을 지우지 못함 |
| claude_tui/session.rs:77, :203 | continuation UUID/launch/settings 수리 | X: 새 native source를 다시 선택, 옛 Start/Stop 재사용 금지 |
| submission_fence.rs:20, :79; owner_input.rs:88, :100, :114 | writer/payload 경계 | K: R 재검증 뒤 기존 first-key 순서; Stop winner 키0 |
| discord/queue_io/input_owner/effect.rs:122; held_watch.rs:62, :82, :145 | 무전송 proof 정산/same-front watch | W: proof 안에 공통 R+D probe; 읽기 실패 Keep, 동일 원본 wake |
| turn_orchestrator/pending_queue_persistence/queued_recovery.rs:88; input_owner/recovery.rs:8 | boot census/owner 복구 | X/W: durable 원본 유지, memory ready bit 복원 금지 |

hook_*, source_verify, transcript_tail, submission_fence, 이 표의 owner_input은 claude_tui/ 접두어다.
binding_events는 tui_prompt_dedupe/다. 같은 이름의 Discord owner와 구분한다.

### 인접 production caller: 이번에 삭제하지 않는 화면 판독

| 좌표 | 경계 |
|---|---|
| provider.rs:628, :784, :804; pane_readiness.rs | 일반/legacy wrapper fallback. ClaudeTui scrape 금지 계약은 이미 있음 |
| claude/host_gate.rs:151; discord/tmux_watcher/terminal_readiness.rs:586; turn_stream_collector/idle_read.rs:39 | watcher/completion/idle cleanup는 #8081. 새 input proof가 cleanup 권한이 되지 않음 |
| discord/turn_presence/activity.rs:1039 | presence final_prompt_ready는 #8081 |
| discord/turn_bridge/tmux_runtime/claude_stop_pane.rs:58, claude_stop_delivery.rs:712 | /stop 판정은 #8081 |
| claude/herdr_turn/composer.rs:95, :111; claude/herdr_return.rs:178; claude/session_handoff.rs:122 | reverse /exit의 기존 composer/stash 보호 유지. forward exact close를 되돌리지 않음 |
| discord/commands/tui_passthrough/herdr_settings.rs:103, :110 | 사용자가 요청한 model/fast/effort UI. 선택/focus 확인은 필요하지만 일반 prompt readiness fallback으로 사용하지 않음 |
| claude_tui/hosting/host_draft.rs:209; followup_support.rs:352 | 보류 정책5의 기존 own-draft 복구 후보 판정 |

“화면은 draft veto만”은 일반 Claude prompt 준비 확정에 적용한다. 제품 전체 screen reader 0을 주장하지 않는다.

## 2. 신호 설계

새 claude_tui/readiness 모듈이 독립 책임을 가진다. observe(context)→Waiting/Ready(proof), wait(context,cancel),
revalidate_before_key(proof), probe(context)를 제공하며 큐/receipt 소유권을 만들지 않는다.
key는 Claude/channel/logical owner/실 host target/PaneEpoch/launch incarnation 또는 Herdr nonce/
native UUID/검증된 path+dev+ino/source generation이다. proof에는 turn generation 및 terminal offset 또는 Start boundary가 있다.
Ready는 전달 성공이 아니다. proof를 serialize하거나 다른 pane/원본/재시작에 ready=true로 복원하지 않는다.
이 API의 권한은 Idle 주입 전용이다. active/native Running으로 이미 분류한 Busy leaf는 이 API를 호출하거나 기다리지 않는다.

결정 규칙:

1. target/source 미검증·변경은 Waiting(SourcePending/Changed). 실제 dead는 기존 lifecycle 소유자에 반환하며 새 kill/cleanup 권한 없음.
2. **Idle 준비 경로**에서는 local reservation/UserPromptSubmit 이후 종료 경계가 없거나 Streaming/UserSubmitted면 Waiting(Running).
   옛/중복 Stop 또는 Start는 이를 해제하지 못한다. 이것은 Busy 경로를 hold로 바꾸라는 지시가 아니다.
   새 ingress가 active/native Running으로 분류되면 기존 Busy 주입/Claude 자체 대기열을 그대로 사용한다.
3. 완료 턴의 current transcript Idle + 마지막 제출 floor 이후 **아래 whitelist로 승인한** terminal이면 Ready(TurnIdle).
   전후 file identity/length와 complete tail을 확인한다. malformed를 넘겨 과거 Idle로 돌아가면 입력 전용 wrapper가 Unknown으로 낮춘다.
   공유 observe_transcript_turn_state의 의미는 #8081/#8082에 영향을 주지 않게 유지한다.
4. fresh/clear/resume 직후에는 같은 incarnation의 accepted SessionStart(startup/resume/clear)가 첫 입력 후보를 만든다.
   init-only/empty만으로 허용하지 않는다. 파일이 아직 없으면 immutable launch와 anticipated UUID/path까지 일치한 Start만 후보며
   transport 조기 수용은 [U] 시험을 요구한다.
5. SessionStart(compact)는 진행 중 housekeeping으로, 첫 입력 gate를 열지 않는다.
   fork/unknown도 startup으로 바꾸지 않고 검증된 current Idle을 기다린다.
6. R 뒤 기존 D를 관측하고 mutation 직전 동일 composer lock 안에서 source/turn/proof/cancel을 다시 확인한다.
   stale proof는 키0의 Held. 기존 attempt/writer/payload mark/receipt가 정산을 소유한다.

Stop 유실·지연 대체는 **주기적 current transcript 재관측 한 가지**로 통일한다. 공유 Idle만이 아니라 terminal whitelist와 continuation fence도 매번 평가한다.
Stop/Start/파일 변경은 즉시 wake 힌트, mtime/size는 재파싱 최적화 힌트다.
mtime 정지/시간 경과를 Idle로 사용하지 않는다. 대기 중 최대 1초마다 같은 mtime라도 bounded read한다.
root 아닌 SubagentStop·다른 세션/global Notify는 현재 R 재관측 외 효과가 없다.
승인 가능한 hard terminal이 쓰였다면 webhook이 없어도 진행한다. summary만 남고 webhook도 없으면 quiescence로 승격하지 않는다.

subscribe-before-observe → buffered event 확인 → current transcript/Start snapshot → event 또는 bounded timer 순서다.
대기 중 composer/registry/global mutex는 잡지 않는다. TTL 만료·다른 /tui/wait의 claim이 영구 대기를 만들지 않는다.
첫 부팅 Start 자체가 영구 유실되면 terminal fallback도 없으므로 Waiting이며 기존 ordered relay의 재시도/복구를 쓴다.
daemon만 재시작하고 provider는 살아 있으면 새 Start를 기다리지 않고 검증된 current source의 완료 턴에서 R을 재구성한다.

### Terminal whitelist와 continuation/suppression 최소 계약 (P2)

공유 tui_turn_state.rs:612는 system/stop_hook_summary·turn_duration·init를 모두 Idle로 분류한다.
반면 discord/tmux_output_stream.rs:610-626은 stop_hook_summary를 SoftStopHookSummary candidate로만 내고
:918-940 시험은 그 뒤 late assistant text를 같은 응답에 합친다. 이 두 계약은 다르다.
readiness 전용 wrapper는 공유 observer/stream parser를 바꾸지 않고 아래 추가 조건을 적용한다.

구현 A 추가 진입점: hook_server.rs의 output guard Hold/Blocked 조기 응답도 root Stop이면 공통 continuation 기록 함수를 호출한다. SubagentStop은 parent latch를 변경하지 않는다.

| transcript 종류 | Idle readiness 승인 |
|---|---|
| complete type=result | current source+turn/floor 이후의 hard terminal. continuation barrier의 아래 해제 조건도 만족해야 함 |
| system/turn_duration | 입력 전용 hard terminal 후보로 승인하되 current turn/floor·완전한 tail·continuation barrier 검사를 동일 적용. 2.1.322에서 continuation 이전에도 나오는 순서는 [U]이며 barrier가 선행 종료 승격을 막음 |
| canonical user interrupt marker | tui_turn_state.rs:586이 인정하는 기존 interrupt envelope만 current turn에 한정해 완료 후보. 새 /stop 판정 권한 없음. 미해제 continuation barrier 우회 금지 |
| system/stop_hook_summary | **단독 승인 금지**. 같은 root/source/turn/floor의 Stop boundary와 로컬 비-continuing disposition이 짝지어진 confirmed summary만 후보. summary·mtime 정지·debounce만으로 승인하지 않음 |
| system/init, empty, assistant/end_turn만, housekeeping, unknown/malformed/partial | 완료 terminal 아님. first-turn SessionStart 계약만 별도 적용 |

source-scoped ledger는 root Stop 후보와 continuation disposition을 한 atomic revision으로 기록한다.
최소 key는 기존 source/epoch/native UUID/turn floor이며 데이터는 stop boundary(seq/offset/assistant identity),
continuation 상태(None/AwaitResume/AwaitFinal/UnknownAfterRestore), suppression floor와 마지막 continuation activity다.
이 ledger는 기존 hook registry의 ready 이벤트를 다시 발행하지 않는다.

1. hook_server.rs:420의 advance_stop_flush 결과가 :459에서 flush를 주는 경우,
   :465 notify·:531 registry·response를 내기 **전에** 같은 source ledger의 AwaitResume을 arm하고
   cached Idle proof의 revision을 무효화한다. suppressed Stop은 후보로만 남고 final disposition이 아니다.
   memento_feedback.rs:167의 reentrant stop_hook_active는 default flush 없음과 final Stop을 혼동하지 않고
   기존 latch를 유지한다.
2. 모든 observe/cache-hit/timer read/최종 first-key 검사는 동일 revision과 latch를 읽는다.
   AwaitResume 중 summary 또는 그 Stop에 딸린 turn_duration/이전 result는 Ready/compact를 만들지 못한다.
   이후 complete native user/assistant/tool activity가 suppression floor를 넘는 것을 읽으면 AwaitFinal로 전이한다.
   late assistant도 이 activity다. D guard와 기존 first-key fence는 계속 필요하다.
3. AwaitFinal에서 마지막 activity **이후**의 whitelist hard terminal, 또는 이후 seq의 matching root Stop가
   비-continuing임이 확인되고 summary까지 짝지어진 경계만 해제한다.
   새 Stop도 feedback를 주면 다시 arm한다. summary/TTL/quiet mtime/global Notify/SubagentStop/old Stop는 해제 불가.
   중복·역순 이벤트가 더 최신 AwaitResume/AwaitFinal을 지우지 못하게 generation/producer sequence를 비교한다.
   SessionStart(compact)도 같은 source의 latch를 지우지 않는다.
4. daemon restore에서 latch 이력을 증명하지 못하면 UnknownAfterRestore로 보수 복원한다.
   이전 summary/캐시/TTL로 None을 만들지 않는다. 복구 floor 뒤 실제 activity+hard terminal 또는
   검증된 새 non-continuing root Stop로 재구성한다. clear/new incarnation은 옛 key 전체를 무효화하며
   새 source는 자기 SessionStart/terminal만 소비한다. 새 영속 queue/Unknown 정산 형식은 추가하지 않는다.
   suppression 이력까지 잃고 실제 종료 뒤 새 증거도 전혀 없는 restore 조합은 진행성 미증명 경계다.
   독립 Restart cfg의 GREEN을 그 복합 상황의 증거로 세지 않고 구현 격리 재시작 시험/설계 리뷰에 남긴다.
5. Stop 유실 fallback은 whitelist hard terminal 재관측이다. summary-only+hook 유실에서는 Ready를 추측하지 않는다.
   2.1.322의 normal/suppressed/reentrant Stop→summary→late assistant→result/turn_duration 순서,
   root Stop의 로컬 disposition 확정과 실제 입력 수용 시각은 [U]로 격리 실측한다.
   다른 훅이 로컬 non-continuing 판단 뒤 계속 실행시키는 반례가 있으면 confirmed-summary 승격을
   활성화하지 않고 hard terminal만 승인하도록 구현 설계를 재검토한다. 시간 경과를 증명으로 대체하지 않는다.

이미 Idle waiter로 선택된 원본은 이 barrier를 Busy 재분류로 우회하지 않는다.
새 ingress가 실제 active/native Running으로 선택하는 기존 Busy 주입은 독립 경로이며 그대로다.
모델은 suppressed Stop→summary→timer→late assistant→hard Finish 순서를 별도 단계로 강제하고
latch만 제거한 mutant가 summary timer에서 조기 Idle Submit/Compact를 허용하는지 검사한다.
이것은 제품 실측이나 모든 다른 훅의 disposition을 검증했다는 주장이 아니다.

## 3. /compact

usage crossing은 기존 generation/window/model/source identity를 담은 pending 요청만 만든다.
Stop 또는 동일 턴의 transcript terminal(Stop-equivalent) 뒤 Idle에서 lock 획득 후 source/turn/latch/occupancy를 재검증하고 한 번 주입한다.
Start만 있는 Idle, compact Start, Running에는 보내지 않는다. front가 mutation을 예약했으면 양보하고 다음 Idle에서 재검사한다.
사람 초안/보호 focus/read 실패는 pre-key 보류로 pending을 유지한다. source 교체/clear/terminate는 해당 generation만 폐기한다.
usage drop/rearm 의미는 유지한다.

현재 submit_under_composer_lock은 active managed turn identity를 요구하므로 단순히 “나중에 worker”로 옮기면
완료 뒤 전부 stale가 되는 버그가 난다. current source+마지막 완료 턴 proof로 바꾸고 successor turn이 시작되면 보류한다.
첫 mutation 이후 불명확한 요청은 자동 재주입하지 않는다. 화면 queued/empty hint로 성공을 선언하지 않고
native compact/occupancy 하락을 관측하거나 ambiguity를 유지한다. 새로운 prompt replay 계약은 없다.

## 4. 사람 초안·출처·인접 경로

기존 composer_owner/draft guard/stash/own-draft 귀속 정책을 유지한다.
Herdr 새 첫 입력의 Person/AgentDesk draft 거절, managed idle의 이미 허용된 stash를 통한 초안 보존,
귀속되지 않은 text 정리 금지, modal/trust 등 사용자 focus 보호를 바꾸지 않는다.
MCP 배너/스피너/marker는 R의 true/false 근거가 아니며, 안전 focus veto를 지워 dialog에 Enter를 보내지도 않는다.

좁은 footer/줄바꿈을 draft present로 영구 기억하지 않는다. D는 composer box/ANSI만 읽고 준비용 busy/auth 판독과 분리한다.
busy_inject/screen.rs:100, :134의 10자 이상 같은 box border는 46열 자체를 금지하지 않는다.
footer가 잘려도 R이 성립하는 fixture와 actual box가 일시 안 읽히는 fixture를 따로 둔다.
후자는 키0 typed Held → 같은 원본 자동 watch → 다음 성공 관측의 wake이며 오류 횟수로 permanent reject/수동 재전송을 요구하지 않는다.

영구 blind에서는 “보이지 않는 사람 초안 없음”을 증명할 수 없다.
fail-open/임의 C-s/Escape/Ctrl-U/resize/recreate로 진행시키면 정책5와 #8077 보호가 바뀌므로 하지 않는다.
PermanentUnread의 진행성은 RED다. DoD4의 GREEN은 transient failure→자동 복구이며 영구 통신 장애에서도 실행한다는 약속이 아니다.
영구 blind까지 GREEN을 요구하는 해석은 남은 반례다. 이를 해결했다고 보고할 수 없다.

| 입력 출처 | 현재 분류·소유권 좌표 | 처리 |
|---|---|---|
| Discord 사람 | input_owner/origin.rs:24, :30; provider_dispatch/owner_input.rs:292 | 기존 original/ticket 유지. Idle 선택이면 R, Busy 선택이면 기존 native queue로 한 번 |
| iMessage 사람 | origin.rs:44; owner_input.rs:398 | GUID 원본과 기존 Idle/Busy 정책 유지, 같은 body의 다른 source와 혼동 금지 |
| bot Discord | 기존 intake filter와 admission context | body/이름으로 사람 추정 금지, 기존 admission/ignore 보존 |
| HTTP/routine/scheduled | origin.rs:38; input_owner/start.rs:56; owner_input.rs:213 | 지금처럼 자기 turn까지 큐 대기 |
| AgentDesk compact | claude_compact_trigger.rs:430 | user 원본으로 위장하지 않는 generation-bound control |
| terminal 직접 입력/초안 | composer_lock.rs:144; UserPromptSubmit/current transcript | 초안 보존, 관측된 native start는 proof 무효화 |
| Process/remote/Codex | origin.rs:81 | placement/소유권 그대로, 이 ledger 대상 아님 |

현재 main person 입력은 active hand_busy(:232), intake_turn.rs:1558→:1763 into_busy_pane,
owner_input.rs:1065→effect.rs:212의 native Running 경로로도 주입된다.
세 경로는 **불변**이다. r1의 active branch→hold_front+watch 제안은 코디네이터 결정에 따라 철회했다.
#8077 forward preflight(:195), busy first-key/receipt/observer/기존 draft 처리, synthetic의 자기 turn 대기도 바꾸지 않는다.
scope는 Idle 쪽 준비 확정과 compact 시점이다. busy mutation을 readiness 변화의 적용 지점으로 오해하지 않도록
Idle proof와 Busy context를 타입/행동으로 구별한다. readiness waiter가 suppression fence를 우회하려고
같은 원본을 Busy로 재분류하지 않는다. 새로 도착한 실제 Busy 입력의 기존 선택은 그대로다.

| 재시작·부팅·인접 경로 | 상태 소비·초기화 |
|---|---|
| fresh/resume: session.rs:326; tui_session_launch/prepared.rs:18 | 실행 전 key, 옛 ready/compact 폐기. 같은 UUID resume도 이전 Start 재사용 금지 |
| daemon boot: runtime_bootstrap/recovery_flush.rs:48; queued_recovery.rs:88; input_owner/recovery.rs:8 | durable census/wake 유지, memory ready 재구성. Unknown 자동 replay 금지 |
| incarnation: pane_registration.rs:340; tui_input/target_epoch.rs:19 | old proof/watch가 새 target에 권한을 주지 못함 |
| native clear: hook_server.rs:426; source_verify.rs:376; session_rotation.rs:233 | old Stop/Idle 폐기, 새 source의 clear Start |
| 앱 clear: commands/control.rs:490; control/clear_teardown.rs:26, :61; v2/control.rs:284 | 기존 durable boundary/큐 취소/teardown 순서 유지, 해당 key invalidate |
| Cancel: v2/control.rs:97, :210; commands/control/cancel_queued.rs:6 | 원본 취소 후 same-front watch 종료. proof가 취소를 이기지 못함 |
| Terminate/host end: control/clear_teardown.rs:146; host_input.rs:712 | 기존 소유자만 end/정리. readiness는 관측만 |
| stop: commands/stop.rs:84, :98; claude_stop_delivery.rs:99; claude_tui/owner_input.rs:121 | 기존 NoKeyYet/취소 유지, 새로운 screenshot stop 판정 없음(#8081) |
| compact: claude_compact_trigger.rs:447; session.rs:337 | pending/ambiguous latch는 source와 초기화, compact Start로 user ready 금지 |
| 두 원본: input_owner/effect.rs:122; provider_dispatch/owner_input.rs:356 | owner/FIFO/single-flight/receipt 유지. Idle proof floor는 갱신하되 Busy 큐 입력을 막지 않음 |

main tui_followup.rs:558의 stop 후 bare UserSubmitted 예외는 mtime+marker를 사용한다.
이 stop/Busy 선택 보정 자체는 #8081 범위로 유지하며 이번 변경으로 Busy 분기를 새로 차단/우회하지 않는다.
다만 그 결과만으로 IdleReadyProof를 발급하지 않는다. Idle 경로로 선택되어도 terminal/Stop 근거가 끝내 없으면
새 R이 그것을 증명하지 못하는 관측 공백은 남는다. 새 stop/cleanup 권한으로 넓혀 해결하지 않는다.

외부 사람이 최종 재검증 뒤 첫 키 사이에 직접 시작하고 훅도 아직 전달되지 않은 창은 local composer lock으로 원자화하지 못한다
(input/submission.rs:137은 native 키를 잠그지 못함).
모델은 Idle 준비의 관측된 native start 경합과 별도 BusyInject→native-queued→BusyAbsorb(현재 턴) 또는 BusyConsume(다음 턴) 행동을 검사한다.
unreported native-start 창까지 막았다는 GREEN은 주장하지 않고 기존 한계로 남긴다.

## 5. 삭제 목록

production caller 0이 된 것만 같은 구현 조각에서 삭제하며 테스트만을 위해 휴면 함수/allow/baseline을 남기지 않는다.

| 후보 | 삭제·유지 |
|---|---|
| input.rs의 HookFastPathOutcome, run_prompt_ready_fast_path, wait_for_prompt_ready_event, event budget/post-event settle, pane-scrape wait 본체 | evidence wait 대체 후 삭제. 필요한 공개 wrapper만 얇게 위임 |
| transcript_has_recorded_turn/transcript_idle_* 및 screen-confirm 전용 timeout/auth logger | strict R wrapper로 대체된 중복 삭제 |
| send_compact_while_busy, compact_steering_decision, compact_pre_enter_modal_guard, compact_queued_message_hint, 전용 settle | idle-only compact 뒤 삭제; 기존 D/focus는 재사용 |
| ClaudePromptMarkerOnly/ClaudePromptMarkerOrIdleTranscript Claude 분기 | PendingSource/current R로 대체 후 삭제 |
| final_prompt_ready/prompt_marker_confirms_prompt_ready/tmux_capture_indicates_claude_tui_ready_for_input 전체 | 삭제 불가: reverse/settings/presence/stop/legacy production caller가 남음. 일반 input 호출만 제거 |
| snapshot/composer_owner/draft_sighting/startup/modal/stranded own-draft·정리 | 기존 D/정책5 caller가 있어 유지. R용 busy/auth 사용만 제거 |
| is_mcp_auth_required_error 및 claude.rs:1941 분기 | 마지막 screen 오류 생산자를 없앤 뒤 함께 삭제할 후보. 다른 producer가 있으면 남는 사유 기록 |
| followup_support.rs:352의 Unknown draft recreate | 정책5 실제 caller이므로 유지 |

## 6. 크기 추정·분할

증분 기준은 현재 prod head다. **상한 추정 prod 26파일 / 순증 +1080 코드 줄**.
새 책임은 약 150~230줄의 작은 모듈 4개로 두고 giant에는 연결/기존 책임 제거만 둔다.
현재 input.rs 3176, host_input.rs 1408, hook_server.rs 1354, claude_compact_trigger.rs 1123,
provider_dispatch/owner_input.rs 1583줄이다. 새 ledger/wait/scheduler를 여기에 얹지 않아 giant 예외는 필요하지 않다.

| prod 파일 그룹 | 역할/추가 코드 상한 |
|---|---|
| 새 claude_tui/readiness.rs, readiness/evidence.rs, readiness/wait.rs, readiness/compact.rs (4) | 값/strict terminal+latch/read/wait/pending compact ≤850. r1의 보수 예산 안에 latch 포함 |
| claude_tui.rs, hook_server.rs, session.rs, tui_prompt_dedupe/runtime_binding.rs (4) | ingress/launch/clear 연결 ≤45 |
| input.rs, input/submission.rs, input/preparation_hold.rs (3) | screen wait 삭제/최종 proof ≤55 추가 |
| hosting/followup_support.rs, hosting/warm_followup.rs, host_input.rs (3) | R/D 분리 ≤40 |
| claude/herdr_turn/composer.rs, first_input.rs, discord/recovery_engine/herdr_row_end.rs (3) | first/recovery R, custody 동일 ≤40 |
| claude_compact_trigger.rs (1) | busy worker→pending ≤25 추가 및 삭제 |
| discord/router/message_handler/intake_turn.rs, tui_followup.rs (2) | Idle 재거절/marker-only 제거만 ≤20. provider_dispatch/owner_input.rs는 prod 수정 목록에서 제외 |
| tui_input/actor/gate.rs, actor/pane.rs, tui_input/submission.rs (3) | proof 전달/기존 D·late uncertainty ≤25 |
| discord/commands/tui_passthrough/herdr_settings.rs, claude.rs, claude_tui/hook_registry.rs (3) | UI helper 경계/죽은 auth caller/필요시 scoped buffer 접근 ≤5 추가 및 삭제 |

r1에서 active hold 제거로 prod 1파일과 약40줄 순증을 줄여 26/+1080이다.
추가 코드 예산 합계1205에서 중복 wait/compact 삭제분을 반영한 순증 상한1080이며, latch는 기존 evidence 모듈 예산에 포함했다.
실제 diff가 PLAN +1200 또는 30/+1500을 넘으면 중단하고 A=strict terminal/continuation fence+evidence/wait+fresh/Idle prompt,
B=compact pending+남는 Idle adapter 제거로 나눈다. 각 조각은 자체 RED/GREEN/custody 시험을 가진다.
default-off dead production code는 만들지 않는다. 비활성 기반이 필요하면 실제 관측 caller까지 포함하거나 test-gate로 두고 새 allow는 쓰지 않는다.

현재 origin/main 대비 합산 cap은 #8077 약20파일/+1005를 이미 포함해 이 증분 예산과 다르다.
구현 REVIEW_READY 전 코디네이터가 #8077 착지 head를 반영한 뒤 #8080 증분 cap을 검사한다.
레인에서 main merge/rebase로 이를 숨기지 않는다. 설계 커밋 자체는 baseline 대비 prod0/+0이다.

## 7. 대상 시험·뮤턴트·DoD6

| DoD | 구현 시험과 실패 판정 |
|---|---|
| 1 46열/resize | 46×39 clipped footer/MCP/background chrome와 120→46→80 event/ANSI fixture. current terminal Idle 뒤 같은 Discord original 한 번, marker 영구 false여도 성공. hook 없음/지연/중복/같은 mtime. fixture 합성·발췌 여부/SHA 명시 |
| 2 boot/clear | empty/init-only, Start 전후, composer 미렌더/half-render/완료. startup/resume/clear 구별. current Start 전 키0, 후 허용 D에서1. old Start/Stop/compact Start/잘못된 nonce·UUID·inode 키0. 2.1.322 격리 실측으로 [U] 해소 |
| 3 busy 회귀 | active hand_busy, intake native Running→into_busy_pane, owner native Running→Busy leaf에서 **기존 busy 주입/키열 불변**. 빈 composer는 paste,Enter, 기존 stash 적용 fixture도 불변. owner_input_busy_gate_tests의 Running 기대 유지. synthetic/#8077/Cancel/receipt 회귀 |
| 3 Idle 오인 방지 | Idle 전용 waiter/submit만 Streaming/UserSubmitted/Unknown·첫 local floor 구간 키0. terminal 뒤 같은 원본1. Busy branch를 hold로 바꿔 통과시키지 않음 |
| 4 draft | Person/AgentDesk/stash/modal/focus의 기존 결과·키열 보존. 일시 read 오류→same original Held→자동 watch→정상 read→1. 운영자 명령/수동 플래그 없음. Person cleanup 키0. 영구 unread는 안전성만/진행성 RED |
| 5 모델 | before narrow RED, after/Start/clear/resume/lostStop/race/restart/compact/draft/일시 unread GREEN. BusyActive/BusyNative 및 SuppressedStop/Compact 추가. no-latch/block-busy mutant RED. 실행 수·로그·SHA 기록 |

Idle 전용 fixture는 실제 caller→typed Held→NotSentProof→hold_not_sent→wake→same original receipt를 시험한다.
busy fixture는 기존 direct Busy leaf→paste,Enter→native queue/receipt를 시험하며 Held를 강요하지 않는다.
P2 fixture: suppressed root Stop → stop_hook_summary → 같은 mtime timer read → late assistant → 실제 final terminal.
Idle-ready 및 pending compact 경로는 실제 종료 전 payload/Enter0, 종료 뒤 같은 원본1이다.
active/native Busy로 처음부터 선택된 별도 입력까지 키0라고 주장하지 않는다.
latch arm/검사 제거 mutant는 첫 timer read에서 조기 Idle 주입 또는 compact를 허용하여 RED여야 한다.
soft summary를 즉시 완료로 바꾸는 mutant, Start/old Stop/mtime/TTL로 latch를 해제하는 mutant도 후보로 둔다.
source scan과 새 lib test id가 required PR lane에서 선택되는지 확인하고 0선택/skip을 GREEN으로 세지 않는다.
뮤턴트는 marker/busy/auth 재도입, mtime 정지=Idle, old Start/Stop 승인, pre-key generation 재검증 제거,
두 번째 front의 옛 Idle 재사용, Unknown=Empty, no held wake, busy compact, post-key Unknown replay,
continuation latch 제거, busy leaf를 Idle hold로 바꾸는 회귀 mutant다.
각 원인 제거 시험이 RED인지 확인하고 원복 SHA256 byte 동일을 기록한다.
#8077 exact close/숫자 target 재사용/C132/Unknown retention 회귀도 해당 경계에서 재사용한다.

구현 REVIEW_READY 전 cargo-shim+지정 profile로 대상 시험/check/clippy(자기 파일 warning0),
source_scan_readers failed0, prepush --static-only PREPUSH_STATIC_OK를 수행한다.
PG는 pg_throwaway만 쓰고 --test-threads=1을 쓰지 않는다.
이번 설계 레그는 cargo/PG/prod 시험을 실행하지 않았으며 모델 결과로 대신했다고 주장하지 않는다.

DoD6는 머지/배포 뒤 코디네이터가 46열 실제 채널의 기존 사용자 후속 메시지를 관측한다.
Discord message ID/channel/session/incarnation, 직전 native terminal/Stop 시각, 입력 수용·receipt 시각,
ready source/draft hold 이유를 같은 행에 기록해 화면 준비 미확정에 따른 지연·거절0을 확인한다.
메시지 수와 관측 구간이 필요하며 관측0개는 PASS가 아니다.
hook fallback ≤1초 관측 지연과 실제 Person draft 대기는 screen-marker 지연과 구별한다.
이 레인은 Discord 발송/실 pane resize/운영 설정 변경을 하지 않았다.

## 8. 리뷰 집중 항목

| 항목 | 막는 방법 / 남은 반례 |
|---|---|
| Stop 직후·다음 첫 transcript 줄 전 | Idle proof floor/revision 재검증, suppression fence. 기존 Busy 주입은 불변; unreported native key 창은 남음 |
| 유실·중복·역순·SubagentStop | Stop은 wake만, current source+turn read, 같은 mtime에도 timer. subagent로 parent Idle 승격 금지 |
| restart/resume old Stop | ephemeral ready 폐기, nonce/generation/UUID/file identity 확인. daemon restore와 provider relaunch 구별 |
| 같은 채널 두 입력 | owner/FIFO/first-key/receipt 재사용, body 같아도 message/ticket 별도. N=2 모델+caller 시험 |
| 옛 형식 복원 | durable queue/Unknown 형식 불변, mapping만 복원해 ready bit 부활 금지; wrapper/Process/Codex 경계 |
| 문서로만 선언한 guard | Idle actor gate/first input/post-wait/latch의 timer read까지 검사. active/native Busy는 불변 행과 기존 키열 시험으로 별도 검증 |
| 잘못된0/PASS | cfg/시험 선택 수·skip·버전·합성 fixture 명시. statechart wrapper 부재와 직접 TLC 실행 구별 |
| 권한 확대 | 기존 D/NotSentProof만, 새 Escape/Ctrl-U/resize/recreate/kill/다른 pane 정리 권한 없음 |
| 영구 capture 실패 | PermanentUnread 진행성 RED 공개; readiness 제거를 draft 보호 제거로 포장하지 않음 |
| main stop 관측 공백 | main tui_followup.rs:558의 기존 stop/Busy 선택은 유지. Idle R proof에 terminal/Stop이 없는 공백은 #8081 후속이며 새 stop 권한 없음 |

설계 리뷰는 Idle 준비와 불변 Busy 경로의 분리, suppression latch와 soft terminal의 결합, transient/영구 blind 경계를 확인한다.
busy 동작 변경이나 “정책 변화 없이 영구 blind까지 GREEN”을 제안하지 않는다.

구현 A caller 경합 시험: Busy 선택 직후와 paste 직후 각각 Finish를 강제하고 Enter 이후 receipt 장벽에서 중복 Idle wake 두 번을 넣는다. 장벽 중 owner.front=None, 이후 원본 user row/Enter/정산 각각1을 검사한다. B compact pending은 A에서 활성화하지 않는다.


### 구현 A 재개: Idle focus와 continuation 증거

Idle 일반 입력의 최종 focus/stash 검사는 `readiness/idle.rs`로 분리한다. MCP auth 배너는
준비 신호가 아니므로 여기서는 modal로 취급하지 않는다. 실제 startup/interactive dialog와
사람 composer 검사는 남기며, Busy의 `before_paste_by`·`inject_owned`·`screen::modal`은 변경하지 않는다.
호출자가 없어진 기존 Idle `before_paste` wrapper만 제거한다. `/compact`는 분할 B 전까지 기존 경로다.

AwaitResume의 새 활동은 suppression floor 이후에 시작한 완결 행이어야 한다. 행의 끝 offset만으로
Stop 때 일부만 쓰인 옛 행의 뒤늦은 완성을 새 활동으로 인정하지 않는다. continuation은 producer
활동 시각도 suppression 시각 이후인지 검사한다. 2.1.322의 timestamp 정밀도·생성/flush 순서는
격리 실측하지 못한 [U]이며, 모델은 이 byte/clock 구체화를 열거하지 않는다.
