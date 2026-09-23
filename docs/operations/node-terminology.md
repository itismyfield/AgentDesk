# Hub, Runner와 기능 모드

대시보드와 운영 안내에서는 역할을 **허브 / 실행 노드**, 기능 모드를
**전체 기능 / 실행 전용**으로 표시한다. YAML과 설정 API의 정식 값도 `hub`와 `runner`다.

| 구분 | 설정 값 | 한국어 표시 | 영어 표시 |
| --- | --- | --- | --- |
| 현재 역할 | `hub` | 허브 | Hub |
| 현재 역할 | `runner` | 실행 노드 | Runner |
| 역할 선택 정책 | `auto` | 자동 선택 | Automatic |
| 기능 모드 | `full` | 전체 기능 | Full features |
| 기능 모드 | `runner` | 실행 전용 | Execution only |

허브는 중앙 운영과 작업 배정을 담당하며 에이전트를 직접 실행할 수도 있다.
실행 노드는 배정된 작업을 실행하는 참여 장비다. 기능 모드는 해당 프로세스에서
어떤 모듈을 시작하는지 결정한다. 전체 기능 모드를 사용하더라도 현재 허브가
아닐 수 있으므로 두 항목을 별도로 표시한다.

화면의 현재 역할은 등록 정보의 `effective_role`을 기준으로 한다. 설정된 역할이나
`runtime_profile=full`만 보고 허브로 표시하지 않는다. 알 수 없는 역할·기능 모드는
미확인 상태로 표시한다. `standby` 값이 전달되면 대기로 표시하며 승계 가능 여부를
추정하지 않는다.

`cluster.enabled=false`이면 운영 화면에 **이 컴퓨터 · 단독 운영**을 표시한다.
등록 노드 수가 한 개라는 이유만으로 단독 운영으로 판단하지 않는다. 클러스터가
꺼진 상태에서는 이전 등록 장비의 카드와 원격 제어를 표시하지 않는다.

## 장비와 에이전트 설정

- Mac mini: **허브 · 전체 기능**.
- Windows PC, Mac Studio 또는 Linux 장비를 실행 전용으로 연결: **실행 노드 · 실행 전용**.
- 에이전트 상세의 **우선 실행 장비**: 새 Discord 세션을 우선 시작할 장비.
- **기본 정책 사용**: 저장한 우선 장비를 해제하고 기존 배정 정책을 사용.

장비는 hostname을 먼저 보여 주고 노드 목록에서 `instance_id`도 확인할 수 있다.
선택·저장·라우팅에는 계속 `instance_id`를 사용한다. 기존 세션 소유권,
필수 실행 조건 및 새 세션의 대체 배정 규칙은
[우선 실행 장비](agent-execution-node.md)에 설명돼 있다.

## 설치와 호환성

허브 장비는 `role: hub`, `runtime_profile: full`을 사용한다.
실행 전용 장비에는 다음 설정을 사용한다. 기존 `instance_id`는 세션 소유권을
가리키므로 이름에 `worker`가 포함돼 있더라도 그대로 유지한다.

```yaml
cluster:
  enabled: true
  instance_id: windows-worker-1
  role: runner
  runtime_profile: runner
  intake_routing:
    enabled: true
    mode: enforce
```

이전 `role: leader`, `role: worker`, `runtime_profile: worker`는 호환 입력으로
계속 지원한다. YAML/JSON으로 다시 저장하면 각각 `hub`, `runner`, `runner`가 된다.
역할은 공통 `ClusterRole` 타입으로 검증하며 오타를 `auto`로 취급하지 않는다.
`runtime_profile: runner`는 활성 클러스터, `role: runner`, 활성 intake routing을
요구한다. 기능 모드를 생략하면 기존과 같은 `full`이다. `execution_only`는 설정 값이 아니다.

바이너리를 먼저 업데이트하고 새 설정을 적용한다. 구버전 실행 파일로 롤백할 때는
설정도 배포 전 백업으로 함께 복원한다. 구버전 실행 파일은 새 이름을 인식하지 못한다.

순차 배포 중 구버전 장비도 동작하도록 공유 레지스트리의 `role`/`effective_role`과
schema-1 실행 준비 정보의 `runtime_profile`은 기존 wire 값을 유지한다. 새 노드는
두 이름을 모두 읽는다. `/api/health.runtime_profile`과 설정 직렬화는 새 값을 사용한다.
DB 스키마·API 경로·노드 ID·소유권·lease 키는 변경하지 않는다. `modules.leader_services`
등 기존 health 키도 유지한다. 이 호환 경계는 설정 파일에서 새 이름을 쓰는 것과 독립적이다.

대시보드의 역할·기능 모드·OS 표시는
[`nodeLabels.ts`](../../dashboard/src/lib/nodeLabels.ts)를 공통으로 사용한다.
설정 검증과 공유 레지스트리 변환은
[`cluster_role.rs`](../../src/config/cluster_role.rs)와
[`runtime_profile.rs`](../../src/config/runtime_profile.rs)에 모은다.
설치 구성과 활성 기능은 [실행 전용 기능 모드](worker-runtime-profile.md)를 참고한다.
