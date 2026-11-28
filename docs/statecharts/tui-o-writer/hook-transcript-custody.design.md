# 재시작 뒤 원본 keyed ticket 정산 — design-6 (#8448 s3, S3a)

<!-- doc-path-refs: proposed src/services/tui_o/store/execution_end.rs src/services/session_host/execution_end.rs src/services/tui_input/human_frame/history/v2/native_end.rs src/services/tui_input/human_frame/history/v2/native_latch.rs src/services/tui_input/human_frame/history/v2/lineage.rs src/services/discord/turn_finalizer/native_source_fate.rs src/services/discord/turn_finalizer/native_read_seal.rs src/services/discord/turn_finalizer/native_admission.rs src/services/discord/turn_finalizer/native_end_prover.rs src/services/discord/turn_finalizer/native_boot_debt.rs src/services/discord/turn_finalizer/native_end_release.rs src/services/discord/turn_finalizer/native_end_raw.rs src/services/discord/turn_finalizer/native_end_scan.rs src/services/tui_o/writer/input_facts/native/scoped.rs -->

재부팅으로 장치 번호(dev)가 바뀐 뒤 새 실행(N2)이 같은 transcript를 이을 때, 옛 실행(N1)의 keyed ticket을 어떻게 정산하는지에 대한 설계다. 범위는 **S3a만**이다. S3b는 S3a 착지 뒤 다음 라운드다(§3).

design-6은 design-5 리뷰 두 개를 닫는다.

- WebGPT 리뷰: P0 0, P1 3, P2 3.
- 외부 교차 리뷰: 순증 P1 3.

뿌리가 같은 항목은 한 줄로 묶었다(§1.3). P2 가운데 S3a 계약을 바꾸는 두 건(P2-1 재투영, P2-2 순차 재실행)은 외부 P1-NEW-3과 뿌리가 같아 이번에 반영했다. P2-3은 §9의 후속이다.

## 0. 기준

| 구분 | SHA |
|---|---|
| S2 (설계 기준 소스) | `bd0c385be24ca25a10725127dcd77eba1274d4fa` |
| main (merge한 기준, 조각 재추정 기준) | `826a01da0ea5f8b5ad27ae78f450bef3aa5c38a0` |

별도 표시가 없는 좌표는 S2 기준이고, 경로는 `src/services/` 아래를 줄여 적었다. 이번에 확인한 범위에서는 S2와 main의 줄이 같다. 다른 경우는 "main N행"처럼 적었다. 새로 만들 파일은 이 문서에서 제안하는 것이고 아직 없다.

이 문서는 설계만 담는다. 운영 코드는 바꾸지 않았다. 이번 레그에서는 두 가지를 실제로 빌드했다. 하나는 main merge 해소의 컴파일 확인이고, 다른 하나는 외부 P1-NEW-1의 실측이다(§1.4).

## 1. 단계 분리

### 1.1 그대로 두는 계약

design-2~5에서 리뷰가 받아들인 계약은 바꾸지 않는다.

- 원본 ticket·namespace·key·channel·episode·target은 바꾸지 않는다.
- 전역 부팅은 저장 custody의 구조 검증만으로 진행한다. 구조 검증은 현재 파일 관측 권한을 주지 않는다.
- 새 receipt·barrier·outcome에는 A(원본 실행 권한), B(현재 파일 관측 권한), C(원본 증거 범위)가 모두 있어야 한다.
- 새 Executed outcome에는 같은 CAS로 ReadSeal을 저장한다. completion 본문은 인증된 뒤에만 읽고, `record`/`record_live`의 반환값이 cleanup을 막는다.
- 원본 행 제거는 기존 exact row CAS로 한다. N2의 행은 건드리지 않는다.
- 결과 없는 정산은 Discord에 아무것도 보내지 않는다. 새 운영자 명령이나 수동 스위치는 없다.
- stat 실패(errno 무관)는 S2처럼 기다린다. 결과 없는 정산과 completion 건너뜀에는 적극적인 증거가 있어야 한다: 다른 inode, 짧아진 길이, seal hash 불일치(design-5 §2.6).

### 1.2 S3a 단독 비회귀

S3a만 main에 있을 때 각 경로가 S2보다 나빠지지 않는다는 근거다. 같은 모집단을 S2, design-5 S3a(모델 규칙 `S3a5`), design-6 S3a(`S3a`)에 각각 돌린 cfg를 둔다(§6). 굵은 행이 이번 리뷰에서 새로 들어온 경로다.

| 경로 | S2 | design-5 S3a | design-6 S3a | cfg |
|---|---|---|---|---|
| **boot restore의 저장 결과, transcript가 stat되지 않음 (P1-1)** | stat 없이 읽고, 읽기가 실패하면 경고 뒤 cleanup | **새 Await gate로 영구 보류 가능** | S2와 같음. seal이 있고 파일이 읽히면 hash를 먼저 확인한다 | `RestoreS2`·`RestoreS3a5`·`RestoreS3a` |
| **live completion, transcript가 stat되지 않음 (P1-1)** | run 범위를 읽고, 실패하면 경고 뒤 진행 | 같은 gate 적용 대상으로 적힘 | S2와 같음(gate 없음) | 모델 밖(진입점 표 §2.6, 제품 RED) |
| **제어 결과(control_proof) 저장 outcome의 restore (P1-1)** | 본문을 읽지 않고 control proof로 검증 | 파일 gate와 구별 없음 | 파일 gate를 붙이지 않음 | 모델 밖(제품 RED) |
| 저장 결과 retry job, transcript가 잠시 stat되지 않음 | exact stat 검사가 job을 막고 다음 retry에서 다시 봄 | 같음 | 같음. 파일이 돌아오면 completion 1회 | `TransientS2`·`TransientS3a` |
| **membership receipt 없이 closer만 읽힘 (P1-2)** | writer가 receipt를 먼저 durable하게 남긴 뒤 outcome | scan이 receipt를 만들지 않아 outcome CAS가 거절되고 대기 | scan이 receipt를 durable하게 만든 뒤 outcome | `ReceiptS2`·`ReceiptS3a5`·`ReceiptS3a` |
| **설치 전 이미 N2, 같은 dev, N1 newline 결과 (P1-3)** | writer가 N1 원본 Bound로 공급 | **writer를 현재 실행으로 좁혀 출구 소실** | S2 출구 유지. 종료 기록·cut·B가 모두 있는 ticket만 finalizer로 인계 | `HistoricS2`·`HistoricS3a5`·`HistoricS3a` |
| **재부팅 뒤 tmux N1의 종료 판독 (P1-NEW-2)** | 종료 기록 없음 | live capture가 필요해 판독 불가 | spawn 기록 + boot time으로 `Ended` | `TmuxS2`·`TmuxS3a5`·`TmuxS3a`, 변형 `S3aTmuxLiveOnly` |
| **dead pane 재사용 launch (P1-NEW-2)** | 기록 없이 respawn | pane이 Present라 기록 없이 덮어씀 | respawn 전에 `pane_dead`와 spawn 신원으로 `Ended` 기록 | 같음 |
| **tmux 종료를 끝내 판독할 수 없음** | S2 출구만 | 같음 | 같음(`Unknown`이면 기록하지 않음) | `TmuxUnknownS2`·`TmuxUnknownS3a` |
| **Settle 재진입: row CAS 지연 뒤 다시 Settle (P1-NEW-3b)** | completion 부작용을 다시 실행 | 진행 표시가 풀린 뒤 다시 실행 | durable 단방향 latch로 1회 | `ReentryS2`·`ReentryS3a5`·`ReentryS3a`, 변형 `S3aNoLatch` |
| **N1 무개행 closer 뒤 N2가 이어 씀, cursor가 `end_len`을 지남 (P1-NEW-3a)** | 대기 | `reopen(…, L2)`이 결합 줄에서 실패하거나 closer를 잃음 | hash 인증은 저장 prefix까지, 해독은 `end_len`에서 멈춤 | 모델 밖(제품 RED, §7.3) |
| 같은 dev, 저장 barrier, N2 idle | N2 idle로 `Empty` | 같음 | 같음. 종료 기록이 있으면 더 일찍 `Empty` | `BarrierS2`·`BarrierS3a` |
| 다른 source cursor 실패로 writer 정지, 이 ticket 파일은 정상 | 영구 대기 | finalizer scan | 같음 + receipt 생산 | `OtherHaltS2`·`OtherHaltS3a` |
| transcript가 끝내 stat되지 않음 (retry 진입) | 영구 대기 | 같음 | 같음(§3) | `GoneS2`·`GoneS3a` |
| seal 없는 저장 결과, dev 변경 (retry 진입) | 영구 대기 | 같음 | 같음(§3). boot restore 진입이면 S2처럼 stat 없이 읽음 | `LegacyDevS2`·`LegacyDevS3a` |
| 같은 N1의 앞 source ticket | 지금의 출구 | 같음 | 같음 | `SwitchedS2`·`SwitchedS3a` |
| stat은 되는데 completion 읽기가 실패 | 경고 뒤 cleanup | 같음 | 같음 | 모델 밖(제품 RED) |
| 종료 기록 쓰기 실패 | 없음 | launch 거절 | 같음. spawn 기록 쓰기 실패는 거절하지 않고 그 실행만 `Unknown`이 된다 | 모델 밖(제품 RED) |

boot restore 진입의 stat 없는 읽기와 경고 뒤 cleanup은 S2의 동작이다. S3a는 이것을 고치지 않고 그대로 둔다. 그래서 모델의 `NoSilentLoss`·`SkipOnlyWhenTerminal`은 restore 진입의 건너뜀을 S2 동등으로 허용한다(§6). `IdleDoesNotExpand`는 S2처럼 깨진다. S2의 N2 idle 정리를 남겼기 때문이다(`IdleNotExpandedCrossDev`는 통과).

### 1.3 P1 해소 표

