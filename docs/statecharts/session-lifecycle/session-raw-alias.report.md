# raw 세션 키 입력 경계 모델 — #8119 S3

`SessionNaming.tla`의 등록 transaction projection 위에 **등록이 끝난 row로 들어오는 세션 키 입력**을 얹은 유한 모델이다. 기존 `Init`의 record에 필드만 더했고 기존 행동·`Next`·cfg 12개는 그대로다. 기존 수명 모델 전체와 합성 탐색한 결과가 아니다.

## 행동과 값 범위

값 범위는 진입점 전수표에서만 가져왔다.

| 모델 값 | 코드 진입점 |
|---|---|
| `src` person·http | `services/dispatched_sessions/force_kill.rs` `prepare`, `tmux_cleanup.rs` `kill_tmux_session_impl`, `services/session_resume.rs` `dispatch_resume_previous`, `server/routes/dispatched_sessions.rs` `reconcile_stale_turn` |
| `src` routine·policy | `engine/ops/timeouts_ops/host_repair.rs` `resolve_policy_target_pg` (`session.kill`·`observeSessionHost`·`repairStaleSession`) |
| `src` queue | `services/queue.rs` 취소: row primary만 읽으므로 `key = "primary"`만 가능 |
| `key` primary·alias | 현재 primary, 승격이 보존한 old alias |
| `RawGc` | `services/dispatched_sessions/hosted_cleanup/thread_gc.rs`: row primary의 이름을 probe |

- `AskStop → Normalize → Effect`: 받은 키는 row만 찾고, 대상 이름은 row의 현재 primary 꼬리, provider는 row 기록에서 온다.
- `RawRepromote`: 정규화와 효과 사이에 세션이 끝나고 successor가 승격된다(tmux만, 1회).
- `RawRestart`/`RawLost`: row·alias·세션은 daemon 재시작을 넘기고, 진행 중 요청은 잃는다. 재시작 뒤 같은 alias로 다시 요청할 수 있다.
- Herdr 실행은 row 자신의 primary로 들어온 요청만 terminate service에 넘긴다. alias 요청은 거절 기록으로 끝난다(main 범위 유지).

## 규칙

| 규칙 | 뜻 |
|---|---|
| `RawStopEndsCurrent` | 효과를 기록한 요청 뒤에 그 row의 세션이 살아 있지 않다 |
| `RawMissNeedsRace` | 효과를 놓친 요청은 실제 승격이 사이에 낀 경우뿐이다 |
| `RawAliasReachesRow` | 거절은 Herdr의 alias 요청뿐이다. tmux의 alias 요청은 row에 닿는다 |
| `RawProviderFromRecord` | 효과에 쓴 provider는 row 기록과 같다 |
| `GcKeepsLive` | stale thread 정리는 live 세션의 row를 지우지 않는다 |
| `RawSettles` (liveness) | 열린 요청은 결국 효과·부재·거절·놓침·유실 중 하나로 기록된다 |

## 실행 결과

TLC 1.7.4, `-workers 1 -fp 0 -lncheck final`, scratch 복사본에서 실행. 깨지는 규칙은 하나씩 지우며 다시 돌려 전체 집합을 구했다(`expected.tsv`와 같은 방식).

| cfg | 결과 | generated | distinct | depth |
|---|---|---:|---:|---:|
| TmuxSessionRawAliasFixed | 13개 불변식·`RawSettles` 유지 | 48363 | 16450 | 26 |
| HerdrSessionRawAliasFixed | 13개 불변식·`RawSettles` 유지 | 29931 | 11106 | 25 |
| TmuxSessionRawAliasParsed (alias 꼬리를 직접 사용) | `RawMissNeedsRace` | 68331 | 24354 | 26 |
| TmuxSessionRawAliasProviderDefault (provider 기본값 추정) | `RawProviderFromRecord` | 48363 | 16450 | 26 |
| HerdrSessionRawAliasProviderDefault | `RawProviderFromRecord` | 29931 | 11106 | 25 |
| TmuxSessionRawAliasUnnormalized (정규화 제거) | `RawAliasReachesRow`, `NoDeadlock` | 45995 | 16194 | 26 |
| TmuxSessionRawAliasGcByAlias (alias 이름을 probe) | `GcKeepsLive` | 48363 | 16450 | 26 |

mutant 행의 수치는 깨진 규칙을 모두 지운 마지막 실행의 것이다.

### 기존 cfg 수정 전/후

`SessionNaming.tla`·`TmuxSession.tla`·`HerdrSession.tla`를 바꿨으므로 기존 naming cfg 12개를 `origin/main` 모델과 수정 뒤 모델로 각각 돌렸다. exit·generated·distinct·depth·첫 위반 규칙이 모두 같다.

| cfg | 수정 전 | 수정 후 |
|---|---|---|
| Tmux·HerdrSessionNamingFixed | exit 0, 3519/1010, depth 19 | 같음 |
| …NamingDuplicateKill | exit 12, 2178/774, depth 14, NoDuplicateKill | 같음 |
| …NamingWithoutAbsenceGuard | exit 12, 11/11, depth 3, LiveOldNotPromoted | 같음 |
| …NamingWithoutNonceCAS | exit 12, 1826/687, depth 13, primary 동등성 | 같음 |
| …NamingWithoutPromotion | exit 12, 1365/571, depth 12, primary 동등성 | 같음 |
| …NamingWithoutPromotionNonceGuard | exit 12, 1534/707, depth 13, PromotionNonceWasCurrent | 같음 |

## 한계

- `RawRepromote` 뒤의 요청은 효과를 놓치고 그 사실을 기록한다. 모델은 재요청 1회로 끝나는 것만 보인다. 정규화와 효과를 한 잠금으로 묶는 계약은 없다.
- row가 삭제된 뒤 같은 키가 다른 row에 다시 등록되는 경우, 여러 row의 동시 요청, 실제 SQL interleaving은 넣지 않았다.
- Herdr의 승격은 구현이 거절하므로 Herdr에는 `RawRepromote`가 없다.
- 도식(`.chart.mmd`)은 만들지 않았다.
