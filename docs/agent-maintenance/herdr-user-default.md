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

config는 socket peer의 실제 HOME/XDG/config 환경과 실행 파일 identity를 확인한 뒤 선택한다. 기본 config와 다른 override, 지원하지 않는 서버, 불명확한 peer는 거부한다. 지원 서버는 protocol 22의 0.9.3 이상 안정판이다(E7과 같은 `supported_herdr_server`). 거부되면 `reason`은 `next_restart_policy_unverified:Unsupported`이고, 진단의 `server`에 그 socket이 마지막 ping에 답한 버전·protocol이 남는다. macOS loaded-image identity adapter는 native arm64 단일 아키텍처·기본 pagezero 레이아웃·APFS의 실행 파일만 판정한다. 근거는 매핑 vnode `(dev, ino)`, 한 관측 안의 같은 exec 세대, 실행 파일 ctime(ns) ≤ 서버 시작(µs)이다. 그 밖의 레이아웃·파일시스템, 서버 시작 뒤 실행 파일 변경, 시작 µs 안의 순서 불명은 원인별 `reason`으로 거부한다. 설치 경로의 stat이나 버전 문자열로 이 판정을 풀지 않는다.

설정 validator는 실행 노드가 이 노드(`cluster.instance_id`가 없거나 비어 원격임을 확인할 수 없으면 이 노드로 본다)이고 이 플랫폼에 loaded-image adapter가 없을 때(linux·macOS 밖)만 UserDefault를 `이 플랫폼 미지원`으로 거부한다. 다른 노드의 endpoint는 그 노드의 부팅 검증이 판정한다. 이 해제는 코드 동작일 뿐이다. 운영 설정의 `mode`는 기본값 `legacy_dedicated`로 남으며, 바꾸는 일은 카나리 승인 사항이다.

## 동의와 진단

실행 사용자의 config에서 restore resume를 false로 만드는 동의는 owner만 결정한다. 아니오·만료·오류에서도 원 입력과 업로드는 기존 custody에 남는다. `!herdr consent retry`는 새 질문 또는 미완료 복구를 요청한다. 카드 없이 보류된 입력에는 원 메시지에 원인별로 안내가 한 번 붙는다(프로세스 재시작을 넘는 중복 억제는 보장하지 않는다). 안내는 그 요청이 관측한 `reason`/`action`과 안정 오류 코드(예: `consent_store_unavailable`)만 담는다. 경로가 든 상세는 실행 노드 로컬 로그에서 같은 코드로 찾는다. 중복 클릭·release는 동일 내구 키로 억제한다. worker 기록 전 오류는 같은 exact row의 기존 Pending 경로로만 돌아가며, 기록을 시도한 뒤의 오류는 Spawned로 보존하고 원 메시지에 row id를 알린다.

`agentdesk doctor --json`과 `/api/health/detail`의 `herdr`를 확인한다. endpoint별 `next_restart_ready`, `startup_proof`, `reason`, `action`, 제한된 checker stdout/stderr, candidate/applied mapping과 migration 진단을 구분한다. health는 마지막 관측을 읽기만 하며 현재 파일을 재검증하거나 create 권한을 주지 않는다. 마지막 fence가 실제 권한을 다시 확인한다.

현재 false가 검증돼 fresh가 준비되어도 `startup_proof=awaiting_server_restart`일 수 있다. 즉시 launch를 위해 서버를 재시작하지 않는다. true/invalid로 바뀌면 신규 create는 막히지만 같은 witness와 정확한 execution의 이미 승인된 입력·Cancel·Terminate 허가는 별도 증거로 유지된다.

## 카나리 순서

1. 코디네이터가 실행 노드·사용자·시험 채널·rollback 설정을 별도 승인한다. 이 문서는 운영 설정 변경을 승인하지 않는다. 첫 카나리는 시험 채널 intake를 실행 노드 gateway가 처리하는 로컬 경로로 한정한다.
2. 실행 노드에서 실행 사용자로 `agentdesk herdr probe-user-default [--socket <절대경로>]`를 돌린다. 읽기 전용이며 설정·DB·서버를 건드리지 않는다. exit 0은 검사가 돌았다는 뜻일 뿐이다. 판정은 출력 JSON으로 한다.
   - 진행: `next_restart_ready`가 `true`이거나, `reason`이 `next_restart_resumes`(동의 카드로 해소 가능)일 때만.
   - `next_restart_image_changed_since_start`: owner가 herdr 재시작 여부를 결정한 뒤 다시 검사한다. AgentDesk는 재시작하지 않는다.
   - `next_restart_image_layout_unsupported`: 중단한다. 그 밖의 `reason`은 `action`대로 고친 뒤 다시 검사한다.
   - exit가 0이 아니면(HOME 없음, 상대 socket 등) 검사가 돌지 않은 것이므로 준비되지 않은 것으로 본다.