WebGPT 리뷰 P1 셋과 외부 교차 리뷰 P1 셋이다. 각 항목은 S2(`bd0c385be2`)와 지금 HEAD(main merge 뒤)의 코드로 확인했다.

| # | 리뷰 항목 (묶은 것) | 판정 | 코드 확인 (S2 = HEAD 좌표) | design-6 처리 | 모델 |
|---|---|---|---|---|---|
| 1 | WebGPT P1-1: 진입점마다 전제가 다른데 design-5가 Await gate를 일괄로 붙임 | **수용·해소** | settle_saved는 `refreshed()`가 exact `Source::validate`를 부름(`discord/turn_finalizer/native_restore.rs:73-102`). retry job도 exact(`discord/turn_finalizer/native_supply.rs:664-675`). boot restore의 `prepare_completion`(`discord/turn_finalizer/native_restore.rs:418-430`)은 `refreshed` 없이 들어가고, 앞단 검사는 barrier가 있을 때만 stat함(`tui_input/human_frame/history/v2/native.rs:156-177`). live completion(`discord/turn_finalizer/terminal_handler.rs:695-747`, `discord/turn_finalizer/actor_state.rs:341-365`)은 stat 없이 읽고 실패하면 경고 뒤 진행(`discord/turn_finalizer/native_completion.rs:74-81`). 제어 결과는 `control_proof`로 따로 검증(`discord/turn_finalizer/native_restore.rs:592-601`) | 진입점별로 S2 전제를 유지한다. gate는 부작용이 실제로 필요한 분기에만 붙인다(§2.6 표) | `RestoreS3a5` RED → `RestoreS3a` GREEN, 변형 `S3aRestoreAwait` RED |
| 2 | WebGPT P1-2: 독립 scan이 durable membership receipt를 만들지 않음 | **수용·해소** | Executed와 raw EOF 기록은 durable receipt를 요구(`tui_input/human_frame/history/v2/native.rs:618-632`). writer는 receipt를 먼저 남기고 다시 snapshot(`tui_o/writer/rotation/owe.rs:616-651`, `discord/turn_finalizer/native_supply.rs:226-283`) | closer 판독과 귀속 권한 확정을 나눈다. scan은 receipt 후보 → durable receipt → fingerprint 재확인 → scoped reduce → outcome CAS 순서로 한다(§2.5) | `ReceiptS3a5` RED → `ReceiptS3a` GREEN, 변형 `S3aScanNoReceipt` RED |
| 3 | WebGPT P1-3: writer를 현재 실행으로 좁히면 S2 역사적 출구가 사라짐 | **수용·해소** | writer는 ticket의 원본 Bound로 공급(`tui_o/writer/rotation/owe.rs:425-457,695-726`, `discord/turn_finalizer/native_supply.rs:109-159,520-568`) | writer의 S2 경로는 그대로 둔다. 종료 기록·cut·B가 모두 있는 ticket만 finalizer로 넘긴다. 이 인계는 scan과 같은 조각(S3a-4)에 둔다(§2.5) | `HistoricS3a5` RED → `HistoricS3a` GREEN |
| 4 | 외부 P1-NEW-1: history.rs의 dev seam만 먼저 착지하면 회귀하고, exact validator가 계보 B를 거절함 | **수용·해소 (실측 확인)** | `Source::validate`는 `file_id(&meta) != self.file`이면 거절(`tui_input/human_frame/history/v2.rs:21-35`). exact 호출: `tui_input/human_frame/history/v2/native_receipt.rs:26`, `discord/turn_finalizer/native_supply.rs:786`(main 821행), `tui_input/human_frame/history/v2/native_barrier.rs:20,121`, `discord/turn_finalizer/native_restore.rs:87`, `tui_input/human_frame/history/v2.rs:382,435,715,951`. seam만 넣은 실측에서 `discord/turn_finalizer/native_renumber_tests.rs:106`의 시험이 깨짐(§1.4) | 계보 허용 validator(§2.11)를 seam과 같은 조각에 둔다. seam은 단독으로 착지하지 않는다(§7) | 모델 밖(실측 RED) |
| 5 | 외부 P1-NEW-2: `tmux_vacant`는 재시작 뒤나 dead pane respawn 때 판독할 수 없음 | **수용·해소** | `vacant`는 실시간 capture를 요구(`discord/commands/control/clear_teardown/clear_end_record.rs:73-101,133-138`). dead pane은 Present이고 그대로 respawn(main `claude.rs` 1788-1799행 → `claude/tui_session_launch.rs:156-168`, `platform/tmux.rs:203`) | tmux 전용 판독을 `InteractiveSessionHost`의 새 메서드 뒤로 옮기고, launch 때 spawn 신원을 기록한다(§2.10) | `TmuxS3a` GREEN, 변형 `S3aTmuxLiveOnly` RED |
| 6 | 외부 P1-NEW-3(a) + WebGPT P2-1: `end_len`보다 긴 저장 cursor에서 `reopen`을 그대로 쓰면 closer를 잃음. cursor 부재·키 이동 | **수용·해소** | `Projection::reopen`은 지정한 `through`까지 해독(`tui_o/writer/input_facts/native.rs:249-277`, 사용 `:388`). refresh가 cursor 키를 옮김(`tui_o/store/source_refresh.rs:144,210`) | hash 인증 범위와 해독 범위를 나눈다. cursor는 refresh 계보로 찾는다(§2.12) | 모델 밖(제품 RED) |
| 7 | 외부 P1-NEW-3(b) + WebGPT P2-2: `BootInputs::restore`의 completion이 진행 표시 밖이고 Settle 재진입이 부작용을 다시 실행 | **수용·해소** | `prepare_completion`(`discord/turn_finalizer/native_restore.rs:425`)이 Register 직후 돈다. Settle은 `:317-327`(terminal 기록 없음)과 `:338`(row CAS 지연)에서 정리 없이 돌아온다. 부작용은 `discord/turn_finalizer/native_completion.rs:215-283`(work dispatch 완료는 `discord/turn_bridge/completion_guard.rs:552`) | 모든 진입점이 보는 durable 단방향 completion latch(§2.13) | `ReentryS2`·`ReentryS3a5` RED → `ReentryS3a` GREEN, 변형 `S3aNoLatch` RED |

P1-NEW-3(b)의 재실행은 S2에도 있다(`ReentryS2` RED). S3a가 finalizer 구동을 더하면 재진입 횟수가 늘어나므로 S3a 계약으로 닫는다.

### 1.4 P1-NEW-1 실측

레인 폴더 안 임시 worktree에서 실행했고, 끝난 뒤 지웠다. cargo-shim과 throwaway PG를 썼다. 필터는 `hook_refresh_`이고, 7개가 선택된다.

| 상태 | 통과 | 실패 |
|---|---|---|
| 기준(HEAD + merge 해소) | 7 | 0 |
| 기준 + S3a-1 seam만(`history.rs`의 `file_id`가 `capture::file_identity`를 부름) | 6 | 1: `native_input_tests::renumber::hook_refresh_settles_the_original_keyed_ticket_after_boot_pg`(`discord/turn_finalizer/native_renumber_tests.rs:163`, `left: None, right: Some(Executed)`) |

`native_renumber_tests.rs:35`의 시험(`hook_refresh_applies_a_renumber_and_settles_the_native_turn_pg`)은 두 상태 모두 통과했다. 이 시험은 `resume_at_dev`로 writer 쪽 저장 dev만 바꾸고 `renumber`를 쓰지 않는다. 그래서 리뷰의 ":35·106 둘 다 깨짐"은 :106만 맞다.

실패 원인은 이렇다. :106은 `renumber::shift`로 stat dev를 바꾼다. seam이 들어오면 native history도 바뀐 dev를 보고, exact `Source::validate`가 원본 ticket의 file을 거절한다. 운영에서는 `renumber`가 꺼져 있으므로(`tui_o/shadow/capture.rs:19-25`) seam 자체는 동작을 바꾸지 않는다. 다만 실제 재부팅에서 exact validator가 계보 B를 거절한다는 사실을 시험이 드러낸 것이므로, validator와 seam을 같은 조각에 둔다.

seam 상태 실행에서는 `reclaim::hook_refresh_recovers_one_cursor_and_reposts_nothing_after_interrupt`도 실패했다. 이것은 seam과 무관한 merge 문제였다. main의 crate 분리 뒤 S2의 fault 주입 지점이 `#[cfg(test)]`라서 main crate 시험에서 꺼진 것이다(`tui_o/store/source_refresh.rs` main 106·296행). merge 해소에서 `any(test, feature = "test-support")`로 고친 뒤 기준 상태는 7/7이 됐다.

## 2. S3a 계약

### 2.1 출구가 N2에 기대면 안 되는 이유

채널 boot gate는 그 채널의 boot input이 모두 settled일 때만 열린다(`discord/turn_finalizer/native_prepare.rs:355-360`). gate가 닫혀 있으면 새 턴 시작은 `Wait::Event`로 기다린다(`discord/router/message_handler/provider_dispatch/owner_input.rs:770-773`). N2는 사람이 pane에 직접 입력하거나 host 복구가 띄울 때만 생긴다. 상태 차트는 N2의 launch·입력과 N1의 죽음에 공정성을 주지 않는다.

### 2.2 종료 기록

N1 원본 target(`owner`, `execution_nonce`)이 끝났다는 사실을 채널별 durable 기록으로 남긴다.

**저장소.** 새 child `tui_o/store/execution_end.rs`에 둔다. 형식과 쓰기 규약은 `tui_o/store/clear_end.rs:24-40,99-126`과 같다(채널별 JSON 한 파일, `durable::replace`, version watch). 항목은 두 종류다.

| 항목 | 필드 | 언제 쓰나 |
|---|---|---|
| `spawn` (design-6) | `owner`, `execution_nonce`, `host`, tmux면 `server_pid`·`session`·`pane_id`·`pane_pid`·`pane_pid_start`·`pgid`·`boot` | tmux launch 직후 pane pid를 안 뒤(§2.10). Herdr는 쓰지 않는다 |
| `end` | `channel`, `provider`, `owner`, `execution_nonce`, `host`, `source`, `binding_seq`, `end_len`, `basis`, `ended_at` | 아래 진입점 |

