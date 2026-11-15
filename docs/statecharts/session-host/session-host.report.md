# SessionHost: #7583 설계 레그 1

코드 SHA: **1974cff8a67bf5d1db041abfdd4d615c518ee104**. Rust 구현을 바꾸지 않은 설계 모델이다.
모델 작성·실행은 30분 안에 끝냈다. 도메인/코드 연결은
[진입점 표](../../design/i7583/entrypoints.md), 전체 설계는 [설계 미러](../../design/i7583-session-host.design.md)다.

## 기존 모델에서 가져온 것과 고친 가정

먼저 README, `session-lifecycle/TmuxSession.tla`, `HerdrSession.tla`,
`host-migration/HostMigration.tla`와 기존 report/cfg를 읽었다. 기존 모델의 역사적 SHA나
기대 RED를 바꾸지 않았다. 이번 공통 수명주기는 해당 두 모델의 누락 영역이므로 별도 폴더에 뒀다.

| 기존 가정 | 현재 main 좌표와 이번 모델 |
|---|---|
| tmux는 재시작 뒤 이름으로 찾고 Herdr는 pane/nonce로 찾음 | `src/services/session_host/model.rs:12`, `src/services/discord/recovery_engine/herdr_reader.rs:235`; host가 살아 있으면 같은 record를 Adopt |
| Herdr launch의 Pending과 위치 기록 사이에 crash 가능 | `src/services/herdr_launch.rs:482`, `src/services/herdr_launch.rs:515`, `src/services/herdr_launch.rs:554`; Reserve/CreateEffect/RecordLocation을 분리 |
| Herdr unplaced Pending은 영원히 점유할 수 있음(역사 모델) | 현재 `src/services/herdr_launch/unplaced.rs`, `src/services/discord/recovery_engine/herdr_reader.rs:377`; 정확한 census로 Adopt 또는 ResolveEmpty. Windows 포트가 이 계약을 증명해야 함 |
| 서버 재시작 시 목록은 자동으로 완성됨(옛 migration 모델) | `src/services/discord/tui_prompt_relay/herdr_source.rs:274`; 실제 source publish·admission 단계가 필요하므로 BootDone만으로 adopted가 되지 않음 |
| close를 보낸 뒤 retire | 공통 trait의 종료는 `src/services/termination_audit/host_terminate.rs:52`에서 Unsupported. 운영 Herdr 종료의 실제 fence/close/absence 순서를 포트로 옮기는 제안이며 현재 trait 구현이라고 주장하지 않음 |
| Windows도 Unix 소켓/foreground group을 쓸 수 있음 | 불가: `src/services/session_host.rs:19`, `src/services/session_host/herdr/provenance.rs:20`, `src/services/session_host/herdr/pane_probe.rs:145`; Platform=windows의 current Herdr adapter는 재채택하지 못함 |

## 규칙과 공정성

- `NoSessionLoss`: 살아 있는 실행의 소유 기록과 찾을 증거가 dcserver crash 뒤에도 남는다.
- `NoDoubleSession`: 한 채널의 살아 있는 실행은 최대 하나다.
- `NoOrphanAfterTerminate`: 종료 완료/Retired 뒤 살아 있는 실행은 없다.
- **PROPERTY `RestartEventuallyReconnect`**: 복구를 빚진 실행은 결국 같은 record/locator로 adopted가 된다.
- **PROPERTY `TerminateEventuallyGone`**: 명시적 종료는 결국 확인된 소멸과 종료 완료에 도달한다.
- `PendingEventuallyResolved`: launch 효과 전/후 crash의 Pending도 결국 Bound 또는 검증된 Retired가 된다.

`Spec`는 Boot/ProbeRecovers/Adopt와 create/close/resolve에 실제 `WF_vars`를 적용한다.
ProbeUnknown은 한 번 뒤 정상화한다. 영구 DB 장애, 권한 불일치, provider/Herdr 자체 사망에는
재연결 liveness를 주장하지 않는다. stop이 restart 이전에 시작됐으면 재채택 대신 종료 liveness를 검사한다.
하나의 실행을 OS 재부팅 뒤 같은 프로세스로 살려내는 규칙은 없다.

## 실행과 결과

`TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar`와 저장소
`python3 scripts/ci/statecharts_tlc.py --root <logs>/tlc-root --jobs 2`를 썼다.
root에는 이 폴더의 **동일 바이트** 모델/cfg와 expected.tsv의 해당 10행을 복사했다.
최종 checker는 **rc=0, 10 cfg, 29 TLC 호출, 4 GREEN/6 기대 RED, 예상 밖 위반 0**이다.
각 cfg에 대한 직접 TLC 로그도 별도로 보존했다. 최초 실행의 경로 선택 오류 두 건과
ReplayPending의 liveness 기대값 오판은 최종 결과로 덮어 보고하지 않고 원본 로그에 남겼다.

