# #7353 KE-T 설계: adk-engine 시험의 test-support 전환과 조각 경계 (맥미니, 설계만)

<!-- doc-path-refs: proposed src/launch/ke_auto_queue_tests.rs src/services/discord/host_key_derivation/ke_auto_queue_tests.rs src/services/discord/shared_data.rs src/services/discord/test_fixtures.rs -->

- base `f352ad6f96`: origin/main `bdf9ad2227`(KRX-T1·T4·T8, 1121i 포함)에 `refs/review/i7353-krxt3-1`(`56438ccde0`, KRX-T2 포함)을 로컬 merge한 것이다. KRX-T3가 `dispatch::test_support`·`PgWireProxy`·panic probe를 KRX로 내린 뒤 상태로 셌다.
- 정적 스캔만 했다. cargo check·build·test는 돌리지 않았고 `cargo metadata`도 쓸 필요가 없었다. 코드 변경은 0이고, 커밋은 이 문서와 조각 표 두 개뿐이다.
- 원본(레인 사본): `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i7353/logs/ket-design/`
  - 스캐너: KRX 설계 사본을 KE용으로 고친 `scan_b.py`·`classify_b.py`·`plan_b.py`. 새로 쓴 것은 `scan_a.py`·`a_merge.py`·`a_defs.py`·`a_closure.py`·`closure.py`·`fixture_users.py`·`slices_ke.py`. `modfile.py`는 KRX-T3 레인 것을 그대로 썼다.
  - 산출: `modfile-base.json`, `test-ids-base.json`, `a-sites*.json`, `a-final.json`, `a-defs.{txt,json}`, `a-closure.{txt,json}`, `b-sites.json`, `b-classified.json`, `b-plan.{json,txt}`, `fixture-users.json`, `slices.{md,json}`
- partition: 저장소 `scripts/crate_split/partition.json`과 KRX 설계가 쓴 partition-v2(`logs/path-3/partition-v2.json`)를 그룹 단위로 비교했다. 네 crate 모두 차이가 0이다(KRX 82·KE 88·DL 85·ROOT 46).
  - partition에 없는 모듈은 다섯 개다. `discord::herdr_owner_port`는 DL로 넣었다(KRX 설계와 같다). KRX-T8 host 네 개는 기본 규칙으로 정했다: `services::krx_claude_tests`는 KE(`services` 폴백), `discord::krx_claude_*`는 DL, 최상위 `krx_claude_tests`는 ROOT다.
  - KE 범위는 partition의 KE 88그룹이다. 다만 `test_support`·`test_env_panic_probe`는 루트 별칭이라 TS·PP 버킷으로 따로 셌다.

## 0. 먼저 읽을 판정

- **(A) 전수: KE 정의 78곳(41파일), 위쪽 사용 939행(296파일, ROOT 484·DL 455).** KRX 설계는 컴파일러 기반 VT2 목록을 썼지만 KE용 VT2 실행은 없다. 그래서 정적 스캐너 `scan_a.py`를 새로 썼다.
  - 경로로 닿는 정의 39곳·410행
  - `.method(`로 닿는 gate 걸린 `pub` 메서드 43곳·528행
  - 손 정정 1건(§1-1)
- **(A) 처방은 KRX와 같다.** `#[cfg(test)]`를 `#[cfg(any(test, feature = "test-support"))]`로 바꾼다. 루트 check-cfg는 KRX-T1이 이미 넣었으므로(`Cargo.toml:19-21`) KE-T1은 `Cargo.toml`을 건드리지 않는다.
  - KRX와 다른 점이 하나 있다. 정의가 같은 파일의 다른 gate 항목을 부르는 **전이 닫힘이 55곳**(19파일) 있어서, 그 gate도 함께 바꿔야 한다. 대부분 `turn_orchestrator`의 `ChannelMailboxHandle` 시험 메서드와 시험 전용 메시지 변형, 그리고 `engine_runtime`이다.
- **(B) 전수: KE 시험 영역에서 DL/ROOT로 가는 사이트 219곳, 74파일.**
  - 버킷: TS 49·PP 13·ALIAS 11·FIX 67·PROD 61·AMB 18
  - 방식: PATH 27·SINK 2·SEAM 6·RELOC 39파일
  - 설계 3이 적은 138곳은 다른 base 값이다. 같은 정규식 방식으로 이 base를 다시 재면 177곳이다(§1-2).
- **KE에는 KRX에 없던 SEAM 버킷이 있다.** KE prod의 `#[cfg(test)]` 문장이 DL/ROOT의 시험 hook이나 포트 설치를 부르는 곳이다. 시험을 옮겨도 풀리지 않는다.
  - 시험 probe 4곳: `herdr_kill.rs:40`, `herdr_kill/effects.rs:154`, `queue/http_force.rs:66`, `auto_queue/runtime.rs:251`. hook 슬롯을 KE로 옮겨 방향을 뒤집는다(KE-T3).
  - 포트 즉석 설치 3곳: `engine/ops/exec_ops.rs:36`, `dispatches/http_outbound.rs:51`, `dispatches/thread_archive_defer.rs:15`. 구현이 DL inflight·outbound에 있어서 KRX-T10 규칙 (i)(기본 구현 내리기)를 쓸 수 없다. **[U]1**
- **KE prod가 옮길 시험 모듈을 부르는 seam이 4곳 있다.** KRX-T5b barrier, T8 218행, T9 test_executor와 같은 꼴이다.
  - `routines/agent_executor.rs:869`, `agent_executor/launch.rs:12`, `agent_executor/owner_input.rs:151`은 `owner_input_tests`를 부른다.
  - `scheduled_messages.rs:413`은 `postgres_tests::START_TARGET`을 부른다.
  - 처방: hook만 KE에 남기는 분리를 조각(KE-T6)에 미리 넣었다.
- **조각은 8개다.** (A) 2개, PATH·SINK·SEAM 1개, RELOC 4개, 다른 레인 착지와 [U] 판정을 기다리는 몫 1개다. 도구 조각은 없다. 같은 binary 안 relocation 인정(KRX-T4)이 이미 base에 들어 있다.
  - **KE-T1**은 (A) 전환 첫 묶음이다. gate 파일 20개, 정의 39곳, 사용 273행, 닫힘 gate 28곳이다. prod 추정은 20~23파일이다.

## 1. KE 시험 지원 표면(두 방향 전수)

### 1-1. (A) 위쪽(DL/ROOT) 시험이 KE의 `#[cfg(test)]` 항목을 쓰는 곳

