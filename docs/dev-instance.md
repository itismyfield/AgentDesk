# 독립 시험 인스턴스 선언

`scripts/dev-instance.sh`는 파일 준비·검증, 명시적으로 승인한 격리 runtime 기동,
종료·삭제를 제공한다. 준비만 한 선언은 `status=DECLARED`, `live_ready=false`다.
실제 live readiness는 네 시험 앱의 identity와 실행 중 health를 대조한 뒤 게시한다.

```sh
scripts/dev-instance.sh prepare /absolute/clean/worktree \
  --profile /absolute/non-secret/profile.json
scripts/dev-instance.sh validate --instance <canonical-uuid>
```

root는 `$HOME/.adk/dev/instances/<canonical-uuid>`로 고정된다. root·config·DB
주소를 프로필이나 argv로 override할 수 없다. 시험 포트 풀은 API 18791–18806,
PG 25432–25447이며 loopback만 사용한다. prepare/validate 시 점유를 확인하지만
예약하지 않으므로 launcher가 bind 직전 다시 확인해야 한다. PG data/socket/DB/role은
인스턴스별 선언이며 `up --resources`의 keeper가 cluster 생성·전용 권한을 확인한다.
상속 `DATABASE_URL`·`PG*`는 빈 값이어도 거부한다.

프로필은 JSON이며 아래 예시의 다섯 ID를 준비된 **시험 앱/채널 ID**로 바꾼다.
운영 env 파일을 읽거나 복사하지 않는다. 아래 ID는 합성 예시이며 Discord identity
확인 증거가 아니다. 세 앱 ID 및 두 채널 ID는 서로 다른 양의 u64 문자열이다.

```json
{
  "schema_version": 1,
  "name": "tui-live",
  "bots": {"claude": "900001", "codex": "900002", "driver": "900003"},
  "channels": ["900004", "900005"],
  "features": {
    "env": {
      "ADK_BUSY_INJECT": "all",
      "ADK_BUSY_INJECT_DISCORD_CHANNELS": ["900004", "900005"],
      "ADK_BUSY_INJECT_STASH_CHANNELS": [],
      "ADK_QUEUED_HEAD_INJECT": "1",
      "ADK_TURN_PRESENCE_TAIL": "1",
      "ADK_INTAKE_ROUTING_MODE": "enforce",
      "ADK_IMSG_OBLIGATIONS": "0"
    },
    "tui_writer": true,
    "tui_turn": true,
    "capacity_aware": true,
    "preferred_node": "local"
  }
}
```

비밀 아닌 기능 env도 위 일곱 키만 허용한다. 채널 플래그는 시험 채널의 부분집합만
허용하며 wildcard를 거부한다. tui writer/turn과 capacity_aware는 실제 config
필드로 생성한다. preferred execution node는 DB 값이므로 config에 가짜 필드를
추가하지 않고 `preferred_node_seed_required=true`로 기록한다. 후속 seed는 receipt의
`execution_node_id`를 사용해야 한다. 플래그 누락 및 실제 witness 누락/불일치/필수
기능 off는 exit 3, `UNEVALUABLE(precondition)`이며 제품 RED에 합산하지 않는다.

`manifest.json`과 비밀 없는 `receipt.json`에 canonical root, clean worktree/HEAD,
API/PG 선언, 앱/채널 ID, 적용 env와 config 기능값을 기록한다. validate는 정확한
config 파일과 manifest/receipt를 재구성해 비교하므로 파일 누락, sibling fallback,
DSN·경로·inline token 추가를 거부한다. receipt의 적용값은 **생성된 선언값**이다.
실행된 daemon의 적용 확인은 별도 precondition witness로 받아야 한다.

```sh
scripts/dev-instance.sh validate --instance <uuid> --witness /absolute/witness.json
```

witness는 `instance_id`, `head`, `applied_env`, `features`, `host`, `execution_node`를
모두 포함한다. `host`는 현재 I1에서 `local`, `execution_node`는 receipt의
`execution_node_id`다. witness 검사는 관측값의 일치 확인이며 관측/실행 자체를
만들지 않는다. 임의 JSON을 증거로 생성해서 live PASS로 처리하면 안 된다.

credential은 stdin만 받는다. 운영 credential을 복사하지 않는다. 공급자가 준비된
시험 비밀을 pipe로 보내도록 연결하며 값은 argv·env·manifest·receipt에 넣지 않는다.

```sh
scripts/dev-instance.sh credential --instance <uuid> --name claude_bot_token < /secure/test-input
scripts/dev-instance.sh validate --instance <uuid> --require-credentials
```

허용 파일명은 `claude_bot_token`, `codex_bot_token`, `driver_bot_token`,
`announce_bot_token`, `notify_bot_token`이다. live admission은 claude·codex·announce·notify
네 credential을 요구한다. driver_bot_token은 기존 선언 검증용이다.
PATH 첫 디렉터리의 레인 바이너리 `agentdesk dev-instance credential-write`가 stdin을
받아 기존 Rust `write_secret_file`의 공유 정책·저장 코어로 쓴다. 디렉터리0700/
파일0600·fsync 및 fresh leaf를 적용하며 Python 직접 쓰기 fallback은 없다.
바이너리가 없으면 `BLOCKED_BINARY`(exit 4)다. root/ancestor/leaf symlink·hardlink credential·
외부 절대경로·unknown credential·legacy credential subtree를 거부한다. 기존
reader의 migration은 사용하지 않는다. 정리도 링크를 따라가지 않으며 소유권이 불확실하면 root를 보존한다.

