# 실행 전용 Worker 프로필

같은 AgentDesk 실행 파일을 사용한다. `cluster.role`은 PostgreSQL leader lease
선택이고, `cluster.runtime_profile`은 프로세스가 시작할 모듈의 범위다.
기존 설정은 모두 `full`로 해석하므로 기존 leader/auto/standby 동작을 보존한다.

```yaml
cluster:
  enabled: true
  instance_id: windows-worker
  role: worker
  runtime_profile: worker
  api_base_url: http://192.168.1.100:8791
  intake_routing:
    enabled: true
    mode: enforce
```

주소는 실제 노드 주소로 지정한다. `worker` 프로필에는 명시적인 worker 역할과
활성 intake routing이 필요하다. agent 채널에 연결된 provider bot 설정도 필요하다.
잘못된 조합이나 bot이 없는 실행 전용 프로필은 기동을 거절한다.
프로필 변경은 기존 cluster 설정과 동일하게 재시작 후 적용한다.

| 기능 | `full` | `worker` |
| --- | --- | --- |
| Provider 실행, intake claim, heartbeat | 유지 | 유지 |
| 세션 출력·취소·복구, provider hook, 종료 대기 | 유지 | 공통 구현 유지 |
| Discord gateway lease 획득·재시도·승격 | 기존 역할/lease 정책 | 시작하지 않음 |
| Voice STT/TTS/receiver/background 작업 | 기존 voice 설정 | 초기화하지 않음 |
| Leader 전용 scheduler·정책 tick·calendar 등 | 기존 lease 대기/실행 | task/thread 자체를 생성하지 않음 |
| Dashboard 자산 보충·정적 제공·WebSocket | 제공 | 제공하지 않음 |
| 설정·agent CRUD·onboarding·queue 관리 API | 제공 | route를 구성하지 않음, 404 |

노드 실행 API의 단일 정의는
[`src/server/routes/domains/runtime.rs`](../../src/server/routes/domains/runtime.rs)다.
전체 모드와 Worker 모드가 같은 handler와 인증 middleware를 사용한다. 공개
`/api/health`와 인증 상태 확인을 제외한 실행 API에는 기존 Bearer/로컬 인증 규칙이
적용된다. 유효한 관리자 토큰도 Worker에서 빠진 route를 활성화하지 못한다.
TUI provider hook/relay는 provider self-exec를 위해 필요한 경우 유지한다.

`/api/health`는 `runtime_profile`, `modules`, `dashboard_required`를 제공한다.
Worker의 dashboard는 `false`, `dashboard_required`도 `false`다. 상세 health의
provider `runtime_role`은 `worker`이며 gateway 접속이 없다는 이유로 standby나
장애로 바꾸지 않는다. 실제 DB 장애, 종료 대기, 복구 미완료 등은 그대로 반영한다.
intake poller와 CLI의 실제 실행 가능 여부는 별도의 readiness 근거로 확인한다.

이 프로필은 모듈 실행 범위의 제한이다. 선택한 trusted-worker 운영 모델에서는
기존 공유 PostgreSQL 연결과 Discord REST credential을 사용한다. 노드별 credential
권한 격리, Discord token 없는 실행, DB 비접속 protocol을 구현했다는 의미는 아니다.

패키지와 executable은 플랫폼별 기존 공통 artifact를 재사용한다. Worker에서
dashboard 파일이 없어도 자동으로 다시 복사하지 않는다. 패키지 크기·다운로드량은
이 변경으로 줄지 않으며, RSS·idle CPU·DB 연결 수·시작 시간은 두 장비 실측 결과와
함께 판정한다. Gateway/voice를 빌드에서 제거한 별도 실행 파일은 만들지 않는다.
