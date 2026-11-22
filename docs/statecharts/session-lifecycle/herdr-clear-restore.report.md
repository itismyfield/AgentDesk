# Herdr clear 종료와 복원된 빈 워크스페이스

코드 좌표 기준 `cd9951d3cac6a4f0b81d02789facabbc6ed2a55d`. [이전 설계](https://github.com/itismyfield/AgentDesk/commit/86b268dd523d54e0ebad47b0f2f03301c991a542)의 종료 기록 선행 조건과 복원 셸 자격을 개정했다. 기존 `HerdrRestartOwner`, `HerdrClearLaunch`, `HerdrRootRecovery`의 실행 식별·selector·교체 서버 판정을 재사용하는 설계다.

[Clear 상태도](herdr-clear-restore.clear.chart.mmd) · [복원 상태도](herdr-clear-restore.restore.chart.mmd) · [행 2·3 전수표](../../herdr-parity-next-lifecycle.design.md)

## 값과 행동의 근거

| 모델 값·행동 | 진입점과 상태 생산·소비 |
| --- | --- |
| `RequestClear`, 기존 Bound/Pending/Retired/Unknown | `src/services/discord/commands/control/clear_teardown.rs:73–77,161–221`의 채널 clear, `src/services/discord/commands/control/clear_teardown/execution_cleanup.rs:92–136`의 owner clear, `src/services/discord/commands/control.rs:582–594`의 restored/retired 선행 경로. Unknown/Conflict는 새 종료 권한을 얻지 않는다. |
| `LoseSource`, `RecordAfterEnd` | tmux는 `src/services/discord/commands/control/clear_teardown.rs:104–122`에서 선택적 증거를 모은 뒤 그 결과와 무관하게 종료를 요청한다. 기존 provider 캡처와 종료 확인의 경계는 같은 디렉터리의 `clear_end_record.rs:37–59,73–99,166–191` 및 `codex_end_record.rs:8–35`다. Herdr도 증거를 기존 nonce에서 선택적으로 붙인다. |
| `CloseExactOrObserveEnded → RetireExact → ReportClear` | `src/services/discord/herdr_terminate.rs:517–624`의 fence·exact row·종료 확인·Retired CAS, `src/services/discord/commands/control/clear_teardown.rs:197–221`의 postlude. `InterruptPostlude`는 이전 실행을 종료한 뒤 프로세스가 중단된 구간을 나타낸다. 새 저널을 두지 않으므로 해당 구간의 기록 재생은 보장하지 않는다. |
| `RestartServer`, 현재 증거와 낡은 관측 | `src/services/discord/recovery_engine/herdr_server_replaced.rs:143–210`, `src/services/discord/recovery_engine/herdr_reader.rs:235–351`, `src/services/session_host/herdr_gate/root_replaced.rs:20–90`이 서버 교체와 이전 실행 종료를 관측한다. 두 번째 서버 세대는 첫 관측이 낡을 수 있다는 유한 추상화다. 같은 불변 복원 계보의 활동 상태는 두 번째 재시작에도 유지된다. |
| `UseRestoredShell → ReturnToShell`, `LoseNoWorkProof` | 현재 `LoneShell`의 foreground 관측은 `src/services/session_host/herdr/pane_probe.rs:206,221–244`다. 현재 셸 화면이 과거 활동 없음과 백그라운드 작업 없음을 보장하지 않는다. 서버 측 활동 증거는 미사용·사용·불명, 작업 증거는 없음·있음·불명으로 분리했다. |
| `LoseLineageEvidence`, `SurveyEmpty → CompareAndClose` | `src/services/herdr_launch.rs:479–492`의 successor 저장이 이전 row를 덮을 수 있다. 독립 계보 증거가 남지 않으면 당시 후보의 자격을 종료한다. 현 관측/닫기 API는 `src/services/session_host/herdr/model.rs:204–258,269–294`의 별도 동작이다. 결정 A의 세 조건을 서버가 닫기와 함께 원자적으로 확인하는 능력은 W2 선행 의존이다. |

## 검증 규칙과 설정

| cfg 접미사 | 결과 | 규칙과 경로 |
| --- | --- | --- |
| ClearFixed | GREEN | `NoRecordRefusesAuthorizedClear`, `NoUnprovenEndRecord`, `NoMissingClearEndRecord`, `AuthorizedClearEnds`. `RequestClear → LoseSource` 뒤 exact 종료·은퇴가 가능하고 응답까지 이어지면 기록 경고가 남는다. 저장 실패도 같은 방식으로 처리한다. |
| ClearMain, ClearSkipRecord | 예상 RED | `NoMissingClearEndRecord`. 정상 종료·기록 가능한 실행에서 종료 기록 없이 응답한다. |
| ClearRecordGate | 예상 RED | `NoRecordRefusesAuthorizedClear`, `AuthorizedClearEnds`. 리뷰 경로 `RequestClear → LoseSource → RefuseUnknownRecord`에서 승인된 실행의 종료가 거절된다. 개정 설계는 원인을 제거했다. |
| RestoreFixed | GREEN | `NoUnprovenWorkspaceRemoved`, `EmptyWorkspaceEventuallyCleared`. 복원 창을 사용한 뒤 셸로 돌아와도 정리 자격을 회복하지 않는다. 세 증거가 계속 만족되고 원자 닫기 기능이 있을 때에만 적격 후보다. |
| RestoreMain, RestoreNoWake | 예상 RED | `EmptyWorkspaceEventuallyCleared`. 계속 적격인 복원 셸을 다시 관측하거나 닫는 단계가 없으면 잔류한다. |
| RestoreNoActivityGate | 예상 RED | `NoUnprovenWorkspaceRemoved`. 리뷰 경로 `RestartServer → UseRestoredShell → ReturnToShell → SurveyEmpty → CompareAndClose`에서 과거 사용을 무시하면 닫힌다. |
| RestoreNoWorkGate | 예상 RED | `NoUnprovenWorkspaceRemoved`. 남은 작업을 배제할 수 없는 상태에서도 닫힌다. |
| RestoreNoLineageGate | 예상 RED | `NoUnprovenWorkspaceRemoved`. 불변 복원 계보 증명이 없어도 닫힌다. |
| RestoreReadThenClose | 예상 RED | `NoUnprovenWorkspaceRemoved`. 오래된 관측 뒤 활동·서버·증거가 바뀌었는데 닫힌다. |

Clear 규칙의 범위는 정확한 기존 종료 권한과 종료 확인이 결국 가능하다는 가정이다. 정상 응답에서는 기록할 수 있는 Bound의 종료 기록을 남기며 원본이나 저장이 불명인 Bound는 기록 경고를 남긴다. 기존 Retired, native 시작 증거가 없는 Pending, Unknown/Conflict는 새로운 종료 기록을 만들 권한을 갖지 않는다. 실행이 끝난 뒤 프로세스가 끊긴 경우, 기존 종료와 새 입력 경계는 보존하며 기록 재생의 liveness는 요구하지 않는다. 저장의 멱등성, Codex 첫 시도 소비와 이전/후속 nonce 귀속은 E의 두 실행 fixture가 검증한다.

복원 규칙은 결정 A에 따른다. 세 증거로 계속 적격인 경우에만 기존 recovery 재평가와 원자 close에 약한 공정성을 가정한다. legacy의 소급 소유 판정, 장기간 endpoint 장애, 사용·작업 이력을 얻지 못한 창은 정리 보장 범위 밖이다. 같은 복원 계보에서 활동 또는 증거 상실을 재관측이 되돌릴 수 없다. 상태도의 같은 라벨에 남는 전이는 생략한다.

## 다시 실행

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
cd docs/statecharts/session-lifecycle
~/.codex/skills/statechart/scripts/statechart HerdrClearRestore.tla \
  --config HerdrClearRestoreClearFixed.cfg --view herdr-clear-restore.view.mjs \
  --out "$(mktemp -d)"
```

cfg 이름의 접미사를 결과표 값으로 바꾼다. 저장소 `scripts/ci/statecharts_tlc.py`는 설정별 기대 위반과 liveness를 대조한다.
