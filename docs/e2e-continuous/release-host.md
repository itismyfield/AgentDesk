# 릴리스 호스트 프로필: 기존 계정으로 도는 상시 E2E

배포 뒤 스모크(E-50·E-51)와 같은 서버·같은 시험 채널·같은 로그인 계정을 쓴다. 시험용 토큰을 따로 공급하지 않는다.
서버 설정 파일은 읽지 않는다. head·boot·Herdr 채널은 실행 중인 서버의 `/api/health/detail`이 알려 준 값만 쓴다.

## 구성

| 조각 | 하는 일 |
| --- | --- |
| `scripts/e2e/continuous/release_host.py tick` | launchd가 5분마다 깨운다. off면 아무것도 읽지 않고 끝난다. 서버 identity를 관측하고, 핵심 묶음 주기 요청을 넣은 뒤 runner tick을 한 번 돈다 |
| `scripts/e2e/continuous/smoke_driver.py` | runner가 부르는 driver. 스모크와 같은 preflight·하네스·판정을 돌리고 schema2 보고를 낸다 |
| `scripts/e2e/continuous/usage.py` | 시험 셀 세션의 전사에서 그 실행이 덧붙인 바이트만 읽어 토큰을 센다 |
| `scripts/e2e/continuous/after_deploy.sh` | `deploy-release.sh`가 스모크 뒤에 부른다. 배포 SHA 요청을 inbox에 넣고 tick job을 올린다 |
| `scripts/launchd/com.agentdesk.e2e-continuous.plist` | tick job 틀. 실행 주기가 아니라 깨우는 주기다 |

기기 전용 설정은 `~/.adk/e2e-continuous/config.json`(0600, 디렉터리 0700)이고 상태는 그 옆 `state/`다.
이 파일이 없으면 배포 스크립트의 호출은 아무 일도 하지 않는다. 끄기는 이 파일의 `enabled=false` 한 곳이다.
채널 ID·bot ID·경로만 담고 비밀값은 담지 않는다.

```json
{"enabled": true, "core_interval_s": 43200, "tick_interval_s": 300, "max_run_s": 600, "tick_grace_s": 300,
 "state_dir": "<home>/.adk/e2e-continuous/state",
 "resource": {"guild": "<id>", "parents": ["<claude 채널>", "<codex 채널>"], "bot_set": ["<bot id>"]},
 "driver": ["/usr/bin/env", "python3", "-m", "scripts.e2e.continuous.smoke_driver", "--config", "<이 파일>"],
 "capabilities": ["usage:claude", "turn_budget:claude", "usage:codex", "turn_budget:codex"],
 "reservation": {"P": {"claude": {"turns": 3, "tokens": 240000, "time_s": 150}}},
 "budget": {"claude": {"daily_turns": 36, "daily_tokens": 2880000, "daily_time_s": 1800}},
 "occurrence_contracts": {"E-50": {"busy_basis": null, "placement": null, "operation": "clear_and_turns", "occurrence": 1}},
 "required_assertions": {"E-50": ["turns_delivered_once"]},
 "release_host": {"base_url": "http://127.0.0.1:8791", "binary": "<release>/bin/agentdesk",
                  "runtime_root": "<release>/runtime", "server_log": "<release>/logs/dcserver.stdout.log",
                  "deploy_lock": "<release>/runtime/deploy-release.lock",
                  "cells": {"claude-herdr": {"smoke_cell": "claude-tui", "channel_id": "<id>", "deadline_s": 120, "turn_tokens": 80000}}}}
```

`cells`에 없는 칸은 켜지 않은 칸이다. driver는 입력을 보내지 않고 `UNEVALUABLE`/`CELL_NOT_ENABLED`를 낸다.

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
- **턴 예산은 시나리오 단위로 미리 확인한다.** 하네스가 프롬프트를 직접 보내므로 driver는 턴 사이에 끼어들지 못한다.
  그래서 시나리오의 프롬프트 수만큼 `TurnGuard.start(turn_tokens)`가 모두 통과하고 남은 시간이 `deadline_s` 이상일 때만
  첫 입력을 보낸다. 하나라도 모자라면 시나리오 전체를 시작하지 않는다. 턴마다 다시 확인하는 runner 계약보다 거칠다.
  실행 뒤 실제 턴·토큰이 예약을 넘으면 runner가 `UNEVALUABLE`로 내리고 그 provider를 보류한다.
- native 턴 수는 그 실행의 marker가 들어 있는 native 입력 레코드 수다. 토큰은 Claude는 API message ID로 중복을 없앤
  합계, Codex는 세션 누적값의 실행 전후 차이다. 전달된 프롬프트의 입력 레코드를 못 찾으면 사용량은 unknown이다.
- host는 서버가 Herdr로 받는 채널 목록에서 읽는다. 계획한 칸의 host와 다르면 `actual_occurrence_key`가 어긋나
  PASS가 되지 않는다.
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

하네스 자체의 재시도 수리는 #7335 범위다.

## 실패 처분

알리기만 한다. 배포를 멈추거나 되돌리지 않는다. 비PASS 결과는 outbox에 남고 `E2E_MONITOR pending_alerts`로 보인다.
operator에게 직접 보내는 전송은 아직 없다.
