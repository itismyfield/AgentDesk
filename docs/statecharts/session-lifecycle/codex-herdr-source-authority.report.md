# Codex Herdr 첫 입력과 Tokio 런타임 진행 (#8292)

기준 배포는 8470a8b911849b2570e00103fb9bee33c5d9c7eb이다. [수리 전 상태도](codex-herdr-source-authority-before.chart.mmd), [수리 후 상태도](codex-herdr-source-authority-after.chart.mmd)

## 모델과 코드 경로

- Codex Herdr 첫 입력 효과가 Pending row를 재조회하고, 같은 pane의 Herdr listing order와 source authority를 차례로 잡는다. 수리 전에는 `restore_pending`이 두 락 아래에서 PostgreSQL 응답을 기다렸다. 해당 조회가 락 획득 전에 끝나면 writer와 discovery는 응답을 기다리는 동안 pane 권한을 읽을 수 있다. [첫 입력 복원](../../../src/services/codex/herdr_turn/first_input.rs)
- O writer가 source authority를 이미 쓰는 pane에 닿으면 수리 전에는 현재 워커에서 동기 대기한다. 수리 후에는 이 poll을 넘기고 다음 poll에서 재확인한다. 이 모델에서 writer는 I/O와 시간을 마지막으로 처리하던 워커다. discovery는 같은 락에서 기다릴 수 있으며 별도 blocking thread에서 실행된다. [Writer poll](../../../src/services/tui_o/writer/actor.rs), [Scope 확인](../../../src/services/tui_o/writer/actor/fresh_resume.rs), [Intake discovery](../../../src/services/tui_o/cutover/intake_route/input.rs)
- Pending 조회 뒤 pane이 바뀌면 source authority 안에서 읽은 spawn nonce가 일치해야 등록할 수 있다. `submitted`는 기존 입력 효과 가드까지 통과한 첫 입력을 모델링한다. marker 교체는 효과가 확정되기 전까지만 모델에 포함했다. [Spawn nonce 확인](../../../src/services/codex/herdr_turn/first_input.rs)

## 규칙과 결과

| 구성 | TLC 결과 | 걸린 규칙 |
|---|---|---|
| Before | RED | `NoDatabaseUnderPaneLocks`, `NoSchedulerLockWait`, `RuntimeKeepsTicking`, `LaunchSettles` |
| After | GREEN | 없음 |
| BlockingWriter | RED | `NoSchedulerLockWait` |
| DatabaseUnderLocks | RED | `NoDatabaseUnderPaneLocks` |
| NoMarkerRecheck | RED | `FirstEffectMatchesLaunch` |

`AuthorityAcquiredAfterListing`은 모든 구성에서 성립했다. 수리 전·후 상태도는 각각 176개와 188개 상태를 탐색한 결과다. 등록된 다섯 구성 모두 저장소 TLC 검사에서 예상 결과와 일치했다. 전체 67.5초, 구성별 최대 19.0초였다.

- `NoDatabaseUnderPaneLocks`는 PostgreSQL 조회를 기다리는 동안 두 락이 모두 비어 있어야 한다는 규칙이다. Before와 DatabaseUnderLocks의 최단 경로는 3단계다. listing order 획득 → source authority 획득 → Pending 조회 시작.
- `NoSchedulerLockWait`는 writer가 워커에서 pane 락을 기다리지 않아야 한다는 규칙이다. Before는 두 락 획득 직후 writer가 기다리는 3단계 경로에서 실패한다. BlockingWriter는 락 밖 조회와 응답 뒤 같은 단계가 이어져 5단계에서 실패한다.
- `RuntimeKeepsTicking`과 `LaunchSettles`는 런타임 타이머가 계속 실행되고 첫 입력 효과가 허용 또는 거절로 끝나야 한다는 규칙이다. Before에서 listing order·source authority 획득 → Pending 조회 시작 → writer가 source authority에서 대기하면 둘 다 멈춘다. TLC가 찾은 `LaunchSettles` 경로는 5단계이며 앞에 무경합 discovery가 있다. `RuntimeKeepsTicking` 경로는 6단계이며 별도의 marker 교체 뒤 writer 대기가 조회 시작에 앞선다.
- `FirstEffectMatchesLaunch`는 첫 입력 효과에 Pending 조회와 해당 launch의 marker가 모두 근거로 남아야 한다는 규칙이다. NoMarkerRecheck는 조회 시작 → Pending 응답 → 다른 nonce 기록 → listing order 획득 → source authority 획득 → 이전 실행의 효과 허용의 6단계에서 실패한다. After는 같은 조건에서 입력을 거절한다.

## 범위와 다시 실행하기

한 pane의 cold launch, writer poll, discovery를 모델링했다. 마지막 I/O·시간 드라이버를 실행하던 워커가 writer에서 대기하면 다른 워커는 진행을 맡을 수 없다는 관측 조건을 고정했다. 드라이버가 진행하는 동안 계속 가능한 응답과 타이머 처리에는 약한 공정성을 가정했다. 모델 범위는 pane 락 경합과 첫 입력이다. 실제 PostgreSQL 가용성, 읽은 row의 독립 갱신에 대한 DB 원자성, 다른 pane, 전체 입력 전달 및 운영 장애 발생률은 별도 관측이 필요하다. 상태도는 정상 타이머 진행, 독립 discovery, 동일 표시 상태의 marker 변경을 합친다. 반례에 등장한 단계는 Before 상태도와 보고서에 남는다.

두 상태도와 세 대조군의 원시 TLC 출력은 레인 로그의 `statechart-8292-*`에 있다. 경합 경로의 소스 재현은 별도 Rust 검증에 속한다.

```sh
unset TMUX TMUX_PANE
export TLA2TOOLS="$HOME/.local/share/tla2tools/tla2tools-1.7.4.jar"
export JAVA=/opt/homebrew/opt/openjdk/bin/java
cd docs/statecharts/session-lifecycle
out_dir=$(mktemp -d /tmp/adk-8292-statechart.XXXXXX)
for config in CodexHerdrSourceAuthority*.cfg; do
  "$HOME/.codex/skills/statechart/scripts/statechart" CodexHerdrSourceAuthority.tla \
    --config "$config" --view codex-herdr-source-authority.view.mjs \
    --out "$out_dir/${config%.cfg}"
done
```
