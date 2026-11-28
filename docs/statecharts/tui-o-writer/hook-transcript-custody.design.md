# 재시작 뒤 원본 keyed ticket 정산 — design-3 (#8448 s3)

<!-- doc-path-refs: proposed src/services/tui_o/store/execution_end.rs src/services/tui_o/store/spool/witness.rs src/services/tui_input/human_frame/history/v2/native_end.rs src/services/discord/turn_finalizer/native_source_fate.rs src/services/discord/turn_finalizer/native_witness.rs src/services/discord/turn_finalizer/native_read_seal.rs src/services/discord/turn_finalizer/native_admission.rs src/services/discord/turn_finalizer/native_end_prover.rs src/services/discord/turn_finalizer/native_scope_fence.rs src/services/discord/turn_finalizer/native_end_release.rs src/services/tui_o/writer/input_facts/native/scoped.rs src/services/tui_o/writer/rotation/owe/historical.rs -->

재부팅으로 장치 번호(dev)가 바뀐 뒤 새 실행(N2)이 같은 transcript를 이을 때, 옛 실행(N1)의 keyed ticket을 어떻게 정산하는지에 대한 설계다.
design-2의 안전성 계약은 그대로 두고, 리뷰가 짚은 두 가지를 고친다.

- N1이 다시 오지 않을 때 원본 ticket이 영영 남아 채널 boot gate가 닫힌 채로 있는 문제(P1-A)
- 상태 차트가 실행 가능한 형태로 없고 채널 단위 liveness가 없던 문제(P1-B)

## 0. 기준

| 구분 | SHA |
|---|---|
| S2 (설계 기준 소스) | `bd0c385be24ca25a10725127dcd77eba1274d4fa` |
| main (조각 재추정 기준) | `5f0f2803e8864650b5e8deffbd3c61492ce5a86e` |

별도 표시가 없는 좌표는 S2 기준이고, 경로는 `src/services/` 아래를 줄여 적었다. main 좌표는 S2 파일과 줄이 달라서 "main N행"처럼 문장으로 적었다. 새로 만들 파일은 이 문서에서 제안하는 것이고 아직 없다.

이 문서는 설계만 담는다. 운영 코드는 바꾸지 않았고 빌드하지 않았다. 코드 확인은 고정 SHA의 소스를 읽은 것이다.

## 1. design-2에서 그대로 두는 계약

- 원본 ticket·namespace·key·channel·episode·target은 바꾸지 않는다.
- 저장 custody의 구조 검증만으로 전역 부팅을 진행한다. 구조 검증은 현재 파일 관측 권한을 주지 않는다.
- 새 receipt·barrier·outcome은 A(원본 실행 권한) + B(현재 파일 관측 권한) + C(원본 증거 범위)가 모두 있어야 한다.
- N1의 증거 범위는 N1이 실제 현재 실행일 때 저장한 실행별 prefix(이하 "원본 범위")로 고정하고, N1용 분석은 그 범위 이하의 완전한 record만 넣은 scoped 입력으로 한다.
- 새 Executed outcome에는 ReadSeal을 같은 CAS로 저장한다. completion 본문은 인증된 뒤에만 읽고, `record`/`record_live`의 반환값이 cleanup을 막는다.
- idle 삭제 권한(IdleAdmission)은 현재 idle의 target이 원본 target과 exact 일치할 때만 생긴다. N2의 idle은 N1 ticket을 정리하지 못한다.
- 원본 행 제거는 기존 exact row CAS다. N2의 행은 건드리지 않는다.
- 선두 receipt 후보가 막혀도 뒤 후보는 진행한다.

## 2. P1-A 출구 계약

### 2.1 출구가 N2에 기대면 안 되는 이유

채널 boot gate는 그 채널의 boot input이 모두 settled일 때만 열린다(`discord/turn_finalizer/native_prepare.rs:355-360`).
gate가 닫혀 있으면 새 턴 시작은 `Wait::Event`로 기다린다(`discord/router/message_handler/provider_dispatch/owner_input.rs:770-773`).
즉 사용자가 메시지를 보내도 그 채널에서는 새 실행이 뜨지 않는다. N2는 사람이 pane에 직접 입력하거나 host 복구가 띄울 때만 생긴다.

그래서 "N2가 뜨면 풀린다"는 출구는 gate와 서로를 기다리는 구조가 된다.
출구는 N2 없이도 열려야 하고, N2가 먼저 떠도 같은 결론이 나와야 한다.
상태 차트에서 N2의 launch와 입력에는 공정성을 주지 않았고, `NoBootProver` 변형(launch 때만 종료를 기록)이 `ChannelEventuallyReady`를 깬다.

### 2.2 종료 기록

N1 원본 target(`owner`, `execution_nonce`)이 끝났다는 사실을 채널별 durable 기록으로 남긴다.

**저장소.** 새 child `tui_o/store/execution_end.rs`(약 95줄)에 둔다. 형식과 쓰기 규약은 기존 `tui_o/store/clear_end.rs:24-40,99-126`과 같다(채널별 JSON 한 파일, 최대 16건·16 KiB, `durable::replace`, version watch).

| 필드 | 뜻 |
|---|---|
| `channel`, `provider`, `owner`, `execution_nonce` | 끝난 실행 |
| `source` | 그 실행이 쓰던 transcript의 `SourceId` (P5의 owner 최신 Bound에서 읽는다) |
| `binding_seq` | 기록 시점의 owner 최신 binding seq |
| `end_len` | 종료를 확인한 뒤 fence 아래에서 stat한 transcript 길이. 파일이 없거나 inode가 다르면 비운다 |
| `basis` | `herdr_retired` · `tmux_vacant` · `host_end` |
| `ended_at`, `host` | 기록 시각과 host 식별 |

기존 clear end 목록에 섞지 않는 이유: 그 목록은 `tui_o/writer/rotation/clear_end.rs:24-80`이 "clear 뒤 첫 bind"를 hop으로 증명하는 데 쓴다. clear가 아닌 종료를 넣으면 hop 판정이 바뀐다.
cursor에 넣지 않는 이유: launch 경로가 launch fence를 쥔 채 writer의 publication lock까지 기다리게 된다(§3).
이 파일은 채널별·유한 크기이고 다른 채널을 조회하지 않는다. 읽는 쪽은 native 정산뿐이다.

**쓰는 진입점.** 넷 다 "옛 실행이 끝났음을 host에서 확인한 뒤, 그 증거가 사라지기 전"에 쓴다.

