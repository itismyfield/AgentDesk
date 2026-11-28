# #8274 Antigravity 채널의 tui_o 출력 설계

저장소 기준은 `881cc5c5d5933cdffc7db87eeb35075d846064a9`다. 스레드 계약과 관련 좌표는 #8118 리뷰 head `78353d7d49e94b088f5e2ee344739bdeeb0dffb8`에서 읽었다. #8291 착지 후에는 아래 전수표의 [8118] 행에서 실제 계약 차이를 확인하고 델타를 남긴다.

선택된 agy 채널의 대화 입력은 실행 전 tui_o에 기록 의무를 넘긴다. tui_o writer는 채널과 store 세대에 귀속되고, agy 프로세스는 그 채널에서 한 턴의 입력을 실행한다. 첫 본문은 성공 종료와 authoritative Done이 내구 기록에 들어온 뒤 게시한다. 이렇게 하면 델타와 최종 본문이 달라도 정정할 메시지가 없고, 한 번 게시한 분할 조각은 그대로 보존된다. 11-14 결정에 따라 #8160의 legacy Done 경로는 현재 동작을 유지한다. 다른 stream-json 공급자와 headless 공통 턴 수명은 #7651 4c의 범위다.

## 1. 진입점 전수표

좌표는 저장소 루트 기준이다. [8118]은 지정 리뷰 head, 나머지는 저장소 기준 커밋이다. 행동명은 [WriterAdoption 모델](../../statecharts/tui-o-writer/WriterAdoption.tla)의 agy 시나리오를 가리킨다. 동일 행동에 여러 좌표가 있는 경우 앞쪽은 진입, 뒤쪽은 공유 가드나 외부 효과다.

