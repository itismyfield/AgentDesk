# herdr 사용자 기본 서버

`mode: user_default`는 실행 노드 사용자의 기존 herdr 서버에 연결한다. AgentDesk는 서버나 daemon을 만들거나 재시작하지 않는다. 설정을 바꾸지 않은 기존 `legacy_dedicated` endpoint의 경로와 판정은 유지된다.

## 설정 예시

```yaml
cluster:
  instance_id: worker
agents:
  - id: example
    name: example
    channels:
      claude: {id: "123456789", runtime: tui}
session_hosts:
  herdr:
    endpoints:
      personal:
        mode: user_default
        execution_node: worker
    channels:
      "123456789": personal
```

socket을 생략하면 **실행 노드**의 실행 사용자 HOME에서 `.config/herdr/herdr.sock`을 선택한다. gateway의 HOME으로 원격 경로를 채우지 않는다. 원격 endpoint의 생략된 경로는 실행 노드가 해석한다. 기본 selector는 `default`다. named/비표준 socket은 `socket_path` 절대경로와 `herdr_session`을 명시한다. `~`나 `$HOME` 문자열을 직접 넣지 않는다. `herdr_home`은 dedicated 모드의 필드이며 UserDefault config를 대신 지정하지 않는다.

config는 socket peer의 실제 HOME/XDG/config 환경과 실행 파일 identity를 확인한 뒤 선택한다. 기본 config와 다른 override, 미검증 버전, 불명확한 peer는 거부한다. 최초 지원은 herdr0.9.3/protocol22다. **현재 macOS loaded-image identity adapter는 Unverified이므로 UserDefault fresh/apply가 준비되지 않는다.** 설치 경로의 stat이나 버전 문자열로 이 판정을 풀지 않는다.

## 동의와 진단

실행 사용자의 config에서 restore resume를 false로 만드는 동의는 owner만 결정한다. 아니오·만료·오류에서도 원 입력과 업로드는 기존 custody에 남는다. `!herdr consent retry`는 새 질문 또는 미완료 복구를 요청한다. 중복 클릭·release는 동일 내구 키로 억제한다. worker 기록 전 오류는 같은 exact row의 기존 Pending 경로로만 돌아가며, 기록을 시도한 뒤의 오류는 Spawned로 보존하고 원 메시지에 row id를 알린다.

`agentdesk doctor --json`과 `/api/health/detail`의 `herdr`를 확인한다. endpoint별 `next_restart_ready`, `startup_proof`, `reason`, `action`, 제한된 checker stdout/stderr, candidate/applied mapping과 migration 진단을 구분한다. health는 마지막 관측을 읽기만 하며 현재 파일을 재검증하거나 create 권한을 주지 않는다. 마지막 fence가 실제 권한을 다시 확인한다.

현재 false가 검증돼 fresh가 준비되어도 `startup_proof=awaiting_server_restart`일 수 있다. 즉시 launch를 위해 서버를 재시작하지 않는다. true/invalid로 바뀌면 신규 create는 막히지만 같은 witness와 정확한 execution의 이미 승인된 입력·Cancel·Terminate 허가는 별도 증거로 유지된다.

## 카나리 순서

1. 코디네이터가 실행 노드·사용자·시험 채널·rollback 설정을 별도 승인한다. 이 문서는 운영 설정 변경을 승인하지 않는다.
2. 실행 노드의 사용자 서버 존재,0.9.3/protocol22, 실제 socket peer와 loaded-image identity, 경로/권한을 확인한다. pathname/version만 있는 관측은 준비 완료가 아니다. macOS에서는 위 identity 제한을 먼저 해소해야 한다.
3. 새 설정을 배포하기 전 live row와 unresolved consent hold가 이동 대상에 없는지 확인한다. migration 거부와 restart_required를 구별하고 old applied mapping이 유지되는지 확인한다.
4. owner가 동의 카드의 경로·preview를 확인한 뒤 예를 선택한다. 원본 backup과 false 전체 config 검증을 확인한다. 첫 입력과 다음 FIFO 입력을 확인하고 draft/modal/busy 상태에서 paste/Enter가 없는지 확인한다.
   실제 Claude cold start 한 번에서 SessionStart hook 수신부터 빈 ready composer marker 관측까지의 지연을 기록한다.
5. 실패 시 원 입력을 지우거나 서버를 재시작하지 않는다. `reason/action` 및 row id를 보존한다. 정리되지 않은 참조는 clear 성공으로 가정하지 않는다.

E 구현 레인의 읽기 전용 전제 조사는 보고서에 남긴다. 해당 관측에서 사용자 기본 socket은 있었지만 `~/.local/bin/herdr`는 없었다. Homebrew 설치 경로의0.9.3 문자열은 peer 증명이 아니다. 실제 사용자 서버나 운영 설정을 시험 대상으로 사용하지 않았다. T19는 고정 SHA 사본과 fake provider만 사용하는 별도 scratch에서 수행한다.

드문 drain 중 release 창과 startup queue merge 재시도 뒤 재생은 후속 #7261이다. 이 후속 상태를 자동 성공으로 처리하지 않는다.