상한은 채널당 `end` 16건과 `spawn` 16건, 합계 32 KiB다. `end`가 쓰이면 같은 nonce의 `spawn`은 지운다.

**host 구분.** 실행이 Herdr인지 tmux인지는 ticket target의 `host`로 정한다(`discord/turn_finalizer/native_prepare.rs:29-31,85-95`). tmux target은 `claude_tui/owner_input.rs` main 452행과 `codex/tui_session_launch/first_input.rs` main 167행이 만든다. `host = "process"`인 keyed target을 만드는 곳은 없다.

**근거.** 판독은 모두 `InteractiveSessionHost::execution_end`(§2.10)를 거친다. `Retired` row 하나나 marker 부재만으로는 종료를 쓰지 않는다.

| `basis` | host | 조건 |
|---|---|---|
| `herdr_ended` | Herdr | pane 종료 판독 + `provider_absent()`가 참(`discord/herdr_terminate.rs:191-205`와 같은 판독) |
| `tmux_rebooted` (design-6) | tmux | spawn 기록의 `boot`가 지금 host boot time과 다름 |
| `tmux_pane_dead` (design-6) | tmux | 같은 boot에서 기록된 pane의 `pane_dead = 1`, 그 pane의 `pane_pid` 신원이 기록과 같음, `pgid` 종료 |
| `tmux_vacant` | tmux | 같은 boot에서 세션 없음, spawn 기록의 pid 신원 종료, `pgid` 종료. 실시간 capture 대신 spawn 기록을 쓴다 |
| `tmux_closed` | tmux | handoff의 `session_handoff::pinned_tmux::end`가 `Ok`(`claude/herdr_turn/handoff.rs:99-110`) |

**쓰는 진입점.**

| # | 진입점 | 언제 | 판독 |
|---|---|---|---|
| E1 | Herdr launch `herdr_launch.rs:492-498` | fence 획득(`:455-459`)과 row 읽기(`:460`) 뒤, 후속 Pending이 row를 덮기(`:548-550`) 전 | row가 `Retired`일 때 `execution_end`. `Ended`가 아니면 기록하지 않고 launch는 진행 |
| E2 | tmux launch `claude/tui_session_launch.rs:106-124` | create/respawn(`:156-168`) 전 | `execution_end`. dead pane 재사용이면 `tmux_pane_dead`가 여기서 잡힌다 |
| E3 | legacy wrapper launch `claude.rs:2600-2608` | 옛 marker 삭제 전 | `execution_end` |
| E5 | tmux → Herdr 첫 전환 `claude/herdr_turn/handoff.rs:99-110` | `pinned_tmux::end` 성공 뒤 | `tmux_closed` |
| E4 | boot debt 구동(§2.5) | outcome 없는 keyed boot input이 있고 그 target의 `end`가 없을 때 | `execution_end`. 현재 row·최신 Bound의 nonce가 target과 다르면 쓰지 않는다 |

- 기록할 옛 실행이 없으면 아무것도 쓰지 않는다.
- E1~E3·E5는 `end`를 쓰기로 정한 뒤 durable하게 써지지 않으면 launch를 거절한다. 판독이 `Live`나 `Unknown`이라 쓰지 않는 경우는 거절하지 않는다.
- spawn 기록이 없는 실행(S3a-1 착지 전에 뜬 실행 포함)은 tmux 판독이 `Unknown`이고 S2 출구만 가진다. 옛 형식을 다시 읽는 장치는 두지 않는다.
- N2의 종료나 N2의 hosted row는 N1의 종료 근거가 아니다.

### 2.3 cut과 범위 안 귀속

S3a의 cut은 `end`의 `end_len`이다. N1이 끝난 것을 확인한 뒤, 후속을 만들기 전에 잰 길이다. 그래서 그 앞의 byte는 후속이 쓴 것이 아니다.

T가 E와 맞으면(§2.7의 조건 1) N1용 분석은 `range.1 <= end_len`인 fact slice다. B는 지금 있는 것만 쓴다.

- 같은 dev이고 exact stat이 맞음.
- 또는 S2 refresh가 이미 만든 승인 계보(`tui_o/store/source_refresh.rs:195-199`)와 인증 prefix. 이 계보는 §2.11의 validator가 검사한다.

B가 없거나 `end_len`이 비면 S3a는 범위 안 귀속도 결과 없는 정산도 하지 않는다(§3).

### 2.4 역사적 raw EOF

일반 capture는 개행까지를 record로 만든다. 개행 없이 끝난 온전한 JSON은 partial에 남는다(`tui_o/shadow/capture.rs:389-419`). 지금의 raw EOF 인증은 현재 hosted row가 원본 N1일 때만 성립한다(`discord/turn_finalizer/native_terminal.rs:122-129`).

S3a는 `end`에 묶인 raw EOF를 더한다(새 child `discord/turn_finalizer/native_end_raw.rs`).

- 조건: E가 T와 맞고 `end_len`이 있으며 B와 durable receipt가 있다.
- 읽기: §2.12의 해독이 `end_len`에서 멈춘 상태의 tail만 본다. 현재 EOF와 N2가 쓴 byte는 해독하지 않는다.
- 판정: tail이 기존 strict closer parser(`tui_o/writer/input_facts/native/terminal.rs:122-136,153-167`)를 통과하면 `Executed` 또는 `Cancelled`로 봉인한다.
- 현재 capture frontier를 빌리지 않는다. 기존 `within()`(`tui_o/writer/input_facts/native/terminal.rs:170-179`)은 현재 경로에만 남긴다.

결과 없는 정산(§2.7)은 raw tail도 증거로 본다. `end_len` 안에 strict closer tail이 있으면 결과 없이 정산하지 않는다.

### 2.5 boot debt 구동, source별 scan, receipt, writer 인계

**구동.** design-5와 같다.

- turn finalizer의 reconcile tick(1초, `discord/turn_finalizer.rs:103,600-601`)이 boot input 순회를 `spawn`한다. 자리는 `native_policy::reconcile` 옆(`discord/turn_finalizer/reconcile.rs:231-240`)이다.
- `cached_shared`는 `FinalizeMsg::NativeBoot` 처리에서 채운다(`discord/turn_finalizer/actor_state.rs:130-135,290-298`).
- 채널마다 시도 하나를 돌리고, 끝난 뒤 5초 간격을 둔다(`discord/runtime_bootstrap/native_input.rs:6`).
- 한 ticket의 실패는 그 ticket만 다음 시도로 넘긴다.

**시도 내용**(새 child `discord/turn_finalizer/native_boot_debt.rs`).

1. `end`가 없으면 E4.
2. 저장 barrier 건의 종료 정리(파일을 읽지 않음).
3. 저장 결과의 completion. 진입점 전제는 §2.6, latch는 §2.13을 따른다.
4. `end`·`end_len`·B가 모두 있는 건(아래 "인계된 ticket")의 source scan(새 child `discord/turn_finalizer/native_end_scan.rs`).

**scan의 순서 (P1-2).** closer를 읽는 일과 귀속 권한을 durable하게 세우는 일을 나눈다. S2 writer의 순서(`tui_o/writer/rotation/owe.rs:616-651`)를 finalizer에서 같은 의미로 되풀이한다.

1. **bounded scan**: §2.12대로 그 ticket의 source만 다시 읽어 `0..end_len`의 fact와 raw tail을 만든다. 이 단계는 아무것도 쓰지 않는다.
2. **receipt 후보**: fact에 T의 membership 증거(prompt record)가 있으면 후보를 만든다. 없으면 T는 receipt 없이 남고, 결과 없는 정산 판단(§2.7)으로 간다.
3. **durable receipt**: `try_scoped_native_receipt`(`discord/turn_finalizer/native_supply.rs:226-283`)를 부른다. 이 안의 exact validate(`tui_input/human_frame/history/v2/native_receipt.rs:26`)는 §2.11의 계보 허용 validator로 바꾼다. receipt가 durable하게 써지지 않으면 이 시도는 끝나고 outcome으로 가지 않는다.
4. **재확인**: source fence와 publication snapshot 아래에서 registration fingerprint(`registrations_fingerprint`)와 `end` digest를 다시 읽는다. 다르면 다음 시도로 넘긴다.
5. **scoped reduce**: `range.1 <= end_len`인 fact slice로 `reduce`를 부른다. raw tail은 §2.4.
6. **outcome CAS**: 증거가 있으면 기존 Registry CAS로 `Executed`를 기록한다. 이 기록은 durable receipt를 요구한다(`tui_input/human_frame/history/v2/native.rs:618-632`).
7. completion은 §2.13의 latch 아래에서 한다.

cursor는 읽기만 한다. writer의 spool·cursor·memo에는 쓰지 않는다.

**writer 인계 (P1-3).** writer의 S2 공급 자격은 좁히지 않는다. writer는 지금처럼 ticket의 원본 Bound(`tui_o/writer/rotation/owe.rs:425-457`)와 `check_origin`(`discord/turn_finalizer/native_supply.rs:109-159`)으로 공급한다. 현재 실행이 N2여도 마찬가지다.

예외는 **인계된 ticket** 하나뿐이다. 인계된 ticket은 `end`, `end_len`(cut), B가 모두 있는 boot ticket이다. writer는 이 ticket을 offer에서 뺀다. 이유는 writer의 귀속이 현재 frontier를 쓰기 때문이다. 현재 frontier는 `end_len`을 넘어 N2의 suffix를 포함할 수 있다(`NoSuffixAttribution`). 인계는 S3a-4에서 scan과 같이 착지한다. S3a-3만 있는 상태에서는 writer가 모든 ticket을 지금처럼 공급한다.

다른 cursor의 reopen 실패로 writer가 멈춰도, scan은 그 ticket의 source만 연다(design-5 §2.5). 이 source 자신이 stat되지 않으면 그 ticket만 기다린다.

