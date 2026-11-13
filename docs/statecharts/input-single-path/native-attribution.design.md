# Claude TUI 입력 귀속과 native 종결 확정 — #8148 d1

기준 코드 `33d1f4b5a9dce7023e2d0014e45a05ed6c530bba` (origin/main·브리프의 운영 배포본).
설계만 작성했다. 생산 코드·운영 상태 변경 0. 이 문서의 proposed 계약은 구현 완료를 뜻하지 않는다.

## 범위 판정

**(b) #7651의 다음 P2/P3에 lease 이전은 없다. tui_o 입력에 한해 O native 증거가 종결을 확정하고 기존 TurnFinalizer가 lease·inflight를 정리하는 작은 선행 조각을 권한다.**

이슈 #7651 본문은 단계 2를 bridge **본문** 삭제, 단계 3을 watcher/session/recovery **본문** 삭제로 정하고 stop·receipt·headless·후처리를 유지한다. 턴 수명 이전은 단계 **4a episode/release → 4b receipt/origin → 4c headless/stop/queue/boot**이며 inflight 전면 삭제는 단계 8이다. 다음 출력 묶음이 lease까지 이전한다는 근거는 없다. `docs/session-host-parity.design.md:144`, `:164`도 #8118 O-b 뒤 P2/P3를 열도록 할 뿐 턴 소유를 바꾸지 않는다. 따라서 단순 SCOPE_DEFER는 두 차례 입력 정체를 그대로 남긴다.

현재 진입은 `router/message_handler/intake_turn.rs:2237`에서 O 채널도 `spawn_turn_bridge`로 보낸다. 다른 세 진입은 `headless_turn.rs:1130`, `recovery_engine/restore_inflight.rs:2378`, `tui_prompt_relay/claude_idle_bridge.rs:407`이다(경로 접두사 `src/services/discord/`). `turn_finalizer/finalize.rs:177`이 mailbox를 풀며 `:326`이 QueueEligible 후처리를 맡는다. O `writer/input_facts.rs:355`, `:552`는 순서 있는 native 경계를 알지만 입력별 finalizer 호출은 없다. `health/recovery/carried_turn_close.rs:163`은 현 세대 행을 제외한다. #8148은 현재 세대 입력이므로 이 복구의 세대 조건을 풀어서 고치지 않는다.

대안 비교:

| 안 | 흐름·비용 | 판정 |
|---|---|---|
| #7651 단계 4 전체를 기다림 | 본문 삭제 뒤에도 bridge가 입력 lease를 소유. 신규 코드 0, 재발 지속 | 기각. 다음 단계가 해결한다는 계약이 없음 |
| O가 입력별 증거를 확정 → 공통 finalizer | O capture/queue 분류·V2 attempt·기존 nonce CAS 재사용. bridge에는 연결·권한 거절만 | **채택**. #7651 4a의 tui_o/Claude 부분 계약으로 흡수 가능 |
| turn_bridge 내부 prompt/end 판독 수리 | 레거시 본문·큐 판독을 다시 확장. 삭제할 코드에 별도 귀속 FSM | 기각. O에도 영향을 준다는 이유로 구경로를 재구현하지 않음 |

`turn_bridge` 본문/완료 추론 알고리즘은 수리하지 않는다. 필요한 접점은 O ticket이 있는 episode의 본문 축적·독자 Complete 제출 거절, 이미 O가 정리한 episode의 중립 종료뿐이다. headless·non-O·Codex 수명은 기존 경로에 남긴다.

## 근거와 이미 충족된 부분

이슈 본문·21:1x 재발 댓글과 진단 §0~§1을 읽었다. 진단의 운영 transcript를 다시 열지 않았다. 첫 사례는 enqueue → 남의 종료 → dequeue → 자기 종료를 모두 지나쳤고, 두 번째는 입력이 외부 진행 턴에 합류했다. 출력 O와 푸터 정상은 **입력 종결 증명과 별개**다. 진단에서 TB의 어느 세부 completion 판정이 보류했는지는 미확정이며 이를 새 모델에서 확정 사실로 만들지 않는다.

