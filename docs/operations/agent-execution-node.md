# 에이전트별 Discord 기본 실행 노드

leader/worker는 AgentDesk **노드의 운영 역할**이다. Discord 에이전트는 기존 역할과
채널 연결을 유지하면서 새 세션을 시작할 기본 장비를 선택할 수 있다. 여러 worker를
구분하기 위해 `leader` 또는 `worker`라는 문자열 대신 안정적인 `instance_id`를 저장한다.

대시보드의 에이전트 상세 → **우선 실행 장비**에서 등록된 노드를 선택하고
저장한다. 목록은 hostname, leader/worker 역할, OS를 함께 보여준다. 등록 정보가
없어진 기본 노드도 설정값에는 유지하며 명시적으로 해제할 수 있다. 실제 새 세션은
우선 장비를 사용할 수 없으면 준비된 다른 호환 장비에 배정할 수 있다.

중앙 PostgreSQL `agents.default_execution_node_id`가 기본값의 정본이다.
worker의 로컬 YAML reseed는 이 값을 쓰지 않는다. API는 full profile에서만 제공한다.

```http
GET /api/agents/{id}/execution-node
PUT /api/agents/{id}/execution-node
Content-Type: application/json

{"default_node_id":"windows-worker-1"}
```

`{"default_node_id":null}`은 기본값을 해제하고 기존 배정 정책을 사용한다. 필수
OS·도구·저장소 조건은 별도의 `execution-requirements` API에 유지한다. 기본 노드를
변경해도 필수 조건을 삭제하거나 기존 세션을 강제 종료하지 않는다.

배정 우선순위는 다음과 같다.

1. 기존 live session owner.
2. 현재 채널/스레드의 명시적 `/node` 선택.
3. 준비 상태·필수 조건·실행 여유를 충족하는 에이전트의 우선 실행 노드.
4. 우선 노드가 불가능하면 같은 조건을 충족하는 다른 노드. 선호 label과 여유 용량으로 선택한다.

우선 노드가 없는 에이전트는 종전 label/전역 자동 배정 정책을 그대로 사용한다.
우선 노드를 지정한 에이전트만 전역 자동 배정의 활성화 여부와 무관하게 이 대체 배정에
참여한다. 우선 장비로 leader를 선택하면 준비된 worker가 있어도 leader를 먼저 사용한다.

직접 에이전트에 연결된 채널은 자신의 설정을 사용한다. 직접 연결이 없는 Discord
스레드는 실제 Discord metadata에서 확인한 부모 채널의 정책을 사용한다. 일반 채널의
category를 부모 에이전트로 취급하지 않으며, 기존 `threadInherit: false`는 유지한다.
부모 정책을 확인하지 못하면 정책이 없는 것으로 간주해 실행하지 않는다.
소유권·중복 방지·응답 대상은 항상 실제 스레드 ID를 사용한다.

기본 노드는 새 세션 배정에 적용된다. Mac이 소유한 기존 대화에서 기본값을 Windows로
바꿔도 다음 입력은 Mac에서 처리한다. 새 스레드/새 세션부터 Windows를 선택한다.
살아 있는 터미널, provider session 파일, 로그인 또는 작업 폴더를 OS 사이에 복제하는
기능은 아니다. worker에 해당 agent의 prompt·bot 설정·CLI 계정과 작업 경로를 준비해야 한다.

대체 배정은 **소유자가 없는 새 세션**에 적용한다. heartbeat만으로 실행 가능하다고
판단하지 않고 provider CLI·선택 계정의 준비 상태, 실행 poller, 필수 OS·도구·저장소,
첨부 전송 지원, 실행 슬롯을 함께 확인한다. 이 기능에 참여할 노드는
`cluster.execution_slots`와 최신 준비 상태를 제공해야 한다. 가능한 노드가 없으면
사유를 표시하고 실행하지 않는다. 강제 고정이 필요하면 기존 필수 실행 조건의
`nodes`를 사용하며, 이 조건은 다른 머신으로의 대체도 제한한다.

세션 소유 머신이 오프라인이거나 소유권이 불명확하면 다른 머신에서 같은 대화를
자동 재실행하지 않는다. 실행이 끝났는지 모르는 상태의 파일 변경·배포·응답 중복을
막기 위해서다. 기존 outbox의 실행 전 복구는 실행 조건과 소유권 fence를 유지한다.
우선 노드를 필수 `nodes` 조건으로 덮어쓰지 않으므로 기본값과 강제 고정은 구분된다.

노드 선택 저장과 실행에는 `intake_routing.mode=enforce`가 필요하다. 이후 routing을
끄더라도 저장된 기본 노드나 필수 조건을 무시하고 다른 장비에서 실행하지 않는다.
전역 자동 용량 배정이 꺼져 있어도 명시적인 에이전트 기본 노드는 사용할 수 있다.

기존 설정에서 cluster를 생략하면 `enabled=false`, `runtime_profile=full`이며 기존
Discord gateway·대시보드·API를 같은 머신에서 실행한다. 에이전트의 우선 노드 기본값은
`null`이다. 기존 에이전트를 일괄 worker로 이동시키거나 leader/worker 선택을 강제하지 않는다.

예를 들어 gateway와 PostgreSQL을 같은 머신에 배치한 경우 실행 Worker가
대체 가능하더라도 중앙 머신의 종료는 수신과 DB 가용성에 영향을 준다. 실행 장치의
대체와 gateway/DB 고가용성은 별도의 운영 계약이다.

이 설정은 Discord intake의 기본 배정이다. 별도 dispatch 작업의
`required_capabilities.execution`이나 이미 배정된 dispatch의 owner를 바꾸지 않는다.
