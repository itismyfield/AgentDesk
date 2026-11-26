# Headless outbox 게시 후 idle JSONL 재처리

코드 기준 SHA `90f4794e8bc0097f14f2517fd2f32428488b2dc1`에는 선행 #8372 head `7ad445baaf`가 포함된다. `WriterAdoption`의 headless 모델은 첫 게시에서 끝난다. `OutboxTransaction`은 outbox INSERT의 트랜잭션 경계를 다룬다. 이 모델은 한 워커의 warm Claude wrapper 답에서 첫 게시가 확인된 뒤 동일 JSONL이 idle sink로 전달되는 경계를 다룬다.

## 상태 차트 진입점

| 변수나 전이 | 기준 코드 | 모델에서 허용한 값과 행동 |
| --- | --- | --- |
| `Source`, `Start`, `End` | `src/services/claude.rs:2765–2839`, `src/services/provider/output_reader.rs:38–71,177–202,220–242` | warm wrapper는 입력 전 시작 위치를 잡고 파일에서 결과를 읽는다. 실제 terminal 줄 끝과 파일을 재조회한 EOF는 서로 다른 관측이다. idle timeout의 synthetic Done에는 실제 terminal 줄이 없다. 모델은 실제 terminal을 읽은 정상 완료만 연다. |
| `Capture`, `Stage` | `src/services/discord/turn_bridge/runtime_handoff_loop.rs:199–219`, `src/services/discord/turn_bridge/terminal_outcome_delivery.rs:885–930`, `src/services/discord/turn_bridge/headless_delivery.rs:272–334,525–555` | Current는 warm pipe 파일의 정확한 terminal 구간을 body와 연결하지 않는다. Target은 worker가 게시하기 전에 동일 파일·세대·구간과 outbox 행의 연결이 저장된다고 가정한다. A와 별도 headless B는 각각 한 pending 행을 만든다. |
| `WorkerPost`, `MarkSent` | `src/server/outbox_worker.rs:83–110`, `src/server/mod.rs:2037–2059,2261–2277` | Discord 전송의 성공과 해당 행의 소유권 조건부 `sent` 갱신을 별도 행동으로 둔다. Pending과 Posted 상태의 행은 증거로 사용할 수 없다. |
| `ClearInflight` | `src/services/discord/turn_bridge/headless_delivery/durable_outbox.rs:65–107`, `src/services/discord/turn_bridge/terminal_outcome_delivery.rs:897–938`, `src/services/discord/turn_bridge/completion_postlude.rs:954–988` | bridge는 보인 `sent`를 확인한 뒤 성공한 자기 inflight를 정리한다. 모델에서 clear 후 idle은 이전 inflight의 턴 nonce를 요구하지 않는다. |
| `ExpireGrace` | `src/services/discord/session_relay_sink.rs:89`, `src/services/discord/session_relay_sink/supervision.rs:169–231` | idle loop는 matching inflight 또는 최근 inflight·세션 유예가 있는 동안 구간을 보류한다. 이후 idle 후보를 읽을 수 있다. 10초를 한 행동으로 추상화한다. |
| `Append`, `CanReplay` | `src/services/discord/session_relay_sink/supervision.rs:62–122,137–154,238–305`, `src/services/discord/session_relay_sink/idle_jsonl.rs:334–395,406–451`, `src/services/cluster/stream_relay/identity.rs:120–137` | 커서는 provider·채널·경로·파일 identity를 관찰한다. 같은 파일의 뒤 구간, 별도의 headless 후속, 파일이 교체되어 같은 세대와 숫자 오프셋이 다시 쓰이는 경우를 선택한다. 모델은 B를 반드시 관찰해 후속 답 안전성 검사를 실질적으로 수행한다. 파일 교체가 wrapper marker를 반드시 바꾼다는 가정은 없다. |
| `ConsumeProof`, `IdlePost` | `src/services/discord/session_relay_sink/terminal_handoff.rs:96–165`, `src/services/discord/session_relay_sink/turn_parser.rs:142–223`, `src/services/discord/session_relay_sink/routing.rs:17–43`, `src/services/discord/session_relay_sink.rs:456–478,819–834`, `src/services/discord/session_relay_sink/task_notification_context.rs:510–646` | Current는 이미 확인된 idle frontier보다 뒤인 rowless 결과를 새 메시지로 보낼 수 있다. Target은 후보 구간을 발신 provider·채널·세션·경로·dev/ino·세대·범위가 동일한 `sent` 증거로 덮을 때 소비한다. 보이지 않은 B와 증거를 연결하지 못한 후보는 기존 idle 게시 경로를 사용한다. |
| 범위 밖의 출처와 수명 전이 | `src/services/discord/session_relay_sink/terminal_handoff.rs:98–124`, `src/services/discord/turn_bridge/guards.rs:308–324`, `src/services/discord/turn_bridge/headless_delivery.rs:447–521`, `src/services/agents/delegation.rs:273–290` | Native Codex idle, outbox 없는 직접 fallback, 취소·비정상 종료, 요청자 결과 카드, 호스트 재시작은 이 실행의 모델 입력에 없다. 동일 파일 교체는 포함한다. |