| # | 진입점 | 언제 | 종료 근거 |
|---|---|---|---|
| E1 | Herdr launch: `herdr_launch.rs:492-498` (main 494~500행). 이미 있는 `takeover_ended` 호출 자리이고 owner port는 `herdr_launch/owner_port.rs:20`, `discord/herdr_owner_port.rs:9` | fence 획득(`herdr_launch.rs:455-459`)과 row 읽기(`herdr_launch.rs:460`) 뒤, 후속 Pending이 row를 덮기(`herdr_launch.rs:548-550`, main 552행) 전 | row가 `Retired`. launch는 `Retired`·`Legacy`가 아니면 `Occupied`로 거절한다(`herdr_launch.rs:508-509`) |
| E2 | tmux launch: `claude/tui_session_launch.rs:106-124` (fence 획득 `:106`, 옛 marker 삭제 `:122-124` 사이) | create/respawn(`:156-168`) 전 | owner가 비었음: tmux 세션이 없거나(`clear_end_record.rs:133-138`의 `vacant`와 같은 읽기), respawn이 받아들이는 dead pane(`platform/tmux.rs:200-216`) |
| E3 | legacy wrapper launch: `claude.rs:2600-2608` | 옛 marker 삭제(`:2605-2608`) 전 | E2와 같음 |
| E4 | 살아 있는 runtime의 종료 확인: native port의 `retry` (`discord/turn_finalizer/native_supply.rs:648-685`), writer poll이 매번 부른다(`tui_o/writer/rotation/owe.rs:579`, `tui_o/writer/actor.rs:441-444`) | outcome 없는 keyed boot input이 있고 그 target의 종료 기록이 없을 때 | Herdr: 현재 hosted row가 그 nonce이고 `execution_end`가 `Ended`(`discord/turn_finalizer/native_terminal.rs:101-137`)이거나 row가 `Retired`. tmux: P5의 owner 최신 Bound가 그 nonce이고 owner가 비었음 |

- 기록할 옛 실행이 없으면(owner의 Bound가 없음) 아무것도 쓰지 않고 진행한다.
- tmux 근거(빈 owner)는 그 nonce가 tmux에서 돈 실행일 때만 쓴다. 채널의 hosted row가 그 nonce를 가리키면 Herdr에서 돈 실행이므로 Herdr 근거(`Retired` 또는 `execution_end`)만 받는다. Herdr에서 tmux로 돌아오는 launch(`claude/herdr_return.rs:60-99`)에서 tmux owner는 처음부터 비어 있어서, 빈 owner만 보면 살아 있는 Herdr 실행을 끝난 것으로 적게 된다.
- E1~E3은 기록이 durable하게 써지지 않으면 launch를 거절한다. Herdr launch가 retired hold를 확인하고 실패하면 멈추는 것과 같은 방식이다(`herdr_launch.rs:499-502`).
- E4는 `RuntimeHintFence::try_acquire`(기다리지 않음, `tmux_common/runtime_hint_fence.rs:345-348`)를 잡고 "현재 실행이 아직 N1인지 → 종료 판독 → stat → 기록" 순서로 한다. fence가 바쁘면 그 poll은 건너뛴다.
- 어느 진입점도 marker가 없다는 사실만으로 종료를 쓰지 않는다. tmux launch는 create가 실패해도 옛 marker를 지운 채 끝날 수 있어서(`tmux_common.rs:1243-1272`), marker 부재는 종료 증명이 아니다. `EndWithoutProof` 변형이 이 지름길을 `EndProofOwned`로 잡는다.
- N2의 종료나 N2의 hosted row는 N1의 종료 근거가 아니다. E4는 row의 nonce가 원본 target과 다르면 쓰지 않는다(`discord/turn_finalizer/native_terminal.rs:122-129`와 같은 비교).

`Retired`를 종료 근거로 받는 이유: `Retired` CAS를 쓰는 곳은 모두 종료 판독 뒤에 있다.

| 쓰는 곳 | 앞선 판독 |
|---|---|
| `discord/herdr_terminate.rs:603-606` | `read_end` |
| `discord/herdr_terminate/restart_recovery.rs:143-152` | `execution_end == Ended` |
| `discord/herdr_terminate/root_replaced.rs:525-539` | 최종 증명과 quiet 확인 |
| `discord/recovery_engine/herdr_row_end.rs:503-510`, `herdr_row_end_live.rs:55-72` | `execution_gone` |
| `discord/recovery_engine/herdr_server_replaced.rs:270-286` | 배치된 pane이 lone shell |
| `claude/herdr_return.rs:82-89` | `/exit` 종료 대기 |
| `herdr_launch/retire.rs:70-104` | `ended` 판독 |
| `herdr_launch.rs:695-715` | pane 없는 Pending(실행된 적 없음) |

launch 자체가 같은 근거로 후속을 띄우고(`herdr_launch.rs:493`), takeover 확정도 `herdr_retired`를 종료로 쓴다(`discord/tui_direct_pending_start/turn_retirement/claude.rs:122-150`). root_replaced와 server_replaced의 판독 내용은 표본만 읽었다(§7).

### 2.3 원본 범위 확정

design-2는 N1이 현재 실행인 동안만 원본 범위를 늘린다. 그러면 N1이 마지막 capture 뒤에 쓴 closer는 N2가 먼저 뜨는 순간 범위 밖에 남는다(리뷰 표의 "closer 늦음" 경합).

design-3은 한 줄을 더한다.

> 종료 기록이 있으면 N1의 원본 범위를 `end_len` 이하의 마지막 완전한 record 경계까지 늘릴 수 있다. 그 너머로는 늘리지 않는다.

- `end_len`은 N1이 끝난 것을 확인한 뒤, 후속을 만들기 전에 fence 아래에서 잰 길이다. 그 앞의 byte는 후속이 쓴 것이 아니다.
- 늘릴 때도 B가 필요하다. writer가 현재 파일의 prefix를 인증해야 하고, 원본 범위의 hash가 이어져야 한다.
- 따로 읽지 않는다. reader의 일반 capture가 그 경계를 지날 때의 `through`와 prefix hash를 원본 범위에 적는다.
- 원본 범위가 그 경계에 닿으면 "확정"이다. 확정 뒤에는 N1의 증거가 더 생기지 않는다.
- `end_len`이 비어 있으면 늘리지 않는다. 그때의 원본 범위는 N2 시작 시점에서 얼어 있는 범위다.

이 규칙으로 closer가 파일에 있는 턴은 N2가 먼저 떠도 Executed로 정산된다. `NoEndCut` 변형(이 규칙 없음)은 `NoLostAnswer`와 `EligibleEventuallySettles`를 깬다.

### 2.4 종료 정산

outcome이 없는 keyed boot input T에 대해 아래가 모두 성립하면 T를 결과 없이 정산한다.

1. 종료 기록 E가 있고 `E.channel == T.channel_id`, `E.owner == T.source.target.owner`, `E.execution_nonce == T.source.target.execution_nonce`, E의 source가 T의 source와 같은 파일(session·path·inode)이다.
2. 원본 범위가 확정됐다(§2.3). 또는 원본 파일이 사라졌거나 바뀐 것이 확정됐다(§2.5).
3. 확정된 원본 범위의 scoped 분석에 T의 완전한 증거가 없다. 증거가 있으면 정산하지 않고 일반 offer가 Executed를 기록한다.
4. T의 registration fingerprint와 종료 기록·원본 범위 digest를 source fence와 publication snapshot 아래에서 다시 확인하고 Registry CAS를 한다.

정산은 이미 있는 두 경로로 끝난다. N2의 idle이나 N2의 suffix는 어느 조건에도 들어가지 않는다.