### 2.6 진입점별 completion 전제 (P1-1)

S2에서 저장 결과 completion에 들어가는 진입점을 `git grep`(`prepare_completion(`, `settle_saved(`, `refreshed(`, `.retry()`, `complete_native_turn`, `control_proof`)으로 다시 셌다. design-6은 진입점마다 S2 전제를 유지한다.

| 진입점 | 좌표 | S2 전제 | design-5 | design-6 |
|---|---|---|---|---|
| writer 새 outcome 직후 `settle_saved` | `discord/turn_finalizer/native_supply.rs:325,540` → `discord/turn_finalizer/native_restore.rs:95-102` | `refreshed()`의 exact `Source::validate`(`:73-92`). 실패하면 정리하지 않고 다음 시도 | Await gate | S2와 같음. seal이 있고 dev만 바뀌었으면 hash로 읽기(아래 판정표) |
| retry job 생성 | `discord/turn_finalizer/native_supply.rs:664-675` | exact validate가 통과해야 job을 만듦 | Await gate | S2와 같음 + 적극적인 변화 증거면 `Skipped` |
| boot restore `prepare_completion` | `discord/turn_finalizer/native_restore.rs:418-430` | `refreshed` 없음. 앞단은 barrier만 stat(`tui_input/human_frame/history/v2/native.rs:156-177`). 읽기 실패면 경고 뒤 진행(`discord/turn_finalizer/native_completion.rs:74-81`) | **Await gate(새 보류)** | **S2와 같음.** stat gate를 붙이지 않는다. seal이 있고 파일이 읽히면 hash가 맞을 때만 본문을 쓴다. 다르면 `Skipped` |
| live completion | `discord/turn_finalizer/terminal_handler.rs:695-747`, `discord/turn_finalizer/actor_state.rs:341-365`, `discord/turn_finalizer/native_completion.rs:105-127` | stat 없음. run 범위만 읽고 실패하면 경고 뒤 진행 | 같은 gate 대상으로 적힘 | **S2와 같음.** gate 없음 |
| 제어 결과 restore | `discord/turn_finalizer/native_restore.rs:298,592-601` | `control_proof`로 검증. 본문을 읽지 않음 | 구별 없음 | **파일 gate 없음.** 제어 결과의 cleanup은 파일 접근에 묶지 않는다 |
| follower·Cancelled·TransferredUnverified 하류 | `discord/turn_finalizer/native_completion.rs:203-283` | 본문 읽기 없이 하류 기록 | 구별 없음 | 파일 gate 없음. latch(§2.13)만 본다 |

판정표(새 child `discord/turn_finalizer/native_source_fate.rs`)는 settle_saved와 retry 진입점에만 쓴다.

| 관측 | S2 | S3a |
|---|---|---|
| path stat 실패(모든 errno), fence busy | job 없음, 다음 retry | 같음: `Await` |
| stat 성공, dev·inode 같음, 길이 ≥ `through` | completion 읽기 | 같음. seal이 있으면 hash가 같을 때 읽고, 다르면 `Skipped` |
| inode 같음, dev만 다름, seal 있음, hash 같음 | 영구 대기 | 읽기(`Recorded`) |
| inode 같음, dev만 다름, seal 없음 | 영구 대기 | 같음: `Await` |
| inode 다름, 또는 길이 < `through` | 영구 대기 | `Skipped` |

`Skipped`에는 원본 내용이 그 path에서 사라졌다는 적극적인 증거가 필요하다. "지금 읽을 수 없다"는 판정에는 쓰지 않는다.

### 2.7 종료 정산

outcome이 없는 keyed boot input T에 대해 아래가 성립하면 정산한다.

1. E가 있고, `E.channel`·`owner`·`execution_nonce`·`host`가 T와 같으며, `E.source`가 T의 source와 같은 파일(session·path·inode)이다.
2. 다음 중 하나다.
   - T가 저장 barrier 건이다(파일을 읽지 않음).
   - 파일 판정이 "inode 다름" 또는 "길이 < `through`"다.
   - `end_len`과 B가 있고, §2.5 scan의 `0..end_len` 분석과 raw tail에 T의 완전한 증거가 없다.
3. T의 registration fingerprint와 `end` digest를 source fence와 publication snapshot 아래에서 다시 확인하고 Registry CAS를 한다.

| T의 저장 상태 | 정산 | 정리 |
|---|---|---|
| barrier 보관 | 기존 abstain. `RestoredInput::abstain_idle`(`discord/turn_finalizer/native_restore.rs:122-201`)이 admission으로 `end`도 받는다(새 child `discord/turn_finalizer/native_admission.rs`) | 같은 함수의 exact row CAS |
| barrier 없음 | `TransferredUnverified`를 `end_proof`와 함께 한 번 기록(새 child `tui_input/human_frame/history/v2/native_end.rs`, 판정 `discord/turn_finalizer/native_end_release.rs`) | 기존 `settle_saved` → `drive` |

- `end_proof`에는 E의 식별 필드, `basis`, cut(`end_len`과 prefix hash) 또는 파일 판정을 담는다. `validate_record`(`discord/turn_finalizer/native_restore.rs:585-673`)에 분기를 더한다.
- 대상은 boot input뿐이다. live input은 기존 제어 경로가 맡는다(`discord/turn_finalizer/native_policy.rs:23-38`).

### 2.8 전역 boot gate

두 전역 boot gate 모두 구조 검증으로 진행한다. 하나는 `BootInputs::prepare`이고, 다른 하나는 `discord/inflight/removal/boot_reaper.rs:83-92,119` → `boot_custody.rs:435-443` → `tui_input/human_frame/history/v2/native.rs:229-275` 경로다. abstain boot 순회의 `?`(`discord/turn_finalizer/native_supply.rs:447`)는 입력별 건너뜀으로 바꾼다.

### 2.9 유지되는 불변식 (S3a 범위)

| 불변식 | 지키는 방법 |
|---|---|
| IdleNotExpandedCrossDev | dev가 바뀌면 N2 idle은 admission을 받지 못한다 |
| NoSuffixAttribution | scoped 입력과 raw tail은 `end_len` 이하. 인계된 ticket은 writer가 공급하지 않는다 |
| RejectUnverifiedFile | 범위 안 귀속에는 B(같은 dev exact 또는 §2.11 계보)가 있어야 한다 |
| EndProofOwned | `end`는 host 메서드의 `Ended` 판독으로만 쓴다 |
| ReleaseNeedsEnd | admission과 정산은 CAS 시점에 `end`를 다시 읽는다 |
| NoSilentLoss | 건너뜀과 closer 있는 턴의 결과 없는 정산에는 적극적인 증거가 있어야 한다. boot restore 진입은 S2 동등 |
| NoLostAnswer | cut 안에 증거(raw tail 포함)가 없을 때만 결과 없이 정산한다 |
| OneCompletionEffect (design-6) | durable completion latch |
| CompletionBeforeCleanup, SkipOnlyWhenTerminal | 반환값 gate와 파일 판정 |
| SuccessorUntouched, ExactTarget, OneOutcome, SavedOutcomePreserved | 기존 exact row CAS, exact ticket CAS, 단일 outcome CAS |

### 2.10 host 다형성: 실행 종료 판독 (P1-NEW-2)

tmux에만 있는 판독(빈 세션 판정, `pane_dead`)을 `InteractiveSessionHost`(`session_host/traits.rs:9-56`) 뒤로 옮긴다. 호출자는 target의 `host`로 `session_host::resolve::host_for`(`session_host/resolve.rs:452`)를 고르고 메서드 하나만 부른다. host 이름으로 분기하지 않는다. 새 타입은 `session_host/execution_end.rs`에 둔다. 이 crate는 adk-runtime이고 `session_host`, `platform::tmux`, `session_host::herdr_gate`도 같은 crate에 있다.

```text
fn execution_end(&self, session: HostSessionRef<'_>, spawn: &ExecutionSpawn) -> ExecutionEnd
enum ExecutionEnd { Ended(EndBasis), Live, Unknown(String) }
```

**trait 메서드**

| 메서드 | 호출 지점 | 계약 |
|---|---|---|
| `execution_end` (새) | E1 `herdr_launch.rs:492-498`, E2 `claude/tui_session_launch.rs:106-124`(respawn 분기 `:156-168` 전), E3 `claude.rs:2600-2608`, E4 `discord/turn_finalizer/native_end_prover.rs`(새) | 읽기만 한다. 2초 안에 답하지 못하면 `Unknown`. `Ended`는 host가 원본 실행의 끝을 직접 확인한 경우만이다. marker 부재, `Retired` row, N2의 상태는 근거가 아니다. 기본 구현은 `Unknown` |
| `presence`, `liveness` (기존) | `session_host/tmux_host.rs:217,229` | 바꾸지 않는다. `execution_end`의 tmux 구현이 안에서 쓴다 |
| `execution_pid` (기존) | `session_host/tmux_host.rs:278` | 바꾸지 않는다. spawn 기록을 쓸 때 pane pid를 얻는 데 쓴다 |

**host별 구현**

