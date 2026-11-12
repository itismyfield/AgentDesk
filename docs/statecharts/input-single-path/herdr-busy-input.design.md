# #8114 Herdr Claude busy 입력 단일 경로 설계 v1

<!-- doc-path-refs: proposed src/services/discord/router/message_handler/provider_dispatch/owner_input/busy_target.rs src/services/discord/router/message_handler/intake_turn/owned_busy.rs src/services/discord/router/message_handler/provider_dispatch/owner_input/claude_pane.rs -->

기준: main/lane `5bcbb82297f5cc3ab8897b81ddb9891459e0321b`, 2026-11-12 KST.
#8080 대조: `refs/review/i8080-land1=b63a97c5115bf469c85996242d61732557122427`.
이 문서는 조사·설계이며 prod 수정·cargo 빌드/시험·운영 상태 조회를 하지 않았다. 미생성 child 경로는 원 계획 후보로 표시하며, 이동한 코드는 조사 기준 SHA의 blob 링크로 보존한다.

## 구현 재개 시 설계 경계 변경 알림 (2026-11-12 14:4x)

P2 early/clear 보완은 아래 본문에 반영했다. 새 사용자 요구의 “세 번째 호스트는 호스트 구현 하나만 변경”은 원 설계의 host-specific owner routes/leaf 유지와 양립하지 않는다. 당시 `InteractiveSessionHost`/`InputTransport`가 nonce/current-source/verified-clear 위임을 제공하지 않아 별도 경계가 필요하므로 `DESIGN_DELTA`로 멈췄다. 현재 재개는 `herdr-busy-input.boundary-delta.md`의 Slice1(기존 Busy route/operation 공통화)과 Slice2(exact source/observer/reobserve·early/late 승인) 구분을 따른다. Slice1은 새 관측 승인을 활성화하지 않는다. 아래 7파일/+450과 시험표는 원 계획과 P2 보완의 이력이며, 새 경계 확장 규모를 승인한 것으로 읽지 않는다.

## 결론과 범위

**#8080 자동 해결이 아니다. 원 v1의 1조각·prod 7파일/순증 약 +450 추정은 이력이며, 현재 구현 조각은 boundary-delta의 Slice1/Slice2 구분을 따른다.**
이미 존재하는 `owner admission → FrontClaim → physical writer pump → host leaf → settlement/receipt → O writer` 한 경로에 Herdr의 외부/late Busy 관측을 연결한다. 새 큐, 새 전송기, 수동 스위치/런북, 운영자 복구 명령, 새 영구 ledger는 필요 없다.

관측된 운영 원문은 first key 전 `herdr_composer_not_ready / Refused(Unknown)` 후 약 4분 대기다. 기존 코드가 정상 Busy 화면도 idle-ready로 인정하지 않으며, Herdr를 Busy 등록에서 제외해 idle first_input로 보내는 원인 사슬은 코드로 확인됐다. 다만 원문 한 줄만으로 당시 capture 실패·draft·modal 등 Unknown의 세부 원인을 구별할 수는 없다. 운영 로그 재열람/운영 재현은 하지 않았다.

최종 사용자 계약:
- 허용된 Discord 원문/첨부 및 iMessage 사람 입력은 같은 Bound Claude pane이 일하는 중이어도 턴 종료를 기다리지 않고 한 번 paste+Enter한다. transport는 호스트별 기존 adapter를 쓴다.
- 모달·source 불명·writer 부팅·transition 충돌처럼 제출 근거가 부족하면 key=0, 같은 원문/티켓을 Held로 보존하고 공통 보류 표시를 남긴다. 그 원인의 실제 회복이 깨운다.
- paste 또는 Enter가 도달했을 가능성이 있으면 MayHaveSent/Unknown으로 기존 증거 정산만 한다. Held로 되돌려 다시 paste하지 않는다.
- 새 Herdr late-busy 경로는 `transcript_turns(channel) && channel_accepts(channel)`로 현재 O 게시 소유/수용을 확인한다. 설정값이나 “모든 TUI는 O”라는 운영 기억만으로 승인하지 않는다.
- 기존 tmux busy 동작, machine-origin own-turn 정책, pending/cold launch, handoff exact close, 기존 stop/clear/terminate 처분은 보존한다.

## 이미 main에 있는 부분