main에서 재사용할 보장:

| 보장 | main 코드 좌표 | 남은 일 |
|---|---|---|
| Claude 내부 큐 Waiting/Dequeued/Attached/Running, remove 이유별 구분 | `src/services/tui_o/writer/input_facts/queued.rs:9`, `:19`, `:93`, `:147`, `:198` | 현재 aggregate owed를 입력 ticket별 실행/철회 증거로 내보내기 |
| native Open/Idle와 record 순서·범위 | `src/services/tui_o/writer/input_facts.rs:39`, `:355`, `:552` | 해당 입력이 속한 run의 종료와 결합 |
| assistant end_turn 단독은 strict idle이 아님 | `src/services/tui_turn_state/completion_scan.rs:196` | `stop_hook_summary`·`turn_duration` 중복을 같은 close로 처리; end_turn부터 닫지 않기 |
| 원본/attempt·source inode·실행 nonce 검증 | `src/services/tui_input/human_frame/history/v2.rs:12`, `:23`, `:323`, `:395` | episode nonce와 input settlement의 영속 연결 |
| 같은 episode/actor만 lease 해제 | `src/services/discord/turn_finalizer/episode.rs:47`, `:79`, `:89` | native ticket 전용 admission 및 완료 결과 기록 |
| 늦은 stream write는 행 부활 불가 | `src/services/discord/inflight/save_store/identity_gate/guarded_read.rs:7` (RowAbsent 반환), `stream_loop_patch.rs:34` | 실제 TB tick·handoff·postloop 경합 시험으로 재확인 |
| 운영자 release는 CAS, 큐·provider 보존, 성공 배송을 주장하지 않음 | `src/services/discord/turn_lease.rs:298`, `:309`, `:329` | 새 경로와 같은 episode 결과 경쟁만 연결; 명령 추가 없음 |

이슈 DoD 4개 중 main만으로 완전히 충족된 것은 없다. 위 leaf 보장은 재구현하지 않는다.

## 입력 출처와 소유

| 입력 작성자/진입 | AgentDesk 원본 소유 | native 턴 시작 소유 | 처리 |
|---|---|---|---|
| Discord 사용자·명령봇, queued front | 있음 (V2 identity/attempt) | 즉시 실행·내부 큐·mid-turn 모두 가능 | ticket으로 귀속; 외부 턴에 합류해도 그 입력만 소유 |
| AgentDesk scheduled_message/routine/HTTP turn/start | 있음. person/synthetic 분류 유지 | 자기 native turn 필요 (`owner_input.rs:213`) | 이번 조각은 기존 synthetic waits-idle 정책 유지; 독자 busy 주입 금지 |
| Claude `scheduled_task_fire`/meta prompt | 없음 | 외부 | FIFO 계산에 포함; AgentDesk 성공·receipt 만들지 않음 |
| Claude background task notification/queued_command | 없음 (AgentDesk 원본 매칭 없는 경우) | 외부 시작 또는 진행 중 합류 | commandMode/origin과 실제 attempt 구분; 알림 텍스트로 소유 추정 금지 |
| 사람이 TUI에서 직접 입력 | 없음 | 외부 | O 출력은 그대로 게시; Discord episode 종료 근거로 쓰지 않음 |
| AgentDesk 입력의 remove(absorbed_mid_turn)·human queued_command | **기존 attempt와 유일하게 매칭되면 있음** | 원래 외부 턴일 수 있음 | 같은 run의 member로 등록. 새 턴을 기다리지 않음 |
| remove(delivered_to_agent)/일반 withdraw/popAll | 원본은 있음, 실행 완료 아님 | 없음/다른 agent | 미실행·이관 결과. Complete로 만들지 않음 |
| headless 및 TUI subagent transcript | 별도 기존 소유자 | 본 조각 밖 | parent source만 수용하는 InputFacts 정책 유지 |

`isMeta`, human origin, 본문 hash, promptId, 채널 busy 여부 하나만으로 소유를 정하지 않는다. AgentDesk가 작성한 예약 입력과 Claude 자체 예약 발화를 같은 scheduled로 묶지 않는다.