| 메서드 | tmux (`TmuxHost`, `session_host/tmux_host.rs:201`) | Herdr (`ReadOnlyHerdrHost`, `session_host/herdr_registry.rs:60`) |
|---|---|---|
| `execution_end` | 1. spawn 기록이 없으면 `Unknown`. 2. 기록의 `boot`가 지금 boot time(`claude_tui/source_verify/renumber.rs:29-48`)과 다르면 `Ended(tmux_rebooted)`. 3. 세션 Missing이고 기록된 pid 신원과 `pgid`가 끝났으면 `Ended(tmux_vacant)`. 4. 기록된 pane의 `pane_dead = 1`(`platform/tmux.rs:962-985`), `pane_pid` 신원이 기록과 같고 `pgid`가 끝났으면 `Ended(tmux_pane_dead)`. 5. 그 밖은 `Live`(pane 살아 있음) 또는 `Unknown` | `HerdrTarget::ended_reading` + `provider_absent()`(`session_host/herdr_gate.rs:470,932-943`). `discord/herdr_terminate.rs:191-205`의 `read_end`도 이 구현을 부르게 바꿔 판독을 하나로 둔다. endpoint가 없으면 `Unknown` |
| spawn 기록 쓰기 | 필요. create/respawn 뒤 `execution_pid`로 pane pid를 얻어 `spawn` 항목을 쓴다(`claude/tui_session_launch.rs:156-168` 뒤). 실패해도 launch는 진행하고 그 실행만 `Unknown`이 된다 | **필요 없음.** Herdr 실행 신원은 이미 durable한 `HostedExecution` 행에 있다(`discord/turn_finalizer/native_prepare.rs:26-40`의 `load_hosted_execution_pg`). Herdr pane과 provider process는 dcserver 재시작에도 Herdr 서버에 남고, 판독은 그 행으로 한다(`discord/herdr_terminate.rs:193`) |
| dead pane 재사용 | 필요. respawn 전 E2에서 판독(위 4) | **필요 없음.** Herdr launch는 dead pane을 제자리에서 다시 쓰지 않는다. 이전 실행이 `Retired`이면 E1이 판독한다(`herdr_launch.rs:492-509`) |
| 재부팅 판독 | 필요(위 2) | 이번에 더하지 않는다. Herdr 서버가 재부팅 뒤 돌아오지 않으면 `Unknown`이고 S2 출구만 남는다(§8) |

`ProcessHost`(`session_host/process_host.rs:21`)와 `UnconfiguredHerdrHost`(`session_host/herdr_host.rs:328`)는 기본 구현(`Unknown`)을 쓴다. `ProcessHost`를 target으로 하는 keyed ticket은 없고, `UnconfiguredHerdrHost`는 endpoint가 없는 상태다.

**시험(두 host 모두).**

| host | 주입 | 단언 |
|---|---|---|
| tmux | spawn 기록 + boot time 변경(시험 seam `boot_time_for_tests`) | `Ended(tmux_rebooted)`, E4가 `end` 1건 기록 |
| tmux | 같은 boot, 세션 kill, pid 신원 종료 | `Ended(tmux_vacant)` |
| tmux | provider 종료 뒤 pane이 dead로 남음 → 같은 세션 launch | respawn 전에 `end`가 디스크에 있음(`tmux_pane_dead`) |
| tmux | pane 살아 있음 / spawn 기록 없음 | `Live` / `Unknown`, 기록 0, launch 진행 |
| Herdr | `Retired` + provider 부재 참/거짓 | `Ended(herdr_ended)` / `Live`. 거짓이면 기록 0 |
| Herdr | endpoint 없음 | `Unknown`, 기록 0 |
| 공통 | 같은 E4 코드가 `host_for(kind)`로 두 host를 부름 | host 이름 분기 0(호출 지점은 메서드 하나) |

### 2.11 계보 허용 validator (P1-NEW-1)

exact `Source::validate`(`tui_input/human_frame/history/v2.rs:21-35`)는 그대로 둔다. 새 함수 `Source::validate_lineage(key, devices)`(새 child `tui_input/human_frame/history/v2/lineage.rs`)를 더한다.

- inode가 같고, dev가 `self.file[0]`이거나 refresh 계보의 `devices`(`tui_o/store/rotation.rs`의 `devices`, `same_registered_file`)에 있다.
- 길이 ≥ `offset`이다.
- prefix는 refresh가 인증한 hash와 같아야 한다. 계보는 그 인증을 거쳐야만 생긴다(`tui_o/store/source_refresh.rs:195-199`).

exact를 쓰던 호출 가운데 B가 필요한 곳만 바꾼다. 바꾸는 곳: receipt(`tui_input/human_frame/history/v2/native_receipt.rs:26`), 증거 공급(`discord/turn_finalizer/native_supply.rs:786`, main 821행), barrier(`tui_input/human_frame/history/v2/native_barrier.rs:20,121`), refreshed(`discord/turn_finalizer/native_restore.rs:87`). `tui_input/human_frame/history/v2.rs:382,435,715,951`은 같은 계보 인자를 받게 바꾼다. 계보가 없으면 결과는 exact와 같다.

dev seam(`tui_input/human_frame/history.rs`의 `file_id`가 `capture::file_identity`를 부름)은 이 validator와 같은 조각에 둔다(§7).

### 2.12 bounded 해독과 cursor 계보 (P1-NEW-3a, P2-1)

`Projection::reopen(path, cursor, through)`(`tui_o/writer/input_facts/native.rs:249-277`)를 `end_len`보다 긴 `through`로 부르지 않는다. 새 함수 `Projection::reopen_bounded`(새 child `tui_o/writer/input_facts/native/scoped.rs`)는 범위를 둘로 나눈다.

- **인증 범위** `0..captured_through`: 저장 cursor의 prefix hash를 확인한다(`tui_o/shadow/capture.rs:245-275,282-295`). `captured_through`가 `end_len`보다 길어도 hash는 끝까지 확인한다.
- **해독 범위** `0..end_len`: decoder는 `end_len`을 EOF로 보고 멈춘다. 마지막 완전 record 경계부터 `end_len`까지는 raw tail이 된다. N2가 나중에 붙인 개행으로 생긴 결합 줄은 해독하지 않는다.

cursor는 refresh 계보로 찾는다.

- refresh가 cursor 키를 옮겼으면(`tui_o/store/source_refresh.rs:144,210`) 계보의 현재 키로 찾는다.
- cursor가 없으면(`pin_source`가 만들지 않음) 인증 범위는 비고 B는 exact stat(같은 dev, 같은 inode, 길이 ≥ `end_len`)만 인정한다. 계보 B만 있는 ticket은 기다린다.

읽기량 상한은 `max(captured_through, end_len)` byte다(§4.2 C10).

### 2.13 completion latch (P1-NEW-3b, P2-2)

completion 부작용(transcript DB, selector, scheduled, work dispatch 완료: `discord/turn_finalizer/native_completion.rs:215-283`, `discord/turn_bridge/completion_guard.rs:552`)은 ticket마다 한 번이어야 한다. 두 장치를 함께 쓴다.

- **동시 진입**: design-5의 actor 진행 표시(in-memory)를 그대로 둔다. 한 process 안의 겹침을 막는다.
- **순차 재진입**: native settlement에 durable 단방향 latch `completion: Pending → Recorded | Skipped`를 더한다(새 child `tui_input/human_frame/history/v2/native_latch.rs`). 쓰는 시점은 부작용이 모두 끝난 뒤, cleanup CAS 전이다. 기존 레코드에 필드가 없으면 `Pending`이다(serde 기본값). 옛 형식 판독 장치는 두지 않는다.

모든 진입점(§2.6 표의 여섯 줄, `BootInputs::restore`의 `:425` 포함)이 completion 전에 latch를 읽는다. `Recorded`·`Skipped`면 부작용을 건너뛰고 cleanup으로 간다. 그래서 Settle이 `discord/turn_finalizer/native_restore.rs:317-327`이나 `:338`에서 정리 없이 돌아온 뒤 다시 들어와도 부작용은 다시 돌지 않는다.

process가 부작용 도중 죽으면 latch는 `Pending`으로 남고 다음 진입이 다시 실행한다. 이것은 S2와 같은 최소 1회 동작이고 S3a가 넓히지 않는다.

`S3aNoLatch` 변형은 `OneCompletionEffect`를 깬다.

## 3. 다음 라운드 (S3b)

이번 라운드에서는 풀지 않는다. design-4 §3의 S3b 계약을 출발점으로 남긴다.

- design-4 리뷰 P1-2: 새 Bound만 생기고 refresh hook이 없는 경우의 B.
- design-4 리뷰 P1-3: cut 없는 역사적 ticket의 readable closer.
- S2와 같이 남긴 공백: 끝내 stat되지 않는 path(`GoneS3a`), retry 진입의 seal 없는 dev 변경 결과(`LegacyDevS3a`).
- spawn 기록 없이 뜬 tmux 실행, 재부팅 뒤 돌아오지 않는 Herdr 서버의 종료 판독.
- `superseded`의 증명 강도(design-4 리뷰 P2-1).

## 4. 반응 시간 계약

### 4.1 지금 있는 계약

| # | 계약 | 좌표 |
|---|---|---|
| T1 | writer poll: iteration 작업을 마친 뒤 1초 기다린다 | `tui_o/writer/actor.rs:23,441-444,486-488`, `tui_o/writer/rotation/owe.rs:575-579` |
| T2 | source당 한 poll에 최대 1 MiB, 끝나지 않은 record는 16 MiB에서 멈춤 | `tui_o/shadow/mod.rs:17`, `tui_o/shadow/capture.rs:16` |
| T3 | `RuntimeHintFence::try_acquire`는 기다리지 않음 | `tmux_common/runtime_hint_fence.rs:291-348` |
| T4 | launch는 fence를 Herdr `herdr_launch.rs:455-577`, tmux `claude/tui_session_launch.rs:106` ~ `claude.rs:1835` 구간에서 쥔다 | 같은 좌표 |
| T5 | source authority는 세션별 프로세스 mutex이고 동기다 | `tmux_common.rs:120-144`, `discord/turn_finalizer/native_supply.rs:66-75` |
| T6 | 전역 boot restore 재시도 5초, 경고 60초 | `discord/runtime_bootstrap/native_input.rs:6,9` |
| T7 | live completion은 run 범위만 한 번 읽고 바로 cleanup 메시지를 보냄 | `discord/turn_finalizer/native_completion.rs:105-127`, `discord/turn_finalizer/actor_state.rs:341-365` |
| T8 | `append_spool`은 publication lock 없이 쓰고 fsync한다 | `tui_o/store/spool.rs:488-558` |
| T9 | turn finalizer reconcile tick 1초, 놓친 tick은 건너뜀 | `discord/turn_finalizer.rs:103,600-601` |

