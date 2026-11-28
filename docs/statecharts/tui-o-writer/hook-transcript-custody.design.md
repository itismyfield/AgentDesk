# 재시작 뒤 원본 keyed ticket 정산 — design-5 (#8448 s3, S3a)

<!-- doc-path-refs: proposed src/services/tui_o/store/execution_end.rs src/services/tui_o/store/spool/witness.rs src/services/tui_input/human_frame/history/v2/native_end.rs src/services/discord/turn_finalizer/native_source_fate.rs src/services/discord/turn_finalizer/native_read_seal.rs src/services/discord/turn_finalizer/native_admission.rs src/services/discord/turn_finalizer/native_end_prover.rs src/services/discord/turn_finalizer/native_boot_debt.rs src/services/discord/turn_finalizer/native_end_release.rs src/services/discord/turn_finalizer/native_end_raw.rs src/services/discord/turn_finalizer/native_superseded.rs src/services/discord/turn_finalizer/native_scope_fence.rs src/services/tui_o/writer/input_facts/native/scoped.rs src/services/discord/turn_finalizer/native_end_scan.rs src/services/tui_o/writer/rotation/source_refresh/end_proof.rs -->

재부팅으로 장치 번호(dev)가 바뀐 뒤 새 실행(N2)이 같은 transcript를 이을 때, 옛 실행(N1)의 keyed ticket을 어떻게 정산하는지에 대한 설계다.
design-4의 단계 분리(S3a 먼저, S3b 나중)는 리뷰가 받아들였다. design-5는 **S3a만** 닫는다.

- design-4 리뷰 P1 가운데 S3a에 속한 두 건을 닫는다. 하나는 S3a가 지금은 재시도하던 파일 접근 실패를 복구할 수 없는 정산으로 바꾸던 것이다(§2.6). 다른 하나는 다른 source 때문에 writer가 멈추면 정상 파일의 ticket도 정산되지 않던 것이다(§2.5).
- P2 가운데 S3a 계약을 바꾸는 두 건(completion 실행 주체, 역사적 raw EOF의 frontier)은 이번에 반영한다(§2.4, §2.6). 나머지는 §9의 후속이다.
- S3b(새 Bound만 생기고 refresh hook이 없는 B 생산, cut 없는 역사적 ticket)는 S3a 착지 뒤 다음 라운드다(§3). 이 문서는 S3b를 풀지 않는다.

## 0. 기준

| 구분 | SHA |
|---|---|
| S2 (설계 기준 소스) | `bd0c385be24ca25a10725127dcd77eba1274d4fa` |
| main (조각 재추정 기준) | `5f0f2803e8864650b5e8deffbd3c61492ce5a86e` |

별도 표시가 없는 좌표는 S2 기준이고, 경로는 `src/services/` 아래를 줄여 적었다. main 좌표는 S2 파일과 줄이 달라서 "main N행"처럼 문장으로 적었다. 새로 만들 파일은 이 문서에서 제안하는 것이고 아직 없다.

이 문서는 설계만 담는다. 운영 코드는 바꾸지 않았고 빌드하지 않았다. 코드 확인은 고정 SHA의 소스를 읽은 것이다.

## 1. 단계 분리

### 1.1 그대로 두는 계약

design-2·3의 계약 가운데 리뷰가 받아들인 것은 바꾸지 않는다.

- 원본 ticket·namespace·key·channel·episode·target은 바꾸지 않는다.
- 저장 custody의 구조 검증만으로 전역 부팅을 진행한다. 구조 검증은 현재 파일 관측 권한을 주지 않는다.
- 새 receipt·barrier·outcome은 A(원본 실행 권한) + B(현재 파일 관측 권한) + C(원본 증거 범위)가 모두 있어야 한다.
- 새 Executed outcome에는 ReadSeal을 같은 CAS로 저장한다. completion 본문은 인증된 뒤에만 읽고, `record`/`record_live`의 반환값이 cleanup을 막는다.
- 원본 행 제거는 기존 exact row CAS다. N2의 행은 건드리지 않는다. 선두 receipt 후보가 막혀도 뒤 후보는 진행한다.
- 결과 없는 정산은 Discord에 아무 것도 보내지 않는다. 새 운영자 명령이나 수동 스위치는 없다.

리뷰의 "판단해 줄 지점" 1~7은 아래처럼 반영한다.

| # | 지점 | 리뷰 판정 | 반영 |
|---|---|---|---|
| 1 | ReadSeal 읽기에서 dev 계보 제거 | 수용 | 그대로(§2.6). 새 귀속의 B는 다음 라운드(§3) |
| 2 | 채널별 `execution_end.rs` | 수용 | 그대로. 실행 증명과 source별 cut은 다음 라운드(§3) |
| 3 | `Retired`를 종료 근거로 사용 | 일괄 수용 곤란(P2-1) | 근거별 증명만 받는다(§2.2) |
| 4 | 종료 길이까지 범위 연장 | 필요 | S3a에서 `end_len`을 cut으로 쓰고 개행 없는 closer까지 본다(§2.3·§2.4). B 생산은 S3b(§3.1) |
| 5 | durable 기록 실패 시 launch 거절 | 수용 | 그대로. 그래서 S3a 첫 조각은 dormant가 아니다(§6) |
| 6 | terminal `empty` → `failed` | 수용 | 그대로. 입력 출처별 하류 시험을 둔다(§7.3) |
| 7 | 실행 host 구분 | target 필드 사용으로 확정 | target의 `host`·`owner`·`execution_nonce`·`root`로 가른다(§2.2) |

### 1.2 S3a 단독 비회귀

S3a만 main에 있을 때 각 경로가 지금(S2)보다 나빠지지 않는다는 근거다. design-4 리뷰는 "규칙 이름이 같다"로는 비회귀가 증명되지 않는다고 했다. 그래서 위험 경로마다 같은 모집단을 S2·design-4 S3a(`S3a4`)·design-5 S3a에 각각 돌린 cfg를 둔다(§6).

| 경로 | S2 | design-4 S3a | design-5 S3a | cfg |
|---|---|---|---|---|
| 같은 dev, 저장 barrier, N2 idle | N2 idle로 `Empty` | 같음 | 같음. 종료 기록이 있으면 더 일찍 `Empty` | `BarrierS2`·`BarrierS3a` |
| 같은 dev, N1 중간 종료, N2 입력 뒤 idle | N2 idle로 `Empty` | 같음 | 같음 | `KilledS2`·`KilledS3a` |
| **저장 결과, transcript가 잠시 stat되지 않음 (P1-1)** | exact stat 검사가 retry job을 막고 다음 retry에서 다시 봄. 파일이 돌아오면 completion 1회 | **ENOENT를 Gone으로 보고 completion 없이 cleanup(유실)** | S2와 같이 기다리고, 돌아오면 completion 1회 | `TransientS2`·`TransientS3a4`·`TransientS3a` |
| outcome 없는 ticket, transcript가 잠시 stat되지 않음 (P1-1) | 기다림 | 결과 없이 정산(closer가 있으면 유실) | 기다림 | 같음 |
| transcript가 끝내 stat되지 않음 | 영구 대기 | 결과 없이 정리 | 영구 대기(S2와 같음, §3) | `GoneS2`·`GoneS3a` |
| seal 없는 저장 결과, dev 변경 | 영구 대기 | completion 없이 cleanup | 영구 대기(S2와 같음, §3) | `LegacyDevS2`·`LegacyDevS3a` |
| **다른 source cursor의 reopen 실패로 writer 정지, 이 ticket의 파일은 정상 (P1-4)** | writer가 멈춰 영구 대기 | 같음(범위 안 귀속이 writer에 남음) | finalizer가 이 source를 따로 읽어 정산 | `OtherHaltS2`·`OtherHaltS3a4`·`OtherHaltS3a` |
| 이 ticket의 파일이 Changed라 writer 정지 | 영구 대기 | 파일 없이 정산 | 같음 | `HaltedS2`·`HaltedS3a` |
| dev 변경, 파일 승인 없음, N2 없음 | 영구 대기 | 파일을 읽어야 하는 건은 대기 | 같음 | `NoN2S2`·`NoN2S3a` |
| 설치 전 이미 N2 | N2 idle(같은 dev barrier) 말고는 대기 | 같음 | 같음 | `SupersededS2`·`SupersededS3a` |
| 같은 N1의 앞 source ticket | 지금의 출구 | 같음 | 같음 | `SwitchedS2`·`SwitchedS3a` |
| 개행 없는 closer, N2 먼저 | 대기 | `end_len` 안의 raw closer로 `Executed` | 같음. 단 현재 frontier를 빌리지 않는다(§2.4) | `RawCloserS2`·`RawCloserS3a` |
| stat은 되는데 completion 읽기가 실패 | 경고 뒤 cleanup | Unreadable이면 건너뜀 | S2와 같음 | 모델 밖(제품 RED) |
| 종료 기록 쓰기 실패 | 없음 | launch 거절 | 같음. 다음 launch에서 다시 시도하는 오류이고 영구 보류는 아니다 | 모델 밖(제품 RED) |

S3a는 S2의 N2 idle 정리(같은 dev exact 경로)를 남긴다. 그래서 `IdleDoesNotExpand`는 S3a에서도 S2처럼 깨진다. S2의 기존 공백이고, S3a가 넓히지 않는다는 것은 `IdleNotExpandedCrossDev` 통과로 보인다.

### 1.3 P1 해소 표

design-4 리뷰 P1 네 건이다(리뷰 번호).

