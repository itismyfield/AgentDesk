# Antigravity 채널 writer 채택과 한 턴 종료 (#8274)

코드 기준 `881cc5c5d5933cdffc7db87eeb35075d846064a9`. 스레드 계약 기준 `78353d7d49e94b088f5e2ee344739bdeeb0dffb8` (#8118 리뷰 head). [설계와 코드 진입점 34행](../../design/i8274/tui-o-agy.design.md#1-진입점-전수표)

기존 `WriterAdoption.tla`에 `AgySpec`을 추가했다. 기존 `Spec`, `LateSpec`, 이를 확장하는 `WriterThreadPrep/ChildSpec`의 agy 상태는 상수다. 새 시나리오는 이전 Legacy 실행의 자체 영수증으로 채택을 해제하고, 한 채널의 durable reservation이 매 턴의 프로세스에 독립적으로 살아남게 한다. #8118에서 가져온 조건은 정확한 이전 실행의 의무, 검증된 부모의 정책, 채택과 body claim의 관찰 경계다. 이 모델은 검증된 채널부터 시작하며 Discord 부모 조회 자체는 별도 수용 시험에 있다.

## 규칙과 관찰 값

| 규칙 | 확인하는 상태 |
|---|---|
| `AgyNoDuplicate` | 채택한 원본에 대한 Legacy 본문 0, 각 O 조각의 외부 생성 효과 최대 1. |
| `AgyNoSilentLoss` | terminal 의무가 있으면 종료까지 spool 보존, 이전 Legacy 의무를 건너뛴 채택 금지, 실패 종료에 가시적 증거 필요. |
| `AgyExactlyOneAnswer` | 성공 종료된 원본별 reply unit 1 및 모든 계획 조각의 생성 효과 정확히 1. `AgyDoneEnds`와 같이 읽으면 미종료로 조건을 피할 수 없다. |
| `AgyOffsetMonotone` | 연속 receipt high-water mark에 오프셋 일치. 두 조각 뒤 Done 재읽기가 high-water mark를 보존한다. |
| `AgyEndOnce` | durable 종료 CAS 최대 1, CAS 뒤에만 종료 효과 적용, 완료 전 모든 효과 확인. |
| `AgyNoPrematureBody`, `AgyInputIdentity` | 최초 본문은 Done 후, 이전 child의 시도가 다음 원본에 적용되지 않음. |
| PROPERTY `AgyDoneEnds` | Done을 기록한 각 원본이 결국 1회 종료되고 effect들이 적용됨. 후속 턴의 입장은 이전 턴의 종료를 증명한다. |
| PROPERTY `AgySecondTurnEnds`, `AgyFailedEnds`, `AgyHeadlessKept` | 두 번째 입력도 종료, Done 없는 실패가 표시되고 종료, opt-out 루틴이 기존 답을 완료. |

모델의 분할 길이는 1·3개다. `AgyReceipt` 두 번 뒤 `AgyRepeatDone`이 가능하고 세 번째 조각만 남는다. `AgyPost`와 receipt 사이에 권한이 사라질 수 있으며, 재시작한 writer는 Prepared를 유지한다. `AgyFinish`는 CAS, `AgyApplyEnd`의 세 단계는 footer, inflight·lease 정리, 큐 알림을 뜻한다. `AgyRejectFinish`는 CAS 앞과 projection 사이에 한 번 실패할 수 있다. authority 회복과 재시도, 최종 응답·검증 가능한 송신 결과, 열린 이전 Legacy receipt에 약한 공정성을 둔다. 종료 후 source 삭제는 별도 행동이다. 두 번째 입력은 TTL이나 삭제에 의존하지 않는다.

## TLC 1.7.4 결과

14개 agy cfg가 모두 기대와 일치했다. 정상 5개와 위반 대조군 9개를 실행했다.

| cfg (공통 접두사 `WriterAgy`) | 기대 위반 | 실제 위반 | 반례의 마지막 행동과 단계 |
|---|---|---|---|
| `Long.cfg` | 없음 | 없음 | 108개 고유 상태. Done, 세 조각, 권한 회복, 종료 저장 거부. |
| `Short.cfg` | 없음 | 없음 | 1개 조각 및 종료. |
| `TwoTurns.cfg` | 없음 | 없음 | 같은 논리 채널의 연속 입력 두 건과 각각의 종료. |
| `Failed.cfg` | 없음 | 없음 | Done 없는 child 실패의 가시적 종료. |
| `Headless.cfg` | 없음 | 없음 | 루틴의 기존 답 완료. |
| `Abandon.cfg` | `AgyDoneEnds` | `AgyDoneEnds` | 7 Done → 8 권한 회수 → 같은 상태에 머묾. |
| `BorrowOld.cfg` | `AgyNoSilentLoss` | `AgyNoSilentLoss` | 1 이전 Legacy 영수증보다 먼저 채택. |
| `DoubleEnd.cfg` | `AgyEndOnce` | `AgyEndOnce` | 18 종료 CAS → 19–21 효과 → 22 같은 Done으로 종료 재확정. |
| `DualOwner.cfg` | `AgyNoDuplicate` | `AgyNoDuplicate` | 8 O가 씰 읽음 → 9 Legacy도 본문 송신. |
| `EarlyPurge.cfg` | `AgyNoSilentLoss` | `AgyNoSilentLoss` | 7 Done 커밋 → 8 담당 writer 적용 전에 삭제. |
| `Preview.cfg` | `AgyNoPrematureBody` | `AgyNoPrematureBody` | 5 델타 append → 6 Done 전에 본문 송신. |
| `Repost.cfg` | `AgyNoDuplicate`, `AgyExactlyOneAnswer` | `AgyNoDuplicate`, `AgyExactlyOneAnswer` | 9 Prepared → 10 POST → 11 권한 회수 → 12 복원 → 13 재POST. 같은 결함이 있는 별도 24단계 경로에서는 조각 중복 상태로 종료. |
| `Rewind.cfg` | `AgyOffsetMonotone` | `AgyOffsetMonotone` | 11·14 두 조각 확인 → 15 Done 재읽기에서 오프셋 초기화. |
| `StaleChild.cfg` | `AgyInputIdentity` | `AgyInputIdentity` | 22 새 입력 → 24 새 child 시작 → 25 옛 child의 이벤트 수용. |

`Rewind`는 기존 `content_arms.rs:335,355` → `inflight/store.rs:497,502,516` 순으로, `Abandon`은 저장 거부 뒤 `guarded_persist.rs:77` → `runtime.rs:194,221` 순으로 소스에서 대조했다. 나머지는 제안 가드의 제거 실험이다. 설계 7절이 구현 후 동일 단계를 거치는 fault injection을 정의한다.

[Mermaid 차트](agy-adoption.chart.mmd)는 Long의 108개 상태와 286개 전이를 12개 상태와 16개 화살표로 묶었다. 같은 라벨 안의 델타 기록, Prepared, 메시지 생성, 앞선 조각 receipt, 진행 중인 종료 단계, 권한 변화와 동일 Done 재읽기는 차트의 자체 전이에 해당한다. 정확한 바이트 분할과 파일 내구성, wire correlation과 확인 불가능한 이력, 실제 세대 이동, 루틴 부수효과는 구현 시험으로 확인한다. 영구적으로 권한을 갖지 못하면 보류가 남으며 `Abandon`이 진행성의 경계를 보여준다.

재현은 상태 차트 디렉터리에서 실행한다. 다른 cfg의 반례는 같은 명령에서 cfg를 바꾸고 `--view`를 생략하면 개별 상태 trace를 얻는다.

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar JAVA=/opt/homebrew/opt/openjdk/bin/java
cd docs/statecharts/tui-o-writer
~/.claude/skills/statechart/scripts/statechart WriterAdoption.tla \
  --config WriterAgyLong.cfg --view agy-adoption.view.mjs --out "$(mktemp -d)/agy-long"
```
