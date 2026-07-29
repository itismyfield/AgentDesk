# design-4712 — `tmux_watcher.rs` 분해 슬라이스 (릴레이 안정화 병목 해소)

작성 2026-07-29. 설계 에이전트(Claude, read-only)의 산출물을 오케스트레이터가 파일로 옮긴 것.
설계 에이전트는 Write 도구가 없어 직접 쓰지 못했고, **아래 사실관계는 오케스트레이터가 리포에서 직접 재확인함**(§0.1).

> **상태: 카운터리뷰 완료 — `VERDICT: ISSUES`. 아래 §-1의 FLAWED 반영 전 구현 착수 금지.**
> 카운터모델(gpt-5.6-luna, fresh) 적대 설계 리뷰 2026-07-29.

---

## -1. 카운터리뷰 FLAWED 반영 (구현 전 필수)

### F1 — **캡처가 5개가 아니라 6개다. 빠뜨리면 컴파일이 안 된다.** (오케스트레이터 직접 확인)

`tmux_watcher.rs:3501`:
```rust
if false && pct >= ctx_cfg.compact_pct && !is_prompt_too_long && compact_cooldown_ok {
```
`is_prompt_too_long`은 `:580`/`:617`/`:668`에서 흐르는 **외부 바인딩**이다. `if false &&`로 비활성화돼 있어도 Rust는 이 참조를 컴파일한다. **§3 S-A의 Context 구조체는 6필드여야 한다.**

### F2 — §2.2 예약형 래칫 다운: 기계적으로는 통과하나 **정책상 우회**

카운터리뷰가 스크립트를 직접 읽고 확인한 것:
- hotfile checker는 낮아진 cap을 정상 처리 (`check_hotfile_ratchet.py:133-153`)
- admission은 `cap > prior_cap`일 때만 raise 판정 (`ratchet_admission.py:178-183`)
- → 3571 → 3513에 admission 이벤트 불필요. **여기까지는 설계가 맞다.**

**그런데 FLAWED인 지점:**
1. checker 자신의 메시지가 "ceiling 아래면 **현재 측정값으로** 낮추라"고 한다 (`check_hotfile_ratchet.py:146-150`). 예약은 그 의도를 우회한다.
2. **만료 조건("#4973 머지 시 또는 4주")을 강제하는 checker가 없다.** 주석뿐이라 잊혀도 CI가 안 잡는다 → 이름만 다른 영구 캡 인상이 된다.
3. **admission count 해석이 틀렸다.** `ratchet_admission_history.toml:3-8`에 count는 metric별이 아니라 **파일별 누적**이라고 명시돼 있다. 실제: hotfile `count=2` 1건(`:64-70`), giant `count=1`(`:16-22`)과 `count=3`(`:72-78`). 즉 "hotfile 2 / giant 3"은 독립 횟수가 아니라 **파일 전체 시퀀스의 현재 count가 3**이다. (다음 admission이 4가 되어 `count > 3`으로 경고가 뜬다는 결론 자체는 유효 — `ratchet_admission.py:31`, `:267-285`.)
4. **R=60의 근거가 무효.** PR #4972의 net `+8`은 맞지만, #4973이 그 접근을 **무동작 판정**했다. 실패한 접근의 비용을 승인된 계약의 하한으로 쓸 수 없다.

**결정 필요(둘 중 하나):**
- (a) S-A 후 cap을 **실측값으로** 낮추고, #4973 루트 배선은 추가 분해 또는 **정식 admission**으로 처리한다 ← 정책 정합성 우선
- (b) 예약을 정책으로 허용하려면 checker에 **만료·owner·잔여량을 기계 검사하는 reservation schema**를 먼저 추가한다

**주의: (b)를 고르면 그 자체가 별도 선행 PR이다.** 어느 쪽이든 R은 #4972가 아니라 **승인된 #4973 설계의 실제 루트 배선을 재측정**해서 정해야 한다.

### F3 — §6.1 "테스트 있으면 하드 실패"는 절대 명제가 아님

`check_test_lane_coverage.py:100-114`, `:613-650`: 실패 조건은 `current - baseline`이다. 새 child의 테스트가 **기존 curated lane에 의해 완전히 선택되면** `new`가 아니다. 다만 baseline에 새 uncovered entry를 추가하는 건 여전히 불가(immutable reference의 subset만 허용, `:618-630`).

**설계가 결정하지 않은 것**: S-A의 토큰/POST/auto-compact/outbox 테스트를 **어디에 둘지**. → **기존 `tmux_watcher/tests.rs`에 둔다**로 확정한다. 새 child test module을 만들려면 curated lane 설계가 선행돼야 한다.

프로덕션 전용 child module 추가 자체는 `new`를 만들지 않는다(`:287-303`, checker가 수집하는 건 `#[cfg(test)] mod`) — **이 부분은 SOUND로 확인됨.**

### F4 — §5 "#4972를 닫아야 한다"는 과도

#3016은 **핫파일 동시 편집 금지**이지 해당 파일을 건드린 모든 OPEN PR을 닫으라는 규칙이 아니다. 필요한 것은 "mergeable 후보가 아님을 명시 + 브랜치가 더 이상 핫파일을 수정하지 않게 고정"이다.

