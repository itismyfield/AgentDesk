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
    - 가장 최근 재접속 패스가 센 값이다. health 요청은 파일을 읽지 않는다.
    - 첫 패스 전에는 `"not_counted_yet"` 이다.
- `agentdesk herdr status`: 이 노드의 Herdr row 를 읽기 전용으로 보여 준다.
  - row 마다 채널·provider·state·nonce·launch 증거(`recorded`/`none`)·pane 상태를 보여 준다.
  - pane 상태 값:
    - `provider_running`, `provider_exited`, `root_replaced`, `provider_unverified`, `missing`, `unreadable`.
    - `root_shell_unrecorded`: launch 증거가 없는 실행의 pane 에 shell 만 있다.
    - `no_location`: 아직 pane 이 기록되지 않은 Pending 이다.
    - `no_local_endpoint`: 기록된 endpoint 가 이 노드에 등록되어 있지 않다.
  - 그 실행의 input hold 가 있으면 기록 시각을 함께 보여 준다.
  - Retired row 인데 그 nonce 의 hold 가 남아 있으면 그 row 도 `state: retired` 로 나온다.
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
  - 완전한 snapshot 에 저장된 pane 이 없다. launch 증거가 없는 Pending 도 pane 이 기록되어 있으면 이 조건으로 은퇴한다.
  - root shell 만 foreground 에 있고, 그 root shell 이 기록된 것(pid·시작 시각)과 같다. launch 증거가 없으면 비교할 기록이 없어 이 조건으로는 은퇴하지 않는다. pane 을 닫은 뒤 다시 실행한다.
- 그때만 row 를 nonce CAS 로 Retired 로 바꾼다. CAS 가 성공한 뒤에만 그 nonce 의 input hold 를 지운다.
  다른 nonce 의 hold 는 건드리지 않는다.
- 출력은 row 결과와 hold 결과를 따로 적는다:
  - `retired` / `was already retired`: row 결과.
  - `no input hold is left`: hold 가 없거나 지워졌다.
  - `removed, but the removal is not confirmed durable`: 파일은 지웠으나 디렉터리 동기화가 실패했다. 장애 뒤 다시 보일 수 있다.
  - `remains`: hold 가 남았다. status 에 그 Retired row 가 hold 와 함께 나온다. 원인을 고친 뒤 같은 retire 를 다시 실행하면 그 nonce 의 hold 만 지운다(row 는 이미 Retired 이고 바꾸지 않는다).
- 거절 이유(row·hold 는 그대로):
  - `ProviderRunning`: provider 가 아직 돈다.
  - `Unproven(..)`: 읽기 실패, 불완전 snapshot, 바뀐 root shell, 기록 없는 root shell(`no recorded root shell`), 확인되지 않은 provider, 기록된 pane 없음(`no recorded pane`).
  - `Changed(..)`: 읽은 뒤 row 가 바뀌었거나 전이가 거절됐다. hold 는 남는다.
  - `NoRow(..)`: 이 노드에 그 채널의 살아 있는 Herdr row 가 하나가 아니고, 지울 hold 가 남은 Retired row 도 하나가 아니다. 아무것도 바꾸지 않는다.
  - `NoLocalEndpoint`: 이 노드에 등록된 endpoint 가 없다.
- 일시적인 읽기 실패는 retire 근거가 아니다. 증명하지 못하면 row 는 그대로(Hold)이고, 이 상태를 "tmux 원복 완료" 로 보지 않는다.
- input hold 만 따로 지우는 명령은 없다.

## 6. 되돌리기(pane kill 0)

1. admission off(3절 파일).
2. drain: 진행 턴이 끝나고 O spool 이 비며 health 의 미전달이 0 인지 확인한다.
3. 채널 제거: 양 노드 yaml 의 `session_hosts.herdr.channels` 에서 채널을 빼고 재시작한다.
4. 원복: 5절 retire 로 row 를 Retired 로 만든 뒤에만 tmux 재기동이 된다. 그 전에는 그 채널의 턴이 typed 거절 상태다.

## 7. 위임 home(`agentdesk channel-home`)

- 지금은 휴면이다. lease task 와 drain 드라이버를 기동하는 운영 경로가 없다. 위임 row 가 생겨도 이 빌드는 그 row 를 넘기거나 받지 않는다.
- drain 이 읽는 운영 포트(턴·미전달 책임·진행 중 POST)는 있지만 만드는 운영 경로가 없다. gateway 쪽 Legacy reset 은 아직 연결되지 않아 releasing drain 은 `source_reset` 에서 기다린다.
- `status`: 읽기 전용이다.
  - row 마다 state·holder·target·epoch·`renewed_at` 을 보여 준다.
  - `open_intake` 는 그 채널의 열린 intake 를 각인된 home epoch 로 나눈다.
    - `current_epoch`: row 의 현재 epoch 로 각인된 행. 현재 holder 만 claim 한다.
    - `other_epoch`: 다른 epoch 로 각인된 행. 어느 holder 도 다시 claim 하지 않는다.
    - `unrouted`: row 가 생기기 전에 만든 행. row 가 있는 동안 claim 되지 않는다.
  - intake 원문과 경로는 출력하지 않는다.
