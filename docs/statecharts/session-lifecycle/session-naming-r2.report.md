# 이름 이전 등록 트랜잭션 모델 — r2

이 모델은 **아직 구현하지 않은 A 승격 계약의 별도 유한 projection**이다. 기존 수명 모델 전체와 naming을 합성 탐색한 결과가 아니다. TmuxSession은 `msg`, HerdrSession은 `handoff`에 Naming record를 넣고 나머지 기존 lifecycle 변수는 고정한다. 기존 `Init`/`Next` 본문과 기존 cfg는 바이트 동일하게 보존했다. 따라서 기존 model의 알려진 수명 위반을 고쳤거나 재검증했다고 주장하지 않는다.

## 범위와 계약

- 한 채널의 옛 실행 하나와 새 successor 하나, old/empty 실행 nonce 2개 initial state, probe failure 최대 1회, 등록 전 nonce 교체 1회, 늦은 old hook 1회와 중지 재요청 1회를 탐색한다.
- 새 이름 요청 자체는 old live 이름/primary/물리 대상을 바꾸지 않는다. old host의 실제 종료와 부재 관측 뒤에만 successor를 준비한다. 등록은 old-host absence와 row nonce empty/new를 확인해 **primary 새 key + old alias**를 한 행동으로 커밋한다.
- 등록 직전 nonce가 old로 바뀌면 등록을 거부하고 자기 fresh successor만 정리한다. 자기 생성·서버 witness가 유효하다는 조건과 DB identity/두 locator lock의 원자성은 이 projection의 가정이다. 소유 증거 교체·기본 tmux 서버·실제 SQL 격리는 구현 시험 책임이다.
- tmux는 `PrimaryTailIsCurrentPhysical`을 검사한다. Herdr primary는 logical artifact 이름이므로 pane ID와 literal 동등성은 검사하지 않고 `PrimaryNamesCurrentLogical`과 `HostTargetMatchesCurrent`(old/new logical ↔ old/new pane)를 검사한다. pane의 실제 생성/복구 프로토콜은 기존 Herdr 모델에 그대로 남아 있다.
- `NoDuplicateKill`은 같은 owned successor에 대한 **종료 효과가 1회를 넘지 않음**이다. 호출 횟수나 모든 외부 프로세스에 대한 kill 권한을 증명하지 않는다. retry stop은 효과 0이다.

모델 작업은 2026-11-12 16:34:58 KST에 시작했고 16:50 KST 무렵 모든 실행을 종료했다. 30분 제한 안이며 진행 중 프로세스는 없다.

## 실행 결과

공식 TLC 1.7.4 배포 jar SHA256 `936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88`, 실행 `/opt/homebrew/opt/openjdk@25/bin/java`(Homebrew OpenJDK 25.0.5.0.3). JVM `-XX:+UseParallelGC`, TLC `-workers 1 -fp 0 -lncheck final`. scratch `/tmp/i8119-r2-models` 복사본에서만 실행했다. cargo/PG/Rust/운영 tmux/Herdr 실행은 없다.

정식 acceptance 구성은 **fixed 2개 + mutant 10개 = cfg 12개**다. fixed 각 8개 불변식은 primary 동등성(호스트별 위 차이), HostTargetMatchesCurrent, LiveOldNotPromoted, OldAliasRetained, PrimaryNotAlias, NoDuplicateKill, LateOldHookRejected, PromotionNonceWasCurrent이다. fixed는 각 52 distinct/78 generated/depth 12, queue 0으로 완주했다. model mutant의 exit 12는 로그에서 invariant violation을 대조한 RED이며 도구 오류가 아니다.

| Host | cfg mode | exit | generated | distinct | depth | 결과 |
|---|---|---:|---:|---:|---:|---|
| Tmux | Fixed | 0 | 78 | 52 | 12 | 모든 8개 불변식 유지 |
| Tmux | WithoutPromotion | 12 | 32 | 23 | 6 | PrimaryTailIsCurrentPhysical |
| Tmux | WithoutAbsenceGuard | 12 | 11 | 11 | 3 | LiveOldNotPromoted |
| Tmux | WithoutNonceCAS | 12 | 38 | 27 | 7 | PrimaryTailIsCurrentPhysical |
| Tmux | WithoutPromotionNonceGuard | 12 | 37 | 26 | 7 | PromotionNonceWasCurrent |
| Tmux | DuplicateKill | 12 | 46 | 33 | 8 | NoDuplicateKill |
| Herdr | Fixed | 0 | 78 | 52 | 12 | 모든 8개 불변식 유지 |
| Herdr | WithoutPromotion | 12 | 32 | 23 | 6 | PrimaryNamesCurrentLogical |
| Herdr | WithoutAbsenceGuard | 12 | 11 | 11 | 3 | LiveOldNotPromoted |
| Herdr | WithoutNonceCAS | 12 | 38 | 27 | 7 | PrimaryNamesCurrentLogical |
| Herdr | WithoutPromotionNonceGuard | 12 | 37 | 26 | 7 | PromotionNonceWasCurrent |
| Herdr | DuplicateKill | 12 | 46 | 33 | 8 | NoDuplicateKill |