> **오케스트레이터 주**: 리뷰 도착 전에 이미 #4972를 닫았다. 코멘트에 무동작 판정 근거·브랜치 보존·reopen 가능을 명시했으므로 리뷰가 요구한 실질(superseded 고정 + 증거 보존)은 충족된다. 되돌릴 수 있는 결정이라 되돌리지 않는다.

### F5 — §0 "스코프가 정정됐다"는 과소기술

세 산출물이 **서로 다르다**: ① #4712 **body**는 watcher root를 후보에 **포함**한다 ② #4712 최신 **댓글**은 #4229 전담이라 배제한다 ③ **registry**는 `#4712`로 태그한다. → **구현 전 registry + #4712 body/acceptance를 먼저 정합화**해야 한다. "이미 정정됨"이 아니다.

또한 #4229 원래 W7은 missing-inflight fallback과 token tail을 **하나의 W7**로 묶는다. S-A가 token tail을 먼저 떼는 것은 7/24 스카우트의 refinement이지 원 문서의 문자 그대로의 단계가 아니다. **"실행 순서만 추가했다"는 서술은 부정확 — W7을 두 독립 슬라이스로 재분할하는 결정이다.**

### F6 — W3~W5의 drop-order를 **명시적 acceptance gate**로 승격

`Drop` 시점은 바이트 역치환으로 검출되지 않는다. 다음의 **상대 순서**가 추출 전후 동일한지 별도 검증 항목으로 고정한다: heartbeat stop → durable commit → confirmed-end advance → lease release → `slot_guard.release()` → post-commit lifecycle.

### F7 — S-A "최저 위험"은 유지하되 side-effect를 축소 서술하지 말 것

제어흐름은 0이 맞지만 블록은 여러 외부 side effect를 수행한다: ADK session status POST(`:3427-3458`), shared core lock 획득(`:3433-3437`), context threshold 조회(`:3460-3463`), KV/PG cooldown 조회(`:3466-3489`), PG update + outbox enqueue(`:3519-3550`).
→ **acceptance에 `post_adk_session_status`와 `run_post_stream_exit` 사이의 await ordering 보존을 명시적 assertion으로 추가한다.**

### 카운터리뷰가 SOUND로 확인해준 것 (재검증 불필요)

- `3425–3552`는 정확히 128줄, 외부 `continue`/`break`/`return` **0개**, `?` 연산자 **없음** → `Outcome` enum 불필요
- S-A는 `slot_guard.release()`(`:2885-2887`) **이후**에 위치 → `RelaySlotGuard` drop 순서와 **무관**
- `liveness.rs`의 contract 앵커 무영향, `MIN_CONTRACT_ANCHORS = 20` 정확(현재 실측 38개)
- PR #4972가 실제로 `tmux_watcher.rs` + 3개 래칫 매니페스트를 수정하는 것은 사실
- #4229에 W1–W8 설계가 **실재**하며 S-A/S-B/S-C 구조가 대체로 일치
- `docs/relay-state-contract.md`의 배달 권위 불변식을 S-A가 직접 깨는 지점은 **발견되지 않음**

---

---

## 0. 스코프 정정 — 이 파일의 주인은 #4712가 아니라 **#4229**다

설계 착수 지시가 "#4712 = tmux_watcher.rs 분해"였는데 **틀렸다.** 이슈 본문/댓글이 명시적으로 배제한다:

| 출처 | 내용 |
|---|---|
| #4712 댓글 | *"이 이슈의 실행 대상은 registry의 `decompose_issue=#4712` 28건 중 **#4229가 전담하는 `tmux_watcher.rs`를 제외한 27건**"* |
| #4712 댓글 표 | `src/services/discord/tmux_watcher.rs` → *"**#4229 W1–W8 전담; #4712에서 중복 분해 금지**"* |
| #4712 댓글 | *"`tmux_watcher.rs` \| registry umbrella만; **분해 구현 없음** \| **#4229 W1→W8**가 root behavior-preserving extraction 전담"* |
| `scripts/giant_file_registry.toml:636` | `decompose_issue = "#4712"` ← **모순** |

**정합성 액션(별건)**: registry의 `decompose_issue`와 `ratchet_admission_history.toml`의 태그를 `#4229`로 고치거나, #4712에 "이 필드는 umbrella 포인터"라고 기록해야 한다. 지금 두 산출물이 서로 다른 말을 하고 있고, **이번 디스패치가 잘못 스코프된 원인이 정확히 이것이다.**

**따라서 아래 슬라이스는 새 분해가 아니라 #4229의 기존 W1–W8 설계를 재측정한 것이다.** 새로 추가한 판단은 **실행 순서**뿐이고, 그마저 #4229의 2026-07-24 노트("토큰 tail은 별도 `terminal_token_update.rs`로 먼저 떼면 W7 자체가 더 작고 안전하다")를 따랐다.

### 0.1 오케스트레이터 직접 재확인 (2026-07-29)