| T의 저장 상태 | 정산 | 정리 |
|---|---|---|
| barrier 보관 | 기존 abstain. `RestoredInput::abstain_idle`(`discord/turn_finalizer/native_restore.rs:122-201`)이 admission으로 "N1 자신의 idle" 대신 종료 기록을 받는다. terminal은 지금과 같은 `Empty` | 같은 함수의 exact row CAS(`:146-179`)와 `RestoreStep::Abstain` |
| barrier 없음 | Registry에 `TransferredUnverified` outcome을 `end_proof`와 함께 한 번 기록한다(새 child `tui_input/human_frame/history/v2/native_end.rs`). range는 없다 | 기존 `settle_saved` → `drive`(`discord/turn_finalizer/native_restore.rs:95-120,264-360`)의 exact row CAS와 cleanup ack |

- barrier 보관 건은 조건 2·3을 요구하지 않는다. barrier는 이미 "이 ticket의 결과는 귀속할 수 없다"는 저장된 판정이다. 조건 1과 4만 본다.
- `end_proof`는 E의 식별 필드와 확정 시점의 원본 범위(`through`, prefix hash) 또는 파일 운명을 담는다. `validate_record`(`discord/turn_finalizer/native_restore.rs:585-673`)에 `end_proof` 분기를 더해 target 일치, disposition, `any_key`를 검사한다.
- `TransferredUnverified`는 새 disposition이 아니다(`tui_input/human_frame/history/v2/native.rs:99-107`). 이미 `TerminalOutcome::Failed`로 매핑된다(`discord/turn_finalizer/terminal_handler.rs:548-560`).
- 대상은 boot input뿐이다. 살아 있는 actor가 있는 live input은 기존 제어 경로가 맡는다(`discord/turn_finalizer/native_policy.rs:23-38`).
- 구동하는 곳은 둘이다. 원본 범위가 확정된 건은 writer의 offer 순회가 scoped 관측과 같은 자리에서 요청한다. 파일이 Gone·Replaced·Changed인 건은 writer가 그 source를 읽을 수 없으므로 native port의 `retry`(`discord/turn_finalizer/native_supply.rs:648-685`)가 관측 없이 요청한다. 둘 다 같은 CAS로 끝난다.
- 다른 key·channel·namespace의 ticket은 조건 1에서 걸러진다. `AnyTarget` 변형이 `ExactTarget`을 깬다.

### 2.5 파일 운명과 PendingRead의 끝

원본 파일을 한 함수로 분류한다(새 child `discord/turn_finalizer/native_source_fate.rs`). 종료 정산과 completion 읽기가 같이 쓴다.

| 분류 | 판정 | 처리 |
|---|---|---|
| Present | path stat 성공, inode가 원본과 같음 | hash·seal 검사로 진행 |
| Gone | path가 `ENOENT`이고 부모 디렉터리는 stat됨 | 확정 |
| Replaced | path stat 성공, inode가 다름 | 확정 |
| Changed | inode는 같으나 길이가 저장된 `through`보다 짧거나 저장된 prefix hash와 다름 | 확정 |
| Unknown | 그 밖(부모 없음, 권한·IO 오류, fence busy) | 기다린다 |

completion의 반환값은 넷이다.

| 반환 | 뜻 | cleanup |
|---|---|---|
| `NotRequired` | follower·control, 또는 원본 행이 이미 없거나 교체됨 | 진행 |
| `Recorded` | 인증된 run을 읽어 기록함 | 진행 |
| `Skipped` | 파일이 Gone·Replaced·Changed이거나, ReadSeal이 없는 저장 결과인데 exact stat이 맞지 않음 | 진행. completion 기록은 만들지 않고 경고를 남긴다 |
| `Await` | Unknown | 멈춘다. 다음 retry에서 다시 본다 |

- `Skipped`는 지금의 정책과 같다. 지금도 run을 읽지 못하면 경고만 남기고 기록 없이 끝낸다(`discord/turn_finalizer/native_completion.rs:72-79`). 달라지는 것은 일시 실패(`Await`)와 확정 실패(`Skipped`)를 가르는 것뿐이다.
- ReadSeal이 있는 읽기의 권한은 내용 commitment다. path와 inode가 같고 `0..through`의 hash가 seal과 같으면 읽는다. 이 읽기는 writer의 진행에 기대지 않는다. 그래서 writer가 그 source를 보지 않아도 끝난다.
- ReadSeal이 없는 저장 결과(업그레이드 전에 저장된 것)는 exact stat이 맞으면 지금처럼 읽고, 맞지 않으면 `Skipped`다. 현재 파일의 hash를 과거 결과의 hash로 소급 저장하지 않는다.
- `PendingForever` 변형(확정 실패도 기다림)은 `ChannelEventuallyReady`를, `SkipTransient` 변형(일시 부재도 건너뜀)은 `SkipOnlyWhenTerminal`을 깬다.

### 2.6 같은 dev 비회귀

| 지금 풀리는 경우 | 지금의 경로 | design-3의 경로 |
|---|---|---|
| 같은 dev, 저장 barrier, N2가 idle | N2 idle → `abstain_idle` → `Empty` | N2 launch가 E1/E2로 종료를 기록 → 다음 poll에 종료 기록 admission으로 같은 `abstain_idle` → `Empty`. N2의 idle을 기다리지 않는다 |
| 같은 dev, N1이 턴 중간에 끝남, N2에 입력 뒤 idle | N2 입력이 N1 barrier를 만들고 N2 idle이 `Empty`로 정리 | 종료 기록 + 원본 범위 확정 → `TransferredUnverified`. terminal 기록이 `empty`에서 `failed`로 바뀐다 |
| 같은 dev, N1이 살아 있음(앱만 재시작) | N1 자신의 closer 또는 idle | 그대로 |
| 같은 dev, 저장 결과의 completion | exact stat으로 읽음 | 그대로 |

상태 차트의 `SameDevBarrierStillClears`가 첫 행을 고정한다. S2와 design-3에서 통과하고 design-2에서 깨진다.

### 2.7 사용자에게 보이는 영향

결과 없는 정산은 Discord에 아무 것도 보내지 않는다. 이미 게시된 출력은 그대로이고 다시 게시되지 않는다. 그 턴의 terminal 기록이 `failed`(barrier 보관 건은 지금과 같은 `empty`)로 남고 completion 기록(transcript 행·세션 selector·scheduled 결과·work dispatch)은 만들지 않는다. closer가 파일에 읽을 수 있게 남은 턴은 이 경로로 가지 않고 Executed로 정산되므로, 이 경로에서 잃는 답은 "끝나지 않은 턴"과 "transcript가 사라지거나 바뀐 턴"뿐이다.

새 운영자 명령이나 수동 스위치는 없다.

### 2.8 유지되는 불변식

| 불변식 | 지키는 방법 |
|---|---|
| IdleDoesNotExpand | 결과 없는 행 정리는 N1 자신의 idle 또는 N1의 종료 기록으로만 한다. N2 idle은 admission을 받지 못한다 |
| NoSuffixAttribution | scoped 입력은 원본 범위 이하, 원본 범위는 `end_len` 이하 |
| EndProofOwned | 종료 기록은 원본 target의 host 판독으로만 쓴다 |
| ReleaseNeedsEnd | 종료 admission과 종료 정산은 종료 기록을 CAS 시점에 다시 읽는다 |
| NoLostAnswer | 원본 범위가 확정되고 그 안에 증거가 없을 때만 결과 없이 정산한다 |
| CompletionBeforeCleanup, SkipOnlyWhenTerminal | 반환값 gate와 파일 운명 분류 |
| SuccessorUntouched, ExactTarget, OneOutcome, SavedOutcomePreserved | 기존 exact row CAS, exact ticket CAS, 단일 outcome CAS |

