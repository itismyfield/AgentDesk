# Native terminal boundary — #8148 delta

Code baseline: `200ef5a37f2926f01f7dbcda70ccdb192285b8b8` (B source binding). 설계 레그이며 Rust·운영 설정을 변경하지 않는다. [모델 결과](native-terminal.report.md), [상태도](native-terminal.chart.mmd), [기존 귀속 계약](native-attribution.design.md).

## 결정과 범위

**안 B: 기존 O poll의 native settlement 경계가 검증된 raw EOF closer를 별도 증거로 받아 projection에 terminal-only overlay를 적용한다.** 일반 SourceCapture는 완전한 줄만 계속 낸다. source binding B와 기존 receipt·결과·정리 순서는 유지한다. interrupt는 기존 strict parser가 terminal로 판정한 row에 대해 Prompt/receipt 분류를 생성하지 않는 수리다.

| 안 | 흐름 | 비용·위험 | 결정 |
|---|---|---|---|
| A — capture가 EOF row를 직접 방출 | provider-ended proof를 capture로 내려 raw JSON을 CapturedRecord/spool에 넣음 | read-only complete-line capture에 host 수명·nonce 권한이 들어감. record.end=line.len+1, prefix/spool cursor·output replay를 함께 바꿔야 함. synthetic newline 없이 기존 record 형태를 쓰기도 어려움 | 제외 |
| B — native projection의 terminal overlay | 기존 poll에서 완전한 prefix C를 확보 → domain port의 exact EOF proof → reducer가 마지막 strict close edge만 적용 → 기존 native 결과 CAS | EOF 전용 stamp/range와 boot proof validation이 필요하지만 output capture/spool/cursor는 그대로. 기존 custody/재시도와 source facts 재평가를 사용 | 채택 |

A/B 모두 새 launch·입력 재전송·kill/삭제 권한을 주지 않는다. B의 raw EOF는 일반 JSON record가 아니며 input/body/receipt 후보로 사용하지 않는다. watcher·bridge의 legacy Complete, assistant end_turn, synthetic newline로 대체하지 않는다.

## 확인한 결함

1. `src/services/tui_o/writer/input_facts/native.rs:452`는 strict interrupt user row도 Prompt로 만든다. `:563`의 foreign Prompt 검사가 `:635`의 aborted Closed보다 앞서 barrier를 저장한다. 직전 PG 실패는 own membership 640..1001, interrupt 1001..1207이며 source Some/AnyKey true다. `src/services/tui_o/shadow/identity.rs:71`은 이미 strict terminator를 우선한다.
2. raw EOF fixture는 `src/services/discord/router/message_handler/provider_dispatch_herdr_http_start_tests.rs:952`에서 strict turn_duration JSON을 newline 없이 쓴다. 일반 capture `src/services/tui_o/shadow/capture.rs:384,403`과 native prefix decoder `src/services/tui_o/writer/input_facts/native.rs:491`은 그 tail을 완전한 row로 내보내지 않는다. 결과 생성자 `src/services/discord/turn_finalizer/native_supply.rs:689`는 close.end≤observed.through를 요구하므로 raw tail을 별도 증거 없이 완료시킬 수 없다.