Skill pipeline으로 차트도 추출했다. fixed 각각 52 states/76 steps, 도식은 8 states/7 arrows이며 같은 도식 상태 안의 48 steps를 숨긴다. 별도 view는 probe·nonce 재확인·late hook/retry의 같은 상태 전이를 접는다. 승격 제거 도식은 반례를 표시한다. wrapper의 janitor shell에 금지된 `rm -rf`가 있어 wrapper를 실행하지 않고 같은 skill의 `worker.ts` pipeline을 직접 실행했다. skill 파일은 변경하지 않았다.

- [tmux 도식](tmux-naming.chart.mmd)
- [Herdr 도식](herdr-naming.chart.mmd)
- [승격 제거 반례 도식](tmux-naming-without-promotion.chart.mmd)

## 반례와 남은 실제 재생

승격 제거 mutant의 최단 반례(도식의 0~5): 옛 실행 → 새 이름 요청 → 옛 host 종료 → 부재 확인 → successor 준비 → **새 실행 등록인데 primary는 old**. PrimaryTailIsCurrentPhysical·PrimaryNotAlias 위반이다. 그 뒤 old hook까지 0~6에서 중지가 잘못된 primary를 보고 진행하지 못하는 NoDeadlock 위반도 나타난다.

나머지 모델 반례는 raw `*-withoutabsenceguard.out`의 LiveOldNotPromoted, `*-withoutnoncecas.out`의 primary 동등성, `*-withoutpromotionnonceguard.out`의 PromotionNonceWasCurrent, `*-duplicatekill.out`의 NoDuplicateKill을 확인한다. 이들은 **제안 계약을 바꾼 모델 반례**이며 현 Rust 버그 재현이 아니다. 사용자 cargo 금지에 따라 실제 PG/Rust 재생은 미실시이며 S2의 PG 승격/늦은 nonce CAS/queue 취소·중지·thread GC 회귀와 restart E2E 수용 시험으로 넘긴다. 여러 채널의 동시 할당·host witness 교체·동시 stop·inflight 복구/alias resolver의 SQL interleaving은 여기서 제외했다.

## 재현

저장소 모델·관련 cfg·SessionNaming 모듈·view를 새 scratch로 복사한 뒤 직접 TLC를 실행한다. 전체 cfg 실행 명령은 외부 로그의 `run_all.py`에 있고 cfg마다 stdout에 명령 원문이 남아 있다. 차트는 다음과 같이 같은 skill worker로 새 stage에 만든다(stage는 먼저 빈 디렉터리로 생성):

```sh
TLA2TOOLS=/tmp/i8119-r2-models/tla2tools.jar JAVA=/opt/homebrew/opt/openjdk@25/bin/java node --disable-warning=ExperimentalWarning /Users/itismyfield/.codex/skills/statechart/scripts/worker.ts /tmp/i8119-new-chart-stage /tmp/i8119-r2-models/TmuxSession.tla /tmp/i8119-r2-models/TmuxSessionNamingFixed.cfg /tmp/i8119-r2-models/session-naming.view.mjs
```

원시 증거: `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8119/logs/r2-statechart/`의 `results.json`, `model-sha256.json`, cfg별 `.out`, `tmux-chart/`, `herdr-chart/`, `mutant-chart/`. 초기 36-state 시도와 view 라벨 수정 전 산출물은 scratch에 남았지만 최종 acceptance 증거로 쓰지 않았다. chart extraction은 최종 모델의 같은 cfg를 도식화하기 위한 추가 실행이며 전체 Rust 시험 수에 합산하지 않는다.

## 리뷰 집중 항목