## 3. 반응 시간 계약

### 3.1 지금 있는 계약

| # | 계약 | 좌표 |
|---|---|---|
| T1 | writer poll 1초. native 공급은 매 poll 한 번 | `tui_o/writer/actor.rs:23`, `:441-444`, `tui_o/writer/rotation/owe.rs:575-579` |
| T2 | source당 한 poll에 최대 1 MiB를 읽는다. 끝나지 않은 record는 16 MiB에서 멈춘다 | `tui_o/shadow/mod.rs:17`, `tui_o/shadow/capture.rs:16` |
| T3 | `RuntimeHintFence::try_acquire`는 기다리지 않는다. 동기 `acquire`는 바쁘면 즉시 오류, `acquire_async`는 blocking pool에서 기다린다 | `tmux_common/runtime_hint_fence.rs:291-348`, 시험 `:931` |
| T4 | launch는 fence를 Herdr `herdr_launch.rs:455-577`, tmux `claude/tui_session_launch.rs:106` ~ `claude.rs:1835` 구간에서 쥔다 | 같은 좌표 |
| T5 | source authority는 세션별 프로세스 mutex이고 동기다. prompt relay·marker 게시·clear capture·`SourceFence::commit`이 같이 쓴다 | `tmux_common.rs:120-144`, `tmux_common/spawn_markers.rs:293-309`, `discord/turn_finalizer/native_supply.rs:66-75` |
| T6 | 전역 boot restore 재시도 5초, 경고 60초 | `discord/runtime_bootstrap/native_input.rs:6,9` |
| T7 | live completion은 run 범위만 한 번 읽고 바로 cleanup 메시지를 보낸다 | `discord/turn_finalizer/native_completion.rs:105-127`, `discord/turn_finalizer/actor_state.rs:341-365` |
| T8 | `append_spool`은 publication lock 없이 쓰고 fsync한다 | `tui_o/store/spool.rs:488-558` |

### 3.2 design-3이 지키는 것

| # | 동작 | fence·lock을 쥐는 구간 | 상한 |
|---|---|---|---|
| C1 | 원본 범위 저장 | `try_acquire` fence + source authority 아래에서 marker·P5 확인과 cursor 저장 한 번. 원본 범위는 cursor 파일에 실려 같이 교체되므로 fsync가 늘지 않는다 | 바쁘면 그 poll은 건너뛴다(대기 0). 추가 파일 읽기 0 |
| C2 | live completion | fence·lock 없음 | 전체 hash 없음. ReadSeal은 outcome CAS 때 writer가 이미 가진 증분 prefix hash로 만든다. 같은 프로세스에서 그 직후 읽는 live 경로는 seal을 만들 때의 stamp(dev·inode·길이)가 그대로면 지금처럼 run 범위만 한 번 읽는다(T7 유지) |
| C3 | boot·cross-dev completion | fence·authority·publication lock 없이 blocking pool에서 streaming hash 한 번. 읽기 전후 FD stamp 확인 | 미완료 저장 결과 하나당 부팅마다 `0..through` 한 번. 결과는 프로세스 안에서 (source, through, hash)로 기억한다 |
| C4 | publication lock | `append_spool`은 새로 잡지 않는다(T8 유지). commit이 이미 쥐는 publication snapshot에서 cursor의 원본 범위 digest를 관측 때의 값과 대조하고, 다르면 CAS를 거절해 다음 poll에 다시 한다 | 추가 대기 0 |
| C5 | 종료 기록 E1~E3 | 이미 쥔 launch fence 아래에서 stat 한 번과 16 KiB 이하 파일 교체 한 번 | launch에 작은 파일 fsync 한 번이 더해진다 |
| C6 | 종료 확인 E4 | `try_acquire` fence 아래에서 hosted row 읽기 + pane 판독 + stat + 기록. row·pane 판독은 2초에서 끊고 fence를 놓는다 | 미확인이면 다음 시도는 5초 뒤(T6과 같은 간격). outcome 없는 keyed boot input이 있고 종료 기록이 없는 채널만 한다 |
| C7 | 종료 기록 → gate 열림 | 없음 | barrier 보관 건은 다음 poll(1초 이내). 그 밖은 `end_len`까지 남은 byte를 T2 속도로 읽은 뒤 다음 poll |

기존 시험의 대기 창은 늘리지 않는다. 새 상수는 C6의 2초 판독 상한 하나이고, 재시도 간격은 T6을 쓴다.

## 4. 상태 소비 경로 전수표

행 1~45는 design-2의 표다. 좌표는 리뷰가 표본 검사했다. design-3에서 계약이 달라진 행은 ★로 표시했다. 행 46~58이 design-3에서 더한 것이다.