## 최소 계약

1. **주입 전에 등록.** 기존 V2 prepare/bind 흐름에서 원본 집합·attempt nonce, provider/channel, mailbox episode nonce/actor, 실행 locator와 execution nonce, native session, SourceId(dev/ino/path), binding watermark, 완전한 줄 기준 pre-write offset을 묶은 `NativeInputTicket`을 보존한다. 새 전역 소유 registry나 운영 설정을 만들지 않는다. pre-source면 RuntimeReady/검증된 binding이 채울 때까지 미확정이며 재주입하지 않는다.
2. **O 판독 하나를 재사용.** `InputFacts::events()`와 `queued::classify`를 사용하는 작은 입력 귀속 fold를 O leaf에 둔다. 기존 presence 읽기 worker의 source/binding 검증과 재개 경로를 재사용하고 채널당 두 번째 무한 full scan을 만들지 않는다. 현재 `QueuedInputs`의 body 기반 aggregate 상태를 그대로 완료 권한으로 쓰지 않는다.
3. **native FIFO 전체를 읽는다.** AgentDesk enqueue뿐 아니라 예약·알림·직접 입력의 enqueue도 source 순서로 세어 dequeue를 대응시킨다. queue receipt는 수신 증거뿐이다. 유일한 등록 attempt/source 범위·실제 native user/command row·기존 `history.rs:622`의 owner 판정을 결합한다. 같은 본문 twin/여러 후보는 Pending이지 임의 첫 후보 성공이 아니다. dequeue만 있는 구형 record는 FIFO 순서가 완전히 보존됐을 때만 연결한다.
4. **run membership.** enqueue→dequeue→user면 그 user가 연 run, `remove(absorbed_mid_turn)` 또는 대응 human queued_command면 **이미 열려 있는 run**에 입력을 가입시킨다. 두 record의 선후·중복·command 생략을 처리한다. 입력 하나는 run 하나에만 가입하며 native turn 하나에 여러 입력이 가입할 수 있다. 외부 run의 이름을 자기 turn_id로 바꾸지 않는다.
5. **종료 증거.** 같은 source·run membership 뒤의 strict closer만 `Executed` 후보를 만든다. assistant end_turn은 본문 봉인일 뿐이다. 현재 strict parser의 result/stop_hook_summary/turn_duration·interrupt 의미를 유지하고 연속 closer는 한 close다. prefix가 바뀌거나 source가 교체되면 저장된 offset을 새 파일에 쓰지 않는다. 입력별 응답 범위는 membership 이후 native unit이고 앞 외부 턴 본문은 자기 full_response에 합치지 않는다. mid-turn에서 가입 이전 부분은 자기 응답으로 소급하지 않는다. O가 외부 턴 자체를 게시하는 것은 정상이다.
6. **withdraw는 별도 결과.** 일반 remove/popAll은 정확한 대기 attempt만 `WithdrawnNotExecuted`로 확정해 기존 retained 안내로 알린다. 자동 재큐·재실행은 하지 않는다. `absorbed_mid_turn`은 철회가 아니고 `delivered_to_agent`는 `TransferredUnverified`다. command/remove가 짝을 이룬 뒤 늦은 dequeue가 다음 입력을 훔치지 않도록 existing removed accounting을 보존한다.
7. **단일 결과 → 정리 → 깨우기.** 새 `NativeInputSettlement` 경로가 ticket 전체를 finalizer에 제출한다. V2의 해당 원본/attempt에 최종 결과와 proof 범위/유형을 먼저 durable 기록(같은 key 멱등). 그 뒤 기존 episode+actor+monotonic-start CAS로 mailbox만 해제하고, 같은 captured row/nonce를 canonical sidecar lock 아래 지운다. lock/IO 실패는 결과를 되돌리거나 성공으로 덮지 않고 미완료 cleanup debt로 재개한다. **lease가 먼저 없어져도 row 정리가 끝나기 전 QueueEligible을 발행하지 않는다.** 결과·정리 debt는 inflight를 지운 뒤에도 원본 V2 기록에서 발견 가능해야 한다.
8. **O 수명과 delivery는 분리.** native settlement는 배송 receipt가 아니다. `Executed`/`WithdrawnNotExecuted`/`TransferredUnverified`/`Cancelled`/`OperatorReleased`를 구분한다. native release에는 O POST 완료를 요구하지 않고, 기존 writer ledger가 body·카드·footer 배송을 계속 책임진다. 기존 origin receipt·dispatch 성공·analytics 완료를 빈 응답으로 위조하지 않는다. delivery 의존 후처리는 기존 O disposition/receipt 관문이 확정할 때만 실행하며 QueueEligible에는 입력 정리 barrier만 사용한다. `CompletionAdmissionPlan`을 상위 호출자의 delivery 관문과 몰래 합치지 않고 별도 native admission 생성자/메시지로 연결한다.
9. **bridge는 호환 소비자로 축소.** intake/restore handoff에서 O ticket을 연결한다. ticket이 있는 episode는 외부 본문 축적·독자 Complete/GateTimeout 완료를 leaf에서 거절한다. O settlement를 받으면 stream loop는 `NativeSettled` 결과로 빠져 Cancel 후처리·provider Escape·pane 종료를 실행하지 않는다. generic cancelled flag를 원인으로 삼지 않는다. `run_post_loop_finalize` 진입 전 이미 정리된 episode 분기만 둔다. giant 파일에 새 귀속 FSM을 넣지 않는다. RuntimeReady·실행 전 명백한 실패는 기존 metadata/StartFailure 처리로 보낸다. any-key 이후 불확실 오류는 새 입력으로 재실행하지 않는다.
10. **큐 진행.** cleanup 완료는 captured episode의 MailboxReleased→QueueEligible 이벤트를 한 번 발행한다. 기존 channel-coalesced owner wake/drive를 사용하고 재확인 시 새 lease가 있으면 주입하지 않는다. 이벤트 수신 전에 idle이 됐거나 boot 때 이벤트를 잃었어도 durable cleanup debt/대기열을 재조회해 wake를 보충한다. 타이머로 입력을 다시 보내지 않는다.