| 리뷰 P1 | 이번 처리 | 해소 | 근거 좌표 (S2) | 모델 |
|---|---|---|---|---|
| P1-1 S3a의 파일 운명 분류가 재시도 경로를 비가역 정산으로 바꿈 | **S3a, 해소** | path가 stat되지 않는 모든 경우(ENOENT, 부모 디렉터리 유무, 권한, EIO, EINTR)를 S2처럼 기다린다. 결과 없는 정산과 completion 건너뜀은 다른 inode, 짧아진 길이, seal hash 불일치라는 적극적인 증거가 있을 때만 한다(§2.6·§2.7) | S2의 exact stat 검사 `discord/turn_finalizer/native_restore.rs:73-92`, `tui_input/human_frame/history/v2.rs:21-35`, retry job 조건 `discord/turn_finalizer/native_supply.rs:664-675` | `TransientS3a4` RED → `TransientS3a` GREEN, `S3aEnoentTerminal` 변형 RED |
| P1-4 다른 source 때문에 writer가 멈추면 정상 source의 정산이 막힘 | **S3a, 해소** | 종료 기록과 `end_len`이 있는 boot debt의 범위 안 분석·역사적 raw EOF·cut 있는 결과 없는 정산을 finalizer가 source마다 따로 한다. 그 source의 transcript를 `Projection::reopen`으로 다시 읽어 저장 prefix hash를 확인하고 `end_len`까지만 분석한다. writer와 다른 cursor에 기대지 않는다(§2.5) | 전체 복구가 cursor 하나의 실패로 멈춤 `tui_o/writer/rotation/readers.rs:75-95,121-138`, `tui_o/writer/actor.rs:367-370,441-453`. 재사용 `tui_o/writer/input_facts/native.rs:249-275` | `OtherHaltS3a4` RED → `OtherHaltS3a` GREEN, `S3aScanInWriter` 변형 RED |
| P1-2 새 Bound만 생기고 refresh hook이 없는 경우의 B | 다음 라운드(S3b) | 풀지 않음(§3) | — | — |
| P1-3 cut 없는 역사적 ticket의 readable closer 유실·승인 공백 | 다음 라운드(S3b) | 풀지 않음(§3) | — | — |

S3a만 main에 있는 동안 다음 라운드 두 건의 운영 영향은 지금과 같다. 해당 채널은 지금처럼 gate가 닫힌 채 남거나, 지금 있는 출구(같은 dev N2 idle, N1 자신의 idle)로 풀린다. S3a는 cut이 없는 ticket을 정산하지 않으므로 P1-3의 유실 경로를 열지 않는다.

## 2. S3a 계약

### 2.1 출구가 N2에 기대면 안 되는 이유

채널 boot gate는 그 채널의 boot input이 모두 settled일 때만 열린다(`discord/turn_finalizer/native_prepare.rs:355-360`).
gate가 닫혀 있으면 새 턴 시작은 `Wait::Event`로 기다린다(`discord/router/message_handler/provider_dispatch/owner_input.rs:770-773`).
N2는 사람이 pane에 직접 입력하거나 host 복구가 띄울 때만 생긴다. 상태 차트는 N2의 launch·입력과 N1의 죽음에 공정성을 주지 않는다.

### 2.2 종료 기록

N1 원본 target(`owner`, `execution_nonce`)이 끝났다는 사실을 채널별 durable 기록으로 남긴다.

**저장소.** 새 child `tui_o/store/execution_end.rs`에 둔다. 형식과 쓰기 규약은 `tui_o/store/clear_end.rs:24-40,99-126`과 같다(채널별 JSON 한 파일, 최대 16건·16 KiB, `durable::replace`, version watch). clear hop 판정(`tui_o/writer/rotation/clear_end.rs:24-80`)에 섞지 않으려고 별도 파일이다.

| 필드 | 뜻 |
|---|---|
| `channel`, `provider`, `owner`, `execution_nonce`, `host` | 끝난 실행. `host`는 ticket target의 `host` 필드(`discord/turn_finalizer/native_prepare.rs:29-31`)를 그대로 쓴다 |
| `source` | 기록 시점에 그 실행이 쓰던 transcript의 `SourceId`(P5의 owner 최신 Bound) |
| `binding_seq` | 기록 시점의 owner 최신 binding seq |
| `end_len` | 종료를 확인한 뒤 fence 아래에서 stat한 `source`의 길이. 파일이 없거나 inode가 다르면 비운다 |
| `basis` | `herdr_ended` · `tmux_vacant` · `tmux_closed` · `host_end` (S3b에서 `superseded`) |
| `ended_at` | 기록 시각 |

**host 구분.** 실행이 Herdr인지 tmux인지는 ticket target의 `host`로 정한다. target은 이미 `host`·`owner`·`execution_nonce`·`root`를 가지고 있고 boot 검증이 그것을 비교한다(`discord/turn_finalizer/native_prepare.rs:29-31,85-95`). 채널 hosted row가 같은 nonce인지로 host를 역추정하지 않는다.

**근거별 증명.** `Retired` row 하나를 일괄 종료로 받지 않는다. `Retired`를 쓰는 경로의 증명 강도가 같지 않기 때문이다. `herdr_launch/retire.rs:51-61,78-97`은 `view.ended`로 `Retired`를 쓰는데, 그 판독은 pane Missing이나 같은 root의 lone shell에서 바로 참이 된다(`session_host/herdr_gate.rs:932-943`). provider process 부재까지 보는 것은 `discord/herdr_terminate.rs:191-205`의 `read_end`다.

| 근거 | 쓰는 조건 |
|---|---|
| `herdr_ended` | `read_end`와 같은 판독: pane 종료 판독 + `provider_absent()`가 참 |
| `tmux_vacant` | `discord/commands/control/clear_teardown/clear_end_record.rs:133-138`의 `vacant`: 세션 없음, session PID 없음, 원본 process identity 종료, process group 종료 |
| `tmux_closed` | handoff의 `session_handoff::pinned_tmux::end`가 `Ok`로 끝남(`claude/herdr_turn/handoff.rs:99-110`) |
| `host_end` | 살아 있는 runtime에서 target host로 위 둘 중 하나를 확인함(E4) |

**쓰는 진입점.**

| # | 진입점 | 언제 | 근거 |
|---|---|---|---|
| E1 | Herdr launch `herdr_launch.rs:492-498`. owner port는 `herdr_launch/owner_port.rs:20`, `discord/herdr_owner_port.rs:9` | fence 획득(`herdr_launch.rs:455-459`)과 row 읽기(`:460`) 뒤, 후속 Pending이 row를 덮기(`:548-550`) 전 | row가 `Retired`일 때 `herdr_ended`를 다시 판독한다. 참이 아니면 기록하지 않고 launch는 지금처럼 진행한다 |
| E2 | tmux launch `claude/tui_session_launch.rs:106-124` | create/respawn(`:156-168`) 전 | `tmux_vacant` |
| E3 | legacy wrapper launch `claude.rs:2600-2608` | 옛 marker 삭제 전 | `tmux_vacant` |
| E5 | tmux → Herdr 첫 전환 `claude/herdr_turn/handoff.rs:99-110` | `pinned_tmux::end` 성공 뒤, Herdr launch(`:153-160`) 전 | `tmux_closed`. Herdr launch는 이 경로의 row가 `Legacy`라서 E1의 `Retired` 분기를 타지 않는다(`herdr_launch.rs:508-509`) |
| E4 | boot debt 구동(§2.5) | outcome 없는 keyed boot input이 있고 그 target의 종료 기록이 없을 때 | target `host`가 Herdr면 `herdr_ended` 또는 기존 `execution_end == Ended`(`discord/turn_finalizer/native_terminal.rs:101-137`), tmux면 `tmux_vacant`. 현재 row·최신 Bound의 nonce가 target과 다르면 쓰지 않는다 |

- 기록할 옛 실행이 없으면(owner의 Bound가 없음) 아무것도 쓰지 않는다.
- E1~E3·E5는 기록을 쓰기로 정한 뒤 durable하게 써지지 않으면 launch를 거절한다. 근거가 약해서 쓰지 않는 경우는 거절하지 않는다.
- 어느 진입점도 marker가 없다는 사실만으로 종료를 쓰지 않는다(`tmux_common.rs:1243-1272`). `EndWithoutProof` 변형이 `EndProofOwned`로 잡는다.
- N2의 종료나 N2의 hosted row는 N1의 종료 근거가 아니다.

### 2.3 cut과 범위 안 귀속

S3a의 cut은 종료 기록의 `end_len`이다. 실행별 원본 범위(design-3 §2.3의 witness)는 S3a에 넣지 않는다. N1이 끝난 것을 확인한 뒤 후속을 만들기 전에 잰 길이라서, 그 앞의 byte는 후속이 쓴 것이 아니다.

T가 종료 기록 E와 맞으면(§2.7의 조건 1) N1용 분석은 `range.1 <= end_len`인 fact slice로 `reduce`를 부른 scoped 입력이다. 이 읽기에도 B가 필요하다. S3a의 B는 지금 있는 것뿐이다.

- 같은 dev이고 exact stat이 맞음
- 또는 S2 refresh가 이미 만든 승인 계보(`tui_o/store/source_refresh.rs:195-199`)와 인증 prefix

B가 없으면 S3a는 기다린다(다음 라운드, §3). `end_len`이 비면 cut이 없으므로 S3a는 범위 안 귀속도 결과 없는 정산도 하지 않는다(S3b, §3.4).

### 2.4 역사적 raw EOF

일반 capture는 개행까지를 record로 만든다. 개행 없이 끝난 온전한 JSON은 partial에 남는다(`tui_o/shadow/capture.rs:389-419`). 지금의 raw EOF 인증은 현재 hosted row가 원본 N1일 때만 성립한다(`discord/turn_finalizer/native_terminal.rs:122-129`).

S3a는 종료 기록에 묶인 raw EOF를 더한다(새 child `discord/turn_finalizer/native_end_raw.rs`).

- 조건: E가 T와 맞고 `end_len`이 있으며 B가 있다.
- 읽기: 마지막 완전 record 경계 `through`부터 `end_len`까지만 읽는다. 그 바깥(현재 EOF, N2가 쓴 byte)은 읽지 않는다.
- 판정: 그 tail이 기존 strict closer parser(`tui_o/writer/input_facts/native/terminal.rs:122-136,153-167`)를 통과하면 기존과 같은 `Executed` 또는 `Cancelled` 증거로 봉인한다. 범위는 `end_len`에 묶는다.
- 현재 EOF나 N2의 hosted row를 빌리지 않는다.
- 현재 capture frontier도 빌리지 않는다. 기존 `within()`(`tui_o/writer/input_facts/native/terminal.rs:170-179`)은 raw witness의 `through`를 현재 관측의 `through`와 비교한다. 역사적 경로는 §2.5의 source scan이 `end_len`에서 끊은 자기 projection의 `through`만 쓴다. N2가 suffix를 써서 현재 frontier가 `end_len`보다 앞서간 경우가 제품 RED에 들어간다(§7.3).