- 경계 시각: request와 host 종료/부재 확인을 다른 행동으로 분리해 old live primary 보존을 검사했다. witness freshness의 실제 관측은 구현 E2E로 남는다.
- 경합: successor 준비 뒤 nonce 교체를 탐색하고 nonce-guard 제거 RED를 얻었다. DB lock 원자성 자체는 PG 시험 책임이다.
- 옛 형식 복원: old alias 보존·old hook 거절을 검사했다. 실제 저장 key 역직렬화·26 소비자는 S2/S3 수용 시험 책임이다.
- 문서만의 guard: fixed 불변식과 guard 제거 모델 5종을 연결했다. 실제 runtime guard가 연결됐다고 주장하지 않는다.
- 잘못된 0/PASS: cfg 12개 고유 실행 결과를 표로 고정했고 cargo/PG 0을 PASS로 세지 않는다. 기존 full lifecycle의 알려진 위반·기존 cfg는 손대지 않았다.

## S2 이식 검증 (2026-11-13 KST)

설계 ref `01053c2be0312bad4d9634f2df46b3141301f279`의 naming projection을 기준 `79bebdd4eaa29b289bf88412825baa7d70a6ac77`에 이식했다. TmuxSession/HerdrSession 전체를 교체하지 않고 naming 정의만 덧붙였다. Tmux의 새 effort 변수는 `EffortDormant`로 초기화하고 naming 행동에서 `UNCHANGED effort`로 고정했다. 기존 cfg 파일과 expected 기대값은 모두 보존했다.

| 비교 대상 | 이식 전 | 이식 후 | 변경 |
|---|---:|---:|---:|
| 기존 expected 행 | 404 | 404 | 0 |
| naming cfg / expected 행 | 0 | 12 | +12 |

공식 `scripts/ci/statecharts_tlc.py`로 naming cfg 12개를 실행했다. Fixed 2개는 깨진 규칙 0, mutant 10개는 등록한 기대 규칙과 실제 깨진 규칙이 모두 일치했다. 이 checker는 첫 반례에서 끝내지 않고 깨진 규칙을 하나씩 제외해 다시 검사하므로, 위 r2의 첫 위반 표보다 많은 위반을 아래 표에 기록한다.

| cfg (호스트별) | Tmux 실제 위반 | Herdr 실제 위반 |
|---|---|---|
| Fixed | 없음 | 없음 |
| DuplicateKill | NoDuplicateKill | NoDuplicateKill |
| WithoutAbsenceGuard | LiveOldNotPromoted | LiveOldNotPromoted |
| WithoutNonceCAS | PrimaryTailIsCurrentPhysical, PrimaryNotAlias, LateOldHookRejected, NoDeadlock | PrimaryNamesCurrentLogical, PrimaryNotAlias, LateOldHookRejected, NoDeadlock |
| WithoutPromotion | PrimaryTailIsCurrentPhysical, PrimaryNotAlias, NoDeadlock | PrimaryNamesCurrentLogical, PrimaryNotAlias, NoDeadlock |
| WithoutPromotionNonceGuard | PromotionNonceWasCurrent | PromotionNonceWasCurrent |

명령: `JAVA=/opt/homebrew/opt/openjdk/bin/java TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar python3 scripts/ci/statecharts_tlc.py --root <naming 모델/12 cfg/12 expected 행만 복사한 scratch> --timeout 60`. 원시 로그는 `/Users/itismyfield/.adk/lane-runs/i8119/logs/s2-tlc-import.log`다.

이번 커밋은 승인된 설계 산출물 이식이다. 실제 SQL의 witness/CAS/rollback interleaving과 liveness PROPERTY 추가는 S2 본체 단계이며, 위 safety 검사로 증명했다고 주장하지 않는다.

## S2 승격 transaction projection

기준 구현 조각 `ae48c900771e10c67c7d76646761683777daf695` 위에서 등록을 discovery → 정렬 locator → identity → row/alias/CAS → tx 내부 primary UPDATE → old alias → commit으로 분리했다. 외부에서 볼 수 있는 primary/alias/current/nonce는 commit에서 한 번에 게시한다. old host 증거가 Live/ProbeFailed/Unknown/다른 server/hosted pending으로 바뀌거나 snapshot nonce가 바뀌면 staged 변경을 버리고 rollback한다. 실제 SQL lock 대기는 PG 경합 시험이 담당한다.

진입점과 모델 행동:

