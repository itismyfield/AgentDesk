# Herdr host runbook (P9)

Herdr 로 띄운 Claude 채널(P9 대상: adk-dash-cc)의 운영 절차다. 모든 절차는 pane 을 죽이거나 닫지 않는다.
pane kill·close 는 P11 범위다.

## 1. 전용 서버

- launchd user agent `com.agentdesk.herdr` 가 전용 herdr 서버를 띄운다. 정규 config 파일은 0444 로 둔다.
- 금지:
  - `herdr server reload-config`
  - 정규 config 파일 수정
  - default socket 사용
- AgentDesk 쪽 설정은 `session_hosts.herdr`(endpoint·channel)이다. 설정을 바꾸면 dcserver 재시작이 필요하다.
  health 의 `herdr.restart_required` 가 `true` 이면 아직 반영되지 않은 것이다.

## 2. 확인

- 전용 env 로 `herdr status server --json` 을 실행해 버전 0.9.3 을 확인한다.
- AgentDesk health(`/api/health`) 의 `herdr` 블록:
  - `endpoints.<key>.local = true`: 이 노드의 endpoint 다.
  - `admission = "open"`: 새 Herdr 작업을 받는다.
  - `reconnect`: 가장 최근 재시작 재접속 패스의 결과다. 이 노드에 local endpoint 가 있을 때만 나온다.
    - `channels`: 읽은 row 수.
    - `published`: source 를 복원했거나, 그 실행 자신의 clear 가 대기 중이라 입력을 허용한 row.
    - `withheld`: 읽은 결과로 거절한 row(RootReplaced·ProviderReplaced·OtherNonce·pane 없음·기준선 없음).
    - `unknown`: 읽지 못한 row. 다음 패스에서 다시 읽는다.
    - `pending`: 아직 Bound 가 아닌 row. 재접속 패스는 손대지 않고 다음 턴에 맡긴다.
  - `input_holds`: 불명확한 prompt 뒤 입력을 막고 있는 실행 수.
- `agentdesk herdr status`: 이 노드의 Herdr row 를 읽기 전용으로 보여 준다.
  - row 마다 채널·provider·state·nonce·pane 상태를 보여 준다.
  - pane 상태 값: `provider_running`, `provider_exited`, `root_replaced`, `provider_unverified`, `missing`, `unreadable`, `no_local_endpoint`.
  - 그 실행의 input hold 가 있으면 기록 시각을 함께 보여 준다.
  - 어느 row 에도 속하지 않는 hold 는 `other_input_holds` 에 nonce·기록 시각으로 나온다.
  - 경로와 입력 원문은 출력하지 않는다.

## 3. 비상 정지

- `touch <runtime_root>/herdr/admission-off` 로 새 Herdr 작업을 멈춘다. 재시작은 필요 없고, 한 번 보이면 프로세스가 끝날 때까지 유지된다.
- 프로세스 단위로 끄려면 `ADK_HERDR_ADMISSION=off` 로 dcserver 를 띄운다. `on` 또는 미설정만 허용이다.
- 재개: 파일을 지우고 dcserver 를 재시작한다.
- Claude 턴 실행 스위치는 `runtime.herdr_turn_enabled` 이다. 미설정·false 면 pane I/O 전에 거절한다.

## 4. 재시작 뒤

- dcserver 만 재시작한 경우(서버·pane 그대로):
  - rehydrate 패스가 이 노드의 Bound row 를 저장된 pane 하나씩 읽는다.
  - root shell·provider·nonce 가 기록과 같을 때만 source 를 복원한다. 다음 턴은 prompt 를 정확히 한 번 보낸다.
- 전용 herdr 서버가 재시작된 경우:
  - pane id 가 같아도 root shell 이 바뀌어 `withheld` 가 된다.
  - 입력·채택·재기동은 하지 않는다. 그 채널의 턴은 거절된다. 5절로 정리한다.
- clear 직후 재시작한 경우:
  - 그 실행 자신의 SessionStart(clear) Pending 이 마지막 기록이면 입력을 허용한다.
  - 다른 실행의 Pending 은 거절한다.

## 5. 끝난 실행 은퇴(`agentdesk herdr retire <channel>`)

- tmux 로 되돌리려면 row 가 Retired 여야 한다. retire 는 pane 을 읽기만 한다. kill·close·입력은 하지 않는다.
- 운영자가 먼저 실행을 끝낸다:
  - herdr 에서 pane 을 직접 닫는다(사람 조작), 또는
  - provider 를 `/exit` 로 끝낸다.
- retire 가 허용되는 경우:
  - 완전한 snapshot 에 저장된 pane 이 없다.
  - root shell 만 foreground 에 있고, 그 root shell 이 기록된 것(pid·시작 시각)과 같다.
- 그때만 row 를 nonce CAS 로 Retired 로 바꾼다. CAS 가 성공한 뒤에만 그 nonce 의 input hold 를 지운다.
  다른 nonce 의 hold 는 건드리지 않는다.
- 거절 이유(row·hold 는 그대로):
  - `ProviderRunning`: provider 가 아직 돈다.
  - `Unproven(..)`: 읽기 실패, 불완전 snapshot, 바뀐 root shell, 확인되지 않은 provider.
  - `Changed(..)`: 읽은 뒤 row 가 바뀌었거나 전이가 거절됐다. hold 는 남는다.
  - `NoRow(..)`: 이 노드에 그 채널의 살아 있는 Herdr row 가 하나가 아니다. 이미 Retired 인 row 에 다시 실행해도 이 거절로 끝나고 아무것도 바꾸지 않는다.
  - `NoLocalEndpoint`: 이 노드에 등록된 endpoint 가 없다.
- 일시적인 읽기 실패는 retire 근거가 아니다. 증명하지 못하면 row 는 그대로(Hold)이고, 이 상태를 "tmux 원복 완료" 로 보지 않는다.
- input hold 만 따로 지우는 명령은 없다.

## 6. 되돌리기(pane kill 0)

1. admission off(3절 파일).
2. drain: 진행 턴이 끝나고 O spool 이 비며 health 의 미전달이 0 인지 확인한다.
3. 채널 제거: 양 노드 yaml 의 `session_hosts.herdr.channels` 에서 채널을 빼고 재시작한다.
4. 원복: 5절 retire 로 row 를 Retired 로 만든 뒤에만 tmux 재기동이 된다. 그 전에는 그 채널의 턴이 typed 거절 상태다.