| # | 코드 좌표 | 통과 조건과 모델 행동 |
|---:|---|---|
| 1 | `src/services/discord/router/message_handler/intake_turn.rs:1009,1898,1926`; `src/services/discord/router/message_handler/intake_turn/worker_entry.rs:49,160` | foreground mailbox claim, 실제 목적지·논리 채널과 nonce를 보존해 `AgyReserve`. |
| 2 | `src/services/discord/router/message_handler/intake_turn.rs:2136,2174,2193` | 프로듀서·panic·기존 bridge 입구. owner 토큰을 확보한 경우에만 `AgySpawn`; 한 종료 경로로 합친다. |
| 3 | `src/services/discord/health/turn_deliver.rs:330,432,510,518` | iMessage 등 인증된 HTTP 사용자 입력은 기존 headless starter를 사용한다. 조각 1의 입장은 Discord foreground로 한정하며 인증된 HTTP는 조각 1b 설계 보강이 필요하다. |
| 4 | `src/services/discord/router/message_handler/provider_dispatch.rs:277,340,510,866` | StreamJson 분기와 요청 구성. 타입이 지정된 agy 채널 프로듀서만 `AgySpawn`; 루틴은 `AgyHeadlessAnswer`. |
| 5 | `src/services/stream_json_cli/request.rs:15,31,54`; `src/services/stream_json_cli/mod.rs:18,28`; `src/services/stream_json_cli/dialects/mod.rs:20,25` | 요청·인증·dialect 라우팅. spool handle은 채널 턴 호출자만 전달한다. `AgySpawn`, `AgyHeadlessAnswer`. |
| 6 | `src/services/stream_json_cli/dialects/agy.rs:15,27,65`; `src/services/stream_json_cli/runner.rs:47,76,79` | 정책 검증, 한 턴 argv, spawn과 취소 토큰 등록. spawn 전 실패도 `AgyChildFailed`로 전달한다. |
| 7 | `src/services/stream_json_cli/runner.rs:97,121,130,143,159,173,196,207` | stdout 큐, cancel, startup timeout, 파서 실패, 수신자 종료, wait 및 codec finish. `AgyAppendDelta`, `AgyAppendDone`, `AgyChildFailed`. |
| 8 | `src/services/stream_json_cli/codec.rs:139,159,172,196,241,263,323` | flat/nested init, delta, result, 종료 판정. 채널 전용 canonical 결과를 `AgyAppendDone`으로 전달한다. |
| 9 | `src/services/discord/turn_bridge/stream_loop/content_arms.rs:420,439,469`; `src/services/discord/turn_bridge/retry_state/replay.rs:5`; `src/services/discord/router/message_handler/headless_turn/replay_start.rs:551,582`; `src/services/discord/turn_bridge/stale_resume.rs:132,141` | stale resume·no-output 판정 및 재입장. `AgyChildFailed` 또는 실행이 시작되지 않았다는 증거를 확인한 뒤 같은 입장 키를 재사용한다. |
| 10 | `src/services/discord/recovery_engine/restore_inflight.rs:98,120,1620,2385`; `src/services/discord/recovery_engine/completion_delivery.rs:265,275,447,599` | 부팅 스캔에서 spool custody를 기존 일반 복구보다 먼저 본다. `AgyRestore` 또는 증거를 갖춘 `AgyChildFailed`. |
| 11 | `src/services/tui_o/channel_policy.rs:97,124,184,224,286` [8118] | 선택·별칭·부팅 seed에서 agy writer의 대상 여부를 판단한다. `AgyAdopt`. |
| 12 | `src/services/tui_o/channel_policy/children.rs:154,188,205,421,432,456,470,476` [8118] | 검증된 부모/자식, 중복 candidate 차단, boot reservation, 부모 정책/자식 호스팅. `AgyAdopt`, `AgyReserve`. |
| 13 | `src/services/tui_o/cutover/claude_thread.rs:26` (제안, #8118 리뷰 커밋에는 존재), 같은 파일의 222,243,252,282 [8118] | 스레드 예약 이후 native effect와 최종 body fence 순서의 참조 계약. agy는 자신의 nonce·spool로 `AgyReserve`, `AgyLegacyReceipt`를 판정한다. |
| 14 | `src/services/tui_o/channel_policy/adoption/child_run.rs:76,124`; `src/services/tui_o/writer/late_adopt/child_run.rs:44,108` [8118] | 옛 실행의 권리와 게시 의무는 경계 후에도 해당 실행에 남는다. `AgyLegacyReceipt` 이후 `AgyAdopt`. |
| 15 | `src/services/tui_o/writer/adoption/child_debt.rs:16,109,154`; `src/services/tui_o/writer/deferred.rs:236,495,698`; `src/services/tui_o/writer/late_adopt.rs:109,127,173` [8118] | 부팅·deferred·late activation의 이전 의무와 send counter 확인. 같은 publication fence에서 `AgyLegacyReceipt`, `AgyAdopt`. |
| 16 | `src/services/tui_o/channel_policy/adoption.rs:237,300,419,485`; `src/services/tui_o/channel_policy.rs:456,532,563`; `src/services/tui_o/ownership/execution.rs:36,57,121,187` [8118] | store·owner generation과 채택/회수. 한 프로세스 종료가 writer의 사용권에 미치는 영향을 분리한다. `AgyAdopt`, `AgyLoseAuthority`, `AgyRestore`. |
| 17 | `src/services/tui_o/cutover/channel_gate.rs:139,166,173,200,223,263,398,424,503,523` [8118] | 최종 Legacy claim과 조회가 원본·세대 기준으로 같은 답을 본다. `AgyReserve`; 겹치는 송신을 `AgyLegacyRace` 대조군으로 검출한다. |
| 18 | `src/services/discord/runtime_bootstrap/o_writer_host.rs:423,431,484,504,817,840` [8118] | provider 투영, writer start와 부팅·owner 개시. 한 프로세스가 사라져도 기록이 남으면 `AgyRestore`. |
| 19 | `src/services/tui_o/writer/host/first_init.rs:37,199,282`; `src/services/tui_o/writer/host/recover.rs:47,81`; `src/services/tui_o/writer/host.rs:425,498` [8118] | 최초 store 준비·복원, actor stop/join. `AgyAdopt`, `AgyRestore`, `AgyArchive`. |
| 20 | `src/services/tui_o/writer/host/child_retirement.rs:92,112,137,140,144` [8118] | child 퇴역은 정확한 row·writer 세대, fresh zero debt, join, 후속 row 부재를 확인한다. `AgyArchive`. |
| 21 | `src/services/tui_o/shadow/mod.rs:23,49,121`; `src/services/tui_o/shadow/binding_reader.rs:10`; `src/services/tui_o/shadow/capture.rs:195,207,280,301,373` | source identity·완전한 줄·해시·EOF를 검증한다. `AgyReadSeal`, `AgyRestore`, `AgyArchive`. |
| 22 | `src/services/tui_o/writer/actor.rs:248,639,828`; `src/services/tui_o/writer/replay.rs:15,59`; `src/services/tui_o/store/mod.rs:496` | replay와 open Prepared 우선, 미정리 스풀을 부채로 계산. `AgyRestore`, `AgyReadSeal`, `AgyReceipt`. |
| 23 | `src/services/tui_o/writer/pieces.rs:115,193,389`; `src/domain/message_split.rs:219` | 확정 본문을 씰링해 범위와 렌더 버전을 고정한다. `AgyReadSeal`, `AgyPrepare`. |
| 24 | `src/services/tui_o/writer/deliver.rs:517,631,765,832`; `src/services/discord/outbound/transport.rs:22` | lease·Prepared·wire·확인. `AgyPrepare`, `AgyPost`, `AgyReceipt`, `AgyBlindRepost` 대조군. |
| 25 | `src/services/discord/turn_bridge/stream_loop/message_conversion.rs:26`; `src/services/discord/turn_bridge/stream_loop/content_arms.rs:245,335,355` | #8160의 Done·offset 0 대입이 있는 지점. 채널 전용 경로에서 `AgyAppendDone` 후 동일 Done은 `AgyRepeatDone`으로 처리한다. |
| 26 | `src/services/discord/turn_bridge/stream_tick/output.rs:71,193,214,305,454`; `src/services/discord/turn_bridge/stream_tick/rollover_guard.rs:14,74` | #8160의 이미 게시된 rollover 동결, W14 계열 gate. 새 경로는 `AgyReceipt`로 조각을 확정하고 `AgyRepeatDone`에서도 유지한다. |
| 27 | `src/services/discord/inflight/store.rs:497,502,516`; `src/services/discord/turn_bridge/stream_tick/guarded_persist.rs:57,77`; `src/services/discord/turn_bridge/stream_tick/runtime.rs:194,221` | #8160의 역행·저장 거부·권한 상실 중단. 새 경로는 `AgyRejectFinish` 또는 `AgyLoseAuthority` 뒤 `AgyRestore`. |
| 28 | `src/services/discord/turn_bridge/mod.rs:771,890`; `src/services/discord/turn_bridge/completion_postlude.rs:16,1057`; `src/services/discord/turn_bridge/finalize_epilogue.rs:44`; `src/services/discord/turn_bridge/terminal_controller_cutover/o_body.rs:20,52,63` | 기존 본문·정리·다음 입력 개시의 경계. 채널 agy는 단일 종료 의무로 `AgyFinish`, `AgyApplyEnd`, `AgyNextInput`; 실패는 `AgyNotifyFailure`. |
| 29 | `src/services/routines/agent_executor.rs:112,549,1550,1702`; `src/services/routines/agent_executor/launch.rs:5,213`; `src/services/routines/agent_executor/reliability.rs:76,147` | 가족 프로필을 포함한 루틴 실행·재시도·완료·체크포인트. `AgyHeadlessAnswer`. |
| 30 | `src/services/discord/health/headless_turn.rs:100,246,295,351`; `src/services/discord/router/message_handler/headless_turn/entrypoints.rs:4,200,227`; `src/services/discord/router/message_handler/headless_turn.rs:35,1135,1198` | 이름·DM·owner별 explicit headless 입구와 종료. 루틴과 인증된 HTTP 모두 기존 경로(`AgyHeadlessAnswer`)를 사용하며 조각 1의 foreground 입장과 분리한다. |
| 31 | `src/services/provider_exec.rs:35,81,102,130` | 별도 simple/headless 실행은 스풀 인자 없이 Text/Done/Error를 수집한다. `AgyHeadlessAnswer`. |
| 32 | `src/services/discord/turn_bridge/headless_delivery.rs:315,503,588`; `src/services/discord/turn_bridge/terminal_outcome_delivery.rs:909,914` | W17 outbox/direct 및 복구. 기존 루틴과 인증된 HTTP는 `AgyHeadlessAnswer`. 인증된 HTTP의 W17 경계는 조각 1b 설계 보강이 필요하다. |
| 33 | `src/services/discord/outbound/body_census.rs:133,175,176,201,213`; `src/services/discord/turn_bridge/stream_tick/output.rs:193,305,462` | provider·origin·W14/W14a-c·W17 원본 분류. 관측은 `AgyLegacyRace`와 `AgyHeadlessAnswer`를 구별한다. |
| 34 | `src/services/legacy_body_census/transport.rs:10,32`; `src/services/legacy_body_census/collector.rs:28`; `src/services/legacy_body_census/evaluation.rs:5,279,444,459`; `src/services/routines/legacy_body_census.rs:30` | Legacy 시도·결과와 O 수신증 수집 및 24시간 평가. `AgyLegacyRace`, `AgyReceipt`, `AgyHeadlessAnswer`. |
| 35 | `src/services/discord/commands/control.rs:548,598,620`, `src/services/turn_orchestrator/clear_channel.rs:146,150`, `src/services/discord/catch_up/retry_state.rs`의 `clear_channel_discarding_catch_up_backlog`와 `discard_cleared_backlog` | clear·stop·cancel의 정확한 턴 처분을 기존 anchor 해제와 catch-up 전진보다 먼저 내구 기록한다. `AgyDiscard`. |

## 2. 스풀 형식과 기록 경계

- 입장 키는 `(destination, provider, original_input_key)`다. 조각 1의 원본 키는 Discord foreground 메시지 ID와 그 메시지에 대응하는 기존 mailbox 인수·inflight nonce다. 턴 키는 이 입장 레코드에 지정된 `effective_finalizer_turn_id + turn_nonce`이고, 실제 목적지와 논리 부모를 함께 검증한다. 루틴의 합성 message ID나 채널명으로 입력 출처를 추정하지 않는다.
- 조각 1의 입장 증거는 게이트웨이가 넘긴 Discord 원본과 성공적으로 저장된 inflight다. 원본 출처를 보존하지 않은 큐 재구성과 워커의 과거 payload는 기존 경로로 보낸다. 이 경계의 내구 증거는 활성 전 조각에서 보강한다.
- 인증된 HTTP는 조각 1b 설계 보강이 필요하다. `(destination, provider, iMessage GUID)`를 내구 원본으로 인정하는 조건, 보호 증거, 활성 경계, 원본 없는 HTTP 입력의 처분을 조각 1b에서 설계한다. API headless와 헤드리스 루틴은 기존 제외 범위를 유지한다.
- 세션 ID는 AgentDesk의 논리 세션 키다. agy `conversation_id`는 별도로 Init 및 Done에서 검증해 provider resume 토큰으로 저장한다. 최초 init 이전에도 턴은 식별된다. launch attempt와 writer generation을 각각 둔다. PID는 취소와 진단에만 쓴다. 늦은 A 프로세스가 B 턴의 파일·권한·종료 키를 얻을 수 없다.
- 조각 1은 `<runtime_root>/o_store/<destination>/agy/v1/<message_id>/<segment>.jsonl`에 기록한다. 원본 디렉터리는 한 번만 만들고 실제 턴 키와 nonce는 레코드에 보존한다. 목적지 및 키는 검증된 숫자·고정 인코딩 ID만 허용한다. 대화 토큰, prompt, 사용자 제공 경로를 파일명에 넣지 않는다. 전용 디렉터리/파일을 소유 계정만 읽고 쓰며, 생성·reopen 시 symlink와 세대 불일치를 거부한다. provider 저장소를 읽을 필요가 없다.
- 모든 레코드의 공통 필드는 `v`, `provider`, `destination`, `session_key`, `turn_key`, `attempt`, `seq`, `kind`, `data`다. `seq`는 턴의 정상 append 경계에서 0부터 증가한다. `open`에 입력 키·부모·writer 세대·resume 기대값을 둔다. `init`에는 확인한 conversation ID, `delta`에는 UTF-8 텍스트, `body_part`에는 확정 본문의 연속 바이트 범위와 UTF-8 텍스트를 둔다. `done`에는 `conversation_id`, 확정 본문의 레코드 범위, UTF-8 바이트 길이와 SHA-256, 성공 exit 및 split 정책 버전을 둔다. `failed`에는 유형화한 사유와 진단 분류만 남긴다. stderr 원문·인증 환경·전체 도구 입출력은 스풀에 저장하지 않는다. 새 버전·상충한 seq·파손된 완전한 줄은 이유를 표시하고 보류한다.
- 한 턴당 등록된 한 프로듀서가 쓴다. 공유 가능한 `O_APPEND` fd에서 레코드와 LF를 쓰고 동기화한다. 이어 쓰기 완료 위치는 기존 OStore 내구 저장 패턴으로 `(segment, committed_bytes, last_seq, prefix_hash)`를 게시한다. 새 파일과 체크포인트 교체에는 부모 디렉터리 동기화가 필요하다. reader는 이 위치 안의 완전한 줄만 적용한다. 줄 크기는 기존 capture 한도보다 작은 UTF-8 조각으로 제한한다. Done은 모든 `body_part` 동기화 후 한 레코드로 커밋한다. 일시적 동기화 오류 때는 runner가 같은 확정 결과를 보유한 채 내구 기록을 재시도한다. durable Done 이전의 호스트 소실로 종료 증거를 복구할 수 없으면 명시적 실패를 기록한다. 실패한 append 이후에는 새 attempt의 소유권을 확인하고 새 segment에 마지막 정상 경계부터 계속한다. 종전 segment의 미커밋 꼬리는 그대로 격리한다. open을 포함한 durable reservation과 스풀 디렉터리를 확보하기 전에는 spawn하지 않는다.
- 기존 agy 코덱은 delta가 나온 경우 `result.response`를 무시하고 누적 텍스트로 Done을 만든다. 전용 capture 경로는 같은 JSON 해석과 성공/실패 판정을 재사용하면서 `result.response`가 있으면 이를 취한다. 최종 response가 생략된 정상 dialect는 누적 delta를 사용한다. 최종 response가 명시적으로 비어 있거나 conversation ID·터미널·exit가 충돌하면 실패로 닫는다. 기존 headless StreamMessage 결과 의미를 유지한다. channel spool sink를 붙인 실행의 생명은 legacy receiver disconnect와 독립이다.

## 3. 정리·재시작·중복 방지

1. 부팅 시 같은 destination/turn 키의 custody, spool seal, writer ledger, inflight와 종료 의무를 복원한 뒤 Ready를 게시한다. 파일 ID와 committed prefix를 확인해 저장된 byte/seq에서 읽고, ledger의 가장 오래된 미확정 Prepared를 먼저 처리한다. capture 오프셋, answer 내 UTF-8 전달 오프셋, mail/lease revision을 독립적으로 관리한다. scan 끝으로 점프하거나 다른 턴의 receipt로 기존 턴을 해소할 수 없다.
2. writer가 바뀌어도 sealed answer와 조각 계획은 고정된다. child가 살아 있으면 기존 런의 종료 신호를 기다린다. 서버가 내려가고 child도 사라져 Done이 남지 않은 경우 선택된 owner가 이유를 남겨 실패로 종료한다. 같은 요청의 자동 재실행은 spawn 이전에 실행/부작용이 없었다는 내구 증거가 있는 경우만 허용한다. stale resume 토큰을 무효화하는 현 동작은 보존하고, 다음 원본 입력이 유효한 토큰으로 시작하게 한다.
3. 조각 키는 destination·turn·answer hash·split 버전·index로 만든다. footer에는 별도 고정 키를 쓴다. Prepared를 먼저 저장하고 epoch와 delivery lease를 확인한 뒤 한 번 POST한다. 직접 수신한 생성 ID 또는 해당 조각의 저장 키와 일치하는 transport 증거를 확인해 `Posted`를 남긴다. 같은 payload가 A와 B에 나타나도 본문 문자열만으로 귀속시키지 않는다. 기존 nonce 보조 수단은 Discord가 보장하는 확인 범위에서 같은 키로 재사용한다. 그 범위를 벗어난 애매한 결과는 exact key 확인으로만 해소한다. 구현 검증에서는 별도 embed footer의 짧은 correlation token을 REST 읽기로 확인하는 방법을 기본으로 삼는다. token에는 입력·세션 내용이 없고 답 content와 분리한다. 동일 bot·목적지·조각 키·payload hash와 일치하는 한 메시지의 ID가 필요하다.
4. 확정 `NotSent`만 자동 재전송 자격을 준다. 정책·권한·timeout·삭제된 history로 확인할 수 없으면 게시 의무를 보존하고 가시적인 보류와 재조사 사유를 남긴다. 영구 권한/저장 장애는 운영 실패로 관측한다. cutover 허용 조건에 Prepared 후 미확정 POST·재시작을 포함한다. 소유권을 되찾으면 다시 같은 턴을 settle하고, 후속 턴으로 이전 실패를 덮지 않는다.
5. `ended` CAS, exact-turn projection 적용, writer capture 및 delivery 영수증을 확인한 뒤 seal된 입력을 증명해 스풀을 퇴역시킨다. 기본 보존 목표는 성공 정리 뒤 7일이며, spool 없이도 중복을 차단하도록 turn 키·body SHA·최종 seq·조각 및 footer receipt·종료 ID의 작은 tombstone을 OStore에 남긴다. tombstone은 대응하는 ingress 재실행 가능 기간보다 일찍 삭제하지 않는다. 미확정·누락·FD 불일치·후속 writer 소유권 불명에는 TTL 강제 삭제를 적용하지 않는다. 활성 reader를 닫고 세대/row epoch를 다시 읽은 뒤 삭제와 디렉터리 동기화를 한다. 턴 스풀 퇴역이 채널 writer나 child store 퇴역을 일으키지 않는다.
6. 명시적 clear·stop·cancel은 원본, `turn_key`, store/lease 세대에 바인딩된 단일 내구 처분이다. Done 전에는 취소 종료로 기록한다. 처분 이후에는 미게시 본문과 완료 footer를 포기하고 새 POST를 금지한다. 기존 메시지 ID와 영수증은 보존한다. 이미 wire에 나간 미확정 POST는 조사 기록으로 유지하며 재송신이나 `NotSent` 판정을 금지한다. 미확정 증거가 있어도 처분·종료 효과가 확인되면 다음 입력을 받을 수 있다. 이전 턴의 늦은 이벤트나 영수증은 후속 턴의 inflight·lease·session selector를 바꾸지 못한다. 일시적 권한 상실은 원래 기록과 의무를 보존하고 복원한다.

## 4. tui_o 선택·채택·소유

- `tui_kind` 계열의 출력 대상 판정에 정규 `antigravity`와 등록 별칭 `agy`를 추가한다. agy 선택에는 별도의 출력 kind(가칭 `AntigravitySpool`)를 두고 실제 실행은 `ProcessBackend`로 계측한다. 계정·권한·runtime, 명령·취소, Claude/Codex pane 판단의 공통 enum에서는 새 출력 kind가 TUI pane 분기로 들어가지 않게 한다. default/allowlist/off/host 및 기존 store 보호 규칙을 준수한다.
- 새 spool 경로 자격은 검증된 목적지와 원본 foreground ingress, provider kind, writer 준비 세대, 권위 있는 mailbox claim 전체로 결정한다. `health/turn_deliver`에서 같이 쓰는 headless router는 호출 자체로 자격을 판단하지 않는다. 조각 1에서는 Discord foreground만 typed origin을 전달한다. 인증된 HTTP는 조각 1b 설계 보강이 필요하며, API/routine/scheduled 등 headless 출처와 simple 호출에는 채널 자격이 없다. headless가 같은 Discord 목적지를 공유해도 이전 경로가 같은 결과를 낸다. 원본 유형을 모르거나 재기동 때 선택이 달라지면 durable route를 우선하고 충돌 사유를 표시한다.
- #8118의 checked parent→child 판정, 부모 선정/자식 목적지, 한 adoption snapshot, 기존 body claim을 막는 최종 fence 및 이전 실행에 묶인 receipt 판정을 계승한다. 기존 실행의 원본·send counter·terminal evidence를 잠금 아래 다시 읽는다. 이전 Legacy 턴은 원래 owner가 마무리하고, 새 턴은 채택과 durable reservation 뒤에 시작한다. 턴 A의 정확한 영수증을 받을 때까지 B의 온전한 스풀·종료로 A를 정리할 수 없다. 모호한 부모·기록 충돌·소스 오류에는 intake를 표시된 hold로 둔다.
- `ShadowProvider::Antigravity` parser를 명시적으로 연결해 turn 키와 seal/seq를 검증한 뒤 한 reply unit과 footer unit을 derive한다. 기존 provider-specific branch의 기본 arm을 확인한다. Claude native binding, Codex rollout, tmux/Herdr pane 생존 판정을 agy readiness에 적용하지 않는다. spool가 없는 정상 idle은 Ready 가능, 시작 후 spool가 사라지면 debt, 종료된 child+seal은 전달 가능이다. 클러스터 owner가 이동하면 stale writer fence를 닫고 정확한 store custody를 재검증한 뒤 새 writer가 이어서 읽는다.
- 선택된 원본의 reservation이 있으면 legacy stream edit·rollover·terminal send·recovery/outbox에서 같은 durable decision으로 suppression한다. O가 수신 확인하기 전 bridge가 inflight/dispatch를 완료로 넘기지 않는다. 채택 전 Legacy 원본은 기존 경로가 책임진다. 미선택 원본과 routine/scheduled headless는 기존 body gate 결과를 쓰도록 typed bypass/명시 가드를 둔다. 설정 변경은 신규 원본의 자격만 바꾼다. 이미 잡힌 원본은 같은 경로로 끝내고, reader 또는 durable floor가 없는 바이너리로 롤백할 때 unresolved spool/Prepared가 있으면 부팅 거부 또는 운영 보류로 표시한다.

## 5. 긴 답, authoritative Done, 종료

- Canonical 본문은 성공한 종료에서 한 번 선택한다. 그 본문을 UTF-8 바이트 범위의 연속 조각으로 매핑하고 각 Discord 메시지의 UTF-16 한도를 준수한다. 기존 splitter를 재사용하되 원본 범위를 추적해 공백·개행·astral 문자·코드펜스를 본문에서 유실하지 않는다. 화면용 펜스 보정과 조각 증거가 본문 원본과 섞이지 않도록 범위/렌더 버전으로 표현한다. 완료로 인정하는 답은 한 reply unit이며 이 unit에 1개 이상의 메시지가 속한다.
- 각 조각은 게시 ID가 확인되면 동결된다. 오프셋은 확인된 연속 범위의 끝으로만 늘어난다. 두 rollover 조각이 게시된 뒤 동일 Done을 다시 읽으면 그 두 ID와 오프셋을 보존하고 마지막 미게시 suffix만 POST한다. 최초 Done까지 전체 본문을 버퍼링하므로 임의로 달라진 델타를 동결하지 않는다. 이미 다른 해시/범위를 기록한 턴에서 상충 Done이 관측되면 게시 ID를 보존하고 충돌을 표시한다.
- 최종 소유자는 `(turn_key, expected lease revision)`에 묶인 내구 CAS로 완료 의무를 한 번 확정한다. 답 영수증, 명시적 실패, 또는 3절의 사용자 처분을 요구한다. 정확히 한 답은 성공한 비취소 턴에 적용한다. 완료 의무가 footer·inflight 정리·해당 lease 해제·다음 FIFO 입력 알림을 갖고, 부팅/owner 전환 때 미완료 항목만 재개한다. footer에는 같은 Prepared/receipt를 적용하고, 사용자 처분에는 footer 폐기를 기록한다. inflight 및 lease는 같은 턴일 때만 삭제/해제한다. 큐 kick 재전달은 허용하지만 큐 선두의 durable claim은 동일 입력에 한 번만 성공한다. 상태 projection 사이 충돌·크래시 지점을 시험한다.
- 진행성은 내구 reservation을 인수할 때부터 검증한다. spawn 실패나 child 종료·실패·소실을 확인하고 저장소와 유효 owner가 회복되면, 해당 원본은 답 전달, 명시적 실패, 또는 명시적 사용자 처분으로 종료해야 한다. 관측한 종료 신호의 기록·인계 누락으로 의무가 남거나 결과 없이 사라지면 대조 모델은 RED다. child가 종료하지 않거나 저장소·owner가 계속 불가한 상태에서는 의무와 사유를 남긴다.
- offset 저장 거부는 해당 시도의 전송 권한을 멈추고 receipt와 Done을 그대로 둔다. 저장 실패 전 lease를 뒤집거나 stale owner가 후속 요청을 보내지 않는다. 살아 있는 owner가 재시도하거나 successor가 내구 의무를 재개하며, receipt 충돌 시 운영 가시성을 제공한다. eventual 종료와 정확히 한 답은 저장소·유효 owner·확실한 transport 결과가 결국 제공된다는 전제에서 성립한다. 이 전제 밖에서는 남은 의무와 보류 이유를 보여준다.

## 6. 헤드리스 루틴 회귀

루틴 `agent` action의 agent/DM 선택, start/retry, timeout, 권한·tool envelope, delivery bot, outbox/직접 W17, silent 동작과 확인 후 체크포인트 승격은 현재 경로를 사용한다. 같은 provider의 routine·scheduled·simple 실행은 opt-in spool handle을 받지 않는다. 선택된 채널과 같은 목적지로 들어가는 루틴도 기존 태그와 정책을 유지하도록 최종 W17 및 복구 판정에 typed origin을 연결한다. `family-profile-probe` fixture의 `pendingDelivery`는 확인 전에는 보존하고 확인 후 한 번 승격하는 기존 계약을 시험한다. 실제 한 번 실행은 구현 후 승인된 환경에서 검증한다.

## 7. 수용 시험과 DoD

| 수용 항목 | 검증 |
|---|---|
| 24시간 agy 채널 LegacyBody 0 | 착지 및 rollout 완료 뒤, 열린 Legacy 턴이 settle된 시점부터 24시간 전 호스트의 연속 coverage와 census completeness를 확인한다. provider=`antigravity`, cutover 대상 channel turn의 W14/a/b/c·W17 및 새로 발견한 복구 site에서 Legacy 본문 시도/전송 0. `RecoveredLegacyBody`, body refresh, unknown, source 식별 공백도 확인한다. 실제 agy 채널 입력·O created와 영수증이 1건 이상인 구간을 요구한다. 분모 없이 0인 구간은 수용하지 않는다. |
| 24시간 provider 원시 집계 | 조각 4에서 채널 전환 PASS와 #8274 원문의 provider 전체 LegacyBody 0건을 별도로 판정한다. 채널 O 영수증 1건과 루틴 W17 LegacyBody 1건이 있으면 채널 기준 통과, provider 원시 건수 1, 원문 DoD 미충족으로 기록한다. |
| 긴 답과 정상 종료 | #8160 재현 fixture의 6,254→0 퇴행, 최종 7,620바이트, 동결 2조각을 합성 데이터로 재현한다. 기존 실패 좌표는 그대로 두고 새 경로에서는 Done 재읽기 전에 2조각의 receipt를 만들고 재시작한다. 조각 ID와 오프셋 유지, 나머지 범위만 게시, 복원한 답 byte 일치, footer 한 개, inflight·lease 제거, 다음 입력 성공을 검증한다. 짧은 단일 답, delta 없는 결과, 상충 delta/최종 response, 동일한 두 조각, Unicode 및 코드펜스도 포함한다. |
| Done 저장 거부·권한 상실 | Done 저장 직전/직후, 두 조각 확인 후, POST 후 receipt 저장 전, 종료 CAS 전후 및 projection 중간에 장애 삽입. 기존 owner의 stale write·새 POST 0, 복구 뒤 영수증·오프셋 단조, 정확한 종료 1회와 다음 턴의 정상 dispatch. 길이 거부·미확정 POST·권한 철회·외부 확인 불가를 구분한다. |
| 채택·재시작·정리 | cold start와 warm/late handoff; 같은 채널에 두 차례 agy spawn, 대화 ID 유지/교체, 오래된 PID와 nonce, 같은 원본 재입장, verified child/부모 불명/충돌, 이전 Legacy 의무, partial line·잘린 스풀·파일 교체·fsync 실패; 각 경우 공통 owner gate와 정확한 replay, 보류 표시, delete fence를 검증한다. |
| 명시적 취소·폐기 | 인수 후 Done 전 cancel, Done 후 첫 POST 전 clear, 두 영수증 뒤 미확정 POST가 있는 상태에서 clear를 각각 재부팅과 다음 입력까지 시험한다. 폐기한 턴의 새 POST 0, 기존 ID와 미확정 증거 보존, 다음 입력의 정상 dispatch 및 늦은 이전 종료의 격리를 확인한다. |
| 헤드리스 시험과 실제 1회 | opt-out/no spool, selected 채널과 같은 목적지, DM/owner routing, 명시적으로 silent 실행, pendingDelivery와 실패·재시도, PG outbox 또는 기존 direct 전달을 대상 시험으로 확인한다. 쉘 통합/PG 시험은 별도 throwaway와 `AGENTDESK_REQUIRE_PG=1`을 사용해 기본 병렬로 실행한다. 승인된 실제 agy routine 1회는 성공한 실행·전달/체크포인트 영수증과 기존 계약을 기록한다. |
| 대상 E2E와 상태 차트 | 실제 agy 채널에서 짧은 턴 1회와 2,000자 초과 턴 1회, 다음 입력까지 확인한다. 게시된 조각·footer ID와 해당 O receipt를 비공개 레인 근거에 연결한다. 이 설계 레그의 증거는 [상태 차트](../../statecharts/tui-o-writer/agy-adoption.chart.mmd)와 [TLC 결과](../../statecharts/tui-o-writer/agy-adoption.report.md)다. |

## 8. 구현 조각과 크기

| 조각 | 변경 면 | prod 추정 | 착지 경계 |
|---|---|---:|---|
| 1. 명시적 채널 입력과 스풀 sink | Discord foreground typed dispatch, agy codec/runner, spool encoder·writer·durability | 10파일 / +340 | 전용 sink와 synthetic fixtures, 기본은 전달 비활성. |
| 1b. 인증된 HTTP 원본 | 내구 원본 인정 조건·보호 증거·활성 경계·원본 없는 입력의 처분 | 설계 보강 필요 | 조각 4의 선행. agy 비서 채널의 iMessage 입력을 포함해야 provider 전체 LegacyBody 0건을 판정할 수 있다. |
| 2. agy reader와 ownership | 선택·검증된 child, source resolver/parser, host boot/adoption/retirement | 13파일 / +465 | 병행 shadow/replay 검증. 새 body owner 전환은 다음 조각 뒤. |
| 3. 분할·영수증·Done | derive/split, Prepared transport 보강, terminal CAS와 projection, recovery/queue | 13파일 / +560 | fault injection에서 중복과 종료 검증. 활성화는 기본 차단. |
| 4. 실제 cutover와 관측 | foreground 및 복구 claim 스위치, route별 census, 루틴 회귀와 운영 진단 | 7파일 / +240 | 조각 1b 완료 및 전수표 재대조 뒤 opt-in 검증, 단계별 배포와 24시간 관측. |
| 합계 | 조각 1b 추정 제외, 조각 사이의 파일 중복 포함 | 43파일 / +1605 | 각 조각은 30파일 / +1500 미만이며 +1200 정지선 아래다. |

`pr_cap_prod.py` 실측에서 #8118의 child record·store 활성화 `bf629b6640`은 17파일/+358, shared-view host `ed19fbbb54`는 9파일/+270, child retirement `185e90cfbb`는 20파일/+545, resume·Legacy claim fence `08e0a3f299`는 3파일/+0이다. 이 사례를 기준으로 agy owner 조각에 parser와 boot/retirement 비용을 더했고 종료 조각에는 송신 증거와 복원 비용을 잡았다. 구현 후 각 조각의 실제 base/HEAD를 같은 스크립트로 잰다.

## 9. 리뷰 집중 항목

- #8118 착지 후 검증된 child/policy 상속, cold reservation, prior Legacy debt, adoption lock 순서 및 retirement ticket의 델타.
- 첫 본문이 Done까지 지연되는 사용자 경험과 #8160의 재읽기 fixture. 최종 body의 누적 delta fallback과 실패 분류가 기존 headless 계약을 유지하는지 확인.
- 조각 1b의 인증된 HTTP headless starter 분기, 같은 destination의 routine/비선택 origin, 재시작 후 원본 타입의 내구성. 24시간 DoD의 provider 전체 분모 해석.
- 정확한 POST 수신증 및 영구적으로 결과를 증명할 수 없을 때의 보류. footer correlation token 노출, 본문 byte 일치, 외부 Discord 확인 포트와 기존 ledger의 차이.
- 새 writer가 부팅·권한 전환 뒤 종료 의무를 이어받는지, 오래된 child/lease가 후속 턴을 종료할 수 없는지, 스풀·ledger tombstone이 같은 정리 경계에 들어가는지.