### 영속화·경합의 실제 경계

기존 frame registry의 `Registration`에 optional `native_settlement`(ticket, outcome, proof range, cleanup state, warned episode)을 두고 기존 저장/lock 경계 안에서 갱신한다. root에 알 수 없는 JSON 필드만 붙이는 안은 사용하지 않는다. **별도 DB·전역 tombstone·무제한 새 journal을 추가하지 않는다.** 완료 원본의 기존 보존/compaction 주기에서 이 표식도 함께 보존하고, 해당 원본이 소거된 뒤 과거 event가 살아 있는 프로세스는 source/actor CAS로 거절한다. bounded 최근-N 캐시를 exactly-once 근거로 삼지 않는다.

**스키마·투영 보존:** `history/v2.rs:113`의 Registration, `:166`의 Snapshot, `:434`의 `Registry::project`와 `:135`의 `Passive::compact`가 같은 표식을 보존해야 한다(접두사 `src/services/tui_input/human_frame/`). passive 표식 변경 때 `encoded=None`으로 기존 encoding cache를 무효화한다. 결과 commit은 lock 아래 **기존 등록 key와 captured episode가 존재할 때만** 가능하며, 미등록/GC된 옛 증거에서 등록·완료를 새로 만들지 않는다. terminal_identities의 24시간 prune(`src/services/turn_orchestrator/queued_head/v2/retained.rs:78`)를 native exactly-once 기억으로 사용하지 않는다.