| 주장 | 확인 결과 |
|---|---|
| `tmux_watcher/**` 네임스페이스 캡 = **700** (1000 아님) | ✅ `scripts/audit_maintainability_config.toml:35` |
| hotfile cap 3571 / giant baseline 3571 / 실제 3569 → **여유 2** | ✅ `hotfile_ratchet.toml:123`, `audit_maintainability_giant_baseline.toml:143`, `wc -l` |
| #4712가 tmux_watcher.rs를 #4229에 위임 | ✅ 댓글 본문 직접 확인 |
| registry는 `#4712`로 태그 | ✅ `giant_file_registry.toml:636` |
| **PR #4972 OPEN + 같은 핫파일 + 3개 래칫 매니페스트 전부 수정** | ✅ files: `tmux_watcher.rs`, `hotfile_ratchet.toml`, `audit_maintainability_giant_baseline.toml`, `ratchet_admission_history.toml`, `test_lane_coverage_baseline.txt`, `outbound/delivery_record.rs`, `tmux_watcher/commit_decisions.rs`, `justfile`, `tests/test_fast_check_ci_wiring.py` |

### 0.2 내가(오케스트레이터) 디스패치에 넣은 틀린 전제 3개

1. **"#4912가 `tmux.rs` → `tmux_watcher/discrete_trigger_marker.rs`로 171줄 옮긴 선례"** — main 기준으로는 거짓. 그 파일은 `8cb56b09a feat(relay): add discrete machine trigger markers (#4804)`가 만든 것이다. 다만 **#4912 브랜치가 실제로 그 이동을 하고 있는 것은 사실**(diff 실측: `tmux.rs -171`, `discrete_trigger_marker.rs +171`)이라 설계 에이전트는 미머지 브랜치를 볼 수 없었을 뿐이다. → 지시할 때 "미머지 브랜치"라고 명시했어야 했다.
2. **"#4953에서 모듈 경로 변경이 CI 레인 분류를 깨뜨린 실사고"** — 서술이 부정확. #4953은 `[ci-red]` 트래커였고 수리는 테스트 레이스였다. **다만 설계 에이전트가 진짜 위험을 대신 찾아냈다** — §6.1의 `test_lane_coverage_baseline.txt` 게이트가 내가 말한 것보다 **더 강한** 하드 실패다.
3. **"신규 모듈 1000줄"** — 아니다. **700**이다.

---

## 1. 실측 현재 상태

```
src/services/discord/tmux_watcher.rs                     3569 raw
scripts/hotfile_ratchet.toml:123                         3571  (raw cap)   → 여유 2
scripts/audit_maintainability_giant_baseline.toml:143    3571  (prod cap)  → 여유 2
scripts/audit_maintainability_config.toml:35             tmux_watcher/** = 700
```

이 파일은 **prod == raw**다. 모든 `#[cfg(test)]`가 자식 파일에 있다(`#[path = "tmux_watcher/tests.rs"] mod tests;` @ `:3567-3569`). 따라서 **두 매니페스트가 항상 같은 delta로 함께 움직여야 한다.**

구조:
- `1–192` import + 27개 `#[path]` 자식 `mod` 선언
- `186–231` `RestoredSeedDisposition` + `watcher_stream_seed_after_restored_seed_discard`
- `235–3565` `tmux_output_watcher_with_restore` — **3331줄, 파일의 93.3%**
- `431–3553` `'watcher_loop: loop`

**모듈 경로 주의**: `tmux.rs:2720`이 `#[path = "tmux_watcher.rs"] mod tmux_watcher;`라서 논리 경로는 `services::discord::tmux::tmux_watcher`다 (`services::discord::tmux_watcher`가 **아님**).

### 루프 본문 블록 맵 (재측정, #4229 스테이지 라벨)

| 스테이지 | 라인 | 크기 | 외부 제어흐름 | 캡처 표면 |
|---|---:|---:|---|---|
| poll prologue (완료, S2) | 432–487 | 56 | outcome match | — |
| collector (완료, S4) | 489–586 | 98 | outcome match | — |
| no-result exits (완료, S6) | 595–648 | 54 | outcome match | — |
| abort exits (완료, S1) | 650–696 | 47 | outcome match | — |
| **W1** pre-emit guard | 698–1017 | **320** | `continue` ×3 | ~14 in, `full_response` write-back |
| **W2** terminal preflight | 1019–1450 | **432** | `continue` ×3 | ~25 out (최대 팬아웃) |
| **W3** relay plan + lease | 1452–1957 | **506** | `continue` ×1 | ~25 out; `RelaySlotGuard` lifetime |
| **W4** emission + rewind | 1959–2335 | **377** | `continue 'watcher_loop` ×2 | ~15 mut locals + RAII |
| **W5** commit / UI / frontier | 2336–2897 | **562** | 없음 | 광범위 mutation, await 순서 의존 |
| **W6** post-commit lifecycle | 2899–3279 | **381** | 없음 | 광범위 read 캡처 |
| commit epilogue wiring (완료, S7) | 3280–3333 | 54 | outcome match | — |
| **W7a** missing-inflight fallback | 3335–3423 | **89** | `break 'watcher_loop` ×1 | 작음 |
| **W7b** token / auto-compact tail | 3425–3552 | **128** | **없음** | **읽기 전용 5개, write-back 0** |
| **W8** bootstrap | 250–430 | 181 | — | 장수명 상태; 반드시 마지막 |

