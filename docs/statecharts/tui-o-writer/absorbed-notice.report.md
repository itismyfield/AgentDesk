# 턴 도중 흡수된 사람 입력의 공지

코드 기준은 `b04ea56650e5397989d33f1be199be21a83c8d0e`와 #8437 수리다.
[기존 공지 정산 모델](notice-evidence.report.md)은 Materialized 이후의 소유권과 보류를 검증한다.
[WriterAbsorbedNotice](WriterAbsorbedNotice.tla)는 해당 근거가 생기기 전부터 관측기 인계, 원본 행 캡처,
Materialized append, Posted 확인과 재시작 재생을 연결한다.

## 진입점

아래 코드 좌표는 기준 SHA에 고정했다. 행 번호가 바뀌어도 같은 함수에서 시작한다.

| 모델 행동 | 코드 진입점과 효과 |
| --- | --- |
| Handoff | `src/services/tui_o/writer/actor/notice_work.rs:132–169`의 `start_notice_migration`이 `MigrationStarted`를 append한다. `src/services/tui_o/store/input_notice_floor.rs:55–76`이 그 기록을 확인한다. |
| Observe | `src/services/discord/tui_prompt_relay.rs:416–442`가 observer 결정을 전달한다. `src/services/discord/tui_prompt_relay/synthetic_start_wiring.rs:141–149,170–181`은 durable 인계 후 announced prompt ID를 기록하고 반환한다. |
| Capture | `src/services/tui_o/writer/actor.rs:905`의 `read_sources`가 source poll을 시작한다. `src/services/tui_o/writer/rotation/retained_capture.rs:35–119`가 완전한 행을 spool에 append하고 `owe`로 전달한다. |
| Project | `src/services/tui_o/writer/rotation/owe.rs:110–135` → `src/services/tui_o/writer/pieces.rs:127` → `src/services/tui_o/writer/input_notices/evidence.rs:8–54`. `src/services/tui_o/writer/input_notices.rs:114,149–215`와 `src/services/tui_prompt_dedupe/extract.rs:165–187`에서 후보와 키를 정한다. `src/services/tui_o/writer/actor/notice_work.rs:213–261`이 Materialized를 기록하고 `src/services/tui_o/store/ledger/input_notices.rs:131–176`이 원본 행과 키를 대조한다. |
| Prepare | `src/services/tui_o/writer/actor/notice_work.rs:285–495`의 `prepare_notices`가 게시 가능 근거를 모은다. `src/services/tui_o/writer/deliver.rs:550–586`가 같은 키와 근거의 Prepared를 POST 전에 기록한다. |
| Post, Confirm | `src/services/tui_o/writer/deliver.rs:592–606,690–714`에서 POST를 시작하고 확인된 결과를 Posted로 기록한다. |
| Restart, Replay | `src/services/tui_o/writer/actor.rs:345–388` → `src/services/tui_o/writer/rotation/readers.rs:94–123`에서 저장된 spool을 같은 `owe`로 재생한다. 보존된 ledger와 `src/services/tui_o/writer/replay.rs:15–48`의 정산 여부가 이미 게시된 키의 재게시를 막는다. |

관측기가 writer에 넘긴 뒤의 적격 인간 입력 하나에 일반 `user` 행만, 흡수된
`queued_command`만, 또는 두 형태가 남을 수 있다. 첫 두 가지는 11-26 트랜스크립트에서 관찰됐다.
두 형태의 경우 큐 행의 `attachment.source_uuid`와 user 행의 native UUID가 같은 입력을
식별하는 전제로 테스트한다. 관측한 busy 입력에서 `source_uuid`는 `remove.commandUuid`와 같고
attachment 최상위 UUID는 달랐다. 같은 본문이라는 사실은 이 모델의 식별 근거가 아니다.
이미 인계된 채널의 대상 입력으로 한정했으므로 인계 전에 별도로 발송한 hook POST의 교차 경계는
[예약 공지 모델](scheduled-notice.report.md)의 기존 범위다.

## 규칙과 결과

- `NoticeExactlyOnce`는 게시가 1건을 넘으면 즉시 깨지고, 관측·행 처리·재생·확인이 끝났을 때 1건보다 적어도 깨진다.
- `EventuallySettles`는 완료 조건이 실제로 도달 가능하도록 검사한다. 원본 행이 도착하고
  각 분류와 게시 단계가 진행되며 한 번의 재시작이 결국 일어난다는 약한 공정성을 둔다.
- `NoDeadlock`은 완료 뒤 정상 대기를 허용한다. 모델에는 바인딩 오류, 소유권 미확정, 영구적인
  Discord 전송 미확인, spool GC, 프로세스 무한 재시작이 없다. Prepared 뒤 재시작에서는 history 확인으로 재게시 없이 정산할 수 있는 POST를
  보존한다.

| Config | 도달 상태 | 실제 결과 |
| --- | ---: | --- |
| Current | 147 | RED. `NoticeExactlyOnce`가 command-only에서 깨진다. 권한 인계 → 흡수 행 보존 → 관측기가 직접 게시를 생략 → 재시작 → 재생이 행을 건너뛰어 최종 게시 0건. 최단 5단계. |
| Target | 197 | GREEN. 두 형식을 같은 입력 키로 분류한다. 일반 입력, 흡수 입력, 두 형태가 남는 경우의 게시와 재생을 모두 탐색했다. |
| SplitKey | 349 | RED. `NoticeExactlyOnce`가 두 행을 물리 키로 분리할 때 깨진다. 권한 인계 → 두 원본 행 보존 → 각 행에서 Materialized·Prepared·POST → 게시 2건. 최단 9단계. |

대응 Rust 시험은 `absorbed_human_command_posts_one_writer_notice_across_restart`와
`queued_command_and_user_with_the_same_input_id_share_one_notice`다. 상태 차트는
같은 표시 상태에 머무는 전이를 접는다. Target은 417단계 중 표시 상태가 같은 180단계를 접어
11개 상태와 20개 화살표로 그린다. [Target 차트](absorbed-notice.chart.mmd).

## 재현

TLC 1.7.4, single worker. CI checker의 3개 설정 선택은 8회 TLC 검사에서 예상과 일치(7.0초, rc 0).
Statechart 실행은 Current/SplitKey rc 1, Target rc 0이다. 위 상태·단계 수는 Statechart의 전체 그래프다.

    unset TMUX TMUX_PANE
    export TLA2TOOLS="$HOME/.local/share/tla2tools/tla2tools-1.7.4.jar"
    export JAVA=/opt/homebrew/opt/openjdk/bin/java
    cd docs/statecharts/tui-o-writer
    "$HOME/.codex/skills/statechart/scripts/statechart" WriterAbsorbedNotice.tla \
      --config WriterAbsorbedNoticeTarget.cfg --view absorbed-notice.view.mjs --out /tmp/sc-absorbed-notice
