# Herdr clear 종료 기록과 서버 재시작 뒤 빈 워크스페이스

코드 기준 `cd9951d3cac6a4f0b81d02789facabbc6ed2a55d`. 기존 `HerdrRestartOwner`의 clear 종료·postlude, `HerdrClearLaunch`의 selector, `HerdrRootRecovery`의 교체된 서버에서 이전 실행 종료 판정을 이어서 모델링했다. 기존 세 모델의 계약과 설정은 유지하고, 동일한 세션 수명 영역에 종료 기록과 workspace 효과를 비교하는 작은 모델을 추가했다. 두 설정군이 공통 모듈의 서로 다른 전이 집합을 실행한다.

[Clear 상태도](herdr-clear-restore.clear.chart.mmd) · [복원 상태도](herdr-clear-restore.restore.chart.mmd) · [행 2·3 전수표](../../herdr-parity-next-lifecycle.design.md)

## 값과 행동의 근거

현행 흐름과 제안한 효과를 분리했다. 행 2·3 전수표에 전체 호출자와 인접 Cancel·Terminate·clear·stop·재시작·부팅의 좌표를 적었다.

| 모델 값·행동 | 진입점과 상태 생산·소비 |
| --- | --- |
| `RequestClear`, Bound/Retired | `src/services/discord/commands/control/clear_teardown.rs:73–77,161–191,197–221`의 channel clear, `src/services/discord/commands/control/clear_teardown/execution_cleanup.rs:92–136`의 owner/target clear, `src/services/discord/commands/control.rs:582–594`의 restored/retired 선행 경로. `refused`는 정확한 이전 실행을 읽을 수 없을 때의 결과다. |
| `StageEndRecord`, 이전 source 확인·불명 | tmux 기준 `src/services/discord/commands/control/clear_teardown/{clear_end_record.rs:37–59,73–99,166–191,codex_end_record.rs:8–35}`의 종료 전 identity/source capture와 종료 뒤 commit. 제안은 Herdr nonce와 원본 source를 close 전 내구성 있게 기록한다. 기존 미확인 Retired는 확증 없이 소급 기록하지 않는다. |
| `CloseExactOrObserveEnded → RetireExact` | `src/services/discord/herdr_terminate.rs:517–624`의 fence, row/dispatch 재검증, 확인된 close 또는 이미 종료, Retired CAS, 정산; `src/services/discord/commands/control/herdr_recovery.rs:47–122`의 교체된 root 경로. Stop/Cancel에는 clear 목적을 부여하지 않는다. |
| `RestartDuringClear → CommitEndRecord → ReportClear` | `src/services/discord/commands/control/clear_teardown.rs:197–221`와 `src/services/discord/commands/control/clear_teardown/execution_cleanup.rs:108–136`의 RetiredExclusion 후처리, `src/services/discord/herdr_terminate.rs:532–554,599–624`의 이미 Retired 재진입. 내구 staging 및 재개는 제안 전이이며 종결된 nonce에만 commit한다. |
| `RestartServer`와 Missing/셸/실행 중, 이전·현재 witness | `src/services/discord/recovery_engine/herdr_server_replaced.rs:143–210`과 `src/services/discord/recovery_engine/herdr_reader.rs:235–351`의 old/new server 판정과 관측, `src/services/session_host/herdr_gate/root_replaced.rs:20–90`의 이전 root/provider 소멸 및 현재 셸 증거. 최초 세대와 두 번의 세대 교체는 오래된 관측 재사용을 판별하는 추상화다. |
| `SurveyEmpty`, 소유 증명 있음·불명, 정리 승인·보류 | `src/services/discord/tui_prompt_relay/rehydration.rs:270,668`의 부팅 pass, runtime wake `src/services/discord/recovery_engine/herdr_row_end/owner.rs:67–70`, 동일 세대의 등록 중 유실 residue 검증 `src/services/session_host/herdr_gate.rs:1119–1221`. 복원 세대 출처를 증명할 지속 레코드와 사용자 승인 및 capability 조건은 제안의 전제다. |
| `WorkspaceChanges → CompareAndClose` | `src/services/session_host/herdr/model.rs:204–258,269–294`에서 snapshot과 pane ID close는 별개. 켜진 provider, pane shell stamp·서버 witness 변경은 이전 snapshot의 권한을 없앤다. 새 원자 compare-and-close는 서버가 동일 세대·귀속·pristine 증거를 효과 시점에 다시 검증할 수 있을 때만 허용한다. |

