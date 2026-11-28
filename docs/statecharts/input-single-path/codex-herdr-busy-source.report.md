# Codex 바쁜 입력의 transcript 원천 — clear 뒤 Herdr·tmux (#8426 D1)

코드 기준 `b08f35f2a05166cd6c340db6c10cb50b6a23ac39`(D1 구현 커밋, base `fcc3fb73ba`). 새 모델 `CodexHerdrBusySource.tla`는 #8114 `HerdrBusyActiveSource`(Claude 전용 수리)를 Codex로 넓힌 독립 모듈이다. 기존 모델·cfg·`expected.tsv` 행은 바이트 그대로 두고, 9행만 새로 더했다. 모델 결과는 Rust 시험을 대신하지 않는다.

## 규칙

clear 뒤 바쁜 턴에 들어온 사람 입력은 다음 둘 중 하나로 끝난다.
- 검증된 현재 원천과 알려진 running turn이 있으면 같은 턴 안에 주입된다.
- 그렇지 않으면 사유를 남기고 거절된다.

키는 현재 세션의 rollout으로만 귀속된다.

## 코드 근거와 값

| 모델 값·행동 | 코드 좌표 (`b08f35f2a0`) | 의미 |
|---|---|---|
| `rowPath=Absent` after `Clear` | `router/message_handler/provider_isolation.rs:340-369`(`:167`, `:354`) | intake seed는 tmux pane 생존으로 경로를 정한다. Herdr 논리 키와 clear 직후 tmux에는 pane이 없어 None이다. |
| 턴 뒤에만 경로 stamp | `codex/herdr_turn.rs:836-850` → `turn_bridge/runtime_handoff_loop/watcher_handoff.rs:86` | 턴 도중에는 행 경로가 비어 있다. |
| `binding` stays `Old` after `Clear` | `herdr_terminate.rs:228-238`(withhold만), `commands/control/managed_reset.rs:140-150`(temp 파일만), `tui_prompt_dedupe.rs:32`(TTL 24h) | clear는 runtime binding을 지우지 않는다. |
| Legacy 정책은 binding을 검사 없이 읽음 | `tui_prompt_dedupe/runtime_binding/codex_verified.rs:453-491`(`:467-468`) | `consumer_allowed`는 Legacy에서 Permitted다. |
| `Prove`: binding=Current, Herdr bound | `tui_prompt_relay/herdr_source.rs:211-224`, `runtime_binding/codex_hook.rs:303-` | 새 hook이 원천을 발행한다. attach는 live binding이 새 Source와 같을 때만 admit하고 Bound로 만든다. |
| `HerdrGate` (not Bound → refused) | `owner_input.rs:82-140`(`read_as`, `:94`), `queue_io/input_owner/claude_handoff.rs:549-566`(`bound_herdr`), `owner_input.rs:442-444` | Pending 행은 Absent → `busy_turn_unobserved`. |
| `Request` | `owner_input/busy_source.rs`(`live_source`, `bound_codex_herdr`) | Now = 행 경로. FixHerdr(D1) = Herdr+Bound+CodexTui binding(경로·native)이면 binding, 아니면 행. FixAnyHost = 브리프 원안(호스트 무관 binding 먼저). |
| `Submit` | `owner_input.rs:1696-1703`(CodexBusy\|CodexHerdrBusy `read_turn`) | 빈 경로는 `NotSent("busy_turn: transcript unavailable")`다. |
| `knownBusy` | `owner_input.rs:250-288`(`has_active_turn` → `hand_busy`) | running turn이 없으면 바쁜 경로에 오지 않는다. |

입력은 clear 창(`window`, 새 pane 생존 + 옛 binding 잔존) 안에서도, proof 뒤(`proven`)에도 올 수 있다. Select는 그 시점의 상태를 읽는다. 모든 local 행동에 weak fairness를 둔다. native 턴은 끝나지 않으므로, 진행성 GREEN은 턴 종료를 기다리지 않는 경로가 있다는 뜻이다.

## 결과 (TLC 1.7.4, `scripts/ci/statecharts_tlc.py`)

| cfg | Policy·Host·Scenario·Mutant | 기대 | 실제 |
|---|---|---|---|
| `CodexHerdrBusySourceNowHerdr` | Now·herdr·healthy | `HerdrProvenInjected` RED (운영 결함) | 일치 |
| `CodexHerdrBusySourceFixHerdrHerdr` | FixHerdr(D1)·herdr·healthy | GREEN | 일치 |
| `CodexHerdrBusySourceFixAnyHostHerdr` | FixAnyHost·herdr·healthy | GREEN (Bound 관문이 창을 닫음) | 일치 |
| `CodexHerdrBusySourceFixAnyHostTmux` | FixAnyHost·tmux·healthy | `NoForeignSourceKey` RED (clear 창의 옛 binding) | 일치 |
| `CodexHerdrBusySourceFixHerdrTmux` | FixHerdr(D1)·tmux·healthy | GREEN (행 경로 유지, 사유 거절) | 일치 |
| `CodexHerdrBusySourceFixHerdrUnverified` | FixHerdr·herdr·unverified | GREEN (attach 없음 → 사유 거절) | 일치 |
| `CodexHerdrBusySourceFixHerdrUnknown` | FixHerdr·herdr·unknown | GREEN (running turn 모름 → 사유 거절) | 일치 |
| `CodexHerdrBusySourceMutNoBoundGate` | FixHerdr·herdr·healthy·NoBoundGate | `NoForeignSourceKey` RED | 일치 |
| `CodexHerdrBusySourceMutUnknownBusy` | FixHerdr·herdr·unknown·UnknownBusy | `NoKeyWithoutKnownBusy` + `UnverifiedRefused` RED | 일치 |

공식 checker 실제 결과는 **PASS 9/9 configs, GREEN 5 / expected RED 4**, 총 22.8초다. config별 TLC 실행은 2~4회, 최장 3.5초로 300초 한도 안이다. 차트는 만들지 않았다.

## 한계

- 한 입력, 한 번의 clear, 하나의 실행을 다룬다.
- Herdr gate의 nonce·pane 고정, 붙여넣기 불확실성, 복수 입력, 부팅 복원은 기존 입력 모델과 Rust 시험의 계약이다.
- tmux에서 raw seed가 성립하지 않아 wrapper 경로가 행에 들어가는 경우(조사 §9-1, 운영 미관측)는 D1이 손대지 않으므로 모델에 넣지 않았다. #8328이 덮는다.
- source policy가 verified인 pane(canary)은 marker 대조로 옛 binding이 막힌다. 모델은 더 약한 Legacy 쪽을 가정했다.