현재 writer는 알 수 없는 optional 필드를 typed round-trip에서 잃을 수 있다. 따라서 **frame registry envelope만 v3**로 쓴다(입력 queue SnapshotV2는 version2 유지). main의 `history/compat.rs:42`는 2 외 envelope를 쓰기 없이 거절하므로 구 writer가 v3 결과를 지울 수 없다. A에서 compat full/prefix reader·`v2/migration.rs::restore`와 typed encode가 v1/v2/v3를 읽고 표식을 보존하게 하되 새 표식이 없으면 v2를 계속 쓴다. B의 첫 native ticket 결과 저장부터 v3로 올리고 뒤의 모든 rewrite는 v3 유지다. 활성화 뒤 rollback 가능한 reader 하한은 **A**이며, 33d1f4b5로의 직접 rollback은 registry를 읽지 못하므로 지원하지 않는다. 운영자 변환 명령이나 수동 복구 절차를 새로 만들지 않는다. 실제 배포·rollback 결정은 이 설계 레그에서 수행하지 않는다.

필수 fixture는 v1/v2→v3 최초 결과, passive/project/compact 뒤 결과 유지, 표식 없는 구 snapshot의 늦은 project, cache 재사용, old main reader의 unsupported-envelope·파일 바이트 불변, 등록 제거 뒤 과거 proof 재제출이다. 새 reader를 먼저 착지시키는 A/B 순서가 추가되는 이유는 기존 writer의 필드 소거를 막기 위해서다. 저장 포트의 atomic 결과 보존을 구체화한 것이며 별도 tombstone 저장소를 추가하지 않는다.

현재 finalizer ledger는 재시작·GC를 넘는 영속 결과가 아니다. 따라서 result 저장을 단순 in-memory ledger에만 추가하면 안 된다. `prepare/bind/project` snapshot 쓰기와 같은 원본 key에 저장하고 receipt/settlement-before-delete crash fixture를 필수로 둔다. 기존 `claim_normal_episode`는 lease→receipt→row 순서이므로 새 helper를 호출했다는 사실만으로 atomic 완료라 주장하지 않는다. 모든 side effect 재개 지점은 저장된 결과에서 다시 출발한다. 결과가 저장된 후 새 입력이 claim한 경우에는 새 actor/nonce를 건드리지 않고 옛 row만 exact CAS로 정리한다.

WARN은 `(ticket, episode, source incarnation)`에 대해 native idle 관측이 계속되는데 결과/정리 debt가 남아 있는 경우 N분 뒤 한 번이다. 이 설계의 N은 **120초**(로그 관측 상수)로 고정한다. 운영 설정값은 **[U] 열람하지 않음**이며 새 운영 설정 키를 요구하지 않는다. 단조 clock·테스트 clock을 쓰며 새 user/open·source 변경은 idle 구간을 끝낸다. persisted warned 표식으로 재시작 spam을 막는다. 시간은 경보에만 쓰며 결과·lease 해제 증거를 만들지 않는다.

## 재시작·부팅·incarnation

| 경로 | 읽을 상태와 행동 | 금지/수용 시험 |
|---|---|---|
| 준비 후 key 전 crash | 기존 Reserved/NotSent 증거로만 기존 StartFailure 정책 | key 0 증명 없는 재전송 0 |
| enqueue 뒤·dequeue 전 crash | 같은 source를 ticket pre-write offset 이전의 필요한 FIFO 문맥부터 replay | queue receipt를 Executed로 승격 0 |
| command/absorption 뒤 closer 전 crash | native run membership 재구성; 관측 경계 resume는 carried state와 source를 함께 인증 | 새 입력으로 인식/두 run 가입 0 |
| 결과 저장 뒤 lease release 전 crash | 저장 결과로 같은 episode CAS 재개 | result count 1 |
| lease release 뒤 row clear 전 crash | cleanup debt와 exact row 유지; boot가 old lease를 재주입 근거로 쓰지 않음 | 후속 actor/nonce 보존, row 정리 뒤 head wake |
| row clear 뒤 wake 전 crash | V2 결과/cleanup debt와 원본 queue를 boot census에서 발견 | 알림 0건이어도 원래 head 진행 |
| 서버 boot generation 변경 | old born_generation은 새 admission identity가 아님. 결과 key는 원본 episode, 발행/actor는 현재 process | 과거 generation의 terminal이 successor를 풀지 않음 |
| herdr server/root 재생성·같은 pane 번호 재사용 | stored execution nonce/endpoint와 binding seq·dev/ino 비교; 옛 FD 증거는 옛 ticket만 대상으로 사용 | locator 문자열·현재 config만으로 증거 갈아끼우기 0 |
| V1/옛 inflight optional 필드 없음 | 기존 row+V2 registration+실제 parent source를 조합해 유일할 때만 ticket 복구 | `None`을 현재 source/성공으로 채우지 않음; herdr 없는 carried lookup의 tmux 제한을 복제하지 않음 |
| 옛 형식으로 귀속 복구 불가/파일 불가독 | 원본 보존·WARN 1회·기존 retained 상태. native 성공을 만들지 않음 | 이 경우 무조건 진행성은 **보장 불가**. 자료가 회복되면 다시 fold; 시간 자동 release 금지 |

