# Herdr 운영 runbook — #5340

기준 커밋: `b3006723b4f0cd9d850b9a479f2f1f3a8b582166`.

이 문서의 모든 `경로:줄` 좌표는 위 커밋을 가리킨다. 코드 동작과 운영 정책을 구분한다. `[O-U-n]`은 추가 확인이 필요한 항목이며, §8.2의 방법과 완료 증거를 충족하기 전에는 완료로 기록하지 않는다.

문서 작성은 지정 커밋의 정적 검토로만 수행했다. 파일·브랜치를 변경하지 않았고 cargo·시험·네트워크를 사용하지 않았다. `agentdesk.yaml`, `launchd.env`, 런타임 디렉터리의 값도 읽거나 기록하지 않았다. 아래 운영 명령과 검증 절차는 실행하지 않았다.

실제 설정값·인증정보·이메일·호스트 IP는 이 문서에 기록하지 않는다. `<ADK>`는 대상 노드·runtime에 맞는 AgentDesk 바이너리, `<HERDR_BIN>`은 고정한 Herdr 바이너리다. 나머지 꺾쇠 표기는 운영자가 확인할 자리표시자다.

## 1. 현재 구현과 전환 한계

### 1.1 실행 선택

`session_hosts.herdr.channels`는 채널 ID를 endpoint key에 연결한다. Endpoint에는 `execution_node`, `socket_path`, `herdr_home`, `herdr_session`이 필요하다. Endpoint 경로는 절대경로여야 하며, 채널은 등록된 Claude 또는 Codex TUI 채널이어야 한다. 이 설정은 부팅 시 고정된다.

근거: `src/config/session_hosts.rs:33`, `:49`, `:165`, `:191`, `:225`, `:252`.

이 노드는 `execution_node`가 자신의 `cluster.instance_id`와 같은 endpoint만 등록한다. 다른 노드의 endpoint를 지정한 턴은 `endpoint_not_local(...)`로 거절된다. Adapter가 원격 Unix socket에 직접 연결하거나 다른 노드에서 대신 실행한다고 해석하지 않는다.

근거: `src/services/session_host/herdr_registry.rs:143`; `src/services/turn_host.rs:277`.

채널 매핑 제거와 실행 은퇴는 별개다. 매핑이 없어도 읽힌 hosted record가 Pending·Bound 또는 해석 불가능한 Herdr record이면 기존 실행 경로로 넘기지 않는다. Retired record는 턴 선택에서 tmux 경로를 허용하지만, 관리용 row witness는 Retired도 Herdr로 취급한다.

정상 Herdr launch는 실행의 `.host_kind=herdr` 표식을 기록한다. retire는 표식을 legacy로 전환하지 않는다. Claude TUI 실행기는 표식이 Herdr이면 tmux 조회·cleanup·launch 전에 거절한다. 따라서 **retire와 매핑 제거만으로 같은 Claude 채널의 legacy 복귀가 완료되지 않는다.** 자세한 제한은 §3을 따른다.

근거: `src/services/turn_host.rs:91`, `:140`; `src/services/session_host/session_record.rs:8`; `src/services/herdr_launch.rs:346`, `:569`; `src/cli/herdr.rs:261`, `:287`; `src/services/claude.rs:1776`; `src/services/claude/host_gate.rs:20`.

### 1.2 설정·스위치·필수 준비

| 항목 | 미설정 또는 미충족 시 동작 | 적용 및 의미 |
|---|---|---|
| `session_hosts.herdr.endpoints`, `.channels` | 빈 map | 부팅 고정. 변경 반영에는 dcserver 재시작 필요 |
| `runtime.herdr_turn_enabled` | false | Claude Herdr 턴 실행 허용. live 설정을 읽음 |
| `runtime.herdr_codex_turn_enabled` | false | Codex Herdr 턴 실행 허용. 꺼져 있으면 unsupported provider로 거절 |
| `runtime.herdr_cancel_enabled` | false | 요청 스위치. 현재 생산 빌드에서는 true여도 Escape 전달 불가 |
| `runtime.herdr_terminate_enabled` | false | 명시적 운영자 pane 종료 허용. live 설정을 읽음 |
| `runtime.channel_home_delegation_enabled` | false | channel-home 변경 CLI와 부팅 시 위임 경로 시작을 제어 |
| O writer 채널 선택 | 자동 준비를 보장하지 않음 | `tui_o.writer.channels` 또는 `all_tui`로 선택하는 부팅 설정. 실행 노드별 확인 필요 |
| 대상 채널의 O 소유권·accepting writer·기존 binding checkpoint | 하나라도 없으면 `o_writer_not_ready` | 일반 Herdr 턴의 필수 준비 조건. 채널 선택만으로 충족되지 않음 |
| Codex cold-launch hook·daemon isolation 조건 | `HooksUnavailable` 또는 `DaemonIsolationUnavailable` 등으로 거절 | hook 활성화·endpoint·바이너리 hook capability와 `--no-daemon` 지원을 별도로 확인 |
| `ADK_HERDR_ADMISSION` | 미설정이면 허용 | 최초 사용 시 읽음. 설정된 값 중 `on`만 허용하며 나머지는 거절 |
| `<runtime_root>/herdr/admission-off` | 파일이 없으면 허용 | 발견하면 해당 프로세스 종료까지 정지 상태를 유지 |

근거: `src/config/session_hosts.rs:33`, `:252`; `src/config/runtime_settings.rs:121`; `src/services/turn_host.rs:199`, `:287`, `:288`; `src/services/herdr_launch.rs:62`, `:72`; `src/services/tui_o/channel_policy.rs:1`, `:62`; `src/services/codex/tui_session_launch.rs:416`, `:427`, `:433`, `:436`; `src/services/provider/cancel_token_claude_interrupt.rs:581`, `:598`; `src/services/discord/herdr_terminate.rs:69`; `src/cli/channel_home.rs:70`; `src/services/cluster/channel_home_boot.rs:95`; `src/services/herdr_admission.rs:38`, `:48`, `:70`, `:78`.

admission 정지는 새 Herdr 작업을 막으며 pane을 종료하지 않는다. 일반 턴 선택과 입력 gate가 모두 admission을 검사하므로 기존 실행의 후속 입력과 시험 입력도 영향을 받는다. 이를 범용 intake pause나 대기 입력의 안전한 동결·이관 기능으로 취급하지 않는다.

admission은 해당 프로세스의 공통 스위치다. 특정 채널만 멈추려는 작업에서는 같은 runtime의 다른 Herdr 채널에 미치는 영향도 확인한다. 취소 키는 입력 admission 예외지만 현재 생산 빌드의 Herdr stop settlement는 hard-false다.

근거: `src/services/herdr_admission.rs:1`, `:76`; `src/services/turn_host.rs:287`; `src/services/session_host/herdr_gate.rs:70`, `:635`; `src/services/provider/cancel_token_claude_interrupt.rs:581`, `:598`.

## 2. 실행 노드별 준비

각 실행 노드에서 다음 준비를 따로 완료한다. 파일·프로젝트·환경·MCP·인증정보의 자동 복제는 #5340 범위 밖이다. 다른 노드에 준비되어 있다는 이유로 이 노드의 준비를 생략하지 않는다.

### 2.1 Herdr 바이너리와 서버 설정

운영 정책:

- 검증한 서버 바이너리를 버전별 경로에 고정하고 배포물 식별값을 운영 기록에 남긴다.
- 클라이언트 업데이트와 실행 서버 바이너리 교체를 별도 작업으로 관리한다.
- 무인 서버 교체·자동 재시작을 허용하지 않는다.
- 관리 대상 서버를 사용자 개인 Herdr 환경과 분리한다. named session 이름만으로 설정까지 분리된다고 가정하지 않는다.
- 운영 중 정규 config 수정이나 `reload-config`를 복구 수단으로 사용하지 않는다.

현재 adapter의 protocol은 22이며 restore 설정 검증인 E7이 허용하는 서버 버전은 **0.9.3 하나**다. 일반 ping handshake는 버전 문자열 일치를 요구하지 않지만 E7은 별도의 검증 버전 목록을 적용한다. 새 버전의 protocol이 같다는 이유만으로 운영 입력 호환성을 승인하지 않는다.

근거: `src/services/session_host/herdr/model.rs:11`, `:15`; `src/services/session_host/herdr/observe.rs:117`, `:153`.

관리 서버의 canonical config는 다음 바이트열이다. 마지막 줄의 줄바꿈도 포함한다.

```toml
[session]
resume_agents_on_restore = false
```

E7은 `<herdr_home>/config.toml`의 내용, 서버 프로세스의 `HERDR_CONFIG_PATH`, `XDG_CONFIG_HOME=<herdr_home>/xdg`, 파일 수정 시각과 서버 시작 시각, socket peer의 PID·시작 식별을 함께 확인한다. config는 서버 시작보다 1초 넘게 앞서 작성되어 있어야 한다. 파일을 시작 후 다시 쓰거나 내용을 추가하면 검증을 통과한다고 가정하지 않는다.

근거: `src/services/session_host/herdr/observe.rs:20`, `:88`, `:103`, `:127`, `:134`.

[O-U-1] 실제 서비스 관리자, 실행 파일 고정 경로, 재시작 정책, socket·디렉터리 소유권과 권한은 배포 환경에서 확인해야 한다. 특정 launchd label과 config mode 0444를 코드가 설치·강제하는 것으로 간주하지 않는다. E7에는 mode 0444 검사가 없다.

### 2.2 AgentDesk·O·프로젝트·provider 환경

각 실행 노드에서 다음을 준비한다.

- 해당 runtime의 AgentDesk 바이너리, PG 연결과 runtime 저장소 접근 권한.
- 고유하고 올바른 `cluster.instance_id`, 로컬 endpoint와 Claude 또는 Codex TUI 채널 바인딩.
- **대상 채널의 O 소유권, writer의 수락 상태, 기존 binding checkpoint.**
- 실제 작업 디렉터리, 프로젝트 및 필요한 개발 도구.
- 해당 provider 바이너리와 인증 profile.
- 해당 프로젝트가 사용하는 MCP와 AgentDesk provider hook.
- transcript/rollout 및 AgentDesk 배달 기록을 읽고 유지할 권한.

O readiness는 해당 노드에서 출력 소유권이 확인되고, writer가 작업을 수락하며, 기존 O store에 대상 채널의 binding checkpoint가 있을 때만 충족된다. checkpoint는 발견한 baseline 또는 적용한 binding이 기록한 것이어야 한다. 빈 store를 만들거나 writer 채널 목록만 선택한 것으로 준비 완료를 선언하지 않는다. writer membership은 부팅 설정이므로 필요한 변경의 적용 범위와 재시작을 따로 계획한다.

근거: `src/services/herdr_launch.rs:62`, `:67`, `:72`; `src/services/turn_host.rs:288`; `src/services/tui_o/channel_policy.rs:1`, `:62`.

**Codex Herdr cold launch는 다음 조건을 별도로 확인한다.**

1. Codex 바이너리와 Herdr용 source-mode 정책이 유효하다.
2. direct TUI hook overrides가 활성화되어 있다.
3. 현재 AgentDesk hook endpoint가 존재한다.
4. 선택된 바이너리의 버전·help 정보로 hook capability가 확인된다.
5. 해당 바이너리의 `resume --help`가 `--no-daemon` 지원을 나타낸다.

이 검사는 새 Pending row를 쓰기 전에 수행된다. hook 조건 미충족은 `HooksUnavailable`, daemon isolation 미지원은 `DaemonIsolationUnavailable`로 거절된다. 일반 TUI resume eligibility만으로 Herdr cold-launch 준비를 대신 판정하지 않는다.

근거: `src/services/codex/herdr_turn.rs:186`, `:191`; `src/services/codex/tui_session_launch.rs:393`, `:416`, `:427`, `:430`, `:433`, `:436`.

코드는 실행 전에 cwd가 존재하는 절대 디렉터리인지 검사한다. Claude Herdr launch는 해당 노드의 AgentDesk 실행 파일과 Claude 바이너리를 사용하고 기존 TUI launch script를 준비한다. 다른 노드의 파일이나 도구를 복제하는 기능은 아니다.

근거: `src/services/herdr_launch.rs:270`; `src/services/claude/tui_session_launch.rs:169`.

Herdr integration installer를 중복 적용하지 않는 것을 운영 정책으로 한다. 현재 launch는 provider exec 직전에 지정된 Herdr pane 환경변수를 제거한다. Claude는 기존 AgentDesk hook endpoint를 launch 설정에 전달한다. Codex는 위의 cold-launch 조건을 검사하고 `--no-daemon`을 포함하는 실행 명령을 준비한다. Herdr 화면의 agent 감지나 상태 보고를 AgentDesk hook·응답 회수의 대체물로 삼지 않는다.

근거: `src/services/herdr_launch.rs:35`, `:285`; `src/services/claude/tui_session_launch.rs:182`; `src/services/codex/tui_session_launch.rs:458`, `:485`.