3. 새 설정을 배포하기 전 live row와 unresolved consent hold가 이동 대상에 없는지 확인한다. migration 거부와 restart_required를 구별하고 old applied mapping이 유지되는지 확인한다.
4. owner가 동의 카드의 경로·preview를 확인한 뒤 예를 선택한다. 원본 backup과 false 전체 config 검증을 확인한다. 부팅 직후 doctor는 `not_observed`이므로 첫 관측을 일으킨 뒤 본다. 첫 입력과 다음 FIFO 입력을 확인하고 draft/modal/busy 상태에서 paste/Enter가 없는지 확인한다.
   실제 Claude cold start 한 번에서 SessionStart hook 수신부터 빈 ready composer marker 관측까지의 지연을 기록한다.
5. 실패 시 원 입력을 지우거나 서버를 재시작하지 않는다. `reason/action` 및 row id를 보존한다. 정리되지 않은 참조는 clear 성공으로 가정하지 않는다. 보류 입력이 Applied 동의에 있으면 `!herdr consent retry`로 해제하고, Denied·Expired·ApplyFailed면 아래 반환 절차를 쓴다.

## 보류 입력 반환(`!herdr consent return`)

owner가 실행 노드 gateway의 해당 채널에서 실행한다. 원격 gateway에서는 실행 노드에서 다시 실행하라는 안내만 하고 상태를 바꾸지 않는다.

1. 설정을 그 채널의 원래 `legacy_dedicated` endpoint로 되돌리고 재시작한다. health의 migration이 `session_hosts_migration_refused`이고 blocker가 `channel <id> unresolved hold`뿐인지 확인한다(live row blocker가 있으면 반환하지 않는다).
2. `!herdr consent return` → **반환 예약**. 응답은 `입력 N건을 legacy 대상 ... 반환 예약했습니다`다. 이때는 아직 아무것도 실행하지 않는다.
3. 재시작 → 부팅이 legacy 승인을 저장하면서 반환 입력을 **채택**한다. 원 입력은 기존 FIFO 순서로 처리된다.
4. 원 입력이 처리된 뒤 legacy 채널에서 같은 `!herdr consent return` → **종료**. 응답은 `반환 종료 N건`이다. 아직 처리되지 않은 반환은 처리된 뒤 다시 실행한다. 종료한 반환은 이후 UserDefault 재전환을 막지 않는다.

거부 응답과 뜻:

- `owner 결정이나 만료 뒤에 반환하세요.`: 동의가 아직 Pending이다.
- `설정을 적용하는 중입니다.`: Accepted 적용 중이다. 결과를 기다린다.
- `이미 승인된 동의입니다. !herdr consent retry로 해제하세요.`: Applied 동의다. 이미 반환 예약된 입력이 남아 있어도 다시 반환하지 않는다. retry로 해제한다.
- `이번 부팅의 migration은 이 채널의 보류 입력만으로 거부되지 않았습니다`: live row 등 다른 사유가 있다. 상태는 바뀌지 않는다.
- `부팅 뒤 session_hosts 설정이 바뀌었습니다`: 재시작한 뒤 다시 실행한다.

복구: 채택 기록이 저장되지 못한 채 종료 전에 legacy 매핑을 다른 endpoint로 바꾸면, 반환 입력이 현재 승인 매핑과 달라 종료되지 않고 UserDefault 재전환이 계속 보류된다. 이때는 원래 legacy 매핑으로 다시 부팅하고 같은 `!herdr consent return`으로 종료한다. clear로는 풀리지 않는다.

E 구현 레인의 읽기 전용 전제 조사는 보고서에 남긴다. 해당 관측에서 사용자 기본 socket은 있었지만 `~/.local/bin/herdr`는 없었다. Homebrew 설치 경로의0.9.3 문자열은 peer 증명이 아니다. 실제 사용자 서버나 운영 설정을 시험 대상으로 사용하지 않았다. T19는 시험 시작 때 해시를 잰 scratch 사본과 fake provider만 사용하는 별도 scratch에서 수행한다.

드문 drain 중 release 창과 startup queue merge 재시도 뒤 재생은 후속 #7261이다. 이 후속 상태를 자동 성공으로 처리하지 않는다.