마지막 행은 관측 가능한 native idle 증거도 입력 귀속도 영구히 얻을 수 없는 경우다. 모델 GREEN은 source/저장소가 결국 읽히고 native 이벤트가 계속 공급된다는 공정성 아래의 결론이며, 영구 증거 손실까지 “무한 대기 0”이라 하지 않는다. withdraw의 완전한 remove 증거는 이 예외가 아니다. 복구 불능 원본을 자동 처분하는 새 정책·수동 명령은 넣지 않으며 필요하면 사용자 결정 항목으로만 올린다.

## 같은 상태를 소비하는 인접 경로

전 저장소 전수는 [진입점 표](native-attribution.entrypoints.md)에 있다. 아래는 모델 값과 부작용이 결정되는 공통 actor 분기다. 새 native ticket 적용 episode는 각 mutation 전에 같은 결과 key/nonce를 소비하도록 구현해야 한다.

| 경로·코드 좌표 | 모델 행동·새 계약 |
|---|---|
| `src/services/turn_orchestrator.rs:1936`, `:2015`, `:2042` TryStart/Restore/RecoveryKickoff | Inject/BootRestore. old 결과가 있으면 새 턴으로 remint하지 않음 |
| `src/services/turn_orchestrator.rs:2381`, `:2400`, `:2408`, `:2435` Finish/IfToken/IfMatches/HardStop | Settle/AdjacentEnd. Complete는 O 증거 필요; Cancel은 별도 결과 |
| `src/services/turn_orchestrator/clear_channel.rs:161`, `checked_purge.rs:32`, `queued_head/v2/control.rs:111`, `policy_close.rs:348` | 명시적 clear/취소·정책 결과를 기존 원본에 기록. drop/clear를 native 성공으로 바꾸지 않음 |
| `src/services/discord/turn_lease.rs:298` 운영자 release | OperatorRelease. 먼저 이긴 결과 유지; 늦은 O Complete는 결과 덮기/재완료 0 |
| `src/services/discord/turn_finalizer/episode.rs:89`, `:136` | Settle/ProjectionClear. actor CAS와 row CAS 사이 crash/debt 보존 |
| `src/services/discord/turn_finalizer/completion_admission.rs:61`, `:89` | Kick. 입력 projection 정리·result 기록을 확인한 뒤 exactly-once wake claim |
| `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:602`, `queue_io/input_owner/drive.rs:88` | Kick/InjectHead. wake 병합 후 실제 head와 lease 재검증 |
| Cancel/stop: `src/services/discord/mailbox_finish.rs:54`, `:72`, `:126`; native stop `turn_bridge/tmux_runtime/owned_input_stop.rs:104` | AdjacentEnd. 실제 stop receipt/권한을 기존 경로가 확인; native proof와 경쟁해 한 결과만 채택 |
| Terminate: `src/services/discord/turn_finalizer.rs:276` HostTerminated; `turn_finalizer/episode/row_retirement.rs:55` | AdjacentEnd. exact 물리 실행의 사망 증거는 native 성공과 다름. 새 kill 동작 추가 0 |
| `/clear`, `!clear`: `src/services/discord/commands/control/clear_teardown/execution_cleanup.rs:431` | source/end 검증·기존 teardown 순서 유지. 늦은 input proof는 successor에 무효 |
| carried boot close: `src/services/discord/health/recovery/carried_turn_close.rs:146`, `:205`, `:240` | BootRestore. 현 세대 제한은 유지, 공통 귀속 leaf만 재사용. 옛 unverified 입력을 성공 처리하지 않음 |
| TB entry·tick·postloop: `bridge_entry_persist.rs:14`, `stream_loop.rs:815`, `post_loop_finalize.rs:95` (discord/turn_bridge 아래) | ProjectionSave/compatibility refusal. O ticket은 연결·거절·중립 종료만, late write는 기존 row fence |

