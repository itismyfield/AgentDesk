# Worker 실행 용량과 자동 배정

이 기능은 검증 중이며 아직 운영 노드에 적용하지 않았다. 같은 바이너리에서 node별
slot 수와 자동 배정 여부를 설정한다. 별도 scheduler 서비스나 역할별 프로그램은 없다.

```yaml
cluster:
  enabled: true
  instance_id: windows-worker-1
  role: worker
  runtime_profile: worker
  execution_slots: 2
  intake_routing:
    enabled: true
    mode: enforce
    capacity_aware: true
```

`execution_slots`는 1~1024의 정수이며 변경 후 재시작해야 한다. 실제 장비에서 측정한
동시 실행 수를 넣는다. CPU 코어 수를 그대로 사용하지 않는다. `capacity_aware`는
새 channel의 적합한 worker를 점유율 → 최근 배정이 오래된 순서 → instance ID로 고른다.
비율을 비교하므로 slot 2개와 8개인 장비를 같은 용량으로 취급하지 않는다.
현재 자동 배정은 worker를 우선하고 leader는 적합한 worker가 없을 때 후보가 된다.

기존 provider·필수 OS/architecture·도구·repository·인증 프로필·첨부 소비 능력 검사를
먼저 통과해야 한다. version 1 용량을 광고하지 않는 구버전 노드는 자동 배정 집합에
포함하지 않는다. 고정 `/node`와 살아 있는 session owner는 그대로 유지한다.

## 예약과 실행의 수명

- `intake_outbox`의 열린 전달 행은 실행 전 예약이다. live ingress, local owner 기록,
  자동 재시도, 운영자 재시도는 PostgreSQL trigger를 통해 같은 상한을 적용한다.
- `node_execution_leases`는 실제 provider 호출을 감싼다. 동일 provider/channel의
  예약과 실행은 `UNION`으로 하나만 계산한다. 전달이 완료돼도 실행 lease는 남는다.
- DB advisory lock으로 같은 node에 대한 용량 확인과 삽입을 직렬화한다. 화면에
  표시한 점유 스냅샷만으로 상한을 보장하지 않는다. 선택 후 경쟁에서 slot을 잃으면
  다른 적합한 후보를 시도한다.
- 실행 lease는 30초이고 10초마다 갱신한다. DB 확인은 5초 이내에 끝내며 갱신을
  확인하지 못하면 기존 CancelToken의 프로세스 정리 경로로 실행을 취소한다.
- 프로세스 내 semaphore는 provider 함수가 반환할 때까지 유지한다. DB lease가
  만료되어도 살아 있는 동일 프로세스의 실행 수를 상한 이상으로 늘리지 않는다.
- lease nonce가 일치할 때만 갱신·해제한다. 오래된 실행의 정리가 후속 실행이나
  다른 worker의 lease를 지우지 않는다. 정상 종료·시작 실패·panic에서는 RAII로
  반환하고, 프로세스가 강제 종료되면 DB 만료와 기존 outbox 복구가 작동한다.

가득 찬 고정 owner를 다른 OS로 이동하지 않는다. 신규 입력은 실행하지 않고 이유를
알리며, 이미 durable queue에 있던 입력은 기존 재시도 정책을 따른다. provider 실행
직전 상한에 도달한 경우에는 provider를 시작하지 않고 오류를 전달한다.

## 적용 범위와 관측

공통 `provider_dispatch::execute`를 지나는 Discord intake와 headless turn에 적용한다.
별도 독립 CLI 명령, 운영자가 직접 실행한 프로세스, provider 외의 background 작업을
노드 전체 프로세스 상한으로 제한하는 기능은 아니다. 실행 중인 turn과 대기 중인
TUI 세션은 다르며 idle 세션만으로 slot을 차지하지 않는다.

노드 API는 `execution_active`와 `execution_occupied`를 반환한다. 각각 실제 실행
lease 수와 전달 예약을 합친 점유 수이다. 기존 `active_dispatch_count`는 전달 작업
관측값으로 유지한다. 계정 quota는 별도 병목이며 worker 증설로 해결된다고 가정하지 않는다.

instance ID는 fleet에서 유일해야 한다. 같은 ID로 두 프로세스를 띄우는 구성은 지원하지
않는다. PostgreSQL 단절 시 새 작업을 거절하고 기존 작업을 취소하는 정책이다.
여러 호스트의 절대적인 프로세스 정지 시점까지 보장하는 분산 하드웨어 fencing은 아니다.

검증 대상은 동시 예약 상한, 전달/실행 중복 계산 방지, 만료·재시작 후 nonce fencing,
다른 node의 lease 독립성, 사용률과 동률 순서, 기존 owner 및 플랫폼 조건 보존이다.