결과 없는 정산(§2.7)은 raw tail도 증거로 본다. `end_len` 안에 strict closer tail이 있으면 결과 없이 정산하지 않는다. `NoHistRaw` 변형은 `ChannelEventuallyReady`를 깨고, 설계 이전 규칙(D3)은 `RawCloserD3`에서 `NoLostAnswer`를 깬다.

### 2.5 boot debt 구동과 source별 scan

writer는 cursor 하나의 reopen이 Changed·다른 inode면 전체 복구를 멈춘다(`tui_o/writer/rotation/readers.rs:75-95`는 채널의 cursor를 모두 돌고, `:121-138`의 오류가 그대로 올라간다). 그 poll 뒤 writer는 루프를 끝낸다(`tui_o/writer/actor.rs:367-370,441-453`). host는 `HaltedMissingSource`만 재개한다(`tui_o/writer/host.rs:701-712`, `tui_o/writer/host/missing.rs:67-68`). 그래서 boot debt를 writer 수명에 두면, 그 ticket의 파일이 멀쩡해도 다른 source 하나 때문에 정산이 멈춘다.

S3a는 boot debt를 모두 finalizer가 구동하고, 파일을 읽어야 하는 일도 source마다 따로 한다.

- **주인.** turn finalizer actor의 reconcile tick(1초, `discord/turn_finalizer.rs:103,600-601`)이다. tick이 `native_policy::reconcile`을 부르는 자리(`discord/turn_finalizer/reconcile.rs:231-240`) 옆에 boot input 순회를 더한다.
- **tick 준비.** tick은 `cached_shared`가 있어야 돈다(`discord/turn_finalizer.rs:764-765`). `FinalizeMsg::NativeBoot` 처리(`discord/turn_finalizer/actor_state.rs:130-135`, 이미 `shared`를 가짐)에서 `actor_state.rs:290-298`과 같은 `is_none` 규칙으로 채운다.
- **한 번의 시도.** 채널마다 시도 하나만 `spawn`한다. tick은 기다리지 않는다. 같은 채널의 다음 시도는 앞 시도가 끝나고 5초 뒤다(`discord/runtime_bootstrap/native_input.rs:6`의 간격).
- **시도 내용**(새 child `discord/turn_finalizer/native_boot_debt.rs`). ticket마다 독립으로 한다. 한 ticket의 실패는 그 ticket만 다음 시도로 넘기고 뒤 ticket은 계속 본다.
  1. 종료 기록이 없으면 E4.
  2. 파일 판정(§2.6).
  3. 저장 barrier 건의 종료 정리(파일을 읽지 않음).
  4. 저장 결과의 completion(§2.6).
  5. 종료 기록과 `end_len`이 있고 B가 있는 건의 **source scan**(새 child `discord/turn_finalizer/native_end_scan.rs`).
     - 그 ticket의 source만 `Projection::reopen`(`tui_o/writer/input_facts/native.rs:249-275`)으로 다시 연다. 이 함수는 저장 cursor의 `captured_through`까지 prefix hash를 확인하며 fact를 만든다.
     - 이어서 `end_len`까지의 완전한 record만 더 읽는다.
     - `range.1 <= end_len`인 fact slice로 `reduce`를 부른다(§2.3). raw tail은 §2.4다.
     - 증거가 있으면 기존 Registry CAS로 `Executed`를 기록하고, 없으면 §2.7의 결과 없는 정산을 한다.
     - cursor는 읽기만 한다. writer의 spool·cursor·memo에는 쓰지 않는다.
- **실행 주체 하나.** 종료 기록이 있는 boot ticket의 역사적 경로는 finalizer만 한다. writer의 offer 순회에는 현재 실행 경로(N1이 현재일 때의 S2 경로)만 남는다. design-4가 writer 쪽 rotation 아래에 두려던 역사적 후보 순회 child와 memo slot은 만들지 않는다.
- **다른 source의 실패.** scan은 그 ticket의 source만 연다. 다른 cursor가 Changed라도 이 source의 scan은 영향을 받지 않는다. 이 source 자신이 stat되지 않으면 그 ticket만 기다린다.

상태 차트에서 writer는 그 ticket의 파일이 Changed이거나 다른 cursor의 reopen이 실패하면(`otherBad`) 공정하게 멈춘다. S3a의 `EndScan`과 정산은 writer 조건이 없다. `S3aScanInWriter` 변형(scan을 writer에 둠)은 `ChannelEventuallyReady`를 깬다. 이 변형이 design-4의 동작이다.

### 2.6 파일 판정과 completion gate

S2의 저장 결과 completion은 모두 exact stat 검사 뒤에 들어간다. 진입점은 셋이다.

- `refreshed()`가 `source.validate()`를 부르는 `settle_saved`(`discord/turn_finalizer/native_restore.rs:95-102`. 호출자는 `discord/turn_finalizer/native_supply.rs:325,540`).
- boot restore의 `prepare_completion`(`discord/turn_finalizer/native_restore.rs:418-430`). 이 경로는 boot 준비가 같은 검증을 거친 뒤다.
- retry job 생성(`discord/turn_finalizer/native_supply.rs:664-675`).

`Source::validate`(`tui_input/human_frame/history/v2.rs:21-35`)는 metadata 읽기 실패, dev·inode 불일치, 길이 부족을 모두 거절하고, 그러면 job을 만들지 않는다. 즉 S2에서 path가 stat되지 않으면 기다린다. design-4는 이 앞단을 구조 검증으로 낮추고 ENOENT를 Gone으로 보아 cleanup까지 진행했다. 그래서 잠시 사라졌다 돌아오는 파일의 completion을 잃었다.

design-5는 S2의 판정을 그대로 두고, S2가 영구히 기다리던 두 경우만 바꾼다(새 child `discord/turn_finalizer/native_source_fate.rs`).

| 관측 | S2 | S3a |
|---|---|---|
| path stat 실패(ENOENT·부모 없음·권한·EIO·EINTR 등 모든 errno) | job 없음, 다음 retry | **같음**: `Await` |
| fence busy | — | `Await` |
| stat 성공, dev·inode 같음, 길이 ≥ `through` | completion 읽기. 읽기가 실패하면 경고 뒤 cleanup | 같음. seal이 있으면 `0..through` hash가 seal과 같을 때 읽고, 다르면 `Skipped` |
| stat 성공, inode 같음, dev만 다름, seal 있음, hash 같음 | 영구 대기 | 읽기(`Recorded`). 내용 commitment로 인증(리뷰 판정 1) |
| stat 성공, inode 같음, dev만 다름, seal 없음 | 영구 대기 | **같음**: `Await` |
| stat 성공, inode 다름 | 영구 대기 | `Skipped`. path가 다른 파일을 가리킨다는 적극적인 증거 |
| stat 성공, inode 같음, 길이 < `through` | 영구 대기 | `Skipped`. 저장된 prefix가 잘렸다는 증거 |

- `Skipped`는 원본 내용이 그 path에서 사라졌다는 적극적인 증거가 있을 때만이다. "지금 읽을 수 없다"는 판정에는 쓰지 않는다. 리뷰의 권고("현재 읽기 불가와 원본 증거의 영구 소실을 구별")를 따른 것이다.
- stat 성공 뒤의 completion 읽기 실패는 S2와 같이 경고 뒤 진행한다(`discord/turn_finalizer/native_completion.rs:72-79`). 이것을 `Await`로 바꾸면 stat은 되는데 읽기가 계속 실패하는 파일(권한 000 등)에서 새 영구 보류가 생긴다.
- **completion 실행 주체 하나**(P2-2). completion은 row CAS 전에 transcript DB 저장·selector·scheduled·work dispatch 부작용을 실행한다(`discord/turn_finalizer/native_completion.rs:215-283`). writer의 새 outcome 직후 정리(`discord/turn_finalizer/native_supply.rs:325,540`)와 finalizer 시도가 같은 ticket을 겹쳐 진행하지 않게 한다. 방법은 finalizer actor의 직렬 메시지 처리를 쓰는 것이다. completion 전에 `NativeBoot` 계열 메시지로 ledger entry의 boot input에 "completion 진행 중" 표시를 CAS로 잡고, 끝나거나 실패하면 푼다. 표시가 이미 있으면 그 시도는 건너뛴다.
- 저장 결과의 retry는 `refreshed`를 구조 검증(ticket identity·record)과 위 판정으로 나눈다. 위 판정이 `Await`면 지금처럼 job을 만들지 않는다.
- `PendingForever` 변형은 `ChannelEventuallyReady`를 깬다. `SkipTransient`·`S3aEnoentTerminal` 변형(stat되지 않는 path를 확정으로 봄)은 `NoSilentLoss`와 `SkipOnlyWhenTerminal`을 깬다.

### 2.7 종료 정산

outcome이 없는 keyed boot input T에 대해 아래가 성립하면 정산한다.

1. 종료 기록 E가 있고 `E.channel == T.channel_id`, `E.owner`·`E.execution_nonce`·`E.host`가 T의 target과 같고, `E.source`가 T의 source와 같은 파일(session·path·inode)이다. 다른 source는 다음 라운드다(§3).
2. 다음 중 하나다.
   - T가 저장 barrier 건이다. 파일을 읽지 않는다.
   - 파일 판정이 "inode 다름" 또는 "길이 < 저장 `through`"다(§2.6의 적극적인 증거). path가 stat되지 않으면 기다린다.
   - `end_len`이 있고 B가 있으며, §2.5의 source scan에서 `0..end_len`의 scoped 분석과 raw tail에 T의 완전한 증거가 없다. 증거가 있으면 정산하지 않고 `Executed`를 기록한다.
3. T의 registration fingerprint와 종료 기록 digest를 source fence와 publication snapshot 아래에서 다시 확인하고 Registry CAS를 한다.