[O-U-2] 실제 노드 준비는 코드의 cwd 검사만으로 증명되지 않는다. 각 Mac에서 프로젝트·도구·MCP·인증·응답 회수를 확인한다. O checkpoint 누락, writer 미선택 또는 수락 불가, Codex hook 비활성화·endpoint 부재, `--no-daemon` 미지원 조건은 격리 환경에서 각각 준비 실패로 판정되는지도 확인한다. 조건 하나의 성공으로 나머지 준비를 대체하지 않는다.

### 2.3 다중 노드 운용

Herdr 실행 endpoint와 채널 소유권 위임을 구분한다. `channel-home`은 현재 부팅·lease·drain과 연결되어 있다. Gateway, standby, REST worker의 시작 경로가 존재하고 releasing drain의 gateway Legacy reset도 연결되어 있다. 로컬 Herdr endpoint가 있는 쪽은 reset에서 pane과 row를 유지한다.

근거: `src/services/discord/runtime_bootstrap.rs:345`, `:411`, `:440`, `:589`, `:634`; `src/services/cluster/channel_home_boot.rs:198`.

위임을 사용할 때는 `<ADK> channel-home status`로 holder·target·epoch와 `open_intake.current_epoch`, `other_epoch`, `unrouted`를 확인한다. 위임 스위치가 켜졌다는 사실이나 home row 생성만으로 준비 완료를 선언하지 않는다. 준비 실패는 채널을 거절하는 경로가 있다.

근거: `src/cli/channel_home.rs:112`, `:118`; `src/services/cluster/home_availability.rs:1`, `:8`; `src/services/turn_host.rs:118`.

[O-U-3] Mac 두 대의 실제 전달·후속 대화·단절 중 완료 후 회수는 별도로 검증한다. 한 Herdr 클라이언트에서 양쪽 화면을 보았다는 사실만으로 이를 통과 처리하지 않는다.

## 3. legacy 복귀의 현재 제한과 안전한 중단 범위

> **같은 채널의 legacy 복귀는 현재 지원되지 않는다.** 현재 main에는 같은 실행 식별을 사용하는 Claude 채널을 legacy로 복귀시키는 완료 절차가 없다. 종료·retire·매핑 제거·dcserver 재시작만으로 복귀 완료를 선언하지 않는다. 지원되는 명시적 전환 기능을 확보하기 전에는 같은 채널의 legacy 복귀 절차를 시작하지 않는다.

정상 Herdr launch는 `.host_kind=herdr`를 기록한다. retire는 종료 증거를 확인하고 row를 Retired로 바꾼 뒤 해당 nonce의 hold를 해제하지만 표식을 전환하지 않는다. 매핑 제거 후 `TurnHost::Tmux`가 선택되어도 Claude TUI는 이 표식을 보고 launch·cleanup 전에 거절한다. 관리용 row witness도 Retired record를 계속 Herdr로 취급한다.

이는 resume 실측 이전의 코드상 차단이다. 다른 시험 채널이나 다른 실행 식별의 legacy 성공으로 대상 채널의 복귀를 대신 인정하지 않는다.

근거: `src/services/herdr_launch.rs:346`, `:569`; `src/cli/herdr.rs:261`, `:287`; `src/db/dispatched_sessions/hosted_execution.rs:434`; `src/services/turn_host.rs:98`, `:140`; `src/services/claude.rs:1776`; `src/services/claude/host_gate.rs:20`; `src/services/session_host/session_record.rs:12`.

### 3.1 보존할 대상과 배출 기준

중단 전에 채널별로 진행 턴, 대기 입력의 원래 요청 식별, 배달 결과, hosted row의 provider·owner·nonce, provider 대화 기록을 대조한다. 운영 기록에는 입력 원문·인증정보를 넣지 않는다.

다음 관측은 서로 다른 의미다.

- `turn-lease inspect`의 JSON identity는 현재 mailbox 턴이 있다는 뜻이다. 성공한 `null`은 해당 조회에서 active lease가 없다는 뜻이다.
- watcher-state의 `has_pending_queue`는 대기 intervention의 존재 여부다. 이것만으로 모든 대기 상태가 없다고 증명하지 않는다.
- 채널 queue API는 pending/dispatched **dispatch 목록**이다. mailbox 입력 목록이 아니다. SQL 실패가 빈 목록으로 축약될 수도 있다.
- 내부 home drain은 mailbox의 active token·intervention·pending dispatch와 inflight, O의 `owed/prepared/unsealed/uncaptured/binding_pending`, open intake, 진행 중 POST를 별도로 확인한다.

근거: `src/server/routes/turn_lease.rs:15`; `src/services/discord/turn_lease.rs:72`; `src/services/discord/health/snapshot.rs:117`; `src/server/routes/queue_api.rs:20`; `src/services/cluster/channel_home_port.rs:40`, `:62`, `:95`; `src/services/cluster/channel_home_drain.rs:15`, `:145`.

**spool 파일을 삭제하거나 비우지 않는다.** 배출은 미완료 책임이 없다는 뜻이다. 보존된 spool 기록 파일이 없어야 한다는 뜻이 아니다. O actor는 spool을 재생하고 owed 조각을 배달한다. 위임 drain도 파일 삭제가 아니라 미완료 카운터를 판정한다.

근거: `src/services/tui_o/writer/actor.rs:1`; `src/services/cluster/channel_home_drain.rs:25`, `:100`.

[O-U-4] 일반 채널의 배출 완료를 공개 CLI/API만으로 모두 증명하는 방법은 아직 확정하지 않는다. 확인되지 않은 필드, `null`, 관측 실패, 빈 dispatch 목록을 배출 완료로 처리하지 않는다.

### 3.2 현재 가능한 제출 중단·배출·실행 은퇴

이 절은 안전한 중단과 기록 보전을 위한 절차다. 같은 채널의 legacy 복귀 완료 절차로 사용하지 않는다.

1. **대상 채널의 새 작업 제출을 운영상 중단한다.** 사용자·자동 작업 생산자가 더 보내지 않도록 조정한다. admission 파일을 범용 intake pause로 취급하지 않는다. 이미 접수된 요청은 원래 식별을 유지하며 처리·배달 여부를 대조한다. 제출 중단 수단의 적용 범위는 [O-U-4]에서 확인한다.

2. **Herdr 실행 스위치·endpoint·O 배달 경로를 유지한 채 진행 턴과 대기 입력을 배출한다.** §3.1의 관측을 확인한다. 해결되지 않은 input hold가 있으면 다음 입력을 보내지 않는다. 불명확한 prompt를 같은 실행이나 다른 노드에 재전송하지 않는다.

   근거: `src/services/claude/herdr_turn.rs:1`, `:395`, `:406`; `src/services/codex/herdr_turn.rs:139`.

3. **배출 후 새 Herdr admission을 닫는다.** 대상 runtime의 admission stop-file을 설정하거나 dcserver 시작 환경의 `ADK_HERDR_ADMISSION=off`를 사용한다. 파일 방식은 발견되면 latch되고 환경값은 최초 사용 시 읽힌다. health에서 `stopped(file)` 또는 `stopped(env)`를 확인한다. 같은 runtime의 다른 Herdr 채널에 미치는 영향을 먼저 확인한다. 두 방법 모두 pane 종료 기능이 아니다.

   근거: `src/services/herdr_admission.rs:38`, `:48`, `:70`, `:78`; `src/server/routes/health_api/runtime_profile.rs:20`.

4. **계획된 종료가 필요한 실행만 종료·retire한다.** 먼저 대화·결과 기록 보존을 확인한다. provider의 정상 종료 방법은 채택 provider에서 확인한다([O-U-5]). 명시적 pane 종료가 필요하면 §3.4의 대상·권한 확인 절차를 따른다. 실행 노드에서 `<ADK> herdr retire <channel>`을 수행할 때는 원래 endpoint를 유지한다.

   retire는 pane을 읽기만 한다. complete snapshot에서 pane이 없거나 기록된 root shell만 남았다는 증거가 있을 때 owner·nonce를 확인하는 CAS로 Retired를 쓴다. CAS 성공 후 해당 nonce의 hold만 해제한다. retire 성공은 legacy 표식·관리 witness의 전환 성공이 아니다.

   근거: `src/cli/herdr.rs:181`, `:202`, `:261`, `:287`; `src/db/dispatched_sessions/hosted_execution.rs:434`.

5. **위임 소유권 반환이 필요하면 별도 작업으로 마친다.** 위임 스위치와 양쪽 dcserver를 유지하고 gateway에서 `<ADK> channel-home reclaim <channel>`을 실행한다. Worker 상태에서 Reclaiming 전이를 요청한 뒤 holder의 drain과 gateway의 반환 완료를 기다린다. status에서 해당 home row가 사라진 것을 확인한다.

   reclaim은 provider 프로세스 이전, hosted row 은퇴, `.host_kind` 전환을 수행하는 명령이 아니다. `home_draining` blocker나 `other_epoch` 입력이 남으면 원인을 해결한다. `force`로 배출 판정을 우회하지 않는다. 소유권 반환 완료도 같은 채널의 legacy 실행 가능성을 증명하지 않는다.

   근거: `src/cli/channel_home.rs:173`; `src/services/cluster/channel_home_boot.rs:198`; `src/services/cluster/channel_home_drain.rs:145`, `:233`; `src/db/intake_outbox.rs:677`.

6. **중단 상태와 남은 책임을 기록한다.** 전용 runtime에서 모든 대상 실행을 배출·종료한 경우 필요한 Herdr 실행·관리 스위치를 끌 수 있다. runtime 전역 스위치의 다른 채널 영향과 위임 반환 완료를 확인한다. 스위치를 끈 매핑 채널은 거절되며 tmux로 자동 전환되지 않는다. 매핑 제거·dcserver 재시작을 같은 채널 복귀의 다음 단계로 안내하지 않는다.

   근거: `src/config/runtime_settings.rs:121`; `src/services/turn_host.rs:178`, `:199`, `:261`; `src/cli/channel_home.rs:70`.

현재 유지할 수 있는 legacy 운영은 **이미 legacy 조건을 충족하는 다른 채널·노드의 실행**이다. Herdr 매핑이 없고 해당 실행에 Herdr record·표식 등 충돌 증거가 없어야 한다. 새 작업을 다른 채널에서 시작하는 결정은 원래 요청의 실행·배달 결과를 먼저 확인한 별도 결정으로 기록한다. 이것을 원래 채널의 복귀나 대화 이전으로 기록하지 않는다.

근거: `src/services/turn_host.rs:91`, `:140`; `src/services/claude/host_gate.rs:20`; `src/services/session_host/session_record.rs:8`.

다른 Herdr 채널을 다시 운용할 때는 일반 제출 중단을 유지하고 admission stop-file과 시작 환경의 정지 조건을 해제한 뒤 dcserver를 재시작한다. 설정된 Herdr 채널이 있는 runtime에서 `admission="open"`을 확인하고 승인된 시험 입력을 검증한 후 일반 제출을 재개한다. 파일 제거만으로 이미 latch된 프로세스가 열리지 않는다.

근거: `src/services/herdr_admission.rs:38`, `:48`, `:53`, `:78`; `src/server/routes/health_api/runtime_profile.rs:20`; `scripts/e2e/tui_relay/herdr.py:92`.

### 3.3 살아 있는 실행을 보존해야 하는 경우

살아 있는 Herdr 프로세스·pane·진행 턴을 그대로 tmux에 넘기는 절차는 제공하지 않는다. 프로세스를 유지해야 하면 Herdr endpoint와 실행 경로를 유지하고 서버 교체와 같은 채널 전환을 연기한다.

provider 대화 resume은 새 프로세스에서 기록을 재개하는 동작이다. 기존 프로세스의 보존·이전과 구분해 검증한다([O-U-5]). resume 실측이 성공해도 §3 첫머리의 Claude legacy gate 차단이 해소되지는 않는다.

retire의 다음 거절을 강제로 해제하지 않는다.

| 결과 | 대응 |
|---|---|
| `ProviderRunning` | 실행을 유지하거나 명시적 종료를 별도로 결정 |
| `Unproven("no recorded pane")` | pane 없는 Pending을 CLI retire로 해결할 수 없음. 원본 유지·조사 |
| 기록 없는 root shell | root shell 비교만으로 retire 불가. 대상 확인 후 종료 필요성을 검토 |
| root 교체·provider 미검증·snapshot 불완전 | 연결·소유권·실행 증거 조사. 조회 실패를 종료 증거로 쓰지 않음 |
| `Changed(...)` | row가 변했으므로 다시 관측 |
| `NoLocalEndpoint` | 원래 endpoint를 확보한 뒤 재관측 |
| `NoRow(...)` | 유일한 대상이 없음. 임의 대상 선택 금지 |

근거: `src/cli/herdr.rs:110`, `:202`, `:261`.

### 3.4 비상 정지와 강제 종료

즉시 입력을 막아야 하면 배출 전에 admission을 닫는다. 진행 프로세스는 유지되지만 대기 입력의 처리·복구 결과까지 보장되는 것은 아니다. 이 경우 §3.2의 정상 배출을 통과했다고 기록하지 않고 원래 요청별 결과를 확인한다([O-U-4]).

