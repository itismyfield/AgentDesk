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