추출 가능 총계 2795 (+ bootstrap 181).

---

## 2. 헤드룸 문제의 정확한 답

### 2.1 함정: 실측값으로 래칫을 내리면 헤드룸이 0이 된다

`check_hotfile_ratchet.py`는 `actual <= cap`을 강제한다. 프로젝트 관행(#4229 S-슬라이스 전부)은 `cap = measured`로 "성과를 고정"하는 것이다. **그렇게 하면 슬라이스 N 후 헤드룸이 정확히 0이고 #4961/#4973은 여전히 막힌다 — 분해만으로는 아무것도 안 풀린다.**

역사적으로 프로젝트는 캡을 **올려서** 풀어왔다(최근 이틀 만에 `3550 → 3561 → 3571`). 지시가 그걸 금지한 건 옳다: `ratchet_admission_history.toml`이 이 파일에 대해 이미 `count = 2`(hotfile) / `count = 3`(giant)를 기록 중이고 `ADMISSION_WARN_THRESHOLD = 3`이라 **다음 giant admission에서 `WARN: RATCHET ADMISSION COUNT EXCEEDED`가 뜬다.**

### 2.2 준수 메커니즘: 예약형 래칫 다운

각 래칫 다운 시점에

```
new_cap = measured_after_slice + R      (R = 60)
```

이건 현재 3571 대비 **모든 경우에 큰 감소**다 — 절대 증가가 아니므로 "줄이는 방향만" 규칙을 문자 그대로 지키고 `[[admission]]` 이벤트도 생기지 않는다. `R`은 #4961/#4973을 위한 **명시적·문서화된 예산**이다.

**R = 60 산정 근거.** PR #4972(Phase B 시도, OPEN)가 Phase B 루트 비용의 유일한 실측이다 — `tmux_watcher.rs`에 **+10 / −2 = net +8**, `:2332` soft-terminal `continue`에 `commit_proven_soft_terminal_backlog(...)` 호출 하나. #4973의 전체 계약은 같은 호출 지점에 partial-prefix 처리(계약 3), START-continuity(계약 4), non-destructive failure(계약 5)를 추가한다. 60은 관측된 하한의 약 7배이면서 파일의 1.8% 미만이다.

**예약은 시한부다**: #4973 구현이 머지되는 즉시, 또는 4주 후 중 빠른 쪽에 measured로 재래칫. **이 문장을 매니페스트 주석에 넣어야** 실패 지점에서 발견된다.

> ⚠️ **카운터리뷰 최우선 검토 대상**: 이 "예약형 래칫 다운"은 형식상 감소지만 실질은 헤드룸 예약이다. 래칫 관행을 게이밍하는 것 아닌지, 시한부 조항이 실효성 있는지 적대적으로 검토할 것.

### 2.3 답: 헤드룸은 **슬라이스 1**에서 열린다

| 시점 | root raw | cap 설정 | 예약 | #4961/#4973 들어가나? |
|---|---:|---:|---:|---|
| 현재 | 3569 | 3571 | 2 | **NO** |
| **S-A (W7b)** | **~3453** | **3513** | **60** | **YES** |
| S-B (W1) | ~3175 | 3235 | 60 | yes |
| S-C (W7a) | ~3100 | 3160 | 60 | yes |

**W7b 하나면 충분하다.** 128줄, **외부 제어흐름 0**, **가변 write-back 0**, 읽기 전용 캡처 5개. 이미 바이트 동일 듀얼리뷰를 통과한 #4229 S5(`post_stream_exit.rs`, 254줄)보다 **엄격히 더 안전**하다. 게다가 #4961이 편집하는 영역(`:2332`, `:2790`)과 **위상적으로 분리**돼 있어 텍스트 충돌이 불가능하다.

**슬라이스 없이 지금 당장 가능한 추가 해제**: #4973의 non-root 작업 — `outbound/delivery_record.rs`의 receipt 스키마, `tmux_watcher/streaming_status_tick.rs`의 POST-success 기록, `health/relay_auto_heal.rs`의 redrive 진입 계약 — 은 **#3016 핫파일을 하나도 안 건드린다.** S-A와 **병렬로 오늘 착수 가능.**

---

## 3. 슬라이스 정의

모든 슬라이스 공통 계약(#4229/#4814/#4829에서 그대로 승계, 재발명 아님):

- **동작 변경 = 0.** 이동 블록의 역치환 바이트 비교. 토큰·문자열·분기·`.await` 순서 변경 금지. import 경로와 가시성만 달라질 수 있다.
- 추출 형태: `Context`(빌린 읽기) + `Locals`(소유 이동) + `&mut State`(write-back) + 모든 외부 `continue`/`break`를 매핑한 타입 `Outcome` enum.
- 게이트: `cargo fmt --all --check`, `cargo check --workspace --all-targets`, `cargo test --lib tmux_watcher`, `check_hotfile_ratchet.py`, `audit_maintainability.py --check`, `check_test_lane_coverage.py`, `check_agent_maintenance_docs.py`, `check_contract_symbol_refs.py`, `generate_inventory_docs.py`.
- 롤백: 단일 PR `git revert`. DB/사이드카/스키마/플래그 도입 없음.
- **중단 규칙**: 이동에 authority / dedup / lease / watermark / recovery **의미** 변경이 필요하면 멈추고 #4414로 넘긴다. "옮기면서 고치기" 금지.

### S-A — W7b: terminal token update + auto-compact tail ← **해제 슬라이스**

- **이동**: `tmux_watcher.rs:3425–3552` → 신규 `src/services/discord/tmux_watcher/terminal_token_update.rs`
- **정확한 내용**: 단일 `if let Some(tokens) = result_usage.map(|usage| usage.context_occupancy_input_tokens()) { … }` 문장. 내부: ADK 세션키 빌드, `shared.core` 락 하 `channel_name` 조회, `thread_channel_id` 파싱, `resolve_role_binding` → `agent_id`, `watcher_terminal_token_update_status(...)`로 `post_adk_session_status(...)`, auto-compact 임계 평가, pg cooldown-key `bind`/`execute`, `enqueue_outbox_best_effort` 🗜️ 알림.
- **신규 심볼**: `pub(super) async fn run_watcher_terminal_token_update(ctx: WatcherTerminalTokenUpdateContext<'_>)`
- **Context 구조체 (5필드, 전부 읽기 전용)**: `shared: &Arc<SharedData>`, `channel_id: ChannelId`, `tmux_session_name: &str`, `result_usage: Option<TokenUsage>`, `watcher_direct_terminal_idle_committed: bool`
- **Outcome enum: 불필요** — 3425–3552 grep 결과 `continue` **0**, `break` **0**, `return` **0**. `.await` 9개는 전부 내부이고 verbatim 이동으로 순서 보존.
- **신규 모듈 크기**: 128 + ~12 ≈ **~140–165줄**. 캡 700 대비 **4배 여유로 PASS.**
- **root delta**: −128 + ~12 = **−116** → **3569 → ~3453**
- **래칫**: hotfile `3571 → 3513`, giant `3571 → 3513`. 58 감소, **admission 이벤트 없음.** 주석에 예약 사유와 만료 조건 명기.
- **동반 편집**: `change-surfaces.md` 자식 모듈 항목 추가(S1–S8 항목 `:345-425` 미러링). `docs/generated/module-inventory.md`는 **untracked**(#4748에서 de-commit) — 로컬 재생성만, 커밋 금지.
- **검증**: 토큰 사용량 → `post_adk_session_status` 왕복, auto-compact 임계 교차(pg cooldown-key 경로 포함), outbox 🗜️ enqueue, `watcher_direct_terminal_idle_committed` 상태 판별자. 릴레이 경로 테스트 변경 없음.
- **위험도**: 가용 슬라이스 중 **최저**.

### S-B — W1: pre-emit guard coordinator

- **이동**: `698–1017` → `tmux_watcher/pre_emit_guard.rs`
- **내용**: 최종 pre-relay 재검사(`paused_now`/`epoch_changed_now`/`turn_delivered_now`/`deferred_monitor_ready`) → `should_suppress_relay_before_emit` arm(`:707`); `watcher_should_yield_to_active_bridge_turn` arm(`:763`); output-metadata 중복범위 가드(`:836`); `last_relayed_offset` strict-`<` dedup(`:854`); stale-resume 탐지+복구(`:909–1017`, `full_response`를 비움).
- **Outcome enum**: `PreEmitGuardOutcome::{Continue, Proceed}` — 외부 `continue` 3개가 `Continue`로 축약.
- **write-back**: `full_response`가 `&mut State`에 있어야 함(stale-resume arm이 비움). `last_relayed_offset`, monitor-token locals도.
- **신규 모듈 크기**: 320 + ~80 ≈ **~400**. 캡 700 PASS.
- **root delta**: −278 → **~3175**; 래칫 `3513 → 3235`
- **검증**: pause/epoch 레이스, active-bridge yield, 교체 워처 중복 범위, stale-resume 자동 재시도 및 rebind-origin 특성화. **외부 `continue` 매핑 수가 정확히 3인지 assert.**

### S-C — W7a: missing-inflight liveness fallback

- **이동**: `3335–3423` → `tmux_watcher/post_commit_observation.rs`
- **내용**: `missing_inflight_after_session_bound_delivery`, `probe_tmux_session_liveness` 게이트, `recent_turn_stop_for_watcher_range`, `terminal_cleanup_committed` 프로브, `missing_inflight_fallback_observation` 플랜과 3개 arm(recent-stop 억제 / dead-tmux drain + `handle_tmux_watcher_observed_death` + `break` / degraded 표기).
- **Outcome enum**: `{BreakWatcherLoop, Fallthrough}` — `break 'watcher_loop` 1개.
- **신규 모듈 크기**: 89 + ~40 ≈ **~130**. PASS.
- **root delta**: −75 → **~3100**; 래칫 `3235 → 3160`
- **검증**: dead/live/ambiguous tmux, recent-stop 억제, committed-placeholder 정리, `#4800`/`#4810` liveness 회귀.

### S-D 이후 — #4229 W-트랙 잔여 (변경 없음, 연기)

**#4961/#4973 목표에 불필요**하므로 항목 수준까지 상세화하지 않는다. #4229의 7/24 스카우트가 이미 경계를 확정했다. 순서용으로만 기록:

| 슬라이스 | 블록 | 라인 | 예상 신규 모듈 | >700? | root 후 |
|---|---|---:|---:|---|---:|
| S-D = W2a | 1019–~1230 preflight prep/reclaim | ~212 | ~280 | no | ~2910 |
| S-E = W2b | ~1232–1450 suppression | ~219 | ~290 | no | ~2720 |
| S-F = W3a | 1452–~1800 plan/decision | ~350 | ~450 | no | ~2410 |
| S-G = W3b | ~1802–1957 lease acquire/heartbeat | ~156 | ~220 | no | ~2275 |
| S-H = W4 | 1959–2335 emission + rewind | 377 | ~480 | no | ~1940 |
| S-I = W5a | 2336–~2412 commit/gate | ~77 | ~130 | no | ~1875 |
| S-J = W5b | ~2414–~2684 UI/completion chrome | ~271 | ~350 | no | ~1635 |
| S-K = W5c | ~2686–2897 frontier/lease/slot | ~212 | ~280 | no | ~1450 |
| S-L = W6a | 2899–~2940 prep | ~42 | ~90 | no | ~1415 |
| S-M = W6b | ~2942–3279 lifecycle/dispatch | ~338 | ~430 | no | ~1120 |
| S-N = W8 | 250–430 bootstrap + closing | 181 | ~250 | no | **~1000, 재측정** |

**크기 게이트 판정: 계획된 어떤 모듈도 700을 넘지 않는다.** 넘길 뻔한 둘(W3 506→~630, W5 562→~700)은 #4229의 7/24 스카우트가 이미 W3a/W3b, W5a/b/c로 선분할했다.

**W8이 <1000에 확실히 도달하지는 않는다.** 단순 산술(3569 − 2795 = 774)은 슬라이스별 배선 비용을 무시한다. 11개 슬라이스에서 관측된 ~13% 오버헤드를 반영하면 잔여는 ~1000–1120이다. #4229 W8 스펙도 "약속하지 말고 재측정하라"고 한다. **#4961 해제 작업의 일부로 이 파일의 de-giant를 계획하지 마라 — 그건 목표가 아니고, 그 둘을 뒤섞은 것이 과거에 이 파일 분해가 멈춘 이유다.**

---

## 4. 순수 이동 vs 로직 변경

**S-A … S-N 전부 순수 이동. 동작 delta = 0. 이 계획의 어떤 슬라이스도 로직을 바꾸지 않는다.**

로직 변경은 #4961/#4973 자체 PR로 완전히 격리되며, 예약된 헤드룸에 착지한다:

- **L1 (#4973, non-root)**: streaming-POST 성공 시점의 durable receipt 스키마 + write. 파일: `outbound/delivery_record.rs`, `tmux_watcher/streaming_status_tick.rs`. **non-hot, S-A와 병렬로 즉시 착수 가능.**
- **L2 (#4973, non-root)**: redrive 진입점 계약 — lookup-before-rewind, partial-prefix, START-continuity, non-destructive failure. 파일: `health/relay_auto_heal.rs`. non-hot.
- **L3 (#4961, root)**: `:2332` soft-terminal 호출 지점. **hot, S-A 예약 필요, 직렬.**

격리 근거: #4973은 PR #4972가 **교차모델 리뷰 3라운드를 쓰고도 무동작 판정**을 받았다고 기록한다(잘못된 증거원을 소비함). 그 일부라도 "순수 이동" 슬라이스에 접으면 **바이트 비교 리뷰가 불가능해지는데, 그게 이 파일의 이전 8개 슬라이스를 안전하게 지킨 유일한 기법이다.**

---

## 5. 핫파일 동시성 판정

**루트를 건드리는 모든 슬라이스는 엄격히 직렬.** `tmux_watcher.rs`는 `check_hotfile_ratchet.py`의 `REQUIRED_HOTFILES`이자 #3016 동시편집 금지 4파일 중 하나다. **"disjoint" 슬라이스끼리도 병렬 불가** — 모든 슬라이스가 `hotfile_ratchet.toml:123`과 `audit_maintainability_giant_baseline.toml:143`이라는 **단일 물리 라인**(전체 주석 이력을 담은)을 편집하기 때문이다.

**병렬 가능한 것**: 이미 추출된 자식 모듈 내부 작업(전부 non-hot) / #4973 L1+L2 / 다른 27개 #4712 항목(`session_relay_sink.rs`, `turn_finalizer.rs` 제외 — 별도 hot).

**즉시 블로커: PR #4972가 OPEN이고 `tmux_watcher.rs` + 3개 래칫 매니페스트 전부를 건드린다.** #3016상 이게 열려 있는 동안 어떤 슬라이스도 시작할 수 없다. #4973이 무동작 판정을 기록했으므로 **S-A 전에 #4972를 닫아야 한다.**

**레인 가용성은 7/24 스카우트 가정보다 훨씬 좋다** — 당시 큐(#4860 → #4533 → #4639 → #4259 R4 → #4759) 중 **#4533/#4639/#4259/#4759 전부 CLOSED.** #4860만 OPEN. 레인은 사실상 비어 있다.

---

## 6. Adversarial self-check

### 6.1 테스트가 모듈 경로에 묶여 있나? — **그렇다. 경고가 아니라 하드 CI 실패다**

`scripts/check_test_lane_coverage.py` + `scripts/test_lane_coverage_baseline.txt`(683 항목, 그중 **24개가 `services::discord::tmux::tmux_watcher::` 하위**)가 두 조건을 강제한다:

```python
growth = baseline - reference_baseline   # 불변 git 커밋 기준 → 비어 있어야 함
new    = current - baseline              # 새로 미커버된 모듈 → FAIL
stale  = baseline - current              # 사라진 모듈        → FAIL
```

결과: **추출 모듈이 인라인 `#[cfg(test)] mod tests`를 가지면 그 논리 경로가 "미커버"가 되고, baseline에 추가할 수도 없어(불변 레퍼런스 대비 subset-only) CI가 하드 실패한다.**

완화책(선호 순):
1. **프로덕션 코드만 추출.** S-A는 자명하게 충족(3425–3552에 테스트 코드 없음). S-C도 동일.
2. 유닛 테스트 재배치가 불가피하면 **기존** `tmux_watcher/tests.rs`(baseline 429행)에 넣는다. **새 `*_tests.rs` 형제 파일 생성 금지** — `single_message_footer_tests` 등은 grandfathered 항목이라 신규는 금지된 growth다.
3. 또는 `justfile`의 `test-non-pg` 레시피에 큐레이션 레인 필터 추가(선례: justfile:64-70의 `#4259` 블록).

**모든 슬라이스 PR 전에 `python3 scripts/check_test_lane_coverage.py` 실행.**

한편 `.github/workflows/ci-pr.yml`에서 모듈 경로에 묶인 `cargo test` 필터 2개는 `services::session_forwarding`(:587)과 `high_risk_recovery::`(:644)다. **둘 다 tmux_watcher와 무관.**

### 6.2 `#[path]`가 논리 모듈 경로에 영향을 주나?

**아니다 — 그래서 이 계획이 안전하다.** `#[path]`는 *파일* 해석만 바꾸고 논리 경로는 `mod` 선언 체인에서 나온다. 인트리 증거: `tmux_watcher/liveness.rs`의 논리 경로는 `services::discord::tmux::tmux_watcher::liveness`(baseline 412행)다.

따라서 `tmux_watcher.rs` 안의 새 `#[path = "tmux_watcher/terminal_token_update.rs"] mod terminal_token_update;`는 `services::discord::tmux::tmux_watcher::terminal_token_update`가 된다. **`cargo test --lib tmux_watcher`가 계속 매칭된다.** `check_test_lane_coverage.py`도 `#[path]` 별칭을 명시적으로 해석(`PATH_ATTR_RE`)하므로 rustc와 판정이 일치한다.

### 6.3 contract-symbol 게이트 — 경로 결합 참조 2개 보존 필수

`tmux_watcher/liveness.rs:666-691`의 `relay_state_contract_refs` 모듈은 **컴파일돼야 하는 모듈 경로 표현식**을 앵커로 갖는다:

```rust
use super::super::loop_poll_prologue::poll_watcher_output_or_continue as _;
use super::super::tmux_output_watcher_with_restore as _;
use super::super::terminal_commit_epilogue::run_terminal_commit_epilogue as _;
use super::reacquire_watcher_inflight_for_active_stream as _;
use crate::services::discord::tmux::advance_watcher_confirmed_end as _;
```

**W8 관련 하드 제약**: `tmux_output_watcher_with_restore`는 **`tmux_watcher.rs`의 직속 자식으로 남아야 한다.** W8 "closing pass"가 이걸 옮기거나 이름을 바꾸면 `check_fast` 레인에서 컴파일 실패.

추가로 `check_contract_symbol_refs.py:104-113`의 `REFERENCE_SOURCE_MODULES`와 `ci-pr.yml:190-193`의 `relay_contract` 경로 필터가 둘 다 `liveness.rs`를 하드코딩하고 CI 주석이 *"Keep this list in sync"*라 한다. 계약 앵커 심볼을 `liveness.rs` 밖으로 옮기면 **둘 다** 같은 PR에서 갱신해야 한다. `MIN_CONTRACT_ANCHORS = 20`은 절대 낮추면 안 되는 하한(#4269). **S-A~S-C는 `liveness.rs`를 건드리지 않는다.**

### 6.4 가시성 변경이 필요한가?

기계적으로 필요하고, **"순수 이동"이 조용히 API를 넓힐 수 있는 유일한 지점**이다. S-A는 `run_watcher_terminal_token_update`를 `pub(super)`로 — 호출에 필요한 최소. **리뷰 규칙: 어떤 심볼도 `pub(super)`/`pub(in crate::services::discord)`보다 넓어지지 않는지 assert.**

#4229 S4가 교훈이다 — `macro_rules!` **정의부 위생** 문제가 13× `E0425`를 만들었고 리뷰어와 구현자 둘 다 놓쳐 rustc 최소 재현까지 갔다. **모든 후보 블록에 대해 이동 전 `macro_rules!` 사용 여부 확인.** (3425–3552는 없음.)

### 6.5 change-surfaces 문서 게이트가 걸리나?

`check_agent_maintenance_docs.py:111-115`의 `DOC_TOUCH_RULES`는 `tmux.rs`를 키로 쓰지 `tmux_watcher.rs`가 아니다. 따라서 `tmux_watcher.rs`만 건드리면 강제되지 **않는다.** 다만 `check_change_surface_line_counts`가 그 문서에 이름 붙은 frozen 경로를 `module-inventory.md`와 교차검사하고 **ghost 항목에서 하드 실패**한다(#3036). S1–S8 선례(change-surfaces.md:345-370)대로 자식 모듈 노트를 추가하라 — **비용은 한 문단, 실패 모드는 red main.**

### 6.6 이 계획이 못 덮는 것

- **`generate_inventory_docs.py` 드리프트.** `module-inventory.md`는 untracked(#4748에서 de-commit). stale 로컬 사본에서 인용한 숫자는 전부 틀린다. **인용 전 반드시 재생성.** 7/24 #4712 스카우트가 정확히 이걸로 15줄 어긋났다.
- **예약이 영구화되는 것.** 재래칫되지 않는 60줄 예약은 이름만 다른 조용한 캡 인상이다. 구속하라: #4973 구현 머지 시 또는 4주 중 빠른 쪽. **그 문장을 매니페스트 주석에 넣어라.**
- **`RelaySlotGuard` / lease-heartbeat drop 순서 (W3/W4/W5).** 렉시컬 수명 RAII다. 감싸는 스코프가 바뀌는 verbatim 이동은 **토큰 하나 안 바꾸고 `Drop` 타이밍을 바꾼다.** `slot_guard.release()`(`:2887`)와 heartbeat stop(`:2727`)은 commit 및 frontier-advance 대비 정확한 상대 위치를 유지해야 한다. **바이트 비교로는 안 잡힌다 — 명시적 drop 순서 리뷰 필요.** S-A/S-C는 무관.

---

## 7. GO / NO-GO

### **GO — S-A(W7b) 즉시, 단 블로커 1개 해소 조건.**

**블로커(선행 필수)**: PR **#4972**가 OPEN이고 `tmux_watcher.rs` + `hotfile_ratchet.toml` + `audit_maintainability_giant_baseline.toml` + `ratchet_admission_history.toml`을 수정한다. #3016상 동시편집 금지이며 매니페스트 편집이 S-A와 라인 단위로 충돌한다. #4973이 #4972의 교차리뷰 판정을 **무동작**으로 기록. → **S-A 개시 전 #4972를 닫거나(또는 머지 후 supersede) 처리.** 유일한 진짜 블로커이며 절차적·당일 해소 가능.

**블로커는 아니지만 S-A PR에서 반드시 처리**:
- `check_test_lane_coverage.py` 실행 — 추출 모듈에 `#[cfg(test)] mod` **없어야** 함(§6.1)
- 캡을 `measured`가 아니라 `measured + 60`으로, 예약 사유와 만료를 매니페스트 주석에(§2.2)
- `change-surfaces.md` 자식 모듈 항목 추가(§6.5)

**GO — S-B, S-C**: S-A 후 같은 레인에서 직렬.

**조건부 GO — S-D … S-N**: 옳고 #4712/#4229의 giant-exit 목표에 필요하지만 **#4961/#4973 해제에는 불필요**하며 묶으면 안 된다. S-N(W8) 전에 재스카우트하고, <1000 도달을 미리 약속하지 마라(§3).

**명시적 NO-GO**: **어떤 분해 슬라이스 안에서도 #4961/#4973의 로직 변경을 시도하지 마라.** #4973은 naive Phase B가 각각 독립적으로 자기무효화하는 4가지 이유로 틀렸다고 기록한다. 이동에 섞으면 이전 8개 슬라이스를 지킨 바이트 비교 리뷰가 파괴된다.

---

## 구현 시 핵심 파일

- `src/services/discord/tmux_watcher.rs` (핫파일; S-A는 `3425–3552` 이동)
- `scripts/hotfile_ratchet.toml:123` (raw cap `3571`)
- `scripts/audit_maintainability_giant_baseline.toml:143` (prod cap `3571`)
- `scripts/test_lane_coverage_baseline.txt` + `scripts/check_test_lane_coverage.py` (§6.1 하드 게이트)
- `src/services/discord/tmux_watcher/liveness.rs:666-691` (W8을 제약하는 경로 결합 계약 앵커)