읽기 전용 main 기준 `ffa099d4f4a6a68c26171fb464f7b5edffca2e5f`: complete-line guard는 `shadow/capture.rs:331,350`, strict terminator 우선은 `shadow/identity.rs:71`에 있다. 둘 다 재구현·완화하지 않는다. 앞선 HTTP 비교(#8148 0/5, #8212/main 5/5)와 B 후 3/5는 이전 보고서의 코드별 실행 증거다. 이번 레그에는 재실행하지 않는다.

## interrupt terminal row

Projection이 이미 쓰는 canonical `classify(Claude, value)`/strict terminator 결과를 한 번 계산한다. strict terminator로 확인된 row는 Closed edge만 만들고 Prompt/queue/input membership receipt를 만들지 않는다. native interrupt의 기존 aborted 의미(Cancelled)를 유지한다. 일반 user row, interrupt 문자열을 언급한 문장, 추가 block을 가진 row는 기존 parser의 결과대로 Prompt일 수 있다. 문자열 포함 검사나 별도 느슨한 parser를 만들지 않는다.

수리는 `native.rs`의 barrier 순서를 무조건 뒤집는 방법보다 terminal 분류의 우선권을 공유하는 방법을 쓴다. genuine foreign Prompt는 계속 barrier를 만들며, 여러 input이 공유한 run의 membership·FIFO·중복 closer 처리를 바꾸지 않는다. 새 본체는 child `input_facts/native/terminal.rs` (proposed)에 둔다. 현재 991줄인 native.rs에는 호출 교체/child 선언만 둔다.

### 이미 저장된 잘못된 barrier

`history/v2/native.rs:282`는 barrier가 있으면 candidate에서 제외하고 `:611`은 native 결과 CAS를 거절한다. 따라서 classifier만 고치는 것은 새 기록만 고치며, 이전 실행이 남긴 잘못된 barrier는 부팅 뒤에도 계속 막힌다. 이를 지우거나 모든 옛 barrier를 무효화하는 migration은 하지 않는다.

read-only native snapshot에 해당 exact ticket의 기존 Barrier를 선택적으로 공급한다. 같은 authenticated O prefix에서 old barrier의 foreign row UUID·byte range·foreign_hashes가 새로 인증한 row의 UUID/range와 기존 frame_hashes 결과에 일치하고 그 row가 **canonical strict interrupt row 자체**임을 확인하고, old membership UUID/range가 이 ticket의 이미 durable인 receipt/run과 일치할 때만 typed `InterruptBarrierSupersession`을 만든다. 이 proof는 namespace/full key/source/nonce, 기존 Barrier raw value의 canonical digest, row/range/digest와 native Cancelled closer를 함께 고정한다. unknown barrier 형식은 Hold이며, 알려진 기존 unversioned shape는 그대로 읽는다.

이 pure proof가 있는 동안만 해당 registration을 Cancelled 분석 후보로 재평가한다. 별도 candidate mutation, durable barrier-clear ACK 또는 두 번째 journal을 만들지 않는다. **같은 history 결과 CAS 안에서** 현재 stored barrier digest·scope·membership/terminal proof·등록 stamp를 다시 대조해 Cancelled 결과와 supersession proof를 함께 저장한다. 원래 barrier 데이터는 감사/복원을 위해 남긴다. 기존 guard는 이 exact Cancelled 예외 이외에는 그대로 거절한다. 다른 row/range/foreign_hashes의 genuine foreign barrier를 현재 interrupt로 넘기는 것은 금지한다. 이미 outcome/control이 있으면 덮어쓰지 않는다.

부팅 시 결과 전에는 old barrier와 source facts를 새로 검증한다. 결과가 durable이면 보존된 supersession과 원래 barrier의 일치를 검증해 기존 cleanup을 잇는다. 이 과정도 기존 Source 가독성 관문을 유지한다. ordinary foreign barrier는 기존 abstention 정책의 권한으로만 다룬다. 새 구현의 범위에 fresh interrupt와 stored bad-barrier replay를 모두 넣는다.

## raw EOF 증거 계약

### 값과 provenance

- `C = captured_through`: 기존 O complete-line cursor와 authenticated prefix hash. O의 현재 SourceCapture/Projection/내구 spool cursor가 같은 source와 C를 가리켜야 한다. 완전한 prefix를 아직 덜 읽었다면 일반 capture가 먼저 진행한다.
- `L = EOF length`, `C < L`: 원본 raw range는 `[C,L)`이며 synthetic +1 byte가 없다. prefix/capture/spool cursor는 C에 남는다.
- 요청에는 namespace, 원본 full ticket/key·owning episode/generation, SourceId(session/path/dev/ino), 원본 binding seq와 execution nonce/owner/root, C/prefix hash, registration fingerprint, O revision을 담는다. channel 또는 같은 physical source의 첫 binding만으로 권한을 고르지 않는다.
- 검증 proof는 이 identity와 EOF Fingerprint(len/dev/ino/mtime/ctime), raw range/digest, canonical strict kind(Completed/Cancelled), 관측 시점의 execution-end witness를 고정한다. body·전체 InputAttempt·transcript를 중복 journal하지 않는다.

### 생성 순서

1. 기존 `Sources::offer_native`/O poll(`rotation/owe.rs:454`, `actor.rs:419`)이 B source ACK와 일반 complete-line capture를 먼저 처리한다. existing reader/seed가 있는 exact native custody만 요청한다. snapshot 메서드는 계속 read-only다. 별도 typed EOF-proof method를 기존 EvidencePort에 둔다.
2. Discord domain child `turn_finalizer/native_terminal.rs` (proposed)가 live/restored custody, current history ticket/source, 원본 Bound/Resolved event, matching HostedExecution을 확인한다. `herdr_terminate.rs:224`의 read-only `execution_end` 판정을 재사용한다. Ended만 허용하며 ProviderAlive/Unconfirmed/unknown endpoint/다른 nonce·root는 Hold다. boolean `Retired` 자체는 proof가 아니다. Retired라도 남아 있는 같은 execution record/target을 다시 확인한다(`db/dispatched_sessions/hosted_execution.rs:562`는 retire 때 record를 보존). 현재 N2만 남아 N1을 검증할 수 없으면 N2에 기대지 않고 진단 가능한 debt다.
3. 같은 SourceId를 read-only FD로 열고 path/FD identity와 Fingerprint를 요청의 O capture snapshot과 맞춘다. `C..L`만 읽는다. 기존 `MAX_PARTIAL_BYTES`를 재사용하며, 정확한 길이·UTF-8·하나의 완전한 JSON value·내부 newline 없음·현재 canonical strict terminator를 모두 요구한다. 기존 native projection의 parent/subagent·isSidechain·sessionId 일치 관문도 그대로 적용한다. 반쯤 쓴 JSON, 일반 JSON, assistant end_turn, trailing garbage는 proof가 아니다. dev가 재번호화된 파일은 기존 authenticated reopen을 먼저 거쳐야 하며 새 leaf가 stat-only로 승인하지 않는다.
4. 읽기 전후 FD/path Fingerprint, C/prefix identity, matching execution-end/nonce를 재확인한다. 크기/metadata/대상/등록 stamp가 변하면 후보를 폐기하고 기존 O retry로 재관측한다. 새 전체 transcript scan이나 native cursor reset을 만들지 않는다. 이미 캡처된 prefix와 durable membership receipt는 O의 기존 인증/재평가 계약을 따른다.
5. producer/RuntimeReady/추가 hook이 없어도 O의 live/boot supply가 같은 method를 호출한다. watcher/기존 recovery가 먼저 E를 관측했어도 wake 힌트일 뿐이며 그 경로의 legacy Done 또는 retirement bit를 native 증거로 바꾸지 않는다. 새 executor/recovery loop는 없다.

### 소비와 결과 CAS

- 최종 Evidence가 생기기를 먼저 기다리지 않는다. 기존 receipt pass의 fresh registrations에서 source Some/candidate/이미 durable인 receipt가 있는 exact ticket을 고르고, 처음부터 `ticket.source.target`을 넣은 native_origin으로 원본 binding seq/nonce를 선택해 EOF proof를 요청한다. proof를 받은 뒤 reducer가 그 ticket의 run membership을 확인한다. 같은 SourceId의 첫 binding이나 아직 없는 terminal Evidence에 의존하는 순환을 만들지 않는다.
- reducer는 기존 captured facts와 durable membership receipt/fingerprint로 run을 복원한 뒤, proof가 `[C,L)`의 **terminal-only edge**인 경우만 마지막에 적용한다. raw JSON을 Prompt/receipt/body로 분류하지 않는다. Source.offset≤membership.start<end≤C이고 close가 같은 run을 닫아야 한다. 이미 닫힌 run의 중복 close는 결과를 늘리지 않는다.
- `Observation.through`는 C 그대로다. 선택적 terminal witness와 별도 terminal end L을 추가한다. `source_evidence`의 기존 `range.end≤through`는 일반 증거에 그대로 적용한다. 초과 범위는 exact raw witness, close=[C,L), matching source/nonce/key/stamp가 있는 이 경우만 허용한다. 단순 max(through,L)로 capture 진행을 위조하지 않는다.
- observed/current 비교에는 raw witness digest/range/Fingerprint와 registration fingerprint/revision도 포함한다. receipt가 막 commit되어 stamp가 바뀌면 기존 captured facts를 다시 평가하고 proof를 새 stamp에 묶는다. raw proof는 AnyKey나 membership receipt를 만들어 주지 않는다.
- 기존 결과 CAS 안에서 control/기존 결과가 이겼는지 검사한다. native 결과가 explicit control 결과를 덮어쓰지 않는다. native 결과+proof durable ACK → 기존 captured actor/episode/row CAS cleanup → cleanup ACK/QueueEligible 순서를 유지한다. raw proof 획득만으로 lease·row를 지우지 않는다.

두 저장소나 외부 파일의 원자적 write를 가정하지 않는다. proof는 검증한 관측 시점의 역사적 사실이고 결과 CAS는 history의 등록/결과 stamp를 선형화한다. 외부 파일의 모든 미래 변경을 잠그는 계약이 아니다. 결과 전 관측된 source/metadata 변화는 재검증하고, 결과 뒤 N2 actor/row는 기존 cleanup CAS가 보호한다. 임의의 과거 prefix 변조를 새 leaf가 전체 재해시로 발견한다고 주장하지 않는다. main의 `shadow/capture.rs:323,369`는 reopen/명시 검증 때 prefix hash, 일반 poll 때 tail guard를 사용한다. 새 EOF path는 그 기존 captured-facts 신뢰 범위를 넓히지 않는다.

### 내구성·부팅·회수

raw 후보/열린 FD는 process-local이며 현재 O poll당 유계 상태다. IO 실패/파일 부재/대상 불일치면 성공 tombstone을 만들지 않고 기존 재시도로 넘긴다. 결과 전 crash면 후보를 버리고 기존 prefix reopen/custody adoption 뒤 같은 tail을 다시 검증한다. 결과 후 crash면 `Outcome.extra.native_proof` 안 선택적 raw witness 요약을 기존 결과와 함께 durable 보존해 기존 boot cleanup을 잇는다. 별도 전체 journal/새 boot census/새 reaper 권한은 없다.

기존 저장 결과는 raw witness None로 그대로 복원한다. 일반 complete-line 증거의 검증은 유지한다. 새 raw witness가 있는 저장 결과는 full ticket/source/nonce, membership/closer 범위, typed kind, C/L과 proof digest/schema를 확인한다. 결과가 이미 durable이면 동일 결과 cleanup을 위해 새 raw EOF proof를 만들지 않는다. 다만 기존 `native_restore.rs:72`의 Source::validate/stat 관문은 그대로다. 파일 소실·교체 시 진단 가능한 기존 debt로 남으며, transcript가 사라져도 cleanup한다고 약속하지 않는다. 아직 결과가 없는 candidate만 반드시 fresh E/EOF를 재검증한다. 잘못된 schema·부족한 raw proof로 range를 확장하는 옛 결과는 Hold다. unknown extension을 legacy 성공으로 해석하지 않는다.

## 제어·오류와 진행성

Cancel/Terminate/clear/stop의 기존 control witness는 native strict-closer proof와 다른 권한이다. 동일 exact ticket의 기존 control CAS가 이기면 그 결과/cleanup을 잇는다. 새 EOF proof가 제어 신호/terminate/clear를 보내거나 control custody를 재등록하지 않는다. foreign Prompt barrier의 기존 abstention과 E/no-terminal notice도 보존한다. 이 정책들에는 기존 scoped 증거와 notice ACK가 필요하며, invalid raw tail을 Native Executed로 만드는 경로가 아니다.

“terminal 증거 없는 삭제 0”의 증거는 이 범위에서 검증된 strict native closer 또는 이미 허용된 exact control/E-notice/barrier 결과 증거다. E만 관측했거나 partial JSON만 있으면 새 native 결과를 만들지 않는다. 닫힌 exact execution의 row는 관측 가능한 exact host end authority, source/host/history의 eventual readability와 기존 retry/notice ACK 공정성 아래 결국 settle한다. 영구 unread/nonce 불명 상태까지 무조건 삭제하는 진행성은 제공하지 않는다. 그 반례 cfg를 기대 RED로 남긴다.

## 구현 전 진입점 전수표

좌표는 모두 `200ef5a37f`의 `src/services/` 아래다. 각 경로가 새 proof를 만들 수 있는지와 그대로 둘 권한을 구분했다.

| 생성·소비 경로 | 좌표 | 처리 |
|---|---|---|
| strict row 정본 | `tui_o/shadow/identity.rs:71` | 기존 terminator 우선 재사용, parser 범위 변경 없음 |
| interrupt projection/barrier | `tui_o/writer/input_facts/native.rs:452,563,635` | strict close에 Prompt/receipt를 붙이지 않음 |
| 일반 O capture | `tui_o/shadow/capture.rs:307,323,333,384,403` | complete cursor·prefix·partial guard 보존, 새 native raw record 방출 없음 |
| O seed/reopen | `tui_o/writer/input_facts/native.rs:266,302,311,491`; `tui_o/writer/rotation/owe.rs:279,329` | 기존 prefix pass/receipt 재평가에 terminal overlay만 분리 |
| O poll·native offer | `tui_o/writer/actor.rs:419`; `tui_o/writer/rotation/owe.rs:454` | live/boot typed raw proof 요청의 정식 진입점 |
| port/interface | `tui_o/writer/actor/native_input.rs:18`; `discord/turn_finalizer/native_supply.rs:57,169,578,689` | snapshot read-only, 별도 EOF proof method, observed/current/raw stamp 검증 |
| B source refinement | `discord/turn_finalizer/native_binding.rs:162,183,264`; `turn_orchestrator/queued_head/v2/cold_source.rs:112,157` | 먼저 기존 B ACK. source None/다른 lineage에 terminal proof 안 줌 |
| Herdr physical E | `discord/herdr_terminate.rs:193,224`; `db/dispatched_sessions/hosted_execution.rs:545` | read-only Ended 재사용, Retired bit 단독 금지, 새 retire/close 호출 없음 |
| producer live reader | `claude/herdr_turn/live_reader.rs:8`; `claude/herdr_turn/first_input.rs:395,436` | 기존 typed terminal/wake 유지, 새 O proof의 필수 생산자로 만들지 않음 |
| 기존 raw EOF reader | `claude/herdr_turn/terminal_replay.rs:201,217` | whole/strict/gone 의미 참고. ended_tail/ended_turn의 전체 prefix 재독을 새 O 경로에서 호출하지 않음 |
| watcher | `discord/tmux_watcher/turn_stream_collector.rs:33,400`; `discord/turn_bridge/stream_loop.rs:212` | 관측/wake 가능, unscoped Done으로 native cleanup 우회 불가 |
| legacy completion 거절 | `discord/turn_bridge/native_input.rs:21,55,70`; `discord/turn_bridge/mod.rs:678` | Pending/Settled native 소유 구분과 neutral exit 유지 |
| live recovery E/terminal | `discord/recovery_engine/herdr_row_end_live.rs:30,54,62,95,120` | 기존 Some(true) 양보를 O proof/wake에 연결 가능. 그 자체 native result/row 삭제 안 함 |
| restarted recovery | `discord/recovery_engine/herdr_row_end.rs:116,386,467,488,538` | 기존 end/notice 정책, exact source 읽기, retirement/notice를 proof로 오인하지 않음 |
| durable receipt | `tui_input/human_frame/history/v2/native_receipt.rs:4`; `discord/turn_finalizer/native_supply.rs:169` | raw EOF로 receipt 생성 안 함, C 안 exact receipt CAS 유지 |
| 저장 barrier 후보·결과 guard | `tui_input/human_frame/history/v2/native.rs:282,611`; `tui_input/human_frame/history/v2/native_barrier.rs:41,58`; `discord/turn_finalizer/native_supply.rs:350` | exact interrupt supersession을 같은 결과 CAS에서 검증. barrier 삭제/migration 없음 |
| native outcome/finalizer | `discord/turn_finalizer/terminal_handler.rs:494,564,610`; `tui_input/human_frame/history/v2/native.rs:592` | source/run/raw range·등록 stamp, single result, 기존 row/actor CAS |
| boot prepare/custody | `discord/inflight/removal/boot_reaper.rs:75`; `discord/inflight/removal/boot_custody.rs:434` | 기존 기록 보존 순서. 후보 메모리 소실과 durable 결과를 구분 |
| boot adoption/proof/cleanup | `discord/turn_finalizer/native_restore.rs:72,95,314,388,489` | 결과 전 재검증 / 결과 후 saved proof cleanup; 기존 형식 None 호환 |
| Cancel/HTTP cancel | `discord/commands/stop.rs:98`; `discord/turn_finalizer/native_policy.rs:41,132`; `turn_orchestrator/policy_close.rs:72` | exact control CAS 우선, native outcome 덮어쓰기 금지 |
| Terminate/force | `discord/herdr_terminate.rs:354`; `discord/herdr_terminate/http_force.rs:32` | 기존 explicit authority만, raw proof로 종료/정리 권한 증가 없음 |
| clear | `discord/commands/control.rs:546,834`; `discord/commands/control/clear_teardown.rs:63,243`; `turn_orchestrator/queued_head/v2/clear.rs:17` | 기존 boundary/control/retained 입력 정책, terminal overlay와 별도 |
| stop | `discord/commands/stop.rs:84`; `discord/turn_bridge/tmux_runtime/judged_stop.rs:345,358`; `discord/turn_bridge/tmux_runtime/owned_input_stop.rs:28` | 기존 Stop witness/신호만, proof 생성으로 key/kill 송신 안 함 |
| foreign barrier/abstention | `tui_input/human_frame/history/v2/native_barrier.rs:58`; `discord/turn_finalizer/native_abstain.rs:54` | genuine foreign/모호성은 계속 Hold/기존 명시 정리. strict interrupt만 오분류 제거 |

## 크기와 리뷰 조각 분할

이전 추정 +240~450에 실측 +828이므로 배율은 828/450=1.84 ~ 828/240=3.45다. 아래 raw 추정을 이 배율로 보정한다. 새 책임은 child로 두며 giant native.rs에는 작은 연결만 둔다.

| 예상 production 변경 | raw net code |
|---|---:|
| new `tui_o/writer/input_facts/native/terminal.rs` (proposed): strict 분류·barrier supersession·raw witness/overlay | +130~180 |
| new `discord/turn_finalizer/native_terminal.rs` (proposed): custody/host E/유계 tail 검증 | +70~110 |
| `rotation/owe.rs`: terminal Evidence 이전 exact-ticket 요청/재평가 | +30~55 |
| `actor/native_input.rs`: typed port와 선택적 barrier snapshot | +16~22 |
| `input_facts/native.rs`: child/호출/optional witness 연결 | +8~12 |
| `native_supply.rs`: typed port·현재 결과 guard 연결 | +12~20 |
| `native_restore.rs`: saved raw/supersession 검증 연결 | +10~16 |
| `history/v2/native_barrier.rs`: exact supersession의 순수 검증 helper | +35~55 |
| `history/v2/native.rs`: 후보/결과 guard 호출 교체 | +6~10 |
| root child/mod·serde 호환 glue 여유 | +10~16 |
| 합계(반올림 여유) | **10파일/+330~600** |

저장 barrier의 candidate/result guard까지 포함한 보정 후는 +608~2070, 기존 B +828과 합하면 **+1436~2898**다. 예상 union prod 약26파일(기존18 + 이번10 − native_supply/history-native 중복2)이며 PLAN_READY +1200 선과 전체 한 조각의 +1500 상한을 넘는다. 전체를 한 review 조각에 구현하지 않는다.

| 다음 리뷰 조각 | 비교 base | 보정 추정 | 완료 조건 |
|---|---|---:|---|
| T1: 현재 B source 구현 확정 | 기존 repair base 3aea6f63bd | **18파일/+828 실측** | source B 5개 실제 Rust mutant 및 대상/형제/최종 check·Windows·clippy/source scan. 남은 terminal 실패를 숨기지 않고 전체 #8148 착지 허가와 구분 |
| T2: fresh interrupt + stored barrier supersession | T1 고정 head | raw +120~270 → **+414~932**, 약8 prod파일 | strict/mention/extra-block/다른 barrier/old replay·boot·control 경합 RED→GREEN/원인 제거 RED. candidate→결과 CAS 전체를 함께 구현하며 broad barrier clear 없음 |
| T3: 검증 raw EOF proof 전체 경로 | T2 고정 head | raw +210~330 → **+725~1139**, 약8 prod파일 | EOF·scope·partial·stamp·crash/boot·control 경합 RED→GREEN/mutants, 원래 HTTP 5/5 및 형제/최종 gates. dormant half-port만 먼저 활성화하는 조각으로 나누지 않음 |

분할은 리뷰 단위다. 코디네이터가 두 조각의 근거를 합쳐 #8148 착지를 판단하며, 남은 두 결함을 착지 후로 미루는 안이 아니다. 각 구현 조각에서 실측/예측 +1200이면 다시 정지한다. cap 정본 상수는 `/home/adk/skills/agentdesk-issue-pipeline/scripts/pr_cap_prod.py`의 FILES=30, NET=1500이고, 매 조각을 그 공식 명령으로 측정한다.

## 수용 시험·변이 계획

1. 기존 native prefix/실시간 append 모두에서 strict interrupt→Cancelled, foreign barrier 0. ordinary prompt/표식 언급/extra block은 기존 분류 유지. 저장된 동일 wrong barrier는 exact Cancelled supersession으로 복구하되 다른 barrier는 넘기지 않는다. strict 우선권·stored override 제거·broad override mutant RED.
2. 실제 Herdr E + whole strict raw closer + 이미 durable인 own receipt에서 O가 original row를 한 번 정리. newline=true 기존 경로도 함께 확인. EOF proof 호출 제거·E guard 제거·partial 허용·cursor 확대 mutant 각각 RED.
3. foreign source/key/nonce, stale generation, N2 교체, file append/rewrite/rename, digest/stamp 변경, EOF 중간 read 실패를 거절. fresh snapshot 재시도는 성공. 범위/lineage/current-stamp guard mutant RED.
4. producer 없는 live/빈 owner boot, 결과 전 후보 소실, 결과 durable 뒤 crash 및 cleanup ACK 소실을 재현. 결과 전 재검증, 결과 후에는 새 EOF 획득 없이 같은 결과 ACK를 잇되, 기존 source 가독성 관문과 IO 회복 전 debt는 유지한다. retry/boot/내구 순서 제거 mutant RED.
5. Cancel/Terminate/clear/stop과 native result의 두 CAS 순서, genuine foreign barrier/기존 NoTerminal notice를 검사. 새 native proof가 기존 결과를 덮어쓰거나 N2 row를 지우지 않음.

모델은 이 계약과 반례를 검사하며 Rust 실행의 대체가 아니다. 이 설계 레그에서 cargo/PG/Windows/clippy/prepush 또는 운영 runtime을 실행하지 않는다. 앞선 static의 ARCH_REGEN_DRIFT·SYNTH_MERGE_CONFLICT는 코디네이터의 착지 준비 범위로 그대로 둔다.