| # | 소비 경로 | S2 file:line | 계약 |
|---|---|---|---|
| 1 | 같은 transcript Herdr Resume | `tui_prompt_dedupe/binding_events/herdr_resume.rs:7-15,39-45` | 새 nonce, 원본 ticket 불변 |
| 2 | 같은 session hook | `tui_prompt_dedupe/runtime_binding/claude_source.rs:520-549` | 현재 source 입력. 역사적 실행 귀속 아님 |
| 3 | 같은 UUID Stop·prompt 재시도 | `claude_tui/hook_server/observation_ingress.rs:196-215,263-285` | S2 오류 전파·재시도 보존 |
| 4 | P5 적용·checkpoint | `tui_o/writer/rotation.rs:422-456` | 적용 완료 checkpoint에 관측 결합 |
| 5 | 현재 stat·prefix reopen | `tui_o/writer/rotation/source_refresh.rs:30-54` | 현재 exact stat과 prefix를 함께 인증 |
| 6 | source rekey·dev 계보 | `tui_o/store/source_refresh.rs:168-215` | source별 계보만 사용 |
| 7 | native seed | `tui_o/writer/input_facts/native.rs:247-276`, `tui_o/writer/rotation/owe.rs:350-398` | ★ 원본 범위(§2.3) 이하의 scoped 입력 |
| 8 | native origin 선택 | `tui_o/writer/rotation/owe.rs:425-457` (main 475~510행) | 현재 관측과 역사적 실행 분리 |
| 9 | registration source 일치 | `tui_o/writer/input_facts/native.rs:94-118`, `tui_input/human_frame/history/v2/native.rs:33-55` | provider·channel·session·path·inode 유지 |
| 10 | receipt 후보 분류 | `tui_o/writer/input_facts/native/receipt.rs:101-164` | 범위 제한을 유일성 판정 전에 적용 |
| 11 | 선두 receipt 차단 | `tui_o/writer/rotation/owe.rs:616-651` (main 738~777행) | 후보별 보류, 후속 진행 |
| 12 | source commit fence | `discord/turn_finalizer/native_supply.rs:66-75,109-159` | ★ 현재 source·계보·원본 범위 digest·원본 P5, 종료 정산은 종료 기록도 재확인 |
| 13 | receipt 공급·CAS | `discord/turn_finalizer/native_supply.rs:226-283`, `tui_input/human_frame/history/v2/native_receipt.rs:15-87` | A+B+C와 exact ticket·receipt |
| 14 | 새 barrier | `tui_o/writer/rotation/owe.rs:653-686`, `discord/turn_finalizer/native_supply.rs:336-398`, `tui_input/human_frame/history/v2/native_barrier.rs:116-185` | 저장과 release 분리 |
| 15 | 저장 barrier 조회 | `tui_input/human_frame/history/v2/native_barrier.rs:18-53`, `tui_input/human_frame/history/v2/read_stamp.rs:174-196` | 구조 검증, candidate=false 유지 |
| 16 | barrier supersession | `tui_input/human_frame/history/v2/native_barrier.rs:73-105`, `tui_o/writer/input_facts/native/terminal.rs:24-52` | digest·interrupt·range 동일성 유지 |
| 17 | registration cache | `tui_input/human_frame/history/v2/read_stamp.rs:174-239` | 저장 검증 cache가 live permit을 만들지 않음 |
| 18 | live·boot 새 outcome | `tui_o/writer/rotation/owe.rs:695-726`, `discord/turn_finalizer/native_supply.rs:458-575,759-831` | A+B+C, 원본 identity 불변 |
| 19 | outcome Registry CAS | `tui_input/human_frame/history/v2/native.rs:587-653`, `discord/turn_finalizer/native_supply.rs:307-325` | ★ exact 결과 멱등과 새 승인 분리. `end_proof` outcome은 전용 CAS(§2.4) |
| 20 | 전역 boot gate | `discord/runtime_bootstrap.rs:298-311`, `discord/runtime_bootstrap/native_input.rs:34-69,108-138` | 개별 관측 대기로 전역 boot를 막지 않음 |
| 21 | custody 보존·회수 | `discord/inflight/removal/native_input_custody.rs:86-216`, `tui_input/human_frame/history/v2/native.rs:200-275` | 저장 구조·manifest 검증 |
| 22 | boot capture·adoption | `discord/turn_finalizer/native_restore.rs:31-61,374-412`, `tui_input/human_frame/history/v2/native.rs:310-420,874-916` | 원본 key·episode·target·source 보존 |
| 23 | Register·channel gate | `discord/turn_finalizer/native_restore.rs:415-432,484-554`, `discord/turn_finalizer/native_prepare.rs:333-360` | ★ gate 정의는 그대로. 여는 출구에 종료 정산이 더해짐 |
| 24 | 원본 ticket 재조회 | `discord/turn_finalizer/native_restore.rs:73-92`, `tui_input/human_frame/history/v2/native.rs:681-762` | stored refresh에는 live stat 불필요 |
| 25 | 저장 outcome retry | `discord/turn_finalizer/native_supply.rs:190-215,307-325,648-684`, `discord/turn_finalizer/native_restore.rs:95-119` | 저장 결과 보존, 읽기 필요성만 별도 판정 |
| 26 | 저장 proof 검증 | `discord/turn_finalizer/native_restore.rs:585-672` | ★ `end_proof` 분기 추가 |
| 27 | 현재 idle 생산 | `tui_o/writer/rotation/owe.rs:728-749`, `discord/turn_finalizer/native_supply.rs:400-415` | 현재 실행에 귀속 |
| 28 | live retained Work 재구동 | `discord/turn_finalizer/native_supply.rs:416-442,285-287` | fresh idle마다 새 admission |
| 29 | boot abstain clear | `discord/turn_finalizer/native_supply.rs:444-453`, `discord/turn_finalizer/native_restore.rs:122-200` | ★ admission은 N1 자신의 idle 또는 N1의 종료 기록. 그 밖은 Empty·clear·settled 금지 |
| 30 | live abstain 부작용 | `discord/turn_finalizer/native_abstain.rs:11-38,54-188,194-254` | 첫 부작용 전 admission, 기존 actor·row CAS |
| 31 | boot completion | `discord/turn_finalizer/native_restore.rs:204-235,415-432` | ★ `Await`이면 Settle 금지, `Skipped`이면 진행 |
| 32 | live completion → drive | `discord/turn_finalizer/actor_state.rs:341-365,370-378`, `discord/turn_finalizer/native_completion.rs:85-102` | ★ `Await`이면 cleanup 메시지 금지, `Skipped`이면 진행 |
| 33 | completion 본문 읽기 | `discord/turn_finalizer/native_completion.rs:23-82,105-150`, `discord/turn_finalizer/native_terminal.rs:20-34` | ★ seal hash와 path·inode로 인증. 파일 운명 분류(§2.5) |
| 34 | 원본 row·clear CAS | `discord/turn_finalizer/native_restore.rs:241-253,264-351`, `discord/inflight/clear_store/snapshot.rs:16-41` | N1 정리 가능, N2 변경 금지 |
| 35 | raw EOF 후보 | `tui_o/writer/rotation/owe.rs:484-571` | 현재 tail을 역사적 fold에 넣지 않음 |
| 36 | raw EOF 종료 확인 | `discord/turn_finalizer/native_terminal.rs:76-97,101-136,139-197` | 원본 nonce·owner·root·host 종료 확인 유지 |
| 37 | Cancel·token finish | `turn_orchestrator/episode_identity.rs:278-315,424-449,719-747` | 현재 actor·episode. dev 계보 전달 금지 |
| 38 | 명시 제어 custody·CAS | `turn_orchestrator/policy_close.rs:72-88,341-376`, `discord/turn_finalizer/native_policy.rs:19-57,151-239`, `tui_input/human_frame/history/v2/native_control.rs:185-258` | 기존 control intent·row 권한 (행 47 참조) |
| 39 | Terminate | `discord/herdr_terminate.rs:396-420,436-478` | 현재 execution·owner·host 권한 유지 |
| 40 | clear | `turn_orchestrator/clear_channel.rs:113-153` | release 전 custody gate 유지 |
| 41 | purge | `turn_orchestrator/checked_purge.rs:14-41` | destructive gate·현재 actor 권한 유지 |
| 42 | 사용자 Stop | `turn_orchestrator/herdr_user_stop.rs:14-49,55-76` | 현재 stop intent, terminal 정산 별도 |
| 43 | registration·bind·attach/paste | `tui_input/human_frame/history/v2.rs:370-391,443-451,717-741,963-968`, `tui_input/human_frame/history/v2/native_binding.rs:20-40` | 현재 exact-stat 유지 |
| 44 | no-key 증명·control ACK | `discord/turn_finalizer/native_restore.rs:389-404,456-481`, `tui_input/human_frame/history/v2/native_control.rs:92-119,338-355` | exact no-key·any_key 상충 검사 유지 |
| 45 | 공통 dev 시험 seam | `tui_o/shadow/capture.rs:19-24`, `tui_input/human_frame/history.rs:186-195` | 두 소비자가 같은 dev 관측 |
| 46 | 두 번째 전역 boot gate | `discord/inflight/removal/boot_reaper.rs:83-92,119` → `boot_custody.rs:435-443` → `tui_input/human_frame/history/v2/native.rs:229-275`(`:266` validate) → `:158-176` → `tui_input/human_frame/history/v2/native_barrier.rs:18-21,41-54` → `tui_input/human_frame/history/v2.rs:21-33`. 실패하면 `discord/runtime_bootstrap/native_input.rs:42-57`이 `Refused` | 행 20과 같은 구조 검증. 한쪽만 고치면 `ReaperExact` 변형처럼 부팅이 막힌다 |
| 47 | 제어 경로의 공유 검증자 | `tui_input/human_frame/history/v2/native_control.rs:85,206` (main 86·110·224행) | 검증자가 구조 검증이 돼도 제어 권한은 live actor(`discord/turn_finalizer/native_policy.rs:23-38`)와 row CAS에서 온다. 제어 비회귀 시험을 둔다 |
| 48 | receipt 경로의 공유 검증자 | `tui_input/human_frame/history/v2/native_receipt.rs:26,43` (main 15·33행) | 새 귀속이므로 B 검사를 그대로 요구한다. 구조 검증으로 낮추지 않는다 |
| 49 | reply anchor membership (main) | main에만 있는 projection membership 모듈의 `Projection::memberships`(7~60행). S2에는 없다 | 현재 실행의 관측 소비자다. 전체 projection을 그대로 읽고 scoped 결과를 anchor에 쓰지 않는다 |
| 50 | notice absorption (main) | main에만 있는 projection notice absorption 모듈의 `removal`·`Window::new`(14~28행, 45행~)와 그것을 부르는 `reduce`(main의 input_facts native 본체 545~562행). S2에는 없다 | notice 정산은 현재 projection. 역사적 ticket의 window는 scoped fact slice 안에서만 계산된다 |
| 51 | raw EOF의 boot 분기 | `discord/turn_finalizer/native_terminal.rs:162-174`(`refreshed`), `:20-34`(`stamp`는 dev 재번호를 허용) | boot 분기도 B(source별 계보 + 인증 prefix)를 통과한 뒤에만 tail을 읽는다. N1이 현재 실행일 때만 성립(`:122-129`) |
| 52 | Herdr launch의 옛 실행 처리 | `herdr_launch.rs:455-460,492-509,548-550,560-577` | 종료 기록 E1 |
| 53 | tmux launch | `claude/tui_session_launch.rs:106-124,156-168`, `claude.rs:1828-1835`; legacy `claude.rs:2600-2608,2734-2743` | 종료 기록 E2·E3. marker 부재는 종료가 아님 |
| 54 | 기존 종료 기록과 그 소비자 | `tui_o/store/clear_end.rs:24-40,99-126`, `tui_o/writer/rotation/clear_end.rs:24-80`, `discord/tui_direct_pending_start/turn_retirement/claude.rs:74-80,91-120`, `tui_o/store/rotation.rs:21-30,482-495` | 바꾸지 않는다. 새 기록은 별도 파일이고 hop·takeover 판정에 들어가지 않는다 |
| 55 | host 종료 판독 | `discord/turn_finalizer/native_terminal.rs:101-137`, `discord/herdr_terminate.rs:191-228`, `discord/commands/control/clear_teardown/clear_end_record.rs:133-138` | 종료 기록 E4. 원본 target과 row nonce가 다르면 쓰지 않는다 |
| 56 | 채널 gate 소비자 | `discord/turn_finalizer/native_prepare.rs:355-360`, `discord/router/message_handler/provider_dispatch/owner_input.rs:770-773`, `owner_input/native_input.rs:210-212` | 변경 없음. gate가 턴 시작을 막으므로 출구가 N2에 기대지 않는다(§2.1) |
| 57 | terminal 기록 매핑 | `discord/turn_finalizer/terminal_handler.rs:548-560`, `discord/receipt_settle.rs:185-208` | `TransferredUnverified` → `failed`. Discord 게시 없음 |
| 58 | native 공급 poll | `tui_o/writer/actor.rs:23,441-444`, `tui_o/writer/rotation/owe.rs:575-579`, `discord/turn_finalizer/native_supply.rs:648-685` | E4와 종료 정산의 구동 지점 |