`scan_a.py`가 셌다. 범위는 scan_b와 같은 시험 영역(시험 전용 파일 전체와 cfg가 test를 요구하는 항목)을 DL/ROOT 쪽에서 본 것이다.
- 1단계(경로): `crate::`·`super::` 경로를 modfile의 가장 긴 모듈 접두로 해석한다. 그 모듈 파일이 시험 전용이면 (A)다. 아니면 다음 마디의 이름이 그 파일의 시험 gate 안에서만 선언(fn·struct·mod·`use` 잎 등)될 때 (A)다.
- 2단계(메서드): KE 비시험 파일의 gate 안에 있는 `pub` fn 중, 이름이 gate 밖과 DL/ROOT 시험 영역 어디에도 없는 것을 모았다. 그리고 위쪽 시험 영역의 `.name(`·`::name(` 토큰을 셌다. 비공개 fn은 뺐다(예: `remint_fence.rs:165` `ptr_eq`는 std `Arc::ptr_eq`와 이름만 같다).
- 정의 위치는 바꿀 gate 줄이다. 시험 전용 파일이면 가장 가까운 비시험 조상의 `mod` 선언 gate다.

| 구분 | 행 | 정의 | 비고 |
|---|---:|---:|---|
| 1단계 경로 | 410 | 39 | |
| 손 정정: `turn_orchestrator.rs:141` | −1 / +2 | 0 | scan_b의 `item_end`가 enum 변형 `#[cfg(test)] Unchecked,`의 범위를 다음 `pub(crate) use recovery_kickoff::RecoveryKickoffResult;`까지 늘려 잡았다. 진짜 (A)는 변형 `TakeHeadCondition::Unchecked` 2행이다(`discord/queue_dispatch.rs:213`, `router/intake_dispatch/queued/transition_busy_tests.rs:678`) |
| 2단계 메서드 | 528 | 43(1단계와 같은 정의 4곳 포함) | |
| **합(A)** | **939** | **78곳 / 41파일** | 사용 파일 296 |
| 빼 둔 것: glob·모듈 자체 경로 | 28 | — | `use crate::services::x::*` 꼴이다. 정적으로 판정할 수 없다. VT2로 확인한다([U]6) |
| 빼 둔 것: 같은 이름이 gate 안팎에 다 있음 | 347 | — | 고유 이름 9개를 손으로 봤다. 모두 시험 모듈 안 `use`가 prod 이름을 들여온 것이다(`InterventionMode`·`ChannelMailboxRegistry` 등). `cfg(not(test))` 쌍둥이 정의는 0이다 |

정의 종류는 KRX와 같은 셋이다.
- **seam·시험 메서드**: prod 타입의 gate 걸린 메서드와 hook이다. 예: `ChannelMailboxHandle::{replace_queue, take_next_soft, v2_adopt, v2_admit_ticket}`, `queue::set_pre_purge_probe_for_test`, `engine_runtime` lease 시험 메서드. 사용 행의 대부분이 여기다(`v2_adopt` 132, `replace_queue` 105, `take_next_soft` 73).
- **지원 모듈**(`#[test]` 없음): `turn_orchestrator::test_support`(100행·36파일), `dispatch::test_support`(140행·48파일, KRX-T3가 남긴 KRX `db::dispatch_test_db` 재노출 facade), `policy_close::test_hooks`(25행), `inject_disposition::test_support`, `health_diagnostics::backlog_probe`.
- **섞인 시험 모듈**(`#[test]`가 함께 있음) 6곳:
  - `scheduled_messages/turn_completion.rs:156` `tests` 3시험·28행
  - `campaign_execution/control.rs:971` `tests` 16시험
  - `services/mod.rs:113` `recovery_defer` 9시험
  - `services/mod.rs:186` KRX-T8 host `krx_claude_tests` 50시험
  - `auto_queue/runtime.rs:616` `slot_reset_host_pg_tests`(B와 겹침)
  - `dispatch/mod.rs:10`(facade + 시험 1)

gate 안 위쪽 참조는 3곳이다(`a-defs.txt`).
- `dispatch/mod.rs:10`·`services/mod.rs:186`은 `crate::test_support`(TS)뿐이다. M-KE가 adk-engine lib root에 KRX와 같은 별칭을 두면 풀린다.
- `auto_queue/runtime.rs:616`은 FIX·PROD 20곳이라 B 쪽 RELOC로 푼다(KE-T4).