## TLC 결과

| cfg 접미사 | 기대와 관측 | 규칙과 대표 경로 |
| --- | --- | --- |
| ClearFixed | GREEN | `NoMissingClearEndRecord`, `PendingRecordDrains` 통과. 종료가 일어난 뒤에는 재시작이 끼어도 staged 기록이 남고, clear 완료 전에 정확한 종료 기록을 확정한다. |
| ClearMain | 예상 RED | `NoMissingClearEndRecord`. clear 요청 → 현재 Herdr 종료 경로 → 종료 전 증거 보관이 없고 종료 이후 capture/commit도 거치지 않는다. 기존 Retired 우회도 selector 후처리만 한다. |
| ClearSkipStage | 예상 RED | `NoMissingClearEndRecord`. 이전 source/nonce 보관을 걷어내고 close를 허용하면 같은 반례가 생긴다. |
| ClearVolatile | 예상 RED | `NoMissingClearEndRecord`, `PendingRecordDrains`. 보관 → 실행 종료 → 재시작 시 임시 보관이 사라진다. 후자는 종료·Retired 이후 재시작 → 확인 실패에서 멈춘다. |
| RestoreFixed | GREEN | `NoLiveWorkspaceRemoved`, `EmptyWorkspaceEventuallyCleared` 통과. 정리 자격이 계속 유지되는 복원 셸은 사라진다. 세대·프로세스·소유 증거가 바뀌면 해당 관측의 닫기는 허용되지 않는다. |
| RestoreMain | 예상 RED | `EmptyWorkspaceEventuallyCleared`. 재시작 → 빈 셸 복원 → 기록상 이전 실행 은퇴 가능 → 셸은 남을 수 있다. Main은 workspace 정리 효과가 없다. |
| RestoreNoWake | 예상 RED | `EmptyWorkspaceEventuallyCleared`. 적격 빈 셸도 관측 재개가 연결되지 않으면 계속 남는다. |
| RestoreReadThenClose | 예상 RED | `NoLiveWorkspaceRemoved`. 셸 확인 → 새 서버 또는 다른 프로세스 부착 → 이전 ticket으로 닫기. 현행 RPC 서명에서 본 설계의 조건부 효과와 차이를 확인했다. 현재 코드가 이 잘못된 닫기를 호출한다는 뜻은 아니다. |

Main 두 cfg의 경로는 위 소스와 11-13 이슈의 복원 관측으로 대조했다. 원인 제거 cfg는 제안을 구현한 다음 대응 진입점 시험으로 재현할 계획이다. 모델은 새 세션이 정리의 대상이 될 수 없음을 ticket으로 검사한다. 채널·thread/dispatch마다 실제 nonce와 source가 한 번만 소비되는지, DB/파일 durable checkpoint의 원자성, close 요청·응답 유실 및 지속적인 endpoint 장애, 양 provider 기록의 저장 형식은 진입점 시험이 맡는다. `PendingRecordDrains`는 Retired 증명과 durable 저장이 결국 진행할 수 있는 환경을 가정한다. `EmptyWorkspaceEventuallyCleared`는 승인, 조건부 RPC, 출처 증거 및 시점상 참인 빈 셸 조건을 가진 후보에 적용한다. 조건이 일시적으로 빠지면 이후 재검증하기 전까지 그대로 둔다. 이력 없는 옛 셸에 liveness나 삭제 권한을 전이하지 않는다. 상태도는 한 라벨에 머무르는 자기 전이를 생략한다.

## 다시 실행

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
cd docs/statecharts/session-lifecycle
~/.codex/skills/statechart/scripts/statechart HerdrClearRestore.tla \
  --config HerdrClearRestoreClearFixed.cfg --view herdr-clear-restore.view.mjs \
  --out "$(mktemp -d)"
```

위 cfg 이름을 결과표의 접미사로 바꾸면 각 상태도와 원시 반례를 다시 얻는다. 저장소 `scripts/ci/statecharts_tlc.py`는 명시된 기대 위반까지 포함해 통과 여부를 판단한다.