Cancel·Terminate·clear·purge·사용자 Stop·attach/paste(행 37~43)는 종료 기록을 읽지 않는다. 종료 기록의 소비자는 행 29와 §2.4의 정산뿐이다.

### 4.1 모델 변수와 표의 대응

| 모델 변수 | 값 | 가져온 행 |
|---|---|---|
| `scen` | complete, closerLate, killed, barrierStored, outcomeSaved, outcomeLegacy | 7·10(prefix 안의 membership·closer), 35(늦은 closer), 14·15(저장 barrier), 18·19·25(저장 결과) |
| `dev`, `file` | same·changed / ok·late·gone·changed | 5·6·9·33·43·45 |
| `n1`, `n2`, `rowN` | alive·dead / absent·running·idle / n1·n2 | 1·2·27·36·52·53 |
| `endRec` | 종료 기록 유무 | 52·53·55 |
| `witCloser`, `witFinal` | 원본 범위 안의 closer, 확정 여부 | 7·12 |
| `barrier` | 저장 barrier | 14·15·16 |
| `outcome`, `sealed`, `completion` | none·Executed·Empty·Unverified / seal 유무 / na·pending·recorded·skipped | 18·19·25·31·32·33 |
| `cleared`, `clearedBy`, `rowOwner`, `succRemoved` | 정리 여부와 근거, 채널 행의 주인 | 23·29·30·34 |
| `boot` | ready·refused | 20·46 |
| `t2` | 다른 target의 ticket | 13·19 |

행 37~42(제어)는 살아 있는 actor가 있어야 하므로 boot input 모델에 넣지 않았다. 행 47의 공유 검증자는 `boot`의 판정으로만 들어온다.

## 5. 상태 차트

모델과 실행 결과는 [모델 보고서](hook-transcript-custody.report.md)에 있다. 요약만 적는다.

- 모듈 `HookTranscriptCustody.tla`, 설정 25개. `Rules`가 S2(현재 코드)·D2(design-2)·D3(이 설계)를 고르고, `Mut`가 D3의 guard 하나를 끈다.
- 정상 설정 `HookTranscriptCustodyDesign.cfg`는 불변식 12개와 liveness 4개를 모두 통과한다(1,093 상태).
- S2는 5개, D2는 3개 규칙이 깨진다. 변형 16개는 각자 겨냥한 규칙에서 깨진다.
- `ChannelEventuallyReady`는 N2의 launch·입력과 N1의 죽음에 공정성을 주지 않은 채 통과한다.

## 6. 구현 조각

제품 조각 4개다. 추정은 main `5f0f2803e8` 기준 운영 코드 순증가이고 시험·모델·문서는 뺀다. 조각마다 상한(코드가 바뀐 prod 30파일, 순 코드 +1500, 정지선 +1200) 안이다.

| 조각 | 내용 | 추정 |
|---|---|---|
| s3-a | 공통 dev seam, 실행별 원본 범위, 종료 기록 저장소, launch 진입점 E1~E3. 읽는 쪽은 아직 없다 | 430 |
| s3-b | ReadSeal, 파일 운명 분류, completion 반환값 gate(live·boot), 저장 결과 retry | 200 |
| s3-c | 저장 검증 분리, 두 전역 boot gate, 개별 pending, idle·종료 admission, 종료 확인 E4, barrier 보관 건의 종료 정리 | 350 |
| s3-d | scoped 분석과 memo, 후보별 진행, 역사적 공급 활성화, 원본 범위 확정, barrier 없는 건의 종료 정산 | 580 |
| 합계 | 범위 약 1,250~1,900 | 1,560 |