## 관련 노드와 함수 접점

- **#8114** 착지 대기 head `41ace13c`는 이 기준 main에 없다는 브리프를 따른다. `provider_dispatch/owner_input.rs::{hand_busy, paste_busy, owner_drive, Registration::read_as, wake}`, `claude_tui/host_input/busy/herdr.rs`와 겹친다. busy TranscriptUnavailable 수리를 복제하지 않는다. busy 주입이 가능해지면 외부 run 합류가 더 자주 생기므로 #8148의 run membership은 별도 수용 조건이다. #8114의 sent/may-have-sent/awaited-clear 결과를 주입 실행 완료로 바꾸지 않는다.
- **#8128** r3 리뷰 중. `turn_lease.rs::{release_on, OperatorRelease::claim}`, `turn_finalizer/episode.rs::{claim_episode, claim_normal_episode}`, `recovery_engine/restore_inflight.rs`, `commands/control/clear_teardown/execution_cleanup.rs`의 cleanup 경계와 겹친다. #8128의 재시작/Terminate exact execution 증거를 소비하고 herdr recovery를 선취하지 않는다. actual locator/endpoint를 ticket에 전달하는 인터페이스만 합의한다.
- #7651 4a에 `NativeInputTicket + NativeInputSettlement + result-before-release + queue wake`를 정본 계약으로 넘긴다. 4b/4c 때 origin·headless 이전이 끝나면 compatibility adapter를 삭제한다. 새 TB 큐 parser를 남기지 않는다.

## 시험·관측으로 닫을 DoD

| DoD | 구현 레그의 RED → GREEN·뮤턴트 | 실제로 셀 값 |
|---|---|---|
| 1 큐 fixture | 고정 parent source에 scheduled_task_fire/enqueue(외부)/enqueue(ADK)/외부 end_turn+stop_hook_summary+turn_duration/dequeue/user(ADK)/자기 end+duration을 넣고 실제 InputFacts→fold→finalizer→mailbox/drive를 연결. 별도 fixture는 외부 진행 중 human command/absorption 선후 2종+중복+생략 | 외부 종료 직후 ADK result 0·lease1·row1, 자기 종료 뒤 result1·release1·row0·head 주입1. 자기 full_response에 외부 이전 body0, O 외부 출력은 보존. mutant: membership 제거/첫 closer 사용 → RED |
| 2 withdraw 무한 대기 0 | enqueue 후 remove 이유별, popAll, remove 뒤 늦은 dequeue, 같은 본문 twin, 외부가 계속 일하는 fixture. timer 없이 remove 읽은 뒤 명시 결과+안내, 큐 재전송0 | WithdrawnNotExecuted1·완료 성공0·원본/안내 보존·lease/row0. absorbed는 철회0, 다음 올바른 closer에서 Executed1. mutant: remove 무시/조용한 drop → RED |
| 3 native idle 미종결 WARN1 | fake monotonic clock N 직전/경계/직후, 새 user·source 변경, 폴링 반복, 재시작 뒤 같은 episode, cleanup IO 실패·복구. 경보 테스트에서 native proof를 주지 않고 timeout 종결0 확인 | 최초 적격 idle interval WARN1, 이후0; 시간 경과만으로 결과/lease 변화0. native 증거·저장소 복구 뒤 처리1 |
| 4 herdr relay e2e | 격리 herdr/시험 channel만: 외부 턴 먼저 시작→Discord 입력을 큐/합류/철회 각 경로로 전달→둘째 Discord head까지 실행. 재시작1회·동일 pane 번호 다른 execution·late old event 포함 | 실제 입력 attempt·native record 범위·게시 message ID·footer·lease GET·row/queue 증거. reply body/카드/footer 중복0·사용자 입력2개 실제 실행/결과2. 시나리오 분모0/skip은 UNEVALUATED |

