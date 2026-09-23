# 실행 노드의 실행 전용 기능 모드

같은 AgentDesk 실행 파일을 사용한다. `cluster.role`은 PostgreSQL 허브 권한 lease
선택이고, `cluster.runtime_profile`은 프로세스가 시작할 모듈의 범위다.
화면에서는 허브(`hub`), 실행 노드(`runner`), 전체 기능(`full`), 실행 전용(`runner`)으로
표시한다. 이전 설정 값은 호환 입력으로 받으며 저장 시 새 이름을 사용한다. [공통 용어 안내](node-terminology.md)를 따른다.
기능 모드를 생략한 기존 설정은 `full`로 해석하므로 기존 허브·자동 선택·대기 동작을 보존한다.

```yaml
cluster:
  enabled: true
  instance_id: runner-example
  role: runner
  runtime_profile: runner
  api_base_url: http://runner.example.invalid:8791
  intake_routing:
    enabled: true
    mode: enforce
```

예시의 ID와 주소는 운영자가 지정한 노드 ID 및 실제 접근 가능한 주소로 바꾼다.
실행 전용 기능 모드에는 명시적인 `role: runner`와
활성 intake routing이 필요하다. agent 채널에 연결된 provider bot 설정도 필요하다.
잘못된 조합이나 bot이 없는 실행 전용 기능 모드는 기동을 거절한다.
기능 모드 변경은 기존 cluster 설정과 동일하게 재시작 후 적용한다.

| 기능 | 전체 기능 (`full`) | 실행 전용 (`runner`) |
| --- | --- | --- |
| Provider 실행, intake claim, heartbeat | 유지 | 유지 |
| 세션 출력·취소·복구, provider hook, 종료 대기 | 유지 | 공통 구현 유지 |
| Discord gateway lease 획득·재시도·승격 | 기존 역할/lease 정책 | 시작하지 않음 |
| Voice STT/TTS/receiver/background 작업 | 기존 voice 설정 | 초기화하지 않음 |
| 허브 담당 scheduler·정책 tick·calendar 등 | 기존 lease 대기/실행 | task/thread 자체를 생성하지 않음 |
| Dashboard 자산 보충·정적 제공·WebSocket | 제공 | 제공하지 않음 |
| 설정·agent CRUD·onboarding·queue 관리 API | 제공 | route를 구성하지 않음, 404 |

노드 실행 API의 단일 정의는
[`src/server/routes/domains/runtime.rs`](../../src/server/routes/domains/runtime.rs)다.
전체 기능과 실행 전용 모드가 같은 handler와 인증 middleware를 사용한다. 공개
`/api/health`와 인증 상태 확인을 제외한 실행 API에는 기존 Bearer/로컬 인증 규칙이
적용된다. 유효한 관리자 토큰도 실행 전용 모드에서 빠진 route를 활성화하지 못한다.
TUI provider hook/relay는 provider self-exec를 위해 필요한 경우 유지한다.

`/api/health`는 `runtime_profile`, `modules`, `dashboard_required`를 제공한다.
실행 전용 모드의 dashboard는 `false`, `dashboard_required`도 `false`다. 상세 health의
provider `runtime_role`은 `worker`이며 gateway 접속이 없다는 이유로 standby나
장애로 바꾸지 않는다. 실제 DB 장애, 종료 대기, 복구 미완료 등은 그대로 반영한다.
intake poller와 CLI의 실제 실행 가능 여부는 별도의 readiness 근거로 확인한다.

실행 노드도 실행 중 받은 후속 입력을 대기열에 저장하고, 앞선 turn이 끝나면 자동으로
이어 실행한다. 재시작할 때는 디스크에 남은 대기열을 복구한다. 이 경로는 Gateway
연결 없이 기존 Discord REST 인증 정보를 사용하며, 전체 기능 모드와 같은 채널 정책,
세션 소유권, dispatch lease와 중복 방지 검사를 거친다. `intake_outbox.status=done`은
실행 노드의 인수 완료를 뜻하므로, 실제 작업 완료는 Discord 답변과 실행·대기열 상태를
함께 확인한다.

이 기능 모드는 모듈 실행 범위의 제한이다. 선택한 신뢰하는 실행 노드 운영 모델에서는
기존 공유 PostgreSQL 연결과 Discord REST credential을 사용한다. 노드별 credential
권한 격리, Discord token 없는 실행, DB 비접속 protocol을 구현했다는 의미는 아니다.

패키지와 executable은 플랫폼별 기존 공통 artifact를 재사용한다. 실행 전용 모드에서
dashboard 파일이 없어도 자동으로 다시 복사하지 않는다. 패키지 크기·다운로드량은
이 변경으로 줄지 않으며, RSS·idle CPU·DB 연결 수·시작 시간은 두 장비 실측 결과와
함께 판정한다. Gateway/voice를 빌드에서 제거한 별도 실행 파일은 만들지 않는다.