| T의 저장 상태 | 정산 | 정리 |
|---|---|---|
| barrier 보관 | 기존 abstain. `RestoredInput::abstain_idle`(`discord/turn_finalizer/native_restore.rs:122-201`)이 admission으로 N1 자신의 idle 외에 종료 기록도 받는다(새 child `discord/turn_finalizer/native_admission.rs`). terminal은 지금과 같은 `Empty` | 같은 함수의 exact row CAS(`:146-179`) |
| barrier 없음 | `TransferredUnverified` outcome을 `end_proof`와 함께 한 번 기록한다(새 child `tui_input/human_frame/history/v2/native_end.rs`, 판정은 `discord/turn_finalizer/native_end_release.rs`) | 기존 `settle_saved` → `drive`(`discord/turn_finalizer/native_restore.rs:95-120,264-360`) |

- `end_proof`는 E의 식별 필드, `basis`, 그리고 cut(`end_len`과 그때의 prefix hash) 또는 파일 판정을 담는다. `validate_record`(`discord/turn_finalizer/native_restore.rs:585-673`)에 `end_proof` 분기를 더한다.
- `TransferredUnverified`는 기존 disposition이고 `TerminalOutcome::Failed`로 매핑된다(`discord/turn_finalizer/terminal_handler.rs:548-560`).
- 대상은 boot input뿐이다. live input은 기존 제어 경로가 맡는다(`discord/turn_finalizer/native_policy.rs:23-38`).
- S2의 N2 idle 정리(같은 dev exact 경로)는 S3a에 그대로 있다(§1.2).

### 2.8 전역 boot gate

두 전역 boot gate 모두 구조 검증으로 진행한다. 첫째는 `BootInputs::prepare`, 둘째는 `discord/inflight/removal/boot_reaper.rs:83-92,119` → `boot_custody.rs:435-443` → `tui_input/human_frame/history/v2/native.rs:229-275`다. abstain boot 순회의 `?`(`discord/turn_finalizer/native_supply.rs:447`)는 입력 하나의 실패로 순회 전체가 끝나므로 입력별 건너뜀으로 바꾼다. `ReaperExact` 변형이 한쪽만 고친 경우를 잡는다.

### 2.9 유지되는 불변식 (S3a 범위)

| 불변식 | 지키는 방법 |
|---|---|
| IdleNotExpandedCrossDev | dev가 바뀐 경우 N2 idle은 admission을 받지 못한다. 같은 dev의 S2 경로는 남긴다 |
| NoSuffixAttribution | scoped 입력과 raw tail은 `end_len` 이하 |
| RejectUnverifiedFile | 범위 안 귀속은 B(같은 dev exact 또는 기존 승인 계보)가 있을 때만 |
| EndProofOwned | 종료 기록은 근거별 host 판독으로만 쓴다 |
| ReleaseNeedsEnd | 종료 admission과 정산은 종료 기록을 CAS 시점에 다시 읽는다 |
| NoSilentLoss | completion 건너뜀과 closer 있는 턴의 결과 없는 정산은 원본 내용이 사라졌다는 적극적인 증거가 있을 때만 |
| NoLostAnswer | cut 안에 증거(raw tail 포함)가 없을 때만 결과 없이 정산한다 |
| CompletionBeforeCleanup, SkipOnlyWhenTerminal | 반환값 gate와 파일 운명 분류 |
| SuccessorUntouched, ExactTarget, OneOutcome, SavedOutcomePreserved | 기존 exact row CAS, exact ticket CAS, 단일 outcome CAS |

모델 규칙 이름의 실제 검사 범위는 [모델 보고서](hook-transcript-custody.report.md)에 적었다(P2-4).

## 3. 다음 라운드 (S3b)

이번 라운드에서는 풀지 않는다. design-4 §3의 S3b 계약(`EndRefresh`, `superseded`, source별 cut, cut 없는 건, N2 idle 정리 닫기)은 출발점으로 남기고, S3a 착지 뒤 다음 항목과 함께 다시 설계한다.

- **리뷰 P1-2**: N1이 끝난 뒤 launch·Resume Bound만 생기고 refresh hook이 없으면 `EndRefresh`의 "새 Bound 없음" 조건이 막는다.
- **리뷰 P1-3**: cut 없는 역사적 ticket(`superseded`, 업그레이드 전 모집단, source 재사용)에서 읽을 수 있는 closer를 잃지 않는 방법과 승인 공백.
- **design-5가 S3a에서 S2처럼 남긴 공백 둘**:
  - path가 끝내 stat되지 않는 ticket(`GoneS3a`). 원본이 영구히 사라졌다는 증거 없이는 정산하지 않는다.
  - dev가 바뀐 seal 없는 저장 결과(`LegacyDevS3a`).
- **리뷰 P2-1**: `superseded`의 증명 강도. E1이 확인하지 못한 종료를 후속 launch만으로 증명하지 않는다.
- 모델의 `Full` 규칙은 design-4의 S3b를 design-5 S3a 위에 얹은 것이다. 위 두 공백 때문에 `Full`의 전체 모집단 cfg에서는 영구 부재 파일과 seal 없는 dev 변경 결과를 뺐다(§6).

## 4. 반응 시간 계약

### 4.1 지금 있는 계약

| # | 계약 | 좌표 |
|---|---|---|
| T1 | writer poll: iteration 작업을 마친 뒤 1초 기다린다. native 공급은 매 iteration 한 번 | `tui_o/writer/actor.rs:23,441-444,486-488`, `tui_o/writer/rotation/owe.rs:575-579` |
| T2 | source당 한 poll에 최대 1 MiB를 읽는다. 끝나지 않은 record는 16 MiB에서 멈춘다 | `tui_o/shadow/mod.rs:17`, `tui_o/shadow/capture.rs:16` |
| T3 | `RuntimeHintFence::try_acquire`는 기다리지 않는다. 동기 `acquire`는 MultiThread runtime과 runtime 밖에서는 기다리고 CurrentThread에서만 바쁘면 바로 오류다 | `tmux_common/runtime_hint_fence.rs:291-348`(분기 `:300-308`) |
| T4 | launch는 fence를 Herdr `herdr_launch.rs:455-577`, tmux `claude/tui_session_launch.rs:106` ~ `claude.rs:1835` 구간에서 쥔다 | 같은 좌표 |
| T5 | source authority는 세션별 프로세스 mutex이고 동기다 | `tmux_common.rs:120-144`, `discord/turn_finalizer/native_supply.rs:66-75` |
| T6 | 전역 boot restore 재시도 5초, 경고 60초 | `discord/runtime_bootstrap/native_input.rs:6,9` |
| T7 | live completion은 run 범위만 한 번 읽고 바로 cleanup 메시지를 보낸다 | `discord/turn_finalizer/native_completion.rs:105-127`, `discord/turn_finalizer/actor_state.rs:341-365` |
| T8 | `append_spool`은 publication lock 없이 쓰고 fsync한다. frame이 없으면 쓰지 않는다 | `tui_o/store/spool.rs:488-558`(`:500-503`) |
| T9 | turn finalizer reconcile tick 1초, 놓친 tick은 건너뜀 | `discord/turn_finalizer.rs:103,600-601` |

### 4.2 S3a가 더하는 것

추가 읽기량, fence 획득 대기, lock을 쥔 채 하는 일, 주기, gate가 열리기까지를 따로 적는다.

| # | 동작 | fence 획득 | 쥔 채 하는 일 | 주기 | gate 열림까지 |
|---|---|---|---|---|---|
| C2 | live completion | 없음 | 없음 | — | 지금과 같음. seal stamp가 그대로면 run 범위만 읽는다(T7) |
| C3 | boot·cross-dev completion | 없음 | 없음. blocking pool에서 `0..through` streaming hash 한 번 | 시도마다. 결과는 (source, through, hash)로 기억 | hash 시간 |
| C5 | 종료 기록 E1~E3·E5 | 이미 쥔 launch fence | stat 한 번, 16 KiB 이하 파일 교체·fsync 한 번. E1은 `read_end` 판독을 더하고 2초에서 끊는다(끊기면 기록하지 않음) | launch마다 | — |
| C6 | 종료 확인 E4 | `try_acquire`(대기 0, 바쁘면 이 시도 끝) | row 읽기, pane·process 판독, stat, 기록. 판독은 2초에서 끊는다. path가 stat되지 않으면 기록하지 않고 다음 시도 | 채널마다 시도 하나, 끝난 뒤 5초(T6 간격) | — |
| C10 | source scan | 없음 | 없음. blocking pool에서 `Projection::reopen`이 `0..captured_through`를 읽고 hash 확인, 이어서 `end_len`까지 읽음. 합계 `end_len` byte 이하 | 시도마다. (source, `captured_through`, hash, `end_len`)로 결과를 기억해 같은 입력은 다시 읽지 않음 | — |
| C7 | 종료 기록 → gate 열림 | — | — | — | barrier 건: 기록 뒤 다음 시도(최대 5초 + tick 1초) + CAS. 그 밖: 다음 시도의 C10 시간 + CAS. writer iteration에 기대지 않는다 |
| C11 | completion 진행 표시 | 없음 | actor 메시지 한 번(잡기)과 한 번(풀기) | completion마다 | — |

- tick은 E4·scan·정산 시도를 `spawn`하고 기다리지 않는다. T9의 1초 tick과 기존 reconcile 작업 시간은 늘지 않는다.
- writer poll(T1)과 그 작업은 바뀌지 않는다. design-4가 writer에 두려던 역사적 순회를 빼서 오히려 줄어든다.
- 기존 시험의 대기 창은 늘리지 않는다. 새 상수는 2초 판독 상한 하나이고, 재시도 간격은 T6을 쓴다.

## 5. 상태 소비 경로 전수표

행 1~45는 design-2, 행 46~58은 design-3의 표다. design-3에서 계약이 달라진 행은 ★로 표시했다. 행 59~68은 design-4, 행 69~75는 design-5에서 더한 것이다. 행 69~75는 정산 구동·재시도를 만들거나 소비하는 진입점을 S2에서 `git grep`(`refreshed(`, `settle_saved(`, `prepare_completion(`, `.retry()`, `source.validate(`, `reopen_native(`, `offer_native(`, `HaltedMissingSource`, `FinalizeMsg::NativeBoot`, `native_policy::reconcile(`)으로 다시 센 것이다. 계약 끝의 (S3a)는 그 단계에서 적용된다는 뜻이고, 표시 없는 행은 바뀌지 않는다.