공통 경합 시험: 결과 durable 전후, lease CAS 직전/직후, row CAS 직전/직후, wake 직전 각각 latch로 crash/Cancel/clear/operator release/후속 claim을 끼운다. `save_generation`만 변한 같은 episode와 nonce가 다른 같은 message-id를 모두 넣는다. 실제 store 실패·구형 schema fixture·source truncate/replacement·multipart closer를 검증한다. source equality를 지운 mutant와 wake를 지운 mutant를 각기 RED로 만든다. 원복 byte SHA를 비교한다.

이 레그에서는 Rust 시험·PG·실제 Claude·herdr e2e를 **실행하지 않았다**. TLC 결과를 위 DoD의 실제 통과로 바꾸지 않는다. E-62류 legacy 게시 기대값을 건드리지 않고 새 입력 귀속 시나리오를 추가하며, 이 조각에서는 O 본문/카드/푸터 기대값은 유지하고 native settlement/queue 진척 기대값만 바뀐다.

## 구현 크기와 리뷰 경계

예상 prod **16~20파일, 순증 +900~1,180**(불확실성 ±30%). 시험/fixture는 별도다. 이미 있는 O facts·V2 registry·finalizer를 재사용하는 추정이며 full lifecycle 이사가 아니다.

| 조각 | 표면·예상 순증 | 수용 경계 |
|---|---|---|
| A 귀속 fold/영속 결과 | O leaf 2~3개, native settlement leaf 2개, compat/migration reader·부모 연결: +500~650 | 큐·membership·withdraw·source/twin·replay, v3 reader 먼저. actor는 아직 활성화하지 않고 cfg(test)로 경고0 유지 |
| B 연결·정리·boot·WARN | discord native input lifecycle leaf 2~3개, finalizer leaf 2개, intake/restore/bridge 접점: +400~530 | 결과→CAS→row→wake, 중립 종료, crash·adjacent race·e2e |

전체가 +1,200을 넘으면 **A(순수 계약/fixture)와 B(활성화/복구)**를 서로 다른 리뷰 조각으로 나눈다. A에 휴면 production actor를 먼저 띄우지 않는다. giant `intake_turn.rs`(2,321줄), `turn_finalizer.rs`(4,887줄), `restore_inflight.rs`에는 import/mod·호출·typed 거절 분기만 둔다. `stream_loop.rs`는 973줄이므로 새 FSM을 넣어 1,000줄을 넘기지 않는다. registry는 큰 본문을 늘리지 않고 child leaf로 분리한다. census/baseline/allowlist 인상은 계획에 없다.

리뷰 집중 항목:

| 항목 | 막는 계약/검증 |
|---|---|
| 경계 시각 | wall-clock timestamp 순서 대신 동일 source의 완전한 줄 offset, run opener/closer; end_turn/summary/duration 분리 fixture |
| 동시성 경합 | durable 결과 key+episode/actor CAS+lock-held row CAS, 단계별 crash latches, late old proof와 successor 주입 경쟁 |
| 옛 형식 복원 | optional None은 Unknown, V1/V2→v3·old reader 거절·project/compact 보존 fixture·unique source 복구; 불가복구는 retained/WARN이며 성공0 |
| 문서로만 선언한 가드 | 실제 intake/restore/Cancel 경로→finalizer와 store leaf를 통과하는 시험, gate/source/wake 제거 mutant RED |
| 잘못된 0/PASS | TLC 선택 cfg/실행 수와 expected 깨짐을 비교; e2e는 비영 분모·message ID·row/lease 증거, PG/실기 미실행을 PASS로 쓰지 않음 |

운영자 새 수동 조작 기능과 대형 가드레일은 제안하지 않는다. 검증 불가능한 옛 입력의 자동 처분 정책이 필요해지면 위 한계와 비용을 사용자 결정 항목으로 올린다.
