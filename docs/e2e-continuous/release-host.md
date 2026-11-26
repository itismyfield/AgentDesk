# 릴리스 호스트 프로필: 기존 계정으로 도는 상시 E2E

배포 뒤 스모크(E-50·E-51)와 같은 서버·같은 시험 채널·같은 로그인 계정을 쓴다. 시험용 토큰을 따로 공급하지 않는다.
서버 설정 파일은 읽지 않는다. head·boot·Herdr 채널은 실행 중인 서버의 `/api/health/detail`이 알려 준 값만 쓴다.

## 구성

| 조각 | 하는 일 |
| --- | --- |
| `scripts/e2e/continuous/release_host.py tick` | launchd가 5분마다 깨운다. off면 서버를 관측하지 않고, 아직 켜짐으로 보이는 status만 꺼짐으로 바꾼다. on이면 서버 identity를 관측하고, 핵심 묶음 주기 요청을 넣은 뒤 runner tick을 한 번 돈다 |
| `scripts/e2e/continuous/smoke_driver.py` | runner가 부르는 driver. 스모크와 같은 preflight·판정을 쓰고, 하네스를 자기 프로세스 안에서 돌린 뒤 schema2 보고를 낸다 |
| `scripts/e2e/continuous/turn_gate.py` | 하네스의 모든 입력 직전에 불린다. 앞 턴을 실제 사용량으로 정산한 뒤에만 다음 턴을 예약한다 |
| `scripts/e2e/continuous/usage.py` | 시험 셀 세션의 전사에서 한 턴이 덧붙인 바이트만 읽어 토큰을 센다. 입력과 끝난 응답의 사용량이 모두 있어야 값이 된다 |
| `scripts/e2e/continuous/after_deploy.sh` | `deploy-release.sh`가 스모크 뒤에 부른다. 배포 SHA 요청을 inbox에 넣고 tick job을 올린다. 전체가 20초 안에 끝나지 않으면 그 단계를 끊고 배포를 계속한다 |
| `scripts/launchd/com.agentdesk.e2e-continuous.plist` | tick job 틀. 실행 주기가 아니라 깨우는 주기다 |

기기 전용 설정은 `~/.adk/e2e-continuous/config.json`(0600, 디렉터리 0700)이고 상태는 그 옆 `state/`다.
이 파일이 없으면 배포 스크립트의 호출은 아무 일도 하지 않는다. 끄기는 이 파일의 `enabled=false` 한 곳이다.
끄면 다음 tick(또는 다음 배포)이 `status.json`을 꺼짐으로 바꿔 소비자가 `DISABLED`·`enabled=0`·`pending_alerts=0`을 본다.
outbox 원장과 tick job은 그대로 둔다. 끌 때 돌던 실행이 끝나며 켜짐으로 다시 쓰면 그다음 tick이 바로잡는다.
한 번도 켠 적이 없어 status가 없는 기기에는 아무것도 쓰지 않는다.
채널 ID·bot ID·경로만 담고 비밀값은 담지 않는다.

```json
{"enabled": true, "core_interval_s": 43200, "tick_interval_s": 300, "max_run_s": 600, "tick_grace_s": 300,
 "state_dir": "<home>/.adk/e2e-continuous/state",
 "resource": {"guild": "<id>", "parents": ["<claude 채널>", "<codex 채널>"], "bot_set": ["<bot id>"]},
 "driver": ["/usr/bin/env", "python3", "-m", "scripts.e2e.continuous.smoke_driver", "--config", "<이 파일>"],
 "capabilities": ["usage:claude", "turn_budget:claude", "usage:codex", "turn_budget:codex", "E-51/herdr"],
 "reservation": {"P": {"claude": {"turns": 3, "tokens": 300000, "time_s": 150}}},
 "budget": {"claude": {"daily_turns": 36, "daily_tokens": 2880000, "daily_time_s": 1800}},
 "occurrence_contracts": {"E-50": {"busy_basis": null, "placement": null, "operation": "clear_and_turns", "occurrence": 1}},
 "required_assertions": {"E-50": ["turns_delivered_once"]},
 "release_host": {"base_url": "http://127.0.0.1:8791", "binary": "<release>/bin/agentdesk",
                  "runtime_root": "<release>/runtime", "server_log": "<release>/logs/dcserver.stdout.log",
                  "deploy_lock": "<release>/runtime/deploy-release.lock",
                  "cells": {"claude-herdr": {"channel_id": "<id>", "deadline_s": 120, "turn_tokens": 100000},
                            "codex-herdr": {"channel_id": "<id>", "deadline_s": 150, "turn_tokens": 40000}}}}
```

