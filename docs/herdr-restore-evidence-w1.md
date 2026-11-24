# Herdr 복원 창 증거 관측

대상은 Herdr 0.9.5, protocol 22다. 서버 바이너리를 복사하고 임시 HOME·XDG·소켓을 지정한 뒤, 격리 서버의 원시 `session.snapshot`과 `pane.process_info` 응답을 수집했다. 현재 제공되는 응답으로는 미사용 창도 자동 정리 요건을 증명할 수 없다.

| 확인할 증거 | 실제로 읽은 값 | 판정 |
| --- | --- | --- |
| 기존 AgentDesk 창의 불변 복원 계보 | `workspaces[].label`, `workspace_id`, `panes[].pane_id`, `terminal_id`. 복원 뒤 같은 `workspace_id`와 `pane_id`에 새 `terminal_id`가 나왔다. 같은 표시의 별개 창이 공존했고, 복원 원본이 없는 새 서버 세대가 이전 ID를 다시 발급했다. | 불명. S04 create 영수증은 첫 실행의 owner·nonce·pane을 묶는다. 복원 응답에는 이 영수증을 가리키는 계보가 없다. 대응 영수증이 없는 동명 창이나 ID 재사용 창은 부적격이다. |
| 복원 직후부터 활동 없음 | `panes[].revision`, `pane.process_info.foreground_processes`, `shell_pid`. 첫 조회 전에 명령을 실행한 표식을 확인했다. 이후 응답은 셸만 보였고, 복원 후 해당 pane의 판독 revision은 `0`이었다. | 불명. 응답은 복원 시점부터의 입력·활동 이력을 제공하지 않는다. 시험에서 첫 조회 전에 사용한 창은 부적격이다. |
| 남은 작업 없음 | 한 탭과 한 pane을 표시한 workspace의 `tab_count`·`pane_count` 및 `foreground_process_group_id`·`foreground_processes`. 동일 셸 밑에서 백그라운드 자식의 부모 PID를 조회 전후 확인했다. 백그라운드 자식이 없는 때와 있는 때의 프로세스 응답이 같았다. | 불명. `pane.process_info`는 백그라운드 작업을 열거하지 않는다. 시험에서 살아 있는 자식을 확인한 창은 부적격이다. |

W1은 S04 create 포트의 성공 응답, 복원 전후의 서버 witness, 동명 대조 창과 새 세대의 ID 재사용, 두 프로세스 응답을 함께 기록한다. 세 관측 시나리오 모두 조회 결과에 pane이 있고, 서버 요청 기록에서 `pane.close`는 0건이었다. fixture가 기록한 사용 및 작업의 정답은 격리된 셸이 만든 파일과 프로세스 조회에서 얻었다.

W2는 서버가 권한 있는 원래 소유와 불변으로 연결한 계보, 복원 때부터 이어진 활동 이력, 남은 전경·백그라운드 작업과 세션 및 추가 pane의 부재를 제공해야 한다. 닫는 요청에서도 현재 서버 세대와 이 세 증거를 동일한 원자 연산에서 확인해야 한다. [설계와 조건](herdr-parity-next-lifecycle.design.md#처방과-사용자-결정)

## 격리 시험 실행

macOS에서 Herdr 0.9.5 바이너리를 임시 경로에 복사한다. 기존 T19 fixture는 임시 경로 아래에서만 Herdr와 가짜 provider를 실행하고, 외부 네트워크를 차단하며, 연결마다 서버 witness를 확인한다. `lib_test_bin`에는 현재 소스의 `cargo test --lib --no-run`으로 만든 lib 시험 바이너리를 지정한다.

```sh
unset TMUX TMUX_PANE
scratch=/private/tmp/adk-e-t19-$$
mkdir -m 700 "$scratch"
mkdir "$scratch/bin"
cp -L /opt/homebrew/bin/herdr "$scratch/bin/herdr"
ADK_TEST_HERDR_ROOT="$scratch" \
ADK_TEST_HERDR_BIN="$scratch/bin/herdr" \
ADK_TEST_HERDR_PIDS="$scratch/w1.pids.jsonl" \
  "$lib_test_bin" --ignored --exact \
  services::discord::krx_session_host_test_host::launch_host_t19_real::herdr_t19_resume_true_restores_claude_and_codex_as_shells_only \
  --nocapture
```

시험 출력에서 한 건 실행·한 건 통과와 `w1_result`의 `pane_close=0`을 확인한다. `w1_created`, `w1_snapshot`, `w1_process` 이벤트의 원시 응답은 임시 JSONL에 남는다. 상태 차트는 `HerdrClearRestoreRestore*.cfg` 7개를 그대로 사용한다. [기대 결과와 실행법](statecharts/session-lifecycle/herdr-clear-restore.report.md#검증-규칙과-설정)