| # | 소비 경로 | S2 file:line | 계약 |
|---|---|---|---|
| 1 | 같은 transcript Herdr Resume | `tui_prompt_dedupe/binding_events/herdr_resume.rs:7-15,39-45` | 새 nonce, 원본 ticket 불변 |
| 2 | 같은 session hook | `tui_prompt_dedupe/runtime_binding/claude_source.rs:520-549` | 현재 source 입력. 역사적 실행 귀속 아님 |
| 3 | 같은 UUID Stop·prompt 재시도 | `claude_tui/hook_server/observation_ingress.rs:196-215,263-285` | S2 오류 전파·재시도 보존 |
| 4 | P5 적용·checkpoint | `tui_o/writer/rotation.rs:422-456` | 적용 완료 checkpoint에 관측 결합 |
| 5 | 현재 stat·prefix reopen | `tui_o/writer/rotation/source_refresh.rs:30-54` | 현재 exact stat과 prefix를 함께 인증 |
| 6 | source rekey·dev 계보 | `tui_o/store/source_refresh.rs:168-215` | source별 계보만 사용 |
| 7 | native seed | `tui_o/writer/input_facts/native.rs:247-276`, `tui_o/writer/rotation/owe.rs:350-398` | ★ S3a: 종료 기록이 있는 boot ticket은 `end_len` cut 이하의 scoped 입력(§2.3). 실행별 원본 범위는 다음 라운드 |
| 8 | native origin 선택 | `tui_o/writer/rotation/owe.rs:425-457` (main 475~510행) | 현재 관측과 역사적 실행 분리 |
| 9 | registration source 일치 | `tui_o/writer/input_facts/native.rs:94-118`, `tui_input/human_frame/history/v2/native.rs:33-55` | provider·channel·session·path·inode 유지 |
| 10 | receipt 후보 분류 | `tui_o/writer/input_facts/native/receipt.rs:101-164` | 범위 제한을 유일성 판정 전에 적용 |
| 11 | 선두 receipt 차단 | `tui_o/writer/rotation/owe.rs:616-651` (main 738~777행) | 후보별 보류, 후속 진행 |
| 12 | source commit fence | `discord/turn_finalizer/native_supply.rs:66-75,109-159` | ★ 현재 source·계보·원본 범위 digest·원본 P5, 종료 정산은 종료 기록도 재확인 (S3a) |
| 13 | receipt 공급·CAS | `discord/turn_finalizer/native_supply.rs:226-283`, `tui_input/human_frame/history/v2/native_receipt.rs:15-87` | A+B+C와 exact ticket·receipt |
| 14 | 새 barrier | `tui_o/writer/rotation/owe.rs:653-686`, `discord/turn_finalizer/native_supply.rs:336-398`, `tui_input/human_frame/history/v2/native_barrier.rs:116-185` | 저장과 release 분리 |
| 15 | 저장 barrier 조회 | `tui_input/human_frame/history/v2/native_barrier.rs:18-53`, `tui_input/human_frame/history/v2/read_stamp.rs:174-196` | 구조 검증, candidate=false 유지 |
| 16 | barrier supersession | `tui_input/human_frame/history/v2/native_barrier.rs:73-105`, `tui_o/writer/input_facts/native/terminal.rs:24-52` | digest·interrupt·range 동일성 유지 |
| 17 | registration cache | `tui_input/human_frame/history/v2/read_stamp.rs:174-239` | 저장 검증 cache가 live permit을 만들지 않음 |
| 18 | live·boot 새 outcome | `tui_o/writer/rotation/owe.rs:695-726`, `discord/turn_finalizer/native_supply.rs:458-575,759-831` | A+B+C, 원본 identity 불변 |
| 19 | outcome Registry CAS | `tui_input/human_frame/history/v2/native.rs:587-653`, `discord/turn_finalizer/native_supply.rs:307-325` | ★ exact 결과 멱등과 새 승인 분리. `end_proof` outcome은 전용 CAS(§2.4) (S3a) |
| 20 | 전역 boot gate | `discord/runtime_bootstrap.rs:298-311`, `discord/runtime_bootstrap/native_input.rs:34-69,108-138` | 개별 관측 대기로 전역 boot를 막지 않음 |
| 21 | custody 보존·회수 | `discord/inflight/removal/native_input_custody.rs:86-216`, `tui_input/human_frame/history/v2/native.rs:200-275` | 저장 구조·manifest 검증 |
| 22 | boot capture·adoption | `discord/turn_finalizer/native_restore.rs:31-61,374-412`, `tui_input/human_frame/history/v2/native.rs:310-420,874-916` | 원본 key·episode·target·source 보존 |
| 23 | Register·channel gate | `discord/turn_finalizer/native_restore.rs:415-432,484-554`, `discord/turn_finalizer/native_prepare.rs:333-360` | ★ gate 정의는 그대로. 여는 출구에 종료 정산이 더해짐 (S3a) |
| 24 | 원본 ticket 재조회 | `discord/turn_finalizer/native_restore.rs:73-92`, `tui_input/human_frame/history/v2/native.rs:681-762` | ★ S3a: 구조 검증과 파일 판정을 나눈다. path가 stat되지 않으면 S2처럼 job을 만들지 않는다(§2.6) |
| 25 | 저장 outcome retry | `discord/turn_finalizer/native_supply.rs:190-215,307-325,648-684`, `discord/turn_finalizer/native_restore.rs:95-119` | ★ S3a: 저장 결과 보존. 구동은 finalizer(행 61), 실행 주체 하나(행 70) |
| 26 | 저장 proof 검증 | `discord/turn_finalizer/native_restore.rs:585-672` | ★ `end_proof` 분기 추가 (S3a) |
| 27 | 현재 idle 생산 | `tui_o/writer/rotation/owe.rs:728-749`, `discord/turn_finalizer/native_supply.rs:400-415` | 현재 실행에 귀속 |
| 28 | live retained Work 재구동 | `discord/turn_finalizer/native_supply.rs:416-442,285-287` | fresh idle마다 새 admission |
| 29 | boot abstain clear | `discord/turn_finalizer/native_supply.rs:444-453`, `discord/turn_finalizer/native_restore.rs:122-200` | ★ S3a: admission에 N1의 종료 기록을 더한다. 같은 dev의 N2 idle 정리는 남긴다. S3b 마지막 조각: N2 idle 정리를 닫는다(§3.4) |
| 30 | live abstain 부작용 | `discord/turn_finalizer/native_abstain.rs:11-38,54-188,194-254` | 첫 부작용 전 admission, 기존 actor·row CAS |
| 31 | boot completion | `discord/turn_finalizer/native_restore.rs:204-235,415-432` | ★ `Await`(stat 실패·busy·seal 없는 dev 변경)이면 Settle 금지, `Skipped`(inode 다름·길이 부족·seal 불일치)이면 진행 (S3a, §2.6) |
| 32 | live completion → drive | `discord/turn_finalizer/actor_state.rs:341-365,370-378`, `discord/turn_finalizer/native_completion.rs:85-102` | ★ `Await`이면 cleanup 메시지 금지, `Skipped`이면 진행 (S3a) |
| 33 | completion 본문 읽기 | `discord/turn_finalizer/native_completion.rs:23-82,105-150`, `discord/turn_finalizer/native_terminal.rs:20-34` | ★ seal hash와 path·inode로 인증. stat 성공 뒤 읽기 실패는 S2처럼 경고 뒤 진행 (S3a, §2.6) |
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
| 46 | 두 번째 전역 boot gate | `discord/inflight/removal/boot_reaper.rs:83-92,119` → `boot_custody.rs:435-443` → `tui_input/human_frame/history/v2/native.rs:229-275`(`:266` validate) → `:158-176` → `tui_input/human_frame/history/v2/native_barrier.rs:18-21,41-54` → `tui_input/human_frame/history/v2.rs:21-33`. 실패하면 `discord/runtime_bootstrap/native_input.rs:42-57`이 `Refused` | 행 20과 같은 구조 검증. 한쪽만 고치면 `ReaperExact` 변형처럼 부팅이 막힌다 (S3a, §2.8) |
| 47 | 제어 경로의 공유 검증자 | `tui_input/human_frame/history/v2/native_control.rs:85,206` (main 86·110·224행) | 검증자가 구조 검증이 돼도 제어 권한은 live actor(`discord/turn_finalizer/native_policy.rs:23-38`)와 row CAS에서 온다. 제어 비회귀 시험을 둔다 |
| 48 | receipt 경로의 공유 검증자 | `tui_input/human_frame/history/v2/native_receipt.rs:26,43` (main 15·33행) | 새 귀속이므로 B 검사를 그대로 요구한다. 구조 검증으로 낮추지 않는다 (S3a) |
| 49 | reply anchor membership (main) | main에만 있는 projection membership 모듈의 `Projection::memberships`(7~60행). S2에는 없다 | 현재 실행의 관측 소비자다. 전체 projection을 그대로 읽고 scoped 결과를 anchor에 쓰지 않는다 (S3a) |
| 50 | notice absorption (main) | main에만 있는 projection notice absorption 모듈의 `removal`·`Window::new`(14~28행, 45행~)와 그것을 부르는 `reduce`(main의 input_facts native 본체 545~562행). S2에는 없다 | notice 정산은 현재 projection. 역사적 ticket의 window는 scoped fact slice 안에서만 계산된다 (S3a) |
| 51 | raw EOF의 boot 분기 | `discord/turn_finalizer/native_terminal.rs:162-174`(`refreshed`), `:20-34`(`stamp`는 dev 재번호를 허용) | 현재 실행 분기는 그대로(B를 통과한 뒤 tail, N1이 현재일 때만 `:122-129`). S3a가 종료 기록에 묶인 분기를 더한다(행 64) |
| 52 | Herdr launch의 옛 실행 처리 | `herdr_launch.rs:455-460,492-509,548-550,560-577` | 종료 기록 E1. `Retired`면 `herdr_ended`를 다시 판독하고 참일 때만 쓴다(§2.2) (S3a) |
| 53 | tmux launch | `claude/tui_session_launch.rs:106-124,156-168`, `claude.rs:1828-1835`; legacy `claude.rs:2600-2608,2734-2743` | 종료 기록 E2·E3, 근거 `tmux_vacant`. marker 부재는 종료가 아님 (S3a) |
| 54 | 기존 종료 기록과 그 소비자 | `tui_o/store/clear_end.rs:24-40,99-126`, `tui_o/writer/rotation/clear_end.rs:24-80`, `discord/tui_direct_pending_start/turn_retirement/claude.rs:74-80,91-120`, `tui_o/store/rotation.rs:21-30,482-495` | 바꾸지 않는다. 새 기록은 별도 파일이고 hop·takeover 판정에 들어가지 않는다 |
| 55 | host 종료 판독 | `discord/turn_finalizer/native_terminal.rs:101-137`, `discord/herdr_terminate.rs:191-228`, `discord/commands/control/clear_teardown/clear_end_record.rs:133-138` | 종료 기록 E4. 근거별 판독(§2.2), host는 target 필드(행 68). boot debt 구동(행 61)이 부른다 (S3a) |
| 56 | 채널 gate 소비자 | `discord/turn_finalizer/native_prepare.rs:355-360`, `discord/router/message_handler/provider_dispatch/owner_input.rs:770-773`, `owner_input/native_input.rs:210-212` | 변경 없음. gate가 턴 시작을 막으므로 출구가 N2에 기대지 않는다(§2.1) |
| 57 | terminal 기록 매핑 | `discord/turn_finalizer/terminal_handler.rs:548-560`, `discord/receipt_settle.rs:185-208` | `TransferredUnverified` → `failed`. Discord 게시 없음 |
| 58 | native 공급 poll | `tui_o/writer/actor.rs:23,441-444`, `tui_o/writer/rotation/owe.rs:575-579`, `discord/turn_finalizer/native_supply.rs:648-685` | S3a: boot debt는 모두 행 61로 옮긴다. 종료 기록이 있는 ticket의 파일 읽기도 finalizer의 source scan이 한다(행 72). writer iteration에는 현재 실행 경로만 남는다 |
| 59 | source 계보의 생산 조건 | `tui_o/writer/rotation/source_refresh.rs:3-19,30-54`, `tui_o/store/source_refresh.rs:195-199`, `tui_o/shadow/capture.rs:226-240,263-275` | 계보는 `user_prompt_submit`·`stop` hook의 `SameSessionVerified` Bound에서만 생긴다. reopen은 계보를 만들지 않는다. S3a는 그대로 쓰고, S3b가 N2 없는 생산자 `EndRefresh`를 더한다(§3.1) |
| 60 | writer source 오류 → actor 종료 → host 재개 | `tui_o/writer/rotation/readers.rs:121-138`, `tui_o/writer/actor.rs:367-370,441-453,913-915`, `tui_o/writer/host.rs:701-712`, `tui_o/writer/host/missing.rs:67-68` | Changed·다른 inode면 writer가 멈추고 host는 missing만 재개한다. cursor 하나의 실패가 채널 전체를 멈추므로(행 71) boot debt와 역사적 scan을 writer 수명에 두지 않는다(§2.5) (S3a) |
| 61 | boot debt 구동 | `discord/turn_finalizer.rs:103,600-601,764-765`, `discord/turn_finalizer/reconcile.rs:231-240`, `discord/turn_finalizer/native_policy.rs:243-262`, `discord/turn_finalizer/actor_state.rs:130-135,290-298`, `discord/turn_finalizer/native_prepare.rs:346-353` | reconcile tick 옆에서 채널별 시도 하나를 spawn, 끝난 뒤 5초. `NativeBoot` 처리가 tick을 준비한다(§2.5) (S3a) |
| 62 | 설치 전에 이미 N2인 초기 상태 | `herdr_launch.rs:492-509,548-568`(row·marker가 N2로 바뀜), `discord/turn_finalizer/native_prepare.rs:85-95`(target 비교) | S3a는 종료 기록 생산자가 없고 지금 출구만 있다. S3b의 `superseded` 근거(§3.2). 모델 `Starts = superseded` |
| 63 | 같은 nonce의 source 전환 | `tui_prompt_dedupe/binding_events.rs:116-126,940-983`, `tui_prompt_dedupe/runtime_binding/claude_source.rs:534-588` | S3a 종료 기록의 `source`는 기록 시점의 최신 Bound이고 다른 source ticket에는 적용하지 않는다. S3b가 실행 증명과 source별 cut을 가른다(§3.3). 모델 `switched` |
| 64 | raw EOF의 종료 경계 | `tui_o/shadow/capture.rs:389-419`, `tui_o/writer/input_facts/native/terminal.rs:122-136,153-167`, `discord/turn_finalizer/native_terminal.rs:122-129,183-188` | 일반 record는 개행까지. 개행 없는 strict closer는 `through..end_len` tail로만 인증하고 결과 없는 정산의 증거로도 센다(§2.4) (S3a) |
| 65 | tmux → Herdr 첫 전환 | `claude/herdr_turn/handoff.rs:99-110,153-160`, `herdr_launch.rs:508-509` | `pinned_tmux::end` 성공 뒤 Herdr launch 전에 종료 기록 E5(`tmux_closed`). Herdr launch의 `Legacy` 분기는 E1을 타지 않는다 (S3a) |
| 66 | `Retired`의 증명 강도 | `herdr_launch/retire.rs:51-61,78-97`, `session_host/herdr_gate.rs:932-943`, `discord/herdr_terminate.rs:191-205`, `session_host/herdr_gate/root_replaced.rs` | `Retired` row만으로 종료를 쓰지 않는다. `herdr_ended`(pane 종료 + provider 부재)를 다시 판독한다(§2.2) (S3a) |
| 67 | 입력 출처별 하류 | `discord/turn_finalizer/native_completion.rs:267-283`, `discord/turn_finalizer/terminal_handler.rs:548-560`, `discord/receipt_settle.rs:185-208` | ordinary·follower·scheduled·work dispatch마다 terminal 기록과 completion 부작용을 따로 시험한다(§7.3) (S3a) |
| 68 | 실행 host 필드 | `discord/turn_finalizer/native_prepare.rs:29-31,85-95` | 종료 기록의 host와 E4의 판독 대상은 target의 `host`로 정한다. hosted row로 역추정하지 않는다 (S3a) |
| 69 | 저장 결과 completion 앞의 exact stat 검사 | `discord/turn_finalizer/native_restore.rs:73-92,95-102,418-430`, `discord/turn_finalizer/native_supply.rs:664-675`, `tui_input/human_frame/history/v2.rs:21-35` | S2는 metadata 실패·dev/inode 불일치·길이 부족이면 job을 만들지 않고 다시 본다. S3a는 stat 실패를 그대로 기다리고, 적극적인 변화 증거에만 `Skipped`(§2.6) (S3a) |
| 70 | writer쪽 저장 결과 정리 호출 | `discord/turn_finalizer/native_supply.rs:300-325,520-541` | 새 outcome 직후 `settle_saved`. finalizer 시도와 같은 ticket을 겹쳐 completion하지 않게 actor의 진행 표시를 잡는다(§2.6) (S3a) |
| 71 | writer 복구의 cursor 순회 | `tui_o/writer/rotation/readers.rs:75-95,118-138` | 과거 source의 NotFound는 넘어가지만 그 밖의 reopen 오류는 전체를 멈춘다. 이 실패가 다른 source의 정산을 막지 않는다(§2.5) (S3a) |
| 72 | source 하나의 독립 재투영 | `tui_o/writer/input_facts/native.rs:249-275`, `tui_o/writer/rotation/owe.rs:295-318` | `Projection::reopen`이 저장 prefix hash를 확인하며 fact를 만든다. finalizer의 source scan이 쓰고 `end_len`까지 이어 읽는다 (S3a) |
| 73 | raw witness의 frontier 비교 | `tui_o/writer/input_facts/native/terminal.rs:170-179` | 현재 관측 `through`와 비교하는 현재 경로는 그대로. 역사적 경로는 scan의 `end_len` 이하 frontier만 쓴다(§2.4) (S3a) |
| 74 | completion 부작용 | `discord/turn_finalizer/native_completion.rs:215-283` | transcript DB·selector·scheduled·work dispatch가 row CAS 전에 실행된다. 실행 주체 하나(행 70) (S3a) |
| 75 | writer poll의 native retry | `tui_o/writer/rotation/owe.rs:579`, `tui_o/writer/actor/native_input.rs:64`, `discord/turn_finalizer/native_supply.rs:648-685` | live input의 retry는 그대로 둔다. boot input의 debt는 행 61이 맡는다 (S3a) |