design-2의 760에서 늘어난 이유는 셋이다. 출구(종료 기록·확인·정산) 약 450, giant 파일을 피하려고 child로 나눈 배치의 중복 약 150, s3-a와 s3-d의 과소 추정 보정 약 200.

### 6.1 순서가 지키는 것

- s3-a는 기록만 한다. 종료 기록이 쌓여도 아무도 읽지 않으므로 동작은 그대로다.
- s3-b 전에는 부팅의 live stat을 풀지 않는다.
- N2 idle의 정리 권한을 닫는 변경과 종료 admission은 같은 조각(s3-c)에 둔다. 따로 내면 같은 dev·저장 barrier 채널이 그 사이에 막힌다.
- N2 suffix가 N1 barrier를 만들지 않게 하는 변경(scoped 분석)과 barrier 없는 건의 종료 정산은 같은 조각(s3-d)에 둔다. 따로 내면 "N1이 턴 중간에 끝난" 채널의 지금 출구가 그 사이에 사라진다.

### 6.2 파일 배치 (main 기준)

1,000줄이 넘는 파일에는 호출 교체만 둔다.

| giant 파일 (main 줄 수) | 두는 것 | 본체가 가는 새 child |
|---|---|---|
| `tui_o/store/spool.rs` (1,170) | `Cursor`의 optional 필드, `append_spool`의 metadata-only 저장 호출(main 484~497행) | `tui_o/store/spool/witness.rs` |
| `tui_o/writer/input_facts/native.rs` (1,002) | `mod scoped;` 한 줄 | `tui_o/writer/input_facts/native/scoped.rs` |
| `tui_input/human_frame/history.rs` (1,182) | `file_id`(`:186-195`)가 `capture::file_identity`를 부르게 교체 | 없음 |
| `claude.rs` (4,108) | E3 호출 한 줄(`:2600-2608`) | `tui_o/store/execution_end.rs` |

| 조각 | 새 파일 | 고치는 기존 파일 |
|---|---|---|
| s3-a | `tui_o/store/spool/witness.rs`, `tui_o/store/execution_end.rs`, `discord/turn_finalizer/native_witness.rs` | `tui_o/store/spool.rs`, `tui_o/store/source_refresh.rs`, `tui_o/store/mod.rs`, `tui_o/writer/actor/native_input.rs`, `tui_input/human_frame/history.rs`, `herdr_launch.rs`, `herdr_launch/owner_port.rs`, `discord/herdr_owner_port.rs`, `claude/tui_session_launch.rs`, `claude.rs` |
| s3-b | `discord/turn_finalizer/native_read_seal.rs`, `discord/turn_finalizer/native_source_fate.rs` | `discord/turn_finalizer/native_completion.rs`, `discord/turn_finalizer/actor_state.rs`, `discord/turn_finalizer/native_restore.rs`, `discord/turn_finalizer/native_supply.rs`, `discord/turn_finalizer/native_prepare.rs` |
| s3-c | `discord/turn_finalizer/native_admission.rs`, `discord/turn_finalizer/native_end_prover.rs` | `tui_input/human_frame/history/v2.rs`, `tui_input/human_frame/history/v2/native.rs`, `tui_input/human_frame/history/v2/native_barrier.rs`, `discord/turn_finalizer/native_supply.rs`, `discord/turn_finalizer/native_restore.rs`, `discord/turn_finalizer/native_abstain.rs`, `discord/turn_finalizer/native_terminal.rs` |
| s3-d | `tui_o/writer/input_facts/native/scoped.rs`, `tui_o/writer/rotation/owe/historical.rs`, `discord/turn_finalizer/native_scope_fence.rs`, `discord/turn_finalizer/native_end_release.rs`, `tui_input/human_frame/history/v2/native_end.rs` | `tui_o/writer/rotation/owe.rs`, `tui_o/writer/rotation/owe/observe_memo.rs`, `tui_o/writer/actor/native_input.rs`, `discord/turn_finalizer/native_supply.rs`, `discord/turn_finalizer/native_restore.rs`, `tui_input/human_frame/history/v2/native_receipt.rs`, `tui_input/human_frame/history/v2/native.rs` |

main에서 달라진 것을 반영했다.

- `tui_o/writer/rotation/owe.rs`는 770줄에서 898줄이 됐다. 후보별 순회 본체는 `owe/historical.rs`에 두고, main 738~777행의 선두 후보 분기를 호출로 바꾼다.
- `tui_input/human_frame/history/v2/native_receipt.rs`와 `tui_input/human_frame/history/v2/native_control.rs`는 main에서 대부분 다시 쓰였다. 검증자 호출은 main에서 receipt 쪽 15·33행, control 쪽 86·110·224행이다.
- `discord/turn_finalizer/native_supply.rs`의 idle 순회는 main 388~441행, boot 순회는 main 432~439행이다.
- `Projection`은 `facts`를 가지고 있고 `reduce`는 fact slice를 받는다(`tui_o/writer/input_facts/native.rs:340-352`, main의 `Projection::memberships`도 같은 호출). scoped 분석은 재투영이 아니라 `range.1 <= cut`인 앞부분 slice로 `reduce`를 부르는 것이다.
- memo는 slot이 하나다(`tui_o/writer/rotation/owe/observe_memo.rs:11-20`). 역사적 분석용 slot을 하나 더 두고 key에 cut과 원본 범위 digest를 넣는다. 현재 분석과 번갈아 와도 서로 밀어내지 않는다.
- S2가 main에 착지한 뒤 조각을 시작한다. `tui_o/store/source_refresh.rs`는 S2에만 있다.

### 6.3 조각별 제품 RED

모든 fixture는 공통 dev seam으로 dev를 바꾼다. `tui_input/human_frame/history.rs:186-195`의 `file_id`가 `capture::file_identity`(`tui_o/shadow/capture.rs:19-25`)를 부르게 하면 `Source::validate`·`stamp`·`Fingerprint`가 같은 재번호를 본다.
fixture는 먼저 writer와 native history가 둘 다 새 dev를 본다는 것을 양으로 단언한다. ticket JSON만 고치는 fixture는 수용 시험으로 세지 않는다.