### 4.2 S3a가 더하는 것

| # | 동작 | fence | 쥔 채 하는 일 | 주기 |
|---|---|---|---|---|
| C2 | live completion | 없음 | latch 읽기 한 번 더 | 지금과 같음(T7) |
| C3 | settle_saved·retry의 cross-dev completion | 없음 | blocking pool에서 `0..through` hash 한 번 | 시도마다, 결과 기억 |
| C5 | E1~E3·E5 | 이미 쥔 launch fence | `execution_end` 판독(2초 상한), 32 KiB 이하 파일 교체·fsync 한 번 | launch마다 |
| C5s | tmux spawn 기록 | 이미 쥔 launch fence | pane pid 조회와 파일 교체·fsync 한 번 | tmux launch마다 |
| C6 | E4 | `try_acquire`(대기 0) | `execution_end` 판독(2초 상한), stat, 기록 | 채널마다 시도 하나, 끝난 뒤 5초(T6 간격) |
| C10 | source scan | 없음 | blocking pool에서 `max(captured_through, end_len)` byte 읽기 | 시도마다. (source, `captured_through`, hash, `end_len`)로 결과 기억 |
| C12 | scan receipt | Registry snapshot | receipt CAS 한 번과 fingerprint 재확인 | scan마다 |
| C11 | completion latch | 없음 | latch CAS 한 번 | completion마다 |

- tick은 시도를 `spawn`하고 기다리지 않는다. T9의 1초 tick은 늘지 않는다.
- writer poll(T1)은 바뀌지 않는다. 인계된 ticket을 offer에서 빼는 비교 하나만 더한다.
- 기존 시험의 대기 창은 늘리지 않는다. 새 상수는 2초 판독 상한 하나다.

## 5. 상태 소비 경로 전수표

행 1~45는 design-2, 행 46~58은 design-3의 표다. design-3에서 계약이 달라진 행은 ★로 표시했다. 행 59~68은 design-4, 행 69~75는 design-5에서 더한 것이다. 행 69~75는 정산 구동·재시도를 만들거나 소비하는 진입점을 S2에서 `git grep`(`refreshed(`, `settle_saved(`, `prepare_completion(`, `.retry()`, `source.validate(`, `reopen_native(`, `offer_native(`, `HaltedMissingSource`, `FinalizeMsg::NativeBoot`, `native_policy::reconcile(`)으로 다시 센 것이다. 계약 끝의 (S3a)는 그 단계에서 적용된다는 뜻이고, 표시 없는 행은 바뀌지 않는다. 행 76~85는 design-6에서 더한 것이다(§2.6, §2.10~§2.13).

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

| 76 | boot restore completion 진입 | `discord/turn_finalizer/native_restore.rs:418-430`, `tui_input/human_frame/history/v2/native.rs:156-177` | S2처럼 stat gate가 없다. seal이 있고 파일이 읽히면 hash가 맞을 때만 본문을 쓴다(§2.6) (S3a) |
| 77 | live completion 진입 | `discord/turn_finalizer/terminal_handler.rs:695-747`, `discord/turn_finalizer/actor_state.rs:341-365`, `discord/turn_finalizer/native_completion.rs:74-81` | gate 없음. latch만 본다(§2.13) (S3a) |
| 78 | 제어 결과의 restore | `discord/turn_finalizer/native_restore.rs:298,592-601` | `control_proof` 검증 그대로. 파일 gate를 붙이지 않는다 (S3a) |
| 79 | membership receipt | `discord/turn_finalizer/native_supply.rs:226-283`, `tui_input/human_frame/history/v2/native_receipt.rs:26`, `tui_input/human_frame/history/v2/native.rs:618-632`, `tui_o/writer/rotation/owe.rs:616-651` | finalizer scan도 durable receipt를 만든 뒤에만 outcome을 기록한다(§2.5) (S3a) |
| 80 | writer의 원본 Bound 공급 | `tui_o/writer/rotation/owe.rs:425-457,695-726`, `discord/turn_finalizer/native_supply.rs:109-159,520-568` | S2 경로 유지. 인계된 ticket만 offer에서 뺀다 (S3a) |
| 81 | exact `Source::validate` 호출 | `tui_input/human_frame/history/v2.rs:21-35,382,435,715,951`, `tui_input/human_frame/history/v2/native_receipt.rs:26`, `discord/turn_finalizer/native_supply.rs:786`, `tui_input/human_frame/history/v2/native_barrier.rs:20,121`, `discord/turn_finalizer/native_restore.rs:87` | B가 필요한 곳은 계보 허용 validator(§2.11) (S3a) |
| 82 | tmux 실행 종료 판독 | `discord/commands/control/clear_teardown/clear_end_record.rs:73-101,133-138`, `platform/tmux.rs:203,962-985`, `claude/tui_session_launch.rs:156-168` | host 메서드 `execution_end`의 tmux 구현과 spawn 기록(§2.10) (S3a) |
| 83 | Herdr 실행 종료 판독 | `discord/herdr_terminate.rs:191-205`, `session_host/herdr_gate.rs:470,932-943` | 같은 메서드의 Herdr 구현. `read_end`도 이것을 부른다 (S3a) |
| 84 | Settle 재진입 | `discord/turn_finalizer/native_restore.rs:317-327,338,425`, `discord/turn_finalizer/native_completion.rs:215-283` | durable completion latch(§2.13) (S3a) |
| 85 | cursor 키 이동과 부재 | `tui_o/store/source_refresh.rs:144,210`, `tui_o/writer/input_facts/native.rs:249-277,388` | 인증 범위와 해독 범위 분리, 계보로 cursor 찾기(§2.12) (S3a) |

Cancel·Terminate·clear·purge·사용자 Stop·attach/paste(행 37~43)는 종료 기록을 읽지 않고, 역사적 ticket의 대체 출구로도 쓰지 않는다. 종료 기록의 소비자는 행 29와 §2.7의 정산뿐이다.

### 5.1 모델 변수와 표의 대응

| 모델 변수 | 값 | 가져온 행 |
|---|---|---|
| `scen` | complete, closerLate, rawCloser, killed, barrierStored, outcomeSaved, outcomeLegacy | 7·10, 35, 64, 14·15, 18·19·25 |
| `dev`, `file` | same·changed / ok·late·gone·changed. 제품은 late와 gone을 구별하지 못하므로 S3a 규칙은 둘을 같게 다룬다 | 5·6·9·33·43·45·69 |
| `approved` | 파일 승인(B) | 5·6·59·81 |
| `writer`, `otherBad` | alive·halted, 다른 cursor의 reopen 실패 | 60·71 |
| `n1`, `n2`, `rowN` | alive·dead / absent·running·idle / n1·n2 | 1·2·27·36·52·53·62 |
| `switched` | 앞 source의 ticket | 63 |
| `endRec`, `cutKnown` | 종료 기록 유무, `end_len` 유무 | 52·53·55·65·66 |
| `fileCloser`, `witCloser`, `witFinal` | 파일의 closer, 범위 안 closer, 범위 확정 | 7·12·64 |
| `barrier`, `outcome`, `sealed`, `completion` | 저장 barrier, outcome, seal, completion 상태 | 14·15·16·18·19·25·31·32·33 |
| `cleared`, `clearedBy`, `rowOwner`, `succRemoved` | 정리 여부와 근거, 채널 행의 주인 | 23·29·30·34 |
| `boot`, `t2` | ready·refused, 다른 target의 ticket | 20·46, 13·19 |
| `entry` (design-6) | 저장 결과의 진입점: restore(boot restore), retry(settle_saved·retry job) | 69·76 |
| `receipt` (design-6) | durable membership receipt 유무 | 79 |
| `host` (design-6) | herdr, tmuxReboot, tmuxDead, tmuxUnknown | 82·83 |
| `fx` (design-6) | completion 부작용 실행 횟수 | 74·84 |

live completion(행 77)과 제어 결과(행 78)는 살아 있는 actor나 별도 proof가 필요해 모델에 넣지 않았다. 제품 RED가 맡는다(§7.3).

## 6. 상태 차트

모델과 실행 결과는 [모델 보고서](hook-transcript-custody.report.md)에 있다. 요약만 적는다.

- **모듈**: `HookTranscriptCustody.tla`, 설정 95개.
- **규칙**: S2(현재 코드), D2·D3, `S3a4`(design-4), `S3a5`(design-5), `S3a`(design-6), Full(design-6 S3a + design-4 S3b).
- **design-6에서 더한 동작**:
  - `WriterReceipt`, `ScanReceipt`: writer와 finalizer scan의 durable receipt 생산. `Settle`·`RawEOFLive`·`HistRawEOF`는 receipt를 요구한다.
  - 진입점별 completion: `ReadCompletion`·`SkipCompletion`이 `entry`를 본다.
  - `CleanupDefer`: Settle이 정리 없이 돌아온 뒤 completion이 다시 대기로 돌아가는 재진입. latch가 있으면 일어나지 않는다.
  - host별 종료 판독: `EndReadable`이 `host`를 본다.
  - writer 인계: `WriterServes`는 인계된 ticket(`endRec ∧ cutKnown ∧ approved`)에서만 거짓이다.
- **design-6에서 더한 규칙**: `OneCompletionEffect`(`fx <= 1`). `NoSilentLoss`와 `SkipOnlyWhenTerminal`은 restore 진입의 S2 동등 건너뜀을 허용한다.

수정 전(`S3a5`)과 후(`S3a`). 같은 모집단을 S2에도 돌렸다(`·` 앞은 불변식, 뒤는 liveness).