Cancel·Terminate·clear·purge·사용자 Stop·attach/paste(행 37~43)는 종료 기록을 읽지 않고 역사적 ticket의 대체 출구로 쓰지 않는다. 종료 기록의 소비자는 행 29와 §2.7·§3의 정산뿐이다.

### 5.1 모델 변수와 표의 대응

| 모델 변수 | 값 | 가져온 행 |
|---|---|---|
| `scen` | complete, closerLate, rawCloser, killed, barrierStored, outcomeSaved, outcomeLegacy | 7·10, 35, 64(개행 없는 closer), 14·15, 18·19·25 |
| `dev`, `file` | same·changed / ok·late(잠시 stat되지 않음)·gone(끝내 stat되지 않음)·changed(다른 inode·prefix). 제품은 late와 gone을 구별하지 못하므로 S3a 규칙은 둘을 같게 다룬다 | 5·6·9·33·43·45·69 |
| `approved` | 파일 승인(B) 유무. 같은 dev이면 참, dev가 바뀌면 계보가 있을 때 참 | 5·6·59 |
| `writer` | alive·halted. 그 ticket의 파일이 Changed이거나 다른 cursor의 reopen이 실패하면(`otherBad`) 멈춤 | 60·71 |
| `otherBad` | 같은 채널의 다른 source cursor가 reopen에 실패함 | 71 |
| `n1`, `n2`, `rowN` | alive·dead / absent·running·idle / n1·n2. `Starts = superseded`이면 N2가 이미 현재 | 1·2·27·36·52·53·62 |
| `switched` | 앞 source의 ticket | 63 |
| `endRec`, `cutKnown` | 종료 기록 유무, `end_len` 유무 | 52·53·55·65·66 |
| `fileCloser`, `witCloser`, `witFinal` | 파일의 closer, 범위 안 closer, 범위 확정 | 7·12·64 |
| `barrier`, `outcome`, `sealed`, `completion` | 저장 barrier, outcome, seal, completion 상태 | 14·15·16·18·19·25·31·32·33 |
| `cleared`, `clearedBy`, `rowOwner`, `succRemoved` | 정리 여부와 근거, 채널 행의 주인 | 23·29·30·34 |
| `boot` | ready·refused | 20·46 |
| `t2` | 다른 target의 ticket | 13·19 |