| 조각 | 주입 | 단언 |
|---|---|---|
| s3-a | N1 capture → dev 변경 → N2 marker와 P5 지연. tmux fence busy(`runtime_hint_fence.rs:931`의 방식). spool fsync와 cursor 저장 사이 crash(새 `cfg(test)` hook) | 두 stat이 새 dev. N1 원본 범위의 상한·hash 불변. record 없는 seed도 저장됨. 복구 뒤 소급 확장 0 |
| s3-a | Herdr launch에서 row가 `Retired`인 N1 뒤 N2. tmux launch에서 빈 owner 뒤 N2. 종료 기록 쓰기 실패 주입 | 후속 생성 호출 전에 N1의 종료 기록이 디스크에 있음. 쓰기 실패면 launch 거절, 후속 0. marker만 없는 상태에서는 기록 0 |
| s3-b | D1에서 outcome과 ReadSeal 저장 → dev 변경 reboot → 인증 지연, 거절, 성공 | outcome 동일. 인증 전 본문 기록·terminal 기록·clear·ack 0. 인증 뒤 정확한 본문과 원본 cleanup |
| s3-b | 저장 결과가 있는데 transcript 삭제, 다른 inode로 교체, 일시 부재 | 삭제·교체: completion 기록 없이 cleanup이 끝나고 채널 gate가 열림. 일시 부재: cleanup 0, 파일이 돌아오면 기록 뒤 cleanup |
| s3-c | 저장 barrier → dev 변경 reboot, 파일 부재 | 전역 boot가 두 gate(`boot_reaper` 경로와 `BootInputs::prepare`) 모두에서 진행 |
| s3-c | **같은 dev + 저장 barrier, N1 종료, N2가 뜨고 idle** | N2 idle만으로는 Empty·clear 0. N1 종료 기록 뒤 원본 행이 `Empty`로 정리되고 `boot_ready(channel)`이 true. 종료 기록이 없으면 gate는 닫힌 채 |
| s3-c | 저장 barrier, N1 종료, N2 없음 | 종료 확인 E4가 기록을 쓰고 gate가 열림 |
| s3-c | 제어 비회귀: live actor의 Cancel·OperatorReleased | 검증자가 바뀐 뒤에도 결과 동일 |
| s3-d | **dev 변경, N1이 턴 중간에 종료, N2 Resume·입력·idle** | `boot_ready(channel)`이 결국 true. N1 ticket은 `TransferredUnverified` 정확히 1회, 원본 행 정리, N2 행 불변, N2 idle 시점의 clear 0 |
| s3-d | 같은 fixture에서 N1의 closer가 마지막 capture 뒤에 있고 N2가 먼저 뜸 | 원본 N1 `Executed` 정확히 1회. 결과 없는 정산 0 |
| s3-d | 봉인된 N1 prefix → dev 변경 뒤 N2 Resume·refresh. 선두 후보 차단 fixture 별도 | 원본 N1 `Executed` 1회. 뒤 후보 완료. 다른 key·channel·namespace 불변. 중복·재게시 0 |
| s3-d | 종료 기록의 nonce만 다른 ticket, 종료 기록 없는 ticket | 정산 0 |

"N2 idle에서 Empty·clear 0"만 단언하는 시험은 두지 않는다. 그 단언은 항상 "종료 기록 뒤에는 gate가 열린다"와 짝으로 둔다.

### 6.4 변형으로 확인할 guard

| 끄는 검사 | RED여야 하는 단언 |
|---|---|
| 종료 기록의 target 비교 | nonce만 다른 ticket 정산 0 |
| 종료 확인의 host 판독(marker 부재로 대체) | 살아 있는 N1의 종료 기록 0 |
| launch 전 종료 기록 | N2가 먼저 뜬 fixture의 gate 열림 |
| 원본 범위 확정 대기 | 늦은 closer의 `Executed` |
| `end_len` 상한 | N2 suffix 귀속 0 |
| 종료 admission을 N2 idle로 대체 | N2 idle 시점의 clear 0 |
| 파일 운명의 확정·일시 구분 | 일시 부재의 cleanup 0, 삭제의 gate 열림 |
| completion 반환값 gate | `Await`에서 원본 행 보존 |
| 원본 row CAS | N2 행 보존 |
| 두 번째 boot gate의 구조 검증 | reaper 경로의 boot 진행 |

## 7. 남은 위험

- **Herdr endpoint가 돌아오지 않는 경우.** Herdr N1의 종료는 pane 판독 또는 `Retired` row로만 증명한다. 재부팅 뒤 Herdr 서버가 끝내 뜨지 않고 기존 server-replaced·restart 복구도 row를 `Retired`로 만들지 못하면 종료 기록이 생기지 않고 그 채널 gate는 닫힌 채다. 증명 없이 여는 대안은 `EndProofOwned`를 깬다. 모델은 "host 판독이 결국 답한다"를 공정성으로 가정했다.
- **실행의 host 구분.** tmux 근거와 Herdr 근거를 가르는 기준을 "채널 hosted row가 그 nonce를 가리키는가"로 적었다. ticket target 안에서 host를 직접 가르는 필드가 있는지는 확인하지 못했다. s3-a에서 실행 context와 대조해 정한다.
- **`Retired` 근거의 표본 확인.** `Retired`를 쓰는 8곳 가운데 `root_replaced`와 `herdr_server_replaced`는 판독 내용을 호출 이름과 주변 코드로만 확인했다. s3-a 구현 전에 두 곳의 종료 판독을 끝까지 읽어야 한다.
- **`end_len`을 재지 못한 종료 기록.** 파일이 그 시점에 없거나 inode가 다르면 `end_len`이 빈다. 그 뒤 파일이 온전하게 돌아와도 원본 범위는 N2 시작 시점에서 멈추고, 그 뒤의 closer는 귀속되지 않는다. 이때의 턴은 결과 없이 정산된다.
- **`Gone` 판정.** 부모 디렉터리가 있고 파일만 없으면 확정으로 본다. 파일이 나중에 같은 path로 돌아오는 환경에서는 completion 기록을 잃는다. 돌아온 파일의 출처를 증명할 방법이 없어서 기다리지 않기로 했다.
- **업그레이드 전에 저장된 상태.** ReadSeal 없는 저장 결과는 dev가 바뀌면 completion 기록 없이 끝난다. 원본 범위 없이 이미 N2로 넘어간 cursor의 N1 ticket은 증거를 새로 만들 수 없어 종료 기록 뒤 결과 없이 정산된다. 둘 다 전환 시점 한 번의 모집단이고 호환 장치는 넣지 않는다.
- **`failed` terminal의 하류.** scheduled·work dispatch 기원 턴이 결과 없이 정산되면 그 기원에는 completion이 기록되지 않는다. 지금의 `TransferredUnverified`와 같은 처리이고 이 설계에서 새로 다루지 않았다.
- **종료 기록 16건 상한.** 한 채널에서 미정산 ticket이 남은 채 실행이 16번 더 끝나면 가장 오래된 기록이 밀려난다. 그 ticket은 E4로 다시 증명할 수 없으면(후속이 이미 떴으면) 남는다. gate가 턴 시작을 막고 있어 현실적이지 않다고 보지만 모델에는 없다.
- **rollback.** 종료 기록 파일과 `end_proof` outcome은 옛 바이너리가 읽지 못한다. 옛 바이너리로 되돌리면 `end_proof`가 든 미정리 boot record에서 부팅 검증이 실패한다.
- **모델의 추상화.** hash·byte 변조, FD/path 경합, Registry fingerprint, launch 경합의 실제 순서, Herdr와 tmux의 차이는 모델에 없고 §6.3의 제품 RED가 맡는다. 모델은 ticket 하나와 후속 실행 하나만 다룬다.
- **실행 재현 없음.** S2·D2 반례는 고정 SHA 소스와 design-2 본문을 읽어 확인했다. 빌드가 금지된 설계 레그라 실행으로 재현하지 않았다.
