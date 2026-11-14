# 독립 시험 인스턴스 선언

`scripts/dev-instance.sh`는 I1의 파일 준비·입력 검증만 수행한다. `up`, PG 기동,
Discord 접속, provider 실행, cleanup 명령은 아직 없다. 성공한 선언도
`status=DECLARED`, `live_ready=false`이며 실제 제품 시험 PASS가 아니다.
I2/I3의 실행 경계·수명·identity read-back이 붙기 전에는 이 config로 daemon을 실행하지 않는다.

```sh
scripts/dev-instance.sh prepare /absolute/clean/worktree \
  --profile /absolute/non-secret/profile.json
scripts/dev-instance.sh validate --instance <canonical-uuid>
```

root는 `$HOME/.adk/dev/instances/<canonical-uuid>`로 고정된다. root·config·DB
주소를 프로필이나 argv로 override할 수 없다. 시험 포트 풀은 API 18791–18806,
PG 25432–25447이며 loopback만 사용한다. prepare/validate 시 점유를 확인하지만
예약하지 않으므로 launcher가 bind 직전 다시 확인해야 한다. PG data/socket/DB/role은
인스턴스별 선언이며 실제 cluster 생성·전용 권한 확인은 후속 단계 책임이다.
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

허용 파일명은 `claude_bot_token`, `codex_bot_token`, `driver_bot_token`뿐이다.
PATH 첫 디렉터리의 레인 바이너리 `agentdesk dev-instance credential-write`가 stdin을
받아 기존 Rust `write_secret_file`의 공유 정책·저장 코어로 쓴다. 디렉터리0700/
파일0600·fsync 및 fresh leaf를 적용하며 Python 직접 쓰기 fallback은 없다.
바이너리가 없으면 `BLOCKED_BINARY`(exit 4)다. root/ancestor/leaf symlink·hardlink credential·
외부 절대경로·unknown credential·legacy credential subtree를 거부한다. 기존
reader의 migration은 사용하지 않는다. 실행 후 제품이 만드는 legacy symlink의
처리는 I2/I3 startup 경계에서 별도로 정해야 한다.

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

**아직 live 가능하지 않다.** HOME/provider·label/API/Memento·프로세스 scan과
복원/clear/cancel 연결 완결은 I2b, lease·실제 up/down과 health 확인은 I3a 책임이다.
현재 선언만으로 daemon/provider/Discord를 기동하지 않는다.
