Implementation code snapshot: `7f7e47d0b397d97527e5fefd92e48ac03fdfb9a4` (production source; Current baseline is listed below).

코드 기준 SHA(수정 전): `e285a5cff1e8f8eadfc87e297ad7a202531f3ed3`. 설정: `WriterWakeI8055Design.cfg`. 분류: **design / GREEN**.
모델 SHA256: `479f54730bb03c8c6d3cc2ce03f91bb0b1d09a1cf129bf9ba7904004115d4413`.

#8055의 좁힌 수리 범위는 기존 `IntakeRoute::Hold` 판정을 유지하고, 로컬 gateway와 host execution permit를 각각 확인한 뒤 기존 custody로 보관하고 ready kick과 보관 뒤 level 확인을 연결하는 경계다. 모델은 기존 권한 판정을 입력으로 추상화하며 서로 다른 gate의 포인터·enum·epoch 동일성을 요구하지 않는다. 새 boot phase/proof 타입은 이 모델 결과의 구현 전제가 아니다.

**모델 범위 밖: 정상 V1 local 실행 경로(이번 변경 없음)**

이 결과는 완료 ingress의 custody/실행/보인 거절, 실제 수락 뒤 checkpoint, 기존 source identity 중복 방지, bounded restart와 공정한 ready/실행을 검사한 설계 모델이다. 실제 payload bytes·DB/파일 원자성·owner 이동·최초 durable commit 중 프로세스 사망까지 증명하지 않는다. 중복 방지는 기존 source fence 계약을 사용하며 실제 V1/V2 경합은 Rust 시험으로 확인한다. 입력 보관 시도나 notice 시도만으로 완료를 선언하지 않는다. visible 거절은 NoSilentLoss를 충족하지만 custody/실행 없는 checkpoint는 허용하지 않는다.

Current는 기준 SHA의 수정 전 동작을 동결한 known RED이고, Design은 현재 구현 가정의 abstract 설계다. 최종 implementation SHA는 외부 report-3에 기록하며 이 문서가 속한 커밋 SHA를 자기참조하지 않는다. 모델 GREEN은 Rust 구현 시험의 통과를 대신하지 않는다. scratch completion-delta는 이 모델·cfg의 일부가 아니며 커밋 대상에서 제외한다.

# WriterWake

TLC reached 492 states and 1420 steps. The chart draws 10 states and 14 arrows.
The chart does not draw 1016 steps that stay in one chart state.

## Rules that hold
BootNoSilentLoss, BootExecuteOnce, BootCheckpointOnlyAccepted, BootPermanentNeverExecutes, NoDeadlock, BootHeldEventuallyExecutes

## 재현 및 증거

모델 재검증 원시 출력: /Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8055/sc-land/boot-design-2. 기존 BootSpec 전이와 oracle는 바꾸지 않았고 RouteSpec용 추가 변수는 이 cfg에서 고정했다. 별도 Gateway/self-wake 보완은 [RouteSpec 보고서](i8055-route-design.report.md) 범위다.

    unset TMUX TMUX_PANE
    TLA2TOOLS=/Users/itismyfield/.local/share/tla2tools/tla2tools-1.7.4.jar JAVA=/opt/homebrew/opt/openjdk/bin/java /Users/itismyfield/.codex/skills/statechart/scripts/statechart docs/statecharts/tui-o-writer/WriterWake.tla --config docs/statecharts/tui-o-writer/WriterWakeI8055Design.cfg --view docs/statecharts/tui-o-writer/i8055-writer-design.view.mjs --out <새-출력-디렉터리>