행 37~42(제어)는 살아 있는 actor가 있어야 하므로 boot input 모델에 넣지 않았다. 행 61의 구동은 정산 동작에 writer 조건을 붙이지 않는 것으로, 행 72의 source scan은 `EndScan` 동작으로 들어온다. 행 70·74의 completion 실행 주체는 모델에 없고 제품 RED가 맡는다(`OneOutcome`은 Registry 쓰기 횟수만 센다).

## 6. 상태 차트

모델과 실행 결과는 [모델 보고서](hook-transcript-custody.report.md)에 있다. 요약만 적는다.

- **모듈**: `HookTranscriptCustody.tla`, 설정 73개.
  - `Rules`: S2(현재 코드), D2·D3, `S3a4`(design-4의 S3a), `S3a`(design-5), Full(design-5 S3a + design-4 S3b).
  - `Others`: 다른 source cursor의 실패를 고른다.
- **design-5에서 더한 동작**:
  - `EndScan`: finalizer의 source scan.
  - `WriterHalt`: 다른 cursor의 실패로도 writer가 멈춘다.
  - `Terminal`: 파일 판정. S3a에서는 다른 inode·prefix만 확정이다.
- **design-5에서 더한 규칙**: `NoSilentLoss`. completion 건너뜀이나 closer 있는 턴의 결과 없는 정산은 파일이 Changed일 때만 허용한다. 제품이 late와 gone을 구별하지 못하므로 gone도 허용하지 않는다.
- **S3a 범위**(`HookTranscriptCustodyS3a.cfg`): 처음 상태 fresh, source 전환 없음, 승인 있음. 파일은 ok·late·changed, seal 없는 저장 결과는 뺐다.
  - 수정 전(`S3a4`)은 `NoSilentLoss`·`SkipOnlyWhenTerminal`이 깨진다.
  - 수정 후(`S3a`)는 S2의 기존 공백 `IdleDoesNotExpand`만 깨지고, liveness 5개는 모두 통과한다.
- **새 P1 두 경로**(같은 모집단을 세 규칙에 돌림):

| 경로 | S2 | S3a4 (수정 전) | S3a (수정 후) |
|---|---|---|---|
| Transient (파일이 잠시 stat되지 않음) | IdleDoesNotExpand, NoSilentLoss, SkipOnlyWhenTerminal · Boot, Channel, Eligible | IdleDoesNotExpand, **NoSilentLoss, SkipOnlyWhenTerminal** · - | IdleDoesNotExpand · - |
| OtherHalt (다른 source 실패로 writer 정지) | IdleDoesNotExpand · Boot, Channel, Eligible, SameDevKilled | IdleDoesNotExpand · **Channel, Eligible, SameDevKilled** | IdleDoesNotExpand · - |

- **S2와 같이 남긴 공백**: `GoneS3a`와 `LegacyDevS3a`는 `ChannelEventuallyReady`만 깨진다. 같은 모집단의 S2(`GoneS2`·`LegacyDevS2`)도 Channel이 깨지고, 여기에 `NoSilentLoss`가 더 깨진다.
- **변형**: `S3aEnoentTerminal`(stat되지 않는 path를 확정으로 봄)은 `NoSilentLoss`, `S3aScanInWriter`(scan을 writer에 둠)는 `ChannelEventuallyReady`를 깬다.
- **Full**(`Design`)은 영구 부재 파일과 seal 없는 dev 변경 결과를 뺀 모집단에서 불변식 14개와 liveness 5개를 모두 통과한다. design-4의 변형 23개는 그대로 각자 겨냥한 규칙에서 깨진다.

## 7. 구현 조각 (S3a)

- **추정 기준**: main `5f0f2803e8` 기준 운영 코드 순증가다. 시험·모델·문서는 뺀다.
- **상한**: 코드가 바뀐 prod 30파일, 순 코드 +1500, 정지선 +1200.
- **실측 비율**: 적용하지 못했다. S3 조각은 아직 하나도 구현·측정되지 않았다. 참고로 같은 이슈의 S2 실측은 `pr_cap_prod.py 1f61b76313 bd0c385be2`로 prod 30파일, 순 +769다. 그래서 아래 추정은 조각마다 상한까지 두 배 가까운 여유를 남기도록 잘랐다.

| 조각 | 내용 | prod 파일(새/기존) | design-4 추정 | design-5 추정 |
|---|---|---|---|---|
| S3a-1 | 종료 기록 저장소, E1·E2·E3·E5(근거별 증명), 공통 dev seam. 쓰기 실패면 launch 거절(dormant 아님) | 1 / 8 | 240 | 240 |
| S3a-2 | 파일 판정(stat 실패는 S2처럼 대기, 적극적인 변화 증거만 확정), ReadSeal, completion 반환값 gate, `refreshed` 분리, completion 진행 표시 | 2 / 6 | 230 | 260 |
| S3a-3 | 두 boot gate 구조 검증과 입력별 건너뜀, boot debt 구동(tick), E4, 종료 admission(저장 barrier), `end_proof` 기록과 검증, Changed 정산 | 4 / 9 | 600 | 590 |
| S3a-4 | finalizer source scan(`Projection::reopen` 재사용, `end_len`까지 이어 읽기), `end_len` cut의 scoped 분석, 역사적 raw EOF(자기 frontier), cut 있는 결과 없는 정산 | 4 / 5 | 490 | 540 |
| **합계** | 4조각 | **11 / 약 21** | **1,560** | **1,630** |

- design-4 대비 +70이다.
  - +40: completion 진행 표시(P2-2).
  - +50: source scan. writer쪽 후보 순회 150과 memo slot 30을 빼고 scan 200과 이어 읽기 30을 넣었다.
  - −10: Gone 분류 제거.
  - −10: raw EOF frontier 분리는 scan에 흡수.
- 가장 큰 조각은 S3a-3의 +590이다.

### 7.1 순서가 지키는 것

- S3a-1은 기록과 launch 거절만 한다. 종료 기록을 읽는 쪽은 S3a-3부터다.
- S3a-2는 S2의 stat 실패 대기를 그대로 둔다. 바꾸는 것은 S2가 영구히 기다리던 두 경우뿐이다. 하나는 seal이 맞는 dev 변경 결과를 읽는 것이고, 다른 하나는 다른 inode·짧아진 길이·seal 불일치를 건너뛰는 것이다.
- cut이 있는 결과 없는 정산은 역사적 raw EOF·source scan과 같은 조각(S3a-4)에 둔다. S3a-3의 정산은 파일을 읽지 않는 건(저장 barrier, 적극적인 Changed)뿐이다.
- N2 idle 정리는 닫지 않는다(다음 라운드).
- S2가 main에 착지한 뒤 조각을 시작한다.

### 7.2 파일 배치 (main 기준)

1,000줄이 넘는 파일에는 호출 교체만 둔다.

| giant 파일 (줄 수) | 두는 것 | 본체가 가는 곳 |
|---|---|---|
| `claude.rs` (4,108) | E3 호출 한 줄(`:2600-2608`) | `tui_o/store/execution_end.rs` |
| `claude/herdr_turn/handoff.rs` (1,133) | E5 호출 한 줄(`:99-110` 뒤) | `tui_o/store/execution_end.rs` |
| `discord/turn_finalizer.rs` (4,803) | `NativeBoot` 처리에 `cached_shared` 인자 전달 | `discord/turn_finalizer/native_prepare.rs` |
| `tui_o/writer/actor.rs` (1,041) | 바꾸지 않는다 | — |
| `tui_o/writer/input_facts/native.rs` (main 1,002) | `mod scoped;` 한 줄 | `tui_o/writer/input_facts/native/scoped.rs` |
| `tui_input/human_frame/history.rs` (main 1,182) | `file_id`(`:186-195`)가 `capture::file_identity`를 부르게 교체 | 없음 |

| 조각 | 새 파일 | 고치는 기존 파일 |
|---|---|---|
| S3a-1 | `tui_o/store/execution_end.rs` | `tui_o/store/mod.rs`, `herdr_launch.rs`, `herdr_launch/owner_port.rs`, `discord/herdr_owner_port.rs`, `claude/tui_session_launch.rs`, `claude.rs`, `claude/herdr_turn/handoff.rs`, `tui_input/human_frame/history.rs` |
| S3a-2 | `discord/turn_finalizer/native_read_seal.rs`, `discord/turn_finalizer/native_source_fate.rs` | `discord/turn_finalizer/native_completion.rs`, `discord/turn_finalizer/actor_state.rs`, `discord/turn_finalizer/native_restore.rs`, `discord/turn_finalizer/native_supply.rs`, `discord/turn_finalizer/native_prepare.rs`, `discord/turn_finalizer.rs` |
| S3a-3 | `discord/turn_finalizer/native_boot_debt.rs`, `discord/turn_finalizer/native_end_prover.rs`, `discord/turn_finalizer/native_admission.rs`, `tui_input/human_frame/history/v2/native_end.rs` | `discord/turn_finalizer.rs`, `discord/turn_finalizer/native_prepare.rs`, `discord/turn_finalizer/native_policy.rs`, `discord/turn_finalizer/native_supply.rs`, `discord/turn_finalizer/native_restore.rs`, `discord/turn_finalizer/native_abstain.rs`, `tui_input/human_frame/history/v2.rs`, `tui_input/human_frame/history/v2/native.rs`, `tui_input/human_frame/history/v2/native_barrier.rs` |
| S3a-4 | `discord/turn_finalizer/native_end_scan.rs`, `tui_o/writer/input_facts/native/scoped.rs`, `discord/turn_finalizer/native_end_release.rs`, `discord/turn_finalizer/native_end_raw.rs` | `tui_o/writer/input_facts/native.rs`, `tui_o/shadow/capture.rs`(`end_len` 상한 읽기), `discord/turn_finalizer/native_boot_debt.rs`, `discord/turn_finalizer/native_supply.rs`, `discord/turn_finalizer/native_terminal.rs` |