`cells`에 없는 칸은 켜지 않은 칸이다. driver는 입력을 보내지 않고 `UNEVALUABLE`/`CELL_NOT_ENABLED`를 낸다.
칸 이름은 manifest의 칸이다. `usage_wait_s`(기본 20)는 한 턴의 사용량 기록을 기다리는 한도다.

E-51은 Codex/Herdr 칸이다. 운영 Codex 채널이 모두 Herdr로 돌기 때문이다(근거는 [묶음별 실측](measurements.md)).
manifest의 `codex/herdr` 공백은 회전 묶음을 계속 막고, `E-51/herdr` capability가 이 시나리오 하나만 연다.

## 주기

`rotation_interval_s`와 `daily_full_at`은 null 그대로다(R1–R5와 F는 실측 n=0).
`core_interval_s`는 핵심 묶음 P를 다시 돌리는 주기다. supervisor는 현재 배포 head의 P 결과가 이 주기보다
오래됐거나 없을 때 `core-<head 12자>-<주기 칸>` 요청 하나를 inbox에 넣는다. 같은 head·같은 칸은 요청 ID가 같아
한 번만 돈다. 그 head의 배포 요청이 이미 기다리고 있으면 넣지 않는다. status의 `rotation_interval_s`에는
켜진 주기(회전이 null이면 핵심 묶음 주기)를 실어, 소비자가 `주기 + max_run_s + tick_grace_s`로 결과 만료를 판정한다.

배포 lock이 잡혀 있으면 배포 스모크가 같은 셀을 쓰는 중일 수 있으므로 실행하지 않고 heartbeat만 갱신한다(`DEFERRED_DEPLOY`).
배포 중이 아닌데 서버를 관측할 수 없으면 실행도 heartbeat도 하지 않는다(`HOST_UNAVAILABLE`). tick 두 번 뒤 소비자가
`HEARTBEAT_STALE`로 보여 주므로, 죽은 서버 위에 옛 결과가 그대로 남지 않는다.

## driver 계약

- 채널 lease를 상속받지 못했거나, 켜지 않은 칸이거나, 셀이 idle이 아니면 입력 없이 거절한다(턴 0, 토큰 0).
- **idle 증명·하네스·판정은 서버가 실제로 그 채널을 돌리는 host를 따른다.** 서버가 Herdr로 받는 채널이면 하네스 셀은
  `<provider>-herdr`이고, preflight는 `/api/health/detail`의 Herdr 블록과 릴리스 바이너리의 `herdr status`로 입력 hold가
  0인지 본다. 판정도 Herdr detail·CLI 증거가 있어야 PASS다. 관측한 host가 계획한 칸과 다르면 `HOST_MISMATCH`,
  Herdr endpoint가 하나로 정해지지 않으면 `HERDR_ENDPOINT_AMBIGUOUS`로 입력 전에 거절한다.
  기본 프로필의 E-50·E-51은 HTTP 200과 복구 완료가 확인되면 `degraded` 상태에서도 다음 검사로
  진행한다. 모든 사유가 같은 응답에서 시험 채널 범위 밖으로 귀속되고 허용 목록과 일치해야 한다.
  허용 목록은 turn presence unknown, relay verdict unknown, reader retention stalled, O binding
  missing과 Claude·Codex 각각 깊이 1의 pending queue다. 응답에서 시험 채널의 자식으로 확인된
  thread도 시험 범위에 들어간다. 큐는 대상 mailbox의 깊이 0, mailbox 행의 유일성, 제공자별 합과
  양수인 큐 전부의 범위가 확인돼야 한다. mailbox 읽기 실패도 입력 전에 거절한다.
  Herdr의 hold·CLI 검사와 대상 mailbox의 활성 상태 검사는 별도로 적용한다.
  사유가 실제 busy 또는 준비 조건 거절이면 `CELL_NOT_IDLE`, health·CLI 관측에 실패하면
  `SERVER_UNOBSERVABLE`로 기록한다. 두 경우 모두 `UNEVALUABLE`이다.