| 계약 | main 코드 근거 | 이번 처리 |
|---|---|---|
| Herdr Busy route | `queue_io/input_owner/target.rs:86`, `provider_dispatch/owner_input.rs:1483` | 재구현하지 않음 |
| Herdr gated paste+Enter | [기존 Herdr effect L99–L118](https://github.com/itismyfield/AgentDesk/blob/5bcbb82297f5cc3ab8897b81ddb9891459e0321b/src/services/discord/queue_io/input_owner/effect/herdr.rs#L99-L118), `claude_tui/host_input.rs:558` | 기존 adapter 사용 |
| 같은 JSONL nonce/frame receipt | [기존 Herdr effect L123–L138](https://github.com/itismyfield/AgentDesk/blob/5bcbb82297f5cc3ab8897b81ddb9891459e0321b/src/services/discord/queue_io/input_owner/effect/herdr.rs#L123-L138), `claude_tui/busy_inject.rs:285-322` | 형식 추가 없음 |
| 원문·티켓·FrontClaim·물리 writer 직렬화 | `input_owner/admission.rs:46`, `drive.rs:15`, `pump.rs:43-78`, `target.rs:16` | 그대로 사용 |
| Bound/Pending/Created 및 legacy handoff | `claude/herdr_turn.rs:258-277`, `input_owner/claude_handoff.rs:32-224` | #8077을 다시 만들지 않음 |
| machine-origin busy 보류 | `input_owner/origin.rs:30`, `target.rs:77-79` | 유지 |
| O가 frame의 answerer | `tui_prompt_dedupe/injected_steer.rs:243-249` | 새 reader 없이 재사용 |

위 경로의 전체 접두사는 아래 진입점 전수표를 따른다.

## 원인과 설계 변경점

1. `intake_turn.rs:1109-1113`의 `defer_busy_view`와 `:1751-1768`의 native Busy handoff가 Herdr를 구성 predicate로 제외한다.
2. `provider_dispatch/owner_input.rs:1060-1086`는 late 관측의 live/running을 tmux에 한정한다. Bound Herdr는 alive=true이지만 busy=false로 등록돼 `Route::HerdrIdle`이 된다.
3. 이 route는 `take_owned:1285-1293`의 Existing launch hook → Herdr `first_input::write` → idle-only composer로 내려간다. 정상 active footer에도 idle-ready 근거가 없으면 `composer.rs:125-130`가 Unknown으로 접는다.
4. early diagnostic도 `tui_followup.rs:455-491`에서 tmux session 필수·tmux liveness를 요구한다. Herdr는 이 diagnostic 앞의 공통 exact-target/source 관측으로 early handoff를 결정해야 하며 기존 diagnostic 결과만 기다리면 안 된다.
5. busy로 등록되면 기존 `Route::ClaudeHerdrBusy`는 idle composer를 거치지 않고 modal·원문 소유·writer·exact target을 확인해 기존 `run_herdr`로 paste+Enter할 수 있다.

### 한 경로와 모듈 배치

호스트 중립 Busy 관측을 작은 child `provider_dispatch/owner_input/busy_target.rs`에 둔다. 기존 tmux running/source 판독을 이곳으로 옮기고 Herdr Bound 판독을 추가한다. 관측 결과는 Running(논리키, native session, transcript와 file identity, exact execution target), NotRunning, Held(기존 사유+원인을 읽는 probe) 정도면 충분하다. 새 persistent 상태나 transport trait를 만들지 않는다.

- tmux는 기존 `claude_followup_transcript` 및 native JSONL Busy 규칙을 유지한다.
- Herdr는 실제 `TurnHost::Herdr(plan)`, `bound_herdr(plan)`의 Bound/location/nonce target과 현재 attached runtime binding을 사용한다. source는 logical key로만 임의 최신 JSONL을 찾지 않고 `binding_events::claude_history(channel,logical,Some(nonce))`와 verified pin/history의 현재 source를 대조한다. native session·path·file identity가 현재 Bound incarnation과 맞아야 Running이다. `claude_history`의 첫 반환 pin은 nonce 필터가 아니므로 그것만 신뢰하면 안 된다. `history.complete`, `history.awaiting.is_none()`, 해당 nonce의 `history.current`와 `current.pin`을 요구하고, current.session/native session, current.pin/전역 verified pin/runtime binding의 session/path/dev/ino를 모두 대조한다. 다른 nonce에서 `current=None`이면 Held다(`claude_fold.rs:288-295`).
- 검증된 같은 nonce의 `awaited_clear(channel,logical,nonce)`는 Busy strict source 관측보다 먼저 기존 HerdrIdle/clear readiness로 위임한다(`herdr_turn.rs:293,320-390`). 첫 prompt가 새 JSONL을 만드는 정상 clear는 old pin/runtime binding·새 JSONL 부재 상태에서도 이 위임을 따라야 한다. 일반 old pin+new nonce·미검증 Pending은 계속 Held다. `awaited_clear`를 다시 구현하거나 #8080 private readiness API를 공개하지 않는다.
- `bound_source`의 기존 증거 순서(`claude/herdr_turn.rs:288-304`)와 공통 runtime binding/`claude_history` 읽기 primitive를 재사용한다. 이 조각에서 #8080의 private idle-readiness Source API를 별도 공개하지 않는다. `bound_herdr`만으로 process live나 source-current가 입증됐다고 간주하지 않는다. 실제 mutation은 기존 Herdr pin/gate가 같은 incarnation을 재검사한다.
- unknown source·missing transcript·capture/modal의 회복은 기존 `held_watch`에 같은 exact target/current-source probe를 연결한다. native Busy여도 source와 pane의 제출 근거가 회복되면 probe는 Ready를 반환한다. endpoint 재선택·tmux fallback은 없다. 기존 source-publication helper는 active turn을 기다리고 Busy면 wake를 생략하므로(`rehydration.rs:403-440`) 이 liveness의 근거로 사용하지 않는다.
- Slice2의 d2 `reobserve`가 verified awaited clear를 읽으면 Busy 권한을 주지 않고 같은 target/front/generation의 소유된 pre-key Held를 깨워 기존 Idle을 재평가한다. UnknownKeep/clear-discard 원문은 부활·재송신하지 않는다. 예약 시험은 `held_unknown_reobserves_verified_clear_then_delegates_idle`이다.
- early intake의 작업은 작은 `intake_turn/owned_busy.rs`에 둔다. 이 helper는 tmux-only `tui_busy_followup_diagnostic` 호출 앞에서 기존 host 경계의 exact target/source 관측을 받아 early handoff를 결정한다. giant `intake_turn.rs`는 공통 policy/관측 helper를 호출하고 기존 `into_busy_pane`로 넘기는 glue만 가진다.
- late `execute_owned`도 같은 관측 helper를 사용한다. Herdr Running+O confirmed/accepts이면 `Registration::observed(...busy=true,until_done=None)`를 만들고 기존 pump/effect로 보낸다. **tmux의 busy observer를 Herdr에 붙이지 않는다.**
- managed/external inflight의 기존 `hand_busy`와 early `into_busy_pane`도 같은 source 관측을 사용하도록 정리한다. 이미 Herdr를 지원하는 host leaf는 바꾸지 않는다.
- 기존 legacy canary 환경 gate를 owner Busy 전송의 새 승인 조건으로 쓰지 않는다. 현재 tmux owner Busy처럼 source/owner 근거로 전달하고, `defer_busy_view`의 gate는 #8113 표시 정책과 연결한다. 설정 변경·운영자 활성화는 필요 없다.
- `intake_gate/busy_inject.rs:covers`의 legacy live Herdr 제외는 유지한다. owner_takes가 Herdr 포함 모든 owner 채널을 intake에 넘기므로 이 legacy 우회까지 열면 같은 원문이 두 writer에 등록될 수 있다. `queued_head_covers`는 이미 Herdr 포함이다.

`owner_input.rs`도 현재 1,583줄이다. 새 host/source 판정·watch를 이 파일에 얹지 않고 기존 판독 블록의 이동/호출 교체만 허용한다. 거절 시 arm/held marker/notice도 child helper가 담당한다. `intake_turn.rs`(2,321줄), `intake_gate.rs`(1,388줄)도 독립 책임 순증 0을 목표로 한다. 부모의 mod/reexport는 glue이며 범위 밖 refactor는 하지 않는다.

### O 게시와 observer 경계

`hosting/warm_followup.rs:739`의 busy observer는 `:747 bind_claude_tmux_session`, `:770 legacy_pane_alive`, `:790 FollowupHost::legacy_tmux`로 고정돼 있다. 단순 host 분기 완화 후 이 observer를 재사용하면 Herdr에 잘못된 tmux liveness/stop binding을 만든다. 신규 legacy observer를 만드는 방안은 채택하지 않는다.

이미 early `into_busy_pane:549`와 managed `hand_busy:344`는 Busy/until_done=None을 쓴다. late Herdr도 동일한 계약을 쓴다. `tui_o/turn_mode.rs:41`의 프로세스 confirmed 집합과 `writer/host.rs:372`의 실제 accepts가 모두 참일 때만 이를 허용한다. O 미확인/부팅 중에는 기존 boot/writer hold와 `queue_io.rs:369-379` writer-ready wake를 사용한다. O가 user/queued row를 수용하여 기존 게시 소유에 따라 답을 한 번 만든다는 통합 시험을 필수로 한다.

### 실패 처리와 최소 보완

조사 기준 [Herdr preflight L37–L72](https://github.com/itismyfield/AgentDesk/blob/5bcbb82297f5cc3ab8897b81ddb9891459e0321b/src/services/discord/queue_io/input_owner/effect/herdr.rs#L37-L72)는 modal에는 watch를 연결하지만 capture 불가, transcript unavailable, history/load 실패는 모두 실제 회복 관측이 연결된 것은 아니다. 새 입력 경로가 기대는 “key=0 실패 후 같은 원문 Held+회복 wake”를 위해 capture/transcript의 읽을 수 있는 원인에 기존 `held_watch` probe를 연결하는 작은 변경만 포함한다. ledger 거절처럼 pane 읽기로 풀리지 않는 원인은 기존 진짜 turn/transition wake에 맡긴다. pre-reserve source 거절은 `owner_input.rs:981-987`의 기존 hold marker 경로(`v2.rs:477 hold_front`)와 notice를 적용하고 probe를 arm한다. 그러면 `drive.rs:158 take_held`→`:109 held_watch::watch`가 실제 실행된다. 단순 `defer_before_submit`이나 `arm`만 하고 held marker를 생략하는 구현은 허용하지 않는다. post-reserve 거절은 기존 `hold_not_sent`/preparation watch를 쓴다. generic periodic retry나 새 backoff 상태는 추가하지 않는다.

`Completion::Submitted`는 Enter ACK의 transport 완료이고 실제 native user/queued row 수용·답 게시와 다르다. 영수증이 늦거나 없어도 다시 paste하지 않는 Enter-terminal 정책([기존 Herdr effect L104–L138](https://github.com/itismyfield/AgentDesk/blob/5bcbb82297f5cc3ab8897b81ddb9891459e0321b/src/services/discord/queue_io/input_owner/effect/herdr.rs#L104-L138), [기존 Completion L142–L148](https://github.com/itismyfield/AgentDesk/blob/5bcbb82297f5cc3ab8897b81ddb9891459e0321b/src/services/discord/queue_io/input_owner/effect.rs#L142-L148))을 그대로 둔다. “Submitted=transcript 확인”이라고 보고하거나 모델 safety를 그렇게 증명하지 않는다.

## 입력 출처 분류표

| 입력 출처 | 현재 정체성/소유 | Busy 정책 / 이번 변경 |
|---|---|---|
| Discord 사람 텍스트·reply·첨부 | admitted original+Discord ticket, `InputOrigin::Discord` | 같은 owner로 즉시 Busy; uploads materialization 전에는 Held |
| 허용 봇·announce bot의 일반 turn 메시지 | `intake_gate/gate.rs:84-118`가 sender/provenance admission, 저장 original의 author_is_bot 유지 | admitted Discord origin은 현재 person-policy(`origin.rs:30`)로 Busy 가능. 봇을 Routine으로 재분류하지 않음; 연속 merge는 `gate.rs:161`대로 비활성 |
| utility/monitor/operational alert·허용 안 된 봇 | 기존 admission에 따라 제외; 텍스트 marker는 인증 수단 아님 | 새로운 Busy 통로를 부여하지 않음 |
| iMessage 사람 | `origin.rs:44-72` channel+GUID synthetic id/ticket, origin=Imessage | id가 synthetic이어도 person; Discord와 같은 Busy 경로. reply contract 유지(`owner_input.rs:398-420`) |
| HTTP headless start | execution으로 owner 여부 결정, origin=HttpStart | Busy면 자기 턴 대기; 사람으로 승격 안 함 |
| routine/scheduled/기타 합성 turn | origin=Routine, 저장 context/ticket | Busy면 자기 턴 대기, idle 시 기존 owner start |
| remote TUI 입력 | execution=RemoteTui, placement owner | 현재 holder node로 넘김. 이 레인에서 local pane 대체 전송 없음 |
| process/non-TUI provider | execution=Process/NonTuiProvider | 기존 경로, owner Busy 활성화 범위 밖 |
| 직접 native 입력/외부 Claude 도구 입력 | 원문 Discord ticket의 새 진입점 아님 | 기존 transcript 관측/turn presence가 Busy를 결정; 이번 경로의 선행 native 턴으로만 모델링 |

출처를 request 문자열·source 이름·message id 모양으로 추정하지 않는다. `Execution::owner_takes`는 실행 위치를, `InputOrigin::is_person`은 Busy 참여를 결정하는 현재 계약을 유지한다.

## 재시작·부팅·incarnation 전수표

| 진입점 | 현재 처리 / 제안과의 접점 | 모델/수용 시험 |
|---|---|---|
| `runtime_bootstrap/queued_recovery.rs:26-52` | V2 파일 재채택 후 owner_recover. registry가 처음 비어도 같은 원문/티켓 | Restart*; fresh process fixture |
| `provider_dispatch/owner_input.rs:658-678`, `input_owner/recovery.rs:8-39` | tick→known-unsent requeue→history sync→notice drain→handoff recover→wake 순서 유지 | 보낸/불확실 입력을 retryable Held로 복원하지 않음 |
| `claude_handoff.rs:32-224` | journal과 old exact pane close가 Busy 관측보다 먼저; Bound면 prepare=false | #8077 handoff 컨트롤 재실행, exact-close 권한 확대 0 |
| `claude/herdr_turn.rs:258-277` | Bound/Pending/Created 분기 유지. Pending/new execution은 새 Busy 경로로 탈출 못 함 | Pending/Created 0 busy keys |
| `claude/herdr_turn.rs:288-304`, `binding_events.rs:507-535` | Bound exact incarnation/current source 관측 | old global pin+new nonce(current=None), incomplete/awaiting history, inode 교체, same text twin |
| `session_host/herdr_gate.rs:433-451,525-586` | 각 pin/write에 현 server/pane/execution/physical writer 검증 | target replacement 전후 stale target key=0 |
| `queue_io.rs:369-379` | foreground가 있어도 writer-ready가 owner를 깨움 | OHeld→OReady, native end 없음 |
| `tui_prompt_relay/rehydration.rs:324,403-440`, `session_transition.rs:132-153` | 기존 source publication은 active/end와 idle에 묶이므로 새 Busy 회복의 wake로 쓰지 않음. source probe는 기존 held_watch로, transition은 기존 guard-release wake로 처리 | source가 Busy인 채 회복/clear-resume 후 same-original 1회 |
| `input_owner/held_watch.rs:94-160` | 세대와 front message를 묶은 기존 watch; 오래된 read가 새 front를 깨우지 않음 | failed first read, front change, generation change |

새 persisted schema/boot migration은 없다. snapshot의 옛 형식/known-unsent/MayHaveSent 분류를 재정의하지 않는다. reader·source·gate 복원 여부가 불명하면 key=0 Held와 표시를 남긴다.

## Cancel·Terminate·clear·stop 인접 경로 전수표

| 진입점 | 기존 권한/처분 | 이번 비확대와 시험 |
|---|---|---|
| slash/text stop `commands/stop.rs:84-111`→`turn_bridge::begin_user_stop` | Herdr는 intent+키 전달 관측, provider terminal 정산. `finish:75`는 cleanup 안 함 | Busy 주입 때문에 별도 tmux cancel binding을 만들지 않음; stop 경쟁에 기존 기준 적용 |
| Herdr current token `turn_orchestrator/herdr_user_stop.rs:14,36,55` | current token에 첫 intent만 기록, cancel=false | legacy stop authority 생성 0 |
| Queue API cancel preserve [Herdr cancel 응답 L13–L52](https://github.com/itismyfield/AgentDesk/blob/5bcbb82297f5cc3ab8897b81ddb9891459e0321b/src/services/queue/herdr_stop.rs#L13-L52) | non-force는 holder stop 답, 완료/inflight cleared라고 거짓 보고 안 함 | Held 원문은 유지; post-key 원문 replay 0 |
| Queue force cancel / explicit terminate `discord/herdr_terminate.rs:400,679`, `server/routes/herdr_terminate.rs:40-95` | exact session_key+execution_nonce 및 existing exclusion/retire | 새 cleanup/kill 권한 추가 0; stale target 재선택 안 함 |
| dispatched session kill `dispatched_sessions/herdr_kill.rs:50` | 같은 terminate service | Busy 경로가 kill을 호출하지 않음 |
| clear `commands/control.rs:540`→`control/herdr_recovery.rs:177-264` | waiting/refused discard, drops receipt settlement, 실제 실패/count_unknown 유지 | clear가 원문을 이미 discard했으면 이후 watch/wake로 resurrect하지 않음 |
| clear teardown `control/clear_teardown.rs:137-204` | explicit exact execution retire 안에서 selector clear | 새 incarnation으로 옛 입력 전송 안 함 |
| session resume/clear release `session_transition.rs:132-153` | guard 해제 뒤 owner wake | guard 보유 중 key=0, 해제 후 1회 |
| restart-retired row `recovery_engine/herdr_row_end.rs:269`→`inflight/clear_store/herdr_row_end.rs:9` | existing exact owner의 retired row 정산 | stale row를 Busy active의 근거로 사용하지 않음 |

main `owner_input.rs:1320-1322`는 Claude first-key native-stop gate를 tmux routes에만 적용한다. [기존 Herdr effect L83–L118](https://github.com/itismyfield/AgentDesk/blob/5bcbb82297f5cc3ab8897b81ddb9891459e0321b/src/services/discord/queue_io/input_owner/effect/herdr.rs#L83-L118)는 기존 Herdr pin/owner payload fence를 쓴다. 이번 설계는 이 차이를 “동일”이라고 선언하거나 독립 stop 프로토콜을 새로 만들지 않는다. 공통 front/claim·payload fence를 우회하지 않는 회귀를 추가하고, 기존 Herdr stop 세부 차이는 main 기존 경계로 기록한다.

## 원 v1의 구현 조각·크기·시험·뮤턴트 (이력)

원 v1은 기존 전송/소유 경로 연결을 한 조각·prod **7파일/+450**로 추정했다. 현재는 boundary-delta의 Slice1(기존 Busy 동작 보존 공통화)과 Slice2(관측·early/late 승인)로 나누며, 아래 파일·크기는 원 추정의 이력이다. 모델/설계 문서는 prod 집계에서 제외하고, 실제 구현의 독립 계약 확장이나 +1200 PLAN 선 접근은 다시 보고한다.

| 파일(신규 표기) | 작업 | prod 순증 추정 |
|---|---|---:|
| `provider_dispatch/owner_input.rs` | 기존 tmux 관측 block 이동, shared observer 호출 glue, Herdr no legacy observer | -30 |
| `provider_dispatch/owner_input/busy_target.rs` (신규) | actual host/current source/native Busy/O readiness 판독; existing watch 자료 | +180 |
| `intake_turn.rs` | 두 tmux-only 조건을 공통 owned policy/Busy dispatch 호출로 교체 | -10 |
| `intake_turn/owned_busy.rs` (신규) | early source resolve+same into_busy_pane, 표시 정책 연결 | +90 |
| `provider_dispatch.rs` | child helper reexport 최소 glue | +3 |
| `queue_io/input_owner/effect/herdr.rs` | pre-key capture/transcript cause watch만 기존 held_watch에 연결 | +45 |
| `message_handler/busy_retry.rs` | #8113 공통 표시 policy와 result 정산 연결이 필요하면 작은 call-site 적응 | +20 |
| 합계/여유 | 예상 +298, 추정 오차 포함 계획값 | **+450** |

기존 prod caller로 연결해 dead_code/unused를 남기지 않는다. `#[allow]`·baseline·감사 예외·scope 완화는 없다. parent mod/reexport는 FILES_AUTO 근거로 기록한다.

수정 전 RED는 실제 intake/execute_owned seam을 통과시켜 Herdr busy가 idle composer에 빠지는 것을 고정한다. 새 회귀 fixture에 managed active holder/inflight를 미리 넣어 이미 동작하는 `hand_busy`로 우회시키지 않는다. early/late entry 진입을 probe로 단언하고 active managed 경로는 별도 기존 GREEN 컨트롤로 둔다. 테스트가 helper return만 보거나 native end를 먼저 넣으면 이 결함을 검출하지 못하므로 금지한다.

| 대상 시험(새 이름은 계획) | 수정 전 RED/기존 컨트롤 | 수정 후 GREEN / 원인 mutant |
|---|---|---|
| `owner_input_herdr_busy_entry_tests::external_busy_reaches_before_native_end` | Bound/O-ready/busy JSONL+busy ANSI, real owner+socket rig; end 없음인데 0 sends/held → RED | 1 paste+1 Enter, settlement; Herdr exclusion 복원→RED |
| 같은 모듈 `idle_to_busy_before_dispatch_uses_busy_route` | actual execute_owned, late native Busy가 HerdrIdle → RED | same target Busy effect; tmux-only live/running 복원→RED |
| `intake_turn/owned_busy_tests::herdr_busy_diagnostic_uses_owner_front` | 실제 tmux diagnostic 앞 관문. tmux pane/liveness 성공 stub 없이 Herdr socket만 사용 | early=1, late=0, one front/ticket, paste/Enter 각1; intake exclusion 복원 또는 기존 diagnostic 종속 mutant→RED |
| `effect/herdr_tests` 및 real owner drive `modal_or_missing_source_recovers_while_turn_stays_busy` | modal 컨트롤은 기존 동작. managed busy + missing source → source publication만 오고 native Busy 유지: 기존 rehydration wake 없음 → RED | 0 keys+visible Held 후 exact-source probe recovery→1회; watch/held marker 제거 또는 idle-only probe mutant→RED |
| `owner_input_herdr_busy_entry_tests::verified_clear_delegates_idle` | same nonce clear Pending+old pin/binding+새 JSONL 부재, actual execute_owned 경유 | 기존 Idle write 이후 Resolved; 모든 입력에 Busy strict guard 적용 mutant→RED |
| `held_unknown_reobserves_verified_clear_then_delegates_idle` (Slice2 예약) | pre-key source Unknown Held 이후 같은 target/front/generation의 verified awaited clear 관측; 아직 미실행 | Busy 승인 없이 기존 Idle 재평가 wake; UnknownKeep/clear-discard 대조군 resurrect/key=0, native-end 의존 wake mutant→RED |
| `provider_dispatch/owner_input_busy_recovery_tests` | O not-ready/source/old nonce fixture는 기존 0 keys 컨트롤 | O/source ready 및 fresh-registry boot wake로 end 없이 1회; O guard 제거/nonce 무시→RED |
| `effect/herdr_tests` 기존 ACK·foreign·draft 컨트롤과 새 uncertain restart | ACK 없는 Enter/partial write를 Held로 바꾼 mutant만 RED | receipt 없어도 no resend; uncertain→Held mutant→두 번째 paste로 RED |
| `tui_o/writer` 기존 socket/transcript rig 통합 | 새 entry가 호출되지 않아 before-native-end 수용 기대 RED | user row 및 queued_command/enqueue 두 모양, terminal 뒤 output 1회; legacy tmux observer 호출 0, reader/nonce 오염 mutant→RED |
| 출처/controls matrix | 사람/allowed bot/iMessage Busy, synthetic own-turn; stop/clear 전후 timing | control 후 resurrect=0, merged/twin 메시지 각 source 1회, Enter 생략/tmux fallback mutant→RED |

기존 시험 재사용: `owner_herdr_busy_claude_writes_the_bound_pane_once`, `owner_herdr_enter_without_receipt_is_delivered_once`, `owner_herdr_foreign_pane_gets_no_content`, `owner_herdr_modal_holds_the_front_without_a_key`, `the_tmux_owner_parks_only_what_the_herdr_owner_parks`. 새 시험 id는 실제 required PR job에 선택되도록 공식 inventory 동기화 및 static coverage 검사를 실행한다.

구현 레인의 검증 순서: 새 RED→구현 GREEN→각 원인 mutant RED 및 바이트 동일 원복→관련 existing filters→clippy 자기 파일 warning 0→`prepush_gates.sh --static-only`. cargo는 사용자 지정 shim/headroom/seed 규칙을 그때 적용한다. **이번 조사 레인은 cargo/시험 바이너리/clippy를 실행하지 않았다.**

## 관련 이슈와 제외 이유

- #6692: 당시 tmux Busy core adapter/attach guard만 구현됐고 Herdr를 명시 제외했다. 같은 commit에 Herdr 일반 paste+Enter가 있어 transport 불가 해석은 틀리다. “영구 안전 금지” 결정은 PR/commit/docs에서 찾지 못했다. 상세 증거는 이 문서 후반 이력 표를 따른다.
- #8077: Legacy busy tmux를 exact close한 뒤 Herdr로 넘기는 handoff이며 main `193a57168fcbcae4a9ca23b25bacc043dac2fd28`에 완료. 현재 Bound Herdr의 진행 중 턴에 다음 입력을 주는 #8114와 다르다.
- #8080: 일반 Idle 입력 readiness를 transcript/hook으로 교체. review head의 [private readiness L78–L90](https://github.com/itismyfield/AgentDesk/blob/b63a97c5115bf469c85996242d61732557122427/src/services/claude_tui/readiness/mod.rs#L78-L90)가 ordinary Running을 계속 거부하고 busy host exclusions도 유지하므로 #8114 자동 해결 아님. idle/Busy 준비 계약을 섞지 않는다.
- #8113: 입력 즉시 주입 성공 때 queue 표시가 깜빡이는 문제를 공통 policy에서 수리한다. Herdr 범위는 #8114가 그 policy를 이용해 넓히며 중복 표시 구현하지 않는다.
- 권장 착지: **#8077 완료 → #8080 → #8113 → #8114**. 조사·회귀 fixture 준비는 병렬 가능, 실제 코드 좌표는 착지 head에서 다시 고정한다.
- Codex Herdr는 범위 밖. `target.rs:89`와 `effect/codex.rs`의 별도 Busy leaf가 이미 있지만 Codex turn/source/steer/full-access 증거는 Claude user/queued JSONL과 다르다. `docs/codex-busy-inject-canary.md:18-20` 기존 capability 범위를 이 이슈로 열지 않는다. host predicate 변경은 provider=Claude에만 적용하고 Codex 분기/표시 컨트롤을 둔다.
- legacy health receipt 정책·warm_followup 재작성, 추가 cleanup/kill, 재시작 schema·lease protocol·대형 방어 구조는 범위 밖이며 이 변경의 선행 조건도 아니다.

## 상태 차트

기존 모델 조사와 새 focused projection의 TLC 결과는 `docs/statecharts/input-single-path/herdr-busy-input.report.md`에 기록한다. 입력 1개의 delivered 횟수, custody, visible Held, uncertain/restart, exact target을 모델링한다. 전달 정확히 1은 at-most-one safety와 healthy-source eventual-injection liveness를 함께 검사하며, blocked/control/unknown에서는 무조건 전달을 요구하지 않는다. transport ACK·실제 native consume·최종 답은 다른 사건이라 코드 통합 시험에서 따로 검증한다.

기존 `InputOwnership`는 host 차원이 없어 Herdr 제외를 표현하지 않았고, `InputPath`의 unread/HerdrNoCtrlS와 `HerdrBusyHandoff.Blocked`는 인접 경계이지 이 결함을 직접 모델링한 가정은 아니다. 새 projection은 기존 owner 모델의 front/lease/settlement를 유지하면서 host dispatch 공백을 추가한다. “실패하면 Held로 돌아감”은 proven-before-key에만 적용한다.

D1 당시 모델 실측: 22cfg(전/후 8쌍+뮤턴트 6), 11 GREEN/11 expected RED. 공식 plain checker 22/22 기대 집합 일치(exit 0), TLC 86회+fairness precheck 22회, 미검사 속성 0. healthy Herdr Before는 `HealthyBusyEventuallyInjected` RED, Fixed GREEN; temporary held ClearedBefore는 `ClearedHoldEventuallyInjected` RED, Cleared GREEN. 단순 delivered-or-visible-hold는 Before도 GREEN이므로 이 결과를 즉시 주입 해결로 세지 않았다. restart/replacement Before의 `ScenarioReached` RED는 옛 host gate가 그 시나리오 경계에 도달하지 못함을 뜻하며 main의 restart 자체 결함 증명은 아니다.

구현 재개 P2 모델 실측: 30cfg(기존22의 기대집합 불변+신규8), GREEN16/expected RED14. 공식 checker 30/30 기대집합 일치(exit0), TLC119회+fairness30회, 미검사0. earlyBefore는 ordering/healthy RED, earlyFixed GREEN, diagnostic-first mutant는 ordering RED. verified awaited_clear Before/Fixed는 기존 Idle 위임을 보존해 둘 다 GREEN이고 strict-Busy mutant만 RED다. source/trait 경계는 아직 DESIGN_DELTA로 구현 전 정지 상태다.

독립 설계 검토 결과: 첫 검토 P1 1건/P2 2건 중 source wake의 native-end 의존, nonce로 제한되지 않는 전역 pin, 기존 active route로 우회할 수 있는 RED fixture를 위 설계와 시험에 반영했다. 델타 검토 PASS, 남은 지적 없음. 원시 검토는 레인 `logs/design-review-1.md`, `logs/design-review-delta-1.md`에 보존한다.

## 리뷰 집중 항목

- 경계 시각: idle→busy late 판독과 busy→idle 제출 사이, native end 미발생 조건의 실제 entry 시험으로 고정. gate가 stale target을 거부하면 no fallback.
- 동시성 경합: 기존 FrontClaim/physical writer/owner ready-start fence 사용. 두 wake·한 pane 두 logical session·clear와 watch 충돌 시험으로 1회성 검증.
- 옛 형식 복원: existing recovery의 known-unsent만 requeue. MayHaveSent/Submitted·구형 unknown을 보내지 않은 것으로 바꾸지 않음.
- 문서로만 선언한 guard: O confirmed+actual accepts, exact Bound/current source, source identity를 운영 기억이 아니라 호출 경로와 assertion으로 검증.
- 잘못된 0/PASS: 모델 liveness cfg별 actual TLC 결과 수를 적고, 프로덕션 cargo 미실행은 미실행으로 기록. final output 0 또는 generic held-property GREEN을 immediate-delivery 해결로 세지 않음.

## 사용자 결정 필요

없음. 자동으로 기존 owner 경로를 연결하는 작은 변경으로 해결 가능하다. 운영자 조작 기능이나 대형 가드레일은 제안하지 않는다.



## 진입점 전수표 및 판독 상세 원장

# #8114 Claude busy 입력 코드 조사

기준 HEAD `5bcbb82297f5cc3ab8897b81ddb9891459e0321b`. 읽기 전용, cargo/시험/운영 세션 접근 없음.

## 핵심 판정

Herdr는 본문 paste+Enter를 이미 지원하며 owner용 busy leaf도 이미 운영 호출자가 있다. 결함은 전송 수단 부재가 아니라 외부에서 시작한 busy pane/late busy 관측을 tmux만 허용하여 Herdr를 Idle로 등록하는 분기다. `Refused(Unknown)`은 반드시 capture 실패를 뜻하지 않는다. Herdr idle composer 판독기가 정상적인 active busy footer도 의도적으로 거부하고, 이를 Unknown 하나로 합친다.

## 진입점 전수표

| 경로/진입점 | 호스트 선택 | 준비 근거 | 제출 확인 | 실패 처분 |
|---|---|---|---|---|
| Discord live legacy `router/intake_gate/busy_inject.rs:189 offered`, `:213 owner_takes`, `:265 inject_human_input` | `:82 covers`가 Herdr launch 구성 채널/부모를 제외. owner 채널은 Herdr 포함 intake에 넘김 | reconcile 완료/restart 아님 `:146`; 첨부 materialize `:234`; health resolve의 runtime binding+native transcript+transition+holder 재확인 | 하위 health `run_received` | NotSent는 원래 intake/queue, effect 이후 Unconfirmed는 본문 재전송 없음 |
| legacy queued-head `queue_io/queued_head_inject.rs:81 try_busy`, `:137 inject_human_input` | `intake_gate/busy_inject.rs:91 queued_head_covers`는 Herdr 포함. health bound Herdr 분기 | ready prefix/본래 queue identity, health target recheck | health queued_owner가 동일 host leaf 사용 | preparation veto `:150` 보류 표시 enqueue 후 kickoff 종료. 기타 결과 기존 idle fallback 유지 |
| human delivery API `health/turn_deliver.rs:199 deliver_with_ports`, `:393 owner_takes`, `:399 try_inject` | owner 채널이면 legacy inject 건너뛰고 owner admission. owner 외부면 health generic | inject mode, holder/transition/native source | tmux/Herdr 아래의 transcript receipt | NotSent는 start-or-queue, Injected/Unconfirmed는 terminal 반환 `:205-231`; owner admission 실패는 QueueRefused `:249` |
| health tmux transaction `health/turn_deliver/inject.rs:806 run_with`, `:829 inject_report_any_turn/inject_report` | Herdr target 있으면 `:815` Herdr로 먼저 dispatch. tmux turn-mode만 AnyTurn `:310` | `busy_inject/submit.rs:116` composer lock, `busy_inject.rs:461` epoch/liveness/capture/modal/(owner는 mode=0), busy policy는 JSONL busy `:565`; frame ledger 등록 `:574` | `busy_inject.rs:648 confirm`: exact nonce/frame transcript, 1회 보강 Enter는 자기 미제출 paste만 `:700`; owner 없는 mode는 async collect만 가능 | pre-key Veto→NotSent; Stash/Paste/Enter failure→Unconfirmed. sent content 다시 paste 없음 |
| health Herdr transaction `health/turn_deliver/inject/herdr.rs:79 run` | Bound row와 provider/channel/logical identity 일치 `:41 bound_pane`, `:72`; local tmux fallback 없음 | `:159 judge`: plain capture, modal 없음, composer admission=Any/exact empty, JSONL busy와 screen busy 둘 다 | `:111 run_herdr` ACK 후 공통 `busy_inject::await_receipt` `:118`; Observed만 Injected | pre-paste gate Refused→NotSent; uncertain paste/Enter/receipt→Unconfirmed, 다시 paste 없음 |
| owner drive managed/external inflight `provider_dispatch/owner_input.rs:151 owner_drive`, `:200 active`, `:232 hand_busy` | `:309 hand_busy`가 `turn_host::for_turn`으로 tmux/Herdr 모두 지원. `:77 bound_herdr`로 alive, caller busy=true로 phase Busy | inflight row names logical session/output path/session id `:315-338`; Registration::observed source file(dev,ino)/offset `:103-108`, claim `:356` | owner effect는 Enter ACK를 Submitted, receipt는 진단으로 async | [기존 run L107](https://github.com/itismyfield/AgentDesk/blob/5bcbb82297f5cc3ab8897b81ddb9891459e0321b/src/services/discord/queue_io/input_owner/effect.rs#L107): proven pre-key hold는 hold_not_sent+watch, Submitted deliver, uncertain은 Unknown/재송신 없음 |
| owner busy tmux `owner_input.rs:1446 Route::ClaudeBusy`→[기존 BusyClaude L164–L186](https://github.com/itismyfield/AgentDesk/blob/5bcbb82297f5cc3ab8897b81ddb9891459e0321b/src/services/discord/queue_io/input_owner/effect.rs#L164-L186) | tmux leaf; host route `target.rs:84` | [기존 inject_owned·first-key L206–L224](https://github.com/itismyfield/AgentDesk/blob/5bcbb82297f5cc3ab8897b81ddb9891459e0321b/src/services/discord/queue_io/input_owner/effect.rs#L206-L224): AnyTurn+screen-proven+person draft park; lock/ledger/source/target owner scope | [기존 collect_receipt L212–L214](https://github.com/itismyfield/AgentDesk/blob/5bcbb82297f5cc3ab8897b81ddb9891459e0321b/src/services/discord/queue_io/input_owner/effect.rs#L212-L214) 공통 exact transcript 비동기; [Enter-terminal 처리 L258](https://github.com/itismyfield/AgentDesk/blob/5bcbb82297f5cc3ab8897b81ddb9891459e0321b/src/services/discord/queue_io/input_owner/effect.rs#L258) | modal/unread/copy mode는 [기존 cause watch L241–L255](https://github.com/itismyfield/AgentDesk/blob/5bcbb82297f5cc3ab8897b81ddb9891459e0321b/src/services/discord/queue_io/input_owner/effect.rs#L241-L255); load 실제 buffer 실패만 cause watch. uncertain은 notice+Unknown |
| owner busy Herdr `owner_input.rs:1483 Route::ClaudeHerdrBusy`→[기존 BusyClaudeHerdr L14–L35](https://github.com/itismyfield/AgentDesk/blob/5bcbb82297f5cc3ab8897b81ddb9891459e0321b/src/services/discord/queue_io/input_owner/effect/herdr.rs#L14-L35) | route `target.rs:86`; Bound target/physical writer로 제한 | [기존 preflight L37–L80](https://github.com/itismyfield/AgentDesk/blob/5bcbb82297f5cc3ab8897b81ddb9891459e0321b/src/services/discord/queue_io/input_owner/effect/herdr.rs#L37-L80): writer bind, ANSI capture 1회, modal veto+watch, transcript metadata/ledger, owner ready; [draft park·payload start L83–L98](https://github.com/itismyfield/AgentDesk/blob/5bcbb82297f5cc3ab8897b81ddb9891459e0321b/src/services/discord/queue_io/input_owner/effect/herdr.rs#L83-L98) | [기존 run_herdr L99–L118](https://github.com/itismyfield/AgentDesk/blob/5bcbb82297f5cc3ab8897b81ddb9891459e0321b/src/services/discord/queue_io/input_owner/effect/herdr.rs#L99-L118) paste+Enter ACK→Submitted; [기존 receipt L123–L138](https://github.com/itismyfield/AgentDesk/blob/5bcbb82297f5cc3ab8897b81ddb9891459e0321b/src/services/discord/queue_io/input_owner/effect/herdr.rs#L123-L138)가 공통 await_receipt 비동기 | first gate refused & no stash→Held; stash 이후 failure/uncertain→Failed/WriteUncertain. 조사 당시 leaf는 tmux의 first-key cancel gate를 쓰지 않음(`owner_input.rs:1320` 명시); 새 경로에서 넓히지 말고 공유 경로 동등성 검증 필요 |
| early diagnostic 선행 관문 `tui_followup.rs:455-491` | 현재 session 필수 및 `TuiHosting`·hook endpoint·tmux live pane 전부 필요; Herdr socket만 있으면 None | host-neutral owned target/source 관측을 이 관문 전에 수행하고 early로 결정 | legacy tmux diagnostic은 기존 범위 유지 | 실제 intake 시험에서 tmux 성공 stub 없이 early=1/late=0/paste+Enter 각1; diagnostic 종속 mutant RED |
| intake pre-submit external busy `intake_turn.rs:1751-1768`→`OwnerTurn::into_busy_pane` | **tmux만 허용 `:1754 herdr_endpoint.is_none`**, Herdr는 defer_before_submit | tmux followup transcript + logical session. `owner_input.rs:529 into_busy_pane` 자체는 generic host resolve와 observed Busy 재검증 | 이후 paste_busy→same owner transaction | 미관측은 NotSent; Herdr는 `hosted_tui_busy_pre_submit`로 원문 전면 보류 |
| owner late execution `provider_dispatch/owner_input.rs:1000 부근 execute_owned`, `:1060 live`, `:1065 running` | **live는 TurnHost::Tmux 한정**. Herdr는 `live=false`, `running=None`→`:1086 read_as(...busy=false)`; bound_herdr alive=true여서 HerdrIdle | tmux는 native transcript busy일 때 busy_observer와 Busy registration. Herdr는 native busy를 여기서 읽지 않음 | HerdrIdle는 `:1288` Existing launch hook에 넘겨 provider first_input 수행 | 현재 Herdr busy는 idle composer Refused→Held; 무한 정지는 watch/turn boundary에 따라 해소되지만 턴 종료를 기다림 |
| owner idle/new first submit `owner_input.rs:1285-1293`→created hook | Cold/HerdrIdle만 driver가 관측 execution을 handover | tmux created `claude/first_input.rs:70 submit_owned` target/source 넘김. Herdr `claude/herdr_turn/first_input.rs:23 submit_owned` logical+nonce target, bound transcript source metadata, physical writer | [기존 created L361–L378](https://github.com/itismyfield/AgentDesk/blob/5bcbb82297f5cc3ab8897b81ddb9891459e0321b/src/services/discord/queue_io/input_owner/effect.rs#L361-L378)는 driver write 결과를 owner settlement로 변환. first_input response observer는 별도 lifetime | Held는 owner hold_not_sent+준비 watch. post-key uncertainty는 resend 없음 |
| Herdr driver prompt `claude/herdr_turn.rs:709 prompt_and_read`→`first_input.rs:69 write` | Bound/Pending/Created source 모두 `first_input` 사용. Bound source `herdr_turn.rs:283` | `first_input.rs:89` durable hold marker; composer mutation lock `:98`; `composer::run` :99; idle-only ANSI readiness | `run_herdr` ACK가 first submit ticket; transcript provider terminal은 `first_input.rs:158 observe`가 별도로 읽음 | Refused/Cancelled{0}→remove hold marker, prepare watch `:119`, `PreparationHold{reason=herdr_composer_not_ready,BeforePayload}` `:147`; 다른 결과 Guarded/no resend |
| tmux normal warm/fresh first-key `claude/first_input.rs:14 submit`, `claude_tui/input/submission.rs:52 submit` | tmux only existing path | submission.rs:137 lock→before_paste :146→managed fence :153→first_key :157; optional draft stash | stash paste+settle+Enter; provider observation separate | first-key stop proof outranks refusal; pre-key Held vs post-key Indeterminate |

표의 경로는 `src/services/discord/` 또는 `src/services/` 기준 접두사를 문맥대로 확장한다.

## Refused(Unknown) 정확한 지점

- `src/services/claude/herdr_turn/composer.rs:50 observation`: capture 없음→Continue; Person/AgentDesk draft 또는 modal→Break(false) `:60-63`; otherwise `tmux_capture_indicates_claude_tui_ready_for_input`와 `owner==Empty`로 snapshot 작성 `:65-72`; `final_prompt_ready`가 true여야 send `:73`.
- `composer.rs:125-130`: 10초 deadline(상수 `:13`), 200ms 판독; false 또는 timeout 모두 `InputRun::Refused(InputRefusal::Unknown)`로 합침.
- `src/services/tmux_common.rs:347-348` 일반 ready marker는 active work 있으면 false. `claude_tui/input.rs:1248 final_prompt_ready`는 captured/live+marker+draft/busy/auth veto; `:1274-1279` live busy evidence가 최종 거부.
- 따라서 일하는 중에도 composer 자체의 empty/typed 판독은 가능하지만 **idle-ready 근거를 얻을 수 없다**. “Herdr라 화면을 못 읽는다”는 원인 단정은 틀리다. capture 오류·unrecognized layout·busy·draft·modal이 같은 Unknown으로 접힌다. 운영 원문은 이 중 어느 것인지 분리하지 못하나, 코드의 busy-only rejection은 확정이다.
- `first_input.rs:117-135`가 그 결과를 `Claude composer held before first key: Refused(Unknown)`로 담고, watch도 같은 idle-only `composer::when_ready`를 재호출 `:122`하므로 busy가 계속되면 깨우지 않는다.

## Herdr transport와 동일 제출 증거

- `claude_tui/host_input.rs:558 run_herdr`는 generic `run_plan` 사용, `:415-424` memory load_buffer→gated paste→fixed settle→gated Enter. `HerdrInput` `:274-281`는 target.send_paste/send_keys로 구현되어 있다.
- `session_host/herdr_gate.rs:525 send_paste`는 bracketed paste wrapper를 붙여 PaneSendText, literal paste-end marker는 unsent refusal. `:549 send_keys`는 PaneSendKeys; 각각 새 pin judgment를 consume `:578`하고 physical writer lease `:563`를 거친다. busy/idle 여부는 transport gate의 전송 제한이 아니다.
- `busy_inject.rs:285 human_input_text`는 `queue-operation/enqueue` `:304`, `attachment/queued_command` `:307`, non-meta user `:322`를 읽는다. exact nonce/frame, ledger registration, file offset 범위가 공통이며 Herdr 별도 포맷 없음.
- 기존 owner Herdr leaf의 async receipt([기존 Herdr effect L123](https://github.com/itismyfield/AgentDesk/blob/5bcbb82297f5cc3ab8897b81ddb9891459e0321b/src/services/discord/queue_io/input_owner/effect/herdr.rs#L123))와 tmux async collect(`busy_inject.rs:624`)는 모두 `busy_inject::await_receipt`; 현 owner 정책은 Enter-terminal, missing receipt가 재송신을 유발하지 않는다. legacy health Herdr는 ACK 후 동기 receipt 필요하므로 policy 차이는 설계에서 유지해야 한다.

## Bound source 재사용

`input_owner/claude_handoff.rs:369 bound_herdr`는 Bound/location, registered endpoint target, physical writer, exact nonce target을 반환하지만 source를 반환하지 않는다. 기존 provider `claude/herdr_turn.rs:283 bound_source`는 gate+ports.confirm_bound 후 runtime binding(logical)의 native session id/output_path를 읽는다 `:296-304`. `binding_events.rs:507 claude_history(channel,logical,Some(nonce))`는 verified pin/history를 제공하여 incarnation 제한 source 확인에 사용할 수 있다. `tui_followup.rs:259`의 runtime binding은 host 이름에 무관하나 함수 전체는 tmux fallback을 포함하므로 Herdr에서는 그 fallback을 부르지 않는 명시 branch가 필요하다.

## 시험 후보와 현재 빈틈

기존 GREEN 증거는 **이 레인에서 실행하지 않았고 소스 계약만 조사**했다.

- `effect/herdr_tests.rs:222 owner_herdr_busy_claude_writes_the_bound_pane_once`; `:237 owner_herdr_enter_without_receipt_is_delivered_once`; `:251 owner_herdr_foreign_pane_gets_no_content`; `:281 owner_herdr_parks_a_visible_draft_before_the_payload_starts`; `:306 owner_herdr_modal_holds_the_front_without_a_key`; `:347 the_tmux_owner_parks_only_what_the_herdr_owner_parks`.
- `provider_dispatch/owner_input_herdr_tests.rs:420 owned_herdr_claude_idle_writes_the_bound_source_once`는 provider hook 직접 시험. 실제 execute_owned late registration의 Herdr busy 진입은 덮지 않는다.
- 수정 전 RED 새 계약: real Herdr socket rig+V2 owner로 `execute_owned` 실제 경로 진입, Bound+same nonce verified source, busy JSONL+empty busy ANSI 준비. native end 레코드를 절대 추가하지 않고 1 paste+1 Enter, settled/empty original queue, 또 wake해도 추가 send 0. 현재 코드에서는 HerdrIdle→composer 10초 거부/0 sends가 되어 RED.
- intake early busy에 대한 RED: Herdr bound busy diagnostic이 생긴 뒤 `into_busy_pane`를 호출하는 실제 intake seam, 동일 no-native-end/2 sends 단언. 단순 `queued_head_covers=true` 단위시험은 결함을 잡지 못한다.
- race 계약: initially idle→등록 직전 native busy; formerly busy→send 직전 idle; unknown/missing transcript; old binding/new nonce; modal recover while native turn stays busy; restart restore original before actor registry exists; post-paste gate refusal/ACK loss는 재전송 0.
- 원인 mutant: host-aware running 관측을 tmux-only로 돌림 또는 intake Herdr exclusion 복원→위 두 RED. 공통 transport mutant: Herdr paste 대신 tmux fallback 허용/Enter 생략→target/write assertions RED. uncertainty를 Held로 바꿈→restart/wake no-resend RED.

## 최소 배치 제안

기존 giant `intake_turn.rs`와 `provider_dispatch/owner_input.rs`에 host/source 독립 책임을 추가하지 않는다. 새 child `owner_input/claude_pane.rs`(또는 별도 작은 모듈)가 host-aware exact source 관측과 Busy registration 자료를 만든다. giant에는 기존 tmux helper 호출을 그 모듈 호출로 교체하는 glue만 둔다. Bound Herdr busy는 기존 owner route/leaf를 그대로 재사용하고 missing/Unknown source는 명시 Held+same-source watch로 반환한다. 단일 경로의 정본은 기존 owner FrontClaim/pump/effect settlement다. host leaf의 서로 다른 IO를 새 공통 transport facade로 다시 쓰지 않는다. health legacy 별도 receipt policy도 이번 변경에서 owner와 합치지 않는다.

## Herdr late busy observer 경계 및 최소 O 경로

`owner_input.rs:1077 busy_observer`를 Herdr에 그대로 호출하면 안 된다. wrapper `:1133-1170`는 `hosting::busy_owner_observer`로 전달하고, `claude_tui/hosting/warm_followup.rs:747`는 cancel.bind_claude_tmux_session, `:753/:770`는 legacy_pane_alive, `:790`는 FollowupHost::legacy_tmux이다. `FollowupHost::herdr` 생성자는 존재하지만 `host_draft.rs:53 session()`이 target.tmux_session()만 허용하므로 이것을 단순 교체하는 것도 올바른 재사용이 아니다.

최소안은 이미 `OwnerTurn::into_busy_pane`가 쓰는 observed Busy/until_done=None 계약을 Herdr late busy에도 쓰는 것이다. 적용 범위는 **코드로** `tui_o::turn_mode::transcript_turns(channel) && tui_o::writer::host::channel_accepts(channel)`를 요구한다. 전자는 `turn_mode.rs:41`의 프로세스 CONFIRMED 집합(설정만으로 참 아님), 후자는 `writer/host.rs:372` 실제 writer work acceptance다. `tui_prompt_dedupe/injected_steer.rs:243-249 claude_claim`은 confirmed turn-mode에서 O writer가 answerer이며 caller reader 없이 Joined로 처리한다고 명시한다. tmux late observer는 기존대로 유지한다. Herdr O 미확인은 기존 custody/writer readiness wake에 보류하고 신규 legacy observer를 만들지 않는다.

이 설계는 `모든 TUI가 O로 이관됨`이라는 운영 기억만 신뢰하지 않으며, 이미 있는 O 입력 출력 경로로 통합하는 변경이다. 실제 Herdr first_input::observe를 busy receipt observer로 재사용하는 대안은 start offset/terminal handoff/binding 갱신을 함께 옮겨야 하므로 최소안보다 훨씬 넓다.

추가 시험 계약: O confirmed+actual accepts, exact bound source, busy JSONL에서 original reaches once; O native queued/user row→terminal→최종 답 1회; legacy_pane_alive 및 bind_claude_tmux_session은 Herdr 경로 0회; O not-ready 상태는 0 keys와 visible Held, writer recovery wake는 턴 종료 없이 원문 1회 전달.


## 변경 이력·관련 설계 증거 원장

# #8114 이력·겹침 조사

조사 기준 main/lane HEAD: `5bcbb82297f5cc3ab8897b81ddb9891459e0321b`. 운영 설정·DB·store·pane에는 접근하지 않았다. GitHub 읽기와 git object 읽기만 수행했다.

## 결론

`RESOLVED_BY=#8080`에 해당하지 않는다. #8080은 **Idle 일반 입력 준비 확정**을 transcript/Start/Stop으로 옮기며, Running 중 Busy leaf를 새로 Herdr로 연결하지 않는다. #8114에는 호스트와 무관한 기존 input-owner busy 경로로 연결하는 변경과 Herdr Busy transport adapter 정합 작업이 남는다.

## #6692 제외 이유: 직접 증거와 추론 구분

- 원 도입 PR: [#6692](https://github.com/itismyfield/AgentDesk/pull/6692), merge `4efe552ec80b7a881fc4326b076d3c65df5cc96c` (2026-10-08). PR 본문 활성화 조건에 “대상 채널은 Claude TUI(tmux) 바인딩이어야 한다. Herdr 호스팅 채널은 gate가 스스로 제외한다.”가 명시된다. implementation commit `4728d48640a096a57464e52380ec99404003cf5e` 메시지도 “never on Windows or a Herdr-hosted channel”을 명시한다.
- 그 SHA의 [covers L80–L88](https://github.com/itismyfield/AgentDesk/blob/4efe552ec80b7a881fc4326b076d3c65df5cc96c/src/services/discord/router/intake_gate/busy_inject.rs#L80-L88)은 `covers()`에서 Herdr 설정을 명시 제외한다. 조사 기준 main의 [covers L82–L88](https://github.com/itismyfield/AgentDesk/blob/5bcbb82297f5cc3ab8897b81ddb9891459e0321b/src/services/discord/router/intake_gate/busy_inject.rs#L82-L88)에도 남아 있다. 그러나 [queued_head_covers L90–L96](https://github.com/itismyfield/AgentDesk/blob/5bcbb82297f5cc3ab8897b81ddb9891459e0321b/src/services/discord/router/intake_gate/busy_inject.rs#L90-L96)는 Herdr를 포함하므로 live legacy gate와 owner/queued-head capability가 다르다.
- 같은 도입 SHA의 [Pane L221–L237](https://github.com/itismyfield/AgentDesk/blob/4efe552ec80b7a881fc4326b076d3c65df5cc96c/src/services/claude_tui/busy_inject.rs#L221-L237)은 `Pane { program, target }`와 tmux command 실행으로 고정돼 있다. [guarded L292–L306](https://github.com/itismyfield/AgentDesk/blob/4efe552ec80b7a881fc4326b076d3c65df5cc96c/src/services/claude_tui/busy_inject.rs#L292-L306)는 tmux `if-shell` 안에서 attach generation을 확인하며, [load-buffer L593](https://github.com/itismyfield/AgentDesk/blob/4efe552ec80b7a881fc4326b076d3c65df5cc96c/src/services/claude_tui/busy_inject.rs#L593), [paste-buffer L629–L634](https://github.com/itismyfield/AgentDesk/blob/4efe552ec80b7a881fc4326b076d3c65df5cc96c/src/services/claude_tui/busy_inject.rs#L629-L634), [Enter L676–L685](https://github.com/itismyfield/AgentDesk/blob/4efe552ec80b7a881fc4326b076d3c65df5cc96c/src/services/claude_tui/busy_inject.rs#L676-L685)가 모두 tmux 문법이다. [health caller L381–L384](https://github.com/itismyfield/AgentDesk/blob/4efe552ec80b7a881fc4326b076d3c65df5cc96c/src/services/discord/health/turn_deliver/inject.rs#L381-L384)도 이 pane을 직접 만들어 busy core를 실행한다. **Herdr busy adapter가 도입되지 않았음은 코드 사실이다.**
- [선행 core PR #6644](https://github.com/itismyfield/AgentDesk/pull/6644)의 설계 설명 “What the guards do and do not promise”는 tmux server의 `if-shell -F` 원자 판정, attach 재검사와 frame ownership을 안전 계약으로 삼는다. 사람이 attach한 뒤 Enter가 전송되던 P0를 이 서버 guard로 수리한 이력이 있다. 즉 당시 Busy core가 기대던 안전 기구가 tmux 전용이었다는 근거가 있다.
- **전송 수단 자체의 한계는 아니다.** 같은 `4efe552ec8`의 [Herdr 계획 L585–L607](https://github.com/itismyfield/AgentDesk/blob/4efe552ec80b7a881fc4326b076d3c65df5cc96c/src/services/claude/herdr_turn.rs#L585-L607)은 `PasteBuffer(prompt), Enter` 계획을 `run_herdr`로 전송한다. [Herdr transport L363–L370](https://github.com/itismyfield/AgentDesk/blob/4efe552ec80b7a881fc4326b076d3c65df5cc96c/src/services/claude_tui/host_input.rs#L363-L370)는 `target.send_paste`와 `target.send_keys`를 제공한다. [Herdr paste L118–L127](https://github.com/itismyfield/AgentDesk/blob/4efe552ec80b7a881fc4326b076d3c65df5cc96c/src/services/session_host/herdr_host.rs#L118-L127)는 bracketed paste를 구현하고 [Herdr keys L232–L247](https://github.com/itismyfield/AgentDesk/blob/4efe552ec80b7a881fc4326b076d3c65df5cc96c/src/services/session_host/herdr_host.rs#L232-L247)는 key mapping 후 send를 구현한다.
- PR #6692의 comments/reviews는 빈 배열이며 원 도입 commit/PR 본문은 제외 자체만 명시한다. repo `docs`의 해당 SHA에는 별도 PR1-D 설계 파일이 없고, [#5845 원 설계 댓글](https://github.com/itismyfield/AgentDesk/issues/5845#issuecomment-5610378994)은 source-binding 설계로 이 제외의 이유를 설명하지 않는다. 따라서 **“busy adapter 미구현 + tmux 전용 안전 계약을 한정 적용한 범위 선택”은 코드로 지지되는 해석이며, Herdr를 영구 금지하는 별도 사용자 안전 결정은 확인하지 못했다.** 설계 이유가 기록돼 있었다고 과장하지 않는다.

## #8077과의 경계

- [#8077](https://github.com/itismyfield/AgentDesk/issues/8077)은 CLOSED이며 main merge `193a57168fcbcae4a9ca23b25bacc043dac2fd28` (#8101)로 이미 포함됐다.
- `docs/statecharts/session-lifecycle/herdr-busy-handoff.design.md:9-15`는 **이전 Legacy tmux 실행이 busy여도 exact close 후 같은 native session을 새 Herdr 실행으로 넘기는 행동**이다. 현재 Bound Herdr pane의 다음 입력을 ongoing 턴 중 주입하는 행동과 다르다.
- 같은 문서 `:15`, `:31`, `:38`, `:53`은 새로운 첫 입력 및 기존 Bound 입력에 기존 composer 준비 판정을 유지하고, pre-key refusal은 typed Held+wake로 보존한다고 명시한다. `BlockedComposerBoundary`의 progress RED를 남겼다.
- #8114는 exact close, journal, old-paste/fallback, session UUID 재선택을 다시 구현하지 않는다. `claude_handoff::prepare`를 busy 등록보다 먼저 하는 현재 `provider_dispatch/owner_input.rs:193-198` 순서를 보존해야 한다.

## #8080 실제 ref 검증

읽은 review ref: `refs/review/i8080-land1` = `b63a97c5115bf469c85996242d61732557122427`. 읽기 전용 fetch로 `refs/remotes/origin/review/i8080-land1`을 만들었고 HEAD/브랜치는 변경하지 않았다. HEAD와 review ref의 merge-base는 `193a57168fcbcae4a9ca23b25bacc043dac2fd28`이다.

| 실제 ref 좌표 | 의미 |
|---|---|
| [first input L99](https://github.com/itismyfield/AgentDesk/blob/b63a97c5115bf469c85996242d61732557122427/src/services/claude/herdr_turn/first_input.rs#L99) | `composer::run_ready(logical,path,target,plan,cancel)`로 준비 근거 교체 |
| [composer L82–L95](https://github.com/itismyfield/AgentDesk/blob/b63a97c5115bf469c85996242d61732557122427/src/services/claude/herdr_turn/composer.rs#L82-L95) | `run_ready` → `when_idle` → `run_herdr` |
| [composer observe L107–L110](https://github.com/itismyfield/AgentDesk/blob/b63a97c5115bf469c85996242d61732557122427/src/services/claude/herdr_turn/composer.rs#L107-L110) | `readiness::observe`가 없으면 계속 기다림 |
| [private readiness L78–L90](https://github.com/itismyfield/AgentDesk/blob/b63a97c5115bf469c85996242d61732557122427/src/services/claude_tui/readiness/mod.rs#L78-L90) | ordinary Running이면 `None`; local compact completion 특례만 별도 |
| [live covers L82–L88](https://github.com/itismyfield/AgentDesk/blob/b63a97c5115bf469c85996242d61732557122427/src/services/discord/router/intake_gate/busy_inject.rs#L82-L88) | legacy live gate의 Herdr 제외는 변경 없음 |
| [defer_busy_view L1106–L1109](https://github.com/itismyfield/AgentDesk/blob/b63a97c5115bf469c85996242d61732557122427/src/services/discord/router/message_handler/intake_turn.rs#L1106-L1109) | `defer_busy_view`의 `herdr_endpoint(...).is_none()` 그대로 |
| [pre-submit busy L1698–L1715](https://github.com/itismyfield/AgentDesk/blob/b63a97c5115bf469c85996242d61732557122427/src/services/discord/router/message_handler/intake_turn.rs#L1698-L1715) | native Running → `owner.into_busy_pane`가 여전히 Claude tmux만; Herdr는 `defer_before_submit` |
| [transcript readiness 설계 L58–L60](https://github.com/itismyfield/AgentDesk/blob/b63a97c5115bf469c85996242d61732557122427/docs/statecharts/input-single-path/transcript-readiness.design.md#L58-L60) | Idle 경로 Running key=0과 Busy 중 paste+Enter 허용을 구분 |
| [기존 Busy 유지 설계 L104–L109](https://github.com/itismyfield/AgentDesk/blob/b63a97c5115bf469c85996242d61732557122427/docs/statecharts/input-single-path/transcript-readiness.design.md#L104-L109) | `hand_busy`, native Running Busy leaf, `SubmissionContext::Busy`는 기존대로 유지한다고 명시 |

따라서 #8080이 착지해도 idle 좁은 화면 준비 문제는 줄지만, **Running을 Idle로 오인해 보내지 않으므로 #8114의 busy 입력은 자동 해결되지 않는다.** #8080 이슈 DoD의 “busy 중 입력은 지금처럼 대기·큐” 문장은 최신 설계에서 기존 Busy 주입을 유지하는 것으로 좁혀졌고, 어느 경우도 Herdr Busy 신규 활성화를 명시하지 않는다.

## #8113 겹침·순서

[#8113](https://github.com/itismyfield/AgentDesk/issues/8113)은 BusyActiveTurn 큐 커밋 때 📬를 먼저 붙였다가 주입 후 ➕로 바꾸는 깜빡임 수리다. 현재 `src/services/discord/router/intake_gate.rs:1007-1013`은 BusyActiveTurn에 기본 options를 사용한다. #8113이 공통 표시 정책을 둔 뒤 #8114가 Herdr 대상 범위를 동일 정책으로 넓히면 된다. #8113 원문은 Herdr의 기존 표시를 유지한다고 명시하므로 #8113 단독으로 #8114 전달 또는 표시 범위가 해결되지 않는다.

권장 착지 순서: **#8077(이미 main) → #8080(준비 API 고정) → #8113(공통 결과 표시 정책) → #8114(Busy 경로 Herdr 연결)**. #8113과 #8114는 서로 전달/표시 책임이 달라 병렬 준비는 가능하지만 `intake_gate.rs`/`intake_turn.rs`의 최종 조건을 동일 predicate로 합쳐야 한다. giant 파일에 독립 host-policy 책임을 늘리지 말고 기존 또는 새 child helper를 호출하는 연결만 둔다.

## Codex Herdr 제외 근거

`docs/codex-busy-inject-canary.md:18-20`은 Codex Busy capability가 Windows/Herdr를 기존 gate에서 제외한다고 명시한다. Claude JSONL의 user/queued_command/enqueue receipt와 Codex native turn/source/full-access 안전 조건은 다르다. #8114는 Claude Herdr 동일 경로 연결 요청이므로 Codex Herdr capability를 여는 변경은 범위 밖이다. 기존 Codex host branch/표시 회귀 시험만 유지한다.

## 원시 자료

외부 조사 logs 폴더에 `pr-6692.json`, `pr-6644.json`, `issue-5845.json`, `issue-8077.json`, `issue-8080.json`, `issue-8113.json`, `issue-8114.json`을 남겼다. 원문 코드는 [#6692 busy-inject 스냅샷 L221–L237](https://github.com/itismyfield/AgentDesk/blob/4efe552ec80b7a881fc4326b076d3c65df5cc96c/src/services/claude_tui/busy_inject.rs#L221-L237)과 [#8080 transcript readiness 설계 스냅샷 L58–L60](https://github.com/itismyfield/AgentDesk/blob/b63a97c5115bf469c85996242d61732557122427/docs/statecharts/input-single-path/transcript-readiness.design.md#L58-L60)에서 확인할 수 있다. cargo/프로덕션 테스트는 실행하지 않았다.


## A안 — Busy strict 권한과 공개 Idle 위임 분리 (2026-11-13 승인)

코디네이터가 `design-delta-impl4-idle.md`의 최소 A안을 승인했다. 기준 source는 `6d44fde3c30fa107e33d3cfc4644de4a35f135a4`다. 정상 cold launch는 authentic hook→Stat/Bound→native hard terminal/Idle 순서와 등록 뒤 hook0에 도달할 수 있다. Busy 전용 verified pin을 먼저 요구하면 공개 B1 Idle Proof로 허용되는 이 입력도 Held로 소비하므로 진행 회귀가 된다. pin0을 Busy 쓰기 권한으로 바꾸지 않고 기존 Idle 위임을 복원한다.

| 경계 | A안이 소비하는 근거와 처분 | 보존하는 계약 |
|---|---|---|
| Busy 승인 | captured exact target/nonce, 실제 O owner/writer 수용, global+nonce current pin, runtime/source/opened identity 모두 필요 | pin0/nativeBusy·Unknown·wrong nonce·moved target은 key0 Held. Busy pin 조건 완화0 |
| Idle 분류 | 기존 public `readiness::observe`의 opaque Proof→`reserve`→`Reservation::current`를 사용한다. 분류용 pre-key reservation은 `not_sent`로 반환한다 | private Source 공개·복제0. stale proof/target/current mismatch는 Held. 분류가 Busy paste나 first-key를 허가하지 않음 |
| Idle 실행 | 분류가 현재 Idle을 위임하면 기존 Idle driver가 새 준비·reservation·current·first-key·전송·receipt 계약을 수행 | 분류용 proof/token을 실제 송신 권한으로 재사용0. transport 기록과 native receipt를 구분 |
| Held 재관측 | 같은 owner/target/front/generation/nonce의 아직 소유된 pre-key 원문만 새 공개 Idle 근거로 재평가 wake | 초기 Held 결과를 영구 cache하지 않음. UnknownKeep·clear-discard original 부활0. verified awaited_clear 기존 위임 유지 |
| S1/producer | 기존 공통 Busy operation/route, physical writer, tmux adapter, B1/2b publisher를 그대로 소비 | 새 production 파일·trait·flag·Herdr Core 상태0. hook 합성·metadata 사전 생성·수동 pin 발행0 |

B1 공개 seam의 조사 좌표는 `readiness/mod.rs:61` Proof, `:66` observe, `:225` reserve, `:257` current, `:270` not_sent다. reserve가 pending/revision을 바꾸므로 판독만 하다가 reservation을 남기면 기존 Idle driver의 새 reserve를 막는다. 같은 proof의 현재성을 검사하고 pre-key 분류 token을 돌려준 뒤, driver가 자기 증거를 다시 얻는 경계다. 실제 함수·파일좌표는 구현 레인의 최종 source에서 재확인한다.

B안(등록 전 hook과 이후 Stat의 durable 관계를 producer에 새로 보존해 후속 hook 없이 pin 발행)은 보류한다. nonce/UUID/path/dev/ino/first-record, NotDurable, rotation/cursor 계약과 B1/2b 변경 승인이 별도로 필요하기 때문이다. A안은 Busy producer 의미를 수정하지 않는다.

A안 크기 승인은 ownS2+675 + S1+162에 +40~70을 더한 +877~907 예상이다(`CAP_MARGIN i8114 907/850`). +935(기존+850의10%)를 넘으면 PLAN_READY에서 멈춘다. 최종 실제 cap·Rust RED/GREEN·뮤턴트·static gate는 구현 레인의 별도 보고서가 맡는다.

새 [cold Idle 모델 보고서](herdr-cold-idle.report.md)는 pin0+후속hook0의 Before/Fixed 진행, Busy/Unknown/nonce/target 보류, same front/generation 재관측과 stale Proof/current 거절을 검사한다. 기존60cfg와 expected RED27은 바꾸지 않는다. 모델은 Rust 실행 성공을 뜻하지 않으며, 실제 cold/late/회복 시험의 동일 원문·키수·정산 증거가 필요하다.