| 경로 | S2 | design-5 S3a (`S3a5`) | design-6 S3a (`S3a`) |
|---|---|---|---|
| S3a 범위 전체(`S3a5`·`S3a`) | — | IdleDoesNotExpand, **OneCompletionEffect** · **Channel, Eligible, SameDevKilled** | IdleDoesNotExpand · - |
| Restore (boot restore 진입) | OneCompletionEffect · Channel | OneCompletionEffect · **Channel** | - · - |
| Receipt (receipt 없이 시작) | OneCompletionEffect · Channel, Eligible | OneCompletionEffect · **Channel, Eligible** | - · - |
| Historic (설치 전 이미 N2) | OneCompletionEffect · - | - · **Channel, Eligible** | - · - |
| Tmux (재부팅·dead pane) | IdleDoesNotExpand, OneCompletionEffect · Boot, Channel, Eligible | IdleDoesNotExpand, OneCompletionEffect · **Channel, Eligible** | IdleDoesNotExpand · - |
| Reentry (Settle 재진입) | **OneCompletionEffect** · - | **OneCompletionEffect** · - | - · - |
| TmuxUnknown (판독 불가) | IdleDoesNotExpand, OneCompletionEffect · Boot, Channel, Eligible | — | IdleDoesNotExpand · Channel, Eligible |

- `Historic`의 S2는 liveness가 모두 통과한다. S2에 있던 출구를 design-5가 지운 것이 `HistoricS3a5`의 Channel·Eligible이다.
- `TmuxUnknown`의 S3a는 종료 기록 없이 S2 출구만 가지므로, 같은 모집단의 S2처럼 Channel이 깨진다. S2보다 나빠지지 않는다(Boot는 통과).
- 변형 넷은 각자 겨냥한 규칙을 깬다: `S3aRestoreAwait`(restore 진입에 Await gate) → Channel, `S3aScanNoReceipt`(scan이 receipt를 만들지 않음) → Channel·Eligible, `S3aTmuxLiveOnly`(tmux 판독에 live pane 필요) → Channel·Eligible, `S3aNoLatch`(latch 없음) → OneCompletionEffect.
- 옛 규칙(S2·D2·D3·S3a4)의 설정 다수는 `OneCompletionEffect`가 새로 깨진다. latch는 design-6에만 있기 때문이다. S2 설정 셋(`S2`, `GoneS2`, `LegacyDevS2`)은 `NoSilentLoss`가 더는 깨지지 않는다. S2 모델의 retry 진입이 exact stat 뒤에서 기다리도록 바로잡았기 때문이다(`discord/turn_finalizer/native_supply.rs:664-675`). 건너뜀은 restore 진입에서만 일어나고, 그것은 S2 동등으로 허용했다.
- `Full`(`Design`)은 불변식 15개와 liveness 5개를 모두 통과한다.

차트: boot restore [S3a5](hook-transcript-custody-restore-before.chart.mmd) · [S3a](hook-transcript-custody-restore-after.chart.mmd), 설치 전 N2 [S3a5](hook-transcript-custody-historic-before.chart.mmd) · [S3a](hook-transcript-custody-historic-after.chart.mmd), Settle 재진입 [S3a5](hook-transcript-custody-reentry-before.chart.mmd) · [S3a](hook-transcript-custody-reentry-after.chart.mmd), receipt [S3a5](hook-transcript-custody-receipt-before.chart.mmd). `ReceiptS3a`는 994 상태라 차트 상한(500)을 넘어 그리지 않았다.

## 7. 구현 조각 (S3a)

- **추정 기준**: main `826a01da0e` 기준 운영 코드 순증가. 시험·모델·문서는 뺀다.
- **상한**: 코드가 바뀐 prod 30파일, 순 코드 +1500, 정지선 +1200.
- **실측 비율**: 적용하지 못했다. S3 조각은 아직 하나도 구현·측정되지 않았다. 같은 이슈 S2의 실측은 `pr_cap_prod.py 1f61b76313 bd0c385be2` 기준 prod 30파일, 순 +769다.

| 조각 | 내용 | prod 파일(새/기존) | design-5 추정 | design-6 추정 |
|---|---|---|---|---|
| S3a-1 | 종료 기록 저장소(`spawn`·`end`), host 메서드 `execution_end`(tmux·Herdr 구현, `read_end` 통합), tmux spawn 기록, E1·E2·E3·E5. `end` 쓰기 실패면 launch 거절. **dev seam은 빼서 S3a-2로 옮김** | 2 / 12 | 240 | 390 |
| S3a-2 | 계보 허용 validator와 dev seam(같은 조각), 파일 판정(settle_saved·retry 진입만), ReadSeal, completion 반환값 gate, `refreshed` 분리, 진행 표시, durable completion latch(모든 진입점) | 4 / 10 | 260 | 410 |
| S3a-3 | 두 boot gate 구조 검증과 입력별 건너뜀, boot debt 구동(tick), E4(host 메서드), 종료 admission(저장 barrier), `end_proof`, Changed 정산 | 4 / 9 | 590 | 570 |
| S3a-4 | source scan(bounded 해독, cursor 계보), scan receipt 생산과 재확인, `end_len` cut의 scoped 분석, 역사적 raw EOF, cut 있는 결과 없는 정산, writer 인계(인계된 ticket만) | 4 / 7 | 540 | 690 |
| **합계** | 4조각 | **14 / 약 30** | **1,630** | **2,060** |

- design-5 대비 +430이다.
  - +150: host 메서드와 두 구현, spawn 기록.
  - +60: 계보 허용 validator와 호출 교체.
  - +70: durable latch와 진입점 확인.
  - +80: scan receipt 생산과 재확인.
  - +70: bounded 해독과 cursor 계보.
  - +20: writer 인계 비교.
  - −20: restore·live 진입의 gate 제거.
- 가장 큰 조각은 S3a-4의 +690이다. 모든 조각이 정지선(+1200) 아래다.

### 7.1 순서가 지키는 것

- S3a-1은 기록과 launch 거절만 한다. `end`를 읽는 쪽은 S3a-3부터다. seam이 없으므로 기존 시험(`discord/turn_finalizer/native_renumber_tests.rs:106`)은 그대로다.
- S3a-2는 seam과 계보 validator를 같이 넣는다. exact가 통과하던 입력은 계보 validator도 통과한다(계보가 비면 exact와 같음).
- S3a-2의 latch는 부작용을 줄이기만 한다. 부작용 도중 process가 죽으면 S2처럼 다시 실행한다.
- S3a-3은 writer 경로를 건드리지 않는다. 정산은 파일을 읽지 않는 건(저장 barrier, 적극적인 Changed)뿐이다.
- writer 인계와 scan은 같은 조각(S3a-4)이다. 중간 착지 상태에서도 S2 writer 출구가 살아 있다.
- S2가 main에 착지한 뒤 조각을 시작한다.

### 7.2 파일 배치 (main 기준)

1,000줄이 넘는 파일에는 호출 교체만 둔다.

| giant 파일 | 두는 것 | 본체가 가는 곳 |
|---|---|---|
| `claude.rs` | E3 호출 한 줄(`:2600-2608`) | `tui_o/store/execution_end.rs` |
| `claude/herdr_turn/handoff.rs` | E5 호출 한 줄 | `tui_o/store/execution_end.rs` |
| `discord/turn_finalizer.rs` | `NativeBoot` 처리에 `cached_shared` 인자 전달 | `discord/turn_finalizer/native_prepare.rs` |
| `tui_o/writer/input_facts/native.rs` | `mod scoped;` 한 줄 | `tui_o/writer/input_facts/native/scoped.rs` |
| `tui_input/human_frame/history.rs` | `file_id`가 `capture::file_identity`를 부르게 교체(S3a-2) | 없음 |
| `tui_input/human_frame/history/v2/native.rs` | `mod native_latch;` 한 줄과 settlement 필드 | `tui_input/human_frame/history/v2/native_latch.rs` |

| 조각 | 새 파일 | 고치는 기존 파일 |
|---|---|---|
| S3a-1 | `tui_o/store/execution_end.rs`, `session_host/execution_end.rs` | `tui_o/store/mod.rs`, `session_host.rs`, `session_host/traits.rs`, `session_host/tmux_host.rs`, `session_host/herdr_registry.rs`, `discord/herdr_terminate.rs`, `herdr_launch.rs`, `herdr_launch/owner_port.rs`, `discord/herdr_owner_port.rs`, `claude/tui_session_launch.rs`, `claude.rs`, `claude/herdr_turn/handoff.rs` |
| S3a-2 | `tui_input/human_frame/history/v2/lineage.rs`, `tui_input/human_frame/history/v2/native_latch.rs`, `discord/turn_finalizer/native_read_seal.rs`, `discord/turn_finalizer/native_source_fate.rs` | `tui_input/human_frame/history.rs`, `tui_input/human_frame/history/v2.rs`, `tui_input/human_frame/history/v2/native.rs`, `tui_input/human_frame/history/v2/native_receipt.rs`, `tui_input/human_frame/history/v2/native_barrier.rs`, `discord/turn_finalizer/native_completion.rs`, `discord/turn_finalizer/actor_state.rs`, `discord/turn_finalizer/native_restore.rs`, `discord/turn_finalizer/native_supply.rs`, `discord/turn_finalizer.rs` |
| S3a-3 | `discord/turn_finalizer/native_boot_debt.rs`, `discord/turn_finalizer/native_end_prover.rs`, `discord/turn_finalizer/native_admission.rs`, `tui_input/human_frame/history/v2/native_end.rs` | `discord/turn_finalizer.rs`, `discord/turn_finalizer/native_prepare.rs`, `discord/turn_finalizer/native_policy.rs`, `discord/turn_finalizer/native_supply.rs`, `discord/turn_finalizer/native_restore.rs`, `discord/turn_finalizer/native_abstain.rs`, `tui_input/human_frame/history/v2.rs`, `tui_input/human_frame/history/v2/native.rs`, `tui_input/human_frame/history/v2/native_barrier.rs` |
| S3a-4 | `discord/turn_finalizer/native_end_scan.rs`, `tui_o/writer/input_facts/native/scoped.rs`, `discord/turn_finalizer/native_end_release.rs`, `discord/turn_finalizer/native_end_raw.rs` | `tui_o/writer/input_facts/native.rs`, `tui_o/shadow/capture.rs`, `discord/turn_finalizer/native_boot_debt.rs`, `discord/turn_finalizer/native_supply.rs`, `discord/turn_finalizer/native_terminal.rs`, `tui_o/writer/rotation/owe.rs`(인계 비교 한 줄), `tui_input/human_frame/history/v2/native_receipt.rs` |

