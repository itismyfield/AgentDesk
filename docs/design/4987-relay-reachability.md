# 릴레이 도달(reachability) 기반 판정 모델 재설계

> **R2 개정본.** 카운터리뷰(gpt-5.6-luna)가 R1/R2/R3로 수용 기준 3개 중 2개를
> 코드로 반증했다. **§-1을 먼저 읽어라.** §2.5 / §6 / §8.2 / §9 / §11은 §-1이 대체한다.

---

## -1. 카운터리뷰 반영 (R2)

### -1.0 한 줄 요약

**진단은 살아남았고, 의무 좌표는 갈아엎었다.** 1라운드는 의무를 *턴*에 묶으려 했고
(그래서 `turn_nonce`가 필요했고, 행이 없으면 무너졌다), 개정본은 의무를
**트랜스크립트 화신(incarnation)의 바이트 구간**에 묶는다. 화신 identity는
`.generation` mtime / `.spawn_nonce` / inode로 **디스크에 실재하며 행과 독립**임을
코드로 확인했다. 턴 identity는 의무 좌표에서 **완전히 제거**했다.

### -1.1 반증 수용 — 다투지 않는다

#### R1. `TuiRuntimeBinding::relay_output_path()`는 독립 좌표가 아니다 — **수용**

`relay_output_path`가 비면 자기 `output_path`로 fallback하고, Codex 생성부는
`tmux_common::session_temp_path(...)` 즉 **행과 같은 wrapper 계열**이다.
1라운드 §6.2가 이걸 "이미 있는 제2 좌표"라고 쓴 것은 **틀렸다. 철회한다.**

**개정**: 독립 좌표의 출처를 **워처 레지스트리 엔트리**로 교체한다.
`TmuxWatcherRegistry`의 엔트리는 자기 `output_path`를 들고 있고
[확인 `watchers/lifecycle/claims.rs:60-72` — `find_watcher_by_tmux_session`가
`entry.output_path.clone()`을 반환], `claims.rs:196-205`의 가드는 바로 이 값이
provider-native로 승격된 뒤 wrapper 경로로 **강등되지 않도록** 보호한다.
#4986 이슈 본문이 이 분리를 직접 진술한다:

> *"이 가드는 watcher 레지스트리 바인딩만 보호하고 인플라이트 행의 `output_path`는
> 보호하지 않는다. 즉 watcher는 native를 보고 있어도 행은 wrapper를 들고 있을 수 있다
> — 관측된 상태가 정확히 그것이다."*

즉 **사고 당시 두 좌표는 실제로 갈라져 있었고, 하나는 옳았다.**
그런데 health로 가는 통로가 없다: `channel_binding()`은
`{owner_channel_id, tmux_session_name}`만 반환한다
[확인 `mod.rs:1208-1219`]. **`entry.output_path`는 health 스냅샷에 노출된 적이 없다.**

#### R2. rowless 의무 산출이 `turn_nonce` 때문에 성립하지 않는다 — **수용, 그리고 좌표를 바꾼다**

`ExactJsonlSourceIdentity`에 `turn_nonce`가 있고 [확인 `delivery_record.rs:124-150`],
트랜스크립트 assistant 레코드에서 nonce를 뽑는 계약은 없다. 맞다.

**개정 — 근본 대응**: 의무는 `ExactJsonlSourceIdentity`를 쓰지 않는다.
**턴 identity가 애초에 필요 없다.** 도달 질문은
*"이 턴이 배달됐나"* 가 아니라 *"이 트랜스크립트 화신의 이 바이트들이 채널에 닿았나"* 다.
바이트 커버리지는 턴에 대해 **불가지(turn-agnostic)** 하다 — 같은 파일의 같은 구간을
어느 턴의 영수증이 덮었든 그 바이트는 배달된 것이다.

그래서 개정 좌표는 turn_nonce를 뺀 6개다(§-1.3). 그리고 **영수증 대조는 사영(projection)**
으로 한다: 영수증 `R`이 의무 `[a,b)`를 덮는지는
`(provider, tmux_session_name, generation_mtime_ns)` 일치 + 구간 포함으로 판정하고
**`turn_nonce`는 보지 않는다.**

이 사영의 방향성이 중요하다: nonce를 무시하면 매칭이 **느슨해지므로 "덮였다"로 더 많이
판정**한다. 즉 **false `Unreachable`(오탐)을 만들 수 없다.** 오탐만이 비용을 만드는
설계이므로(§7) 안전한 방향의 느슨함이다.

R2의 두 번째 지적 — *"assistant 블록이 있다"와 "라이브 턴이 배달해야 할 산문이 있다"는
다른 명제* — 도 수용한다. **개정본은 후자를 주장하지 않는다.** 전자만 주장한다.
그리고 그것으로 충분하다: out-of-band 워치독이 3개 형상을 전부 잡을 때 쓴 명제가
정확히 전자이고, 워치독도 턴 identity를 모른다 [확인 `relay_watchdog.py:1333`].

#### R3. `AnchorWithoutReceipt`와 "identity 미참조" 주장 — **수용, 신호 삭제**

`current_msg_id`(스트리밍 placeholder)와 `panel_msg_id`(terminal anchor)가 같은
객체라는 계약은 없다 [확인 `delivery_record.rs:203-225` — `panel_msg_id`는
"terminal-replace edited in place" 대상으로만 규정]. 정상 스트리밍 중간 상태를
`Degraded`로 오탐한다.

**개정**: `AnchorWithoutReceipt`를 **설계에서 삭제한다.** #4974는 Tier A 구간
커버리지만으로 잡힌다(§-1.4). 그리고 1라운드의
*"identity를 전혀 참조하지 않는다"* 는 문장은 **거짓이었다. 철회한다.**
정확한 문장으로 교체한다:

> 개정 의무 좌표는 **턴 identity를 참조하지 않으며, 인플라이트 행을 참조하지 않는다.**
> **화신(incarnation) identity는 참조한다** — 그리고 그 identity의 권위는 디스크
> 사이드카이지 행이 아니다.

### -1.2 사이드카 조사 결과 — 뼈대는 복구되는가

코디네이터가 준 단서를 코드로 검증했다.

| 사이드카 | 쓰는 곳 | 읽는 곳 | 행 독립? | `ExactJsonlSourceIdentity` 매핑 |
|---|---|---|---|---|
| `.generation` **mtime** | 스폰 시 1회 (`claude.rs`/`codex.rs`가 `tmux::create_session` 직후), 이후 라이브 wrapper가 **절대 안 건드림** [확인 `tmux_session_files.rs:12-15`] | `read_generation_file_mtime_ns(tmux_session_name)` [확인 `:19-36`] ← `current_generation_mtime_ns` [확인 `delivery_record.rs:1277-1287`] | **YES** — 키가 `tmux_session_name` | **`generation_mtime_ns` 그 자체.** 즉 영수증 좌표의 이 필드는 **이미 행 독립이다** |
| `.spawn_nonce` **content** | `write_spawn_nonce()` 스폰당 1회, v4 UUID, atomic rename [확인 `:69-91`] | `read_spawn_nonce(tmux_session_name)` [확인 `:100-111`] | **YES** | 매핑 없음 (신규 좌표 항으로 추가) |
| `.owner` / `.runtime-kind` | 스폰 시 | — | YES | 미사용 |
| `tmux_session_name` | — | `authoritative_tmux_session(enriched, mailbox_cancel_session)` [확인 `snapshot.rs:391-398`] | **YES — 이중 출처** (워처 레지스트리 + mailbox cancel token) | `tmux_session_name` |

**핵심 발견 [확인]:**

> `DeliveredCommit` / `ExactJsonlSourceIdentity`의 `generation_mtime_ns`는
> **`tmux_session_name`으로 `.generation` 파일을 stat한 mtime**이다.
> 즉 **영수증 좌표계는 이미 절반이 행 독립이었다.** 1라운드는 이걸 못 봤고,
> 그래서 의무를 행/턴에 묶는 잘못된 경로로 갔다.

