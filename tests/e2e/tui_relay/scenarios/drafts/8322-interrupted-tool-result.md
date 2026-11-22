# 초안: #8322 중단 자리표시 뒤 실제 결과가 와도 tui_o writer가 멈추지 않는다

상태: **비활성 초안**. 이 디렉터리는 census·continuous catalog·`run_tui_relay.py`가 읽지 않는다.
#8322 수정이 배포된 뒤 YAML로 옮겨 활성화한다. 그 전에 라이브로 돌리면 시험 셀 writer가
SchemaBlocked로 영구 정지하고 배포 health 게이트를 막으므로 돌리지 않는다.

## 재현할 transcript 모양

Claude Code가 같은 `tool_use_id`에 `tool_result`를 두 번 쓴다.

1. 세션이 끝날 때 남는 자리표시: `[Tool call interrupted: the session ended before this call's result was recorded…]`
2. 세션 재개 뒤 같은 호출의 실제 결과(예: `Exit code 137 …`)

첫 결과는 ledger에 excluded(`tool_result_internal`)로만 남고 게시되지 않는다. 운영 관측(11-22 09:54 KST,
1479671298497183835)에서는 두 번째 결과가 `SealOutcome::Conflict` → `WriterAlarm::SchemaBlocked`로 이어져
writer가 약 20초 간격으로 다시 멈췄다.

## 절차(제안)

- 셀: Claude TUI O 채널 시험 셀(canary 셀 제외). `isolation: required`, `destructive: true`.
- 1. Discord로 긴 Bash 호출을 시킨다: `python3 -c "import time; time.sleep(60); print('E8322_DONE')"`,
  끝나면 `[E2E:E8322:{run_id}:BODY]` 한 줄.
- 2. Bash가 도는 동안 provider 세션을 끝낸다(Claude 프로세스 종료). 지금 러너에는 이 단계가 없다.
  `kill_tui_process`는 파괴 단계 목록에만 있고 실행기가 없으므로, 활성화 때 실행기를 먼저 만든다.
- 3. 같은 세션을 재개시켜(같은 채널의 다음 Discord 입력) transcript에 실제 결과가 같은 `tool_use_id`로 붙게 한다.
  두 결과가 모두 기록됐는지 transcript에서 확인하지 못하면 UNEVALUABLE로 남긴다.
- 4. 재개 뒤 짧은 턴 `[E2E:E8322:{run_id}:AFTER]`를 보낸다.

## 판정

- `/api/health/detail`의 `tui_o_writer_readiness`가 대상 채널에서 `accepts: true`, `hold_reason: null`로 돌아온다.
- dcserver 로그에 대상 채널의 `SchemaBlocked`·"writer stopped and stays stopped"가 0건이다.
- `[E2E:E8322:{run_id}:AFTER]`가 provider 본문으로 정확히 한 번 게시된다.
- 게시된 단위(도구 카드·본문)의 내용 변경은 여전히 막힌다는 점은 이 라이브 시나리오가 아니라 #8322의 단위 시험이 지킨다.

## 활성화 조건

- #8322 수정 배포, 시험 셀 writer 준비 상태 확인.
- 세션 종료 단계의 실행기와 그 시험.
- 활성화 때 YAML로 옮기면 census(`CENSUS_LIVE`)·README 개수·continuous manifest를 같은 변경에서 갱신한다.