A와 B의 응답 본문 값은 같다고 가정한다. 결정은 occurrence의 좌표와 `sent` 행만 읽는다. 정규화·형식화·멀티 청크, crash 후 디스크와 PG의 부분 실패, 유일성 키의 구현, 실 파일 조각 순회는 추상화했다. 별도 headless B와 기존 A proof 사이에는 서로 다른 구간을 유지한다. `Append` 및 각 진행 전이에 대한 약한 공정성은 전송과 idle poll이 결국 처리되는 실행을 가정한다. 무한 재시도 또는 영구 전송 실패는 포함하지 않는다.

## 검사 결과

`NoDuplicate`는 각 실제 terminal의 게시를 최대 한 번으로 제한한다. `NoQuietLoss`는 소비 완료된 terminal의 실제 게시를 요구한다. 두 안전 규칙을 합치면 소비된 각 terminal은 정확히 한 번 게시된다. `EveryAnswerPosts`는 유계의 두 답이 결국 실제로 게시되도록 검사한다. 뒤에 붙은 다른 레코드와 같은 본문의 후속 답은 각각의 독립 occurrence로 센다.

| 구성 | statechart 종료 | 결과 | 첫 반례 |
| --- | ---: | --- | --- |
| [Current](headless-idle-replay-current.chart.mmd) | 1 | `NoDuplicate` RED | 정상 outbox 게시 → `sent` → inflight 삭제 → 유예 종료 → 동일 A idle 게시 |
| [Target](headless-idle-replay-target.chart.mmd) | 0 | 두 안전 규칙과 `EveryAnswerPosts` GREEN | 없음 |
| [NoLink](headless-idle-replay-no-link.chart.mmd) | 1 | `NoDuplicate` RED | 증거가 존재하지만 idle 판정 연결을 제거하면 A를 다시 게시 |
| [EofRange](headless-idle-replay-eof-range.chart.mmd) | 1 | `NoQuietLoss`와 `EveryAnswerPosts` RED | A proof가 실제 terminal 뒤에 추가된 B까지 덮어 B를 게시 없이 소비 |

Current·Target·NoLink는 상태 43개와 전이 50개, EofRange는 상태 51개와 전이 58개에 도달했다. 그림은 같은 표시 상태로 이어지는 전이를 생략한다. 공식 `scripts/ci/statecharts_tlc.py`는 네 구성에서 예상 위반과 실측이 모두 일치했으며 9.1초에 4/4를 반환했다. 세 경우의 RED는 모델 대조군 결과다. 소스 커밋의 Rust 시험 또는 Rust 뮤턴트 실행을 대신하지 않는다.

## 관측과 시험 인계

11-26 KST 20:23:49.591에 warm follow-up이 종료됐다. 20:23:51.511~51.760에 headless outbox가 첫 답을 게시했고, 20:23:52.565에 동일 inflight가 제거됐다. 20:24:02.551~02.580에 rowless sink가 두 번째 메시지를 게시했다. 20:24:07.165에 채널 조회에서 두 건이 확인됐다. 20:23:59.100에 나온 위임 결과 카드는 별도 요청자 채널이었다. 이는 Current의 7단계 반례와 대응한다. 실제 서버 로그는 정확한 byte 범위를 출력하지 않으므로 그 값은 모델에서 0·1·2로 추상화했다.

회귀는 실제 outbox worker의 wire 호출과 `sent` 확인을 거친 뒤 동일 파일·세대·구간을 rowless sink에 전달해 총 게시 수를 센다. 같은 본문의 B와 A terminal 뒤 새 구간도 각각 한 번 보여야 한다. 부정 대조에서는 동일 marker라도 다른 파일에 이전 proof를 적용하지 않는다. 모델 Target에서 가정한 proof 저장·조회 연결은 런타임 시험과 코드 검토에서 확인할 대상이다.

재현은 저장소의 `docs/statecharts/session-relay`에서 아래 명령을 각 cfg에 실행한다.

```sh
unset TMUX TMUX_PANE
export TLA2TOOLS="$HOME/.local/share/tla2tools/tla2tools-1.7.4.jar"
export JAVA=/opt/homebrew/opt/openjdk/bin/java
"$HOME/.claude/skills/statechart/scripts/statechart" HeadlessIdleReplay.tla \
  --config HeadlessIdleReplayTarget.cfg --view headless-idle-replay.view.mjs \
  --out "$(mktemp -d)"
```