강제 종료는 별도 운영자 결정이다. `runtime.herdr_terminate_enabled=true`를 필요한 기간에만 허용하고 다음 명령을 실행한다.

```sh
<ADK> herdr terminate \
  --session-key <verified-session-key> \
  --execution-nonce <verified-execution-nonce> \
  --reason <non-secret-reason>
```

nonce는 `herdr status`에 나오지만 **session key와 pane ID는 나오지 않는다.** `/api/sessions`는 session key 후보를 제공하지만 channel·hosted nonce 결합을 제공하는 응답은 아니다. [O-U-6]에서 대상 관계를 확인할 수 없으면 강제 종료하지 않는다.

근거: `src/cli/herdr.rs:26`, `:337`; `src/server/routes/agents_crud.rs:943`, `:977`.

CLI는 loopback API에 POST 한 번을 보낸다. redirect·자동 재시도·proxy를 사용하지 않고 최대 120초 응답을 기다린다. 서버는 loopback peer, Origin 부재, 인증 principal을 검사한다. `server.auth_token`이 비어 있지 않으면 관리자 Bearer가 필요하다. 인증정보를 명령 예시나 운영 기록에 넣지 않는다.

근거: `src/cli/herdr.rs:61`; `src/cli/client/transport.rs:78`; `src/server/routes/herdr_terminate.rs:35`.

성공은 HTTP 200과 `result="retired"`뿐이다. 서버는 pane 종료 확인·row retire·hold 해제·포착한 턴의 정산을 수행한다. `retire`와 동시에 실행하지 않는 것을 운영 정책으로 한다. terminate 성공도 legacy 전환 성공이 아니다.

근거: `src/cli/herdr.rs:69`; `src/server/routes/herdr_terminate.rs:102`; `src/services/discord/herdr_terminate.rs:215`, `:250`, `:299`, `:417`.

- `disabled`, `refused`, `gate_refused`, `not_sent`: 원인을 해결하고 대상을 재관측한다.
- `confirmation_required`: 서비스는 확인을 대신 주지 않는다. workspace/group 전체 종료로 확대하지 않는다.
- `provider_survived_close`: pane close 후 provider 생존이 확인된 상태다. retire 완료로 보지 않는다.
- `indeterminate`: 종료가 반영됐거나 row·hold 정리가 일부 끝났을 수 있다. 원래 key·nonce와 서버 결과 로그를 먼저 확인한다.
- 요청 연결이 끊겨도 서버 task는 계속할 수 있다. CLI 오류만 보고 즉시 새 실행을 만들지 않는다.
- 이미 Retired인데 hold가 남으면 같은 채널의 `herdr retire`로 해당 hold 해제를 재시도한다. `remains`와 durability 미확정 출력은 완료가 아니다.

근거: `src/services/discord/herdr_terminate.rs:37`, `:215`, `:250`, `:441`; `src/cli/herdr.rs:157`, `:232`.

턴 lease가 남으면 먼저 inspect한다. 실제 종료·배달 결과를 확인한 운영자가 inspect의 정확한 identity를 지정해 release할 수 있다. planned restart·identity 변경·메시지 identity 없는 episode 등은 거절될 수 있다. release를 provider 종료나 답변 배달 증거로 쓰지 않는다. “시작되지 않았으면 다시 보낸다”는 자동 대응을 하지 않는다.

근거: `src/cli/turn_lease.rs:4`, `:12`; `src/services/discord/turn_lease.rs:89`, `:149`, `:201`.

### 3.4.1 never-attached 실행의 정리

attach가 한 번도 되지 않은 Pending 실행(logged start 없음)은 다음 턴마다 거절된다. 거절 사유는 `herdr turn: pending execution <nonce> never attached (no logged start of <session>)`로 시작하고, 뒤의 꼬리가 아래 경우를 가른다. 같은 내용이 dcserver 로그 `event="herdr_turn_never_attached"`에 channel·session key·nonce·pane·terminate 스위치와 함께 남는다. 이 경우에 한해 session key를 거절 사유와 로그에서 얻을 수 있다(§3.4의 [O-U-6] 공백).

pane 위치는 있고 launch 증거가 없으면, 거절하기 전에 기존 launch 재탐침이 그 pane을 한 번 더 읽는다. 자기 nonce의 provider가 root shell의 자식으로 증명될 때만 증거를 한 번 기록한다. pane을 만들거나 입력하거나 닫지 않는다. create가 서버에 닿지 않은 launch(marker 실패, 두 번째 E7 거절, create `NotSent`)는 그 자리에서 자기 Pending을 Retired로 만든다. 다음 입력은 새 launch로 간다.

근거: `src/services/claude/herdr_turn/never_attached.rs:12`, `:30`; `src/services/herdr_launch.rs:442`.

**A. 증거 있음 — 꼬리 `end it with ...` 또는 `ending it needs runtime.herdr_terminate_enabled ...`**

0. 대상 확정(읽기 전용). 거절 사유나 로그에서 session key·nonce·실행 노드를 얻는다. 그 노드의 `<ADK> herdr status`에서 같은 channel·nonce가 `state pending`·`launch_evidence recorded`인지 대조한다. 다르면 멈춘다.
1. 사용자 결정: 그 노드의 `runtime.herdr_terminate_enabled`를 켤지 정한다. 이미 켜져 있으면 3으로 간다.
2. 적용: 그 노드의 live 설정에 `herdr_terminate_enabled: true`를 넣는다(§1.2 live reload).
3. 실행: `<ADK> herdr terminate --session-key <key> --execution-nonce <nonce> --reason never-attached`. `disabled`는 아무것도 읽거나 쓰지 않은 결과라 재시도해도 안전하다. 적용을 기다린 뒤 다시 실행한다.
4. 성공은 HTTP 200 `result="retired"`뿐이다. 다른 결과는 §3.4 분류를 따른다. `herdr status`에서 그 channel 항목이 사라졌는지 확인한다.
5. 1에서 켰으면 원래 값으로 되돌린다. 다음 사용자 입력은 새 launch로 간다.

operator terminate는 대기열을 건드리지 않으므로 Retired 뒤 남은 큐가 새 launch로 진행한다. `/clear`도 같은 실행을 terminate 서비스로 끝내지만 실행 시점의 대기열을 버린다. 스위치가 꺼져 있으면 `/clear`는 pane 종료·row retire를 하지 않고 종료 미확인을 알리지만, 큐 폐기와 세션 초기화는 그대로 한다. 막힌 동안 도착한 입력은 각자 턴으로 거절돼 소비되므로, 보존되는 것은 terminate 시점에 남은 대기열뿐이다.

근거: `src/services/discord/herdr_terminate.rs:28`, `:70`; `src/services/discord/commands/control.rs:376`; `src/services/discord/commands/control/clear_teardown.rs:121`.

**B. 위치 있음, provider 미증명 — 꼬리 `its provider in pane <pane> is not proven (...)`** (손 조치, 자동 복구가 아님)

1. `herdr status`에서 nonce와 `pane root_shell_unrecorded`(또는 `provider_unverified`)를 확인한다. 거절 사유의 pane ID를 확인한다.
2. 사용자 승인 뒤 그 노드의 release herdr 서버에서 `herdr pane process-info <pane>`으로 root shell과 자식을 확인한다. 기대와 다르면 멈춘다.
3. `herdr pane close <pane>`.
4. `<ADK> herdr retire <channel>` → `retired`. status 항목이 사라졌는지 확인한다.

**C. 위치 없음 — 꼬리 `no pane was recorded and its create outcome is unknown, so it is kept`**

create 응답을 잃었거나 위치 기록 전에 실패해 pane이 있을 수도 있는 상태다. 현재 정리 절차가 없다. status 목록에 나오지 않고 retire는 `NoRow("no single live herdr row of the channel")`, terminate는 `refused`(`execution identity differs`)다. 원본 row를 유지하고 수동으로 덮어쓰지 않는다.

근거: `src/db/dispatched_sessions/hosted_execution.rs:309`; `src/cli/herdr.rs:250`.

### 3.5 후속 필요 — 같은 채널의 명시적 전환