| 코드 진입점 | 모델 행동/값 |
|---|---|
| `src/services/dispatched_sessions/hook.rs:101,107` → `src/db/dispatched_sessions/canonical_identity.rs:178,191,220` | LateOldNonceHook; nonce CAS 없는 old alias의 UPDATE 거절 |
| `src/db/dispatched_sessions/canonical_identity.rs:144` 및 `src/services/discord/tui_prompt_relay/synthetic_start/bridge_handoff.rs:650` | snapshotNonce/nonceContested; 명시 witness가 없으면 primary 승격 없음 |
| `src/db/dispatched_sessions/key_promotion.rs:26,79,100` (discovery/locator/validation) | TxDiscover → TxLocatorLocks → TxIdentityLock → TxRows; 비권한 snapshot과 under-lock CAS |
| `src/db/dispatched_sessions/key_promotion.rs:183` (promote_primary) | PromotePrimary → TxPreserveAlias → TxCommit; 충돌은 TxRollback |
| `src/services/discord/session_locator.rs:130,217` (channel_key/observe_old) | RequestNewName / ObserveOldHost; live old의 관측 namespace만 재사용, 미관측 namespace는 Unknown |
| `src/services/discord/router/message_handler/provider_dispatch.rs:353` (registration_origin) | 명시 canonical 등록만; synthetic/process/routine/scheduled의 override는 범위 밖 |
| `src/services/tui_prompt_dedupe/binding_context.rs:467` → scoped completion → `src/services/discord/session_locator.rs:360` | 실제 새 incarnation과 spawn marker 게시 뒤 fresh registration witness 생산 |

file:line 전수 결과는 `/Users/itismyfield/.adk/lane-runs/i8119/logs/s2-registration-entrypoints.log`다. 이름의 suffix로 입력 kind/provider/channel을 새로 추론하는 진입점을 모델에 추가하지 않았다.

| cfg | 변경 전(r2 safety) | 승격 제거 liveness 대조 | 변경 후 transaction/liveness |
|---|---|---|---|
| TmuxSessionNamingFixed | safety GREEN, PROPERTY 없음 | CanonicalPrimaryEventuallyCurrent RED | safety/rollback invariant 및 PROPERTY GREEN |
| HerdrSessionNamingFixed | safety GREEN, PROPERTY 없음 | CanonicalPrimaryEventuallyCurrent RED | safety/rollback invariant 및 PROPERTY GREEN |
| naming mutant 10개 | 기대 위반 집합 | 해당 없음 | 동일한 기대 위반 집합 유지 |

원시 로그: `s2-tlc-before-tx.log`(PROPERTY만 둔 승격 제거 대조), `s2-tlc-tx.log`(12 cfg). Fixed 두 cfg는 `SPECIFICATION NamingSpec`에서 WF로 등록 시도를 진행하며 `PROPERTY CanonicalPrimaryEventuallyCurrent`를 검사한다. rollback은 새 primary/old alias의 부분 게시가 없음을 별도 invariant로 검사한다. 기존 main 404 cfg/expected 행의 기대값은 바꾸지 않았다.

범위 밖과 관계:

- 여러 채널/DB row/서로 다른 SQL backend의 실제 blocking은 모델의 lock phase로 추상화했다. PG 경합 시험이 정렬 locator/identity 순서와 old-alias hook 겹침을 검증한다.
- fresh tmux의 atomic create와 incarnation marker 게시 프로토콜은 S1/기존 binding-context 계약이다. 모델은 그 creator의 fresh witness만 입력으로 받는다.
- ordinary process/routine/scheduled 및 historical ownership token은 canonical registration의 증거가 아니며 primary 승격 경로에 합치지 않는다.
- 명시 old-alias force_kill/host_repair의 파괴적 소비자 효과는 S3로 이관됐다. current-primary의 queue/force-kill/policy/GC 효과는 S2 실제 PG/격리 tmux 시험이 맡는다.
- 이 projection은 한 fresh successor와 한 등록 시도다. rollback 뒤 호출자의 새 탐색/재시도, 다중 채널 및 외부 host 변경 전체를 무한 탐색했다고 주장하지 않는다.

등록 완료 연결은 `binding_context/registration.rs`의 thread scope callback을 통해 상위 locator에 전달한다. 낮은 층이 Discord 구현을 직접 참조하지 않는다. 같은 row의 incoming new-key alias 소비와 foreign alias 거부는 PG namespace 시험이 담당하며, 이 단일-successor projection은 사전 alias 집합 전체를 탐색하지 않는다.