- `delegate <channel> --provider claude|codex --to <node>`, `reclaim <channel>`, `force <channel>`:
  - `runtime.channel_home_delegation_enabled` 가 true 일 때만 실행한다. 미설정·false 면 DB 에 접속하기 전에 거절한다.
  - delegate·reclaim 은 이 노드의 `cluster.instance_id` 를 gateway 로 쓴다. gateway 노드에서 실행한다.
  - 출력의 `run_on` 은 실행한 노드, `planned` 는 적은 holder·target 이다. 의도한 노드인지 확인한다.
  - provider 는 앞뒤 공백·대소문자를 무시하고 소문자로 저장한다. claude·codex 밖은 거절한다.
  - force 는 holder 의 lease 가 F(200초 = H 20초 + 조각 lease 180초)보다 오래 갱신되지 않았을 때만 row 를 `orphaned` 로 만든다. 그 뒤 아무 노드도 채널을 받지 않는다. 옛 노드의 store 를 운영자가 확인한다.
- health `channel_homes` 는 이 프로세스에 위임 home 이 등록됐을 때만 나온다. `homes` 는 home 별 상태(`intake_open`·`draining`·`lost`)이고, `home_draining` 은 drain 이 기다리는 이유(`blocker`)다.
- retry 복구 계약:
  - 자동 sweep 과 `intake-outbox force-fail` 의 재시도 행은 원래 행의 `home_epoch` 를 그대로 둔다. 같은 lifecycle 의 같은 holder 일 때만 다시 claim 된다.
  - holder 나 epoch 가 바뀐 뒤의 재시도 행은 `status` 에 `other_epoch` 로 보이고 claim 되지 않는다. 채널당 열린 route 가 1개뿐이라 그 채널의 다음 intake 도 막힌다.
  - 지금은 이 행을 현재 home 으로 다시 보내는 도구가 없다. force-fail 도 같은 epoch 로 다시 만든다. 현재 home 을 다시 읽는 재시도가 생기기 전에는 위임을 켜지 않는다.
  - drain 의 `blocker` 가 `open_intake` 에 오래 머물면 이 경우인지 `status` 로 확인한다.

## 8. 중단 안내 중복(at-least-once)

- 지금은 휴면이다. 생산 빌드의 Herdr 정산 스위치는 hard-false 라서 아래 경로는 실행되지 않는다. 활성화 뒤의 운영 기준이다.
- 보장 범위:
  - 중단 안내(`⏹ 중단됨`)는 이 순서로 처리한다: receipt 또는 admitted kind 를 row 에 durable 하게 쓴다 → 안내를 보낸다 → 보낸 결과를 같은 row 에 ACK 로 쓴다 → 그 row 만 정리한다.
  - 원래 메시지 편집이 실패하면 새 메시지로 보낸다(`SentFallbackAfterEditFailure`). 이것도 실제로 보인 안내로 ACK 한다.
  - 안내를 보낸 뒤 ACK 가 durable 해지기 전에 프로세스가 끝나면, 재시작이 Pending 을 보고 안내를 한 번 더 보낸다.
    - 편집으로 보였던 안내는 같은 메시지를 다시 편집하므로 보이는 결과가 같다.
    - fallback 으로 보였던 안내는 새 메시지가 하나 더 생길 수 있다.
  - 안내가 정확히 한 번 보인다고 보장하지 않는다. 중복 안내는 terminal 이 두 번 처리됐다는 뜻이 아니다.
- 진단: 같은 episode(채널·`turn_nonce`·generation·안내 메시지)로 다음을 대조한다.
  - 로그 `herdr_turn_policy_close`: `channel_id`·`logical`·`turn_nonce`·`runtime_generation`·`settled`.
  - 로그 `herdr_turn_held`: `hold_kind`·`hold_reason`·`settlement`.
  - 재시작 로그 `recovery kept a before-start policy receipt` 와 `recovery kept an admitted Herdr terminal` 의 `why`.
  - inflight row: receipt 의 notice 값(Pending·Delivered), `tui_terminal_kind`, `terminal_delivery_committed`.
  - 실제 Discord 채널에 보인 안내 메시지 수.
- 경보 기준(운영자 점검 기준이다. 자동 경보는 없다):
  - 같은 episode 에서 fallback 안내가 반복된다.
  - 재시작 뒤에도 receipt 가 Pending 으로 남는다.
  - receipt 가 Delivered 인데 row 가 정리되지 않고 남는다.
  - 이 빌드가 읽지 못하는 receipt 의 row 가 있다. 재시작은 그 row 를 그대로 둔다. 소비된 흔적이 보이면 조사한다.
- 수동 대응:
  1. 3절로 새 Herdr admission 을 막는다.
  2. 해당 inflight row 파일과 receipt 를 그대로 복사해 둔다.
  3. `agentdesk herdr status` 의 pane 상태와 그 채널의 mailbox(진행 턴·대기열)를 대조한다.
  4. 처리는 운영자가 판단한다. 금지: row 삭제, kind 를 Completed 로 바꾸기, 안내 임의 재전송.
- 롤백:
  - 지금 생산 정산은 hard-false 다. 끌 것이 없다.
  - 활성화 뒤 정산을 다시 끄면, 지금 코드는 receipt 나 admitted kind 가 있는 row 를 일반 복구 경로로 읽는다.
    끄기 전에 그런 row 가 남아 있는지 확인하고, 남아 있으면 원본을 보존한 뒤 운영자가 처리한다.
  - 꺼도 이런 row 를 일반 복구로 넘기지 않는 계약은 아직 없다. 활성화 전에 필요한 선행 작업이다.