- **하네스는 driver 프로세스 안에서 돈다.** 채널 lease를 가진 프로세스가 곧 입력을 보내는 프로세스다. driver가 죽으면
  입력도 같이 끝나고, lease가 풀린 뒤 남아서 보내는 프로세스가 없다.
- **턴마다 예산을 확인한다.** 하네스의 모든 채널 입력은 보내기 직전에 gate를 지난다(`tui_relay/discord.py`의 `input_gate`).
  gate는 앞 턴을 실제 사용량으로 정산한 뒤에만 `TurnGuard.start(turn_tokens)`로 다음 턴을 예약한다.
  앞 턴의 사용량을 `usage_wait_s` 안에 못 읽거나, 턴당 상한을 넘었거나, 예산·기한이 모자라거나, 계획에 없는 프롬프트면
  그 뒤 입력은 0이다(`TURN_USAGE_UNKNOWN`·`TURN_OVER_BUDGET`·`TURN_BUDGET_EXHAUSTED`·`UNPLANNED_PROVIDER_INPUT`).
  `!clear` 같은 text command는 턴을 예약하지 않지만 앞 턴 정산과 실행 기한은 똑같이 거친다.
  전사 조회가 느릴 수 있어 예산 확인을 보내기 직전의 마지막 단계로 둔다.
  시작 전에는 예약이 시나리오 전체(프롬프트 수 × 턴당 상한, `deadline_s`)를 담는지도 본다.
- **한 턴의 사용량은 입력과 끝난 응답이 모두 전사에 있을 때만 값이다.** Claude는 그 턴 입력 뒤 `end_turn` 응답의 usage,
  Codex는 그 턴(`turn_id`)의 `token_usage_record`가 마지막 모델 출력 뒤까지 있고, 각 응답의 사용량 합이 턴 누계와 같고,
  `task_complete`가 있어야 한다. Claude도 도구 결과에 짝이 되는 도구 호출 메시지가 빠지면 unknown이다.
  필수 counter가 빠지면 0이 아니라 unknown이다.
  한 턴이라도 unknown이면 실행 전체의 턴·토큰을 unknown으로 보고한다. runner는 예약을 그대로 두고 그 provider를 보류한다.
  이번 턴의 입력이 없는 옛 세션 전사는 0으로 센다.
- 마지막 턴은 시나리오가 끝난 뒤 정산한다. 그 턴이 상한을 넘거나 사용량이 없으면 스모크가 통과했어도 `UNEVALUABLE`이다.
  예산 gate가 실제로 입력을 막은 경우만 빠진 턴을 `UNEVALUABLE`로 완화하고, 모든 입력이 나간 뒤 본 전달 실패는 `FAIL`이다.
- 실행 뒤에도 host를 다시 읽어 `actual_occurrence_key`에 싣는다. 실행 중 바뀌면 계획 key와 어긋나 PASS가 되지 않는다.
- 실행 중 서버 head·boot·binary가 바뀌면 `SERVER_CHANGED_DURING_RUN`이다.
- 서버 로그 발췌는 판정 계수만 읽고 지운다. 그 구간에는 다른 채널의 로그도 들어 있기 때문이다.

## 관측 불가와 실패

스모크 판정이 FAIL이어도 원인이 관측 실패면 `UNEVALUABLE`이다. 기능 실패로 세지 않는다.

| 관측 | 분류 |
| --- | --- |
| 조회 전송 실패(`TimeoutError`, URL error, HTTP 5xx, 재시도 소진)로 시나리오가 끊김 | `UNEVALUABLE` / `UNOBSERVED_TRANSPORT` |
| 시나리오는 통과했는데 서버 로그가 교체돼 발췌가 없음 | `UNEVALUABLE` / `UNOBSERVED_SERVER_LOG` |
| 같은 본문이 두 번 이상 전달된 것을 봄(위 둘과 겹쳐도) | `FAIL` |
| 전송 오류 없이 본문이 기한 안에 오지 않음 | `FAIL` |
| 예산 gate가 남은 입력을 막아 시나리오가 끝까지 가지 못함(중복 전달·입력 거절을 본 경우는 `FAIL`) | `UNEVALUABLE` / gate 사유 |

하네스 자체의 재시도 수리는 #7335 범위다.

## 실패 처분

알리기만 한다. 배포를 멈추거나 되돌리지 않는다. 비PASS 결과는 outbox에 남고 `E2E_MONITOR pending_alerts`로 보인다.
operator에게 직접 보내는 전송은 아직 없다.