`.generation`이 스폰 후 절대 재기록되지 않고(그래서 mtime이 wrapper 인스턴스를 유일하게
식별하고 #1270 rotation 대응), adoption이 내용을 고칠 때조차 **mtime을 보존**한다는 것도
확인했다 [확인 `tmux_session_files.rs:141-150`]. 화신 identity로 쓰기에 적합하다.

**단, 코디네이터 단서 중 하나는 범주 오류다 [확인]:**
`born_generation`(=50)은 `.generation` 파일의 **내용**(=49)과 비교 대상이 아니다.
`born_generation`은 **dcserver 재시작 세대**다 — `shared.restart.current_generation`이
대입되고 [확인 `tmux_reaper.rs:1117,1245,1343`] 같은 값과 비교된다
[확인 `tui_direct_pending_start.rs:736-743`]. 서로 다른 카운터가 우연히 인접한 정수였다.
**따라서 "50 vs 49 불일치"는 그 자체로 이상 증거가 아니다.**
그럼에도 단서의 결론(*행과 독립인 generation 권위가 디스크에 실재한다*)은 **참이다** —
다만 근거는 파일 **내용**이 아니라 파일 **mtime**이고, 그 mtime은 이미 영수증 좌표다.

**판정: 뼈대는 복구된다. 단 1라운드보다 좁아진다.**

### -1.3 개정 좌표 — `IncarnationRange` (턴 아님, 행 아님)

```rust
/// 행·턴과 독립인 도달 좌표. `ExactJsonlSourceIdentity`를 대체하지 않고
/// 그 위로 **사영**된다 (turn_nonce를 무시하는 방향으로만).
struct IncarnationRange {
    provider: ProviderKind,          // 채널→provider, 행 무관
    tmux_session_name: String,       // 워처 레지스트리 or mailbox cancel token
    generation_mtime_ns: i64,        // .generation mtime  ← 영수증과 동일 권위
    spawn_nonce: Option<String>,     // .spawn_nonce content (없으면 None, 위조 금지)
    transcript_file_id: (u64, u64),  // (dev, ino) — 경로가 아니라 파일 자체
    range: (u64, u64),               // 바이트 구간
}
```

`turn_nonce` **없음**. `user_msg_id` **없음**. `current_msg_id` **없음**. 행 참조 **없음**.

**트랜스크립트 경로 해결 — 3순위 폴백, 실패 시 fail-closed [제안]:**

| 순위 | 출처 | 행 독립 | 비고 |
|---|---|---|---|
| 1 | **워처 레지스트리 엔트리 `output_path`** [확인 `claims.rs:60-72`] | YES | #4986-1에서 native를 들고 있던 값. **health에 노출 필요** (신규 배선) |
| 2 | `TuiRuntimeBinding` | **부분적** — R1대로 wrapper로 fallback 가능 | 1과 **다를 때만** 의미 있는 비교 피연산자로 사용 |
| 3 | 파일시스템 discovery (워치독 방식) | YES | provider별 휴리스틱. 비용·위험 큼 |
| — | 전부 실패 | — | `Unknown{TranscriptUnresolved}` — **`Reachable`이 아니다** |

그리고 **divergence는 경로 문자열 비교가 아니라 파일 identity 비교**로 재정의한다:
`(dev, ino, size)` 를 비교한다. #4986-1은 행 경로가 stat 실패(파일 없음)인데
레지스트리 경로는 살아 있는 파일 → `RowPathUnresolvableWhileRegistryLive`.
R1이 지적한 "둘 다 같이 틀릴 수 있다"는 **경로 문자열 비교에만 해당**하고,
"한쪽은 열리고 한쪽은 안 열린다"는 문자열이 같든 다르든 성립하는 관측이다.

### -1.3b 개정 판정 상태 — `TransportUnknown` 신설

계약이 success→commit 크래시 창을 `Unknown`으로 유지하는데
[확인 `relay-state-contract.md:45-48`], 1라운드는 `NoReceipt`를 곧바로
`Unreachable`로 보냈다. 그러면 사람이 수동 재배달해 **중복**을 만든다 —
#4986이 수동 개입을 거부한 바로 그 실패 모드다.

```rust
enum ReachabilityVerdict {
    Reachable,
    Degraded { .. },
    /// 영수증 없음 + 전송이 실제로 일어났을 실증적 근거 있음
    /// (릴리즈되지 않은 lease 흔적, 재시작 경계 교차, placeholder 존재 등).
    /// **Unreachable이 아니다. 알람 문구가 다르고 "수동 재배달 금지"를 명시한다.**
    TransportUnknown { since_secs: u64, evidence: TransportUnknownEvidence },
    /// 영수증 없음 + 전송 흔적 없음.
    Unreachable { .. },
    Unknown { reason: ReachabilityUnknownReason, since_secs: u64 },
}
```

`TransportUnknown`은 **건강이 아니고**(degraded에 기여) **재배달 권한도 아니다**.
계약 I11의 "positive delivered-elsewhere proof 없으면 보존" 규율과 같은 자리에 놓인다.

### -1.4 개정 후 세 형상 검출 재증명

**전제가 하나 바뀌었다.** 극성 반전(§4.1) 덕분에 **수용 기준은 "Unreachable을 낸다"가
아니라 "GREEN이 아니다"** 이다. `Unknown`도 GREEN이 아니다. 따라서 검출은
*discovery 성공에 의존하지 않는다* — discovery가 실패하면 그 실패 자체가 non-GREEN이다.
이것이 R1의 공격(좌표가 같이 틀릴 수 있다)에 대한 구조적 방어다.

| 형상 | 1차 검출 (Tier A) | 즉시 파생 | discovery 실패 시 | 결과 |
|---|---|---|---|---|
| **#4974** | 화신 구간 의무 누적, 해당 구간 영수증 0 → `Unreachable` | (없음 — `AnchorWithoutReceipt` **삭제됨**) | `Unknown{TranscriptUnresolved}` | **non-GREEN** |
| **#4986-1** | 레지스트리 경로로 해결한 native 트랜스크립트에서 의무 누적, 영수증 0 → `Unreachable` | `RowPathUnresolvableWhileRegistryLive` (파일 identity 비교, 문자열 아님) | `Unknown{TranscriptUnresolved}` | **non-GREEN** |
| **#4986-2** | mailbox cancel token → `tmux_session_name` → 사이드카 → 화신 좌표 → 의무 누적, 영수증 0 → `Unreachable`. **행·턴 identity 불필요** | `RowlessActiveTurn` (설명 속성으로 강등, 판정 생산 안 함) | `Unknown{TranscriptUnresolved}` | **non-GREEN** |

#### 각 형상의 행 독립성 근거 (개정)

- **#4974**: 의무는 화신 구간이고 `user_msg_id`/anchor identity를 참조하지 않는다.
  `restart_mode`, `drain_restart`, zero-origin 여부와 **무관하게** 성립한다.
- **#4986-1**: `generation_mtime_ns`는 `tmux_session_name`으로 `.generation`을 stat하므로
  [확인 `tmux_session_files.rs:19-36`] 행의 깨진 `output_path`와 **무관하게** 해결된다.
  이것이 1라운드가 놓친 진짜 독립 좌표다.
  **[추정]** 사고 시점 레지스트리 `output_path`가 실제로 native였는지는 이슈 본문의
  진술만 있고 값 자체는 덤프되지 않았다. 그러나 위 표대로 이 추정이 틀려도
  결과는 `Unknown` = non-GREEN이므로 **수용 기준은 추정에 의존하지 않는다.**
- **#4986-2**: `tmux_session_name`의 행 독립 출처가 코드에 실재한다
  [확인 `snapshot.rs:391-398` `authoritative_tmux_session(enriched, mailbox_cancel_session)`].
  거기서 사이드카가 전부 해결되고, `turn_nonce`는 좌표에서 제거됐으므로 R2가 지적한
  결손이 발생하지 않는다.

#### 오탐 반례 — 인수 테스트에 필수 포함 [제안]

카운터리뷰가 요구한 7개를 그대로 인수 조건에 넣는다. **각각이 `Unreachable`을 내면 실패다.**

| # | 반례 | 기대 판정 |
|---|---|---|
| 1 | POST 성공 + receipt write 실패 (크래시 창) | `TransportUnknown`, **not** `Unreachable` |
| 2 | 다른 generation의 receipt만 존재 | `Unreachable` (정탐 — generation 게이팅이 작동) |
| 3 | 이전 턴 블록만 존재, 현재 턴 산문 없음 | `Reachable` (의무 0) |
| 4 | wrapper·native가 같은 크기 다른 inode | divergence 검출 (파일 identity 비교이므로 크기 일치가 가리지 못함) |
| 5 | bounded read가 오래된 메시지 미반환 | 워치독 `Unknown` (§-1.5), dcserver 판정 **불변** |
| 6 | placeholder 존재 + terminal receipt 아직 없음 | grace 내 `Reachable`, 초과 시 `Degraded` — **`Unreachable` 금지** |
| 7 | ledger malformed | `Unknown{ReceiptStoreUnreadable}`, **not** `Unreachable` |

#### 여전히 남은 blind spot — 정직하게

**의무 0의 GREEN.** 트랜스크립트 ingestion 자체가 죽으면 assistant 블록이 안 보이고
"산문 없음"과 구별되지 않는다. 1라운드 §A7은 이걸 "정확한 동작"이라 했는데,
**틀렸다.** 개정: `Reachable`을 선언하려면 **화신이 살아 있다는 양성 증거**가 필요하다 —
`transcript_file_id` 해결 성공 **그리고** (파일 크기 전진 **또는** pane idle 확증).
둘 다 없으면 `Unknown{TranscriptUnresolved}`. 즉 **"안 보인다"는 절대 GREEN이 아니다.**

### -1.5 나머지 FLAWED 반영

| 지적 | 반영 |
|---|---|
| **I13이 I10과 충돌** (`:420-432`가 intentional classified drop으로 cursor 소비 허용) | **I13 재작성.** 의무 소멸 사유를 typed로 분리: `ReceiptCovered` / `ClassifiedDrop{reason}` / `IncarnationRetired`. I13의 금지 대상은 **암묵적 소멸**(프론티어 전진·offset 전진·grace 만료)이지 분류된 drop이 아니다. 그리고 **건강을 `Reachable`로 승격하는 증거는 `ReceiptCovered`뿐**이고, `ClassifiedDrop`은 의무를 닫되 **별도 카운터로 관측**된다. cursor 소비 규칙(I10)과 건강 승격 규칙을 분리했다 |
| **I14는 컴파일러 강제가 아니다** (`InflightTurnState`가 `pub(in crate::services::discord)`) | **"컴파일러가 강제한다" 철회.** 대체 수단 3택 중 **소스 게이트**를 채택: `scripts/check_reachability_row_independence.py` — `health/reachability/**`의 `use` 구문에 `inflight::` 경로가 나오면 CI 실패. `check_contract_symbol_refs.py`와 같은 장르다. **이것은 린트이지 타입 증명이 아니다**라고 문서에 명시. 진짜 강제를 원하면 별도 crate 분리가 필요하고 그건 범위 밖 |
| **I15도 타입 제약이 아니다** (`plan_relay_recovery`가 단일 상태에서 파괴적 action 직접 산출) | **주장 하향 + 실제 수단 명시.** `RelayRecoveryActionKind`의 파괴적 variant를 private 생성자 뒤로 옮기고, 그 생성자를 가진 모듈(`relay_recovery::destructive`)을 `reachability`가 import하지 못하게 한다(위 소스 게이트가 같이 검사). 이는 **리팩터 비용이 있는 실제 제약**이며, 하지 않을 경우 I15는 **규율(convention)** 임을 명시한다 |
| **성공→commit 크래시 창을 false `Unreachable`로 만듦** | `TransportUnknown` 신설 (§-1.3b). 알람 문구에 **"수동 재배달 금지"** 명시 |
| **"DB migration 불요"를 GO 근거로 쓰지 마라** | 표현 하향: *"이번 관측 슬라이스는 DB를 건드리지 않는다."* host-local sidecar는 **cluster authority가 아니다** [`relay-live-state-taxonomy.md:17-20, 52-74`]. 다중 노드에서 이 판정은 **호스트 로컬 관측**일 뿐이며, 계약 §Task-32의 canonical authority로 승격하려면 PG 마이그레이션 + `immutable-checksums.json` 등록이 필요하다. **GO 근거 목록에서 삭제** |
| **워치독 `unreachable`을 즉시 최종 권위로 승격 금지** | **수용.** 현재 워치독은 bounded read(`--limit 100`)라 pagination 불완전·edit/delete·stale transcript 선택으로 false `unreachable` 가능. **개정**: sidecar 스키마에 `generation_mtime_ns` / `spawn_nonce` / `transcript_file_id` / `watchdog_epoch` / `read_complete: bool` 를 넣고, ① `read_complete=false` → **`Unknown`** (권위 없음), ② 화신 identity가 현재와 불일치 → **무시**, ③ 일치 + `read_complete=true` 일 때만 **동일 화신 내에서 단조 악화** 권위. **freshness 기반이 아니라 identity-gated monotonicity** |
| **B1 fixture 사각지대** (Python이 `(epoch, text)`만 반환, byte offset·CRLF·partial line·multi-byte slicing 없음) | **수용 — B1 범위 확대.** 양쪽이 canonical schema `(generation, start, end, identity, reason)` 를 내도록 **Python 측 `assistant_blocks_from_lines`를 확장**해야 한다 [확인 `relay_watchdog.py:1321-1333` 현재 스키마 부족]. 게이트에 **뮤테이션 테스트** 포함: 한쪽 구현만 바꾸면 반드시 실패. partial line / CRLF / multi-byte 경계 / rotation 좌표를 fixture에 포함 |
| **의무 0 blind spot** | §-1.4 말미에서 재작성 — `Reachable` 선언에 화신 생존 양성 증거 요구 |
| **오탐 반례 7종** | §-1.4 표로 인수 조건화 |

### -1.6 개정 슬라이스 — 신규/변경분

§9 표는 유효하되 아래가 **추가·변경**된다.

| # | 변경 | 파일 | 핫파일 | 비고 |
|---|---|---|---|---|
| **S0 (신규, 선행)** | 워처 레지스트리 `entry.output_path`를 health에 노출 | `mod.rs` (`channel_binding` → `TmuxWatcherBinding`에 `output_path` 추가), `health/session_enrichment.rs` | **없음** | `mod.rs` 5808줄 = giant → **레지스트리 admission 노트 필요**. 순수 가산 read 노출 |
| **S1 변경** | 의무 좌표를 `IncarnationRange`로 (turn_nonce 제거), `Reachable`에 화신 생존 양성 증거 요구 | `health/reachability/**` | 없음 | – |
| **S1 변경** | **B1 확대**: Python `assistant_blocks_from_lines` 스키마 확장 (byte offset/CRLF/partial line/multi-byte/rotation) | `scripts/relay_watchdog.py` | 없음 | 1라운드 추정보다 **작업량 큼** |
| **S3 변경** | `TransportUnknown` 추가, I13 typed 소멸 사유, I15 파괴적 variant private 생성자 리팩터 | `relay_recovery/decision.rs` 등 | 없음 | I15 리팩터는 **비용 있음**. 안 하면 I15는 규율 수준 |
| **S6 축소** | 워치독 sidecar에 화신 identity + `read_complete` 추가, **identity-gated monotonic** 권위로 제한 | `relay_watchdog.py`, `reachability/external_verdict.rs` | 없음 | `read_complete=false` → `Unknown` |
| **신규 게이트** | `scripts/check_reachability_row_independence.py` (소스 린트) | `scripts/`, `ci-script-checks.sh` | 없음 | 타입 증명 아님을 명시 |

`AnchorWithoutReceipt`(S4 일부)는 **삭제**. `RowlessActiveTurn`은 판정 생산자에서
설명 속성으로 **강등**.

### -1.7 개정 GO / NO-GO

#### **조건부 GO — 관측 슬라이스(S0/S1/S2)만. 판정 권한(S3)은 NO-GO(게이트 전).**

1라운드는 "GO — S1,S2,S6 즉시"였다. 개정본은 **더 좁다.** 이유:
검출 논거가 **2~3중 중복에서 사실상 단일 메커니즘(Tier A 화신 구간 커버리지)으로 축소**됐다.
`AnchorWithoutReceipt`는 삭제됐고 divergence는 재소싱됐으며 그 재소싱은
**레지스트리 값이 실제로 native였다는 미덤프 추정**에 일부 기댄다.
여유(margin)가 줄었으므로 권한 부여는 더 늦춘다.

**GO 근거 (개정):**
1. 화신 identity의 행 독립 권위가 **코드로 실재 확인**됐다 —
   `generation_mtime_ns`는 이미 `tmux_session_name`으로 해결되는 영수증 좌표다
   [확인 `delivery_record.rs:1277-1287` → `tmux_session_files.rs:19-36`].
2. 극성 반전 덕에 **수용 기준이 "non-GREEN"** 이므로 discovery 실패도 검출이다(§-1.4).
   R1의 "좌표가 같이 틀릴 수 있다"가 검출을 무력화하지 못한다.
3. 세 형상 모두 **행·턴 identity 없이** non-GREEN에 도달한다(§-1.4 표).
4. S0~S2는 핫파일 0, `tmux_watcher.rs` +0줄.

**GO 근거에서 삭제된 것**: "DB migration 불요" — host-local sidecar는 cluster
authority가 아니므로 장점이 아니라 **범위 한정 사실**이다.

#### Blocker (전부 닫히기 전 S3 진행 금지)

| # | blocker | 비고 |
|---|---|---|
| **B1′** | Rust↔Python canonical schema `(generation,start,end,identity,reason)` 동치 + **뮤테이션 게이트** | 1라운드보다 범위 큼 — Python이 byte range를 아예 안 낸다 |
| **B2** | 30일 관측, `SuppressedByDedup < 0.1%` | 유지 |
| **B3** | 틱 소요 p99 계측 | 유지 |
| **B4 (신규)** | 사고 재현 환경에서 **레지스트리 `output_path`가 실제로 native인지 실측** | 참이면 divergence가 1차 검출로 승격, 거짓이면 `Unknown` 경로만 남음 — 어느 쪽이든 설계는 성립하나 **문서의 주장 강도를 확정해야** 함 |
| **B5 (신규)** | I15 파괴적 variant private 생성자 리팩터 착수 여부 결정 | 안 하면 I15를 "규율"로 하향 표기하고 계약에 그대로 기재 |

#### NO-GO (개정)

| 항목 | 판정 | 사유 |
|---|---|---|
| **S3 판정 권한 부여** | **NO-GO (게이트 전)** | B1′~B5 미해소. 단일 메커니즘 의존으로 여유 축소 |
| **`ExactJsonlSourceIdentity`를 의무 좌표로 사용** | **NO-GO** | `turn_nonce`의 행 독립 출처 없음 (R2). `IncarnationRange`로 대체 |
| **`AnchorWithoutReceipt`** | **NO-GO (삭제)** | `current_msg_id`↔`panel_msg_id` 계약 부재, 스트리밍 중간 상태 오탐 (R3) |
| **`TuiRuntimeBinding`을 독립 좌표로 사용** | **NO-GO** | wrapper로 fallback (R1) |
| **워치독 `unreachable`의 무조건 권위** | **NO-GO** | pagination completeness 증명 없으면 `Unknown` |
| **S7 재배달** | **NO-GO (범위 밖)** | 유지 |
| **의무 원장 PG 승격** | **NO-GO (범위 밖)** | 유지 |

### -1.8 §-1이 대체하는 절

| 절 | 상태 |
|---|---|
| §2.5 (좌표계 — `ExactJsonlSourceIdentity` 재사용) | **대체** → §-1.3 `IncarnationRange` |
| §6.1 `AnchorWithoutReceipt` | **삭제** |
| §6.2 `TuiRuntimeBinding` 독립 좌표 주장 | **철회** → §-1.1 R1, §-1.3 |
| §6.3 "행 등장하지 않는다" | **유지** (개정 좌표에서 더 강해짐) |
| §6.4 요약표 (2~3중 검출) | **대체** → §-1.4 표 (단일 1차 + fail-closed) |
| §8.2 I13/I14/I15 | **대체** → §-1.5 (typed 소멸 사유 / 소스 린트 / 주장 하향) |
| §9 슬라이스 표 | **보완** → §-1.6 (S0 신설, B1 확대) |
| §11 GO/NO-GO | **대체** → §-1.7 |
| §A5 (증상 패치 반박) | **약화** — `AnchorWithoutReceipt` 삭제로 논거 일부 소멸. 핵심(의무 뺄셈이 무게중심)은 유지 |
| §A7 (의무 0은 정확한 동작) | **철회** → §-1.4 말미 (화신 생존 양성 증거 요구) |
- 대상: `#4974` / `#4986 형상1` / `#4986 형상2`
- 성격: **판정 모델 구조 변경**. 증상 조건 추가가 아니다.
- 권위 문서: [`docs/relay-state-contract.md`](../docs/relay-state-contract.md) — 본 설계는 이 계약을 **개정**한다(§8).

---

## 0. 표기 규약 — 확인한 것과 추정한 것

| 표기 | 의미 |
|---|---|
| **[확인]** | 이 세션에서 리포 코드 / 이슈 본문 / 커밋을 직접 읽고 확인한 사실. `file:line` 또는 이슈 인용을 동반한다. |
| **[추정]** | 코드에서 직접 확인하지 못했고, 관측 데이터로부터 추론한 것. 구현 전 검증이 필요하다. |
| **[제안]** | 이 문서가 새로 정하는 설계 결정. 아직 코드에 없다. |

수치 임계값(grace, 알람 지연)은 **전부 [제안]이며 슬라이스 S1의 관측 데이터로 재보정해야 한다.** 측정 없이 확정한 임계값은 이 설계의 실패 지점이다(§10-A6).

### 0.1 리포 상태 기준점 [확인]

```
HEAD            dd46cc807
#4974 수정      f4f816438 "fix(#4974): admit zero-origin relay reattach identity (#4982)"
                 → destructive_cancel_gate.rs / relay_recovery_circuit_breaker.rs /
                   relay_recovery_auto_heal_apply.rs (+259 -10)
tmux_watcher.rs  3569줄 (캡 3571, 여유 2줄)
계약 문서        docs/relay-state-contract.md 494줄, I1–I12
```

---

## 1. 현재 판정 모델의 정확한 지도

### 1.1 신호 인벤토리 — 어디서 만들어져 어디서 소비되나

| 신호 | 생산 (`file:line`) | 소비 | **실제로 측정하는 것** |
|---|---|---|---|
| `RelayHealthSnapshot` (23필드) | `health::unpaired_active_token::build_relay_health_snapshot` | `/api/health/detail`, 분류기, 복구 플래너 | 구조 스냅샷 |
| `relay_stall_state` | `relay_health::RelayStallClassifier::classify`, 호출부 `health::snapshot::{watcher_state_snapshot_for_shared, build_health_snapshot_with_options}` | `relay_recovery::decision::plan_relay_recovery`, `health::stall_liveness` 판단 로그, `health::relay_auto_heal::apply_orphan_pending_token_cleanup` | 구조 이상 8분류 |
| `desynced` | `health/session_enrichment.rs:186-190` (`SessionEnrichment::desynced`), 호출 `snapshot.rs:584`, `:758` | 분류기 `relay_health.rs:160-166`, 워치독 coverage | **capture offset과 confirmed-end offset의 격차** |
| `capture_lagged` | `session_enrichment.rs:129-136` | `desynced` | 위와 동일. **`inflight_state_present` 필수 조건** |
| `last_capture_offset` | `session_enrichment.rs:103-113` → `capture_coordinate_for_path()` `session_enrichment.rs:234-260` | `desynced`, `unread_bytes` | **인플라이트 행의 `output_path`를 `fs::metadata`한 파일 크기** |
| `last_relay_offset` | `session_enrichment.rs:86-102` — `tmux_relay_coords[ch].confirmed_end_offset` | `desynced`, 스냅샷 | I4 confirmed-end 워터마크 (인메모리 atomic) |
| `watcher_attached_stale` | `session_enrichment.rs:58-60` | 분류기 `relay_health.rs:157-159`, 워치독 | 워처 핸들 하트비트 |
| `stall_shadow_verdict` | `health/stall_verdict.rs:250` (`classify_health_snapshot_lossy`), 호출 `snapshot.rs:807` | `/api/health/detail` 관측 전용 (shadow) | 구조 스냅샷 재분류 |
| `CaptureAssessment.advancing` | `health/liveness_authority.rs:265-347` | `stall_liveness.rs:364-418` **방어적 사용만** | capture offset 전진 여부 |
| `DeliveredCommit` (durable frontier) | `outbound/delivery_record.rs:207-226`, 기록 `:794 commit_ordered_jsonl_range` | I10/I11 dedup, 재시작 hydration | **확인된 전송 후의 내구 커밋** |
| `ConfirmedDeliveryReceipt` | `outbound/delivery_record.rs:157-173` | dedup / seed 제거 권한 | **Discord `message_id`를 보유한 확정 영수증** |
| `CompletedTurnLedger` | `outbound/completed_turn_ledger.rs:56-67`, `:167 append_completed_turn` | catch-up 게이트 | 배달 완료된 inbound `user_msg_id` 집합 |
| out-of-band 워치독 판정 | `scripts/relay_watchdog.py:2250-2304` (`evaluate`) | `discord-sendmessage` 직접 알림 + GitHub 이슈 | **소스 트랜스크립트 ↔ Discord 채널 본문 대조** |

### 1.2 이 지도가 드러내는 단일 구조 결함

위 표에서 **결과(바이트가 Discord에 도달했나)를 측정하는 행은 정확히 두 개**다:
`DeliveredCommit`/`ConfirmedDeliveryReceipt`(= 우리가 **보냈다고 기록한 것**)와
워치독의 `evaluate`(= **채널에 실제로 있는 것**).

그런데 **전자는 판정에 쓰이지 않는다.** `RelayHealthSnapshot` 23개 필드 중
`delivery_record`를 읽는 필드는 **0개**다 [확인 — `snapshot.rs:400-432` 전 필드 검토].
영수증은 오직 *중복 억제*(I9/I10/I11)에만 쓰이고, *건강 판정*에는 참여하지 않는다.

그 결과 현재 판정 모델의 건강 함수는 전부 이 형태다:

```
health = f(inflight_row, watcher_handle, mailbox, tmux)
```

**네 항이 전부 "릴레이 기계의 내부 상태"다.** 배달 결과는 항으로 들어오지 않는다.

### 1.3 더 나쁜 성질 두 개 — 이것이 세 형상의 공통 원인이다

#### (a) 관측자가 피관측자와 **같은 깨진 좌표를 공유**한다 [확인]

```rust
// health/session_enrichment.rs:103-113
let output_path_for_metadata = inflight
    .as_ref()
    .and_then(|state| state.output_path.as_deref())   // ← 행이 들고 있는 경로
    .map(str::to_string);
let capture_coordinate = match output_path_for_metadata {
    Some(path) => spawn_blocking(move || capture_coordinate_for_path(&path)).await ...
```

```rust
// health/session_enrichment.rs:239-245
let Ok(metadata) = std::fs::metadata(path) else {
    return CaptureCoordinateObservation { offset: None, ..., status: CoordinateStatus::Missing };
};
```

건강 프로브는 **릴레이가 읽는 바로 그 경로**를 stat한다. 경로가 깨지면 릴레이도 0바이트를
읽고 프로브도 `offset: None`을 받는다. **관측자와 피관측자가 동시에 눈이 먼다.**

#### (b) 증거의 **부재가 건강으로 채점**된다 [확인]

```rust
// health/session_enrichment.rs:129-136
let capture_lagged = last_capture_offset
    .map(|capture| {
        relay_state_matches_inflight
            && inflight_state_present      // ← 행이 없으면 구조적으로 false
            && capture != last_relay_offset
            && relay_stale
    })
    .unwrap_or(false);                     // ← 좌표가 없으면 "지연 아님"

// :186-190
pub fn desynced(&self, live_tmux_present: bool, attached: bool) -> bool {
    let live_tmux_orphaned =
        live_tmux_present && self.inflight_state_present && !attached && self.relay_stale;
    self.capture_lagged || live_tmux_orphaned || self.tmux_session_mismatch
}
```

`desynced`의 세 항 중 두 항이 `inflight_state_present`를 요구하고, 세 번째
(`tmux_session_mismatch`)도 `inflight_state_present && inflight_tmux_session.is_some()`을
요구한다(`:70-73`). 따라서:

> **인플라이트 행이 없으면 `desynced`는 참이 될 수 없다.** 코드 구조상 불가능하다.

그리고 `CaptureAssessment.advancing`은 `stall_liveness.rs:364-418`에서 **오직 방어적으로만**
(파괴적 정리를 *유예*하는 근거로만) 쓰인다. **capture 증거가 경보를 만드는 경로는 존재하지 않는다.**
증거가 사라지면 보호가 약해질 뿐, 아무도 소리치지 않는다.

### 1.4 세 형상이 통과한 경로 — 개별 추적

#### #4974 (zero-origin 재부착 identity)
행은 존재하고 `restart_mode=drain_restart`로 고착. `current_msg_len=3`(`...` 앵커).
세 가드가 각자 "안전하게" 회피 [이슈 본문 인용]. 판정기 관점에서는
`inflight_state_present=true`, tmux 살아있음, 워처 붙음 → 구조적으로 정상. **경보 없음.**
이슈 본문이 직접 요구한 것: *"`/api/health`에 relay 도달성 신호가 없음.
`last_successful_prose_relay_age_secs` 류 지표 필요"* — 즉 이 형상은 **이미 도달 신호를 요구했다.**

#### #4986 형상1 (`output_path` ENOENT)
1. 행의 `output_path` → wrapper 경로, 실제 파일 없음 [이슈 실측].
2. `capture_coordinate_for_path` → `fs::metadata` 실패 → `offset: None`, `status: Missing`
   [확인 `session_enrichment.rs:239-245`].
3. `last_capture_offset = None` → `capture_lagged = unwrap_or(false)` [확인 `:129-136`].
4. `unread_bytes = None` [확인 `:114-116`].
5. `desynced = false` → 분류기 첫 분기 통과 [확인 `relay_health.rs:160-166`].
6. `active_turn = Foreground` → `ActiveForegroundStream` [확인 `relay_health.rs:184-189`].
7. `plan_relay_recovery` → `ObserveOnly`, `skipped_reason="live_foreground_turn"`
   [확인 `relay_recovery/decision.rs:326-331`].

**깨진 경로가 판정기를 침묵시키는 것이 아니라, 판정기를 *눈멀게* 해서 침묵이 건강으로 읽힌다.**

#### #4986 형상2 (행 없는 라이브 턴)
관측: `inflight_state_present=False`, `desynced=False`, `active_turn` 활성, `queue_depth=16`.

분류기를 손으로 돌려보면 [확인 `relay_health.rs:154-190`]:
- `desynced=false` → `TmuxAliveRelayDead` 아님
- `stale_thread_proof=false` → 아님
- `OrphanPendingToken`은 `!watcher_attached && tmux_alive != Some(true)` 요구 → 워처 붙어있고 tmux 살아있으므로 **아님**
- `QueueBlocked`는 `!has_live_relay_evidence()` 요구 → `active_turn.is_active()` 참 → **아님**
- → `RelayActiveTurn::Foreground` → **`ActiveForegroundStream`**

실측 `relay_stall_state=active_foreground_stream`와 **정확히 일치**. 재현 완료.
그리고 §1.3(b)에 의해 행이 없으므로 `desynced`는 **영원히** false다.

### 1.5 결론 — 진단의 정확한 문장

> 세 형상은 서로 다른 버그가 아니라 **같은 판정 모델의 세 가지 관측**이다.
> 판정 함수의 모든 항이 인플라이트 행에서 파생되므로, **행이 손상되거나 사라지면
> 판정 함수는 위반을 내는 대신 증거를 잃고, 증거의 부재는 건강으로 채점된다.**

따라서 조건을 하나 더 추가하는 처방은 원리적으로 실패한다. 추가되는 조건도
같은 행에서 파생될 것이기 때문이다. **필요한 것은 행과 독립인 제2의 권위다.**

---

## 2. "배달됐다"의 정의 — 기존 계약 안에서

### 2.1 계약이 이미 분리해 둔 것 [확인]

`relay-state-contract.md` §Task-32 "Terminal body" 행은 이미 두 개를 분리한다:

> Target linearization is **confirmed Discord transport** plus **identity-gated durable frontier commit**.

그리고 코드에 이미 두 타입이 있다 [확인 `outbound/delivery_record.rs`]:

| 타입 | 위치 | 의미 |
|---|---|---|
| `ConfirmedDeliveryReceipt { source, delivery_channel_id, message_id }` | `:157-173` | **confirmed transport** — Discord가 message id를 돌려준 사실 |
| `DeliveredCommit { range, generation_mtime_ns, panel_msg_id, panel_channel_id }` | `:207-226` | **durable commit** — 릴리즈 후에도 살아남는 배달 프론티어 |
| `ExactJsonlSourceIdentity` | `:124-151` | 두 개를 묶는 불변 소스 좌표 (provider / tmux session / turn_nonce / range / generation) |

**즉 "배달됐다"의 정의는 이미 계약에 있다. 없는 것은 그 반대편이다.**

### 2.2 빠져 있는 항 — 의무(obligation)

영수증은 "우리가 무엇을 보냈는가"를 답한다.
**"우리가 무엇을 보냈어야 하는가"를 답하는 durable 객체는 존재하지 않는다.**

`CompletedTurnLedger`가 가장 가깝지만 [확인 `outbound/completed_turn_ledger.rs:56-67`]
그것은 **inbound `user_msg_id`** 의 원장이다 — *입력*이 처리됐는지를 기록할 뿐,
*출력 산문*이 도달했는지는 기록하지 않는다.

그래서 현재 시스템은 뺄셈을 할 수 없다:

```
도달 결손 = (보냈어야 할 것) − (보냈다고 증명된 것)
                ^^^^^^^^^^^^ 이 항이 없다
```

out-of-band 워치독은 이 항을 **자기가 매번 새로 계산**해서(트랜스크립트 파싱)
채널 본문과 대조한다 [확인 `relay_watchdog.py:1333 assistant_blocks`, `:2250 evaluate`].
**그래서 워치독만 세 형상을 전부 잡았다.** 유일하게 의무 항을 가진 판정기이기 때문이다.

### 2.3 [제안] 정의 — 3단 증거 사다리

배달을 단일 불리언으로 정의하지 않는다. 계약이 이미 confirmed transport와 durable
commit을 분리했으므로, **그 분리를 그대로 연장해 3단으로 정의**한다.

| 등급 | 이름 | 증거 | 기존 계약 대응 | 비용 |
|---|---|---|---|---|
| **E0** | `Obligated` | 소스 트랜스크립트에 assistant text 블록이 존재하고, 그 range가 어떤 영수증에도 덮이지 않음 | **신규** — 계약에 없던 항 | 로컬 파일 tail read |
| **E1** | `Confirmed` | `ConfirmedDeliveryReceipt`가 그 range를 덮고 `message_id != 0` | §Task-32 "confirmed Discord transport" | 무료 (이미 기록됨) |
| **E2** | `Committed` | `DeliveredCommit.range`가 그 range를 덮음 (identity-gated) | §Task-32 "durable frontier commit", I10 | 무료 (이미 기록됨) |
| **E3** | `Audited` | 그 `message_id`가 **지금 채널에 실재**함이 재조회로 확인됨 | **신규** — 영수증 원장 자체의 검증 | Discord API 1회 |

**정의 [제안]:**

> **도달(reachable)** = 모든 `Obligated` range가 `Confirmed` **이고** `Committed` 로 덮여 있다.
> **미도달(unreachable)** = 어떤 `Obligated` range가 `bound` 시간을 초과해 덮이지 않은 채 남아 있다.
> **불명(unknown)** = 의무 집합을 산출할 수 없다 (트랜스크립트 미해결 등).

핵심 세 가지:

1. **E0가 새 항이고, 나머지는 이미 있다.** 이 설계의 신규 구현 부담 대부분은 E0다.
2. **E3(채널 재조회)는 정의에 들어가지 않는다.** E3는 *E1 원장 자체를 감사*하는 항이지
   배달의 정의가 아니다. 매 메시지 재조회는 비현실적이고(§3), 재조회 실패가
   배달 실패로 오독되면 안 되기 때문이다(§7).
3. **`#4081 fingerprint`는 이 사다리에 들어가지 않는다.** 그것은
   degenerate-key 상황의 *중복 억제* 휴리스틱이고 [확인 `tmux_watcher/turn_identity.rs:304-350`],
   그 자체가 오판 이력이 있다(`:333` — "the #4081 guard misjudged it as a duplicate and
   refused delivery"). **배달 증거로 승격하지 않는다.** dedup 권한으로만 남긴다.

### 2.4 [제안] 의무의 정의는 단 하나여야 한다 — 이중 오라클 위험

E0를 in-band(Rust)와 out-of-band(Python 워치독) 양쪽에서 계산하게 되는데,
**두 구현이 "assistant text 블록"을 다르게 정의하면 서로 다른 두 오라클이 생기고
둘 중 하나는 반드시 틀린다.** 계약의 "Missing or legacy identity fields are explicit
typed states, never wildcards" 원칙의 연장이다.

현행 워치독의 규칙 [확인 `relay_watchdog.py:1283-1332`]:
- `type == "assistant"` → `message.content[]` 중 `type == "text"`
- `is_harness_control_assistant_record()`로 harness 제어 레코드 제외

**[제안] 이 규칙을 계약 문서에 명문화하고, Rust/Python 양쪽이 동일한 golden fixture
코퍼스(`tests/fixtures/relay_obligation/*.jsonl`)로 검증되게 한다.**
fixture 불일치는 CI 실패다. 이것은 S1의 인수 조건이며 선택 사항이 아니다(§10-A4).

### 2.5 좌표계 — 새 좌표를 만들지 않는다

> **[R2 대체됨 → §-1.3]** 아래 "`ExactJsonlSourceIdentity`를 그대로 재사용" 주장은
> `turn_nonce`의 행 독립 출처가 없어 **무효**다(R2). 의무 좌표는 `IncarnationRange`다.

E0 의무는 `ExactJsonlSourceIdentity`를 **그대로 재사용**한다 [확인 `:124-151`].
문서 주석이 이미 이 확장을 예고한다:

> *"Phase B can reuse this exact source identity for subordinate assistant-text
> segment receipts without inventing another turn/incarnation coordinate system."*

즉 **의무와 영수증이 같은 좌표계에 놓이므로 뺄셈이 정의된다.** 이것이
"경로는 wrapper인데 오프셋은 native"(#4986 형상1) 같은 좌표계 분열을
구조적으로 불가능하게 만드는 지점이다 — 의무는 자기가 읽은 파일의
`(dev, ino, generation_mtime_ns)`를 함께 기록하므로 좌표와 경로가 분리될 수 없다.

---

## 3. 어떻게 싸게, 연속적으로 증명하나 — 비용/지연 트레이드오프

### 3.1 기존 비용 기준선 [확인]

| 주기 | 무엇 | 근거 |
|---|---|---|
| **30s** | in-process stall watchdog 틱 (provider별) | `STALL_WATCHDOG_INTERVAL_SECS = 30`, `health/recovery/watchdog_decisions.rs:286`; 루프 `health/recovery.rs:2184` |
| 매 health 호출 | `tmux has-session` **서브프로세스** (채널당 1회, `spawn_blocking`) | `session_enrichment.rs:159-172` |
| 매 health 호출 | `fs::metadata(output_path)` 1회 | `session_enrichment.rs:239` |
| **120s** | out-of-band 워치독 폴 (`agentdesk discord read` 1회/채널) | `relay_watchdog.py:255 poll_secs=120`, `:4756` |

**이 기준선이 중요한 이유**: 새 프로브의 비용은 절대값이 아니라 *이미 도는 것 대비*로
평가해야 한다. 이미 채널당 매 틱 **fork+exec 서브프로세스**가 돈다.

### 3.2 [제안] 3계층 증명 구조

#### Tier A — in-band 의무↔영수증 뺄셈 (매 30s 틱, **네트워크 0**)

한 채널 한 틱의 작업:

1. **트랜스크립트 독립 해결**: 후보 project dir `readdir` + `stat` (N ≲ 10) → 최신 mtime 선택.
   워치독의 `select_watch_transcript_with_reason` [확인 `relay_watchdog.py:1203`]와 동일 규칙.
2. **증분 tail read**: `[obligation_watermark, EOF)` 만 읽는다. 전체 파일이 4.8MB여도
   읽는 양은 **지난 틱 이후 생산분**이다.
3. **파싱**: assistant text 블록 추출 (§2.4 규칙).
4. **뺄셈**: 각 블록 range를 `DeliveryRecord`의 `confirmed_deliveries` / `delivered_frontier`와 대조.
   파일 1개 read + flock 없음(읽기 전용) [확인 `delivery_record.rs:359 read_record_at`].

비용 산정:

| 항목 | 양 | 근거 |
|---|---|---|
| 틱당 읽는 바이트 | **관측 델타**. #4986 실측에서 `last_offset` 4,777,773 vs native 4,802,043 = 24KB 격차 [이슈 실측]. 30s 틱이면 통상 수 KB, 스트리밍 폭주 시에도 수백 KB | 페이지 캐시 히트, syscall 1~2회 |
| 상한 | **1MB/틱 캡** [제안]. 초과 시 `Unknown(truncated)` — 조용히 건너뛰지 않는다 | fail-closed (§7) |
| 네트워크 | **0** | 영수증은 이미 로컬 sidecar |
| 기존 대비 | `tmux has-session` fork+exec **1회보다 싸다** [추정 — 벤치 미실시, fork+exec ~1-3ms vs 캐시된 8KB read ~µs] | |

**검출 지연**: 의무 생성 → 최대 1틱(30s) 내 관측. 경보는 grace 이후.

#### Tier B — out-of-band 채널 대조 (120s, Discord read 1회/채널) — **현행 유지**

워치독이 이미 한다. **정의상 유일하게 "우리가 보냈다고 믿는 것"과
"채널에 실제로 있는 것"의 차이를 볼 수 있다.** 즉 Tier A가 구조적으로 볼 수 없는
것 — *영수증이 거짓말하는 경우* — 을 덮는다. §4에서 권위를 부여한다.

#### Tier C — 영수증 재확인 프로브 (희소, 바운드) [제안]

`GET /channels/{ch}/messages/{message_id}` 로 최근 영수증 N개의 message id 실재를 확인.
**Tier A의 영수증 원장 자체를 감사**한다.

발동 규칙 [제안]:
- 정상 시: 채널당 **15분에 1회**, 최신 영수증 1건만.
- Tier A가 `Degraded`로 처음 전이할 때: **즉시 1회** (경보 승격 전 확증).
- 429/5xx: 재시도 없음. `AuditUnavailable`로 기록하고 **판정을 악화시키지 않는다**(§7).

비용: 채널 10개 기준 `10 / 900s ≈ 0.011 req/s`. Discord 봇 글로벌 한도 대비 무시 가능
[추정 — 한도값은 리포에서 확인하지 않음].

### 3.3 샘플링을 쓰지 않는 이유

"매 메시지 재조회는 비현실적 → 샘플링" 은 잘못된 이분법이다.
**연속 증명은 Tier A에서 이미 무료로 얻어진다** — 영수증이 POST 시점에 이미 기록되기
때문이다. 재조회가 필요한 것은 오직 *원장의 신뢰성*이고, 그건 샘플링으로 충분하다.

즉 트레이드오프의 정확한 형태는:

```
연속 · 무료 · 자기보고    → Tier A (의무↔영수증)
주기 · 저렴 · 외부검증    → Tier B (채널 전체 대조, 120s)
희소 · API · 원장감사     → Tier C (영수증 실재 확인, 15min)
```

세 계층은 **서로의 실패 모드를 덮는다**:

| 실패 모드 | A | B | C |
|---|---|---|---|
| 릴레이가 바이트를 안 보냄 | **잡음** | 잡음 | – |
| 영수증은 있는데 채널엔 없음 | 못 잡음 | **잡음** | **잡음** |
| 의무 산출 자체가 틀림 (트랜스크립트 오선택) | 못 잡음 | **잡음** (독립 해결) | – |
| dcserver 프로세스 사망 | 못 잡음 | **잡음** | 못 잡음 |
| 워치독 사망 | **잡음** (A가 계속 돔) | – | – |

마지막 행이 중요하다. 현재는 **워치독이 죽으면 아무도 안 본다** (#4381의 06-29 교훈:
"사라졌고, 사라진 걸 아무도 몰랐다"). Tier A가 있으면 워치독 사망이 전면 실명이 아니다.

### 3.4 검출 지연 비교 [제안 임계값 — S1에서 재보정 필수]

| | 현재 (워치독 단독) | 제안 Tier A | 제안 A+B 합의 |
|---|---|---|---|
| 관측 주기 | 120s | 30s | 30s / 120s |
| grace (정상 지연 허용) | 600s | **120s** | – |
| 실패 선언 | 마지막 성공 배달 + 900s | 의무 미충족 + **420s** | – |
| 실측 검출 시간 | **14분** (#4986 실측) | **~7분** [추정] | – |
| 오탐 압력 | 텍스트 매칭 노이즈(청킹/편집)로 grace를 크게 잡아야 함 | range 대조라 텍스트 노이즈 **없음** → grace를 짧게 잡을 수 있음 | – |

Tier A가 더 짧은 grace를 감당할 수 있는 이유는 **텍스트 매칭을 안 하기 때문**이다.
워치독의 600s grace는 대부분 "청킹/in-place 편집 때문에 뒤 블록이 먼저 도착"하는
매칭 노이즈에 대한 보호다 [확인 `relay_watchdog.py:256-260, 2258-2264`].
range 뺄셈에는 그 노이즈가 없다.

다만 **"긴 툴 호출 중 산문이 합법적으로 미게시 상태로 머무는 시간"** 은 여전히 덮어야 한다.
이 분포는 **측정된 적이 없다.** 따라서:

> **S1은 관측 전용이다.** 의무 생성→영수증 커버 지연의 히스토그램을 30일 수집하고,
> `p99.9 × 2` 로 grace를 확정한 뒤에야 S3에서 판정 권한을 준다.
> 위 120s/420s는 자리표시자이며 근거 없는 값이다.

---

## 4. 판정 계층 재배치

### 4.1 [제안] 새 정점 — `ReachabilityVerdict`

```rust
enum ReachabilityVerdict {
    /// 모든 의무가 Confirmed+Committed로 덮임. 의무가 0개인 경우도 포함.
    Reachable,
    /// 미충족 의무가 warn_bound를 넘김. 아직 fail_bound 미만.
    Degraded { oldest_unsatisfied_age_secs: u64, uncovered_ranges: u32 },
    /// 미충족 의무가 fail_bound를 넘김.
    Unreachable { oldest_unsatisfied_age_secs: u64, uncovered_ranges: u32 },
    /// 의무 집합을 산출할 수 없음. **Reachable이 아니다.**
    Unknown { reason: ReachabilityUnknownReason, since_secs: u64 },
}

enum ReachabilityUnknownReason {
    TranscriptUnresolved,          // 독립 해결 실패
    TranscriptCoordinateDivergence,// 행 좌표 ≠ 독립 해결 좌표
    RowlessActiveTurn,             // mailbox active인데 인플라이트 행 없음
    ReadTruncated,                 // 1MB/틱 캡 초과
    ReceiptStoreUnreadable,        // delivery_record 읽기 실패
}
```

**핵심 규칙 [제안]:**

> `RelayStallState`는 더 이상 **독립적으로 `Healthy`를 선언할 수 없다.**
> 최종 건강 판정은 `(ReachabilityVerdict, RelayStallState)` 의 **곱**이며,
> `ReachabilityVerdict != Reachable` 이면 구조 신호가 무엇이든 `Healthy`가 아니다.

이것이 모델 구조 변경의 본체다. 조건 추가가 아니라 **판정의 극성(polarity) 반전**이다:

```
현재:  구조 이상이 없다        → 건강
제안:  도달 의무가 충족됐다    → 건강
       도달 의무를 산출 못 한다 → 건강 아님 (Unknown)
```

### 4.2 기존 신호 분류 — 유지 / 강등 / 폐기

| 신호 | 판정 | 근거 | 이후 역할 |
|---|---|---|---|
| `relay_stall_state` | **강등** | 세 형상 모두 통과시킴 (§1.4). 그러나 7개 분류는 *복구 행동*을 고르는 데 여전히 유효 | **행동 선택기**로 유지. 건강 선언 권한 박탈. `Healthy`/`ActiveForegroundStream` 반환은 `Reachable` 하에서만 유효 |
| `desynced` | **강등** | 행 없으면 구조적으로 false [확인 §1.3(b)]. 배달이 아니라 *소비 지연*을 잰다 | `RelayHealthSnapshot`의 설명 속성으로 유지. 단독 판정 근거에서 제외. **`TmuxAliveRelayDead` 진입 조건에서만 잔존** |
| `capture_lagged` / `last_capture_offset` | **폐기(현 형태)** | 행의 `output_path`를 stat한다 = 관측자·피관측자 좌표 공유 [확인 §1.3(a)] | **독립 해결 좌표로 재정의**하여 대체. 행 경로 stat은 *비교 대상 중 하나*로만 남고 단독 권위 상실 |
| `watcher_attached_stale` | **유지** | 하트비트는 도달과 독립인 유효 신호. 오탐 이력 없음 | 그대로. 강등 없음 |
| `stall_shadow_verdict` | **폐기** | shadow(관측 전용) 재분류기. 같은 구조 스냅샷을 다시 분류할 뿐이라 §1.5의 결함을 그대로 상속 | `ReachabilityVerdict`가 shadow 자리를 대체. `stall_verdict.rs:250 classify_health_snapshot_lossy` 호출 제거 |
| `CaptureAssessment.advancing` | **유지 + 역할 명시** | 현재 **방어적으로만** 사용 [확인 `stall_liveness.rs:364-418`] | 그대로 방어적으로만. **경보 생산 금지를 계약에 명문화** — 이걸 경보로 승격시키는 것이 전형적 증상 패치다 |
| `DeliveredCommit` / `ConfirmedDeliveryReceipt` | **승격** | 이미 정확한 배달 증거인데 dedup에만 쓰임 (§2.2) | **판정 1급 입력**. E1/E2 |
| `#4081 fingerprint` | **유지, 승격 금지** | 오판 이력 [확인 `turn_identity.rs:333`] | dedup 권한만. 배달 증거로 쓰지 않음 |
| `CompletedTurnLedger` | **유지** | inbound 원장. 직교 | 변경 없음 |
| out-of-band 워치독 | **승격** (§5) | 유일하게 세 형상 전부 검출 | 판정 권위 부여, 단 out-of-band 유지 |

### 4.3 계층 합성 규칙 [제안]

```
                 ┌─────────────────────────────┐
   Tier B (OOB)  │ ExternalRelayVerdict         │  단조 악화만 가능
   워치독        │ (sidecar에 write, 읽기 전용) │  ─────────┐
                 └─────────────────────────────┘           │
                                                            ▼
   Tier A (in)   ┌─────────────────────────────┐   ┌──────────────────┐
   의무↔영수증   │ ReachabilityVerdict          │──▶│  RelayVerdict     │
                 └─────────────────────────────┘   │  (최종 건강)      │
   Tier C        ┌─────────────────────────────┐   └──────────────────┘
   영수증감사    │ ReceiptAudit (악화 불가)     │──────────▲
                 └─────────────────────────────┘           │
                 ┌─────────────────────────────┐           │
   구조 신호     │ RelayStallState              │───────────┘  행동 선택만
                 └─────────────────────────────┘
```

합성 규칙:
1. `RelayVerdict = worst(ReachabilityVerdict, ExternalRelayVerdict)`.
   구조 신호는 **합성에 참여하지 않는다** — 오직 어떤 복구 행동을 고를지에만 쓰인다.
2. `ExternalRelayVerdict`는 **악화만** 가능하다. 워치독이 "정상"이라 말해도
   Tier A의 `Unreachable`을 뒤집지 못한다. 반대로 워치독의 `Unreachable`은 Tier A의
   `Reachable`을 뒤집는다 (§5.3).
3. `ReceiptAudit`(Tier C)는 **악화 불가**. 감사 실패는 판정을 나쁘게 만들지 않고
   자체 알람만 낸다 (§7.2).

### 4.4 소비 지점 변경

| 소비자 | 현재 | 변경 |
|---|---|---|
| `/api/health` 집계 | `degraded` 플래그에 릴레이 도달 항 없음 | `reachability` 필드 추가. `Unreachable`이면 전체 `degraded=true` |
| `/api/health/detail` | `relay_stall_state`, `stall_shadow_verdict` | `reachability { verdict, oldest_unsatisfied_age_secs, uncovered_ranges, reason }` 추가. `stall_shadow_verdict` 제거. **#4974가 요구한 `last_successful_prose_relay_age_secs`가 여기서 충족된다** |
| `plan_relay_recovery` (`relay_recovery/decision.rs:314`) | `RelayStallState` 단독 입력 | `(RelayStallState, ReachabilityVerdict)` 입력. `ActiveForegroundStream + Unreachable` 조합에 대해 **비파괴 행동** 신설 (§7.1) |
| 워치독 `evaluate_active_foreground_coverage` (`relay_watchdog.py:1973`) | `relay_stall_state=="active_foreground_stream"` 을 coverage 근거로 씀 | `reachability != Reachable`이면 coverage를 **부여하지 않는다**. 즉 현재 "활성 스트림이니 desync는 정상"이라는 면죄부가 도달 신호에 종속된다 |

마지막 행이 #4986 형상1의 워치독 측 대응이다: 워치독은 27틱 동안
`attached_but_desynced`를 보고했지만 coverage 로직이 `active_foreground_stream`을
면죄부로 인정할 여지가 있었다 [확인 `relay_watchdog.py:1988-1993, 2044-2056`].
도달 신호가 있으면 그 면죄부가 사라진다.

---

## 5. #4381 워치독의 지위

### 5.1 결론 — **out-of-band 유지. 흡수하지 않는다. 판정 권위는 부여한다.**

세 가지를 분리해서 판단해야 한다:

| 속성 | 결정 | 이유 |
|---|---|---|
| **실행 위치** | out-of-band 유지 | #4381의 핵심 교훈: *"릴레이가 죽으면 감사 결과도 사용자에게 못 간다. 자기가 죽은 걸 자기 입으로 알릴 수 없다."* dcserver 프로세스 안으로 들어가는 순간 이 성질이 사라진다 |
| **알림 경로** | 독립 유지 (`discord-sendmessage` 봇 토큰 직접 POST) | 릴레이 전면 사망 중에도 도달 실증됨 [#4381 인용: `04:57:38Z`, `05:02:01Z`] |
| **판정 권위** | **부여** | 유일하게 세 형상 전부 검출. 권위 없이 두면 "관측은 했는데 시스템은 여전히 GREEN"이 반복된다 |

### 5.2 [제안] 권위 부여 메커니즘 — 단방향 sidecar 인테이크

워치독은 판정을 **파일로 쓴다**. dcserver는 그 파일을 **읽기만** 한다.

```
runtime/discord_external_relay_verdicts/<provider>/<channel_id>.json
{
  "verdict": "unreachable",
  "observed_at_epoch_ms": 1785321543885,
  "source": "relay_watchdog",
  "watchdog_state_version": 1,
  "reason": "gap",
  "lost_blocks": 3,
  "last_delivered_ts": 1785320700000
}
```

- 저장 패턴은 `completed_turn_ledger`를 그대로 따른다 [확인 `completed_turn_ledger.rs:24-31`]:
  `runtime/` 하위 전용 서브트리, `delivery_record::lock_record_path` flock 재사용,
  `runtime_store::atomic_write`. **새 락 메커니즘 없음. DB 마이그레이션 없음.**
- 읽기는 보수적: 없음/파손 → `None` → 판정에 **영향 없음** (`read_record_at`와 동일 원칙
  [확인 `delivery_record.rs:357-362`]).

### 5.3 왜 단방향인가 — 흡수 금지의 정확한 이유

**의존 방향을 만들면 out-of-band 가치가 사라진다.** 워치독이 dcserver의 API에
판정을 POST하도록 설계하면, dcserver가 죽었을 때 워치독의 판정도 갈 곳이 없다.
파일 쓰기는 dcserver 생존과 무관하다.

또한 **`ExternalRelayVerdict`는 악화만 가능**하게 한다(§4.3-2). 이유:

- 워치독이 `unreachable`이라 하면 → dcserver는 그것을 **믿어야 한다**. 워치독은
  채널 실물을 봤고 dcserver는 자기 장부만 봤다. 실물이 이긴다.
- 워치독이 `ok`라 하면 → **믿지 않는다.** 워치독은 텍스트 매칭 기반이라
  false negative가 원리적으로 가능하고(정규화 충돌, 60자 프로브 우연 일치),
  워치독 자체가 죽었거나 stale한 상태를 dcserver가 구분할 수 없다.
  `observed_at_epoch_ms`가 오래되면 그냥 무시한다.

즉 **out-of-band는 "실패를 단언할 권한"만 갖고 "건강을 단언할 권한"은 갖지 않는다.**
이것이 워치독 사망이 조용한 GREEN으로 퇴화하지 않게 하는 장치다.

### 5.4 워치독 자체의 생존 감시

#4381이 지적한 06-29 사고("plist도 스크립트도 사라졌고, 사라진 걸 아무도 몰랐다")를
막으려면 **워치독의 부재도 신호**여야 한다 [제안]:

- sidecar의 `observed_at_epoch_ms`가 `3 × poll_secs`(=360s) 이상 낡으면
  dcserver는 `ExternalVerdictStale`을 `/api/health/detail`에 노출한다.
- 이것은 **판정을 악화시키지 않는다**(§4.3-3 원칙과 동일 — 감사자의 부재는
  배달 실패의 증거가 아니다). 그러나 **그 자체로 별도 알람**이다.
- `deploy-release.sh`는 이미 plist를 설치/갱신한다 [확인 `deploy-release.sh:3101-3160`].
  배포 경로는 이미 닫혀 있다.

---

## 6. 세 형상 검출 증명 — **이 설계의 수용 기준**

> **[R2 대체됨 → §-1.4]** 이 절의 `AnchorWithoutReceipt`(§6.1)는 **삭제**됐고,
> §6.2의 `TuiRuntimeBinding` 독립 좌표 주장은 **철회**됐다. 개정 증명은 §-1.4다.

각 형상에 대해 (1) 현재 왜 통과하는지, (2) 제안 모델의 어느 항이 잡는지,
(3) 그것을 증명하는 **뮤테이션 테스트**를 명시한다. 테스트를 쓸 수 없으면 설계 실패다.

### 6.1 #4974 — zero-origin / stale `drain_restart` 고착

**현재 통과 경로** [확인]: 행 존재 + tmux alive + watcher attached → 구조 정상.
`current_msg_len=3` (`...` 앵커)이 영원히 안 채워짐. 세 가드가 각자 회피.

**제안 모델의 검출** — **2중**:

| # | 검출 항 | 지연 | 메커니즘 |
|---|---|---|---|
| 1 | **Tier A 의무 누적** | fail_bound | 트랜스크립트에 assistant text 블록이 계속 쌓이는데 `ConfirmedDeliveryReceipt`가 하나도 안 생김 (`terminal_delivery_committed=False`). `oldest_unsatisfied_age_secs`가 단조 증가 → `Unreachable` |
| 2 | **`AnchorWithoutReceipt`** [제안 파생 신호] | 즉시 | 인플라이트 행이 `current_msg_id`를 들고 있는데 그 message id를 주장하는 `DeliveredCommit.panel_msg_id`/영수증이 없고, 의무는 존재 → 모순. grace 없이 `Degraded` |

**결정적 성질**: Tier A는 `user_msg_id` / mailbox anchor / `restart_mode` / identity 비교를
**전혀 참조하지 않는다.** #4974의 본질은 *identity 비교가 틀렸다*는 것인데, 도달 층은
identity 비교를 하지 않으므로 **identity 오류로 눈멀 수 없다.** 이것이
f4f816438(identity 수정)과 본 설계가 겹치지 않고 직교하는 이유다.

**뮤테이션 테스트** [제안]:
```
tests: reachability_unreachable_when_row_frozen_with_growing_obligations
  given  인플라이트 행 restart_mode=drain_restart, terminal_delivery_committed=false
  and    트랜스크립트에 assistant text 블록 3개 추가 (fail_bound 초과 경과)
  and    delivery_record에 영수증 0건
  then   ReachabilityVerdict == Unreachable
  and    RelayStallState 는 여전히 ActiveForegroundStream (변경 없음 확인)
  and    최종 RelayVerdict != Healthy
```
마지막 두 줄이 중요하다 — **구조 신호가 여전히 GREEN인 채로 최종 판정이 RED**가 되는 것을
직접 단언한다. 이것이 §4.1 극성 반전의 회귀 잠금이다.

### 6.2 #4986 형상1 — `output_path` ENOENT / 좌표계 분열

**현재 통과 경로** [확인, §1.4에서 7단계 추적 완료]: `fs::metadata` 실패 →
`last_capture_offset=None` → `capture_lagged=false` → `desynced=false` →
`ActiveForegroundStream` → `ObserveOnly`.

**제안 모델의 검출** — **3중**:

| # | 검출 항 | 지연 | 메커니즘 |
|---|---|---|---|
| 1 | **`TranscriptCoordinateDivergence`** | 즉시 (1틱, 30s) | 행의 `output_path`는 stat 실패인데, **독립 해결**한 트랜스크립트는 살아 있고 크기가 행의 `last_offset`(4,777,773)과 24KB 이내로 정합. 두 좌표가 서로 다른 파일을 가리킨다는 **모순의 직접 관측** |
| 2 | **Tier A 의무 누적** | fail_bound | 독립 해결 경로에서 읽은 native 트랜스크립트에 assistant 블록 존재, 영수증 0 → `Unreachable` |
| 3 | Tier B 워치독 | ~14분 (실측) | 현행 그대로 |

**#1이 "조건 하나 더 추가"가 아닌 이유** — 이 구분이 이 설계의 핵심이다.

```
증상 패치:  if !Path::new(&row.output_path).exists() { alarm }
            → 같은 권위(행)에서 파생. 형상2(행 없음)에 대해 여전히 무력.

모델 변경:  divergence(row_coordinate, independently_resolved_coordinate)
            → 제2 권위 도입. 형상2도 같은 항으로 잡힌다.
```

두 번째는 판정 함수에 **행과 독립인 항**을 추가한다. 첫 번째는 안 한다.
그리고 in-band에 이미 제2 좌표가 **존재한다** [확인 `health/snapshot.rs:227-244`]:

```rust
fn resolve_bound_selector(inflight_output_path, inflight_session_id, binding) {
    let bound_output_path = non_blank(inflight_output_path)
        .or_else(|| non_blank(binding.map(|b| b.relay_output_path())));   // ← fallback일 뿐
```

`TuiRuntimeBinding::relay_output_path()`가 이미 있지만 **행이 값을 갖고 있으면
쳐다보지도 않는 fallback chain**이다. 이 설계는 그것을 **비교 쌍**으로 바꾼다.
구현 비용이 매우 낮고(양쪽 값 이미 계산됨) 구조적 효과는 크다.

**뮤테이션 테스트** [제안]:
```
tests: reachability_detects_row_path_vs_resolved_transcript_divergence
  given  행 output_path = <존재하지 않는 wrapper 경로>, last_offset = 4_777_773
  and    독립 해결 트랜스크립트 = <존재, size 4_802_043>
  then   ReachabilityVerdict == Unknown{TranscriptCoordinateDivergence}
  and    desynced == false            ← 기존 신호는 여전히 눈멀어 있음을 단언
  and    최종 RelayVerdict != Healthy
```

### 6.3 #4986 형상2 — 인플라이트 행 없는 라이브 턴

**현재 통과 경로** [확인, §1.4에서 분류기 손계산으로 재현]:
`inflight_state_present=False` → `desynced`가 **구조적으로 false 불가능**
→ `OrphanPendingToken`/`QueueBlocked` 조건 미달 → `ActiveForegroundStream`.

**제안 모델의 검출** — **2중**:

| # | 검출 항 | 지연 | 메커니즘 |
|---|---|---|---|
| 1 | **`RowlessActiveTurn`** | 짧은 bound (60s [제안]) | mailbox `has_cancel_token=True` + `active_user_message_id` 존재 + `inflight_state_present=False` 가 bound 이상 지속. **워터마크를 기록할 행이 없다 = 배달 불가능의 구조적 증명**. 턴 승인~행 생성 사이 정상 창을 덮기 위해 즉시가 아닌 bound |
| 2 | **Tier A 의무 누적** | fail_bound | **의무 산출이 행을 필요로 하지 않는다.** tmux session → 런타임 바인딩 → 트랜스크립트 독립 해결. 행이 0개여도 의무는 정상 산출됨 → 영수증 0 → `Unreachable` |

**결정적 성질**: #2가 이 형상의 진짜 답이다. 현재 모델은 `desynced`가
`inflight_state_present`를 요구해서 행 부재 시 실명하는데 [확인 `session_enrichment.rs:129-136,186-190`],
제안 모델의 의무 산출 경로에는 **행이 등장하지 않는다.**

**뮤테이션 테스트** [제안 — 이것이 설계 전체의 falsification 테스트]:
```
tests: reachability_unreachable_when_inflight_row_absent_during_live_turn
  given  mailbox active(cancel token + active_user_message_id)
  and    인플라이트 행 파일을 **삭제**
  and    트랜스크립트에 assistant text 블록 존재, 영수증 0건
  then   ReachabilityVerdict == Unreachable
  and    desynced == false           ← 구조적으로 false일 수밖에 없음을 단언
  and    RelayStallState == ActiveForegroundStream   ← 구조 판정 불변
  and    최종 RelayVerdict != Healthy
```

> **이 테스트를 작성할 수 없으면 설계는 실패다.** 판정 함수에서 행을 완전히
> 제거해도 살아남는 항이 있는지를 직접 묻는 테스트이기 때문이다.

### 6.4 요약표 — 수용 기준 충족

| 형상 | Tier A 의무 | 즉시 파생 신호 | Tier B | 검출 |
|---|---|---|---|---|
| #4974 | ✅ `Unreachable` | `AnchorWithoutReceipt` | ✅ | **2중** |
| #4986-1 | ✅ `Unreachable` | `TranscriptCoordinateDivergence` | ✅ | **3중** |
| #4986-2 | ✅ `Unreachable` | `RowlessActiveTurn` | ✅ | **2중** |

세 형상 모두 **행과 독립인 항으로 잡힌다.** 하나라도 행 파생 신호에만 의존하는
형상이 없다는 것이 이 표의 요점이다.

---

## 7. 오탐 비용 — fail-open / fail-closed 경계

### 7.1 [제안] 근본 규칙 — 도달 판정은 **구조적으로 비파괴다**

> `ReachabilityVerdict`는 **어떤 파괴적 행동의 근거도 될 수 없다.**
> 턴 취소, tmux kill, mailbox anchor 정리, 인플라이트 행 force-clean,
> destructive cancel — 전부 금지. 이 권한은 기존 구조 판정기가 그대로 보유하며
> 기존의 live-evidence 가드도 그대로 유지된다.

도달 판정이 할 수 있는 것은 정확히 세 가지:

| 행동 | 성격 | 오탐 시 최대 피해 |
|---|---|---|
| **알람** (health degraded, 워치독 알림, 이슈 등록) | 비파괴 | 노이즈 |
| **프론티어 전진 거부** | 보수적 (덜 하는 쪽) | 중복 억제 권한이 약해짐 → 중복 1건 가능성 |
| **미커버 range 재배달 시도** | 가산적, 기존 dedup 하위 | 재배달이 dedup에 막혀 no-op |

**턴을 죽이는 경로가 아예 없으므로, "멀쩡한 턴을 죽인다"는 오탐 비용은
설계상 발생 불가능하다.** 이것이 §5.1에서 워치독을 흡수하지 않은 것과 같은 논리다 —
새 판정기에 파괴 권한을 주지 않는 것으로 오탐 비용의 상한을 먼저 고정한다.

### 7.2 fail-closed / fail-open 경계

두 종류의 "모름"을 **반대로** 다룬다. 이 비대칭이 설계의 핵심이며, 근거가 다르다.

| 상황 | 정책 | 근거 |
|---|---|---|
| **의무 커버리지의 부재** (의무는 있는데 영수증이 없음) | **fail-closed** — 건강 아님 | 시스템이 자기 일을 했다는 증명을 못 내놓았다. §1.5의 "증거 부재 = 건강" 결함을 정확히 뒤집는 지점 |
| **의무 산출 불가** (`Unknown{TranscriptUnresolved}` 등) | **fail-closed (bound 후)** — `Reachable`이 아님 | 같은 이유. 단 짧은 정상 창(턴 승인 직후 등)을 위해 bound |
| **Tier C 감사 프로브 실패** (429, 5xx, 타임아웃) | **fail-open** — 판정 악화 없음 | 감사자가 못 돈 것은 배달 실패의 증거가 아니다. 여기서 fail-closed하면 Discord rate limit이 곧바로 전면 RED가 된다 |
| **Tier C 감사자 자체의 연속 실패** | **별도 fail-closed 알람** | #4381 교훈: "감사가 아예 돌지 않았다"가 사고의 1번 원인. 워치독의 `read_fail_alert_after=5` 선례 [확인 `relay_watchdog.py:279-281`] |
| **`ExternalRelayVerdict` 부재/stale** | **fail-open + 별도 알람** (§5.4) | 동일 논리 |

정리하면:

```
"우리가 배달했다는 증거가 없다"        → fail-closed  (판정 악화)
"우리가 확인해 볼 수단이 없었다"       → fail-open   (판정 유지) + 감사자 알람 별도
```

이 두 문장을 혼동하는 것이 #4381 사고의 구조였다. 감사자가 안 돈 것을
"이상 없음"으로 읽었다.

### 7.3 재배달 오탐 — 중복 배달 위험의 정확한 상한

재배달(§7.1 3번)은 **기존 dedup 권위 하위에서만** 실행된다:
공유 delivery lease (I9), `DeliveredCommit` 프론티어 (I10),
edit-failure 게이트 (I11), `#4081` fingerprint.

따라서 도달 오탐 → 중복 배달 경로는 **이 네 개를 전부 뚫어야** 성립한다.

그런데 **반대 위험이 실재한다**: `#4081` 가드는 정상 배달을 중복으로 오판한
이력이 있다 [확인 `tmux_watcher/turn_identity.rs:333` — *"the #4081 guard misjudged it
as a duplicate and refused delivery — stranding..."*]. 즉 재배달이 **조용히 억제**될 수 있다.

**[제안] 따라서 재배달 결과는 3분류로 typed 기록한다:**

| 결과 | 의미 | 처리 |
|---|---|---|
| `Redelivered` | 새 POST 확인됨 | 의무 충족, 정상 |
| `SuppressedByDedup` | dedup이 거부 | **성공으로 기록 금지.** 별도 카운터 + 의무는 **미충족 유지** |
| `Failed` | 전송 실패 | 재시도 대상 |

`SuppressedByDedup`을 성공으로 접으면 **"고쳤다고 보고했는데 여전히 죽어 있던"**
#4381의 1시간 34분이 재현된다. 이 세 분류는 계약 §Task-32 "Terminal ACK" 행의
typed outcome 원칙(`Delivered`/`FreshDelivered`/`NotDelivered`/`Unknown`)과 같은 규율이다.

### 7.4 오탐률 자체가 측정 가능해야 한다

`SuppressedByDedup` 카운터가 곧 **도달 판정 오탐의 직접 측정치**다
(의무가 미충족이라 판정했는데 dedup은 이미 배달됐다고 판단한 경우).
이 카운터가 유의하게 오르면 의무 산출 규칙(§2.4)이 틀린 것이다.

**[제안] S1 30일 관측의 종료 조건**: `SuppressedByDedup / total_obligations < 0.1%`.
초과하면 S3(판정 권한 부여)를 진행하지 않는다.

---

## 8. 계약 변경 조항 목록

`docs/relay-state-contract.md` 대비 변경분. **본 설계는 이 계약을 개정한다.**

### 8.1 §Task-32 표 — 행 추가

| Surface | Current production authority | Required target / gap | Identity and linearization | Forbidden fallback / acceptance |
|---|---|---|---|---|
| **Terminal reachability** (신규) | **없음.** 구조 liveness(`relay_stall_state`, `desynced`)가 사실상 배달 대리 지표로 쓰이나 배달을 측정하지 않음 | 소스 파생 의무 원장 + 기존 영수증/프론티어의 뺄셈이 1급 건강 신호 | 의무는 `ExactJsonlSourceIdentity` 재사용. 의무 산출은 **인플라이트 행과 독립**이어야 함 | 구조 liveness, watcher attach, tmux alive, capture offset 전진은 **배달 증거가 아니다**. 증거 부재는 건강이 아니다 |

### 8.2 신규 불변식

> **[R2 대체됨 → §-1.5]** I13은 I10(intentional classified drop)과 충돌해 **재작성**됐고,
> I14의 "컴파일러가 강제한다"와 I15의 타입 제약 주장은 **철회**됐다(소스 린트/리팩터로 대체).

#### I13. 도달 의무는 확정 영수증으로만 소멸한다
- Definition: `ReachabilityObligation` (신규, `health/reachability/obligation.rs`).
- Producer: 의무 프로버가 독립 해결한 트랜스크립트에서 assistant text range를 산출.
- Consumer: `ReachabilityVerdict` 계산.
- Invariant: 의무는 `ConfirmedDeliveryReceipt`(E1) **및** `DeliveredCommit`(E2)이
  그 range를 덮을 때만 소멸한다. 프론티어 전진, capture offset 전진, `last_offset` 전진,
  구조 liveness, grace 만료로는 **소멸하지 않는다.**
- Violation surface: 소비 워터마크가 배달 워터마크로 오인되어 미배달이 소멸 처리됨
  (= #4986 형상1의 정확한 형태 — `last_offset`이 4.7MB로 전진했으나 0바이트 배달).
- Invariant key: `obligation_cleared_by_receipt_only`.

#### I14. 의무 산출은 인플라이트 행과 독립이다
- Invariant: 의무 프로버는 `InflightTurnState::output_path`를 **의무 산출의 입력으로
  사용하지 않는다.** 행 경로는 오직 *divergence 비교의 피연산자*로만 읽는다.
- Violation surface: 행이 손상/부재일 때 판정기가 동시에 실명 (§1.3 — 세 형상 공통 원인).
- 강제 방법 [제안]: 프로버 모듈이 `InflightTurnState`를 **의존성으로 갖지 않게**
  모듈 경계를 긋는다(비교기만 양쪽을 본다). 컴파일러가 강제한다.
- Invariant key: `obligation_source_row_independent`.

#### I15. 도달 판정은 파괴적 행동을 승인하지 않는다
- Invariant: `ReachabilityVerdict`는 turn cancel / tmux kill / mailbox 정리 /
  인플라이트 force-clean의 근거가 될 수 없다 (§7.1).
- Violation surface: 도달 오탐이 라이브 턴을 죽임.
- 강제 방법 [제안]: `plan_relay_recovery`가 `ReachabilityVerdict`를 받되,
  파괴적 `RelayRecoveryActionKind`를 산출하는 분기에는 **전달되지 않는** 타입 분리.
- Invariant key: `reachability_verdict_non_destructive`.

### 8.3 기존 불변식 개정

| 조항 | 변경 | 내용 |
|---|---|---|
| **I4** (confirmed-end 단일 소유자) | **명문화 추가** | confirmed-end 워터마크는 **배달 증거가 아니다.** 이것은 소유권/중복 억제 좌표다. 건강 판정이 이 값의 전진을 배달로 읽는 것을 금지 |
| **I6** (`last_offset` owner-gated/monotonic) | **명문화 추가** | `last_offset`은 **소비(consumption) 워터마크**다. #4986 형상1은 `last_offset=4,777,773`으로 정상 전진하면서 0바이트를 배달했다. "전진 = 건강"의 해석을 계약에서 명시적으로 금지 |
| **I10** (idle cursor는 확정 커밋만 소비) | **원칙 확장** | 같은 원칙을 *건강 판정*에 확장: 판정도 확정 커밋만을 배달 증거로 인정한다 (I13) |
| **I11** (edit 실패는 재전송 권한 아님) | **변경 없음** | 재배달 경로가 이 게이트 하위에 놓임을 §7.3에서 재확인 |
| **I5** (duplicate-suppression handshake) | **변경 없음** | 재배달은 이 프로토콜을 우회하지 않는다 |

### 8.4 계약 문서 게이트 동반 요구 [확인 `relay-state-contract.md:87-148`]

계약 문서를 고치면 **두 반쪽 게이트를 동시에 만족**해야 한다:

1. 새 `sym:` 앵커마다 `#[cfg(test)] mod relay_state_contract_refs` 블록에
   컴파일러 검증 참조(`use <path> as _;` 등)를 추가. 허용 위치는 6개 모듈:
   `inflight/store.rs`, `turn_bridge/terminal_delivery.rs`, `tmux_watcher/liveness.rs`,
   `router/message_handler/watchdog.rs`, `mailbox_finish.rs`, `session_relay_sink.rs`.
   **이 중 `session_relay_sink.rs`는 핫파일**이다 (§9 직렬 판정에 반영).
2. `scripts/check_contract_symbol_refs.py` 집합 비교 통과.
   블록 cfg 게이트는 `#[cfg(test)]` 또는 `#[cfg(all(test, unix))]` 만 허용.

**[제안] 회피 전략**: I13–I15의 `sym:` 앵커를 전부 신규 `health/reachability/**` 심볼로
잡고, 참조 블록은 **`inflight/store.rs`** (비핫파일)에 몰아넣는다.
`session_relay_sink.rs`를 건드리지 않아 §9의 직렬 제약을 피할 수 있다.
가시성(`pub(in crate::services::discord)`)이 이를 허용하는지는 구현 시 확인 필요 [추정].

### 8.5 마이그레이션

**DB 마이그레이션 불필요** [제안 근거]: 의무 원장과 외부 판정 sidecar는
`completed_turn_ledger` / `delivery_record`와 동일한 파일 sidecar 패턴
(`runtime/<subtree>/<provider>/<channel>.json`, flock + `atomic_write`)
[확인 `completed_turn_ledger.rs:24-31`]. 따라서
`migrations/postgres/immutable-checksums.json` 등록 **불요**.

다만 계약 §Task-32가 요구하는 *canonical durable admission store*(PostgreSQL)로
의무 원장을 승격하는 순간에는 마이그레이션이 필요하며,
**그때는 `migrations/postgres/immutable-checksums.json` 체크섬 등록이 동반 요구사항**이다.
본 설계는 그 승격을 **범위 밖**으로 둔다 (호스트 로컬 sidecar는
`relay-live-state-taxonomy.md`가 정의한 host-local 등급 그대로).

---

## 9. PR 슬라이스 분해

### 9.1 핫파일 접촉 총평

**S1–S6은 핫파일(`turn_bridge/mod.rs`, `tmux_watcher.rs`, `session_relay_sink.rs`,
`turn_finalizer.rs`)을 전혀 건드리지 않는다.** 이것은 우연이 아니라 설계 결정이다:
도달 판정 층은 *관측*이므로 전송 경로에 손댈 이유가 없다. 전송 경로를 건드리는 것은
S7(재배달)뿐이고, S7은 **범위 밖으로 분리**한다(§9.3).

`tmux_watcher.rs`(3569줄 / 캡 3571, 여유 2줄)에 **1줄도 추가하지 않는다.**
따라서 #4229 W7b 분해(`scratchpad/design-4712.md` S-A)는 **S1–S6의 선행조건이 아니다.**
S7에 대해서만 선행조건이 된다.

### 9.2 슬라이스 표

| # | 이름 | 파일 | 예상 prod 증감 | 핫파일 | 병렬/직렬 | 선행 |
|---|---|---|---|---|---|---|
| **S1** | 의무 프로버 (관측 전용) | **신규** `health/reachability/{mod,discovery,obligation,ledger}.rs` | +~940 (파일당 ≤320) | 없음 | **병렬 가능** | – |
| **S2** | 영수증 인덱스 read 경로 | **신규** `outbound/receipt_index.rs` | +~240, `delivery_record.rs` **+0** | 없음 | **병렬 가능** | – |
| **S3** | 판정 합성 + 권한 부여 | `relay_health.rs`, `health/snapshot.rs`, `relay_recovery/decision.rs`, `server/routes/health_api.rs` | +~210 / `stall_verdict` 경로 제거로 −~40 | 없음 | 직렬 (S1·S2 후) | S1, S2, **30일 관측** |
| **S4** | divergence / rowless 파생 신호 | `health/reachability/divergence.rs` (신규), `health/snapshot.rs` (비교 쌍 배선) | +~260 | 없음 | 직렬 (S1 후), S3와 병렬 가능 | S1 |
| **S5** | Tier C 영수증 재확인 프로브 | `health/reachability/reconfirm.rs` (신규) | +~280 | 없음 | **병렬 가능** (S3 후) | S2 |
| **S6** | 워치독 판정 인테이크 | `scripts/relay_watchdog.py`, `health/reachability/external_verdict.rs` (신규) | py +~120, rs +~180 | 없음 | **병렬 가능** | – |
| **S7** | 미커버 range 재배달 | **범위 밖** — §9.3 | – | **YES** | 직렬 단독 | #4229 W7b |

### 9.3 S7을 분리하는 이유

§5의 수용 기준은 **검출**이다. 재배달(remediation)은 별개 문제이고,
전송 경로(`session_relay_sink.rs` 또는 `tmux_watcher.rs`)를 건드려야 한다.

- `tmux_watcher.rs` 경유 시: 여유 2줄 → **#4229 W7b 분해 선행 필수**
  (`scratchpad/design-4712.md` S-A: 128줄, 외부 제어흐름 0, GO 판정됨).
- `session_relay_sink.rs` 경유 시: 핫파일 #3016 규칙 → **직렬 단독**.
- 대안: `health/relay_auto_heal.rs`의 기존 redrive 경로 재사용 (핫파일 아님).
  단 `relay_auto_heal.rs`는 2239줄로 이미 giant 임계 초과 → 레지스트리 admission 노트 필요.
  **[추정]** 이 경로가 가장 저렴해 보이나 redrive 시맨틱이 range 재배달에
  적합한지 미검증. S7 착수 시 별도 조사 필요.

**S7 없이도 세 형상은 전부 검출된다(§6).** 재배달은 후속.

### 9.4 슬라이스별 동반 요구사항

#### 공통 (전 슬라이스)
1. `python3 scripts/generate_inventory_docs.py` → tracked-output `git diff` 클린
2. `python3 scripts/check_agent_maintenance_docs.py --warning-only --line-count-gate`
   → `agent-maintenance freshness check passed`
3. `cargo fmt --all --check`, `cargo check --workspace --all-targets`

#### `docs/agent-maintenance/change-surfaces.md`
**[제안] 신규 surface 항목 `relay_reachability` 추가**:
- canonical_modules: `src/services/discord/health/reachability/**`,
  `src/services/discord/outbound/receipt_index.rs`
- 동반 수정 요구: 의무 규칙 변경 시 **반드시** `scripts/relay_watchdog.py`의
  `assistant_blocks`/`is_harness_control_assistant_record`와 golden fixture를 함께 갱신 (§2.4)
- do_not_edit_without_migration_plan: n/a (신규)

#### `scripts/check_test_lane_coverage.py` [확인 — baseline은 immutable reference의 subset만 허용]
신규 `#[cfg(test)] mod`는 **curated lane이 명시적으로 선택**해야 한다.
lane은 `justfile`의 개별 필터 라인이다 [확인 `justfile:37-78` 패턴].

**[제안] 각 슬라이스는 자기 lane 라인을 함께 추가한다**:
```
cargo test --lib services::discord::health::reachability::obligation::tests -- --skip _pg --skip pg_ --skip postgres
cargo test --lib services::discord::health::reachability::verdict::tests   -- --skip _pg --skip pg_ --skip postgres
cargo test --lib services::discord::outbound::receipt_index::tests         -- --skip _pg --skip pg_ --skip postgres
```
**baseline(`scripts/test_lane_coverage_baseline.txt`, 현재 683행)에 신규 항목 추가는 불가.**

#### 계약 문서 게이트 (S3에서 I13–I15 도입 시)
§8.4의 두 반쪽 게이트. 참조 블록은 **`inflight/store.rs`** 에 배치하여
핫파일 `session_relay_sink.rs`를 회피한다.

#### giant 파일
- `health/snapshot.rs` 1279줄 — 이미 giant. S3/S4에서 +~60 → **레지스트리 admission 노트 필요**.
  가능하면 배선만 남기고 로직은 `reachability/**`에 둔다.
- `health/recovery.rs` 6042줄 — giant 레지스트리 등록됨 [확인 `giant_file_registry.toml:107`].
  **틱 호출을 여기 추가하지 않는다.** 도달 틱은 `runtime_bootstrap/spawns.rs`에서
  독립 태스크로 띄운다 (`STALL_WATCHDOG_INTERVAL_SECS`와 동일 주기, 별도 루프).
- `outbound/delivery_record.rs` 5093줄 — giant 레지스트리 등록됨 [확인 `:811`].
  **+0줄.** 읽기는 기존 `read_record_at`를 신규 `receipt_index.rs`에서 호출.

#### 모듈 크기 경계 [제안]
`health/reachability/**`는 `tmux_watcher/**`의 700 캡 대상이 **아니다**
(캡은 `scripts/audit_maintainability_config.toml`에서 경로별 지정).
그래도 1000 giant 임계 아래를 유지하도록 미리 분할 경계를 긋는다:

| 파일 | 책임 | 목표 |
|---|---|---|
| `mod.rs` | 타입 재export, 틱 진입점 | ≤120 |
| `discovery.rs` | 트랜스크립트 독립 해결 (readdir/stat/선택 규칙) | ≤280 |
| `obligation.rs` | JSONL 파싱 → assistant text range 산출 (§2.4 규칙) | ≤320 |
| `ledger.rs` | sidecar 원장 read/write (flock + atomic_write) | ≤240 |
| `verdict.rs` | 뺄셈 + `ReachabilityVerdict` 합성 (순수 함수) | ≤200 |
| `divergence.rs` (S4) | 행 좌표 ↔ 해결 좌표 비교 | ≤260 |
| `reconfirm.rs` (S5) | Tier C 프로브 + rate budget | ≤280 |
| `external_verdict.rs` (S6) | 워치독 sidecar 읽기 | ≤180 |

`verdict.rs`는 **순수 함수**로 유지한다 — `relay_health.rs`의
`RelayStallClassifier::classify`가 순수 테이블 구동인 것과 같은 규율
[확인 `relay_health.rs:154-191`, 테이블 구동 테스트 `:197-312`].

---

## 10. Adversarial self-check — 이 설계를 내가 공격한다

### A1. "도달 확인 자체가 실패하면?"

**공격**: 의무 프로버의 트랜스크립트 독립 해결이 틀린 파일을 고르면,
의무가 통째로 허구가 되어 **영구 false `Unreachable`** 이 된다. 알람 피로 →
운영자가 음소거 → #4381 이전 상태로 정확히 회귀. **알람은 무시되는 순간 0의 가치다.**

**방어**:
- 해결 불가 → `Reachable`이 아니라 `Unknown{TranscriptUnresolved}`. 두 상태를
  **UI/알람에서 구분**한다. `Unknown`은 "릴레이가 고장"이 아니라 "관측 불능"이므로
  다른 문구·다른 쿨다운을 갖는다.
- 워치독의 선례 채택: `selector_divergence_confirmed` + `swap_confirm_secs=300`
  [확인 `relay_watchdog.py:293-298, 2215`] — 발산은 **300초 지속 확인 후**에만 단언.
  post-swap rebind lag를 stuck으로 오독하지 않기 위한 것이고, 같은 이유가 여기 적용된다.
- Tier B와의 **교차 검증**: 워치독도 독립으로 트랜스크립트를 고른다. 두 해결이
  다르면 그것 자체가 `Unknown`이며 자동 이슈 등록 대상.

**잔존 리스크 [인정]**: 두 구현이 **같은 방식으로 틀리는** 경우(예: 워크트리 명명 규칙
변경으로 양쪽 discovery가 동시에 새 디렉터리를 놓침)는 방어하지 못한다.
이건 상관 실패(correlated failure)이고, 근본 해법은 트랜스크립트 경로를
*추론*이 아니라 *런타임이 선언*하게 만드는 것이다 — 그건 별도 설계다.

### A2. "Discord API가 느리거나 rate limit이면?"

**공격**: Tier C가 429를 맞으면 판정이 나빠지고, 429는 부하가 높을 때 발생하니
**부하 급증 시 전면 false RED**가 된다.

**방어**: §7.2에서 이미 **Tier C는 fail-open**(판정 악화 불가)으로 못박았다.
그리고 Tier A는 **Discord API를 아예 호출하지 않는다** — 영수증은 POST 시점에
이미 로컬에 기록돼 있다. rate limit이 도달 판정의 주 경로에 영향을 줄 수 없는
구조다. 이것이 "매 메시지 재조회"를 배달 *정의*에서 배제한(§2.3-2) 실질 이유다.

**잔존 리스크 [인정]**: rate limit이 **실제 배달**을 지연시키면 의무가 정상적으로
쌓인다 → `Degraded`. 이건 오탐이 아니라 정탐(배달이 실제로 늦다)이지만
**운영자가 "또 rate limit이네"로 학습하면 A1의 알람 피로로 수렴**한다.
완화: 429 발생을 `Degraded` 사유에 첨부해 "전송 지연"과 "전송 실종"을 구분한다.

### A3. "재시작 경계에서 도달 증거가 사라지면?"

**공격**: 계약이 명시한 success→commit 크래시 창(`Unknown` 클래스,
`relay-state-contract.md:45-48`)에서 죽으면, POST는 성공했는데 영수증이 없다.
재시작 후 의무는 남고 영수증은 없으니 **false `Unreachable`** + 재배달 시 **중복**.

**방어**:
- 의무는 durable sidecar이므로 재시작을 견딘다(설계 의도).
  영수증도 durable(`DeliveredCommit`이 재시작 hydration의 근거
  [확인 `delivery_record.rs:93-96`]). 손실되는 것은 오직 크래시 창의 한 건.
- **판정은 하되 행동은 하지 않는다** — §7.1에 의해 도달 판정은 파괴적 행동을
  못 하고, 재배달은 I11의 "positive delivered-elsewhere proof 없으면 보존"
  규율 하위이므로 [확인 `relay-state-contract.md:456-458`] 자동 중복이 되지 않는다.
- 즉 이 창의 결과는 **"경보는 뜨지만 아무것도 파괴되지 않는다"** — 허용 가능한 실패 모드.

**잔존 리스크 [인정]**: 경보가 뜬 후 사람이 수동 개입해 중복을 만들 수 있다.
#4986이 *의도적으로 손대지 않은* 이유가 정확히 이것("이전 사고에서 수동 정리가
`user_msg_id=0` 행을 만들어 같은 병을 재발시킨 전례"). **알람 문구에
"수동 정리 금지" 를 명시**해야 한다. 설계 산출물에 포함.

### A4. 이중 오라클 드리프트 — **가장 과소평가되기 쉬운 위험**

**공격**: 의무 규칙이 Rust와 Python에 각각 구현된다. 두 구현이 갈라지면
in-band와 out-of-band가 서로 다른 의무 집합을 갖고, **둘 중 하나는 상시 오답**이다.
그런데 서로가 서로를 교차검증한다고 믿고 있으므로(§A1 방어) **방어가 함께 무너진다.**

**방어**: §2.4의 golden fixture 코퍼스를 **S1의 하드 인수 조건**으로 둔다.
같은 `.jsonl` 입력에 대해 Rust 프로버와 Python 워치독이 **바이트 동일한
range 집합**을 산출해야 하며, 불일치는 CI 실패다.

**이것이 이 설계에서 가장 깨지기 쉬운 지점이라고 판단한다.** 두 언어, 두 배포
경로(`cargo` vs `deploy-release.sh`가 스크립트를 복사 [확인 `deploy-release.sh:3106-3110`]),
두 리뷰 문화. **fixture 게이트 없이 S1을 머지하면 안 된다.**

### A5. "이것도 결국 증상 패치 아닌가?"

**공격**: `TranscriptCoordinateDivergence`, `RowlessActiveTurn`은
결국 "조건을 두 개 더 추가한" 것 아닌가?

**방어**: 판별 기준은 **그 조건이 어느 권위에서 파생되는가**다.
- `if !row.output_path.exists()` → 행 파생. 형상2에 무력. **증상 패치.**
- `divergence(row_coord, independent_coord)` → 제2 권위 도입. 세 형상 공통.

그리고 이 두 파생 신호는 **주 검출기가 아니다.** 주 검출기는 Tier A 의무 뺄셈이고,
세 형상 전부 그것 하나로 잡힌다(§6.4 표의 첫 열). 파생 신호는 *지연 단축*
장치일 뿐 없어도 검출은 성립한다. **모델의 무게중심이 의무 뺄셈에 있는지**가
증상 패치와의 구분선이고, §6.4 표가 그것을 보인다.

**자기 반박 [인정]**: 그럼에도 `RowlessActiveTurn`은 mailbox+행 조합을 보는
구조 신호가 맞다. 이것 하나만 놓고 보면 증상 패치와 구별되지 않는다.
**이 신호는 §6.3의 뮤테이션 테스트를 통과시키는 데 필요하지 않다**(테스트는 Tier A로
통과한다). 만약 리뷰에서 논쟁이 되면 **S4에서 `RowlessActiveTurn`을 빼도
설계는 성립한다.** 빠져도 되는 부분임을 명시해 둔다.

### A6. 임계값이 근거 없이 정해지면

**공격**: 120s/420s/60s가 측정 없이 확정되면 오탐 또는 미탐이 보장된다.
"긴 툴 호출 중 산문이 합법적으로 미게시로 머무는 시간"의 분포를 **아무도 모른다.**

**방어**: §3.4에서 S1을 **관측 전용 30일**로 못박았고, §7.4에서
`SuppressedByDedup < 0.1%`를 S3 진입 조건으로 걸었다.
**S1 머지 시점에 임계값을 코드에 넣지 않는다** — 히스토그램만 수집한다.

### A7. 의무가 0인 정상 턴 — **[R2 철회됨 → §-1.4 말미]**

> 아래 "이건 정확한 동작이다"는 **틀렸다.** transcript ingestion이 죽어도 의무 0으로
> 보여 동일하게 GREEN이 된다. `Reachable` 선언에 화신 생존 양성 증거를 요구하도록 개정.

**공격**: 툴만 쓰고 산문을 안 낸 턴은 의무가 0 → `Reachable`. 릴레이가 죽어 있어도 GREEN.

**방어**: 이건 **정확한 동작**이다. 배달할 것이 없으면 배달 실패도 없다.
릴레이 사망은 다음 산문 블록이 생기는 즉시 의무로 잡힌다.
다만 **"장시간 의무 0"은 그 자체로 관측 대상**이어야 한다 — 세션이 산문을
전혀 안 내는 것이 정상인지 아닌지는 도달 층이 답할 문제가 아니고,
기존 `idle_quiet_secs`(2h) 계열 신호의 영역이다 [확인 `relay_watchdog.py:270`].

### A8. 성능 — 내가 든 근거가 약한 곳 [인정]

§3.2에서 "Tier A가 `tmux has-session` fork+exec보다 싸다"고 썼는데
**벤치마크를 돌리지 않았다.** [추정] 표기했지만, 채널 수가 많고 트랜스크립트가
동시에 폭주하면 30s 틱 안에 못 끝날 가능성이 있다.
**S1의 인수 조건에 틱 소요 시간 계측(p99)을 포함**해야 한다.

---

## 11. GO / NO-GO

> **[R2 대체됨 → §-1.7]** 아래 판정은 1라운드 것이다. 개정 판정은 **더 좁다**:
> 관측 슬라이스만 GO, 판정 권한(S3)은 blocker 전 NO-GO. "DB migration 불요"는
> GO 근거에서 **삭제**됐다.

### (1라운드, 무효) GO — S1, S2, S6 즉시. S4는 S1 직후. S3·S5는 관측 게이트 통과 후.

근거:
1. 세 형상 전부에 대한 검출 경로가 **행과 독립인 단일 항(Tier A 의무 뺄셈)** 으로
   성립하고, 각각에 대해 **구조 신호가 GREEN인 채 최종 판정이 RED**임을 단언하는
   뮤테이션 테스트를 쓸 수 있다 (§6.1–6.3).
2. **핫파일 접촉 0**, `tmux_watcher.rs` **+0줄**, **DB 마이그레이션 불요**.
   #4229 W7b는 S1–S6의 선행조건이 아니다 (§9.1).
3. 오탐 비용의 상한이 설계상 고정된다 — 도달 판정에 파괴 권한이 없으므로
   "멀쩡한 턴을 죽인다"가 발생 불가능하고, 중복은 기존 dedup 4겹 하위다 (§7.1, §7.3).
4. #4974 이슈 본문이 직접 요구한 `last_successful_prose_relay_age_secs`가
   §4.4에서 충족된다 — 이미 합의된 요구사항이다.

### 진행 전 반드시 닫아야 할 조건 (blocker)

| # | blocker | 어느 슬라이스 |
|---|---|---|
| **B1** | **golden fixture 코퍼스와 Rust↔Python 동치 CI 게이트.** 없으면 이중 오라클 드리프트로 교차검증 방어가 통째로 무효 (§A4) | **S1 머지 전제** |
| **B2** | **30일 관측 + `SuppressedByDedup < 0.1%`.** 미충족 시 S3(판정 권한 부여) 진행 금지 | **S3 진입 게이트** |
| **B3** | **틱 소요 p99 계측.** 30s 예산 초과 시 채널 샤딩 또는 틱 주기 재설계 필요 (§A8) | **S1 인수 조건** |

### NO-GO 항목

| 항목 | 판정 | 사유 |
|---|---|---|
| **S7 (미커버 range 재배달)** | **NO-GO (범위 밖)** | 핫파일 접촉 불가피. `tmux_watcher.rs` 경유 시 #4229 W7b 선행 필수(여유 2줄), `session_relay_sink.rs` 경유 시 #3016 직렬 단독. `relay_auto_heal.rs` redrive 재사용 가능성은 **미검증 [추정]**. **검출이 수용 기준이므로 S7 없이 목표 달성** |
| **의무 원장의 PostgreSQL 승격** | **NO-GO (범위 밖)** | 계약 §Task-32의 canonical durable admission store는 별도 과제. 승격 시 `migrations/postgres/immutable-checksums.json` 등록이 동반 요구사항 (§8.5) |
| **`#4081 fingerprint`의 배달 증거 승격** | **NO-GO** | 오판 이력 [확인 `turn_identity.rs:333`]. dedup 권한으로만 유지 (§2.3-3) |
| **워치독의 in-band 흡수** | **NO-GO** | #4381의 핵심 성질(릴레이가 죽어도 알림이 도착)이 소멸. 단방향 sidecar 인테이크로 권위만 부여 (§5) |
| **S1 시점의 임계값 하드코딩** | **NO-GO** | 근거 없는 값. 관측 후 확정 (§3.4, §A6) |