| (A) 처리 | 정의 | 사용 행 | gate 파일 | 닫힘 gate | 조각 |
|---|---:|---:|---:|---:|---|
| feature 전환 ①: `services/` 밖 + `services/a~r` | 39 | 273 | 20 | 28 | KE-T1 |
| feature 전환 ②: `services/s~z`(`scheduled_messages`·`session_resume`·`turn_cancel_queue_guard`·`turn_orchestrator` 가족 14) | 33 | 629 | 17 | 26 | KE-T2 |
| B와 겹침: `auto_queue/runtime.rs:616` `slot_reset_host_pg_tests` | 1 | 1 | 1 | 0 | KE-T4 |
| 미룸: `policy_close.rs:486`(#8343), `imsg_reply/agy_input.rs:30,47`(#8339 확인 [U]5), `services/mod.rs:113,186`(KRX-T5·T5b host 줄과 같은 파일, `:186`은 [U]3) | 5 | 36 | 3 | 1 | KE-T8 |
| 합 | 78 | 939 | 41 | 55 | |

### 1-2. (B) KE 안 시험이 KE 밖(DL/ROOT) 항목을 쓰는 곳

`scan_b.py`(KE용, 빌드 없음)로 셌다. 아래 = KRX·KE, 위 = DL·ROOT다. `pub(in crate::services::discord)` 가시성 경로는 VIS 몫이라 뺐다.

| 버킷 | 사이트 | 처리 |
|---|---:|---|
| TS: `crate::test_support::*`(루트 별칭 `src/lib.rs:299-300` `use adk_test_support::test_support;`) | 49 | M-KE lib root에 같은 별칭 1줄. 사이트 수정 0(KRX-T3 판정과 같다) |
| PP: `crate::test_env_panic_probe::*`(루트 별칭 `src/lib.rs:297-298` → KRX `config::test_env::panic_probe`) | 13 | 4파일 경로를 `crate::config::test_env::panic_probe::`로 바꾼다(KE-T3). KE root에 위쪽 별칭을 다시 만들지 않는다 |
| ALIAS: KRX·KE 항목을 DL/ROOT 재노출 경로로 부름 | 11 | 경로 교체(PATH 파일 3개는 KE-T3, RELOC 파일 안의 것은 그 조각) |
| FIX: 위쪽 시험 gate 걸린 fixture | 67 | `write_bindings_for_test` 2곳만 SINK. 나머지는 옮기기 |
| PROD: 위쪽 prod API | 61 | 옮기기(SEAM 7곳은 별도) |
| AMB: 이름이 위·아래 양쪽에 있음 | 18 | 옮기기. 조각에서 재판정한다. `discord::health::{reserve_headless_agent_turn, rebind_channel_provider_session, snapshot_pending_queue_state}`는 KE `engine_runtime::health_port`에 같은 이름의 포트가 있어 PATH로 풀릴 수 있다 |
| 합 | **219** | 74파일. 매크로 0 |

- 대상 crate(사이트): DL 85, ROOT 72, 특수(TS·PP) 62
- 종류: 시험 전용 112곳·41파일, 인라인 107곳·33파일
- 파일 단위 방식(`plan_b.py`, 처음 맞는 것): SEAM 6, RELOC 39, SINK 2, PATH 27
  - RELOC 대상 crate: ROOT 31, DL 8. 순서 KE < DL < ROOT에서 그 파일이 부르는 가장 위 crate다.
  - RELOC 시험 전용 26파일·230시험. `super::` 90곳, `use super::*` 17파일
  - RELOC 인라인 13파일: 사이트가 든 시험 함수 19, 함수 밖 사이트 8(모듈 수준 `use`·helper), 그 모듈들의 시험 합 112. 옮길 함수는 19~112다(KRX-T5 교훈: 모듈 수준 `use`가 있으면 모듈 전체가 기댄다).
- 손 판정 정정:
  - **ROOTED 2파일**: `src/high_risk_recovery.rs`(7시험)와 그 자식 `auto_queue/dispatched_entry_recovery_pg_tests.rs`(6시험)는 시험 전용 KE 그룹 파일이다. 그런데 선언은 루트 `src/lib.rs:252-253`에 있다. M에서 루트 crate에 남으므로 이미 ROOT 시험이다. 코드 변경 없이 partition에서 `high_risk_recovery`를 ROOT로 옮긴다(M 메모). 실제 RELOC는 37파일이다.
  - 포트 2파일(`http_outbound.rs`·`thread_archive_defer.rs`)은 SEAM(port)로 KE-T8에 둔다.
- **설계 3과 다른 이유.** 설계 3 방식(시험 전용 파일 전체 + 첫 `#[cfg(test)]` 뒤의 `crate::` 정규식)으로 이 base를 재면 177곳이다(인라인 106 + 전용 71). 설계 3의 138은 그보다 앞 base(KE-C·KRX-T8 착지 전) 값이다. 219와의 차이는 KRX 설계 §1-2와 같은 넷이다: use 트리 펼치기, 모르는 discord 키를 버리지 않기, TS·PP 포함, 인라인 범위를 gate 항목으로 좁히기.

### 1-3. 스캐너 밖에서 조각이 함께 져야 하는 것

`fixture_users.py`로 셌다. RELOC 시험 전용 모듈을 그 모듈 밖에서 부르는 곳과, 선언 부모가 그 모듈을 부르는 곳이다. 인라인 시험 모듈 이름도 같은 방식으로 찾았다.

| 꼴 | 위치 | 처방 | 조각 |
|---|---|---|---|
| 공유 fixture 연쇄 | `routines/session_control.rs:757` → `verified_reset_pg_tests::Fixture` | Fixture가 위쪽(ROOT)을 부르므로 그 KE 인라인 시험 함수도 함께 옮긴다(justification 행) | KE-T4 |
| 공유 fixture 연쇄(DL 사용처) | `discord/turn_bridge/tmux_runtime/stop_host_tests.rs:1112` → `slot_reset_host_pg_tests::runtime_clears_after_done` | helper는 KE `RUNTIME_CLEARS_DONE`만 읽는다. 그래서 KE `auto_queue/runtime.rs`에 `any(test, feature)` helper로 남기고, DL 사용처 1줄만 바꾼다. 통째로 ROOT로 보내면 DL 시험이 ROOT를 부르게 된다 | KE-T4 |
| 공유 fixture 연쇄(KRX-T8 host) | `claude/herdr_turn/first_input/clear_held_tests.rs`·`claude/herdr_turn/first_input/unproven_tests.rs`(DL host) → `services::krx_claude_tests::claude_herdr_first_input::{NONCE, bound_target}` | host gate(`services/mod.rs:186`, 16파일)를 통째로 바꾸지 않는다. 두 항목만 KE 지원 위치로 뗀다 | KE-T8, [U]3 |
| KE prod → 옮길 시험 모듈 | `agent_executor.rs:869`·`launch.rs:12`·`owner_input.rs:151` → `owner_input_tests::{LEGACY_TEARDOWNS, inject, OWNER}` | KRX-T9 test_executor처럼 hook·task_local은 KE의 같은 모듈 경로에 남기고 시험 본문만 옮긴다 | KE-T6 |
| KE prod → 옮길 시험 모듈 | `scheduled_messages.rs:413` → `postgres_tests::START_TARGET` | 같은 분리 | KE-T6 |
| RELOC가 새 (A) 행을 만듦 | `engine/ops/timeouts_ops/host_repair_tests.rs`(→DL)가 `stale_turn_reconciler::owner_boundary_tests`(PATH, KE에 남음)를 씀 | `stale_turn_reconciler.rs`의 그 선언 gate를 `any(test, feature)`로 바꾼다. KE-T5 파일이라 T5 뒤 | KE-T7 |
| KRX 시험이 KE 시험 모듈을 씀 | KRX `observability/events/capture_stress_tests.rs:10` → `turn_lifecycle::policy_observability_tests`(KRX-T7 행) | KRX-T7이 먼저 옮긴 뒤 KE-T5가 `turn_lifecycle.rs` 시험을 옮길 때 그 사용처 위치를 다시 본다 | KE-T5(선행 KRX-T7) |
| 위쪽 타입을 담는 static | `auto_queue/runtime.rs:600` `RUNTIME_CLEARS: Mutex<Vec<(u64, Option<ManagedReset>)>>` | KE prod `verdict.apply()`(`:281`)가 이미 ROOT `admin_host_guard::ManagedReset`을 받는다. 이것은 시험이 아니라 prod 간선이라 KE-C 몫과 묶어 판정한다 | KE-T8, [U]4 |

## 2. 방식 비교와 선택

### 2-1. (A) feature 전환

KRX 설계 §2-1의 A1(`cfg(any(test, feature = "test-support"))` + M에서 dev-dependency로 켜기)을 그대로 쓴다. A2(정의를 adk-test-support로 옮기기)와 A3(사용처를 내리기)를 기각하는 이유도 같다. seam과 시험 메서드는 KE prod 타입에 붙어 있어서 옮길 수 없다.

KE에서 덧붙일 것:
- **M 전 상태.** 루트 `Cargo.toml:19-21`의 `[lints.rust] unexpected_cfgs … check-cfg = ['cfg(feature, values("test-support"))']`가 KRX-T1에서 이미 들어갔다. 그래서 KE-T1·T2는 Cargo 줄이 0이다. 루트는 feature를 선언하지 않으므로 M 전에는 `any(test, feature)` ≡ `test`이고, 동작 변화도 0이다.
- **전이 닫힘.** feature만 켜고 test를 끈 빌드에서는, 바꾼 항목이 부르는 다른 `#[cfg(test)]` 항목이 없으면 컴파일 오류가 난다.
  - 같은 파일 닫힘: 55곳(`a_closure.py`, 표는 `slices.md`). 큰 것은 `engine_runtime/mod.rs` +17(lease 시험 타입 `LeaseToken`·`Acquired`·`Committed` 등), `turn_orchestrator.rs` +6(`HardStop`·`ReplaceQueue`·`AgeActiveTurnForTest`·`AgeInboundWaitsForTest` 메시지 변형, `take_soft_matching`)이다.
  - 파일 넘는 닫힘: 위 변형의 match 팔(`queued_head/v2/control.rs:63-66,130`, `registry_purge.rs:124-125,177,187`). 둘 다 KE-T2 파일이다.
  - 그 밖의 파일 넘는 닫힘은 정적으로 다 세지 못했다. 조각 관문 (5) 뮤테이션 check가 잡는다(§6-2).
- **섞인 시험 모듈.** KRX와 같이 시험 함수만 쓰던 import·helper에 `#[cfg(test)]`를 붙인다. KE-T1에서는 `control.rs:971`(16시험)과 `dispatch/mod.rs:10` facade의 `mod tests`가 대상이다.
- **feature 사슬(M 메모).** KE gate 안에서 KRX의 `any(test, feature)` 항목을 부르는 곳이 있다. 예: `dispatch/test_support.rs` → KRX `db::dispatch_test_db`, `health_diagnostics.rs:162` → KRX `db::pg_wire_proxy`. 그래서 M-KE는 `adk-engine` `[features] test-support = ["dep:adk-test-support", "adk-runtime/test-support"]`로 사슬을 건다. 루트 dev-dep는 `adk-engine = { …, features = ["test-support"] }`다.
- **feature 누수 가드.** KRX 설계가 제안한 `cargo tree -p agentdesk -e normal,build -f '{p} {f}' | grep -c test-support` = 0을 M-KE 수용 기준에도 넣는다.
- **CI 시험 목록.** 시험을 옮기지 않으므로 이름이 그대로다. M에서 binary-id만 바뀌고 relocation이 자동 인정된다.

### 2-2. (B) 버킷 처방

| 후보 | 흐름 | 판정 |
|---|---|---|
| **B1. 시험 모듈 선언을 위쪽 host로 옮기기(`#[path]`, 파일 제자리)** | KE 부모의 `mod x;`를 지운다. 위쪽 host 시험 전용 파일에 `#[path]` 선언을 둔다. `super::`는 `crate::<KE 경로>::`로 바꾼다. 같은 binary 안 이동은 KRX-T4 도구가 relocation으로 인정한다 | **채택**(RELOC 37파일) |
| **B2. 경로 교체·fixture 내리기** | PP 13(4파일)·ALIAS 3파일 경로 교체. `write_bindings_for_test`(`discord/agentdesk_config.rs:600-608`)는 본문이 KRX `config`만 쓴다(`closure.py`). 그래서 KRX `config/test_env.rs`로 내리고 DL 자리는 `pub(crate) use` facade로 둔다. KRX 사용처 `tui_prompt_dedupe/runtime_binding/codex_verified_tests.rs`(KRX-T9 FILES 행)도 KRX 경로로 풀린다 | **채택**(KE-T3) |
| **B3. SEAM 뒤집기** | KE prod가 DL hook을 부르는 곳은 hook 슬롯(thread_local·`run_*`)을 KE로 옮긴다. DL의 설치 API(`herdr_terminate::probe_before_{selection,released}`, `admin_host_guard::before_verified_clear_recheck_for_tests`)는 KE로 위임하거나 재노출해서 DL·ROOT 시험 코드를 바꾸지 않는다 | **채택**(probe 4, KE-T3) |
| B4. 포트 즉석 설치 3곳 | KRX-T10 (i) 규칙으로는 닫힘이 DL이라 못 쓴다 | **[U]1** |
| B5. M 때 upper 통합 시험으로 | KRX 설계와 같은 이유(private 넓히기 폭발)로 기각 | 기각 |

- SINK 후보 중 KE/KRX 안에서 닫히지 않는 것(그래서 RELOC):
  - `host_teardown_gate/test_support.rs`: DL `inflight`·`tmux`·ROOT `health`를 부른다.
  - `session_locator_pg_tests.rs`·`host_defer_gate_tests.rs`: DL·ROOT를 부른다.
  - `server::run_five_min_policy_tiers_for_test`(`src/server/mod.rs:686-697`): ROOT 비공개 `fire_tick_hook_by_name_with_pg`를 부른다.
  - `health::register_bot_auth_for_tests`: DL `make_shared_data_for_tests`를 부른다.
- **host 위치.** KRX 교훈에 따라 `lib.rs`(KRX-T5·T5b·T9 host 줄)와 `discord/mod.rs`(DL-MV1)를 피한다.
  - ROOT host: `src/launch.rs`(90줄, `launch` 모듈) 아래 시험 전용 child. 예: `src/launch/ke_auto_queue_tests.rs`(제안)
  - DL host: `pub(crate) mod host_key_derivation`(`discord/mod.rs:34`, 191줄) 아래 child. 예: `src/services/discord/host_key_derivation/ke_auto_queue_tests.rs`(제안)
  - 둘 다 최상위 모듈이 아니다. 그래서 KRX 때 생긴 `generate_inventory_docs.py`·`ARCHITECTURE.md` FILES_AUTO가 필요 없을 것으로 본다(조각에서 `regen_after_merge.py --check`로 확인한다).
  - DL host는 KRX-T8처럼 cross_os_rust 경로 필터에 들어가야 하는지 조각에서 확인한다.
  - 원래 사슬이 `cfg(all(test, unix))`인 파일은 같은 사슬의 host를 하나 더 둔다(KRX-T8 [설계 이탈]).
- **인라인 RELOC와 justification.** 인라인 시험 함수는 선언 파일이 바뀌어 ID가 바뀐다. 사전 표 행이 필요하고, 이 목록은 FILES 정지 대상이다.
  - 조각별 하한~상한: KE-T4 7~31, KE-T5 17~51, KE-T6 8, KE-T7 2. 합 하한 34, 상한 92(+ session_control 연쇄). **[U]2**
  - 새 파일에 상한 없는 대기가 들어가면 `scripts/ci/test_wait_allowlist.txt`가 막는다("rows only leave"). KRX-T3 PgWireProxy, T9 binding_context가 그 예다. 그래서 인라인 이동분은 옮길 함수 본문의 대기를 먼저 본다.
  - 시험 전용 파일 중 allowlist 행이 있는 것은 `hang_forensics/tests/mod.rs`, `scheduled_messages/owner_input_tests.rs`, `turn_bridge/tmux_runtime/stop_host_tests.rs`다. 셋 다 제자리에 남으므로 영향이 없다.
- **pg15·nested libtest.** RELOC·SEAM 대상 모듈에 `scripts/ci/pg15_required_ids.txt` ID가 0, `scripts/ci/nested_libtest_declarations.json` 항목이 0이다. 그래서 KRX-T5의 하드닝 고정값 사슬(pg15 명령 문자열·`job_sha256`·`ci-pr.yml` pin) 변경은 예상하지 않는다.

### 2-3. M 전에 할 수 있는 것과 M-KE 안에서만 되는 것

| M 전(조각) | M-KE 안에서만 |
|---|---|
| gate 전환 72곳 + 닫힘 54곳(KE-T1·T2), 미룬 5곳(KE-T8) | `adk-engine` `[features] test-support = ["dep:adk-test-support", "adk-runtime/test-support"]`, 루트 dev-dep feature |
| PP 경로 교체·ALIAS·SINK·SEAM 뒤집기(KE-T3) | adk-engine lib root `#[cfg(any(test, feature = "test-support"))] use adk_test_support::test_support;`(TS 49곳 수정 0) |
| RELOC 37파일 + 연쇄·seam 분리(KE-T4~T7) | partition: `high_risk_recovery`를 KE에서 ROOT로(ROOTED 2파일), `test_support`·`test_env_panic_probe`는 별칭이라 crate 소속 없음 |
| 포트 3·`RUNTIME_CLEARS` 판정 뒤 구현(KE-T8) | binary-id 변경(자동 relocation 확인), feature 누수 가드, KE용 VT2 real 모드 잔여 0 |

## 3. 조각 표

공통:
- 크기 상한은 브리프 공통 기준이다. 코드가 바뀐 prod 30파일 / 순 +1500(PLAN_READY 정지선 +1200), 실측은 `pr_cap_prod.py <base> HEAD`로만 한다. 조각 운영 상한은 KRX-T 실측에 맞춰 prod 26 안팎으로 둔다.
- "prod 추정 기본"은 선언 부모, 인라인 파일, host 부모 줄, gate 파일, seam 파일을 센 것이다. 시험 전용 파일(`*_tests.rs`, `tests/`)은 prod에 들지 않는다.
- **실측 보정**(KRX-T의 추정 대비 실측, prod 파일 수):

| 조각 | 추정 | 실측 | 비율 | 출처 |
|---|---|---|---:|---|
| KRX-T1((A)) | 24 | 20 | 0.83 | `pr_cap_prod.py a6e7b3bfba^ 736e650559` |
| KRX-T2((A)) | 21~23 | 25 | 1.09~1.19 | `pr_cap_prod.py 6789dd67f2^ e0bf180b64` |
| KRX-T3(PATH·SINK) | 18~22 | 8 | 0.36~0.44 | report-krxt3-1(fixture 본문이 시험 전용 파일로 가 prod에서 빠짐) |
| KRX-T5(RELOC) | 18+3 | 28(첫), 26(3행 되돌림 뒤) | 1.33 | report-krxt5-1(넓히기 9파일, CI 고정값 파일도 prod로 셈) |
| KRX-T5b(RELOC) | 15~20 | 12 | 0.60~0.80 | report-krxt5b-1 |
| KRX-T8(RELOC) | 15+3 | 24(재측정 25) | 1.33~1.39 | report-krxt8-1(넓히기 12파일·73줄, 예상 0~5) |
| KRX-T9(RELOC) | 11+3 | 13 | 0.93 | report-krxt9-1 |

  - 반영: (A)는 기본 × 1.15를 위 끝으로 잡는다. PATH·SEAM은 한 줄 교체 위주라 × 1.0이다. RELOC는 기본 × 1.35에 넓히기 +0~5를 더한다(T5·T8 기준). 26을 넘으면 그 조각 뒤쪽 행부터 남은 범위로 돌린다.
- **겹침(진행 중 레인 회피):**
  - 맥 KRX-T6: `discord/outbound`·`tui_o` writer·store. KE 행과 파일 겹침 0
  - WSL KRX-T7: config·logging·settings 등. SINK 대상 KRX `config/test_env.rs`는 T7 행 파일이 아니지만 같은 디렉터리다. `capture_stress_tests.rs` 순서 제약(§1-3)
  - 미니 KRX-T10: discord leaf·포트 자동 설치, `discord/health.rs`·`router/mod.rs`·`platform/tmux/session_inventory.rs` 등. KE 행 겹침 0. KE 포트 3곳은 T10과 같은 [U] 판정 계열
  - DL-MV1: `discord/mod.rs`·`shared_data.rs`·`test_fixtures.rs` 등. `make_shared_data_for_tests`·`register_resume_watcher_for_tests` 등을 옮기지만 `discord::` 재노출을 남기므로 KE 사용처 경로는 그대로 resolve된다. DL host는 `discord/mod.rs`에 두지 않는다
  - #8343 입력 경로: `history/v2/native_control.rs`·`owner_input/native_input.rs`·`input_owner/effect.rs`·`policy_close.rs`·history v2 Registry. `policy_close.rs:486`은 KE-T8로 미뤘다
  - #8339 agy_acp: base에 해당 파일이 없다. 이름이 겹치는 KE 파일 `imsg_reply/agy_input.rs`만 KE-T8로 미뤘다([U]5)
  - base에 없는 KRX-T5·T5b·T9: `services/mod.rs`(host 줄)·`services/agents/query.rs`·`tmux_turn_liveness/tests_pg.rs`·`engine/ops/phase_gate_owner_tests.rs`·`auto_queue/activate_command.rs`(KE 행은 TS 1, 수정 0)·`lib.rs`·`discord/session_canonical_identity.rs`. `services/mod.rs` (A) 2곳은 KE-T8로 미뤘다

| 조각 | 범위 | 대상(전수는 `slices.md`) | 건수 | prod 추정(+넓히기) | 선행 | 겹침·순서 |
|---|---|---|---|---|---|---|
| **KE-T1** | (A) gate 전환 ①: `services/` 밖 + `services/a~r`, 닫힘 gate, 섞인 모듈 import 정리 | gate 파일 20 | 정의 39 / 사용 273 / 닫힘 28 | 20 × 0.83~1.15 → 17~23 | 없음(check-cfg는 base에 있음) | `queue.rs`·`busy_followup_retry_store.rs`·`session_control.rs`·`reconcile.rs`는 뒤 RELOC 조각과 다른 hunk라 T1을 먼저 한다 |
| KE-T2 | (A) gate 전환 ②: `scheduled_messages/turn_completion`·`session_resume/catalog`·`turn_cancel_queue_guard`·`turn_orchestrator` + 자식 13 | gate 파일 17 + 파일 넘는 match 팔(같은 17파일 안) | 정의 33 / 사용 629 / 닫힘 26 | 17 → 17~22(× 1.15 → 25) | KE-T1(같은 레인, 형식 공유) | `policy_close.rs` 뺌(#8343). `turn_orchestrator.rs`(7063줄)는 토큰만 바꾼다 |
| KE-T3 | PATH: PP 4파일·ALIAS 3파일(TS만 있는 20파일은 수정 0) + SINK `write_bindings_for_test` → KRX + SEAM probe 4 뒤집기 | 인라인 18·전용 15(수정은 인라인 9·전용 2) + DL 3(`herdr_terminate.rs`·`admin_host_guard.rs`·`agentdesk_config.rs`) + KRX `config/test_env.rs` | 사이트 68 / 시험 413(파일 전체, ID 변경 0) | 15~17 | KE-T1(새 KE hook gate 형식) | `turn_orchestrator.rs:6945` PP는 T2와 다른 hunk |
| KE-T4 | RELOC auto_queue·dispatched_sessions·dispatches·queue·session_activity·agent_channels·maintenance·hang_forensics + `session_control.rs` 연쇄 + DL 사용처 1줄 | 전용 7 + 인라인 5 + 덧붙임 2 | 사이트 44 / 시험 105, 인라인 함수 7~31 | 기본 13 × 1.35 → 18(+0~5) | KE-T1 | 넘으면 `hang_forensics/tests/mod.rs`(31시험) 행을 뒤로 |
| KE-T5 | RELOC turn_lifecycle·session_resume·stale_turn_reconciler·queued_head(mend·v2 origin·s2a·tests)·busy_followup_retry_store | 전용 5 + 인라인 4 | 사이트 57 / 시험 112, 인라인 함수 12~51 | 기본 10 × 1.35 → 14(+3~8, `queued_head_s2a_tests.rs`의 `super::` 14곳) | KE-T1·T2(같은 파일 gate 먼저), KRX-T7(`capture_stress_tests`) | — |
| KE-T6 | RELOC routines(agent_executor 가족·execution_mode·reliability)·scheduled_messages(postgres·owner_input) + KE prod seam 분리 4 | 전용 8 + 인라인 2 + seam 파일 4 | 사이트 15 / 시험 69, 인라인 함수 8 | 기본 9 × 1.35 → 12(+0~5) | KE-T1 | seam 분리 새 파일은 `*_tests.rs` 이름(KRX-T5b 선례), 대기 없는 hook만 |
| KE-T7 | RELOC engine(exec_ops·circuit_evidence·host_repair·unsent_notify)·github triage·reconcile + `owner_boundary_tests` 새 (A) gate | 전용 4 + 인라인 2 + 덧붙임 1 | 사이트 21 / 시험 43, 인라인 함수 2 | 기본 8 × 1.35 → 11(+0~3) | KE-T5(`stale_turn_reconciler.rs`) | `exec_ops.rs:36` 포트 줄은 건드리지 않는다(KE-T8) |
| KE-T8 | 미룬 몫: (A) 5(`policy_close`·`agy_input` 2·`services/mod.rs` 2), 포트 3, `RUNTIME_CLEARS`, KRX-T8 host fixture 분리, ROOTED 2(partition, M) | (A) 파일 3 + 포트 KE 3 + DL 구현 | (A) 36행 / B 14사이트 | [U] 판정 뒤 산정 | #8343 착지, KRX-T5·T5b 착지, [U]1·3·4·5 | 포트에 C 줄이 섞이면 pro 리뷰 |

- 합계(B): 74파일 = KE-T3 33 + T4 12 + T5 9 + T6 10 + T7 6 + T8 4. 덧붙인 행 9(T4 2·T6 4·T7 1·T8 2)는 스캐너 밖이다.
- 합계(A): 78 = T1 39 + T2 33 + T4 1 + T8 5.
- 순서·나란히:
  - T1 → T2 → T3는 같은 레인에서 차례로 한다(gate 형식 공유, T2·T3가 `turn_orchestrator.rs`를 같이 만진다).
  - T4·T6은 T1 뒤에 서로 파일이 겹치지 않아 나란히 할 수 있다. T5는 T2와 KRX-T7 뒤, T7은 T5 뒤다.
  - T8은 다른 레인 착지와 [U] 판정 뒤에 한다.
  - host 부모 파일(`src/launch.rs`·`discord/host_key_derivation.rs`)은 RELOC 조각마다 줄을 더한다. 나란히 하면 hunk가 붙으므로 줄 위치를 조각마다 떼어 둔다(T4 위, T6 아래 등).

**조각마다 검증 명령(공통, KRX 설계 §3을 그대로 쓴다).**
1. `cargo fmt --all --check`
2. `cargo check --workspace --all-targets`: 경고 수를 base와 대조한다. (A) 조각은 `unexpected_cfgs` 0을 따로 센다.
3. Windows: `cargo check --lib --tests --target x86_64-pc-windows-gnu`
4. `python3 scripts/crate_split/equiv_check.py --base <base> --head HEAD`. C 줄은 cfg 토큰, `mod`/`#[path]` 선언, 경로 교체, `pub(crate)` 넓히기, seam 분리뿐이어야 한다.
5. (A) 조각: 커밋하지 않는 뮤테이션. 루트에 `[features] test-support = []`를 잠시 넣고 `cargo check --lib --features test-support`를 돌린다. 바꾼 범위 오류 0, `unused_imports` 0이어야 한다. 원복한다.
6. 시험 개수 보존: `python3 scripts/ci/rust_test_sources.py inventory`의 시험 수가 base와 같아야 한다(이 base의 `agentdesk` lib 19,313).
7. RELOC 조각: `rust_test_sources.py removed`. removed − relocated가 그 조각의 justification 행과 같아야 한다.
8. 정적 닫힘: 조각 base에서 `scan_b.py`·`classify_b.py`(KE용)를 다시 돌려 그 조각 파일의 FIX/PROD/AMB가 0인지 본다. `scan_a.py`로 새 (A) 행이 바꾼 gate 안에 있는지 본다.
9. 대상 시험: 옮기거나 바꾼 파일의 시험을 `cargo test --lib <모듈 필터>`로, PG 시험은 `pg_throwaway.sh`로 돌린다(WSL 위임).

## 4. KE-T1 구현 브리프 초안

- **제목:** `#7353 KE-T1 — (A) adk-engine 시험 정의 gate 전환 ①(services 밖 + services/a~r) (check 관문, 시험 WSL 위임)`
- **범위:** base(착수 시점 묶음 base)에서 아래 gate를 `#[cfg(any(test, feature = "test-support"))]`로 바꾼다. 원래가 `#[cfg(all(test, unix))]`면 `#[cfg(all(any(test, feature = "test-support"), unix))]`다. 같은 파일 닫힘 gate(괄호 안)도 함께 바꾼다. 줄은 base `f352ad6f96` 기준이고, 착수 base에서 `scan_a.py`·`a_closure.py`로 다시 확인한다.
  - `src/dispatch/mod.rs:10`(+`:21` `emit_probe` 재노출 확인)
  - `src/engine/mod.rs:1049`
  - `src/kanban/transition_core.rs:29`(+`:24`)
  - `src/reconcile.rs:238,254`
  - `src/supervisor/orphan_recovery/mod.rs:9`
  - `src/services/campaign_execution/control.rs:971`(섞인 모듈 16시험)
  - `src/services/discord/busy_followup_retry_store.rs:66,73,79,387,392,457`(+`:57`)
  - `src/services/discord/inject_disposition.rs:469`
  - `src/services/discord/inject_disposition/dispatched.rs:131`(+`:118,120`)
  - `src/services/discord/recovery_known_ids.rs:176`
  - `src/services/dispatched_sessions/hosted_cleanup/thread_gc.rs:38`
  - `src/services/dispatches/discord_delivery/mod.rs:9`
  - `src/services/engine_runtime/mod.rs:72,100,241,738,918,953`(+17: `:92,95,745,752,769,771,775,778,789,791,793,796,800,804,808,813,1127`)
  - `src/services/engine_runtime/relay_state.rs:105,579,806,964`(+`:10,750,753,769,781`)
  - `src/services/hang_forensics.rs:525`
  - `src/services/health_diagnostics.rs:42,162`(+`:166`)
  - `src/services/imsg_reply/mod.rs:118`
  - `src/services/queue.rs:24,27,29`
  - `src/services/routines/session_control.rs:3,5`
  - `src/services/routines/store.rs:11,2102`
- **방식:**
  - 재노출 사슬이 있으면 정의와 재노출을 둘 다 바꾼다. 예: `queue.rs:24-30` → `queue/cancel_effects.rs`, `session_control.rs:3-6` → `persistent_target`.
  - 섞인 모듈(`control.rs:971`, `dispatch/test_support.rs`의 `mod tests`)은 시험 함수만 쓰는 import·helper에 `#[cfg(test)]`를 붙인다.
  - `Cargo.toml`은 바꾸지 않는다. check-cfg가 `Cargo.toml:19-21`에 이미 있다.
  - giant 파일(`routines/store.rs` 4258줄, `reconcile.rs` 1892줄, `engine/mod.rs` 1209줄, `engine_runtime/mod.rs` 1160줄)은 attribute 토큰만 바꾸고 새 책임은 얹지 않는다.
- **관문(수용):**
  1. 루트에서 feature를 켤 수 없으므로 기본 빌드 동작 변화가 0이다. gate 전후 손 대조표를 낸다.
  2. `cargo check --workspace --all-targets` 경고 수가 base와 같고 `unexpected_cfgs`가 0이다.
  3. Windows check rc=0이다.
  4. 시험 수 19,313 그대로, 이름 변경 0.
  5. 뮤테이션(§3 공통 5)에서 바꾼 범위의 오류 0, `unused_imports` 0. dead_code는 기록만 한다. 파일 넘는 닫힘이 나오면 그 gate를 같은 조각에서 바꾸고 표에 덧붙인다.
  6. 바꾼 gate 안 위쪽 그룹 참조 0. 예외는 `crate::test_support`(TS, M 별칭)뿐이다.
  7. 다른 레인 소유 파일 변경 0(§3 겹침 목록).
- **검증:** §3 공통 1~6, 8. 대상 시험은 바꾼 파일의 시험 모듈을 필터로 돌린다(동작 변화 0 확인용, WSL): `services::engine_runtime`, `services::queue`, `services::discord::busy_followup_retry_store`, `services::campaign_execution::control`, `services::routines::session_control`.
- **크기:** 추정 prod 20파일(+파일 넘는 닫힘 0~3) / 순증 +0~20. 실측은 `pr_cap_prod.py <base> HEAD`로 한다. 26을 넘으면 `services/engine_runtime/*` 2파일(닫힘 22)을 KE-T2로 넘긴다.
- **리뷰:** cfg 토큰뿐이라 묶음 리뷰 대상이다. 닫힘 판정이 처음이므로 첫 착지는 pro를 권한다.
- **산출:** 보고서 첫 줄 `REVIEW_READY head=<sha> ref=refs/review/i7353-ket1-1 base=<base> gates=<n>(+closure <n>) rows=273 warn=<base>→<head> tests=19313→19313 equiv=<…>+수동 win=<0|n> mut=<오류 0|n> cap=<파일>/<+순증> static=<결과>`. 손 대조표(gate 전후), 뮤테이션 출력 요약, 파일 넘는 닫힘 덧붙임 표를 붙인다.

## 5. 다음 단계

1. 코디가 [U]1~6을 판정한다. KE-T1~T3은 [U]와 무관해서 바로 띄울 수 있다.
2. KE-T1 → T2 → T3를 한 레인에서 차례로 한다.
3. KE-T4·T6을 나란히 하고, T5(KRX-T7 착지 뒤) → T7 순서로 간다.
4. KE-T8은 #8343·KRX-T5/T5b 착지와 [U]1·3·4·5 판정 뒤에 한다.
5. 묶음 끝(M-KE 직전)에 KE용 VT2 shadow를 한 번 돌려 정적 (A)와 대조한다([U]6). test_cfg 잔여가 전환된 정의의 부분집합인지 확인한다.
6. DL-T 설계는 이 스캐너 세트(`scan_a.py`의 아래 = KRX·KE·DL, 위 = ROOT 변형)로 같은 형식을 만들 수 있다. DL-MV1 착지 뒤 base에서 센다.

## 6. 남는 위험·[U]

### [U] 목록(후보와 함께)

1. **[U]1 포트 즉석 설치 3곳**(`exec_ops.rs:36`, `http_outbound.rs:51`, `thread_archive_defer.rs:15`). 구현(`discord/inflight/runtime_view.rs:16-22` `install_engine_probes`, `discord/outbound/mod.rs:70-73` `install_engine_ports`)의 닫힘이 DL이다. 그래서 KRX-T10 규칙상 (i)는 안 되고 사용자 결정 대상이다.
   - (ii) linkme류 링크 시점 등록: 새 의존이라 공급망 검토가 필요하다
   - (iii) 위쪽 시험 helper가 명시적으로 설치하고, 설치에 기대는 KE 시험은 옮긴다
   - **권고:** (iii) + KE 쪽 `any(test, feature)` 빌드에서 "not installed"를 조용한 분기(`thread_archive_defer`는 미설치를 fresh로 본다) 대신 큰 소리로 실패시키는 장치. KRX-T10 판정과 같이 받는다.
2. **[U]2 인라인 RELOC justification 행**(합 하한 34~상한 92 + 연쇄). KRX-T5 FILES 선례처럼 조각마다 사전 표로 승인받는다. 대안은 모듈 수준 `use`를 함수 안으로 내려 옮길 함수를 하한(사이트가 든 함수)에 맞추는 것이다.
3. **[U]3 KRX-T8 KE host `krx_claude_tests`의 fixture `NONCE`·`bound_target`을 DL host 시험 2파일이 쓴다.**
   - (a) `services/mod.rs:186` host gate를 통째로 바꾸기: 16파일·50시험이 feature 빌드에 들어간다
   - (b) 두 항목만 KE 지원 위치로 떼기
   - **권고:** (b). KRX-T8b(submission·readiness 공유 fixture) 처방과 함께 정한다.
4. **[U]4 `auto_queue/runtime.rs` `RUNTIME_CLEARS`.** KE prod `verdict.apply()`가 ROOT `ManagedReset`을 돌려받는 prod 간선이 먼저다. KE-C 쪽 열린 조각이 있는지, 시험 static만 KE 투영 타입(예: `Option<String>` 결과)으로 바꿀지 정해야 한다.
5. **[U]5 #8339 agy_acp 파일 목록.** base에 해당 파일이 없다. KE `imsg_reply/agy_input.rs`(A 2곳)가 겹치는지 확인한 뒤 KE-T2나 KE-T8에 넣는다.
6. **[U]6 KE용 VT2 실행 여부.** (A)는 정적이다. glob 28행, `#[cfg(test)]` 구조체 필드·enum 변형(1건 손 발견), trait impl gate, `use … as` 별칭, 매크로는 놓칠 수 있다. KRX처럼 WSL에서 `vistool_testcfg.py` KE 모드를 한 번(약 1.5h) 돌려 보정할지 정한다. 권고 시점은 KE-T2 착수 전이다.

### 위험

1. **(A) 정적 스캔의 빈칸.** 위 [U]6. 조각마다 관문 5(뮤테이션 check)가 마지막 그물이다.
2. **feature 전용 빌드의 닫힘.** 같은 파일 55곳은 셌다. 파일 넘는 것은 match 팔 2파일 말고는 세지 못했다. 컴파일로만 확인되므로 맥이 WSL에서 본다(KE-T1 관문 5).
3. **feature 사슬.** M-KE에서 `adk-runtime/test-support`를 함께 켜지 않으면 `dispatch::test_support`·`health_diagnostics::pg_wire_proxy` facade가 feature 빌드에서 깨진다(§2-1).
4. **공유 fixture 연쇄·seam(KRX-T8 215/217, T9 output_tests, T5b barrier 교훈).** 정적으로 찾은 것(§1-3)은 조각에 미리 넣었다. 착수 base에서 `fixture_users.py`를 다시 돌린다. 새로 나오면 KRX-T8처럼 FILES로 멈춘다.
5. **넓히기 과소추정.** KRX-T8은 예상 0~5파일에서 실측 12파일이었다. RELOC 추정에 × 1.35와 +0~5를 넣었다. `queued_head_s2a_tests.rs`(`super::` 14)는 넓히기가 몰릴 후보다.
6. **test_wait_allowlist.** 인라인 이동으로 새 파일을 만들 때 상한 없는 대기가 섞이면 등록할 수 없다(KRX-T3·T9). seam 분리 새 파일에는 대기 없는 hook만 둔다.
7. **host 부모 줄 hunk.** RELOC 조각을 나란히 하면 `src/launch.rs`·`host_key_derivation.rs`에서 줄이 붙는다. 조각마다 줄 위치를 떼거나 차례로 착지한다.
8. **버킷은 이름 기반 휴리스틱이다.** AMB 18과 이름 충돌(`install`·`attempts` 등)이 있다. 조각 착수 때 재판정하고, RELOC 파일 수는 ±10%로 본다.
9. **겹침 목록의 한계.** 브리프 목록, `wt/i7353-dlmv1`·`wt/i7353-krxt10` 로컬 브랜치 diff, KRX-T5·T5b·T9 diff 파일, KRX 설계 slices의 T6·T7 행으로 모았다. 각 레인이 착수 뒤 더한 파일은 보지 못했다. `turn_bridge/tmux_runtime/stop_host_tests.rs`(KE-T4 DL 1줄)는 목록 밖이지만 착수 때 소유를 확인한다.
10. **M 메모(이 설계 밖).** ROOTED partition 재분류, TS 별칭, feature 사슬과 누수 가드. 시험 ID는 `pg15` 해당 0이라 실행 줄 분리가 필요 없다.

## 정리

- 레인 폴더에 만든 것은 `logs/ket-design/`(스크립트·JSON·txt·`slices.md`)와 `tools/` 사본이다. 빌드 산출물·바이너리·target은 없다.
- 이 세션에는 Memento 도구가 없어 앵커를 읽지 못했다.
