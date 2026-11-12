# #8109 /effort 즉시 재기동 설계

기준 코드: 193a57168fcbcae4a9ca23b25bacc043dac2fd28. 조사일: 2026-11-12 KST. 설계만 수행하며 prod 변경·cargo·운영 설정/DB/store 접근·Discord 시험은 수행하지 않았다. 원 브리프의 A/B 재전송 비교는 10:0x 확정 결정으로 대체했다.

소스 worktree: /Users/itismyfield/adk-lanes/adk-i8109-d1. 아래 상태 차트 report의 상대 링크는 이 worktree의 docs/statecharts/session-lifecycle/ 기준이다. 리뷰 ref에는 같은 본문의 effort-restart.design.md를 함께 커밋한다.

## 확정된 동작

Discord /effort는 Claude와 Codex TUI 채널에서 현재 작업의 종료를 기다리지 않고, 같은 native session/thread를 resume하는 새 TUI incarnation을 즉시 시작한다. ultra도 하나의 채널 effort로 저장하고 다른 effort 선택 전까지 모든 후속 launch에 다시 적용한다. 지원하지 않는 모델에서는 설정과 모델을 바꾸지 않고 거절한다. /fast와 /model 단독 명령의 기존 적용 시점은 유지한다.

실제로 끊긴 턴이 있으면 성공한 재기동 뒤 AgentDesk 자동 입력 하나로 작업을 이어간다. 원문을 다시 보내지 않는다. 자동 입력 문안은 다음과 같다.

> [AgentDesk 자동 이어하기] effort 변경을 적용하려고 같은 세션을 다시 시작했습니다. 방금 중단된 작업의 진행 상태를 확인하고 남은 작업을 이어서 진행하세요. 이미 끝낸 작업과 이미 전달한 답변은 반복하지 마세요.

이 문안은 사용자 메시지로 위장하지 않는다. provider TUI composer를 통한 입력은 provider transcript에서 user role이 될 수 있으나, AgentDesk의 author는 bot이며 출처는 agentdesk_effort_continue, InputOrigin은 새 EffortContinuation(is_person=false)이다. 실제 요청자의 author ID·Discord message ID를 재사용하지 않는다.

최종 계약은 다음과 같다.

- 이미 게시한 답의 동일 output unit/range를 다시 게시하지 않는다.
- 대기 중 사용자·자동 입력의 original/ticket/custody를 유지한다. 실패를 Accepted나 Consumed로 취급하지 않는다.
- 실제로 중단한 각 턴에 중단 사실을 정확히 한 번 알린다. 자연 완료가 먼저 확정된 턴과 실행 전 대기 입력은 중단 대상으로 세지 않는다.
- 중단된 episode마다 자동 이어하기 입력은 성공한 resume 뒤 정확히 하나다. 최초에 진행 중 턴이 없으면 0이다.
- 영구 인프라 장애·끝없는 새 effort 변경·사용자의 명시적 stop/clear가 없고 기존 출력·입력 IO가 회복한다는 공정성 아래, 자동 이어하기와 대기 입력이 결국 처리된다.

## 현재 코드와 달라지는 경계

현재 tmux Claude /effort는 설정 및 channel_relaunch_pending을 저장한다(tui_passthrough.rs:145-151, config/provider_settings.rs:285-310). 다음 turn 시작의 control/relaunch.rs:232-256이 기존 TUI를 종료할 뿐, 이 명령이 새 프로세스의 ready까지 직접 실행하는 구조가 아니다. tmux_recreate.rs:87-90은 busy를 Deferred로 돌린다.

Herdr /effort는 재기동 경로가 아니다. tui_passthrough/herdr_settings.rs:195-233에서 transition/composer lock과 정확한 실행을 확인한 뒤, ready pane의 /effort overlay를 조작하고 저장한다. :225가 명시하듯 relaunch를 예약하지 않는다. 이 준비 상태 대기와 키 조작을 새 즉시 restart의 실행 수단으로 쓰지 않는다. /fast의 기존 overlay 동작은 유지한다.

기존 busy 거절문 한 줄을 지우거나 /clear를 호출하는 방식은 채택하지 않는다. clear는 control.rs:551-573에서 큐·provider session·selector를 버린다. 기존 HostTerminated는 exact execution을 닫고 큐를 보존하지만 알림/상태 패널을 생략한다(herdr_terminate.rs:337-369, turn_finalizer/cleanup.rs:278). 또한 현재 clear_end는 fresh launch만 증명하므로 same-session resume의 증거로 사용할 수 없다(tui_o/writer/rotation/clear_end.rs:167-179).

effort 재기동이 이미 저장된 /fast·/model pending과 겹치면 같은 launch snapshot으로 함께 적용하고 그 값에 대응하는 pending만 소비한다. 다음 사용자 턴에서 같은 변경을 이유로 두 번 재기동하지 않는다. launch 준비 중 effective model이 바뀌면 capability를 새 모델로 재검증하고, 아직 종료하지 않았으면 mutation 없이 다시 준비한다. 이미 old TUI가 끝났으면 같은 intent/C ticket을 유지한 채 마지막 유효 snapshot으로 준비한다. 모델의 무관한 자동 교체는 없다. 수용 시험은 model/fast pending + effort 즉시 변경→same-session restart1 및 최신 모델의 capability 재검증이다.

## CLI와 모델 지원

Codex CLI 0.167.2와 Claude Code 2.1.322를 --version/--help로 재확인한다. 로컬 설치 binary의 정적 코드·프리셋만 조사하며 금지된 사용자 설정과 캐시를 읽지 않는다. CLI 조사표와 정확한 byte 좌표·SHA는 아래 부록에 포함한다.

Codex 지원 값의 진실은 모델별 supported_reasoning_levels이다. 전체 열거형에 ultra가 존재한다는 이유로 모든 모델에 제안하면 안 된다. 확인한 프리셋에서 gpt-6-astra, gpt-6.1-sol, gpt-6.1-terra, gpt-6-sol, gpt-5.6-sol, gpt-5.6-terra는 ultra를 갖고, gpt-6-luna와 gpt-5.6-luna는 max까지, gpt-5.5는 xhigh까지다. 이 목록은 설치 버전의 evidence이며 영구적인 모델명 substring 규칙으로 삼지 않는다.

채널의 모델은 override→dispatch role→role-map→provider default 경로로 해석한다. Claude의 기존 다섯 일반 effort는 기존 CLI 유효성 계약을 유지하며, 아래 Unknown 거절은 Claude ultra와 모델별 capability가 필요한 Codex에 적용한다. 기본 모델이 None이면 launch에서 -m을 생략하므로, 설정을 읽지 않은 이 조사만으로 실제 운영 채널의 모델을 특정하거나 ultra 지원이라고 단정할 수 없다. 구현은 해당 launch가 사용할 resolved model과 동일한 값으로 capability를 판정한다. 모델이 미확정이면 선택지를 추측하지 않고 '현재 채널 모델을 확인하지 못해 effort를 바꾸지 않았습니다'로 거절한다.

선택지는 고정 poise ChoiceParameter 5개 대신 문자열 autocomplete + 진입점 단일 검증으로 제공한다. 제안은 provider/model의 지원 값과 사용자가 입력한 prefix의 교집합이다. 수동으로 입력된 ultra도 같은 validator를 통과해야 한다. 모델/설정 소유권 검사는 현재 check_settings_owner와 host guard를 그대로 사용한다. 지원을 확인하지 못한 모델의 fallback, 자동 모델 교체는 없다.

