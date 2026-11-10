분류: known RED / P2 recheck mutant

코드 SHA(수정 전): b1c74c75da0940f4dfc30862dc7d293b2c717555. P2 수정 후 production SHA: 2d9fb9d663044929d9389647146067f24f706fe4.
모델 SHA256: 479f54730bb03c8c6d3cc2ce03f91bb0b1d09a1cf129bf9ba7904004115d4413. TLC 1.7.4 / JAVA=/opt/homebrew/opt/openjdk/bin/java, one worker.

host readiness와 input Gateway는 독립적이다. Readiness::accepts는 ready/live/Owned/not-stopping/resumed를 읽고, for_input_placement는 successor_unproved를 이 readiness보다 먼저 Hold로 판정한다. RouteSpec은 일회성 callback 또는 postcommit wake가 이미 도착한 host-ready/Gateway-Hold 상태의 원본 하나를 다룬다. common preclaim과 late claimed-front의 hold 뒤 재확인을 같은 전이로 추상화한다.

규칙 BootRouteNoBlockedSelfWake: Gateway Hold가 유지되면 스스로 같은 입력의 wake를 다시 예약하지 않는다. Current는 host ready만 확인하는 수정 전 분기, Design은 host ready와 Gateway를 모두 확인한다. 일회성 외부 wake가 Hold 상태에서 도착하는 것 자체는 허용한다.

시계·주기 polling·새 executor를 모델링하지 않는다. RouteGatewayOpens는 외부 source 판정의 복구이고 RouteExistingOwnerWake는 기존 owner/start/reconcile 이벤트다. 두 사건의 약한 공정성은 환경 전제이며 Gateway가 열린 것만으로 이 코드가 새 자동 wake를 보낸다고 주장하지 않는다. 두 전제가 없으면 held liveness를 보장하지 않는다. gateway lease와 local execution 권한은 이 RouteSpec에서 유효하게 고정한다. lease 상실/provider 불일치 등의 거절은 기존 BootSpec의 별도 범위다.

**모델 범위 밖: 정상 V1 local 실행, Codex fresh-input 예외의 입출력 증거, physical keys, DB/파일 원자성, owner 이동, gate epoch 경쟁.** InputPath는 변경하지 않았다. 모델 결과는 Rust 실제 경계 시험을 대신하지 않는다.

# WriterWake

TLC reached 20 states and 44 steps. The chart draws 6 states and 6 arrows.
The chart does not draw 12 steps that stay in one chart state.
The view hides 20 steps, so the chart does not draw them.

## BootRouteNoBlockedSelfWake breaks
The shortest path has 2 steps:
0. host ready · Gateway Hold · 일회성 wake
1. Gateway Hold로 원본 보관: hold 커밋 뒤 아직 Gateway Hold
2. host ready만 보고 self-wake 재예약: Gateway Hold인데 self-wake 재예약

## Rules that hold
BootNoSilentLoss, BootExecuteOnce, BootCheckpointOnlyAccepted, BootPermanentNeverExecutes, NoDeadlock, BootHeldEventuallyExecutes, BootRouteEligibleHeldExecutes

## 재현 및 증거

원시 출력: /Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8055/sc-land/route-current-2. 새 출력 디렉터리로 재현한다.

    unset TMUX TMUX_PANE
    TLA2TOOLS=/Users/itismyfield/.local/share/tla2tools/tla2tools-1.7.4.jar JAVA=/opt/homebrew/opt/openjdk/bin/java /Users/itismyfield/.codex/skills/statechart/scripts/statechart docs/statecharts/tui-o-writer/WriterWake.tla --config docs/statecharts/tui-o-writer/WriterWakeI8055RouteCurrent.cfg --view docs/statecharts/tui-o-writer/i8055-route.view.mjs --out <새-출력-디렉터리>

chart와 report 본문은 위 공식 statechart 실행 산출물을 그대로 사용했다. view는 stuttering만 숨기며 bug path의 두 행동과 Gateway 복구/기존 이벤트를 숨기지 않는다.