design-4가 고치려던 `tui_o/writer/rotation/owe.rs`·`tui_o/writer/rotation/owe/observe_memo.rs`는 S3a에서 바꾸지 않는다.

### 7.3 조각별 제품 RED

- 모든 fixture는 공통 dev seam으로 dev를 바꾼다.
- 각 fixture는 먼저 writer와 native history가 둘 다 새 dev를 본다는 것을 양으로 단언한다.
- helper 직접 호출의 GREEN으로 실제 진입점 시험을 대신하지 않는다.
- 비회귀 fixture는 같은 입력을 S2 동작 기준으로도 단언한다.

| 조각 | 주입 | 단언 |
|---|---|---|
| S3a-1 | Herdr `Retired` N1 뒤 N2(provider 부재 판독 참/거짓), tmux vacant N1 뒤 N2, tmux→Herdr 첫 전환, 쓰기 실패 | 참이면 후속 생성 전에 기록이 디스크에 있음, 거짓이면 기록 0·launch 진행. 쓰기 실패면 launch 거절. marker만 없으면 기록 0 |
| S3a-2 | **저장 결과 + 같은 파일을 잠시 숨겼다가 같은 inode·내용으로 되돌림. 부모 디렉터리가 남은 ENOENT와 부모까지 없어진 ENOENT 각각** | 숨긴 동안 completion·cleanup 0, retry job 0. 되돌린 뒤 completion 정확히 1회와 cleanup 정확히 1회 |
| S3a-2 | 저장 결과 + 권한 오류·EIO 주입(stat 단계) | completion·cleanup 0. 오류가 풀리면 각 1회 |
| S3a-2 | 저장 결과 + stat은 되는데 읽기가 실패(권한 000) | S2와 같이 경고 뒤 cleanup. 영구 보류 0 |
| S3a-2 | 저장 결과 + 다른 inode로 교체, 길이가 `through`보다 짧게 잘림, 같은 길이로 내용 변경(seal 불일치) | completion 기록 없이 cleanup, gate 열림 |
| S3a-2 | ReadSeal 저장 → dev 변경 → 실제 retry → `refreshed` → completion | 인증 전 부작용 0, 인증 뒤 정확한 본문 1회. seal 없는 결과는 dev 변경에서 S2처럼 대기 |
| S3a-2 | writer의 새 outcome 직후 정리와 finalizer 시도가 같은 ticket에 겹침 | transcript DB·selector·scheduled·work dispatch 부작용 각 1회 |
| S3a-3 | 저장 barrier → dev 변경 reboot, 파일 부재 | 두 boot gate 모두 진행 |
| S3a-3 | **같은 dev + 저장 barrier, N1 종료, N2가 뜨고 idle** | gate 열림, `Empty` 1회 |
| S3a-3 | **Changed source로 writer 시작 → 첫 E4 fence busy → writer halt → fence 해제 → 종료 판독 성공** | 원본 ticket 1회 정산, gate 열림. 턴이 없는 채널에서도 tick이 돎 |
| S3a-3 | outcome 없는 ticket, 파일이 잠시 stat되지 않음 | 결과 없는 정산 0. 돌아온 뒤 closer가 있으면 `Executed` 1회 |
| S3a-4 | **source A와 B의 cursor 보존 → A reopen 실패 → 채널 writer 종료 → B 파일은 정상이고 N1 closer 있음 → B 종료 증명 성공** | B의 N1 `Executed` 정확히 1회, gate 열림, A cursor·spool 불변 |
| S3a-4 | **개행 없는 온전한 N1 closer → N2 먼저 launch → N2가 suffix를 써서 현재 capture frontier가 `end_len`을 지남** | N1 `Executed` 1회, `TransferredUnverified` 0, N2 행 불변. 역사적 경로가 현재 frontier를 쓰지 않음 |
| S3a-4 | 같은 dev(또는 계보 있음), N1 턴 중간 종료, N2 Resume·입력·idle | `TransferredUnverified` 또는 `Empty` 정확히 1회, N2 행 불변, gate 열림 |
| S3a-4 | N1 closer가 마지막 capture 뒤, N2가 먼저 뜸 | `Executed` 1회, 결과 없는 정산 0 |
| S3a-4 | 입력 출처별: ordinary·follower·scheduled·work dispatch 결과 없는 정산 | terminal `failed` 1회, 각 하류 변화가 지금의 `TransferredUnverified`와 같음, Discord 게시 0 |
| S3a-4 | 종료 기록의 nonce·host만 다른 ticket, 종료 기록 없는 ticket | 정산 0 |

### 7.4 변형으로 확인할 guard

| 끄는 검사 | RED여야 하는 단언 |
|---|---|
| 종료 기록의 target·host 비교 | nonce·host만 다른 ticket 정산 0 |
| 근거별 판독(`Retired`·marker 부재로 대체) | provider가 살아 있는 N1의 종료 기록 0 |
| stat 실패의 대기(ENOENT를 확정으로) | 숨겼다 되돌린 파일의 completion 1회 |
| source scan의 writer 분리 | A reopen 실패 fixture의 B `Executed` |
| completion 진행 표시 | 겹친 정리의 부작용 각 1회 |
| 역사적 raw EOF의 자기 frontier | frontier가 앞선 fixture의 `Executed` |
| `end_len` 상한 | N2 suffix 귀속 0 |
| completion 반환값 gate | `Await`에서 원본 행 보존 |
| 원본 row CAS | N2 행 보존 |
| 두 번째 boot gate의 구조 검증 | reaper 경로의 boot 진행 |

## 8. 남은 위험

- **Herdr endpoint가 돌아오지 않는 경우.** Herdr N1의 종료는 pane·process 판독으로만 증명한다. Herdr 서버가 끝내 뜨지 않으면 종료 기록이 생기지 않고 그 채널 gate는 닫힌 채다. 모델은 host 판독이 결국 답한다고 가정했다.
- **끝내 stat되지 않는 transcript.** S3a는 S2와 같이 기다린다. 원본이 영구히 사라졌다는 증거가 없으므로 정산하지 않는다. 해당 채널은 지금처럼 닫힌 채 남는다(다음 라운드).
- **seal 없는 저장 결과의 dev 변경.** S2와 같이 기다린다(다음 라운드).
- **stat 성공 뒤의 읽기 실패.** S2와 같이 경고 뒤 진행하므로, stat과 읽기 사이에 파일이 사라지는 경합에서는 지금처럼 completion을 잃을 수 있다. S2의 기존 동작이고 S3a가 넓히지 않는다.
- **E1에서 쓰지 않는 기록.** `Retired`이지만 provider 부재가 2초 안에 확인되지 않으면 기록하지 않는다. 그 ticket은 지금의 출구만 가진다.
- **`end_len`을 재지 못한 종료 기록.** 파일이 그 순간 없거나 inode가 다르면 `end_len`이 빈다. S3a는 그 ticket을 cut으로 정산하지 않는다(적극적인 Changed면 결과 없이 정산).
- **source scan의 읽기량.** scan은 `0..captured_through`를 다시 읽어 hash를 확인한다. 같은 입력의 결과는 기억하지만, 미정산 ticket이 많은 큰 transcript에서는 시도마다 그 크기의 읽기가 한 번 생긴다. 상한은 `end_len` byte다.
- **종료 기록 16건 상한.** 한 채널에서 미정산 ticket이 남은 채 실행이 16번 더 끝나면 가장 오래된 기록이 밀려난다. 모델에는 없다.
- **모델의 추상화.** 모델에 없는 것은 §7.3의 제품 RED가 맡는다.
  - hash·byte 변조, FD/path 경합, Registry fingerprint, launch 경합의 실제 순서.
  - 식별자별 격리. `ExactTarget`은 다른 target ticket 하나만 본다.
  - completion 부작용의 중복. `OneOutcome`은 outcome 쓰기 횟수만 본다.
  - stat 성공 뒤의 읽기 실패.
- **실행 재현 없음.** 반례는 고정 SHA 소스를 읽어 확인했다. 빌드가 금지된 설계 레그라 실행으로 재현하지 않았다.

## 9. P2 처리

| 리뷰 P2 | 처리 |
|---|---|
| P2-1 `superseded`의 증명 강도 | 후속(다음 라운드). S3b 계약이라 S3a에 영향 없음 |
| P2-2 completion 부작용의 중복 방지 | **반영**. S3a가 finalizer 구동을 새로 넣으므로 S3a 계약이다. actor의 진행 표시로 실행 주체를 하나로 한다(§2.6, 행 70·74, C11) |
| P2-3 모델과 제품 RED의 검증 범위 | **일부 반영**. 역사적 raw EOF가 현재 frontier를 빌리지 않는 계약과 시험(§2.4, 행 73, §7.3). 규칙별 검사 범위는 보고서 표에 적었다. 그 밖(실제 key·owner·root 조합, gate 뒤 대기열 wake)은 후속 |
| P2-4 rollback 운영 계약 | 후속. S3a-3부터 `end_proof` outcome이 저장되므로, 그 뒤 옛 바이너리로 되돌리면 미정리 boot record에서 부팅 검증이 실패할 수 있다. rollback 경계는 S3a-3 착지 전이다. 운영 절차 문서는 S3a-3 PR에서 정한다 |
