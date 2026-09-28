# #5340 단계 1 본체(H3~H5) — 현행 main 대비 delta 설계

- 작성: Opus 레인(설계 전용). 기준 `c4c00cae1`(origin/main, worktree `/private/tmp/adk-i5340-host` detached, **남겨 둠**). 모든 `파일:줄`은 `c4c00cae1` 기준이다.
- 입력: 설계 정본 `issue-5340-comments-20260929.md` r2 §5(:648~720), r3 §3.2·§5 H3·부록 A(:798~1030), 잔여 조사 `stage1-remainder.md`(cd8fe090 기준).
- 표기: **[사실]** = 이번에 소스(`git grep`/`sed`/`git diff 78dfe50f96 c4c00cae1`) 또는 읽기 전용 python(`giant_production_loc()`)으로 직접 확인. **[추정]** = 미확인. 실제 `rustfmt` 결과와 줄 수는 구현할 때 측정해야 한다.
- 준수: 코드 변경·커밋·push·PR·GitHub·DAG 조작 없음. cargo 미실행. 기억 저장 없음.

---

## 0. 결론 (한 페이지)

1. **H3·H4 대상 파일 7개는 78dfe50f96 이후 내용이 바뀌지 않았다 [사실].** `git diff --stat 78dfe50f96 c4c00cae1`에서 `tmux_diagnostics.rs`, `recovery_engine/tmux_probe.rs`, `recovery_engine.rs`, `live_agent_recovery.rs`, `abandon_guard.rs`, `watchers/lifecycle/liveness.rs`, `post_stream_exit.rs`, `turn_bridge/tmux_runtime.rs`의 변경은 0이다. r3가 적은 줄 번호가 그대로 유효하다. 바뀐 파일은 H5 쪽 `claude.rs`, `codex.rs`, `qwen.rs`(#6342로 줄 이동)와 H3 인접 `inflight/rebind_reap.rs`(#6361·#6365), `platform/tmux.rs`(#6338)이다.
2. **#6338이 H1의 "소비자 0"을 깨고 `TmuxHost.liveness_within`을 추가했다 [사실].** `session_host/tmux_host.rs:25-31`이 추가됐고 `engine/ops/exec_ops.rs:3,365`에서 쓴다. `pane_liveness` 자체의 의미는 바뀌지 않았다. 코드를 `platform/tmux/liveness.rs:6-19,53-89`로 추출했을 뿐이다. 따라서 `legacy_collapse` 3개 헬퍼와 `TmuxHost::liveness`(`tmux_host.rs:51-53`)의 대응도 그대로다. 단 `liveness_within`은 공백 이름을 `ProbeError`로 보내고(`liveness.rs:36-38`, `pane_liveness`는 `DeadOrAbsent`), 준비된 PATH만 쓴다. **그래서 H3~H5에서 `liveness_within`을 쓰면 동작 변경이다.**
3. **r3 설계에서 그대로는 성립하지 않는 항목 3개를 새로 찾았다 [사실].**
   - (a) **역방향 매핑이 없다.** `model.rs:34,42`에는 "no reverse mapping"이라 적혀 있고 `From<PaneLiveness> for HostLiveness`만 있다. 그래서 `abandon_guard.rs:162`(→ `abandoned_tmux_cleanup_decision(bool, PaneLiveness, …)` `:35-38`), `routing_orphan.rs:111`(→ `routing_orphan_pane_alive(PaneLiveness)` `:74`), `rebind_reap.rs:36`(→ `proven_dead_from_signals(PaneLiveness, …)` `:50`)을 `host_for(Tmux).liveness`로 바꾸면 **타입이 맞지 않는다**. 해결하려면 순수 함수 시그니처를 바꿔 테스트를 수정하거나(I-2·I-5 위반), 역방향 `From`을 추가해야 한다(r1 원칙 위반). → 결정 D1.
   - (b) **Windows 스텁이 있다.** `recovery_engine.rs:20-21`의 `#[cfg(unix)] use tmux_diagnostics::{…, tmux_session_has_live_pane}`와 `:221-224`의 `#[cfg(not(unix))] fn tmux_session_has_live_pane → false`가 있다. `tmux_probe.rs`는 무조건 컴파일되고(`recovery_engine.rs:58-59`), `recovery_engine/**`는 cross-OS 컴파일 대상이다(`ci-pr.yml` cross_os_rust 목록). r3 치환식(`has_live_pane_bool` 직접 호출)을 쓰면 Windows에서 스텁(false, 프로세스 없음)이 실제 `tmux list-panes` 호출로 바뀐다. `platform::tmux::has_live_pane`은 `tmux.rs:877`에 cfg 없이 정의돼 있고 `tmux_command`에는 `:52` `cfg(windows)` 분기가 있다. **Windows 동작 변경이다.** → 결정 D3.
   - (c) **거대 파일은 순증 0이 하드 게이트다.** `giant_file_progress.py:300-305` `new_or_growing_errors`는 prod LoC ≥ 1000인 파일이 1줄이라도 늘면 실패하며, admission 경로가 없다. 측정값 [사실]: `claude.rs` 2729, `codex.rs` 2775, `qwen.rs` 1535, **`discord/tmux.rs` 1469**. r3 치환식은 모두 100열을 넘어 `rustfmt`가 줄을 나눈다. 예: `let session_exists = probe_failed_to_missing(TmuxHost.presence(HostSessionRef::tmux(tmux_session_name)));`는 약 109열이다. 사이트마다 +1줄, import 블록도 +3~5줄이 붙어 **claude.rs가 +6~8줄로 추정**된다. → 결정 D2.
   - 덧붙여 `discord/tmux.rs:21-24`의 import(`tmux_session_exists, tmux_session_has_live_pane`)는 H3 이후 자식(`post_stream_exit.rs`, `watchers/lifecycle/liveness.rs`)이 바뀌면 **미사용 import**가 된다 [사실: 그 서브트리의 맨이름 사용처는 이 두 파일뿐]. 그러므로 H3는 거대 파일 `discord/tmux.rs`를 반드시 건드린다. 이 파일의 shrink 기한은 **2026-10-31**이다(`giant_file_registry.toml:727-730`). 기한이 지나면(overdue) prod LoC가 1줄만 줄어도 `pr_strict_progress`(≥200줄 감소 요구, `giant_file_progress.py:563-567,326-329`)로 판정된다. **H3는 10-31 전에 머지하거나, 이 import 편집을 줄 수 0 변화로 맞춰야 한다.**
4. **H2 게이트 없이 진행해도 동작 0의 안전 증명에는 빠지는 것이 없다.** r3 §8.1-5도 래칫은 "성장 억제 장치이며 안전 증명이 아님"이라고 적었다. 빠지는 것은 (i) 치환한 파일에 원시 호출이 다시 들어오는 것을 막는 장치, (ii) `legacy_collapse` 개수를 등호로 핀하는 자동 증명이다. 새 인프라는 만들지 않는다. **PR 본문 전수표(고정 `git grep` 명령의 base/head 결과), 기존 게이트(G-1·G-2·G-5), I-1~I-5 체크리스트, 별칭·재수출 0 diff 검사**로 대신한다(§2).
5. **개정 분할은 H3 → H4 → H5, PR 3개다.** 모두 cap 여유가 크다(각 prod ≤ 8파일, ≤ +80줄 추정). H3는 abandon_guard·routing_orphan·rebind_reap을 제외(D1)해 7파일이다. H4는 1파일이다. H5는 이름을 받는 헬퍼(D2) 1파일과 provider 6파일이다(§3).

---

## 1. H3·H4·H5 항목별 현행 좌표 대조표

판정 범례: **동일** = 줄·식·의미 동일 / **이동** = 줄만 이동 / **의미 변화** = 주변 의미나 타입 제약이 바뀌어 치환식을 고쳐야 함 / **신규** = r3에 없던 사이트.

### 1.1 H3 (recovery·watcher·diagnostics)

| # | 설계(r3) 좌표·치환식 | 현재(c4c00cae1) | 판정 | 비고 |
|---|---|---|---|---|
| 3-1 | `tmux_diagnostics.rs:22-31` 내부 `tmux_session_exists(&name)` → `probe_failed_to_missing(host_for(Tmux).presence(..))`. 바깥 `unwrap_or(Ok(true)).unwrap_or(true)`는 유지 | `:22-31`, 내부 호출 `:26`. `tmux_session_exists`는 `:11-13`, `has_session`은 `tmux.rs:139-141` `session_presence()==Present` | **동일** | 진리표(Present→T, Missing→F, ProbeFailed→**F**, 10s timeout/join→T)는 정의상 동치다 [사실]. D2-a를 채택하면 `legacy_collapse::tmux_present_bool(&name)`. `probe_failed_to_present`는 여전히 없다(`legacy_collapse.rs` 헬퍼 3개: `:8,:13,:18`). |
| 3-2 | `recovery_engine/tmux_probe.rs:6,:11` 글롭 상속 `tmux_session_has_live_pane` → `has_live_pane_bool` | `:6,:11` 동일. 이름 출처는 `recovery_engine.rs:20-21`(`#[cfg(unix)]` import) **+ `:221-224` `#[cfg(not(unix))]` 스텁(false)** | **의미 변화(Windows)** | r3 식을 그대로 쓰면 Windows에서 스텁이 실제 tmux 호출로 바뀐다. 해결은 D3(스텁을 `has_live_pane_bool` 이름으로 옮김). 재시도는 첫 시도 + 재시도 2회(`:9 for attempt in 1..=2`), `std::thread::sleep(recovery_retry_backoff)`는 유지. |
| 3-3 | `tmux_probe.rs:24,:29` `platform::tmux::has_session` → `probe_failed_to_missing(presence)` | `:24,:29` 동일 | **동일** | `has_session`은 cfg 없이 양 OS에서 컴파일된다. Windows 의미도 같다(같은 함수를 호출). |
| 3-4 | `live_agent_recovery.rs:193-204`. `:175-177` 분기는 무변경, `:199 pane_liveness` → `host_for(Tmux).liveness`, `matches!(..DeadOrAbsent)` → `dead_only_if_dead_or_absent` | process 분기 `:193-195`(`!process_session_is_alive`), tmux 분기 `:197-204`(`spawn_blocking` + `matches!(pane_liveness, DeadOrAbsent)` + `.unwrap_or(false)`) | **동일** | `ProcessHost::liveness`(`process_host.rs:43-49`)는 `process_session_is_alive`→Live, 아니면 DeadOrAbsent이다. 그래서 `!alive ≡ dead_only_if_dead_or_absent(host_for(Process).liveness(..))`이다 [사실]. `spawn_blocking`·`unwrap_or(false)`는 호출부에 그대로 둔다(I-4). |
| 3-5 | `placeholder_sweeper/abandon_guard.rs:162` `tmux_session_pane_liveness` → `host_for(Tmux).liveness` | `:162` 동일. 받는 쪽은 `abandoned_tmux_cleanup_decision(bool, PaneLiveness, RuntimeActivityEvidence)` `:35-38`. 그 파일 안 호출 9건 중 테스트 다수(`:517`~) | **의미 변화(타입)** | `HostLiveness→PaneLiveness` 역매핑이 없다(`model.rs:42` "no reverse mapping"). 치환하려면 시그니처와 테스트를 바꿔야 한다(I-2/I-5 위반). → **D1: H3에서 제외(인벤토리)** 권고. |
| 3-6 | `watchers/lifecycle/liveness.rs:14-31`, `:22` 글롭 상속 → `has_live_pane_bool`. 10s timeout·`unwrap_or(Ok(false)).unwrap_or(false)`는 유지 | `:14-31`, `:22` 동일. 이름 출처는 `discord/tmux.rs:21-24`(unix 전용 트리: `discord/mod.rs:119-120` `#[cfg(unix)] mod tmux`, `tmux.rs:62-63` `#[path] mod watcher_lifecycle`) | **동일** | Windows 영향 없음. |
| 3-7 | `tmux_watcher/post_stream_exit.rs:111,:133,:188`. `:111`은 `has_live_pane_bool`. `:133,:188`은 `probe_failed_to_missing(presence) && !has_live_pane_bool(..)`. kill(`:189-192`)은 무변경 | `:111`, `:133`, `:188`, kill `:189-192` 동일 | **동일** | 3곳 모두 `spawn_blocking` 안이다. 파괴 래칫 `post_stream_exit.rs: 1`(`destructive_call_site_baseline.json:29,87`)은 유지. |
| 3-8 | (r3 미기재) `discord/tmux.rs:21-24` import | `:23` `tmux_session_exists, tmux_session_has_live_pane` | **신규(강제)** | 3-6·3-7 이후 미사용이 된다. 거대 파일(1469)이다. 이름 두 개를 지우면 `rustfmt`가 import 블록을 다시 감싼다. 남는 항목 4개는 한 줄에 들어가지 않아 2줄 블록이 유지될 것으로 **추정**하며, 그러면 prod LoC 변화는 0이다. **감소도 기한(10-31) 이후에는 위험하다**(§0-3). |
| 3-9 | (r2 §5 H3 "이미 3-상태 → 1:1") `recovery_engine/routing_orphan.rs`, `inflight/rebind_reap.rs` | `routing_orphan.rs:111`(→ `routing_orphan_pane_alive(PaneLiveness)` `:74-76`), `rebind_reap.rs:34-38`(**#6361/#6365로 `#[cfg(not(test))]`/`#[cfg(test)] tests::tmux_session_pane_liveness` 시험 봉합이 생김**, `rebind_reap/tests.rs:22-30`) | **의미 변화(타입 + 봉합)** | 3-5와 같은 역매핑 문제다. rebind_reap은 prod 줄을 바꾸면 cfg(test) 봉합의 폴백(`tests.rs:28`, 원래 wrapper 호출)과 어긋난다. → **D1: 제외** 권고. |
| 3-10 | 부록 A: `restore_inflight.rs` → `tmux_has_session_with_retry` 사용(래퍼), P2-1 대상 아님 | `restore_inflight.rs:22,:1456` | **이동** | 래퍼를 쓰는 쪽이다. 3-3이 내부를 치환하면 자동으로 흡수된다. |

### 1.2 H4 (turn_bridge)

| # | 설계 좌표·치환식 | 현재 | 판정 | 비고 |
|---|---|---|---|---|
| 4-1 | `tmux_runtime.rs:176-177` `spawn_blocking(|| platform::tmux::send_keys(&s,&keys))` → `host_for(Tmux).send_keys(ref,&keys)`, `Confirmed ⇔ status.success()` | `:175-178` 동일. 결과 match `:180-215`: `Ok(Ok(o)) if success → true` / `Ok(Ok(o)) → warn(status,stderr) false` / `Ok(Err(e)) → warn(error) false` / `Err(join) → warn false` | **동일(bool) / 로그 변화** | 반환 bool과 분기는 동치다. 다만 `map_output`(`tmux_host.rs:14-22`)이 비성공 종료와 spawn 실패를 모두 `Transport(String)`으로 합친다. 그래서 **`status=` 필드와 "send-keys failed"/"send-keys error" 구분이 로그에서 사라진다.** → 결정 D4. |
| 4-2 | `:555-556` `pane_pid` → `execution_pid` | `:555-556` 동일(`spawn_blocking` 안) | **동일** | `TmuxHost::execution_pid`는 `Ok(tmux::pane_pid(..))`(`tmux_host.rs:95-97`)라 `Err`가 나오지 않는다. `.ok().flatten()` 또는 `.unwrap_or(None)`과 동치다. 비-unix `pane_pid` 스텁(`tmux.rs:521-524`)도 같은 함수를 거친다. |
| 4-3 | 무변경 선언: `:117-126`, `:151-160`, `:435`, `:508-527`, `:694`, `process_backend_cancel.rs`, `process_table.rs` | `:117`(`interrupt_provider_cli_turn`), `:124-125`(`tmux_session.is_none()` → `interrupt_process_backend_turn`), `:498,:509,:525`(하드스톱), `:687`(`cancel_active_token`) | **동일** | `turn_bridge/**`의 78dfe50f96 이후 변경 16파일에 `tmux_runtime.rs`와 `process_backend_cancel.rs`는 없다 [사실]. 파괴 래칫: `tmux_runtime.rs: 1`, `process_backend_cancel.rs: 1`(`baseline.json:75-76`). |

### 1.3 H5 (providers)

| # | 설계 좌표(r2/r3) | 현재 | 판정 | 비고 |
|---|---|---|---|---|
| 5-1 | `claude.rs:60-62` import | `:59-63`(`#[cfg(unix)] use tmux_diagnostics::{record_tmux_exit_reason, should_recreate_session_after_followup_fifo_error, tmux_session_exists, tmux_session_has_live_pane}`) | **이동** | 거대 파일, 순증 0 필수(D2). |
| 5-2 | `claude.rs:1726` `tmux_session_exists` | **`:1737`** | 이동(+11, #6342) | presence 치환식은 100열을 넘는다(D2). |
| 5-3 | `claude.rs:1752` `tmux_session_has_live_pane(..) && profile_matches` | **`:1763`** | 이동 | 같음. |
| 5-4 | `claude.rs:2498`, `:2524` | **`:2506`**, **`:2532`** | 이동(+8) | 같음. |
| 5-5 | (r3 부록 C에서 형태 3으로 계수만, 부록 A 치환 대상 아님) `claude.rs:2139,:2192` 클로저 | **`:2147`**(`SessionProbe::new(move || tmux_session_has_live_pane(&tmux_name_alive), …)`), **`:2200`**(`|| tmux_session_has_live_pane(tmux_session_name)`) | **신규(치환 권고)** | 이 둘을 남기면 import에서 이름을 지울 수 없어 import 절감 효과가 사라진다. 같은 bool 래퍼를 1:1로 바꾸는 것이므로 H5 포함을 권고한다(D5). |
| 5-6 | `codex.rs:44-46` import | `:43-47` | 이동 | 거대 파일(2775). |
| 5-7 | `codex.rs:1695,:1743,:2171,:2189` | **`:1696,:1744`**, `:2171,:2189` | 이동(+1) / 동일 | |
| 5-8 | (부록 C만) `codex.rs:1647,:1657` 클로저 | **`:1648,:1658`**(`rollout_tail::…_for_tmux(…, || tmux_session_has_live_pane(..), …)`) | **신규(치환 권고)** | D5. |
| 5-9 | (stage1-remainder에서 언급, r3 미기재) `codex.rs:1612` `pane_pid` | **`:1613`**(`wire_cancel_token_to_tmux_session`) | **신규(보류 권고)** | H4의 `execution_pid`와 같은 모양이지만 거대 파일의 import가 늘어난다. phase 1 대상에서 뺀다(D5). |
| 5-10 | `codex/followup_reader.rs:53` | `:53` | 동일 | `mod followup_reader`는 `#[cfg(unix)]`(`codex.rs:4-5`). `:37` `SessionProbe::tmux_with_structured_output`은 무변경. |
| 5-11 | `qwen/followup_reader.rs:69` | `:69` | 동일 | `#[cfg(unix)] mod`(`qwen.rs:3-4`). `:53` `SessionProbe::tmux`는 무변경. |
| 5-12 | `qwen/session_lifecycle.rs:25,:48` | `:25,:48` | 동일 | 모듈은 cfg 없이 선언(`qwen.rs:20-21`). 함수는 `#[cfg(unix)]`(`:4`). |
| 5-13 | (r3 미기재) `qwen.rs:53-57` import | `:53-57` 동일 구조 | **신규(강제)** | 5-11·5-12 이후 이 import의 두 이름은 글롭 자식 전용이다. 치환 방식에 따라 import를 바꿔야 하므로 거대 파일(1535) 순증 0 대상이다. |
| 5-14 | `claude/backend_routing.rs:81-89` 무변경(역매핑 금지) | `:88` `tmux_diagnostics::tmux_session_pane_liveness` 주입, `:71` `impl FnOnce(&str)->PaneLiveness` | **동일** | 무변경 결정을 유지한다. `claude.rs:830-837`(`prepare_tmux_backend_after_refused_process_demotion`)도 이 주입을 거친다. |
| 5-15 | `SessionProbe::tmux*` 7건 무변경 | `claude.rs:2915`, `codex.rs:2384`, `codex/followup_reader.rs:37`, `qwen/followup_reader.rs:53`, `qwen/session_lifecycle.rs:259` 외 | 동일 | P2-1 대상. |

### 1.4 `legacy_collapse` 헬퍼 대응 (#6338 3-상태 영향)

- `PaneLiveness`는 #4489부터 이미 Live/DeadOrAbsent/ProbeError 3-상태였다. #6338은 **`pane_liveness`를 추출만 했고**(`tmux.rs:925-930` → `liveness.rs:6-19` → `probe_pane_liveness :53-89`, 서브커맨드·2초 타임아웃·공백→DeadOrAbsent 동일) **`pane_liveness_within`을 새로 추가했다** [사실: `git diff 78dfe50f96 c4c00cae1 -- src/services/platform/tmux.rs`는 이 구간만 +16/−35].
- 결과: `probe_failed_to_missing`(`SessionPresence` 기반), `dead_only_if_dead_or_absent`(`HostLiveness` 기반), `has_live_pane_bool`(`has_live_pane` bool, 2초 바운드 없음 `tmux.rs:877-899`)의 극성과 프로브 동일성은 **바뀌지 않았다**.
- 새 위험: 구현자가 "3-상태면 `liveness_within`이 더 안전하다"고 보고 `TmuxHost.liveness_within`을 쓰는 경우다. `liveness_within`은 공백→ProbeError, prepared-PATH 부재→ProbeError, 공유 예산을 쓰므로 **동작 변경**이다. H3~H5에서는 `liveness`(= `pane_liveness`)만 허용한다(리뷰 항목 R-3).

### 1.5 새로 생긴 원시 호출 사이트 (브리프 grep 전수)

명령: `git grep -n -E 'tmux_session_exists|tmux_session_has_live_pane|pane_liveness|has_session|tmux_session_pane_liveness' <rev> -- src ':!src/services/session_host/**' ':!src/services/platform/tmux.rs' ':!src/services/platform/tmux/**'`. 결과는 78dfe50f96에서 216줄, c4c00cae1에서 222줄이다. 공백을 정규화해 줄 번호를 떼고 `comm`으로 비교한 결과 **추가 6줄, 삭제 0줄**이다 [사실].

| 추가 줄 | 분류 | phase 1 처분 |
|---|---|---|
| `recovery_engine/restore_inflight/kickoff_identity.rs:68` `probe_tmux_session_pane_liveness(..).await` (#6208) | **prod, 비동기 3-상태**(#5185 오버라이드 경로) | P2-2 인벤토리, **무변경**(r3 원칙). `recovery_engine/**` 원시 0 판정에서 명시 예외로 둔다. |
| `inflight/rebind_reap.rs:38` `tests::tmux_session_pane_liveness` (#6361/#6365) | `#[cfg(test)]` 봉합 | 계수 제외(시험). prod `:36`은 3-9 참조. |
| `inflight/rebind_reap/tests.rs:22,:28` | 시험 | 제외. |
| `recovery_engine/restore_inflight/kickoff_identity_tests.rs:42` | 시험 함수 이름 | 제외(오탐). |
| `tmux_watcher/streaming_harness_tests.rs:257` (#6296) | 시험 | 제외. |

패턴 밖의 신규 소비자: `engine/ops/exec_ops.rs:3,:365` `TmuxHost.liveness_within`(#6338). session_host의 첫 외부 소비자이며 3-상태를 보존하는 올바른 소비다. phase 1 완료 판정의 "session_host 외부 소비자 = H3~H5 사이트뿐" 식 문구가 있다면 이 사이트를 예외로 적어야 한다.

줄만 이동한 기존 사이트(범위 밖, 참고) [사실]: `health_api.rs:1204→1159`(함수 포인터, #6320), `turn_start.rs:446,470→453,477`, `control.rs:138,154→140,156`, `turn_lifecycle.rs:209,290,300,323→207,288,298,321`, `reliability.rs:145→149`.

전체 222줄의 prod 분류(요약) [사실, 수동 분류]:
- **H3~H5 대상**: 1.1~1.3의 사이트.
- **범위 밖 bool 래퍼 전체 경로 호출**(r3 부록 A에 없음, phase 1 완료 디렉터리 밖): `claude_tui/input.rs:387,803,2049`, `codex_tui/input.rs:422`, `codex_tui/warm_followup.rs:268,497`, `commands/tui_passthrough.rs:249`, `idle_recap_interaction.rs:398`, `idle_relay_drift.rs:510`, `inflight/removal.rs:419`, `provider_isolation.rs:279,352,437`, `tui_followup.rs:554`, `watchdog.rs:85`, `terminal_ui_obligation.rs:339`, `tui_prompt_relay/{claude_idle_runtime.rs:666, codex_idle_rollout.rs:421, rehydration.rs:41,42,155,183,253}`, `provider.rs:1148`, `provider_cli/session_guard.rs:193`, `termination_audit.rs:179`, `session_activity.rs:13→52`, `tmux_reaper.rs:8→190,252,317,452,702`, `completion_gate.rs:196`, `server/routes/agents.rs:457,458`, `reports.rs:24→199,201,325,327`(+ 비-unix 스텁 `:27,:31`).
- **완료 디렉터리 안인데 r3 부록 A에 없는 것**: `recovery_engine/rebind_runtime.rs:482`(bool 래퍼, 클로저 안), `recovery_engine/terminal_watcher.rs:37`(함수 포인터 `has_session`, r3는 S3로 분류). → phase 1 완료 판정에 **명시 예외**로 올리거나 H3에 넣어야 한다. rebind_runtime은 클로저 1:1이라 넣을 수 있다. terminal_watcher는 함수 포인터라 시그니처 변경이 필요하므로 예외로 둔다. → D6.
- **has_session 원시(범위 밖, 인벤토리)**: `cli/dcserver.rs:101,823`, `provider_auth_profiles.rs:333,481`, `control.rs:140,156`, `turn_start.rs:453,477`, `turn_lifecycle.rs:207,288,298,321`, `health_api.rs:1159`, `claude_tui/tui_relay.rs:98`(H6).
- **3-상태 pane_liveness 원시(범위 밖)**: `health/session_enrichment.rs:516`.
- 나머지(지역 변수·필드·주석·트레이트 메서드·`has_session_id` 등)는 **오탐**이다(skill.rs, text_commands.rs, recovery.rs:789, response_format.rs, completion_gate.rs:77, placeholder_live_events, adk_session.rs, hook_relay 테스트, snapshot 주석 등).

---

## 2. H2 게이트 부재의 영향과 최소 대체

### 2.1 설계가 H3~H5에서 H2에 기대던 것
| 기대 | 설계 위치 | 부재 시 처리 |
|---|---|---|
| H3/H4/H5 파일 목록의 `scripts/session_host_boundary_baseline.json` 갱신 | r2 §5 H3·H4·H5 | **삭제.** 베이스라인 파일이 없다 [사실: `clippy.toml`도 없고 `scripts/ci/h2_*.toml`도 없음]. H2 측정기(`scripts/ci/h2_measure.py`)는 clippy 기반이라 cargo 빌드가 필요하고 CI에서 호출되지 않는다. |
| 의존 "H3: H1, **H2(r3)**" | r3 §5 H3 | **삭제.** H3는 H1(착지)에만 의존한다. |
| G-3 경계 래칫 `--check`(원시·래퍼 비증가, `legacy_collapse::` 등호) | r2 §5 공통 | **PR 본문 전수표로 대체**(아래 E-1). |
| 별칭 import·`pub use` 재수출 red, 함수 포인터 계수 | r3 H2 (d)(f) | **diff 검사 1줄로 대체**(E-2). 새 파일 전체가 아니라 PR diff의 `+` 줄만 본다. |
| phase 1 완료 판정 "경계 래칫 (a)(b) 카운트 0" | r2 §5 완료 판정 | **고정 `git grep` 명령의 결과표**로 판정한다(E-1의 전역판). 명시 예외 목록은 §1.5와 D6. |

### 2.2 동작 0 증명(G·I)만으로 충분한가
- **안전 측면에서는 충분하다.** 동작 0의 증명력은 원래 G-1(`library_sweep`), G-2(파괴 래칫), G-4 = I-1~I-5(사이트별 동치 논증), G-5(거대/핫 파일)에서 나온다. G-3은 "이후에 원시 호출이 다시 늘지 않게" 하는 성장 억제 장치다. r3 §8.1-5도 같은 입장이다.
- **잃는 것**: (i) 치환이 끝난 파일에 원시 호출이 다시 들어오는 것을 CI가 막지 못한다. (ii) `legacy_collapse` 호출 수를 자동으로 핀하지 못한다. 둘 다 런타임 안전과는 무관하다. H2가 나중에 활성화되면(3a/3b) 그 시점의 실측이 베이스라인이 되므로 되돌릴 비용도 없다. H3~H5는 H2 기준 행 수를 줄이는 방향이다.
- **추가로 막아야 할 것**: H2의 변이 픽스처가 잡던 "같은 이름 함수를 `legacy_collapse` 밖에 정의"와 "별칭 import"는 리뷰 체크리스트와 E-2로 막는다.

### 2.3 최소 대체 (새 스크립트·CI 배선 없음)
- **E-1 PR 본문 전수표**: PR마다 아래 명령을 base와 head에서 실행해 파일별 개수를 두 열로 붙인다. 대상 파일에서는 "치환 계획 수만큼 감소 = legacy/host 증가"가 성립해야 하고, 대상 밖 파일의 개수는 같아야 한다.
  ```
  S='\b(tmux_session_exists|tmux_session_has_live_pane|tmux_session_pane_liveness|probe_tmux_session_exists|probe_tmux_session_pane_liveness)\b|platform::tmux::(has_session|session_presence|has_live_pane|pane_liveness|pane_pid|send_keys)\b|\b(probe_failed_to_missing|dead_only_if_dead_or_absent|has_live_pane_bool|tmux_present_bool|tmux_live_pane_bool)\b|\b(host_for|TmuxHost|ProcessHost)\b'
  for r in <base> <head>; do echo "== $r"; git grep -c -E "$S" $r -- src ':!src/services/session_host/**' ':!src/services/platform/tmux.rs' ':!src/services/platform/tmux/**' ':!*tests.rs' ':!*/tests/*'; done
  ```
  (`tmux_present_bool`/`tmux_live_pane_bool`은 D2-a 헬퍼 이름의 예시다. 채택한 이름으로 바꾼다.) 사이트별로 "전 식 → 후 식 → 동치 근거(§1 행 번호)" 표를 함께 붙인다(I-1).
- **E-2 별칭·재수출 0**: `git diff <base>..<head> -- src | grep -E '^\+.*(pub(\([a-z]+\))? use .*(platform::tmux|tmux_diagnostics|legacy_collapse)|(platform::tmux|tmux_diagnostics|legacy_collapse)(::\{[^}]*)?\b[a-z_]+ as )'` 결과가 비어야 한다.
- **E-3 기존 게이트(변경 없음)**: G-1 `library_sweep`. G-2 `python3 scripts/check_destructive_call_site_ratchet.py --check` 통과와 `git diff --exit-code scripts/destructive_call_site_baseline.json`(파일별 개수이며, 치환은 kill 줄을 건드리지 않는다). G-5 `python3 scripts/check_hotfile_ratchet.py`, `audit_maintainability` 거대 파일 래칫, `giant_file_progress`(CI). 로컬에서는 D2 측정 명령을 쓴다.
- **E-4 I-5 증명**: `git diff --stat <base>..<head> -- '*tests.rs' '*/tests/*'`에 **추가만** 있어야 한다(새 진리표 시험). 기존 시험 줄 수정은 0.
- 측정 인프라 신규 0(메인 결정 E 준수).

---

## 3. 개정 PR 분할

공통 사항: 동작 변경 0. 한 필터에 한 명령. cap은 `python3 ~/ObsidianVault/RemoteVault/99_Skills/agentdesk-issue-pipeline/scripts/pr_cap_prod.py origin/main HEAD --repo <wt>`로 잰다(prod 20파일/+800, Rust는 첫 `#[cfg(test)]` 위만 계수 [사실: 스크립트 docstring]). 주의: `tmux_diagnostics.rs`는 첫 `#[cfg(test)]`가 `:60`이라 `:22-31` 편집만 계수된다. **거대 파일(claude/codex/qwen/discord::tmux) prod LoC는 base보다 늘면 안 된다**(측정 명령은 아래 공통 박스).

```
# 거대 파일 prod LoC 측정 (읽기 전용, cargo 불필요) — base 와 head worktree 각각에서
cd <wt>/scripts && PYTHONDONTWRITEBYTECODE=1 python3 -c "import sys;sys.path.insert(0,'.');from audit_maintainability.checks.giant_files import giant_production_loc as g;d=g();[print(k,d.get(k)) for k in ('src/services/claude.rs','src/services/codex.rs','src/services/qwen.rs','src/services/discord/tmux.rs','src/services/discord/tmux_watcher.rs')]"
```
c4c00cae1 값: claude 2729 / codex 2775 / qwen 1535 / discord/tmux 1469 / tmux_watcher 2522(hotfile 상한 2544, 이번 분할에서는 편집하지 않음).

### H3 `refactor(recovery,watcher): bool·3-상태 tmux 프로브를 session_host 경유로 (동작 0)`
- 의존: H1(착지). **H2 의존 삭제.** 머지 목표는 **2026-10-31 이전**(discord/tmux.rs 기한, §0-3).
- prod 파일(7): `tmux_diagnostics.rs`(3-1), `discord/recovery_engine.rs`(D3 스텁 이전, +2~4), `recovery_engine/tmux_probe.rs`(3-2·3-3), `health/recovery/live_agent_recovery.rs`(3-4), `watchers/lifecycle/liveness.rs`(3-6), `tmux_watcher/post_stream_exit.rs`(3-7), `discord/tmux.rs`(3-8 import 정리, 순증 0). D2-a를 채택하면 `session_host/legacy_collapse.rs`(+~12)가 더해져 8파일이다. D6을 채택하면 `rebind_runtime.rs:482`가 더해진다.
- 시험(cap 제외): `tmux_diagnostics.rs` 하단 `#[cfg(test)]`에 `probe_tmux_session_exists` 진리표를 둔다(공백 이름 `""` → ProbeFailed → false, r3 §5 H3). D2-a 헬퍼 동치 시험도 넣는다(`legacy_collapse.rs` tests, 공백 이름).
- 추정 규모: prod +40~70 / −25~35.
- 제외(명시): `abandon_guard.rs`, `routing_orphan.rs`, `inflight/rebind_reap.rs`(D1), `tmux_watcher.rs`(핫 파일), `terminal_watcher.rs:37`(함수 포인터, S3), `kickoff_identity.rs:68`(비동기, P2-2), `tmux_reaper.rs`(S3b).
- 선택 잡(예상): `check_fast`(rust_or_policy `'src/**'`), `library_sweep`(`ci-pr.yml:1457`), `high-risk-recovery`(`:1096`, 필터 `'src/services/discord/**'`), `check_fast_cross_os`(`:749`; `recovery_engine.rs`·`recovery_engine/**`·`health/**` 파생 목록과 `'src/services/*'`에 `tmux_diagnostics.rs`가 포함된다. **D3의 Windows 스텁 보존을 여기서 컴파일로 증명한다**), `lint`.
- 로컬:
  - `cargo test --lib services::tmux_diagnostics::`
  - `cargo test --lib services::session_host::`
  - `cargo test --lib services::discord::recovery_engine::`
  - `cargo test --lib services::discord::health::recovery::`
  - `cargo test --lib services::discord::tmux::watcher_lifecycle::` (모듈명 [사실]: `discord/tmux.rs:62-63`)
  - `cargo test --lib services::discord::tmux::tmux_watcher::post_stream_exit::` (`tmux.rs:2358`, `tmux_watcher.rs:172-173`)
  - `cargo clippy --lib -- -D warnings`(미사용 import 검출; justfile `:16`과 같은 플래그)

### H4 `refactor(turn_bridge): tmux 확정 분기의 send_keys·pane_pid를 TmuxHost 경유 (동작 0)`
- 의존: H1. **H3와 독립**이다(파일이 겹치지 않음). r2의 "H3 의존"은 H2 베이스라인 순서 때문이었으므로 삭제한다. 병렬 착수가 가능하다.
- prod 파일(1): `turn_bridge/tmux_runtime.rs`(`:175-215` send_keys match, `:556` pane_pid). D4-b를 채택하면 pane_pid만 바꾼다.
- 추정 규모: prod +8~15 / −6~10.
- 무변경: §1.2 4-3 목록(`:117-126`, `:151-160`, `:498-527`, `:687`, `process_backend_cancel.rs`, `process_table.rs`).
- 선택 잡: `check_fast`, `library_sweep`, `high-risk-recovery`(discord/**), `check_fast_cross_os`(`turn_bridge/**`, 비-unix `pane_pid` 스텁 경로), `lint`.
- 로컬: `cargo test --lib services::discord::turn_bridge::tmux_runtime::` / `cargo test --lib services::session_host::`.
- 증명 추가: `git diff -U0 … | grep -E 'interrupt_process_backend_turn|hard_stop_unresponsive|lock_current_claude_interrupt_session|with_composer_mutation_lock|tmux_session.is_none'` 결과 0(호출 위치 변화 0, r2 §4.5). 파괴 래칫 `tmux_runtime.rs: 1`, `process_backend_cancel.rs: 1` 유지.

### H5 `refactor(providers): 기동 시 존재·생존 bool 프로브를 legacy_collapse 경유로 (동작 0)`
- 의존: H1, H0(착지). D2-a 헬퍼를 H3에서 넣었다면 H3에도 의존한다. 넣지 않았다면 H5가 헬퍼를 넣는다.
- prod 파일(6~7): `claude.rs`(5-1~5-5: 사이트 6개 + import), `codex.rs`(5-6~5-8: 사이트 6개 + import), `qwen.rs`(5-13 import), `codex/followup_reader.rs:53`, `qwen/followup_reader.rs:69`, `qwen/session_lifecycle.rs:25,48`, (+`legacy_collapse.rs`).
- **거대 파일 제약(하드)**: claude/codex/qwen의 prod LoC가 모두 base 이하여야 한다. 방법은 D2-a(이름을 받는 헬퍼 + 중첩 import)다. 사이트는 1줄 대 1줄로, import 블록은 줄 수가 같게 바꾼다. **추정**: 중첩 import의 둘째 줄 `    tmux_diagnostics::{record_tmux_exit_reason, should_recreate_session_after_followup_fifo_error},`가 정확히 100열이라 경계값이다. `cargo fmt` 후 위 측정 명령으로 확인한다. 초과하면 D2 대안으로 간다. 무관한 줄 삭제로 보상하는 것은 금지한다.
- 무변경: `SessionProbe::tmux*`(5-15), `backend_routing.rs`(5-14), `codex.rs:1613` pane_pid(D5), kill_session 전부(파괴 래칫 `claude.rs: 6`, `codex.rs: 4` 유지, `baseline.json:12,14`).
- 추정 규모: prod +15~30 / −15~30(사이트 치환은 순 0, 헬퍼 +12).
- 선택 잡: `check_fast`, `library_sweep`, `high-risk-recovery`(H0 경로 `ci-pr.yml:129-135`; 잡의 시험은 provider 계열뿐이며 이 모듈의 시험은 library_sweep에서 돈다), `check_fast_cross_os`(`'src/services/*'`와 `claude/**`·`qwen/**`·`session_host/**` `:568-577`). 대상 사이트는 모두 `#[cfg(unix)]` 함수나 모듈 안이지만 import의 cfg 짝이 Windows 컴파일에서 증명된다. `pg_db`는 미선택(`provider.rs`/`platform/tmux.rs` 무편집).
- 로컬: `cargo test --lib services::claude::` / `cargo test --lib services::codex::` / `cargo test --lib services::qwen::` / `cargo test --lib services::session_host::` / `cargo clippy --lib -- -D warnings`.
- 리뷰 분량이 부담이면 H5a(claude.rs)와 H5b(codex.rs + qwen)로 나눠도 cap과 무관하다. 권고는 하나로 두는 것이다(같은 치환식이 세 번 반복되므로 한 번에 대조하는 편이 싸다).

---

## 4. 구현 전 결정 항목

| ID | 질문 | 선택지 | 권고 |
|---|---|---|---|
| **D1** | 이미 3-상태 `PaneLiveness`를 순수 함수에 넘기는 3곳(`abandon_guard.rs:162`, `routing_orphan.rs:111`, `rebind_reap.rs:36`)을 H3에서 치환할 것인가 | (a) 제외하고 인벤토리에만 둔다 (b) `From<HostLiveness> for PaneLiveness` 역매핑을 추가한다(`model.rs` "no reverse mapping" 파기, H5 backend_routing 무변경 근거와 충돌) (c) 순수 함수 시그니처를 `HostLiveness`로 바꾸고 시험을 고친다(I-2·I-5 위반) | **(a).** 이 3곳은 이미 무손실 3-상태이고 bool 붕괴가 없어 phase 1의 목적(붕괴 지점에 이름 붙이기)과 무관하다. 이전은 P2(호스트 주입 설계)에서 한다. phase 1 완료 판정 문구에서 `recovery_engine/**`·`placeholder_sweeper/**`·`inflight/**`의 "원시 0"을 "bool 붕괴 원시 0, 3-상태 동기 래퍼는 예외 목록"으로 바꾼다. |
| **D2** | 거대 파일에서 치환식이 100열을 넘어 줄이 늘어나는 문제 | (a) `legacy_collapse`에 이름을 받는 얇은 헬퍼 2개(`tmux_present_bool(name) = probe_failed_to_missing(TmuxHost.presence(HostSessionRef::tmux(name)))`, `tmux_live_pane_bool(name) = has_live_pane_bool(HostSessionRef::tmux(name))`)를 추가하고, 호출부는 1줄 대 1줄, import는 `use crate::services::{session_host::legacy_collapse::{..}, tmux_diagnostics::{..}}` 중첩으로 줄 수를 같게 한다 (b) `tmux_diagnostics::tmux_session_exists`/`tmux_session_has_live_pane` **본체**를 호스트 경유로 바꾼다(거대 파일 무편집, 전 소비자 자동 경유, 사이트별 가시성 없음) (c) r3 식 그대로 + 거대 파일을 분해해 여유를 만든다(범위 밖) | **(a).** 사이트에 `legacy_collapse` 이름이 남아 S1 때 찾을 수 있다. 헬퍼 2개는 "같은 방향, 이름만" 원칙(r3 D12)을 지킨다. 헬퍼는 H3에서 먼저 넣어 H3 사이트(3-1·3-7)에도 같은 이름을 쓴다. (a)에서 import가 +1줄로 판명되면 **(b)를 claude/codex/qwen 세 import에만 한정해 적용하지 않고**, 대신 H5를 (b)로 전환할지 코디네이터가 다시 판정한다(자동 폴백 금지). |
| **D3** | `recovery_engine.rs`의 비-unix 스텁 보존 방법 | (a) `:20-21` import를 `#[cfg(unix)] use …::legacy_collapse::tmux_live_pane_bool;`로 바꾸고, `:221-224` 스텁 이름을 `tmux_live_pane_bool`로 바꾼다. `tmux_probe.rs:6,11`은 그 이름을 호출한다 (b) `tmux_probe.rs` 안에 cfg 분기 (c) 스텁을 버린다(Windows 동작 변경) | **(a).** 스텁 반환(false, 무프로세스)과 unix 경로(같은 `has_live_pane`)가 모두 보존되고, 편집은 `recovery_engine.rs` 2~4줄로 끝난다. D2-a 헬퍼 이름을 쓰므로 D2와 짝을 이룬다. (c)는 금지다. |
| **D4** | H4 send_keys 치환 시 로그 필드 손실(`status=`, failed/error 구분) | (a) 받아들인다: `Ok(Ok(Confirmed))→true`, `Ok(Err(HostError::Transport(d)))→warn(detail=d) false`, join 오류는 그대로 둔다. PR 본문에 "로그 문구 변화, bool 동작 0"을 명시한다 (b) send_keys는 H4에서 빼고 `pane_pid→execution_pid`만 한다 | **(b).** 동작 0 PR에 관측 가능한 로그 변화를 섞지 않는다. send_keys 이전은 `HostError`에 종료 코드를 싣는 변경(H1 모델 확장)과 함께 H6/P2에서 한다. H4는 1파일 수 줄로 줄어든다. H4 자체를 H3에 흡수할지도 코디네이터가 판단한다. 흡수하면 H3 prod 9파일로 cap 여유는 충분하다. |
| **D5** | r3 부록 A에 없던 claude/codex 클로저 4곳과 `codex.rs:1613` pane_pid | 클로저 포함 여부 / pane_pid 포함 여부 | **클로저는 포함**(같은 bool 래퍼 1:1이고, 빼면 import 정리가 불가능해 D2 순증 0이 깨진다). **pane_pid는 보류**(거대 파일 import가 늘고, H4 D4와 같은 로그·타입 판단을 따른다). |
| **D6** | 완료 디렉터리 안인데 r3 부록 A에 없는 `rebind_runtime.rs:482`(클로저 bool 래퍼), `terminal_watcher.rs:37`(함수 포인터) | H3 포함 / 예외 목록 | **rebind_runtime은 H3 포함**(1:1, cross-OS에서 이 파일은 cfg 없이 전체 경로 호출을 쓰므로 D3 문제가 없다 [추정: 해당 함수의 cfg는 구현 시 확인]). **terminal_watcher는 예외**(S3, 시그니처 변경). |

---

## 5. 리뷰 집중 항목 (선기재)

- **R-1 경계 시각(프로브 재시도·timeout 보존)**
  - `tmux_probe.rs`: 첫 시도 + 재시도 2회, `recovery_retry_backoff(attempt)` sleep이 호출 순서와 함께 그대로 남는다. has-session 3초 바운드(`tmux.rs:130`)도 그대로다.
  - `liveness.rs:18-27`: 10s `tokio::time::timeout`과 `unwrap_or(Ok(false)).unwrap_or(false)`가 문자 그대로 유지된다.
  - `tmux_diagnostics.rs:24-30`: 10s timeout과 `unwrap_or(Ok(true)).unwrap_or(true)` 유지. 내부 ProbeFailed는 **false**다(S3b 전까지).
  - `has_live_pane_bool` 경로는 2초 바운드가 없는 `list-panes`를 그대로 쓴다(`tmux.rs:880-898`). 이것을 `pane_liveness`나 `liveness_within`으로 "개선"하면 반려한다.
- **R-2 동시성(spawn_blocking/timeout 래핑 보존)**: `live_agent_recovery.rs:197-204`, `post_stream_exit.rs:110,132`(블록 안의 `:188` 재확인 포함), `tmux_runtime.rs:175,555`의 `spawn_blocking` 클로저 경계가 그대로인지 본다. 호스트 호출이 클로저 **안**에 있어야 한다. `&'static dyn` 호스트나 `TmuxHost` 유닛 구조체는 `Send`라 move 캡처가 바뀌지 않는다. 캡처하는 `String` 소유권 변화도 없어야 한다.
- **R-3 옛 형식 복원(bool 붕괴 극성)**
  - 존재 bool은 `probe_failed_to_missing`(= `==Present`) **한 방향만** 쓴다. `!= Missing`, `probe_failed_to_present`, 새 헬퍼가 `ProbeFailed→true`로 가는 것은 전부 반려한다.
  - `post_stream_exit.rs:133,188`의 `exists && !live`는 `exists` 쪽 ProbeFailed→false이므로 kill하지 않는(보존) 방향이다. 극성이 뒤집히면 **kill이 늘어나는 파괴 방향**이다.
  - `live_agent_recovery`의 tmux 분기는 `dead_only_if_dead_or_absent`(ProbeError→false=보존)다.
- **R-4 문서로만 선언한 가드**: "동작 0"을 주석이나 PR 문장으로만 주장하지 않는다. 다음은 모두 **시험 또는 명령 결과**로 붙인다: `probe_tmux_session_exists` 4행 진리표 시험, D2-a 헬퍼 동치 시험, D3 Windows 스텁은 `check_fast_cross_os` 실행 결과(선택만이 아니라 실제 잡 PASS 링크), 거대 파일 LoC 측정 출력, E-1/E-2 명령 출력. "H2가 나중에 잡는다"는 근거로 쓰지 않는다(비활성).
- **R-5 잘못된 0/PASS**
  - (i) `ProbeFailed`가 `Missing`이나 `Present`로 바뀌는 극성 변화(R-3).
  - (ii) `ProbeError→DeadOrAbsent`. `liveness_within`의 공백→ProbeError와 `pane_liveness`의 공백→DeadOrAbsent가 서로 뒤바뀌는지 본다.
  - (iii) E-1 표의 "0"이 grep 패턴 누락(글롭 상속 맨이름, 함수 포인터) 때문에 생긴 0인지 본다. 패턴은 이름 철자 기준이므로 맨이름도 잡히지만, D2-a 헬퍼 이름을 패턴에 넣지 않으면 증가분이 0으로 보인다.
  - (iv) `library_sweep` PASS가 대상 모듈을 실제로 실행했는지 본다(잡 로그에서 `recovery_engine::`/`claude::` 시험 이름 확인). `high-risk-recovery`가 선택되더라도 provider·session_host 모듈 시험은 돌지 않는다(r2 H1/H5 명시).
  - (v) D1 제외 사이트가 "치환 완료"로 집계되지 않았는지 본다.

---

## 부록. 사실/추정 구분 요약
- **[사실]** §0-1~0-3, §1 표의 현재 좌표 전부, §1.5 grep diff(추가 6, 삭제 0), 거대 파일 prod LoC 5개, `giant_file_progress.py` 판정 로직(`:300-305`, `:563-567`, `:326-329`), discord/tmux.rs 기한 2026-10-31, `recovery_engine.rs` cfg 스텁, `model.rs` 역매핑 부재, `#6338` diff 범위, 파괴 래칫 파일별 개수, CI 잡 줄 번호(`check_fast :688`, `check_fast_cross_os :749`, `high-risk-recovery :1096`, `library_sweep :1457`), `pr_cap_prod.py` 계수 규칙.
- **[추정]** rustfmt 결과 줄 수(D2 중첩 import 100열 경계, discord/tmux.rs import 재포장 후 줄 수), PR별 추정 규모, `rebind_runtime.rs:482` 포함 함수의 cfg, 선택 잡 목록(필터 대조는 했지만 실제 선택은 PR에서 확인).
- 실행 로그: `git worktree add --detach /private/tmp/adk-i5340-host c4c00cae1`; `git grep`/`git diff`/`sed`/`comm`; 읽기 전용 python(`giant_production_loc`, `check_hotfile_ratchet.py` 출력만). 작업 트리에 쓴 파일은 없다(`PYTHONDONTWRITEBYTECODE=1`; `check_hotfile_ratchet.py`는 판정 출력만 한다).

LANE_DONE i5340 host-delta