**후속 노드 `i5340_legacy_return`(#5340 범위, 맥 소유)**

같은 채널을 유지하는 지원된 Herdr→legacy 전환 기능이 필요하다. 최소 계약은 다음과 같다.

1. 진행 턴·대기 입력·배달 책임의 배출과 provider 종료를 확인한다.
2. 대상 owner·execution nonce와 Retired 상태를 확인하는 CAS로 동시 변경을 거절한다.
3. 이전 hosted record, 실행·대화·배달 기록과 전환 이력을 보존한다.
4. `.host_kind` 표식과 관리용 row 증거를 일관되게 전환한다. 턴 선택과 관리 명령이 서로 다른 host로 판단하는 상태를 완료로 인정하지 않는다.
5. 전환 중간 상태와 실패 상태에서 입력을 차단하고, 재시도·복구 판단에 사용할 증거를 남긴다.

이는 현재 구현의 기능 설명이 아니라 후속 기능의 요구사항이다. `.host_kind`, hosted record, inflight 또는 spool을 수동으로 삭제·덮어써서 우회하는 절차를 제공하지 않는다.

[O-U-10] 후속 기능을 확보한 뒤 **동일한 Claude 채널·실행 식별**에서 전체 복귀 순서, 첫 legacy 턴, 후속 턴, 관리 명령을 검증한다. 표식 전환을 생략하는 뮤턴트와 다른 채널의 성공을 대상 채널의 성공으로 인정하는 뮤턴트를 실패시켜야 한다. 그 전에는 같은 채널의 안전한 복귀 수용 기준을 미충족으로 유지한다.

현재 차단의 근거: `src/services/herdr_launch.rs:346`, `:569`; `src/cli/herdr.rs:287`; `src/db/dispatched_sessions/hosted_execution.rs:434`; `src/services/claude.rs:1776`; `src/services/claude/host_gate.rs:20`; `src/services/session_host/session_record.rs:12`.

## 4. Herdr 서버 업데이트·복구

### 4.1 호환 클라이언트 업데이트 — 서버 유지

운영 정책상 호환 클라이언트 업데이트는 서버를 유지한 채 수행한다.

1. 업데이트 전 server ping 결과, 서버 프로세스 식별, 대상 실행 nonce·pane 상태를 기록한다.
2. 서버 서비스 정의와 서버 바이너리 경로를 유지하고 클라이언트만 교체한다.
3. 같은 관리 서버에 재연결하여 화면 관측과 AgentDesk 상태를 확인한다.
4. 서버 프로세스 식별과 실행 nonce의 연속성을 확인한다. 연결 실패나 버전 문자열 차이만으로 서버를 재시작하지 않는다.
5. 호환성이 부족하면 클라이언트를 이전 버전으로 되돌리고 서버를 유지한다.

[O-U-7] 이 저장소는 외부 Herdr 클라이언트 업데이트·attach의 호환성을 구현하지 않는다. 실제 사용할 클라이언트 버전 쌍을 격리 서버에서 측정한다. 클라이언트 분리·재연결 중 provider 프로세스와 AgentDesk 응답 회수가 유지되는지 확인한다.

Adapter의 서버 식별은 socket peer PID와 OS 시작 식별을 사용한다. metadata, pane ID 또는 버전 문자열만으로 같은 서버 프로세스라고 판단하지 않는다. 일반 handshake의 version 허용과 E7의 0.9.3 제한은 별개다.

근거: `src/services/session_host/herdr/transport.rs:41`, `:80`, `:156`; `src/services/session_host/herdr/observe.rs:117`, `:153`.

### 4.2 실행 서버 교체 — 별도 승인·정리

운영 정책상 서버 교체에는 별도 승인, 대상 노드·관리 서버·영향받는 모든 pane의 목록, 종료할 실행과 보존할 기록, 교체 버전 및 되돌릴 바이너리를 명시한다. 클라이언트 업데이트 승인에 서버 교체를 포함시키지 않는다.

1. §3.1–§3.2의 **제출 중단·배출·admission 정지**를 수행한다. 같은 채널의 legacy 복귀를 시작하는 작업으로 해석하지 않는다.
2. 영향을 받는 모든 실행을 정상 종료·retire하거나 별도 승인한 종료 절차를 수행한다. 살아 있는 실행을 보존해야 하면 교체를 연기한다.
3. 서비스 관리자의 재기동 정책을 확인한다([O-U-1]). 정지 직후 자동으로 다시 뜨는 상태에서 바이너리 교체를 진행하지 않는다.
4. 지정한 관리 서버만 정지한다. 저장소의 격리 서버 코드에는 `<HERDR_BIN> server stop`과 `<HERDR_BIN> server`가 사용된다. 이를 운영 서비스의 완성된 lifecycle 절차로 간주하지 않는다.
5. 기존 바이너리와 설정·대화·배달 기록을 보존하고 새 서버를 명시적으로 기동한다. 현재 입력 재개에는 E7 검증 목록의 0.9.3 조건이 필요하다. 새 서버 버전 승인은 adapter 검증·변경을 별도 작업으로 다룬다.
6. **일반 제출 중단을 유지한 채 읽기 전용 검사를 마친다.** ping, socket 접근, 새 서버 식별, endpoint, E7 provenance 조건, row·hold를 확인한다. health 조회가 E7을 실행하는 것으로 가정하지 않는다.
7. **admission stop-file과 시작 환경의 정지 조건을 해제하고 dcserver를 재시작한다.** 설정된 시험 채널의 runtime에서 `admission="open"`과 준비 상태를 확인한 뒤 승인된 시험 입력만 보낸다. 첫 입력·후속 입력·원래 요청의 응답 회수를 검증한다. 시험 통과 후 일반 제출을 재개한다.
8. 실패하면 일반 제출 중단을 유지하고 필요한 admission 정지를 다시 적용한다. 새 시험 실행의 종료·기록을 확인한 뒤 이전 바이너리로 되돌리는 절차를 적용한다. 이전 바이너리를 다시 띄웠다는 사실을 이전 provider 프로세스 복원으로 기록하지 않는다([O-U-5]).

admission을 닫은 채 시험 입력을 요구하지 않는다. latch된 프로세스에서 stop-file만 제거해도 시험은 계속 거절될 수 있다. 재시작 후 `open`을 확인한 경우와 대조하는 검증을 [O-U-1]에 포함한다.

근거: `src/services/turn_host.rs:287`; `src/services/session_host/herdr_gate.rs:635`; `src/services/herdr_admission.rs:48`, `:53`, `:78`; `scripts/e2e/tui_relay/herdr.py:92`; `src/server/routes/health_api/runtime_profile.rs:15`; `src/services/session_host/herdr/launch_host_real_tests.rs:48`, `:68`, `:92`; `src/services/session_host/herdr/model.rs:15`; `src/services/session_host/herdr/observe.rs:103`.

[O-U-5] provider 정상 종료, Herdr 서버 완전 종료·재기동 시 provider 프로세스 소멸, 새 Herdr 프로세스에서의 대화 resume은 격리 환경에서 측정한다. experimental live handoff나 실행 중 프로세스 이동을 기본 안전성의 전제로 사용하지 않는다. 이 측정으로 같은 Claude 채널의 legacy 전환 차단을 해소했다고 판정하지 않는다.

### 4.3 서버 비정상 시 복구

1. 새 admission을 닫고 원래 요청·row·hold·대화·배달 기록을 유지한다.
2. read-only ping, snapshot, process-info와 `herdr status`로 연결 불능·pane 부재·provider 종료·교체를 구분한다. socket 연결 오류만으로 서버 종료나 작업 실패를 확정하지 않는다.
3. 서버와 pane이 살아 있으면 접속 환경·권한을 복구하고 재조회한다. 새 pane 생성·prompt 재전송·다른 노드 실행으로 우회하지 않는다.
4. dcserver만 재시작했다면 restart reader가 원래 Bound 실행을 재조회한다. 확인된 실행과 source baseline만 재연결한다. Pending은 이 패스에서 실행하지 않는다.
5. 서버가 교체되었거나 pane/root/provider 증거가 달라졌다면 원래 실행을 채택하지 않는다. 종료를 증명할 수 있으면 retire하고, 증명하지 못하면 원본을 유지하여 조사한다.
6. 서버 재기동이 필요하면 §4.2의 승인·정리 절차를 따른다. 일반 제출 중단을 유지하고 admission 재개·승인된 시험·원래 요청의 결과 및 배달 대조를 마친 후 일반 제출을 재개한다.

근거: `src/services/session_host/herdr/model.rs:206`; `src/services/discord/recovery_engine/herdr_reader.rs:26`, `:178`, `:280`; `src/services/discord/tui_prompt_relay/herdr_source.rs:271`; `src/cli/herdr.rs:202`, `:261`.

dcserver 재접속은 provider 응답을 자동으로 재실행하는 경로가 아니다. Claude 자신의 clear Pending은 source를 복원하지 않고 입력을 허용할 수 있다. 다른 실행의 Pending이나 baseline 부재를 같은 조건으로 취급하지 않는다.

근거: `src/services/discord/tui_prompt_relay/herdr_source.rs:271`, `:291`.

## 5. 확인 명령표

아래는 운영 시 사용할 명령·API다. 문서 작성 과정에서는 실행하지 않았다. API는 대상 노드의 승인된 local 접근·인증 경로를 사용한다. 인증정보는 기록하지 않는다.

### 5.1 읽기 전용 확인

| 시점·목적 | 명령 또는 API | 기대 결과와 한계 | 근거 |
|---|---|---|---|
| 서버 확인 | Socket API `ping`: `{"id":"ops-ping","method":"ping","params":{}}` | 같은 id의 `result.type="pong"`, `protocol=22`. 현재 E7 허용 버전은 `0.9.3`. ping만으로 E7·프로세스 연속성 증명 불가 | `src/services/session_host/herdr/model.rs:164`, `:221`, `:270`; `src/services/session_host/herdr/observe.rs:153` |
| 서버 pane 목록 | Socket API `session.snapshot` | `result.type="session_snapshot"`와 snapshot. pane ID만으로 AgentDesk 실행 소유권 판단 불가 | `src/services/session_host/herdr/model.rs:169`, `:238`, `:277` |
| 대상 pane 프로세스 | Socket API `pane.process_info`, params `{"pane_id":"<verified-pane-id>"}` | shell/foreground 정보. 실행 확인에는 nonce와 OS 시작 증거가 추가로 필요 | `src/services/session_host/herdr/model.rs:173`, `:245`; `src/services/discord/recovery_engine/herdr_reader.rs:51` |
| 설정 반영·admission | `GET /api/health/detail` | `herdr.configured_channels`, endpoint `local`, `restart_required`, `admission`. `last_e7="never"`는 E7 성공이 아님 | `src/server/routes/health_api/runtime_profile.rs:15` |
| 실행·hold 현재 관측 | `<ADK> herdr status` | `executions`, `other_input_holds`; launch evidence와 pane 상태. Retired는 hold가 남을 때만 표시됨 | `src/cli/herdr.rs:301`, `:319` |
| 현재 턴 | `<ADK> turn-lease inspect --provider <provider> --channel-id <channel>` | identity 또는 성공한 `null`. 조회 거절은 idle 증거가 아님 | `src/cli/turn_lease.rs:12`; `src/server/routes/turn_lease.rs:15` |
| 대기 intervention·relay | `GET /api/channels/<channel>/watcher-state` | `has_pending_queue`와 이용 가능한 mailbox/inflight·relay 필드. `unread_bytes=null`은 0이 아님. O drain 전체 증명 불가 | `src/server/routes/domains/runtime.rs:109`; `src/server/routes/queue_api.rs:267`; `src/services/discord/health/snapshot.rs:78`, `:117` |
| dispatch 참고 조회 | `GET /api/channels/<channel>/queue` | `dispatches` 목록. mailbox 입력 목록이나 확실한 배출 판정으로 사용하지 않음 | `src/server/routes/queue_api.rs:20` |
| 종료 session key 후보 | `GET /api/sessions` | `sessions[].session_key`, provider·instance·agent. 조회 상태 필터가 있고 channel/nonce 결합은 없음. [O-U-6] 필요 | `src/server/routes/agents_crud.rs:943`, `:977` |
| 위임·반환 | `<ADK> channel-home status` | home state·holder·target·epoch, 열린 입력의 epoch별 수. 반환 완료 후 대상 home row 부재 확인 | `src/cli/channel_home.rs:112`; `src/services/cluster/channel_home_boot.rs:198` |

Socket API는 코드의 line-delimited JSON 방식으로 요청마다 별도 연결을 사용한다. framing 소스에는 미검증 가정이라는 주석이 남아 있다. [O-U-8] 채택 바이너리에서 실제 framing·필수 응답 필드·오류 응답을 격리 환경으로 확인한다.

근거: `src/services/session_host/herdr/wire.rs:1`, `:49`; `src/services/session_host/herdr/transport.rs:1`, `:105`.

health의 reconnect 카운터는 최신 pass의 요약이다.

- `published`: source를 복원했거나 자신의 clear를 기다리며 입력을 허용한 실행.
- `withheld`: 읽힌 불일치·부재 및 기타 보류 결과의 합계. 모두 영구 거절이라는 뜻은 아니다.
- `unknown`: 읽지 못했거나 판단할 수 없는 실행.
- `pending`: 다음 턴에 맡긴 Pending.
- `input_holds`: pass가 센 캐시. 첫 pass 전에는 `not_counted_yet`, 읽기 실패는 `unreadable`.

health 요청은 현재 hold 파일을 새로 읽지 않는다. CLI의 현재 관측을 함께 확인한다. `published>0`만으로 대상 채널이 복구됐다고 선언하지 않는다.

근거: `src/services/discord/recovery_engine/herdr_reader.rs:74`, `:178`, `:319`; `src/server/routes/health_api/runtime_profile.rs:50`.

`herdr status`의 `provider_running`은 provider가 존재한다는 뜻이다. 진행 턴 완료를 뜻하지 않는다. `root_shell_unrecorded`, `root_replaced`, `provider_unverified`, `unreadable`, `no_location`, `no_local_endpoint`를 정상 은퇴·복귀 상태로 취급하지 않는다.

근거: `src/cli/herdr.rs:261`, `:301`.

설정된 Herdr 채널이 없는 runtime의 `admission="not_evaluated"`는 정상 표현이다. Herdr 시험 입력을 수행할 runtime의 `open` 확인과 구분한다.

근거: `src/server/routes/health_api/runtime_profile.rs:20`.

### 5.2 변경 명령의 결과 확인

| 절차 | 명령 | 기대 결과·실패 시 정지 조건 | 근거 |
|---|---|---|---|
| 실행 은퇴 | `<ADK> herdr retire <channel>` | `retired`와 `no input hold is left`. row와 hold 결과 모두 확인. legacy 전환 증거로 사용하지 않음 | `src/cli/herdr.rs:157`, `:202`, `:287` |
| 명시적 강제 종료 | `<ADK> herdr terminate --session-key <key> --execution-nonce <nonce> --reason <reason>` | HTTP 200, `result="retired"`만 성공. 다른 결과나 연결 오류에서 임의 재실행 금지 | `src/cli/herdr.rs:26`, `:61`; `src/server/routes/herdr_terminate.rs:102` |
| 위임 소유권 반환 요청 | gateway의 `<ADK> channel-home reclaim <channel>` | 출력 row가 Reclaiming. CLI 성공은 반환 완료가 아님. 이후 status와 drain 확인. legacy 실행 전환과 별개 | `src/cli/channel_home.rs:173`; `src/services/cluster/channel_home_boot.rs:198` |
| 부팅 설정·admission latch 재평가 | 대상 runtime의 `<ADK> restart-dcserver` | CLI 결과 후 detail에서 설정·readiness 확인. Herdr 서버 식별과 실행 연속성 별도 확인. 같은 채널 복귀 수단이 아님 | `src/cli/args.rs:32`; `src/cli/dcserver.rs:430`; `src/server/routes/health_api/runtime_profile.rs:35` |
| 종료 확인 후 남은 턴의 명시적 해제 | `<ADK> turn-lease release --expected '<exact-inspect-identity-json>' --reason <reason>` | 정확한 identity의 결과 확인. 거절 시 우회하지 않음 | `src/cli/turn_lease.rs:21`; `src/services/discord/turn_lease.rs:149`, `:201` |
| 승인된 관리 서버 정지·기동 | 관리 서버 환경의 `<HERDR_BIN> server stop`, `<HERDR_BIN> server` | 저장소에는 격리 서버 사용만 확인됨. 운영 manager와 권한은 [O-U-1], 프로세스 소멸은 [O-U-5] 확인 | `src/services/session_host/herdr/launch_host_real_tests.rs:48`, `:68`, `:92` |

admission stop-file 설정·해제는 파일 시스템 운영 조치다. 전용 AgentDesk CLI 명령은 아니다. 실제 경로를 문서 예시에서 추정하지 않는다. 정지 해제 후 재시작과 detail 관측을 포함한다.

근거: `src/services/herdr_admission.rs:48`, `:70`; `src/server/routes/health_api/runtime_profile.rs:20`.

## 6. 시험 채널 gate와 dcserver 재시작 관측

운영 변경 후 별도로 승인된 시험 채널에서 수행한다. 아래 하네스는 입력을 보내거나 관측 파일을 생성한다. 문서 작성만을 위해 실행하지 않는다. 일반 제출 중단과 시험 입력 허용을 별도로 관리하고, 시험 전에 Herdr admission이 `open`인지 확인한다.

### 6.1 첫 Claude gate

공통 인자는 다음과 같다.

```text
--cell claude-herdr
--channel-id <test-channel>
--base-url <target-node-loopback-base>
--herdr-endpoint <endpoint-key>
--herdr-status-bin <same-runtime-adk>
```

하네스와 status CLI는 API 대상 노드의 같은 runtime에서 실행한다. readiness 검사는 모든 configured endpoint가 local인지도 요구한다. local endpoint 하나만 선택해 mixed/remote endpoint 구성을 승인할 수 없다.

근거: `scripts/e2e/tui_relay/herdr.py:88`, `:158`; `scripts/e2e/run_tui_relay.py:251`.

운영 정책상 첫 gate는 E-1 → E-3 → E-2 → E-15 → E-50을 순서대로 한 번씩 실행하고 시나리오별 `status=pass`를 확인한다. 종료 코드 0, `known_gap`, `unexpected_pass`, `not_applicable`을 pass로 바꾸지 않는다. 각 실행에 고유 run ID와 output을 사용한다.

```sh
python3 scripts/e2e/run_tui_relay.py <common-args> \
  --no-reset-before-each --filter <scenario> \
  --run-id <unique-run> --output <unique-output>

python3 scripts/e2e/post_deploy_turn_smoke.py judge \
  --report <output>/report.claude-herdr.json \
  --scenarios tests/e2e/tui_relay/scenarios \
  --scenario E-50 --cell claude-herdr \
  --channel-id <test-channel> --driver-rc <driver-rc>
```

하네스는 detail readiness와 현재 CLI row·hold를 검사한다. 정상 row는 Bound, 해당 provider, `provider_running`, `launch_evidence="recorded"`, 유효 nonce, hold 없음이어야 한다. cold 관측에는 row 부재가 허용된다.

근거: `scripts/e2e/tui_relay/herdr.py:38`, `:108`, `:130`, `:158`; `scripts/e2e/post_deploy_turn_smoke.py:101`, `:335`.

공유 노드의 degraded reason 예외는 `--herdr-allowed-degraded-reason <pattern>`으로 해당 실행에만 줄 수 있다. 대상 채널 또는 Herdr 사유, 빈 패턴, `*` 단독은 면제하지 않는다. 집계 사유가 대상 채널과 무관하다는 증거를 별도로 확보할 수 없으면 면제하지 않는다. report의 waiver 기록을 검수한다.

근거: `scripts/e2e/tui_relay/herdr.py:30`, `:38`, `:67`, `:72`.

E-50은 지원된 `!clear` 거절 답장을 실패로 처리한다. clear 완료·거절은 해당 송신 응답 이후 답장을 기준으로 관측한다. 이 gate를 stop·terminate·전체 재시작 시나리오나 Codex·Mac 두 대 검증으로 확대 해석하지 않는다. 다른 채널의 gate 성공은 §3의 대상 채널 legacy 복귀 증거가 아니다.

근거: `scripts/e2e/tui_relay/herdr.py:25`; `scripts/e2e/run_tui_relay.py:3828`, `:4004`.

### 6.2 운영자가 수행한 dcserver 재시작 관측

살아 있는 정상 Herdr 실행에 대해 먼저 mark한다.

```sh
python3 scripts/e2e/run_tui_relay.py <common-args> \
  --herdr-restart-observe-only mark \
  --herdr-restart-mark <unique-mark.json> \
  --output <unique-mark-output>
```

운영자가 dcserver만 재시작한 뒤 같은 대상과 mark로 verify한다.

```sh
python3 scripts/e2e/run_tui_relay.py <common-args> \
  --herdr-restart-observe-only verify \
  --herdr-restart-mark <unique-mark.json> \
  --output <unique-verify-output>
```

이 모듈은 서버를 재시작하지 않는다. verify는 재접속 관측 뒤 후속 턴을 보낼 수 있으므로 admission과 제출 통제를 준비한다. `--herdr-isolated-server`, `--restart-script`, 재시작 단계가 있는 후속 시나리오를 병용하지 않는다. gate와 mark/verify의 output을 분리한다.

근거: `scripts/e2e/tui_relay/herdr_restart.py:1`, `:21`, `:105`, `:126`.

재접속 검사는 `published>0`, `channels=published`, `withheld/unknown/pending=0`과 정상 현재 row를 요구한다. 노드 전체 관측이므로 다른 채널의 문제로도 실패할 수 있다.

근거: `scripts/e2e/tui_relay/herdr.py:113`, `:121`, `:130`.

`server_uptime` 판정은 mark 완료 후 경과와 uptime을 비교한 추정이다. boot ID 수준의 incarnation 증명이 아니다. mark에 완료 시각·지원된 부팅 시계가 없으면 새 mark가 필요하다. uptime 부재는 `not_proven`, 충분한 재시작 증거가 없으면 `not_observed`다. 별도 운영자 재시작 증거를 보존한다. 이 관측으로 Herdr 서버 교체를 증명하지 않는다.

근거: `scripts/e2e/tui_relay/herdr_restart.py:12`, `:42`, `:81`.

## 7. 아직 운영 경로에 연결되지 않은 stop settlement

현재 생산 빌드의 provider-terminal stop settlement는 hard-false다. `runtime.herdr_cancel_enabled=true`를 설정해도 활성화할 수 없다. stop intent·terminal 정산·중단 안내 재전달 관련 코드를 현재 운영 보장으로 사용하지 않는다.

근거: `src/services/provider/cancel_token_claude_interrupt.rs:581`, `:598`.

반면 `channel-home`의 부팅·lease·drain·gateway reset은 현재 연결되어 있다. stop settlement의 휴면 상태를 channel-home에도 적용하지 않는다.

근거: `src/services/discord/runtime_bootstrap.rs:345`, `:411`, `:440`, `:589`, `:634`.

[O-U-9] 향후 stop settlement 활성화 시 별도 운영 문서와 검증이 필요하다. 중단 안내 송신 뒤 durable ACK 전 장애, fallback 안내 중복, receipt가 남은 상태의 재시작·롤백을 측정한다. 검증 전 정확히 한 번 안내나 안전한 자동 정산을 약속하지 않는다.

## 8. 운영 기록과 미확인 항목

### 8.1 작업 결과 기록

안전한 중단·서버 교체·장애 복구마다 다음을 기록한다.

- 대상 노드·runtime·채널과 승인 범위.
- 전후 서버 식별·버전, 실행 nonce, retire 및 hold 해제 결과.
- 진행 턴·대기 입력·배달 책임의 확인 결과와 미확인 항목.
- 위임 사용 시 반환 완료와 열린 intake 확인.
- 전후 설정 반영·readiness, admission 상태와 승인된 시험 결과.
- 적용되는 미확인 항목의 증거 위치 또는 작업 중단 사유.
- 같은 채널의 legacy 전환은 현재 미지원이라는 제한과 후속 기능 상태.

이 runbook의 존재나 초안 승인만으로 #5340의 실기기 검증·운영 책임 이전·서버 종료 승인·같은 채널 복귀가 완료된 것은 아니다.

### 8.2 미확인 항목 대장

확인 담당은 역할별 책임이다. 코디네이터가 실제 담당자를 지정하고 증거 위치를 기록한다. 담당 지정·방법·완료 증거가 빠진 항목은 완료 처리하지 않는다.

| 안정 ID | 항목 | 확인 담당 | 방법 | 완료 증거 |
|---|---|---|---|---|
| O-U-1 | 서비스 관리자·버전 고정·재기동 방지·socket 권한·admission 재개 | 노드 운영 담당 | 비밀값을 출력하지 않고 서비스 정의·파일 메타데이터 확인. 격리 환경에서 자동 재기동·비허가 연결 검사. stop-file latch 후 파일만 해제한 경우와 dcserver 재시작 후 시험을 대조 | 배포물 식별, 서비스·권한 점검 기록, latch 유지 시 입력 거절 및 재시작 후 `open`·시험 성공 기록 |
| O-U-2 | 노드별 O·프로젝트·도구·MCP·provider 준비 | O 담당과 provider·프로젝트 담당 | 각 Mac의 실제 개발 작업 확인. checkpoint 누락, writer 미선택·수락 불가, Codex hook 조건 미충족, `--no-daemon` 미지원 조건을 각각 격리해 준비 판정 확인 | 노드별 준비표, 조건별 거절 증거, cwd·도구·MCP 사용과 응답 회수 기록 |
| O-U-3 | Mac 두 대 전달·후속 입력·단절 후 결과 회수 | 다중 노드 검증 담당 | 양쪽 시험 채널 동시 실행, 연결 단절 중 완료, 재연결 후 원래 요청별 배달 대조 | 노드·provider·nonce·요청별 결과 및 배달 대조표 |
| O-U-4 | 제출 통제와 배출을 증명할 운영 관측 | intake·O 운영 담당 | intervention·pending dispatch·미배달·진행 중 POST를 각각 만들고 관측과 실제 처리 대조. 관측 실패도 포함 | 제출 중단 적용 범위, 각 미완료 책임의 관측·해소 기록. 증명 불가 시 중단 기록 |
| O-U-5 | 정상 종료·서버 종료 시 프로세스 소멸·Herdr 대화 resume | provider 검증 담당 | 격리 실행의 PID·시작 식별·대화 기록을 종료·재기동 전후 비교. 새 프로세스 resume과 기존 프로세스 보존을 구분 | 종료 확인, 전후 프로세스 식별, 기록 보존 및 새 Herdr 실행의 resume 결과. legacy 전환 해소 증거로 사용하지 않음 |
| O-U-6 | channel·session key·nonce·pane의 안전한 결합 | 종료 작업 담당 | status, sessions API, 원래 실행 생성 기록을 대조해 유일한 대상을 확인 | 대상 결합표와 확인 시각. 유일성 미확보 시 종료 중단 기록 |
| O-U-7 | 호환 클라이언트 업데이트의 서버 유지 | Herdr 클라이언트 검증 담당 | 실제 버전 쌍으로 detach/update/attach 수행, 서버 식별·nonce·응답 회수 연속성 확인 | 버전 쌍, 동일 서버·실행 증거, 연결 전후 결과 회수 기록 |
| O-U-8 | 채택 서버의 Socket API framing·필수 필드 | adapter 검증 담당 | 격리 서버의 read-only ping/snapshot/process-info와 오류 응답을 framing·schema와 대조 | 바이너리 식별, 비민감 요청·응답 검수 결과와 불일치 목록 |
| O-U-9 | 향후 stop settlement 활성화·롤백 | 취소·terminal 계약 담당 | 생산 연결 확보 후 송신→ACK 사이 장애, fallback 중복, 남은 receipt의 재시작·롤백 검증 | 활성화 구현 SHA, 계약별 결과·durable 증거. 현재 hard-false는 통과로 기록하지 않음 |
| O-U-10 | 같은 Claude 채널의 명시적 legacy 전환 | 전환 기능 담당과 독립 검수 담당 | §3.5 계약 구현 후 동일 채널의 전체 복귀·첫 턴·후속 턴·관리 명령 확인. 표식 전환 누락 및 다른 채널 성공 대체 뮤턴트 검증 | `i5340_legacy_return` 구현 SHA, 이전 기록 보존·CAS·증거 전환·중간 입력 차단 증거, 동일 채널 시험 및 두 뮤턴트 실패. 현재 `[후속 이슈 미발급]` |

## 9. Herdr 책임 이전과 연결 코드 기록 — #5340

### 9.1 기준과 판정 범위

기준 커밋 **S = `b3006723b4f0cd9d850b9a479f2f1f3a8b582166`**. 이 절의 모든 `경로:줄` 좌표는 S를 가리킨다. 변경 이력은 별도 커밋 SHA로 표시한다.

이 절은 `docs/agent-maintenance/herdr-runbook.md`의 §9 전체 교체본이다. 지정 커밋의 코드·diff를 정적으로 검토했다. 파일·브랜치를 변경하지 않았고 cargo·시험·네트워크를 사용하지 않았다. `agentdesk.yaml`, `launchd.env`, 런타임 디렉터리의 값도 읽거나 기록하지 않았다.

상태는 다음처럼 구분한다.

- **운영 연결됨**: 생산 진입점에서 소비하는 경로를 코드로 확인했다. 스위치·사전조건이 필요하며 실제 배포 활성화나 실기기 성공을 뜻하지 않는다.
- **구현 제공**: 메서드·adapter·registry 제공은 확인했다. 해당 생산 소비자 연결과 운영 이전 실적은 별도로 확인한다.
- **휴면**: 생산 경로가 명시적으로 차단되어 현재 운영 실적으로 집계할 수 없다.
- **예정**: 아직 확보하지 못한 기능이나 후속 검증 대상이다.

Claude와 Codex 실행기는 생산 dispatch에 연결되어 있지만 각각 기본 꺼짐 스위치를 사용한다. 실제 활성화와 운영에서 이전된 책임 범위는 [R-U-01], provider별 실기기 결과는 [R-U-02], [R-U-03]이다. 구현 제공·휴면·예정 상태를 이전 실적에 포함하지 않는다.

근거: `src/services/discord/router/message_handler/provider_dispatch.rs:188`, `:358`, `:416`, `:435`; `src/services/turn_host.rs:199`, `:217`.

`InteractiveSessionHost`는 관측과 비파괴 입력 경계다. 생성·종료·실행 정체성·취소 라우팅까지 하나의 trait로 통합된 것은 아니다. 생성과 명시적 종료는 별도 연결 코드가 담당한다.

근거: `src/services/session_host.rs:1`; `src/services/session_host/traits.rs:7`.

모든 미확인 항목은 안정 ID `[R-U-nn]`로 참조한다. 확인 담당·방법·완료 증거는 §9.7에서 정의한다. 역할별 담당자의 개인 지정은 [R-U-19]로 관리한다.

### 9.2 책임과 생산 소비 경로

‘이전 담당’은 비교할 legacy 구현 좌표다. legacy 구현 삭제나 전체 운영 전환을 뜻하지 않는다.

**기록된 실행의 reader/view와 Codex target 화면 관측은 운영 연결됨이다. 공통 `HerdrHost` 관측 facade의 각 메서드는 구현 제공 상태이며 생산 소비자 연결 여부를 별도로 표시한다.**

#### 9.2.1 실제 소비 경로가 연결된 책임

| 책임 | 이전 담당 코드 | 생산 진입점 → 소비자 → Herdr API 및 소유자 경계 | 상태·실측 |
|---|---|---|---|
| 지속 실행용 workspace·pane 생성 | tmux 생성 `src/services/platform/tmux.rs:153`; provider 호출 예 `src/services/claude.rs:2852`, `src/services/codex.rs:2220` | provider dispatch `provider_dispatch.rs:358`, `:416`, `:435` → provider Herdr 실행기 → `src/services/herdr_launch.rs:297` → `src/services/session_host/herdr/launch_host.rs:122`의 `workspace.create`·`pane.send_input`. Herdr는 pane 실행 기반을 제공하고 AgentDesk는 실행 준비·Pending 선기록·위치·실행 증거를 소유 | **운영 연결됨**, provider 스위치 조건부. [R-U-01], [R-U-02], [R-U-03] |
| 첫 prompt·후속 입력과 키 전달 | tmux 입력 `src/services/session_host/tmux_host.rs:96`, `:104`; legacy warm 소비자 `src/services/claude_tui/hosting/host_draft.rs:29` | provider dispatch `provider_dispatch.rs:358`, `:416`, `:435` → Claude `src/services/claude/herdr_turn.rs:166`, `:611` 또는 Codex `src/services/codex/herdr_turn.rs:139`, `:362`, `:497` → `HerdrTarget` mutation gate → `pane.send_text`·bracketed paste·`pane.send_keys`·`pane.send_input` (`src/services/session_host/herdr_gate.rs:386`, `:394`, `:404`, `:418`, `:424`). 対象 검증·입력 불명확 시 hold·턴 의미는 AgentDesk 책임 | **운영 연결됨**, admission·gate 조건부. [R-U-02], [R-U-03], [R-U-11], [R-U-13] |
| 기록된 실행의 pane 존재·root/provider 프로세스 증거 조회 | legacy restart probe `src/services/discord/recovery_engine/tmux_probe.rs:28` | rehydration `src/services/discord/tui_prompt_relay/rehydration.rs:277`, `:457` → restart reader `src/services/discord/recovery_engine/herdr_reader.rs:181`, `:295` → `SocketHerdrReader::of/read_pane` (`:36`, `:51`, `:55`) → registry view `src/services/session_host/herdr_registry.rs:251` → `HerdrPaneView::read_execution` (`src/services/session_host/herdr_gate.rs:554`) → `session.snapshot` (`:565`)·`pane.process_info` (`:481`, `:575`)と OS 시작 식별. 동일 서버와 저장된 실행의 대조는 AgentDesk 책임 | **운영 연결됨**. 공통 liveness·cwd·PID facade 전체의 이전으로 집계하지 않음. [R-U-08], [R-U-09] |
| 운영 상태·은퇴를 위한 기록된 pane 조회 | legacy 상태·종료 관측 | `<ADK> herdr status`·`retire`의 CLI 경로 (`src/cli/herdr.rs:189`, `:202`, `:301`) → registry view → `HerdrPaneView::read_execution` → `session.snapshot`·`pane.process_info`. 상태 해석·종료 증명·Retired CAS·hold 해제는 AgentDesk 책임 | **운영 연결됨**. 상태 조회와 진행 턴 완료는 구분. [R-U-02], [R-U-03], [R-U-12] |
| Codex composer 화면 관측 | legacy tmux 화면 capture `src/services/session_host/tmux_host.rs:116` | Codex dispatch `provider_dispatch.rs:416`, `:433` → Codex first/followup (`src/services/codex/herdr_turn.rs:375`, `:516`) → `empty_composer/composer_ready` (`:310`, `:287`, `:293`) → `HerdrTarget::capture` (`src/services/session_host/herdr_gate.rs:440`) → capture request `src/services/session_host/herdr/contract.rs:209`, `:216` → `pane.read`. composer 준비·draft·modal 판정은 AgentDesk 책임 | **운영 연결됨**. `HerdrHost.capture_screen` 생산 연결과 별개. [R-U-03], [R-U-13] |
| dcserver 재시작 후 같은 실행에 재접속 | legacy probe·retry `src/services/discord/recovery_engine/tmux_probe.rs:28` | rehydration `rehydration.rs:277`, `:457` → restart reader `herdr_reader.rs:181` → 위 reader/view의 Herdr 관측 → source attach `src/services/discord/tui_prompt_relay/herdr_source.rs:271`, `:300`. source 재등록·cursor·불일치 시 withholding은 AgentDesk 책임 | **운영 연결됨**. 재시작 연속성 [R-U-08]. 책임 전체가 Herdr로 이전된 것은 아님 |
| 명시적 강제 종료 | tmux kill `src/services/platform/tmux.rs:399` | 운영자 route `src/server/routes/herdr_terminate.rs:37`, `:89` → `terminate_registered_herdr`·`terminate_explicit_herdr` (`src/services/discord/herdr_terminate.rs:443`, `:299`) → nonce·owner 검증 (`:315`) → target 종료 gate `src/services/session_host/herdr_gate.rs:341`, `:352` → `pane.close`. 종료 재조회·Retired 전환·hold 해제·턴 정산은 AgentDesk 책임 (`herdr_terminate.rs:417`, `:420`, `:423`) | **운영 연결됨**, 별도 스위치·운영자 권한 조건부. [R-U-12] |
| Claude provider 대화 초기화 | legacy warm draft 정리·재생성 `src/services/claude_tui/hosting/host_draft.rs:57`, `:92` | native control `src/services/discord/commands/control/native.rs:407` → `HerdrClear` → native clear 계약의 `submit` (`src/services/session_host/herdr_clear_adapter.rs:200`, `:209`) → target gate·`send_line("/clear")` (`:220`, `:226`) → `pane.send_input` (`src/services/session_host/herdr_gate.rs:404`). pane reset·kill로 대체하지 않음 | Claude **운영 연결됨**. [R-U-02]. Codex의 동일 clear 계약은 **예정**, [R-U-03]에서 지원 범위 별도 기록 |

위 표의 `provider_dispatch.rs`, `rehydration.rs`, `herdr_reader.rs`, `herdr_terminate.rs` 축약 표기는 같은 행에서 명시한 전체 경로를 가리킨다. 표의 연결 증거는 `cfg(test)` 호출을 생산 연결로 집계하지 않는다.

#### 9.2.2 공통 관측 facade — 구현과 연결을 별도 판정

`host_for(Herdr)`는 등록된 단일 local endpoint의 읽기 전용 host를 선택한다. 선택할 수 없으면 fail-closed host를 반환한다. 이 registry 제공만으로 모든 공통 관측 소비자의 이전을 증명하지 않는다.

근거: `src/services/session_host/resolve.rs:425`, `:427`; `src/services/session_host/herdr_registry.rs:48`, `:73`, `:77`, `:93`, `:101`, `:108`.

| facade 항목 | 확인한 구현 | 생산 소비자 연결 판정 |
|---|---|---|
| `presence`·pane 조회 | `HerdrHost.presence` → snapshot 관측. `src/services/session_host/herdr_host.rs:168`, `:211` | **구현 제공**. reader/view의 연결은 §9.2.1에 별도 기록. facade 자체의 생산 소비 경로는 [R-U-17]에서 확인 |
| `liveness` | snapshot·process-info를 liveness로 변환. `src/services/session_host/herdr_host.rs:181`, `:215` | **구현 제공 / 공통 provider probe에는 미연결**. Herdr target은 host ref가 없어 `Unknown`, bool probe는 `Unobservable`. `src/services/provider/session_probe.rs:197`, `:209`, `:266` |
| `execution_pid` | `pane.process_info` 결과 변환. `src/services/session_host/herdr_host.rs:276` | **구현 제공**. 기록된 실행의 process stamp 관측과 공통 PID 소비자 연결을 구분. 후자는 [R-U-17] |
| `current_working_dir` | `pane.get` 결과 변환. `src/services/session_host/herdr_host.rs:268` | **구현 제공**. 공통 cwd 소비자의 생산 연결을 이 표에서 확보하지 않았음. [R-U-17] |
| `capture_screen` | capture request와 결과 변환. `src/services/session_host/herdr_host.rs:259` | **구현 제공**. Codex의 연결된 화면 관측은 `HerdrTarget::capture` 경로다. stop 관련 화면 소비 코드가 있어도 현재 hard-false 경로의 호출을 활성 실적으로 세지 않음. [R-U-04], [R-U-17] |

공통 provider probe는 Herdr에 `HostSessionRef`를 제공하지 않는다. 이 경로의 `Unknown`/`Unobservable`을 provider 종료나 facade 이전 완료로 해석하지 않는다.

근거: `src/services/provider/session_probe.rs:197`, `:201`, `:209`, `:266`.

#### 9.2.3 interrupt와 AgentDesk에 남는 책임

| 책임 | 현재 경계 | 상태 |
|---|---|---|
| interrupt·세션 보존형 stop | raw `HerdrHost.interrupt`는 `ctrl+c`를 구현하지만 registry의 읽기 전용 host는 interrupt를 거절한다. 제품 Escape 전달은 provider-terminal settlement에 묶여 있으며 생산 settlement는 hard-false | **휴면**. [R-U-04]. 근거: `src/services/session_host/herdr_host.rs:250`; `src/services/session_host/herdr_registry.rs:64`, `:89`; `src/services/provider/cancel_token_claude_interrupt.rs:581`, `:598` |
| 같은 Claude 채널의 Herdr→legacy 전환 | launch의 Herdr 표식, Retired row witness, Claude tmux 시작 전 gate를 일관되게 전환하는 완료 기능이 없음 | **예정**. [R-U-16], 후속 노드 `i5340_legacy_return`(#5340 범위, 맥 소유). 근거: `src/services/herdr_launch.rs:346`, `:569`; `src/cli/herdr.rs:287`; `src/services/session_host/session_record.rs:12`; `src/services/claude.rs:1776`; `src/services/claude/host_gate.rs:20` |

| AgentDesk에 남는 책임 | 근거 |
|---|---|
| 실행 노드·endpoint 선택과 잘못된 노드에서의 실행 거절 | `src/services/turn_host.rs:261`, `:277`; `src/services/session_host/herdr_registry.rs:143` |
| 실행 admission과 O 준비 판정 | `src/services/turn_host.rs:287`, `:288`; `src/services/herdr_launch.rs:62`, `:72` |
| Codex hook·source-mode·daemon isolation 사전조건 | `src/services/codex/herdr_turn.rs:186`; `src/services/codex/tui_session_launch.rs:416`, `:427`, `:436` |
| 실행 정체성: owner·nonce·노드·endpoint·named session·pane·process stamp·source | `src/db/dispatched_sessions/hosted_execution.rs:33`, `:44`, `:55`, `:64`, `:74`, `:88` |
| provider transcript/rollout과 실행의 결합 | `src/services/discord/tui_prompt_relay/herdr_source.rs:154`, `:210`, `:274` |
| 요청의 terminal·배달 증거·큐 해제 | SentButUncommitted를 별도로 표현하고 강제 종료 뒤에도 AgentDesk 정산을 수행. `src/services/discord/session_relay_sink/terminal_handoff.rs:46`, `:176`; `src/services/discord/herdr_terminate.rs:417`, `:423` |
| 불명확한 입력의 hold와 복구 거절 | `src/services/claude/herdr_turn.rs:396`, `:417`; `src/services/discord/tui_prompt_relay/herdr_source.rs:281` |
| legacy 전환의 기록 보존·CAS·표식/row 일관성·중간 입력 차단 | 현재 기능 미확보. runbook §3.5의 후속 요구사항과 [R-U-16]으로 관리 |

### 9.3 실제로 줄어든 중복 코드

확인한 축소는 **legacy 소비자의 직접 조작을 공통 executor로 모은 것**이다. Herdr 전환으로 tmux 구현 전체를 삭제했다는 주장은 하지 않는다.

| 확인한 변경 | 변경 좌표와 현재 위치 |
|---|---|
| Claude warm 후속 입력의 강한·약한 draft clear 함수와 backspace 직접 전송을 `followup_support.rs`에서 제거하고 두 정책을 `host_draft`의 공통 실행 루프로 합침. 강·약 정책 차이는 유지 | 커밋 `954d7a4cc8d54f3b9e79b9bbefd8033095654b0b`, `src/services/claude_tui/hosting/followup_support.rs` 삭제 hunk `@@ -373,147 +370,3 @@`. S의 공통 루프: `src/services/claude_tui/hosting/host_draft.rs:93`, `:111`, `:117` |
| 같은 warm 경로의 직접 tmux 키·종료 조작을 host executor 경계로 이동. 현재 retire는 검증된 tmux target에서만 legacy retire를 호출 | 같은 커밋의 `src/services/claude_tui/hosting/warm_followup.rs` 및 `src/services/claude_tui/host_input.rs` diff. S의 경계: `src/services/claude_tui/hosting/host_draft.rs:53`, `:59`, `:67` |

H3 probe 이전은 이번 축소 실적으로 계산하지 않는다. 해당 변경은 호출 좌표를 session host adapter로 옮기면서 기존 probe·retry·fallback을 유지한 경계 추출이다. tmux probe 구현 제거의 증거로 사용할 수 없다.

이력 좌표: `a2808b8cb9eb732ee2f8b218ae98f8e97ffdefd9`의 `src/services/tmux_diagnostics.rs` 및 `src/services/discord/tmux_watcher/post_stream_exit.rs` diff.

코드 줄 수나 삭제량은 성공 기준으로 삼지 않는다. 위 항목 이외의 중복 축소 실적은 [R-U-18]에서 생산 소비자·변경 diff·대체 동작을 대조한다.

### 9.4 새 연결 코드와 소유자 경계

여기서 소유자는 코드 구성요소의 책임이다. 개인 담당자와 정리 이슈 좌표는 [R-U-19]로 관리한다.

| 모듈·좌표 | 역할 | 소유자 경계 |
|---|---|---|
| `src/services/session_host/traits.rs:7`; `src/services/session_host/resolve.rs:425`; `src/services/session_host/consumer_guard.rs` | 공통 host 관측·입력 경계, host 선택과 소비자 guard | AgentDesk 공통 경계. 메서드 구현 제공과 생산 소비자 연결을 구분. 생성·종료·배달 판정은 별도 책임 |
| `src/services/session_host/herdr/model.rs:180`; `src/services/session_host/herdr/wire.rs:99`, `:117`; `src/services/session_host/herdr/contract.rs` | Socket API 요청·응답·framing·결과 변환 | AgentDesk가 wire와 결과 의미를 해석하고 Herdr가 요청을 실행 |
| `src/services/session_host/herdr/transport.rs:80`, `:105`, `:143`, `:156` | local Unix socket, peer witness, 읽기 retry와 mutation 단발 전송 | AgentDesk 제어 연결. Herdr UI 클라이언트 재연결과 별개 |
| `src/services/session_host/herdr/launch_host.rs:122`; `src/services/herdr_launch.rs:297`, `:333`, `:346` | workspace/pane 생성·실행 명령, Pending 선기록과 host 표식 | Herdr는 pane 실행 기반, AgentDesk는 실행 식별·명령·준비·기록 책임 |
| `src/services/session_host/herdr_registry.rs:143`, `:176`, `:251`; `src/services/session_host/herdr_gate.rs:189`, `:302`, `:424`, `:554` | local endpoint, mutation target, 기록된 실행의 read-only view | AgentDesk가 실행 증거와 변경 권한을 검증하고 관측 결과를 해석 |
| `src/db/dispatched_sessions/hosted_execution.rs:88`, `:382`, `:423`, `:434` | hosted record와 Pending/Bound/Retired CAS | AgentDesk 저장·동시성 계약. pane ID가 실행 정체성을 대신하지 않음 |
| `src/services/claude/herdr_turn.rs:123`, `:439`, `:594`; `src/services/codex/herdr_turn.rs:113`, `:139`, `:403` | provider별 cold start·후속 입력·source 대기·결과 처리 | provider 대화·턴 의미는 AgentDesk adapter 책임 |
| `src/services/discord/tui_prompt_relay/herdr_source.rs:154`, `:210`, `:274`; `src/services/discord/recovery_engine/herdr_reader.rs:26` | native source 결합과 재시작 재조회·재등록 | 기존 source authority·binding 기록 재사용. 새 화면 기반 답변 저장소를 만들지 않음 (`herdr_source.rs:175`, `:186`, `:300`) |
| `src/services/session_host/herdr_clear_adapter.rs:200`; `src/services/discord/herdr_terminate.rs:299`; `src/server/routes/herdr_terminate.rs:37`; `src/cli/herdr.rs` | native clear, 운영자 종료, 상태·은퇴 | AgentDesk가 정책·종료 확인·정산·은퇴를 소유하고 Herdr는 허가된 pane I/O 수행 |
| `src/services/cluster/channel_home_boot.rs:97`; `src/services/discord/runtime_bootstrap.rs:589`; `src/services/cluster/intake_router_hook.rs:165`; `src/services/cluster/intake_worker.rs:550`; `src/services/session_forwarding/home_stop.rs:176` | holder·home epoch·lease/drain·기존 intake/제어 전달 | 멀티노드 배치·전달은 AgentDesk 책임. 대상 노드의 Herdr 제어는 local socket 경계 |
| `src/services/turn_host.rs:130`, `:261`; `src/services/herdr_admission.rs`; `src/config/session_hosts.rs`; `src/services/tui_o/channel_policy.rs:62` | 실행 host, admission, endpoint·writer 부팅 설정 | AgentDesk 진입 정책. 실제 설정·활성화는 [R-U-01] |
| `src/services/codex/tui_session_launch.rs:416`, `:427`, `:436` | Codex hook capability와 daemon isolation 준비 | AgentDesk cold-launch 사전조건. 실제 노드 준비 결과는 [R-U-03] |

### 9.5 이중 경로 점검

| 함께 남아 있는 경로 | 확인한 경계와 유지 이유 | 정리·확인 방법 |
|---|---|---|
| tmux/process driver와 Herdr turn driver | `TurnHost`로 분기. Herdr branch는 즉시 반환하고 다른 driver로 fallback하지 않음. 살아 있는 Herdr record가 있는 미설정 채널도 tmux로 넘기지 않음. `src/services/discord/router/message_handler/provider_dispatch.rs:188`; `src/services/turn_host.rs:91`, `:140` | 기존 모드 유지 범위로 보존. 동일 요청의 이중 실행은 [R-U-05], [R-U-08], [R-U-10]에서 실측. 저장소 전체 호출부 판정은 [R-U-17] |
| 공통 `HerdrHost` 입력 메서드와 `HerdrTarget` mutation 경로 | registry의 공개 host는 입력·키·interrupt를 거절. 제품 입력은 대상 실행 gate를 소비. `src/services/session_host/herdr_registry.rs:48`, `:81`, `:89`; `src/services/session_host/herdr_gate.rs:424` | raw host 우회와 각 facade의 생산 소비자는 **호출부 감사**로 확인. [R-U-17]. 실기기 성공만으로 우회 경로 부재를 판정하지 않음 |
| legacy warm draft 재생성과 Herdr 후속 입력 | legacy warm host는 tmux 전용. Herdr는 provider turn에서 Bound 실행으로 후속 입력. `FollowupHost::herdr` 생성자만으로 legacy 재생성 경로 이전을 인정하지 않음. `src/services/claude_tui/hosting/host_draft.rs:29`, `:40`, `:59`; `src/services/claude/herdr_turn.rs:166`; `src/services/codex/herdr_turn.rs:139` | 사용처·대체 동작을 [R-U-17], [R-U-18]에서 확인. 현재 삭제 대상으로 확정하지 않음 |
| legacy restart probe와 Herdr restart reader | legacy probe는 다른 host를 probe·retry하지 않는 계약. Herdr reader는 기록된 local pane을 읽음. `src/services/discord/recovery_engine/tmux_probe.rs:28`; `src/services/discord/recovery_engine/herdr_reader.rs:26` | 코드 경계는 [R-U-17], 단절·서버 교체 시 실제 중복 복구는 [R-U-08], [R-U-09], [R-U-10]에서 확인 |
| 세션 보존형 stop과 운영자 terminate | Escape stop은 production settlement hard-false로 휴면. terminate는 별도 스위치·권한 뒤 close·종료 확인·은퇴·정산 수행. `src/services/provider/cancel_token_claude_interrupt.rs:581`, `:598`; `src/services/discord/herdr_terminate.rs:299`, `:417` | stop 연결과 취소 계약은 [R-U-04], terminate는 [R-U-12]. stop을 pane close로 대체하지 않음 |
| 기존 intake 전달과 delegated channel home | home row가 있으면 holder·epoch로 기존 `route_to_instance` 사용. worker는 같은 epoch의 intake open을 재확인. Herdr transport는 대상 노드의 Unix socket 사용. `src/services/cluster/intake_router_hook.rs:134`, `:232`; `src/services/cluster/intake_worker.rs:550`; `src/services/session_host/herdr/transport.rs:80` | 확인한 경로는 **기존 intake → 지정 holder → local Herdr socket**. 중앙 원격 Herdr 직접 제어의 필요·부재와 불필요한 이중 경로 여부는 저장소 전체 생산 호출부 감사 [R-U-17]로 판정 |

transport의 재접속과 prompt 재전송을 구분한다. 읽기만 retry하고 mutation은 witness가 일치하는 연결에서 한 번 교환한다. 입력 ACK 유실을 prompt 재송신의 근거로 사용하지 않는다.

근거: `src/services/session_host/herdr/model.rs:206`; `src/services/session_host/herdr/transport.rs:143`, `:156`.

과거의 “lease/drain 운영 경로가 없다”, “gateway Legacy reset이 연결되지 않았다”는 설명을 S의 상태 설명으로 재사용하지 않는다. S에는 RestWorker·Standby·Gateway의 호출, 스위치 조건부 boot, `legacy_reset` 연결이 있다. 실제 활성화와 수용 결과는 [R-U-01], [R-U-05], [R-U-06]이다.

근거: `src/services/discord/runtime_bootstrap.rs:345`, `:411`, `:440`, `:589`, `:634`.

종료·retire·매핑 제거 후 같은 Claude 채널이 legacy로 복귀한다는 설명도 사용하지 않는다. 표식과 Retired row 증거를 전환하는 기능은 현재 미확보다. [R-U-16]이 해소되기 전 다른 채널의 성공으로 복귀를 완료 처리하지 않는다.

근거: `src/services/herdr_launch.rs:346`, `:569`; `src/services/session_host/session_record.rs:12`; `src/services/claude.rs:1776`; `src/services/claude/host_gate.rs:20`.

### 9.6 실기기 검증 항목과 현재 결과

모든 항목은 미확인이다. 각 결과에는 실행 SHA, 노드별 provider/host, 시험 식별자, 관측 시각, 실행 nonce와 증거 위치를 남긴다. 운영 설정·인증값을 문서에 복사하지 않는다. 담당·방법·완료 증거의 상세 계약은 §9.7을 따른다.

| 측정 항목 | 확인할 내용 | 현재 결과 |
|---|---|---|
| Claude 생성·후속 입력·응답 회수·clear·종료 | 노드 준비와 각 지원 경로의 실제 동작. 첫 gate의 결과를 모든 시나리오로 확대하지 않음 | 미확인 [R-U-02] |
| Codex 생성·후속 입력·응답 회수·종료 | O 조건, hook·endpoint·capability·`--no-daemon`, composer와 source 결합. clear 지원 범위 별도 기록 | 미확인 [R-U-03] |
| 두 Mac 동시 실행·후속 대화 | 지정 노드·cwd·provider, 동일 실행의 후속 입력, 요청·답변 혼입과 이중 실행 여부 | 미확인 [R-U-05] |
| 대상 노드 부재 | 다른 노드로 조용히 대체하지 않는지. holder·required target·대기 또는 거절 결과 | 미확인 [R-U-06] |
| UI detach/reattach·통합 UI | UI 없이 provider·결과 수집 유지, 한 클라이언트에서 양쪽 관리 세션 확인 | 미확인 [R-U-07] |
| dcserver restart | nonce·root/provider stamp·pane 연속성, 재접속, cursor 회수, 중복 생성·제출 여부 | 미확인 [R-U-08] |
| Herdr server restart | 동일 pane ID를 동일 실행으로 오인하지 않는지. 바뀐 실행 증거와 복구 판정 | 미확인 [R-U-09] |
| 네트워크 단절·원격 작업 완료 | 재연결 후 원래 transcript/rollout 회수, blind 재실행·누락·중복 배달 여부 | 미확인 [R-U-10] |
| 입력 직후 ACK 유실 | API 요청·실제 provider 제출 횟수·hold·후속 입력 혼입 | 미확인 [R-U-11] |
| 세션 보존형 stop | 생산 연결·settlement를 확보한 뒤 취소 계약 검증. 현재 hard-false를 통과 처리하지 않음 | 현재 휴면, 미확인 [R-U-04] |
| 운영자 terminate | 정확한 대상·nonce, close·생존 재조회·Retired·hold·terminal 정산 | 미확인 [R-U-12] |
| 동일 pane ID·한글·긴 여러 줄 입력 | 노드·endpoint·nonce를 포함한 대상 정확성, paste/Enter, 잘림·턴 혼입 | 미확인 [R-U-13] |
| 유지보수 부담·실제 책임 감소 | 코드 호출 증거와 세션 관리·복구 단계·대응 시간의 전후 비교. daemon·socket·source 연결 비용도 포함 | 미확인 [R-U-14] |
| 실제 개발 작업 | 지정 Mac의 프로젝트·도구·MCP로 작업하고 AgentDesk 결과 확인 | 미확인 [R-U-15] |
| 같은 Claude 채널의 legacy 복귀 | 현재 미지원. 명시적 전환 기능을 확보한 뒤 동일 채널의 전체 절차·첫 턴·후속 턴·관리 명령 검증 | 기능 미확보 [R-U-16], 후속 노드 `i5340_legacy_return`(#5340 범위, 맥 소유) |

실기기 시험은 일반 제출 중단을 유지하고 admission `open`과 준비 조건을 확인한 뒤 승인된 시험 입력으로 수행한다. 닫힌 admission에서 시험 성공을 요구하거나 stop-file 해제만으로 latch가 풀렸다고 가정하지 않는다.

근거: `src/services/turn_host.rs:287`; `src/services/session_host/herdr_gate.rs:635`; `src/services/herdr_admission.rs:53`, `:78`; `scripts/e2e/tui_relay/herdr.py:92`.

### 9.7 모든 미확인 항목의 담당·방법·완료 증거

안정 ID는 항목이 완료되어도 유지한다. 여러 검증에 같은 증거를 사용할 때는 각 ID의 완료 조건을 충족하는 부분을 명시한다. 담당 지정, 확인 방법 또는 완료 증거가 없는 ID는 완료 처리하지 않는다.

확인 담당은 역할별 책임이다. 코디네이터는 [R-U-19]에서 개인 담당자를 지정한다. 실제 증거를 확보하기 전에는 아래 표의 방법을 수행했다고 기록하지 않는다.

| 안정 ID | 확인 대상 | 확인 담당 | 확인 방법 | 완료 증거 |
|---|---|---|---|---|
| R-U-01 | 노드별 실제 활성화와 운영에서 이전된 책임 범위 | 노드 운영 담당·코디네이터 | 노드별 배포 SHA·provider/host·스위치·endpoint locality·O 준비의 비민감 요약을 실제 실행과 대조. §9.2의 책임별 이전 여부 판정 | 노드별 활성화 확인표, 실제 실행·nonce, 책임별 이전/미이전 범위와 증거 위치 |
| R-U-02 | Claude 실기기 생성·후속 입력·응답 회수·clear·종료 | Claude 검증 담당 | 승인된 시험 채널에서 cold/warm, 답변 배달, native clear, 종료·은퇴 검증. runbook §6의 첫 gate와 추가 시나리오 구분 | 시나리오별 결과, 실행·source·nonce, 요청/답변 대조, clear·종료 증거 |
| R-U-03 | Codex 준비와 실기기 실행·후속·회수·종료 | Codex 검증 담당·O 담당 | 노드별 O ownership·accepting writer·checkpoint와 cold-launch 조건 확인. checkpoint 누락·writer 미선택·hook 조건 미충족·`--no-daemon` 미지원 각각의 거절 확인 후 정상 경로 검증 | 준비표·조건별 거절 결과, 정상 first/followup·source·배달·종료 증거. 지원하지 않는 clear 범위 명시 |
| R-U-04 | stop 생산 연결·활성화·취소 계약 | 취소·terminal 계약 담당 | hard-false를 대체하는 생산 연결을 먼저 확보. 세션 보존, Escape 전달, terminal settlement, 안내 송신→durable ACK 사이 장애와 재시작·롤백 검증 | 구현 SHA와 생산 호출 경로, 취소 계약별 결과·durable 증거·회귀 결과. 현재 hard-false는 완료 증거가 아님 |
| R-U-05 | 두 Mac 동시 실행·후속 대화 | 다중 노드 검증 담당 | 양쪽 시험 채널에서 구분 가능한 요청을 동시에 수행하고 후속 입력의 node·cwd·provider·nonce와 답변 대조 | 두 노드의 요청별 실행·배달표, 후속 실행 연속성, 혼입·이중 실행 검사 결과 |
| R-U-06 | 대상 노드 부재 | intake·channel-home 검증 담당 | 지정 holder/target을 사용할 수 없는 조건에서 요청 경로 관측. 원래 요청을 유지하고 다른 노드의 대체 실행 여부 대조 | holder·epoch·required target, 대기/거절 결과, 다른 노드 실행 부재 또는 발견한 위반 증거 |
| R-U-07 | UI detach/reattach와 통합 UI | Herdr 클라이언트 검증 담당 | UI를 분리한 동안 실행·결과 수집 확인. 재연결 후 한 클라이언트에서 양쪽 관리 세션 확인 | 클라이언트 버전, 전후 서버·실행 식별, UI 관측 증거와 요청별 회수 결과 |
| R-U-08 | dcserver restart 연속성 | 복구 검증 담당 | runbook §6.2의 mark→운영자 dcserver restart→verify. nonce·pane·root/provider stamp·source cursor·제출 횟수 대조 | mark/verify 결과, 별도 재시작 증거, 동일 실행·cursor 회수와 중복 생성/제출 검사 |
| R-U-09 | Herdr server restart·실행 교체 판정 | Herdr 서버·복구 검증 담당 | 격리 서버에서 승인된 종료·재기동 수행. socket peer 시작 식별과 pane/root/provider 증거 비교. 같은 pane ID 재사용도 확인 | 전후 서버 witness·process stamp, 원래 실행 채택/거절 근거, 단일 복구 판단과 기록 보존 결과 |
| R-U-10 | 단절 중 완료와 재연결 후 회수 | 다중 노드·relay 검증 담당 | 연결 단절 중 원래 작업 완료, 재연결 뒤 transcript/rollout·cursor·배달 대조. 재실행·누락·중복 확인 | 원래 요청 식별, source 구간·제출 횟수·배달 결과 대조표 |
| R-U-11 | 입력 ACK 유실과 hold | 입력 adapter 검증 담당 | 격리 환경에서 제출 직후 ACK 유실 주입. 요청 횟수·provider 수신·hold·다음 입력을 각각 관측 | 제출 횟수 대조, hold 발생·차단·해소 근거, 재제출·입력 혼입 검사 결과 |
| R-U-12 | 명시적 terminate | 종료 작업·terminal 검증 담당 | channel·session key·nonce·pane 결합을 확인하고 종료 수행. 실패·indeterminate·연결 유실도 포함해 생존·row·hold·정산 재조회 | 유일한 대상 결합표, API 결과, 종료·Retired·hold·terminal 증거와 실패 시 재조회 기록 |
| R-U-13 | 동일 pane ID·한글·긴 여러 줄 입력 | 입력 adapter·다중 노드 검증 담당 | 양쪽 서버의 같은 pane ID 조건에서 endpoint·nonce를 구분. 한글·여러 줄 입력, paste/Enter, 잘림·혼입 확인 | 서버·endpoint·nonce별 송수신 대조와 대상 정확성·입력 완전성 결과 |
| R-U-14 | 유지보수 부담·실제 책임 감소 | 운영 책임자·코드 감사 담당 | 같은 종류의 작업으로 전후 관리·재접속·복구 단계와 대응 시간 비교. legacy 생성·입력·kill 호출 여부는 호출부 증거로 대조 | 비교 범위·조건, 단계·시간 기록, 생산 호출 경로와 추가 daemon/socket/source 비용을 포함한 평가 |
| R-U-15 | 실제 프로젝트 개발 작업 | 각 노드의 프로젝트 담당 | 지정 Mac에서 실제 프로젝트·도구·MCP를 사용한 개발 작업 수행, node·cwd·provider·결과 회수 확인 | 작업 식별, 비민감 도구·MCP 성공/실패, 결과와 AgentDesk 배달 대조 |
| R-U-16 | 같은 Claude 채널의 안전한 legacy 복귀 | 전환 기능 담당·독립 검수 담당 | runbook §3.5의 배출·종료, owner/nonce·Retired CAS, 이전 기록 보존, 표식/row 일관 전환, 중간 입력 차단 구현 후 **동일 채널·실행 식별**의 전체 복귀·첫 턴·후속 턴·관리 명령 검증. 표식 전환 누락과 다른 채널 성공 대체 뮤턴트 검증 | 후속 이슈·구현 SHA, 계약별 증거, 동일 채널 전체 결과와 두 뮤턴트 실패. 현재 기능 없음·`[후속 이슈 미발급]` |
| R-U-17 | 저장소 전체 생산 이중 경로·raw host 우회·facade 소비자 연결 | 코드 감사 담당·독립 검수 담당 | 고정 SHA에서 `git grep`로 host selector, `host_for`, raw `HerdrHost`, `HerdrTarget`, provider dispatch, restart, intake/forwarding 호출을 찾고 `git show`로 caller를 추적. `cfg(test)`를 제외하고 스위치·hard-false·호출 가능성을 구분해 진입점→소비자→API 지도 작성 | 감사 SHA·검색 범위·호출부 목록·도달성 판정, 책임별 경로표, 중복의 유지 이유·정리 대상. 발견 항목의 이슈 좌표 또는 `[후속 이슈 미발급]`. twomac 성공만으로 완료 금지 |
| R-U-18 | 확인한 warm 통합 외의 중복 축소 | 코드 정리 감사 담당 | 변경 전후 `git diff`와 S의 생산 호출부 대조. 삭제·통합된 책임과 대체 경로 및 유지해야 할 정책 차이를 확인 | 변경 SHA·diff hunk·대체 소비 경로·동작 검증 연결표. 삭제량만으로 완료 금지 |
| R-U-19 | 개인 담당자와 추가 정리 이슈 좌표 | 코디네이터·운영 책임자 | 코드 소유자 역할과 실제 운영 담당자를 지정·대조. 미확인 ID별 개인 담당·증거 관리 책임을 지정하고 정리 항목의 실제 이슈 좌표 확인. 미발급은 그대로 표기 | 담당 지정 기록, ID별 담당·역할표, 실제 이슈 좌표 또는 `[후속 이슈 미발급]` 및 추적 책임. 실기기 결과로 대체 금지 |
| R-U-20 | #5071 선행조건 충족 | 선행 작업 담당·코디네이터 | #5071의 수용 기준과 완료 근거를 확보해 기준별로 구현·배포·검증 증거 대조. 이번 정적 검토에서 확보하지 못한 이슈 내용은 추정하지 않음 | #5071 수용 기준별 증거표, 관련 SHA·검증 결과와 선행 충족 판정 기록. 기준 또는 증거 미확보 시 미확인 유지 |
| R-U-21 | #5340 전체 완료 | 코디네이터·독립 수용 검수 담당 | #5340 수용 기준·DAG 요구사항별 증거 대조. [R-U-20] 선행조건, 실제 책임 범위, 실기기, 호출부 감사, 운영 문서와 [R-U-16] 복귀 계약을 확인. 초안 승인을 기능·운영 완료와 구분 | 수용 기준별 pass/block/해당 범위 밖 판정과 근거, 필수 미확인 항목 해소, 독립 검수 기록. 같은 채널 복귀 기능 미확보 상태에서는 해당 수용 기준 완료 금지 |

### 9.8 문서 검수와 완료 판정

문서 검수는 다음 조건을 확인한다.

1. 모든 미확인 참조에 안정 ID가 있고 §9.7에 대응하는 행이 있다.
2. 각 행에 확인 담당·방법·완료 증거가 있다. 방법이나 증거가 없는 ID를 완료 처리하지 않는다.
3. 생산 연결 실적은 시험 호출이나 메서드 존재가 아니라 진입점→소비자→Herdr API 경로로 설명한다.
4. 코드 호출부 감사, 개인 담당자 지정, 선행조건·전체 수용 판정을 twomac 실측 성공으로 대체하지 않는다.
5. 같은 Claude 채널의 복귀는 현재 미지원으로 남긴다. 종료·retire·매핑 제거 또는 다른 채널의 성공을 완료 증거로 인정하지 않는다.
6. 기능과 검증이 확보되면 해당 ID의 증거를 갱신하고 ID·기준 SHA·판정 이력을 보존한다.

이 기록은 코드 책임 경계와 확인 계약의 초안이다. 문서 초안의 준비 완료는 실제 운영 책임 이전 [R-U-01], twomac 수용 [R-U-05]–[R-U-13], 저장소 전체 이중 경로 감사 [R-U-17], 선행조건 [R-U-20], #5340 전체 완료 [R-U-21]의 승인과 별개다.