### 7.3 조각별 제품 RED

- fixture는 공통 dev seam으로 dev를 바꾼다(S3a-2부터).
- 각 fixture는 먼저 writer와 native history가 둘 다 새 dev를 본다는 것을 단언한다.
- helper 직접 호출의 GREEN으로 실제 진입점 시험을 대신하지 않는다.
- 비회귀 fixture는 같은 입력을 S2 동작 기준으로도 단언한다.

| 조각 | 주입 | 단언 |
|---|---|---|
| S3a-1 | §2.10의 host 시험 표(tmux 다섯, Herdr 둘, 공통 하나) | 표의 단언. 쓰기 실패면 launch 거절, spawn 기록 쓰기 실패면 launch 진행 |
| S3a-2 | seam + 계보가 있는 dev 변경 → receipt·barrier·refreshed 각각 | 계보 validator로 통과, 계보 없는 dev 변경은 exact처럼 거절. `native_renumber_tests.rs:106` GREEN 유지 |
| S3a-2 | **boot restore 진입의 저장 결과 + transcript stat 실패** | S2와 같이 경고 뒤 cleanup. 보류 0 |
| S3a-2 | **live completion + transcript 읽기 실패** | S2와 같이 경고 뒤 진행. gate 0 |
| S3a-2 | **제어 결과(`control_proof`) restore + transcript 없음** | 파일 접근 없이 cleanup |
| S3a-2 | 저장 결과 retry + 파일을 숨겼다가 같은 inode로 되돌림(부모가 남은 ENOENT와 부모까지 없는 ENOENT) | 숨긴 동안 completion·cleanup 0, 되돌린 뒤 각 1회 |
| S3a-2 | 저장 결과 retry + 다른 inode, 잘린 길이, seal 불일치 | completion 없이 cleanup |
| S3a-2 | **Settle이 terminal 기록 없음(`:317-327`)·row CAS 지연(`:338`)으로 돌아온 뒤 재진입. restore `:425` 포함** | transcript DB·selector·scheduled·work dispatch 부작용 각 1회 |
| S3a-2 | writer 새 outcome 직후 정리와 finalizer 시도가 겹침 | 부작용 각 1회 |
| S3a-3 | 저장 barrier → dev 변경 reboot, 파일 부재 | 두 boot gate 모두 진행 |
| S3a-3 | Changed source로 writer 시작 → 첫 E4 fence busy → writer halt → 해제 → 종료 판독 성공 | 원본 ticket 1회 정산, gate 열림 |
| S3a-3 | **설치 전 이미 N2, 같은 dev, N1 newline 결과, `end` 없음** | writer가 S2처럼 공급해 `Executed` 1회 |
| S3a-4 | **receipt 없는 boot ticket, closer는 `end_len` 안** | receipt가 먼저 durable하게 써진 뒤 `Executed` 1회. receipt CAS 실패를 주입하면 outcome 0 |
| S3a-4 | source A·B의 cursor 보존 → A reopen 실패 → B 정상, N1 closer 있음 → B 종료 판독 성공 | B의 `Executed` 정확히 1회, A cursor·spool 불변 |
| S3a-4 | **개행 없는 N1 closer → N2가 개행과 suffix를 붙임 → 저장 cursor가 `end_len`을 지남** | N1 `Executed` 1회, 결합 줄 해독 0, N2 행 불변 |
| S3a-4 | **refresh가 cursor 키를 옮김 / cursor가 없음** | 계보 키로 찾아 정산 / exact stat이면 정산, 계보만 있으면 대기 |
| S3a-4 | 인계된 ticket과 writer offer | writer offer 0, scan만 정산 |
| S3a-4 | 입력 출처별 결과 없는 정산(ordinary·follower·scheduled·work dispatch) | terminal `failed` 1회, Discord 게시 0 |

### 7.4 변형으로 확인할 guard

| 끄는 검사 | RED여야 하는 단언 |
|---|---|
| 진입점별 전제(restore에 Await) | restore 진입 보류 0 |
| scan의 receipt 생산 | receipt 없는 ticket의 `Executed` |
| writer 인계의 범위(현재 실행으로 좁힘) | 설치 전 N2 fixture의 `Executed` |
| tmux spawn 기록 판독(live pane 요구) | 재부팅 뒤 `end` 기록 |
| durable latch | 재진입 부작용 각 1회 |
| 계보 허용 validator(exact로 되돌림) | 계보 있는 dev 변경의 receipt |
| 해독 범위의 `end_len` 상한 | 결합 줄 해독 0 |
| 종료 기록의 target·host 비교 | nonce·host만 다른 ticket 정산 0 |
| stat 실패의 대기 | 숨겼다 되돌린 파일의 completion 1회 |

## 8. 남은 위험

- **Herdr 서버가 돌아오지 않는 경우.** Herdr N1의 종료는 pane·process 판독으로만 증명한다. 서버가 끝내 뜨지 않으면 `Unknown`이고 S2 출구만 남는다.
- **spawn 기록 없이 뜬 tmux 실행.** S3a-1 착지 전에 뜬 실행이나 spawn 기록 쓰기가 실패한 실행은 `Unknown`이다. S2 출구만 가진다.
- **tmux pid 재사용.** 같은 boot 안에서 pid가 재사용되면 pid만으로는 신원을 확정할 수 없다. 그래서 pid와 process 시작 시각을 함께 본다(`session_host/herdr/provenance.rs`의 `process_start`). 시작 시각을 읽을 수 없으면 `Unknown`이다.
- **끝내 stat되지 않는 transcript, retry 진입의 seal 없는 dev 변경.** S2와 같이 기다린다.
- **boot restore 진입의 stat 없는 읽기.** S2 동작을 그대로 둔다. stat과 읽기 사이에 파일이 사라지면 지금처럼 completion 본문을 잃을 수 있다.
- **부작용 도중 process 종료.** latch가 `Pending`으로 남아 다음 진입이 다시 실행한다(S2와 같음).
- **source scan의 읽기량.** `max(captured_through, end_len)` byte다. 같은 입력의 결과는 기억한다.
- **종료 기록 상한.** 채널당 `end` 16건, `spawn` 16건이다. 넘으면 가장 오래된 것이 밀려난다.
- **실행 재현 범위.** 이번 레그에서 실행한 것은 merge 해소 컴파일과 P1-NEW-1 실측뿐이다. 나머지 반례는 소스를 읽어 확인했다.

## 9. P2와 후속

| 리뷰 P2 | 처리 |
|---|---|
| WebGPT P2-1 역사적 raw EOF의 재투영 방법 | **반영**. 외부 P1-NEW-3(a)와 뿌리가 같다(§2.12) |
| WebGPT P2-2 진행 표시가 순차 재실행을 막지 않음 | **반영**. 외부 P1-NEW-3(b)와 뿌리가 같다(§2.13) |
| WebGPT P2-3 다른 inode가 영구 소실과 같지 않을 수 있음 | 후속: 운영에서 쓰는 transcript 교체·이동 방식(Claude·Codex 저장 규약)과 판정표를 대조한다. S3a 계약은 바꾸지 않는다 |
| design-4 리뷰 P2-4 rollback 운영 계약 | 후속: rollback 경계는 S3a-2(latch 필드)와 S3a-3(`end_proof`) 착지 전이다. 절차는 해당 PR에서 정한다 |

## 10. 범위 축소와 단계 분할 선택지

P1 추이는 3 → 2 → 5다(이번 라운드는 두 리뷰를 합쳐 뿌리 기준 7행). design-6은 모두 설계로 닫았다고 판단한다. 그래도 다음 리뷰에서 남는 P1이 절반(2건 이하)으로 줄지 않으면 더 개정하지 않고, 아래 가운데 하나로 범위를 줄인다.

| 선택지 | 먼저 착지하는 것 | 운영이 나빠지지 않는 이유 | 남는 위험 |
|---|---|---|---|
| A. 기록만 먼저 | S3a-1(종료 기록, host 메서드, spawn 기록)과 S3a-2의 latch만. 소비자는 없다 | 기록은 읽는 쪽이 없다. latch는 이미 끝난 부작용의 재실행만 막는다. writer·진입점 전제는 그대로다 | gate 개선은 아직 없다. launch마다 fsync 한두 번이 늘고, `end` 쓰기 실패면 launch를 거절한다 |
| B. 파일을 읽지 않는 정산 먼저 | A + S3a-3(E4, 저장 barrier 종료 정리, 적극적인 Changed 정산). receipt·writer 인계·scan·계보 validator는 뒤로 | 새 정산은 transcript를 읽지 않으므로 receipt·역사적 출구·B를 건드리지 않는다. writer의 S2 공급은 그대로다 | closer가 있는 boot ticket과 턴 중간에 끝난 ticket은 계속 S2 출구만 가진다 |
| C. host 단계 분할 | Herdr 경로만 먼저(`read_end`가 이미 있음). tmux 구현은 기본값 `Unknown`으로 두고 spawn 기록 검증 뒤에 올린다 | tmux 채널은 지금과 같은 S2 출구를 쓴다. Herdr는 이미 durable한 `HostedExecution`으로 판독한다 | 재부팅 뒤 tmux 채널은 개선되지 않는다 |

리뷰가 S3a 전체에서 P1을 다시 3건 이상 낸다면 B를 권한다. §1.3의 7행 가운데 B 범위에 드는 것은 5행(host 판독)과 7행(latch)뿐이다. 1행과 4행은 B가 진입점 gate와 dev seam을 넣지 않으므로 생기지 않는다. 2·3·6행(receipt, writer 인계, bounded 해독)은 B 범위 밖이다.