`child_environment()`는 고정 PATH/locale 및 검증된 시험 선언으로 새 env를 만든다.
Discord token·agent-op 자격·Memento key 및 임의 ADK env를 상속하지 않는다.
HOME/provider 인증, tmux/Herdr socket, launchd label, 공유 Memento 접근 차단의
실행 증거는 I2 범위다. 같은 UID의 임의 파일읽기를 막는 OS sandbox가 아니다.

## 호스트 선언과 기동 전 경계

manifest와 receipt의 `hosts`는 전용 tmux socket·tmpdir·세션 prefix 및 Herdr
endpoint·home·session을 함께 고정한다. 실행 node ID와 cluster/preferred node는
접두 없는 instance UUID로 같다. 자식 env의 `AGENTDESK_TEST_MANIFEST`가 선언을
지정하면 바이너리는 일반 명령 처리 전에 정확한 root/config/receipt와 대조하고
프로세스 동안 namespace를 고정한다. marker를 나중에 제거해도 격리가 풀리지 않는다.

tmux는 `runtime/tmux/socket`을 명시적으로 선택하며 `AdkTest-<uuid>` namespace만
사용한다. 기존 세션·pane ID·다른 socket·전역 server 정리는 거부한다. Herdr는
`legacy_dedicated` endpoint만 선언하고 개인 HOME/XDG 경로와 다른 `herdr-home`을
사용한다. 자동 기동과 기존 workspace 채택은 차단한다.

수동 Herdr 소유 helper는 직접 spawn한 child handle과 시작 identity를
`runtime/herdr-owner.json`에 기록한다. receipt만으로 다른 PID를 정리하지 않는다.
기동 실패 정리가 끝나지 않으면 오류가 소유 child handle을 보존한다. supervisor가
이를 이어받아 종료를 확인해야 한다. Python close는 create와 동일한 kernel flock을
획득하고 immutable daemon 항목과 현재 시작 identity를 대조한다.

제품 Herdr transport는 owner receipt의 전용 socket·daemon PID/시작 identity와
실제 peer가 일치할 때 ping과 자기 create를 허용한다. create는
`runtime/herdr-owner.lock`을 유지하고 반환된 workspace/pane 쌍을 atomic 게시한
뒤에만 해당 pane 입력으로 진행한다. 개별 pane 조회·입력·close는 기록된 pane만
허용하며 전체 snapshot·기존 workspace 채택·residue 정리는 거부한다.
canonical Herdr config는 prepare 단계에서 만들며 수동 spawn 전에 동일 bytes와
1초 preparation margin을 확인한다. margin 미충족은 `UNEVALUABLE`이다.
실제 E7 provenance/age/read-back 통과는 supervisor의 별도 수용 조건이다.

## 명시적 기동과 정리

깨끗한 worktree의 HEAD와 PATH 첫 디렉터리의 레인 바이너리 SHA256을 지정한다.
resources JSON은 `postgres_bin`, `initdb_bin`, `psql_bin`, `herdr_bin`의 안전한 절대경로와
`startup_timeout`, `stop_grace`, `max_aux=2`, `pg_pool=4`, `pg_reserve=1`을 선언한다.
wall 상한은 600초, stop grace 상한은 30초이며 resources의 grace와 일치해야 한다.
stdin 첫 줄에 아래 형식의 비밀 없는 admission JSON을 미리 넣어 live 검증을 명시한다.
네 application ID는 서로 달라야 하고 준비한 시험 앱과 일치해야 한다.

```json
{"kind":"admission","schema":1,"identities":{"claude_bot_token":"900001","codex_bot_token":"900002","announce_bot_token":"900006","notify_bot_token":"900003"}}
```

```sh
scripts/dev-instance.sh up --instance <uuid> --head <sha40> \
  --binary-sha256 <sha256> --wall-timeout 420 --stop-grace 20 \
  --resources /absolute/resources.json < /absolute/admission.json
# 다른 셸에서 준비한 시험 인스턴스의 턴 하나를 관측한다.
python3 -E -s -m scripts.dev_instance.driver --instance <uuid> --provider codex --timeout 180
scripts/dev-instance.sh down --instance <uuid>
```

`up`은 foreground에서 Gateway 하나와 keeper가 소유한 PG·Herdr만 실행한다.
완료 시 `STOPPED`, 기한 초과 시 `TIMEOUT`, 회수 불확실 시 `HOLD`를 반환한다.
`runtime/instance-run.json`의 verified/live_ready는 현재 실행의 검증 결과다.
Discord identity와 Driver는 공통 `DiscordBot (https://github.com/itismyfield/AgentDesk, 0.1)`
User-Agent를 사용한다.

`down`은 살아 있는 keeper에 기존 nonce/socket을 대조한 종료 요청을 보내고 원래
핸들의 회수를 기다린다. supervisor가 죽은 기록 PID는 기존 root/cwd 검사로만 회수한다.
자식 종료·keeper 종료·pool lease 해제를 확인한 뒤 credential을 포함한 canonical
`$HOME/.adk/dev/instances/<uuid>` 하나를 삭제하고 `REMOVED`를 반환한다.
root 밖 경로, release 경로, symlink, hardlink 및 소유하지 않은 특수 파일을 거절한다.
종료/소유권이 불확실하면 `HOLD`(exit 6)로 파일을 보존한다. 변경된 선언이나 worktree도
검증을 통과해야 한다. 재기동과 경합하지 않도록 pool의 고정 close-only lease inode는
삭제하지 않으며 잠금만 해제한다. 다른 인스턴스와 운영 자원은 삭제 대상이 아니다.
