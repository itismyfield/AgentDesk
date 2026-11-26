# 게시된 답 앞의 wrapper sentinel과 idle 읽기 창

기준 코드 `ba47ddeae6c2e42eb093bed87855b87940a89140`. 기존 `HeadlessIdleReplay`의 `Spec`은 성공한 outbox 게시가 같은 terminal의 재게시를 막는지 검사한다. 이번 `CursorSpec`은 같은 A의 outbox가 `sent`이고 inflight와 10초 유예가 끝난 시점에서 시작한다. 같은 파일의 후속 B도 A와 별개의 답이다. 두 본문은 같을 수 있으며 상태 차트는 바이트 좌표만 사용한다.

## 상태 차트 진입점

| 변수나 전이 | 기준 코드 | 허용된 값·행동 |
| --- | --- | --- |
| `cursor`, `committed`, source | `src/services/discord/session_relay_sink/supervision.rs:63–124,138–155,235–250` | 현재 디바이스·inode·세대의 local cursor와 이미 전송이 확인된 frontier를 별도로 유지한다. 첫 좌표 0은 앞선 답 result의 끝에서 잰 상대값이다. 파일 교체에서 증거를 재사용하지 않는 조건은 기존 `Spec`의 `ReplacedFile`이 검사한다. |
| `PrefixKinds`, `CursorConfirmPrefix` | `session_relay_sink/supervision.rs:252–281`, `session_relay_sink/headless_replay.rs:50–73,83–125` | 앞 구간은 wrapper sentinel, 미게시 가시 X, 실패한 outbox의 미게시 X 또는 pending X다. pending/processing은 대기이고 실제 게시 후 `sent`가 된 X만 자기 범위를 소비한다. 실패 행은 조회의 소유 대상에서 빠져 가시 레코드가 idle 게시를 기다린다. |
| `CursorGapAdvance` | `session_relay_sink/supervision.rs:239–294`, `session_relay_sink/headless_replay.rs:19–29,50–52`, 목표 분기 | 다음 proof가 같은 source에서 `sent`인 경우, 시작점까지 남은 구간이 완전한 `ready_for_input` 레코드뿐이거나 이미 확인된 frontier가 덮을 때만 local cursor를 그 proof의 시작점으로 옮긴다. 파일과 세대를 다시 확인한다. X 미게시 또는 pending이면 건너뛰지 않는다. |
| `CursorConsumeSent`, `CursorPrefixProof` | `session_relay_sink/supervision.rs:267–280`, `session_relay_sink/headless_replay.rs:19–29,50–73` | 현재 cursor를 포함하는 `sent` 구간만 정확한 terminal 끝까지 넘긴다. 이전 pending X가 게시된 경우도 같은 경계 규칙을 사용한다. |
| `CursorAppend`, `CursorOpenWindow` | `src/services/discord/session_relay_sink.rs:91`, `session_relay_sink/supervision.rs:74–77,239–245,282–294,321–365` | A의 시작은 75, 끝은 706,908, B의 끝은 1,312,781이다. B가 들어온 뒤 local cursor 0의 1,048,576바이트 상한은 마지막 complete newline 1,040,066으로 잘린다. durable `committed`는 suffix 시작만 이동시키며 창의 끝은 local cursor에서 계산한다. 정상 75바이트 sentinel은 `src/services/tmux_wrapper.rs:240–262,402–403`, `src/services/tmux_common.rs:1422–1445`에 있다. |
| `CursorPostVisiblePrefix`, `CursorSinkSkip`, `CursorPostB` | `session_relay_sink/terminal_handoff.rs:139–187,198–251`, `session_relay_sink/turn_parser.rs:101–106,221–249`, `src/services/cluster/stream_relay.rs:810–850` | 프레임은 새 parser에서 가시 terminal 끝으로 나뉜다. 기존 sink가 같은 파일과 세대의 `sent` A를 생략할 때 sequence에 Delivered는 남지만 local cursor는 그대로다. 미게시 X와 B는 실제 전송 후 해당 terminal 끝이 확인된다. |
| `CursorRetireCommitted` | `session_relay_sink/supervision.rs:321–335` | 게시가 확인되어 이미 덮인 창은 local cursor도 전진한다. 대기·실패·기록 미확인 결과에는 이 완료를 적용하지 않는다. |

`ASkippedBExactlyOnce`는 A의 outbox 게시를 한 건으로 고정하고 A의 idle 재게시와 B의 중복을 금지하며 B가 소비될 때 실제 B 게시가 한 번 있었음을 요구한다. `NoUnpostedPrefixSkipped`는 가시 X를 게시하기 전 그 경계를 넘지 못하게 한다. `ReadStartAdvances`와 `LaterAnswerPosts`는 B가 완전히 기록되면 시작점이 A 끝 이상으로 전진하고 B가 한 번 게시되어야 한다고 검사한다. 약한 공정성은 스캐너와 전송에 결국 실행 기회가 있고 pending X가 결국 게시된다는 가정이다. 영구 전송 실패, source 교체 중 재시도, 단일 답이 창 상한보다 큰 기존 경계는 이 cursor 검사에서 제외했다.

## 검사 결과

| 구성 | 결과 | statechart exit | 유일 상태·전이 |
| --- | --- | ---: | ---: |
| [CursorCurrent](headless-idle-replay-cursor-current.chart.mmd) | RED `ReadStartAdvances`, `LaterAnswerPosts` | 1 | 57·76 |
| [CursorTarget](headless-idle-replay-cursor-target.chart.mmd) | GREEN 안전 규칙과 두 PROPERTY | 0 | 64·79 |
| [CursorNoAdvance](headless-idle-replay-cursor-no-advance.chart.mmd) | RED `ReadStartAdvances`, `LaterAnswerPosts` | 1 | 86·132 |
| [CursorUnsafePrefix](headless-idle-replay-cursor-unsafe-prefix.chart.mmd) | RED `NoUnpostedPrefixSkipped` | 1 | 42·51 |

Current 반례는 B append → 1MiB 창 읽기 → A 생략 → 같은 창 다시 읽기의 순환이다. A 생략과 함께 cursor가 0에 머물러 이후 창에도 B terminal이 없다. NoAdvance는 sentinel을 확인하는 단계가 추가된 뒤 같은 순환으로 돌아간다. UnsafePrefix에서는 미게시 X를 보낸 적이 없는 상태에서 그 끝으로 바로 이동하면 안전 규칙을 위반한다. Target은 정상 sentinel을 확인하면 `0 → 75 → 706,908`, X가 있을 때는 실제 게시 확인 뒤 같은 순서를 밟는다. 새 창은 B terminal을 포함한다.

공식 `statecharts_tlc.py`가 기존 네 구성과 새 네 구성 8/8을 예상 결과대로 확인했다. 종료 0, 총 6.4초. 그림은 같은 표시 상태 안의 전이를 생략한다. Rust와 PG의 실제 실행은 GCP 범위다.

```sh
unset TMUX TMUX_PANE
export TLA2TOOLS="$HOME/.local/share/tla2tools/tla2tools-1.7.4.jar"
export JAVA=/opt/homebrew/opt/openjdk/bin/java
"$HOME/.claude/skills/statechart/scripts/statechart" HeadlessIdleReplay.tla \
  --config HeadlessIdleReplayCursorTarget.cfg --view headless-idle-replay-cursor.view.mjs \
  --out "$(mktemp -d)"
```