공식 자료는 Ultra가 subagents를 사용하는 모드이고 Luna가 Ultra를 지원하지 않음을 뒷받침한다. 이는 실제 채널/계정에서의 가용성을 증명하지 않는다. [OpenAI Models](https://learn.chatgpt.com/docs/models).

## 저장과 launch 계약

기존 DiscordBotSettings.channel_effort_levels와 설정 read/write/clone→try_save→publish 순서를 재사용한다. /fast와 같은 채널 설정 저장면을 쓰되 새로운 전역 환경변수나 설정 파일을 만들지 않는다. 기존 low/medium/high/xhigh/max 문자열은 그대로 복원하며 ultra를 새 정규 값으로 추가한다. 저장된 ultracode라는 알 수 없는 구형 값은 명시적 사용자 선택 없이 ultra로 승격하지 않는다.

effort 범위는 실제 명령을 실행한 채널이다. /fast의 parent fallback을 무조건 복제하지 않는다. 부모 채널의 저장값을 바꾸어 다른 채널의 TUI를 재기동하는 기능을 만들지 않는다. 같은 채널에서 provider가 바뀌면 현재 provider/model에 값이 유효한지 다시 확인한다.

Claude:
- ultra는 일반 --effort ultra 인자가 아니다. 기존 hook/fast settings payload에 ultracode:true를 합쳐 기존 한 개 --settings 경로에 전달한다. hooks·fastMode·SessionStart를 덮어쓰는 두 번째 settings 인자를 붙이지 않는다.
- 다른 effort 선택 시 ultracode:false를 명시하고 --effort <level>을 전달한다. 이전 resume의 session state나 상속 환경 때문에 ultra가 남지 않아야 한다.
- channel 선택이 있는 경우 기존과 같이 CLAUDE_CODE_EFFORT_LEVEL의 우선권을 제거한다. 권한 모드와 allowed tools, 계정 home, hook endpoint는 현재 launch와 같다.
- 같은 native session ID를 --resume으로 보존하며, transcript를 못 찾거나 계정이 바뀌었으면 새 세션으로 대체하지 않는다.
- 실제 active ultracode까지 확인한 후 Applied로 응답한다. capability probe의 별도 프로세스 active 값은 실제 새 TUI의 applied 증거로 재사용하지 않는다. ready 시점에는 재기동 완료/적용 확인 대기를 구분한다.

Codex:
- channel override > 기존 role/wrapper/environment fallback > CLI default 순으로 한 번 결정한 effort를 CodexLaunchOptions.reasoning_effort에 넣는다.
- 기존 append_codex_common_launch_args 경로의 -c model_reasoning_effort="ultra"를 사용한다. AGENTDESK_CODEX_REASONING_EFFORT를 프로세스 전역에서 바꾸지 않는다.
- tmux와 Herdr의 option fingerprint에도 같은 값을 넣는다. 기존 fresh/후속 입력의 drift 비교를 무력화하지 않는다.
- resume <기존 thread ID>를 사용하고 새 session ID 생성이나 --last/--fork 계열로 대체하지 않는다.
- Herdr는 현재 fresh만 허용한다. validate_herdr_fresh_argv를 느슨하게 만드는 대신 fresh 검증을 그대로 보존하고, 정확한 expected_native_session_id와 resume argv가 일치하는 별도 resume validator/prepare child를 추가한다. Pending→Bound Startup에서도 같은 native thread가 확인되어야 한다.

## Claude capability의 미해결 기술 전제

설치된 Claude의 실제 활성값은 requested && ultracodeAvailable이고, ultracodeAvailable은 동적 workflows gate와 모델 지원의 결합이다. 모델 이름만 비교하거나 권한 모드만 보고 true로 취급하면 안 된다.

현재 CLI statusline/common hook payload와 AgentDesk hook_bundle/statusline.rs:31-34에는 ultracodeAvailable도 실제 ultracode도 없다. effort.level=high 같은 값은 ultracode active 증거가 아니다. --settings true도 requested만 증명한다. CLI의 get_settings control 응답에는 applied.ultracode / ultracodeRequested / ultracodeAvailable이 있으나 현재 TUI에 이 control transport를 연결한 AgentDesk 경로는 확인되지 않았다.

구현 조각 1의 필수 선행 검증은 같은 Claude binary·resolved model·auth overlay·권한/launch settings로 모델 요청 없이 get_settings를 얻을 수 있는 자동 capability adapter를 만드는 것이다. print/control transport를 쓴다면 초기화/종료와 TUI의 workflows predicate 입력 동등성을 코드·격리 실기로 증명해야 한다. 동등성이 확인되지 않으면 그런 probe를 신뢰하지 않는다. 이 레그에서는 CLI 실행 제한 때문에 probe를 실행하지 않았다.

capability port는 Supported(active/available 관측 포함), Unavailable, Unknown(이유)의 세 결과를 돌려준다. Unavailable은 '현재 Claude 모델/워크플로에서 ultra를 지원하지 않아 변경하지 않았습니다. 모델은 그대로 유지됩니다.'로 거절한다. Unknown은 'ultra 지원 여부를 확인하지 못해 변경하지 않았습니다. 모델은 그대로 유지됩니다.'로 거절한다. 두 결과 모두 저장·중단·자동 입력을 만들지 않는다. 이 adapter의 동등성 증거 없이는 Claude ultra를 구현 완료나 실기 GREEN으로 선언할 수 없다. 모델의 Supported 값은 검증될 외부 관측 전제이며 구현이 존재한다는 뜻이 아니다.

## 즉시 재기동 순서

본체는 commands/effort_restart/ child에 둔다. 기존 tui_passthrough, control, provider 거대 파일에는 호출/인자 연결만 둔다. 명령 권한은 기존 /effort 범위이며 새 운영자 명령·수동 스위치·독립 retry 프레임워크를 추가하지 않는다.

1. 명령 defer 후 현재 effective provider/model, 해당 host와 실행/native session/account, capability를 확인한다. transition lock을 잡고 target·설정이 여전히 같은지 재검증한다. 동일한 저장값이고 미해결 restart debt가 없으면 효과 없는 응답이다.
2. 직렬화된 채널 단위 restart intent를 durable하게 기록한다. request identity, desired effort revision, old execution nonce/binding, same native session, 실제 active episode 및 자동 continuation의 별도 source/ticket을 보존한다. 대기 큐 전체를 복제하지 않는다. 설정 저장 실패 또는 intent 저장 실패면 kill하지 않는다.
3. owner의 effort 전용 continuation barrier를 세운다. 사용자 원본·Held custody는 현재 durable owner에 남긴다. 전달 직전 NoKeyYet/MayHaveSent 상태는 기존 writer 소유권으로 결정한다. 아직 한 키도 안 들어간 원본은 대기 입력이다. MayHaveSent는 전달 실패로 재분류하거나 원본을 재전송하지 않는다.
4. 정확히 capture한 active token/turn/exec에만 중단 원인을 effort_restart로 기록하고 물리 TUI를 종료한다. busy를 무시하는 것은 이 명령의 현재 턴에 대한 정책이며, 소유권·target identity 검사를 우회하는 권한이 아니다. tmux는 admitted target의 기존 종료 수단, Herdr는 기존 exact termination의 close→physical Ended 확인→retire CAS→settle 경로를 사용한다.
5. 실제 종료 후 old source의 유효 EOF와 cutoff, binding seq, old incarnation을 restart-end witness로 durable하게 기록한다. native interrupted closer가 있다면 그 증거를 우선 쓴다. 없는 경우에도 이 witness는 'native transcript에 완료가 기록되었다'고 가장하지 않는다. old process가 실제로 끝났다는 확인으로 old episode만 닫는다.
6. 같은 witness를 InputFacts·writer·presence가 먼저 소비해 old Open을 닫는다. writer가 old episode의 미게시 prefix를 seal하고 기존 unit identity/ledger의 durable delivery debt로 보존한다. 이미 Prepared인 POST의 소유권은 그대로 둔다. Discord POST 완료를 물리 resume의 선행조건으로 기다리지 않는다. 이미 게시한 offset·spool·checkpoint는 초기화하지 않는다. old inflight는 output snapshot/receipt debt가 durable한 exact old identity로만 settle한다. output drain/notice 실패는 보존된 debt로 회복하며 post success 전에 checkpoint를 진행하지 않는다.
7. 중단 안내는 한 durable episode identity를 사용한다. 기존 outbox/nonce 게시·영수증 경로로 같은 사건만 retry하고 unkeyed say를 별도로 보내지 않는다. 이미 끊긴 턴의 partial reply나 [Stopped]와 중단 문구가 두 군데 게시되지 않도록 effort cause의 lifecycle notice 소유자는 한 군데다. ordinary command ACK는 중단 안내로 세지 않고 같은 문구를 반복하지 않는다.
8. turn presence는 old witness와 정확히 일치하는 Open만 닫는다. 새 binding/nonce의 관측을 다시 얻으며, daemon current_generation 자체를 증가시키지 않는다. 새 incarnation nonce가 바뀌어도 native session은 동일하다. witness 없이 new ready 화면만 보고 old Open을 Idle로 덮어쓰지 않는다.
9. 다음 사용자 턴을 기다리지 않고 provider의 resume launch를 즉시 실행한다. 같은 계정·working directory·native session, 새 execution nonce와 requested effort를 전달하고 Startup/Bound/ready 및 validated launch args 반영을 확인한다. 출력 writer host의 재획득은 기존 lease/custody 규칙을 통과한다. 실제 Codex turn_context는 첫 입력 뒤 생성될 수 있으므로 이것을 launch/continuation의 선행조건으로 기다리지 않는다.
10. capability preflight 및 같은 세션의 verified launch/ready 확인 후 미전달 continuation 하나를 기존 owner에 앞자리로 원자적으로 admit한다. admission만으로 barrier를 풀지 않는다. 같은 C ticket이 정확한 first reserve와 실제 Submitted/receipt를 얻을 때까지 사용자 grant와 prefer_person 회전을 막는다. C가 일시 Held 또는 Unknown이면 barrier/custody를 유지해 같은 original만 reconcile한다. 그 뒤 사용자는 원래 FIFO다. transition guard를 해제한 다음 SessionTransitionCompletion/owner wake로 drive를 시작한다. barrier는 first submit 후 owner commit에서 해제하므로 이 drive와 자기 잠금을 교착시키지 않는다. 단순 ready 기록이나 Held watcher Gone는 wake를 대신하지 못한다.

durable intent의 정본은 기존 owner SnapshotV2다(queued_head/v2_types.rs:18-38). 기존 unfinished_handoff 패턴처럼 optional effort_restart 필드(serde default, 구 snapshot은 None)를 추가한다. 새 타입 본체는 v2_types/effort_restart.rs child에 두고 commands/effort_restart/state.rs는 mailbox actor의 기존 Owner::commit(v2/commit.rs:39-71)을 호출하는 adapter다. 독립 restart journal 파일이나 두 번째 큐 저장소를 만들지 않는다. phase·desired revision·cancel revision·C original/ticket·notice debt를 같은 snapshot에서 갱신한다. witness 파일은 physical end라는 불변 사실의 출력 투영이고 intent와 별도 제어 정본이 아니다.

intent를 준비한 뒤 기존 settings 저장에 성공하고 그 revision의 SettingsSaved를 owner commit에서 확인한 때만 kill한다. 설정 저장 실패면 intent를 명시 취소하고 기존 큐를 보존한다. 두 저장 사이 crash는 Prepared intent/기존 settings 결과를 재관측하여 같은 revision을 복구하며 성공으로 추측하지 않는다. 새 명령/명시 stop이 진행한 revision을 옛 복구가 덮지 못한다.

intent/witness는 이 명령의 미완료 작업과 기존 output 경계만 다루는 작은 durable 기록이다. intent에서 kill 전/kill 확인 뒤/새 bind 뒤/continuation admit 확인 뒤를 구분한다. enqueue의 Pending을 완료로 지우지 않는다. 자동 continuation ticket·original과 post한 notice receipt를 기록에서 분리해서 새로 생성하지 않는다. 완료된 작업은 기존 저장의 정상 정리 규약으로만 닫으며 새 bulk 삭제/오래된 세션 kill을 만들지 않는다.

## 명시적 중지의 선형화 지점

현재 judged_stop.rs:406-430은 A token이 이미 정산된 공백에서 NoActiveTurn/SessionStop을 돌려줄 수 있다. 따라서 기존 stop 함수를 호출한다는 선언만으로 pending C가 취소되지 않는다. S6의 effort_restart/cancel child를 다음 경로에 최소 호출로 연결한다.

- slash/text stop: tmux_runtime/judged_stop.rs:345 begin_user_stop 직전의 사용자 stop 목적에서 해당 채널 pending C intent의 cancel revision을 기록한다. text_commands.rs:20도 같은 의미를 따른다.
- HTTP nonforce Cancel: queue/herdr_stop.rs:12와 cancel_effects.rs:78의 local/holder 경로에서 같은 pending intent를 닫는다. stop은 기존 사용자 B를 purge하지 않는다.
- clear: control.rs:510 및 control/clear_teardown.rs:138의 기존 discard 지점과 함께 그 native session에 묶인 C만 취소한다. explicit clear의 원래 discard 범위는 보존한다.
- force Cancel/Terminate: queue/http_force.rs:52와 herdr_terminate scoped postlude가 capture한 operation/exec와 일치할 때만 닫는다. old nonce 요청으로 새 execution/intent를 취소하지 않는다.

cancel revision과 owner barrier/debt terminal 상태는 같은 owner commit에서 선형화한다. launch 직전 및 continuation admission/first reserve 직전 같은 revision을 재확인한다. 물리 종료/계정/selector 권한을 추가하지 않는다. 수용 시험은 'A 정산→사용자 stop→resume 성공→C=0, B 보존'과 'stale Terminate→새 C 보존'이다.

## 자동 이어하기와 경합 표

| 상황 | 자동 입력 수와 순서 | 보존/완료 조건 |
|---|---|---|
| 처음부터 active 턴 없음 | 0. 기존 대기 입력 FIFO | 설정 적용을 위한 same-session restart만 한다 |
| active A를 끊음 | A에 대한 C(A) 1개, 이어서 기존 B… | A original 원문은 재접수하지 않음 |
| restart 중 사용자 B 도착 | C(A) 뒤 B, 기존 사용자 상대 순서 유지 | B original/ticket을 Held로 저장, 새 사용자 입력이 C를 취소하지 않음 |
| restart 실패/응답 유실 | ready 확인 전 C=0 | 같은 intent와 C ticket 보존; 재시도 성공 시 총 1 |
| C 전달 전 effort가 연속 변경 | 같은 interrupted episode의 미전달 C 하나 유지, 마지막 저장값으로 launch | 요청마다 새 C를 만들지 않음; 이미 받은 사용자 입력 index와 queue front를 같은 owner commit으로 갱신 |
| C가 실제 시작된 뒤 새 effort | C 실행 자체가 새로 중단된 episode이면 그 episode의 continuation 하나 | 앞선 C(A)를 재전송하지 않음; 각 실제로 중단된 episode당 1 |
| old A의 자연 완료가 stop capture보다 먼저 확정 | A 중단 안내 0, C(A)=0 | 자연 완료 body/receipt가 우선; 설정 restart는 계속 |
| continuation delivery MayHaveSent/Unknown | 새 ticket로 전송하지 않음 | 기존 native receipt/observer와 같은 original을 reconcile |
| 명시적 /stop·Cancel·Terminate·/clear가 도중에 옴 | 그 사용자의 현재 명령 정책이 우선 | 구 epoch continuation이 뒤늦게 되살아나지 않도록 동일 operation/owner commit에서 debt를 명시적으로 닫고 안내 |
| session/account/target가 바뀜 | 자동 continuation 0 | 같은 session resume을 확인할 수 없으면 새 세션 fallback 없이 실패, 대기 원본 보존 |

barrier의 조건은 intent에 저장한 미제출 C ticket 하나로 표현하며 일반 우선순위 프레임워크를 추가하지 않는다. 일반 machine enqueue는 위 순서를 보장하지 않는다. 현재 source_admission은 tail에 넣고, busy machine에서는 prefer_person이 사용자를 앞으로 회전시킨다. effort 전용 barrier와 앞자리 admission을 기존 owner snapshot 한 번의 commit으로 연결한다. 별도 일반 우선순위 큐나 운영자 reorder 명령은 만들지 않는다.

native/TUI 입력이 모호한 상태에서 무조건 재시도하면 '정확히 1'을 깨뜨린다. 전달 모호성은 기존 receipt 관측이 결국 해소된다는 조건 아래 진행성을 검증하며, 영구 모호성을 성공이라고 하지 않는다. 새로운 내부 retry는 사용자 입력으로 분류하지 않는다.

## launch 완료와 실제 적용 확인의 구분

continuation의 선행조건은 capability preflight, exact same-session/new-incarnation bind, ready와 검증된 launch args다. 'Codex 새 turn_context가 아직 없음'은 실패가 아니고 첫 입력을 기다리는 상태다. C를 넣기 위해 새 context를 기다리고, context를 만들기 위해 C를 기다리는 순환을 만들지 않는다. C 또는 첫 사용자 턴이 만든 native evidence만으로 Applied를 비동기 확정한다. idle에서는 자동 입력을 만들어 검증하지 않는다.

Claude도 probe의 requested/active와 실제 incarnation active를 구분한다. 실제 TUI의 Ultracode 표시 또는 연결이 검증된 actual-session get_settings가 없으면 '적용 확인 대기/미확인'으로 남긴다. 이를 launch 실패나 unsupported로 바꾸어 원문을 재전송하지 않는다. 실제 evidence가 요청과 다르면 mismatch 안내와 intent의 실패 상태를 남기며 무한 자동 재기동을 추가하지 않는다.

수용 시험: resume ready + 새 turn_context 없음→C 정확히1→그 C의 context로 Applied 확인. idle ready + context 없음→C0, pending-confirm. 오래된 context와 별도 probe 결과로 Applied 확정은 0이다.

S4에서 old Herdr effort overlay 호출을 제거하면 herdr_settings.rs의 effort/effort_labels_known 전용 본체도 제거한다. /fast의 공유 ready/record 경로는 보존한다. dead_code를 allow로 덮지 않는다. 기존 EffortLevel은 새 문자열 boundary validator 뒤의 내부 표현으로만 유지하거나 신규 selection 타입으로 대체해 사용처를 남긴다.

## 기존 시험의 새 계약

tui_passthrough.rs:349 truthful_effort_choices_exclude_rejected_ultracode는 정적 다섯 선택지 고정 대신 truthful_effort_choices_match_provider_model_capabilities로 바꾼다. Claude Available에는 canonical ultra를 포함하고 Unavailable/Unknown에는 제외한다. Codex는 현재 모델 metadata의 지원 값과 정확히 같아야 한다. raw ultracode라는 기존 미지원 문자열을 묵시적으로 승격하지 않는 시험은 따로 유지한다.

gateway_runtime/slash_command_tests.rs:705 registered_choices_cover_every_fixed_slash_parameter에서는 effort를 고정 choices 목록에서 빼고, autocomplete=true 및 provider/model filtered 결과·수동 invalid 입력 거절을 별도 검증한다.

control/relaunch_effort_tests.rs의 다음 새 턴 재기동 기대는 기존 /fast·/model 회귀에 유지하며, /effort 자체는 command 직후 old PID 종료·같은 session 새 nonce/ready·자동 continuation1 기대를 새 시험으로 고정한다. Codex 전체 거절 기대는 지원 모델 저장/즉시 resume와 미지원 ultra 거절(설정/모델/kill 변화0)로 나눈다. Herdr off-local 거절은 host unavailable/unknown·non-TUI 거절을 유지하고 지원 Herdr에는 같은 resume 계약을 시험한다.

### 안내의 실제 전달 증거

기존 persistent outbox는 enqueue 중복을 막는 수단이다(message_outbox.rs:238-315). 그것만으로 Discord visible exactly-once까지 이미 구현됐다고 주장하지 않는다. retained_input_notice 패턴을 참고하되 일반 lifecycle_notifier가 항상 nonce를 받는다고 가정하지 않는다(outbound/manual_delivery/headless_nonce.rs:31-41,178).

effort notice child는 old interrupted episode에 묶인 stable event/nonce와 전달 receipt를 intent에 보존하고 기존 nonce outbound port를 사용한다(gateway.rs:320, restart_report.rs:439-451, outbound/manual_delivery.rs:532-560). HTTP 결과 유실은 기존 durable delivery/history 관측으로 같은 event를 reconcile한 뒤에만 재시도한다. 다른 nonce·새 outbox key·unkeyed say로 우회하지 않는다. accepted message ID 또는 exact duplicate receipt를 얻기 전에는 notice settled로 쓰지 않는다. 영구 모호성은 debt/실패로 남기고 GREEN으로 선언하지 않는다. 이 adapter는 일반 outbox 전체를 고치는 작업이 아니며 S4의 lost-reply+restart 시험과 실제 wire nonce 증거가 수용 조건이다.

## 이미 main에서 충족된 것

아래는 193a57168f의 구현을 재사용한다. 이 이슈 전체가 이미 완료된 것은 아니다.

- Claude 채널 effort 저장/복원 및 같은 session/account resume 검증: config/provider_settings.rs:266-310, relaunch.rs:136-161, tmux_recreate.rs:80-109.
- 정확한 Herdr execution 종료·captured token 정산·큐 보존: herdr_terminate.rs:437-604 및 :337-369.
- 기존 owner ticket/custody/MayHaveSent 처리와 synthetic original 복원: queue_io/input_owner/admission.rs:25-33, start.rs:12-51, turn_orchestrator/queued_head/v2/source_admission.rs:43-79.
- O writer의 Prepared/ledger/lease/uncertain-history 규칙: tui_o/writer/deliver.rs:1-125.
- transition 해제 뒤 owner wake: session_transition.rs:120-153.

## 상태별 정리 책임

| 자원 | 이번 변경의 책임 | 재사용하지 않는 파괴적 동작 |
|---|---|---|
| inflight | exact old turn nonce/source/exec로 종료·전달 debt 유지; successor row는 건드리지 않음 | channel 전체 stale unlink, /clear |
| 자리표시 | 기존 controller로 old entry를 terminal/aborted로 닫고 edit 실패 시 retry debt 유지 | 먼저 flag/row 삭제 |
| tui_o 답 | old range drain→ledger commit; 새 이어하기 episode에는 새로운 unit identity | old transcript/spool/checkpoint 초기화, bridge와 O 이중 게시 |
| owner input/Held | original·source generation·ticket 유지, C 앞자리+사용자 FIFO를 한 commit | take/clear pending queue, new ticket retry |
| watcher | old reader의 drain/종료 권한을 기다리고 새 nonce/bind로 재관측, 기존 host lease 재획득 | 옛 reader를 무조건 abort하고 처음부터 tail |
| turn presence | 실제 종료 witness가 old Open만 닫고 new binding 재관측 | timeout/pane ready만으로 Idle 선언 |
| boot/recovery | 미완료 effort intent의 같은 native/session/continuation key를 복원, 실제 phase에 따라 한 번 이어감 | daemon restart generation과 TUI incarnation 혼용, DrainRestart 원본 replay |

DrainRestart/HotSwapHandoff는 서버 재시작과 inflight 원본 복원 정책이 있으므로 effort 변경의 자동 이어하기 원인으로 재사용하지 않는다. legacy output 경로는 census에만 포함하고 새 기능/결함 수리를 얹지 않는다. O 경로와 그 입력·물리 종료 연결만 수용 범위다.

## 검증 계획

이 레그의 실행 범위는 정적 읽기와 상태 차트다. 아래는 구현 후 격리 시험 채널에서 에이전트가 직접 수행할 절차이며, 현재 Discord 메시지 전송 금지를 넘어서 실행하지 않았다. agent-desktop와 agent-browser SKILL.md 및 설치 CLI 도움말로 사용 가능한 관측/입력 수단을 확인했다.

1. 수정 전 새 수용 시험이 실제 old code에서 RED인지 먼저 확인한다. fake tmux/Herdr·임시 runtime root·격리 PG fixture·mock Discord transport만 쓴다. SQL 시험은 격리 pool이 실제 실행됐는지 기록한다. production 15432를 쓰지 않는다.
2. capability true/false/unknown, active/idle, tmux/Herdr, Claude/Codex, submit 전/후, delayed/duplicate completion, restart failure와 복원, 연속 effort 2회, 새 사용자 2개를 표로 시험한다.
3. 원인 제거 mutant는 (a) old boundary 소비 제거, (b) continuation barrier 제거, (c) ticket 새로 생성, (d) witness 전 checkpoint, (e) owner wake 제거, (f) ultracode:false 제거, (g) model 지원 무시다. 각 표적 시험이 RED여야 하며 mutant 원복은 byte 동일성을 남긴다.
4. 사용자에게 시험을 부탁하기 전에 agent-desktop로 Discord 앱을 snapshot하고 시험 채널명이 맞는지 확인한다. 설치 platform/core skill을 읽고 현재 ref의 composer에서 /effort autocomplete를 선택·실행한다. ref가 stale이면 다시 snapshot한다. action 결과가 may-have-delivered면 다시 Enter하지 않고 관측한다. 물리 입력이 필요한 앱이면 사용자 승인 없는 --headed 전환으로 우회하지 않는다.
5. agent-browser는 같은 시험 채널 웹 UI나 앱 대시보드에서 선택지·ACK·중단 안내 1회·후속 답을 확인한다. 사용 가능한 로그인된 browser를 독립 세션/tab에 연결하고 snapshot/ref로 조작한다. 실제 Herdr/Claude/Codex pane에는 시험 에이전트가 키를 직접 보내지 않으며 입력은 Discord /effort 제품 경로로만 한다.
6. 긴 진행 작업 A와 컨텍스트 표식 K를 만든 뒤 /effort ultra, 재기동 중 B와 C를 입력한다. A 중단 안내 1, continuation native input 1, B/C FIFO, old body 중복 0, placeholder 고착 0, prompt 원문 재전송 0을 관측한다. idle 상태에서는 continuation 0이어야 한다.
7. launch 증거는 시험 PID만 지정해 ps의 필요한 argv를 추출하고 credential/env 전체를 기록하지 않는다. Claude는 같은 --resume ID와 hook payload 안의 ultracode:true/false 설정, **실제 get_settings active 값 또는 실제 TUI Ultracode 표시**를 함께 확인한다. statusline effort.level이나 --settings 요청값만으로 GREEN 처리하지 않는다.
8. Codex는 새 PID의 resume ID와 -c model_reasoning_effort로 launch를 확인한다. C 또는 첫 후속 사용자 입력 뒤 생성된 rollout의 그 turn_context.reasoning_effort로 실제 적용을 비동기 확인한다. idle 재기동이면 입력 0을 유지하고 적용 확인 대기로 표시한다. 오래된 turn_context를 증거로 쓰지 않는다. --model을 전달하지 않은 모델은 rollout의 실제 model과 지원 metadata도 확인한다.
9. 새 incarnation nonce, same native session/thread ID, 이전 표식 K를 이어서 활용한 답, continuation source/ticket receipt를 함께 남긴다. '맥락 유지'는 ID만 같다는 것으로 끝내지 않는다.
10. 해당 required test lane에서 새 시험 ID가 선택되는지 확인하고 check/clippy warning 0→static prepush→review ref push/read-back 순서를 따른다. cargo 전에 headroom/seed_target 규칙을 적용하며 이 설계 레그에서는 cargo를 실행하지 않는다.

## 사용자 결정 항목

1. 결정됨: 원문 재전송 없이 AgentDesk 자동 이어하기 입력 하나. 위 경합·순서는 이 결정의 구현 세부이며 사용자 입력을 조용히 취소하거나 재작성하지 않는 쪽으로 정했다.
2. 결정됨: ultra는 다른 effort를 고르기 전까지 채널에 지속. Claude ultracode의 세션 한정 CLI 상태는 매 launch 주입으로 채널 지속 계약을 구현한다. 토큰/시간 증가와 자동 위임 특성은 UI 설명에 짧게 밝히되 별도 승인 팝업을 추가하지 않는다.
3. 결정됨: 미지원 모델에서는 거절하고 모델 유지.

남은 제품 결정은 0건이다. Claude capability 관측 adapter는 사용자 선호 문제가 아닌 선행 기술 검증이며, 증거가 없으면 구현을 성공으로 선언할 수 없다. resume 실패 때 새 세션으로 조용히 전환하지 않고 입력을 보존한다는 정책은 원 요청의 '같은 세션' 조건에서 정해진다.

## 리뷰 조각

상한 정본 pr_cap_prod.py의 FILES=30, NET=1500을 직접 읽었다. 조각별 계획선은 +1200이다. 아래 수치는 빈 줄/주석/시험/문서를 제외한 prod 순증 예상이며 실제 구현 전후 공식 스크립트로 다시 잰다. 조각은 리뷰 단위로 쌓되 기능 전체가 완성되기 전 배포하지 않는다. caller 없는 지원 코드는 최종 연결 전까지 cfg(test)로만 두고 새 allow/baseline 예외를 만들지 않는다.

### S1. 모델 capability·채널 effort·launch 인자

예상 prod 13파일 / +650, 의존: 없음. 기존 read/launch 호출에 연결한다. Claude capability probe는 무모델 요청·TUI predicate 동등성을 격리 확인하기 전까지 Unknown이며 알 수 없는 schema를 거절한다.

prod 파일(신규 child 포함):

- src/services/discord/commands/config/provider_settings.rs
- src/services/discord/commands/config/effort.rs
- src/services/discord/commands/config.rs
- src/services/discord/commands/mod.rs
- src/services/discord/model_catalog.rs
- src/services/discord/model_catalog/effort.rs
- src/services/discord/commands/effort_capability.rs
- src/services/claude/capability_probe.rs
- src/services/claude_tui/session.rs
- src/services/claude_tui/session/effort.rs
- src/services/claude_tui/hook_bundle.rs
- src/services/claude/tui_session_launch/prepared.rs
- src/services/codex.rs

대상 시험: capability_false_or_unknown_has_zero_save_stop; truthful_effort_choices_match_provider_model_capabilities; ultra_persists_and_normal_effort_turns_ultracode_off; channel_effort_outranks_env_and_wrapper_high; legacy_five_levels_restore; merged_settings_keep_hooks_fast.

수정 전 RED 계약: 현 main에는 ultra 저장/launch·Codex 채널 override·capability 추출이 없어 새 시험이 RED여야 한다. 기존 ultracode 제외 시험은 supported/unsupported/unknown 및 invalid raw 값 검증으로 바꾼다.

### S2. 같은 source의 물리 종료 witness와 O/presence

예상 prod 8파일 / +600, 의존: S1. same native source의 old incarnation만 닫는다. fresh ClearEnd/CodexEnd의 의미를 바꾸거나 native transcript에 거짓 Closed를 쓰지 않는다.

prod 파일(신규 child 포함):

- src/services/tui_o/store/mod.rs
- src/services/tui_o/store/restart_end.rs
- src/services/tui_o/writer/mod.rs
- src/services/tui_o/writer/actor.rs
- src/services/tui_o/writer/restart_end.rs
- src/services/tui_o/writer/input_facts.rs
- src/services/discord/turn_presence/activity.rs
- src/services/discord/turn_presence/activity/effort_restart.rs

대상 시험: same_source_open_without_closer_progresses_after_exact_end; wrong_nonce_or_cutoff_cannot_close; prepared_post_survives_restart_without_duplicate; old_end_cannot_close_new_open; witness_is_applied_before_seal.

수정 전 RED 계약: 현 main의 no-closer Open이 Unknown에 머무는 투영을 RED로 고정한다. native Closed 대조는 기존 GREEN을 유지한다.

### S3. 자동 continuation의 custody와 첫 제출 barrier

예상 prod 10파일 / +630, 의존: S2. 별도 stable synthetic source/ticket·generation 복원, 원자 prepend와 queued bind index 갱신을 구현한다. barrier는 first submit/receipt에서 해제한다.

prod 파일(신규 child 포함):

- src/services/turn_orchestrator/queued_head/v2_types.rs
- src/services/turn_orchestrator/queued_head/v2_types/effort_restart.rs
- src/services/discord/queue_io/input_owner/mod.rs
- src/services/discord/queue_io/input_owner/origin.rs
- src/services/discord/queue_io/input_owner/effort_continue.rs
- src/services/discord/queue_io/input_owner/start.rs
- src/services/discord/router/message_handler/provider_dispatch/owner_input.rs
- src/services/turn_orchestrator/queued_head/v2.rs
- src/services/turn_orchestrator/queued_head/v2/source_admission.rs
- src/services/turn_orchestrator/queued_head/v2/effort_continue.rs

대상 시험: continuation_is_machine_and_not_original_replay; continuation_before_two_waiting_users_even_when_held; retry_keeps_numeric_source_ticket_generation; admission_pending_is_not_consumed; coalesced_changes_owe_one_continuation; no_active_turn_owes_none.

수정 전 RED 계약: 현 main의 tail enqueue/prefer_person으로 순서 시험 RED. old 사용자 ID 재사용과 admit 직후 barrier 해제 mutant도 RED.

### S4. tmux 즉시 resume 흐름과 복원

예상 prod 15파일 / +1050, 의존: S1, S2, S3. 명령이 launch/ready를 즉시 구동한다. 작은 durable intent에 phase·C ticket·notice receipt를 보존하며 polling framework는 없다. O caller만 연결하고 legacy output을 수리하지 않는다.

prod 파일(신규 child 포함):

- src/services/discord/commands/effort_restart/mod.rs
- src/services/discord/commands/effort_restart/state.rs
- src/services/discord/commands/effort_restart/tmux.rs
- src/services/discord/commands/effort_restart/notice.rs
- src/services/discord/commands/effort_restart/recovery.rs
- src/services/discord/commands/tui_passthrough/herdr_settings.rs
- src/services/discord/commands/tui_passthrough.rs
- src/services/discord/commands/mod.rs
- src/services/discord/commands/control.rs
- src/services/discord/commands/tmux_recreate.rs
- src/services/discord/turn_finalizer/finalize_context.rs
- src/services/discord/turn_bridge/cancel_finalize_policy.rs
- src/services/discord/runtime_bootstrap/recovery_flush.rs
- src/services/codex/absence_launch.rs
- src/services/claude/tui_session_launch.rs

대상 시험: busy_effort_restarts_now_same_session; late_old_completion_keeps_successor; failed_resume_preserves_queue_and_sends_zero_continuation; restored_intent_resumes_one_ticket; lost_notice_reply_reconciles_one_post; normal_complete_wins_without_interrupt_notice; output_delivery_delay_does_not_block_physical_resume.

수정 전 RED 계약: 현 main은 busy Deferred·다음 턴 대기라 RED. 기존 same-session/account 보존 시험을 회귀 재사용한다.

### S5. Herdr exact restart와 Codex resume

예상 prod 9파일 / +730, 의존: S4. 기존 terminate core가 같은 lock을 재획득하지 않도록 held transition을 빌리는 scoped entry를 연결한다. fresh validator를 유지하며 resume 전용으로 exact native/thread를 검증한다.

prod 파일(신규 child 포함):

- src/services/discord/commands/effort_restart/herdr.rs
- src/services/discord/herdr_terminate.rs
- src/services/discord/herdr_terminate/effort_restart.rs
- src/services/claude/herdr_turn.rs
- src/services/claude/tui_session_launch.rs
- src/services/codex/herdr_turn.rs
- src/services/codex/herdr_turn/resume.rs
- src/services/codex/tui_session_launch/launch_policy.rs
- src/services/codex/tui_session_launch/launch_policy/resume.rs

대상 시험: herdr_busy_effort_closes_only_captured_execution; codex_herdr_resume_exact_thread_and_new_nonce; resume_wrong_thread_refused_before_pane_io; herdr_fresh_argv_still_rejects_resume; claude_herdr_reapplies_ultracode_and_false_on_exit; no_tmux_fallback.

수정 전 RED 계약: 현 main의 Codex fresh-only/Herdr overlay-only가 새 계약에서 RED. 기존 terminate successor fence·queue 보존·fresh guard는 GREEN 유지.

### S6. stop/Cancel/clear/Terminate의 pending continuation 연결

예상 prod 9파일 / +380, 의존: S4, S5. A settle→C submit의 NoActiveTurn 공백에서도 pending C를 명시 취소한다. 기존 stop/clear/force 큐 정책을 넓히지 않고 old nonce Terminate로 successor intent를 닫지 않는다.

prod 파일(신규 child 포함):

- src/services/discord/commands/effort_restart/cancel.rs
- src/services/discord/turn_bridge/tmux_runtime/judged_stop.rs
- src/services/discord/commands/control.rs
- src/services/discord/commands/control/clear_teardown.rs
- src/services/discord/commands/text_commands.rs
- src/services/queue/herdr_stop.rs
- src/services/queue/cancel_effects.rs
- src/services/queue/http_force.rs
- src/services/discord/herdr_terminate/effort_restart.rs

대상 시험: stop_gap_cancels_continuation_keeps_waiting_user; text_stop_gap_same_contract; http_cancel_gap_same_contract; clear_prevents_old_continuation_revival; stale_terminate_keeps_successor_continuation; terminate_matching_operation_cancels_pending_c.

수정 전 RED 계약: 현 main은 NoActiveTurn 공백에서 effort debt를 모른다. 새 hook 없는 시험 RED와 stale Terminate 대조 GREEN을 함께 둔다.

합계: 리뷰 6조각, 고유 prod 60파일 / 예상 순증 +4040. 가장 큰 S4는 15파일/+1050으로 PLAN 정지선보다 작다. 실제 변경이 한 조각 +1200에 닿으면 구현 전에 다시 분할한다. 문서·상태 모델은 이 prod 합계에서 제외한다.


## 상태 차트

# Effort 변경 즉시 재기동 상태 차트

코드 기준은 `193a57168f`다. prod 변경·cargo·Rust 시험·운영 설정/DB/세션 접근 없이 기존 `TmuxSession.tla`에 `Effort*Spec` 투영을 추가했다. `InputPath.tla`, `WriterWake.tla`를 먼저 읽고 소유 원본 보존·완료 wake·게시 ack debt 계약을 가져왔다. 기존 TmuxSession cfg의 예상 위반 세 개는 그대로다. 새 cfg 15개의 기대값만 `expected.tsv`에 추가했으며 기존 baseline 행은 바꾸지 않았다.

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
| `NoNotice` | 142 | RED: EffortInterruptedNoticeExactlyOnce, EffortInterruptedEventuallyTold | [report](effort-restart-NoNotice.report.md) |
| `DuplicateNotice` | 334 | RED: EffortInterruptedNoticeExactlyOnce | [report](effort-restart-DuplicateNotice.report.md) |
| `Repost` | 448 | RED: EffortReplyOnce | [report](effort-restart-Repost.report.md) |

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
| Repost: 동일 old body를 resume 뒤 재게시(6단계) | 기존 replay가 acked record만 제외함을 확인했으며 실제 main에서 duplicate가 발생함을 확인한 것은 아님 | 설계 guard mutant. live 반례로 인계하지 않음 |

## 범위 밖과 외부 가정

1. `EffortSameSession`의 sameSession=TRUE는 고정한 strict launch port 입력이다. wrong-session launch를 탐지했다는 증거가 아니다. 실제 provider session/thread equality, account, exact execution nonce, binding pin은 코드 수용 시험이 검증한다.
2. lifecycle 진입 전 capability=`supported`라는 oracle를 가정한다. `unavailable`/`unknown`에서는 설정 저장·중단을 하지 않는 preflight 계약은 별도 classifier/adapter 시험 대상이다. 특히 Claude ultracodeAvailable을 실제 TUI에서 얻는 adapter의 기술 검증을 대신하지 않는다.
3. EffortResume은 same session + new binding + ready + 검증된 launch 인자/capability를 축약한다. `revision`은 launch에 실은 최신 요청 번호다. 실제 model effort active 증거는 가정하지 않는다. Codex fresh `turn_context.reasoning_effort`는 첫 입력(C 또는 user) 뒤 비동기로 확인하며 Resume/C 선행조건으로 기다리지 않는다. Claude 별도 probe의 active 값도 재기동 incarnation active 증거가 아니다.
4. confirmed end와 durable cutoff를 얻기 전 kill 실패·unknown host는 모델의 EffortInterrupt에 진입하지 않는다. 이 안전 경계, dcserver crash가 end/cutoff/notice/continuation 기록 사이에 오는 경우, opaque MayHaveSent 입력의 원본 복구, 실제 queue 인덱스/재정렬, native transcript cutoff 소비는 구현 acceptance가 담당한다. 모델이 영속 write의 crash 원자성을 증명한 것은 아니다.
5. input 값은 person 원본 두 개와 독립 synthetic continuation이다. 기존 Routine/HttpStart/재시도 lineage 전체의 동시 병합·외부 입력은 모델 밖이다. continuation의 idempotency ticket이 반복 RPC/worker crash에서도 유지되는지는 owner store 시험이 담당한다.
6. 성공한 provider 작업·notice/output 전송이 공정하게 끝나고 변경 요청은 유한하다고 가정한다(WF). 영구 네트워크 장애·무한 effort 변경·영구 start 실패에 무조건 eventual processing을 주장하지 않는다. Failure cfg는 입력을 durable hold하고 실패를 보이는 것으로 멈춘다. Retry action은 외부 재요청의 추상화이며 새 자동 retry loop 설계가 아니다.
7. output의 old debt는 one durable body/ack의 축약이다. parser와 archive/rotation/owed cutoff 전부를 다시 증명하지 않는다. 기존 WriterWake/replay 계약을 소비하되 POST 완료를 physical resume의 선행조건으로 넣지 않았다.

## 재실행과 원시 증거

```sh
unset TMUX TMUX_PANE
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
~/.claude/skills/statechart/scripts/statechart docs/statecharts/session-lifecycle/TmuxSession.tla --config docs/statecharts/session-lifecycle/EffortRestartDesign.cfg --view docs/statecharts/session-lifecycle/effort-restart.view.mjs --out /tmp/i8109-effort-new
python3 scripts/ci/statecharts_tlc.py --base 193a57168f --head HEAD --jobs 2
```

cfg별 raw evidence는 `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8109/sc-out/<Name>-final2/{tlc.out,tlc-<PROPERTY>.out}`와 `logs/sc-<Name>-final2.log`다. legacy는 `sc-out/legacy-tmux-final`, `logs/sc-legacy-tmux-final.log`다. 초기 렌더 label 오류는 `*-r2`, `*-checked` 등 이전 폴더에 별도로 남고 위 최종 결과에 섞지 않았다. 원시 TLC의 clean finish/temporal checking을 report 생성 도구가 검사했으며 최종 CI expected match는 커밋 후 루트가 실행한다.

모델링 시작 10:04 KST, 최종 표/문서 완료 시각은 별도 lane 보고에 남긴다. 30분 제한 내 멈춘다. Rust 시험·prod 빌드·GUI 조작은 실행하지 않았다.


추가 구현 경계: capability/실제 applied 관측, Codex first-turn context 지연, snapshot 원자 prepend/queued-bind 재작성, /stop NoActiveTurn 공백 취소는 구체적인 진입점 수용 시험의 책임이다. 이 작은 lifecycle 투영의 GREEN을 해당 구현의 실행 검증으로 해석하지 않는다.

## 진입점 전수표와 CLI 근거

아래 두 부록은 동일 기준 SHA의 읽기 조사다. 코드 분기 대조는 실행 RED가 아니다. 최신 설계 순서·정책은 위 본문이 정본이며, 부록의 초기 순서 권고보다 우선한다.

### CLI·명령·launch 전수 조사

# #8109 CLI·명령·launch 조사

기준: worktree HEAD `193a57168f`, 2026-11-12 KST. 읽기 전용 조사이며 prod·운영 설정·runtime·DB를 읽거나 바꾸지 않았다. CLI 실행은 `--version`, `--help`만 사용했다. 외부 보고 파일만 작성했다.

사용자 최신 확정 결정(루트 전달): 끊긴 턴이 실제로 있을 때 같은 세션 재기동 뒤 자동 `이어서 진행` 프롬프트를 한 번 보낸다. 원본 재전송은 하지 않는다. ultra는 다른 effort를 고를 때까지 채널 단위로 유지한다. Codex 미지원 ultra는 거절하고 모델을 유지한다.

## 선행 이슈와 현재 차이

- #8109 OPEN, CLI 양쪽 ultra·Codex 채널 설정·진행 중 턴 즉시 중단·같은 세션 재기동이 요구됨.
- #2882 CLOSED: 당시 Claude ultracode 인자는 거절, Codex xhigh 최대라는 근거로 미지원 처리가 착지했다. 지금 CLI와 다르다.
- #6486 CLOSED 및 PR #7690 merge `5f6e988cf2a70707f47aa4e6e4ca6a92bd06c81a`: 로컬 Claude 설정 저장 후 다음 새 턴에만 같은 세션 재기동. UI 키를 치지 않고 `--effort`, `--model`, launch settings fastMode를 전달한다. 진행 중 턴은 계속 진행한다.
- #7940 CLOSED: Herdr는 재기동 경로가 아니라 live idle pane의 /effort selector와 native /fast passthrough 경로를 추가했다.
- #7984 MERGED: Claude 2.1.319 화면의 `Ultracode off`, `Tab to toggle`를 포함하는 effort overlay 라벨 파싱을 고쳤다. ultra를 Discord 선택지에 추가한 변경은 아니다.

## CLI 직접 근거

`codex --version` → `codex-cli 0.167.2`; `claude --version` → `2.1.322 (Claude Code)`.

Codex 도움말: `-c/--config key=value` TOML override, `-m/--model`, `resume` 지원. Claude 도움말: `--effort low|medium|high|xhigh|max`, `--settings JSON-or-file`, `--resume`, `--dangerously-skip-permissions`. 도움말만으로 ultra 지원 여부는 판정할 수 없다.

설치 바이너리의 내장 정적 소스/메타데이터를 읽었다(설정 파일·models_cache.json 안 읽음):

- Codex binary: `/opt/homebrew/lib/node_modules/@openai/codex/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex`
- SHA256 `97bc82fd46f117cf10efb95e6ff0c81dc4784f5797bcdc2cb75093ba09e4def0`
- byte offset `208587612`부터 JSONDecoder.raw_decode로 읽은 `models` JSON 길이 539087 bytes.
- `supported_reasoning_levels`와 `default_reasoning_level` 직접 추출 결과:

| model slug | 기본 effort | 지원 값 |
|---|---|---|
| gpt-6-astra | low | low medium high xhigh max ultra |
| gpt-6.1-sol | low | low medium high xhigh max ultra |
| gpt-6.1-terra | low | low medium high xhigh max ultra |
| gpt-6-sol | medium | low medium high xhigh max ultra |
| gpt-6-luna | medium | low medium high xhigh max |
| gpt-5.6-sol | low | low medium high xhigh max ultra |
| gpt-5.6-terra | medium | low medium high xhigh max ultra |
| gpt-5.6-luna | medium | low medium high xhigh max |
| gpt-5.5 | medium | low medium high xhigh |

숨김 모델 daybreak-blue/red도 ultra 지원, codex-auto-review는 max까지이나 채널 UI에는 visibility=list만 사용한다. ultra description은 `Maximum reasoning with automatic task delegation`.

Claude binary: `/Users/itismyfield/.local/share/claude-stable/claude`, SHA256 `a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450`.

| binary byte offset | 확인 내용 |
|---|---|
| 261539158 | `ultracode` boolean settings schema: standing dynamic workflow orchestration at any effort, session scoped, --settings/apply_flag_settings; interactive toggle는 비영속 |
| 265635243 | `Sm() = GOs()===undefined`; workflow runtime·managed/org policy·available·enableWorkflows로 실제 동적 워크플로 가용성 계산 |
| 265657348 | `NVe(model)`은 model capability/caps/cache·xhigh_effort 지원을 계산; 단순 모델명 표와 같지 않음 |
| 265657956 | `Ue(model)=NVe(model)` |
| 265657985 | `vF(model)=Sm() && (model===undefined || Ue(model))`; `Pg(requested,model)=requested===true && vF(model)` |
| 286059868 | `/effort ultracode on` 처리: workflow false면 `Ultracode needs dynamic workflows enabled (see /config)`; 모델 false면 `Ultracode isn't available on <model>`; on/off는 effort 자체를 유지 |
| 286060506 | 파서는 `ultracode`, `ultracode on`, `ultracode off`만 허용 |
| 263621534 / 263622018 | applied runtime schema `{model,effort,ultracode,ultracodeRequested,ultracodeAvailable}` |
| 302364442 | control `get_settings`가 위 applied 상태를 직접 응답하며, ultracodeAvailable은 vF(current model) |
| 274835366 | statusline raw JSON에는 `effort:{level:...}`만, ultracode/ultracodeAvailable 없음 |
| 274587817 | 공통 hook payload Ip에도 session/transcript/cwd/permission/agent/effort만, ultracodeAvailable 없음 |
| 280836700 | 실제 active는 bridge metadata `is_ultracode_active`로 보고 가능; 일반 statusline 필드가 아님 |

### Claude capability 관측의 미해결 지점

현행 AgentDesk statusline wrapper는 `session_id/rate_limits/model/effort/fast_mode/written_at`만 저장한다(`claude_tui/hook_bundle/statusline.rs:31-34`). upstream raw statusline 자체에도 ultra 가용성·실제 active가 없으므로 wrapper에 이름만 추가해도 증거가 생기지 않는다. `get_settings` control response는 정확한 applied 값을 제공하나 현재 hosted TUI에 그 control transport를 연결한 AgentDesk 경로는 조사 범위에서 확인하지 못했다.

`--settings {"ultracode":true}`는 요청값 전달 근거이고, `Pg(true,model)`가 false일 수 있어 실제 적용의 증거는 아니다. `--effort xhigh`나 응답 effort xhigh 또한 ultracode active의 증거가 아니다. Live overlay 열기 금지 설계라면 (1) control transport의 read-only get_settings 연결, 또는 (2) 완전히 격리된 같은 launch context의 control probe 후 새 실제 incarnation에서 active 증거 확인이 필요하다. (2)는 운영 계정 조건·feature flag 동일성/부작용·신선도 확인이 추가로 필요해 아직 구현 처방으로 확정할 근거가 없다. available 미확인은 true로 추정하지 않고 `ultracode 사용 가능 여부를 확인하지 못해 변경하지 않았습니다`로 거절해야 한다. workflow false / model false가 직접 확인된 경우 각각 이유를 명시한다. 이 조사에서는 control CLI 실행이나 기존 세션 접속을 하지 않았다.

## 명령·설정 전수표

아래 경로는 모두 `src/services/` 기준이다.

| 책임 | file:line | 현재 행동 |
|---|---|---|
| Discord 등록 | discord/runtime_bootstrap/framework_setup.rs:210,213,216 | cmd_model / cmd_fast / cmd_effort 등록 |
| command export | discord/commands/mod.rs:96,100,113 | fast/model/effort 노출 |
| /effort handler | discord/commands/tui_passthrough.rs:309 | auth→effective provider→defer→save_notice |
| effort 선택지 | discord/commands/tui_passthrough.rs:8-31 | low..max 정적 enum |
| Codex effort 거절 | discord/commands/tui_passthrough.rs:96-100,125-126 | 채널 설정면 미구현 안내 |
| Herdr effort | discord/commands/tui_passthrough.rs:128-143 | TuiHosting일 때 live overlay 적용 |
| tmux effort | discord/commands/tui_passthrough.rs:145-151 | 저장 후 다음 턴 재기동 알림 |
| /fast | discord/commands/fast_mode.rs:54,97-142 | Claude·Codex 공통 지원, feature parent scope 사용; Herdr Claude는 native 즉시 적용 |
| /model | discord/commands/model_picker.rs:194 | 모델 picker 카드 진입 |
| model 저장 | discord/model_picker_interaction.rs:49-94 | owner 검사→override save→pending 상태에 따른 알림 |
| model pending 알림 | discord/model_picker_interaction.rs:109-112 | 다음 새 턴 같은 세션 재기동 |
| feature 설정 key | discord/commands/config/provider_settings.rs:24-45,97-107 | legacy thread fast/goals override 있으면 thread, 없으면 parent |
| 설정 owner | discord/commands/config/provider_settings.rs:110-167 | execution owner local/foreign/stale/conflict 판정 |
| model 저장 | discord/commands/config/provider_settings.rs:196-258 | candidate 저장 성공 후 in-memory publish; Claude tmux relaunch pending, 다른 경로 reset 정책 |
| effort read | discord/commands/config/provider_settings.rs:261-281 | Claude만, channel_effort_levels raw channel key, low..max whitelist |
| effort save | discord/commands/config/provider_settings.rs:285-311 | Claude local tmux만, candidate try_save 후 relaunch pending |
| fast save | discord/commands/config/provider_settings.rs:314-375 | provider별 pending key, candidate try_save 후 overrides |
| settings 구조 | discord/mod.rs:906-908,937-938 | channel_relaunch_pending set + channel_effort_levels map |
| effort restore | discord/settings/read.rs:127-138,308-309 | 저장 string을 복원(읽기 시 whitelist와 구분됨) |
| effort persist | discord/settings/write.rs:461-471 | pending + effort를 같은 저장면에서 직렬화 |
| boot | discord/runtime_bootstrap.rs:215-216,273 | load_bot_settings→shared bot_settings |
| audit 보존 | discord_config_audit/bot_settings.rs:59-60,93-104; legacy_bot.rs:113-130 | legacy pending/effort를 이관·보존 |
| 기존 거절 계약 시험 | discord/commands/tui_passthrough.rs:349-355 | ultracode 제외=정적 선택지 low..max |
| effort relaunch 시험 | discord/commands/control/relaunch_effort_tests.rs:51,114,144,183-202,209 | same-session/clear/미지원·ultracode/Codex 거절/실제 effort 확인 |

Codex effort 설정은 `/fast`의 candidate-save·owner·thread scope 계약을 재사용하되, 기존 Claude raw channel key의 의미를 무의식적으로 parent 상속으로 바꾸면 안 된다. 현행 feature scope가 fast/goals 존재만 검사하므로 effort-only thread override도 별도 인식하도록 설계해야 한다. 저장 문자열을 같은 map에서 재사용할 경우 provider 변경 때 이전 provider effort가 새 provider에 적용되는지 명시적으로 검증해야 한다.

## 모델 지원 값의 근거·운영 전제

| 책임 | file:line | 근거 |
|---|---|---|
| 모델 우선순위 | discord/commands/config.rs:78-91 | runtime override > dispatch role > role-map > provider default |
| model snapshot | discord/commands/config.rs:236-270 | 실제 turn이 읽는 동일 모델 계산 |
| model for turn | discord/commands/config.rs:293-305,329-335 | default일 때 provider runtime_model, 그 외 명시 모델 |
| Codex default | domain/provider/catalog/rows.rs:38-65 | runtime_model=None, 기본 모델명 코드에 고정되어 있지 않음 |
| -m 생성 | codex.rs:432-436 | 명시 모델이 있을 때만 -m 전달 |
| Codex catalog | discord/model_catalog.rs:99-154 | fallback은 과거 gpt-5.x 목록이며 지원 effort 정보 없음 |
| dynamic catalog | discord/model_catalog.rs:232-244,444-482 | cache schema는 slug/display_name/visibility만 읽어 effort 정보를 버림 |
| default resolver | discord/model_catalog.rs:782-789 | Codex default model을 해석하지 않고 None |

운영 설정 금지 아래 특정 운영 채널이 Astra/Sol인지 확정할 수 없다. 코드가 모델을 고정하지 않으므로 실제 채널 모델 지원을 `모두 ultra 지원`으로 판정하면 안 된다. 변경 command는 turn과 같은 effective_model_snapshot을 고정하고 해당 모델의 provider-native 지원 목록을 받아 교집합을 만든다. 명시 모델/지원 metadata가 없거나 default alias가 정확히 해석되지 않으면 ultra를 거절하고 현재 모델을 유지한다. 기존 catalog cache parser에 `supported_reasoning_levels`를 보존하는 child helper를 추가하는 방법이 작지만, cache가 해당 launch account의 것인지와 absent metadata 정책은 구현 시험으로 고정해야 한다. 모델 capability를 주어진 slug 목록으로 추측하거나 `max`까지 전 모델 공통이라고 가정하지 않는다.

## launch·resume·incarnation 전수표

| 책임 | file:line | 현재 동작 |
|---|---|---|
| pending 소비 | discord/commands/control.rs:225-383 | 다음 turn reset 계획에서만 relaunch; Herdr로 이동한 pending은 release+안내 |
| relaunch 검증 | discord/commands/control/relaunch.rs:63-77,136-161,232-251 | target session/cwd/auth profile/transcript 보존 증명 |
| relaunch 실행 | discord/commands/control/relaunch.rs:255-286 | TUI 종료 후 다음 launch가 resume; 별도 즉시 boot 아님 |
| busy defer | discord/commands/control/relaunch.rs:288-293 | 진행 중 턴을 끊지 않음 |
| kill 재시도 | discord/commands/control/relaunch.rs:296-315 | 2회 실패 후 다음 launch 적용 안내 |
| tmux relaunch | discord/commands/tmux_recreate.rs:80-109 | readiness busy이면 Deferred; host guard→clear_channel_session→kill/cleanup→auth restore |
| host clear | discord/commands/tmux_recreate.rs:26-46 | namespaced key와 session row로 입장 |
| 재기동 후 확인 | discord/commands/control/relaunch_confirm.rs:11-26,73-91,113-124 | old process 종료 시각/offset 이후 statusline 및 첫 응답, session 불일치 거절 |
| Claude launch settings | claude_tui/session.rs:275-281,326-358 | model/fast/effort; ultracode field 없음 |
| Claude argv | claude_tui/session.rs:367-396,414-416 | --dangerously-skip-permissions, --resume ID, --settings hook file, --model, --effort |
| Claude env | claude_tui/session.rs:464-466 | 명시 effort면 CLAUDE_CODE_EFFORT_LEVEL unset |
| Claude launch settings JSON | claude_tui/hook_bundle.rs:128-144 | hooks+fastMode 한 파일; 기존 --settings를 대체하면 hooks 잃음 |
| Claude tmux prepare | claude/tui_session_launch.rs:12-119; prepared.rs:37-67 | PreparedIncarnation 후 파일 생성, fence 아래 pane create/respawn |
| Claude same session 선택 | claude/tui_session_launch/prepared.rs:97-129 | 요청 transcript 존재면 resume, 없으면 fresh substitution이므로 즉시 effort는 kill 전 strict check 필요 |
| Claude Herdr prepare | claude/tui_session_launch.rs:154-186 | expected session과 resume를 비교; 현재 model만 전달, fast/effort 전달 없음 |
| Codex env default | codex.rs:232-235 | AGENTDESK_CODEX_REASONING_EFFORT 읽기 |
| Codex options | codex.rs:147-158,170-188 | resume/model/reasoning_effort 저장 |
| Codex config argv | codex.rs:417-437 | -c model_reasoning_effort="<value>", -m model |
| Codex exec resume | codex.rs:485-496 | exec resume ID |
| Codex wrapper | codex.rs:332-358,499-516 | env effort→--reasoning-effort, --resume-session-id |
| Codex tmux TUI resume argv | codex/tui_session_launch/launch_policy.rs:147-164 | resume 먼저, common options, ID 마지막 |
| Codex warm/absence launch | codex/absence_launch.rs:121-138,171-198,326-395 | env effort, same session selection/preflight, new incarnation, launch, resume proof |
| Codex process launch | codex/process_session_launch.rs:110 | env effort로만 시작 |
| Codex Herdr launch options | codex/herdr_turn.rs:174-185 | turn.model + env effort; channel effort 없음 |
| Codex Herdr identity | codex/herdr_turn.rs:188-195 | expected_native_session_id=None, resume=false |
| Codex Herdr fresh guard | codex/tui_session_launch/launch_policy.rs:178-182,210-214,229-260 | resume launch 거절(다음 단락 참조) |
| Codex warm fingerprint | codex_tui/warm_followup.rs:125-126 | reasoning_effort도 launch fingerprint에 포함 |
| wrapper default | codex_tmux_wrapper/input.rs:279-290 | explicit effort 없고 model 지정이면 high, 양쪽 없으면 None |
| incarnation 새 nonce | tui_prompt_dedupe/binding_context.rs:333-363 | 새 UUID execution_nonce, expected_native_session_id, launch_mode resume/fresh |
| incarnation durable | tui_prompt_dedupe/binding_context.rs:366-401 | temp write/fsync/hard-link/parent sync 후 immutable publish; verified Codex archive |
| Herdr incarnation | herdr_launch.rs:463-494,729 | command 생성 전 durable evidence; stamp spawn markers |

### Codex Herdr resume는 별도 구현 의존

현재 CodexHerdrTurn에는 native session_id 자체가 없고, fresh launch만 있다(`herdr_turn.rs:174-195`). `prepare_codex_herdr_launch`는 준비 전·후 모두 `validate_herdr_fresh_argv`를 호출한다. 이 validator는 `mode != fresh`, resume session이 있음, argv command가 resume 중 하나면 거절한다(`launch_policy.rs:256-260`). 시험 `herdr_fresh_argv_rejects_resume_without_rejecting_option_values`(fresh_tests.rs:4), `codex_herdr_prepare_rejects_resume_before_launch_effects`(:67)가 이를 고정한다.

따라서 #8109 Herdr Codex 같은 thread 재기동은 launch option 하나 배선으로 끝나지 않는다. 기존 fresh 경로 계약은 유지하고, 검증된 native thread·account root·expected session·새 execution nonce를 받는 explicit resume launch 경로를 child module로 추가해야 한다. blanket validator 완화는 잘못된 세션 resume·fresh 경로 변경을 함께 허용하므로 금지한다. 이 별도 조각의 RED 대상은 `resume context accepts exact native ID / rejects another ID before pane I/O / fresh validator unchanged / resume launch same thread`이다.

## 제안 시험 계약

- `truthful_effort_choices_exclude_rejected_ultracode`를 provider/model capability로 거른 choices 시험으로 대체: Claude available true→ultra 포함, false/unknown→제외, Codex Astra→ultra·Luna→max·gpt5.5→xhigh. Discord 정적 ChoiceParameter는 모델별 선택지를 못 거르므로 String autocomplete 또는 picker로 옮기고 저장 때 같은 snapshot으로 재검증한다.
- `relaunch_effort_tests.rs:183-202`는 unknown 문자열 거절을 유지하되 `ultracode` invalid raw 값 거절과 canonical ultra·Codex 지원 계약을 나눠 고친다. raw unknown은 launch하지 않는다.
- 실제 CLI flag/model effort·same native ID·새 execution nonce 증거를 시험한다. ultra는 Claude effort xhigh와 동치가 아니므로 xhigh 응답으로 active 성공 판정 금지.
- Claude launch는 기존 generated --settings JSON에 ultracode true/false를 합성한다. 단독 --settings JSON으로 기존 hooks/fastMode를 덮어쓰지 않는다. 다른 effort 선택은 explicit false를 써 inherited/stored request를 끈다.
- Codex launch는 채널 effort > env 기본 > CLI 기본 순서. wrapper의 pinned model high fallback이 채널 effort를 덮지 않도록 시험한다.
- Herdr Bound warm fingerprint는 바뀐 effort를 기존 pane followup으로 보내지 않고 재기동 계획으로 보내야 한다.
- post-launch applied 확인은 requested 전달과 구분한다. Claude ultracodeAvailable 관측 경로는 아직 실증 필요; 확인 불가를 성공이나 정책 거절 사유로 꾸미지 않는다.

## capability spike: local stream-json control probe 후보

추가 바이너리 코드 검토(실행하지 않음):

| byte offset | transport/초기화 근거 |
|---|---|
| 268728292 | get_settings가 import하는 `Nm(session)`은 `wBr({hostedWorker:yHn(session),transportPersists:...})` |
| 262044412 | `wBr`는 hostedWorker 또는 remote Gh()이면서 persistent transport일 때 true; local nonhosted stdio는 이 거절 조건에 들지 않는 구조 |
| 302292788 | structured input for-await loop가 `type=control_request`를 user input과 분리해 처리 |
| 302295311 | subtype initialize 분기; hook/sdk server/skills 필드 유효성 검사 뒤 jX 호출 |
| 302400427 | initialize 함수 jX는 command/model/SDK metadata를 초기화·응답하는 경로 |
| 302286477-302286630 | initialized flag Y와 ke() 완료 처리 |
| 302364442 | get_settings 핸들러는 current state·model·effort·vF capability를 읽어 Dt 응답. 이 핸들러에 user enqueue나 model query 호출 없음 |
| 302382402 이후 | user/assistant/system 프레임 처리 경로는 control 분기와 구분 |

코드상 후보 프로토콜은 새 로컬 `claude -p --input-format stream-json --output-format stream-json` 프로세스에 user frame 없이 initialize control frame을 보내고, 성공 응답 후 get_settings control frame만 요청하는 것이다. CLI help에 input/output stream-json이 있고 control loop에 get_settings가 있으므로 **모델 요청을 만들지 않는 설정 관측 경로 후보**로는 근거가 있다. 이 레인에서는 user 금지에 따라 --help/--version 외 실행하지 않았다.

그러나 startup hooks/MCP/plugin sync/cron wake 등 launch 부수효과가 전혀 없다는 것, 출력형(--print) 컨텍스트에서 동적 워크플로 계산이 운영 TUI와 완전히 동일하다는 것, feature·auth·policy 신선도 및 probe가 실제 채널 프로세스 조건을 정확히 복제한다는 것은 미실증이다. `--bare`는 필요한 사용자 설정·훅·키체인 등을 생략하므로 parity 확보 수단으로 곧바로 쓰면 안 된다. 실기 없는 설계에서 `get_settings print probe가 이미 안전하고 정확하다`고 선언하지 않는다.

기술 spike 수용 기준: 동일 account/cwd/env/model의 격리 fixture에서 initialize→get_settings만으로 applied fields 수신, user/assistant/request API usage 0, hook/스케줄/플러그인 부수효과 범위 확인, 실제 hosted TUI의 workflow on/off·model support 결과와 교차 확인, stale/timeout/error→Unknown 처리, Unknown이면 stop/kill/settings save가 모두 0. 동적 워크플로 off·미지원 모델 false일 때 이유를 정확히 안내하고, 둘을 분리할 증거가 없으면 이유를 추측하지 않는다. 이 spike를 통과하기 전 ultra 명령을 가용하다고 노출하거나 launch 성공으로 표시하지 않는다.


### 재기동·입력·출력 전수 조사

# #8109 재기동·입력·출력 조사

기준: `193a57168f` worktree 읽기 전용. prod 수정, cargo, 시험, 운영 설정·runtime·DB 접근 없음. 아래는 코드로 확인한 사실과 설계 권고를 구분한다. 최신 사용자 결정: A/B(원문 재전송 여부) 비교는 폐기. 실제 진행 중 턴이 끊겼을 때에만 같은 세션 재기동 성공 뒤 고정 AgentDesk 자동 입력을 한 번 보낸다. 자동 입력을 사용자 FIFO보다 먼저 처리한다. ultra는 채널 설정으로 유지하고 미지원 모델 요청은 거절하며 모델은 바꾸지 않는다.

## 진입점 전수표

| 책임/진입 | 기준 좌표 | 확인한 계약 |
|---|---|---|
| `/effort` tmux 저장 | `src/services/discord/commands/tui_passthrough.rs:118`, `:146`, `:309` | Claude 전용. tmux는 다음 새 턴 때 relaunch를 약속하며 현재 턴을 종료하지 않음. Herdr는 live setting 별도 경로 |
| 새 턴의 pending relaunch | `src/services/discord/commands/control.rs:225`, `:243`, `:284`, `:302` | provider Claude만 pending을 소비. Herdr로 이동했으면 relaunch pending을 지움. 현재 채널 model/effort를 Requested에 넣음 |
| 기존 same-session 재기동 | `src/services/discord/commands/control/relaunch.rs:244`, `:271` | session ID·cwd 존재와 같은 계정 transcript 재개 가능성 먼저 확인. 성공 시 다음 launch가 같은 session resume을 수행 |
| busy guard·physical kill | `src/services/discord/commands/tmux_recreate.rs:93`, `:99`, `:104`, `:115` | busy면 Deferred. host admission 후 kill_and_clean, 계정 마커 복원. session ID·DB selector 보존. 즉시 재기동은 busy guard 제거만으로 구현할 수 없음 |
| 기존 실패 정책 | `src/services/discord/commands/control/relaunch.rs:291`, `:306` | busy는 다음 턴 미룸, kill 2회 실패 후 pending 해제. 즉시 재기동 사용자 계약과 다름 |
| 기존 `/restart` | `src/services/discord/commands/restart.rs:216`, `:223`, `:237` | host refusal 먼저, inflight 경고·cancel, resume 지원 provider는 ID를 유지해 reset 후 seed turn. seed 자동입력 책임까지 있으나 generic reset은 verified Codex/Herdr 제한과 새 marker 계약을 그대로 따라야 함 |
| `/stop` slash | `src/services/discord/commands/control.rs:617`, `:637`, `:668`, `:682`; `commands/stop.rs:84` | 원격 holder cancel forwarding/revalidate 이후 begin_user_stop. acknowledgement 뒤 finish에서 실제 stop 후 queue consequence |
| text `!stop` / `!clear` | `src/services/discord/commands/text_commands.rs:20`, `:102` | 같은 begin_user_stop·clear 본체를 사용. 자동 continuation 중 이 입력이 들어와도 기존 stop 정책을 넓히지 말 것 |
| HTTP 일반 Cancel | `src/services/queue/herdr_stop.rs:12`, `:24`; `src/services/queue/cancel_effects.rs:78` | nonforce live Herdr는 holder `/stop`; confirmed publish 뒤 기존 queue drain. 강제 cancel은 별도 purge 계약 |
| HTTP force Cancel | `src/services/queue/http_force.rs:31`, `:52`; `src/services/queue/cancel_effects.rs:256` | 선택한 Herdr 실행을 terminate exclusion 안에서 retire하고 큐 purge하는 사용자 의도. effort에서 호출하면 큐 보존 계약 위반 |
| HTTP Terminate | `src/server/routes/herdr_terminate.rs:40`, `:93`; `src/services/discord/herdr_terminate.rs:401`, `:679` | exact execution nonce 대상 종료. 이 서비스의 scoped core만 재사용 가능하며 force cancel/dispatch fail/retry postlude는 붙이지 않음 |
| physical Herdr terminate core | `src/services/discord/herdr_terminate.rs:437`, `:453`, `:478`, `:486`, `:568`, `:580`, `:600` | row/owner/nonce 일치→captured token/actor→transition→fence→row 재확인→end 관찰→retire CAS→hold 해제→captured finalizer. successor를 잘못 끊지 않도록 identity를 매 단계 고정 |
| Herdr turn settlement | `src/services/discord/herdr_terminate.rs:314`, `:337`, `:355`, `:382` | release_hold는 exact execution nonce, finalizer는 captured TurnKey/episode nonce/snapshot. AlreadyFinalized도 exact token이 실제 해제됐는지 다시 확인 |
| clear teardown | `src/services/discord/commands/control.rs:510`, `:521`, `:550`, `:561`, `:574`; `control/clear_teardown.rs:70` | queue discard, native session memory clear, provider selector forget. effort에서 clear 본체 재사용 금지 |
| `/clear` Herdr | `src/services/discord/commands/control/clear_teardown.rs:138`, `:192` | terminate의 after_retire 안에서 selector를 지워 successor 경합 방지. effort는 selector를 지우지 않는 새 postlude 필요 |
| legacy managed process 종료 | `src/services/discord/commands/control/managed_reset.rs:75`, `:110`, `:146` | runtime hint fence 아래 process handle/tree + exact tmux end, kill 확인 후 temp cleanup. 임의 name kill 권한을 새로 만들면 안 됨 |
| verified Codex reset refusal | `src/services/discord/commands/control/managed_reset.rs:13`, `:45`, `:60` | verified incarnation output proof 전 reset 불가. effort라는 이름으로 우회하지 말고 종료·writer proof 연동 수용 시험 필요 |
| process resume launch | `src/services/claude/tui_session_launch.rs:12`, `:154`, `:166`; `src/services/codex/tui_session_launch.rs` | Claude Herdr prepare는 context.launch_mode=resume 및 expected_native_session_id 일치 요구. 새 native session으로 fallback하는 즉시 restart 성공 보고 금지 |
| Herdr 최초 launch+첫 입력 | `src/services/claude/herdr_turn.rs:234`, `:590`, `:617`, `:665`; `src/services/codex/herdr_turn.rs:124`, `:174`, `:188` | launch/SessionStart/confirm_bound 뒤 gated 첫 prompt. unclear send는 hold. 재기동을 위해 이 경로를 타더라도 continuation original은 별도 custody를 유지 |
| 부팅 시 Herdr 재획득 | `src/services/discord/recovery_engine/herdr_reader.rs:220`, `:228`, `:506` | Bound Herdr row read/reconnect→existing source attach. effort pending debt도 attach 성공과 구분해야 함 |
| 부팅 시 watcher 재획득 | `src/services/discord/watchers/lifecycle/restore_retry.rs:83`, `:93`; `runtime_bootstrap/recovery_flush.rs:55`, `:73` | recovery handled channel skip으로 duplicate watcher 방지. restart local command도 같은 registry fences 보존 |
| stream active inflight 재생성 | `src/services/discord/tmux_watcher/liveness.rs:462`, `:473`, `:486`; `tmux_watcher/loop_poll_prologue.rs:483`; `tmux_watcher/streaming_status_tick.rs:268` | turn-mode 채널은 synthetic reacquire 안 함. legacy active stream은 recovery gate + save-if-absent. old watcher가 restart 후 원치 않는 inflight를 다시 만들지 않도록 exact generation 종료 필요 |
| watcher 종료 | `src/services/discord/watchers/lifecycle_decision.rs:113`, `:127`; `tmux_watcher/liveness.rs:102` | cancelled/shutdown이면 quiet stop, otherwise pane death로 TmuxDied. watcher instance_id는 successor lease와 구분 |
| idle 정리 | `src/services/discord/session_idle_cleanup.rs:32`, `:45`, `:73`, `:86`, `:158`, `:179` | watcher 있으면 expire 안 함. configured Herdr/active mailbox/unproven idle 제외. provider resume ID는 DB에 유지. effort 새 책임은 idle clear로 넓히지 않음 |
| 전환 완료·wake | `src/services/discord/session_transition.rs:120`, `:132`, `:145` | completion을 guard보다 먼저 선언해 guard drop 뒤 owner wake. 성공뿐 아니라 실패·early return도 wake 필요 |

## 같은 상태를 소비하는 입력·finalizer·writer·presence

| 상태 | 코드 좌표 | 설계 영향 |
|---|---|---|
| exact episode finalizer | `turn_finalizer.rs:141`, `:155`, `:270`; `turn_finalizer/finalize.rs:87`, `:170`, `:210` | TurnKey(channel,message,generation,episode nonce)와 captured actor를 사용. inflight/active token/count는 exact identity로 한 번만 닫힘 |
| HostTerminated | `turn_finalizer.rs:278`, `:299`; `turn_finalizer/finalize_context.rs:37`; `turn_finalizer/finalize.rs:203`, `:302`; `turn_finalizer/terminal_handler.rs:202`; `turn_finalizer/cleanup.rs:278` | mute: clear_inflight는 하되 cancelled flag·panel·normal queue publication을 생략. output 소유는 reader에 남음. 이 이벤트만 호출해도 사용자 중단 안내 1회나 continuation은 생기지 않음 |
| regular Cancel | `turn_finalizer/cleanup.rs:282`; `turn_finalizer/finalize.rs:198`, `:325` | Cancel은 aborted panel reconcile, normal completion reactions 억제 및 completion barrier 뒤 queue wake. effort 안내와 합쳐 중복 알림이 되지 않도록 effort reason의 단일 notice 소유자 필요 |
| output POST | `src/services/tui_o/writer/deliver.rs:1`, `:36`, `:40`, `:85`, `:125` | ownership+delivery lease 아래 fsync Prepared 후 POST, uncertain은 history reconcile. 기존 O ledger/posted frontier 유지. restart는 ledger 삭제·cursor 0 reset·새 legacy body 게시를 하지 않음 |
| O writer 소스 rotation | `src/services/tui_o/writer/rotation/clear_end.rs:125`, `:167`; `rotation/codex_end.rs:105` | Claude ClearEnd는 fresh launch만 증명. CodexEnd는 old→successor source hop 증명. same native session/source resume의 중단 boundary로 오용 불가 |
| parent turn facts | `src/services/tui_o/writer/input_facts.rs:20`, `:38`, `:90` | Unknown/Open/Idle; Closed aborted는 기존 interrupt native boundary. restart는 같은 parent source로 재구성 |
| stale Open presence | `src/services/discord/turn_presence/activity.rs:685` | native closer 없이 hard kill된 Open은 Busy 이후 Unknown(open_without_progress). 새 ready pane만으로 Idle되지 않음 |
| presence nonce | `src/services/discord/turn_presence/activity/unbound/live_ownership.rs:80`, `:176`, `:253`, `:307`; `unbound/binding_metadata.rs:107`, `:128` | spawn nonce/registry generation/source binding이 바뀌면 이전 measurement로 admission 못 함. 새 incarnation publish 필요 |
| queue front/custody | `src/services/turn_orchestrator/queued_head/v2.rs:26`, `:123`, `:369`, `:482`; `queue_io/input_owner/effect.rs:19`, `:105` | original/ticket/attempt를 owner가 보존. Held=정확히 no-key proof인 재시도 가능 front, WriteUncertain=may-have-sent로 자동 재전송 금지 |
| Held wake | `queue_io/input_owner/drive.rs:118`; `held_watch.rs:73`, `:104` | no-key hold는 real wake 대기. Gone pane은 watcher를 끝내며 wake하지 않음. restart completion 및 writer ready에서 owner wake가 반드시 필요 |
| legacy stop preserve | `src/services/turn_lifecycle.rs:96`, `:104`, `:123` | existing queue-preserving stop은 HotSwapHandoff restart mode로 inflight 보존. effort shutdown이 실제 CLI restart 전용일 때 dcserver hot-swap와 섞지 말 것 |

위 표에서 약칭 `turn_finalizer`, `queue_io`, `tmux_watcher`, `runtime_bootstrap`는 `src/services/discord/` 아래다.

## 입력 출처와 자동 continuation

- 사용자 입력: `queue_io/input_owner/origin.rs:6` InputOrigin::Discord / Imessage. `:27` is_person은 이 둘뿐이다. `turn_orchestrator/queued_head/v2.rs:62` person_input은 context 없음=Discord, origin=imessage 또는 source=imessage이며 routine_id/scheduled_message_id가 없는 headless를 사람으로 본다.
- 자동 입력: HttpStart / Routine은 machine. 자동 continuation은 `origin=headless`, `source=agentdesk_effort_continue`, explicit metadata에 old interrupted episode와 restart intent를 둔다. 사람의 request_owner로 가장하거나 Discord message를 만들지 않는다. 새 agent-private prompt를 사용한다.
- 재시도: `queue_io/input_owner/ingress.rs:39` existing_retry는 기존 ingress retry 표시다. `start.rs:12` original은 retry_original로 같은 original/generation/ticket을 복구한다. Pending은 Accepted/Consumed가 아니며 원본을 보존한다(`ingress.rs:50`).
- 권고 API: `admission.rs:85` admit_with_context + `:61` status + `:75` exact_turn. Accepted는 durable custody, Pending은 같은 ticket으로 재확인, Rejected는 caller debt 유지. 이미 Armed/Settled/receipt면 다시 주입하지 않는다.
- `routine.rs:267` synthetic_original은 turn_id 마지막 숫자를 message/source ID로 사용하고 `turn-{channel}-{message}` ticket을 만든다. **old user message ID를 재사용하면 source admission dedupe와 충돌한다.** episode nonce로 도메인 분리한 synthetic numeric ID 또는 기존 reserved synthetic source를 하나 발급하고 intent에 먼저 저장한다. ticket은 nonce까지 포함한 stable key로 저장하고 원본 generation은 retry_original로 복구한다.
- 고정 문구: `effort 변경 처리를 위해 세션을 다시 시작했으니 중단된 작업을 이어서 진행하라`. 원문·이전 첨부를 다시 보내지 않는다.
- 실제 끊김은 requested 순간 Busy만으로 확정하지 않는다. transition 안에서 captured old episode가 여전히 Open임을 확인하고, physical end를 성공 확인한 exact episode에만 continuation debt 생성/확정한다. 자연 완료가 먼저 이겼으면 0.
- 성공 전송 규칙: restart intent와 old episode 키를 먼저 durable 기록 → physical end/EOF cutoff 확인 → 같은 native ID의 new incarnation bind/ready 확인 → continuation ticket 단 한 개 admission → receipt로 완료. launch 실패는 전송 0, 나중에 같은 intent가 성공하면 1. consecutive effort가 같은 중단 episode를 가리키면 desired revision만 갱신하고 debt/ticket은 한 개로 coalesce한다.
- 순서 주의: current `v2/source_admission.rs:750`은 tail append다. `provider_dispatch/owner_input.rs:238-257`은 busy machine이 Held이면 `v2.rs:123` prefer_person으로 대기 사용자 입력을 앞으로 회전한다. 단순 machine admission만으로 continuation-first를 보장할 수 없다. effort pending continuation barrier를 owner front grant에 연결하고 continuation을 한 번 앞세우는 원자 commit에서 queued bind의 index도 함께 갱신해야 한다. 사용자끼리 FIFO, original identity, Held/Unknown custody는 유지한다. broad priority API나 운영자 override를 만들지 않는다.

## 코드로 대조한 반례 후보

1. **busy guard만 제거**: effort 저장→tmux_recreate busy 무시→kill/cleanup→old mailbox token/inflight 미정산→new user start hold. `tmux_recreate.rs:93-121`에 finalizer 호출이 없고 `control.rs:225` 호출 자체가 다음 turn context인 것을 코드로 확인했다. 수용 시험은 active turn interrupt+exact finalizer release+late complete 무해를 검증한다.
2. **clear 재사용**: clear 함수 호출→`control.rs:550` discard waiting backlog→`:561` memory clear→`:574` selector forget. 원래 사용자 대기 입력과 맥락 보존 계약을 직접 깨므로 배제.
3. **새 ready만으로 presence 복구 가정**: old source Open→hard end(no native Closed)→same session resume→`turn_presence/activity.rs:685-700` old Open이 여전히 Busy/Unknown. 정확 old execution end+EOF cutoff의 synthetic interrupt boundary를 별도 scoped witness로 소비해야 한다. 테스트에서는 old open with no closer + same source new incarnation에서 continuation과 user input이 진행하는지 확인한다. native transcript를 조작하지 않고 InputFacts/presence projection에 exact witness를 반영한다.
4. **Held의 pane Gone을 완료 wake로 간주**: Held front→old pane end→`held_watch.rs:73-80` End(no wake)→owner 영구 보류. restart completion drop wake와 writer-ready wake가 이를 닫아야 한다.
5. **자동 continuation에 old 사용자 source ID 사용**: synthetic_original derives old suffix ID→owner source dedupe가 이미 admitted/settled old input으로 판단. 별도 synthetic ID 및 stable intent key가 필요.
6. **자동 continuation을 그냥 enqueue**: user B가 restart 중 먼저 queued→machine continuation tail push→B가 먼저 실행됨. 또는 continuation Held→prefer_person이 B를 앞으로 옮김. scoped barrier/원자 prepend가 필요.
7. **취소와 reader를 같은 cancelled flag로 처리**: HostTerminated mute 계약은 reader output ownership 보존을 위해 token.cancelled를 건드리지 않는다(`finalize.rs:203`). regular Cancel을 무작정 붙이면 writer permit과 late body 처리에 영향을 주므로 effort interruption notice를 별도 non-turn outbox로, body는 기존 writer로 둔다.

반례는 소스 분기를 따라 대조한 결과이며 실행 RED를 주장하지 않는다. 본 레그에서는 prod 시험을 실행하지 않았다.

## 종료 순서 권고

1. capability/같은 session resume 가능성 확인, desired channel setting 저장, restart intent(원래 native ID·old execution nonce·old turn key/source/binding watermark) durable 기록.
2. transition을 잠그고 captured identity 재확인. owner ingress는 durable queue에 계속 받고 pane effect는 Held. old output actor/ledger는 살려 이미 Prepared인 POST가 기존 lease/history 규칙으로 정산되게 한다.
3. 현재 turn physical end를 exact target로 수행. native abort boundary 관찰을 우선 활용하되 지연 때문에 즉시 restart를 다음 턴으로 미루지 않는다. hard end 확인 없이 새 incarnation을 띄우지 않는다.
4. confirmed end 후 old source final complete-line EOF와 identity를 기록. old exact turn finalizer만 settle, 새 turn은 건드리지 않는다. same-source interrupted boundary witness는 native transcript를 쓰지 않고 projection에서 소비한다.
5. interrupted notice를 episode-key persistent outbox로 1회 enqueue. existing `queue_io/retained_notice.rs:83` carried_unverified_closed가 동일한 keyed non-turn pattern이다. 기존 notice 문구를 그대로 쓰지 말고 effort 이유·계속 진행 예정을 명시한다. 큐에는 ordinary bot-turn로 재유입되지 않는 provenance를 붙인다.
6. 기존 provider launcher로 same native session resume. new incarnation/nonce/Bound/source/ready 및 setting applied 증거 확인. success 전 continuation 0. failed는 debt와 사용자 queue 보존, 정확한 실패 상태 안내 후 transition release/wake. 기존 automatic retry event만 이어 쓰고 새 polling/manual 기능은 넣지 않는다.
7. continuation을 scoped priority custody로 하나 admission. 원래 interrupted 요청은 재전송하지 않는다. continuation이 Submitted/receipt에서 한 번 닫힌 뒤 barrier release, 사용자 FIFO wake. writer는 old unposted prefix debt만 처리하며 새 body는 native identity로 dedupe.

## 범위·남은 계약

- main이 가진 general `/stop` cancel ambiguity, force purge, 기존 watcher의 lost POST history 규칙은 바꾸지 않는다. effort가 새로 도입하는 정확한 restart edge와 continuation custody만 연결한다.
- `HostTerminated`는 exact token settlement를 이미 구현했으므로 재구현하지 않는다. same-session/source interrupted boundary와 continuation-first는 main에 없으므로 새 child 책임이 필요하다.
- 임의 외부 사용자가 native TUI에서 시작한 turn에 managed token이 없을 수 있다. `tmux_runtime/session_stop.rs:35` SessionStop은 authoritative binding+parent InputFacts Open으로 찾아 exact observed session을 interrupt한다. 이 경우 자동 continuation debt key는 native turn ID+execution nonce/source opener로 만들고 discord user message 0을 고유 ID처럼 쓰지 않는다.
- restart 중 `/stop`·`/clear`·Terminate가 이기면 effort intent는 captured incarnation mismatch로 종료한다. `/clear`의 명시적 discard 정책은 보존하되 old effort debt가 다음 fresh session에 부활하지 않도록 exact native ID 확인으로 닫는다. 이 조건은 scoped acceptance test 대상이며 새로운 broad kill/clear 권한은 부여하지 않는다.