| cfg | 기대/실제 safety 위반 | 기대/실제 PROPERTY 위반 |
|---|---|---|
| CurrentProcess | NoSessionLoss | RestartEventuallyReconnect |
| CurrentWindowsHerdr | 없음 | RestartEventuallyReconnect |
| ProposedWindowsHerdr | 없음 | 없음 |
| ProposedTmux | 없음 | 없음 |
| ProposedPending | 없음 | 없음 |
| ProposedTerminate | 없음 | 없음 |
| ReplayPending | NoDoubleSession | 없음 |
| EarlyRetire | NoSessionLoss, NoOrphanAfterTerminate | 없음 |
| NoWake | 없음 | RestartEventuallyReconnect |
| ForgetIdentity | NoSessionLoss | RestartEventuallyReconnect |

## 반례를 코드와 단계별로 대조

1. CurrentProcess: 등록된 실행 → Crash가 `PROCESS_SESSIONS`를 잃음 → Boot → lookup이 Missing.
   `src/services/session_backend.rs:341`, `src/services/session_backend.rs:515`, `src/services/session_backend.rs:526`과 process launch의 재사용 분기
   `src/services/claude/process_session_launch.rs:49`가 이를 확인한다. 살아 있는 child가 남는 경우의
   레지스트리 유실을 모델링하며, 실제 child가 EOF로 끝나는 다른 경우까지 살아 있다고 단정하지 않는다.
2. CurrentWindowsHerdr: 이미 Bound인 row를 입력 fixture로 둠 → Crash → Boot → adapter가 없음 →
   영원히 미채택. `src/services/session_host.rs:19`, `src/services/discord/recovery_engine.rs:131`,
   `src/services/turn_host.rs:243`의 gate와 일치한다. **현재 Windows에서 이 Bound를 만들 수 있다는
   버그 재현이 아니라 포트에 필요한 능력의 반례**다.
3. ReplayPending: Reserve → CreateEffect → 위치 기록 전 Crash → BootDone → ReplayPending →
   CreateEffect로 live={1,2}. 현재 main은 `src/services/herdr_launch.rs:432`에서 Pending을
   Occupied로 보고 `src/services/herdr_launch/unplaced.rs`로 회수한다. 따라서 main 결함이 아닌 그 guard 제거의 반례다.
4. EarlyRetire: Bound → RequestStop → BeginClose에서 Retired/완료를 먼저 씀 → provider는 live.
   현재 Herdr 종료/Return은 close 또는 `/exit` 확인 이후에 retire한다
   (`src/services/claude/herdr_return.rs:80`, `src/services/claude/herdr_return.rs:87`); 새 trait도 이 순서를 유지해야 한다.
5. NoWake: Bound → Crash → Boot → BootDone에서 끝남. 실제 main의
   `src/services/discord/tui_prompt_relay/rehydration.rs:270`, `src/services/discord/tui_prompt_relay/rehydration.rs:668` → `src/services/discord/recovery_engine/herdr_reader.rs:235` →
   `src/services/discord/tui_prompt_relay/herdr_source.rs:274`를 Windows에서도 연결해야 한다. 함수만 cfg 해제하고 boot 호출을 남겨두면 발생한다.
6. ForgetIdentity: Crash 때 locator와 registry를 잃고 nonce census도 제공하지 않음 → 살아 있는
   실행을 찾을 수 없음. `src/db/dispatched_sessions/hosted_execution.rs:47`, `src/db/dispatched_sessions/hosted_execution.rs:68`, `src/services/discord/recovery_engine/herdr_reader.rs:35`의 durable location과
   expected/nonce를 빼는 변경을 금지하는 제어다. 현재 코드가 이를 잊는다는 주장이 아니다.

## 증명의 한계

각 호스트 API의 한 단계는 실제 구현의 OS witness 검증과 효과를 포함하는 포트 계약이다.
Windows pipe 서버 PID/FILETIME, detach/Job 수명, provider nonce와 native transcript/훅,
close의 root/provider 소멸은 아직 [U]다. GREEN은 이 계약을 구현하면 가능한 순서임을 보이며
네이티브 Windows에서 이미 성립한다는 실행 증거가 아니다.
기존 cross-host fresh migration의 반례는 현재 `src/services/claude/herdr_turn/handoff.rs:74`에도 남아 있으며 이 모델의 도메인 밖이다. 기존 모델 기대 RED를 그대로 보존했고 새 Windows host가 Unknown 이전 host 위에 추가 생성하지 않는 계약만 더한다. 두 실행/한 채널/한 daemon crash의 유계 모델이고, 무한 메시지·여러 노드·입력 정확히 한 번·
O 게시 권위는 기존 입출력 모델의 계약으로 남긴다. 미측정 수를 성공 0으로 취급하지 않는다.
차트는 모델 action을 손으로 투영한 Mermaid이며 TLC가 그린 전이 전수 그림이라고 주장하지 않는다.

원시 로그: `/home/adk/adk/lane-runs/i7583/logs/design-1/tlc-final.log`, `tlc-<cfg>.log`.
