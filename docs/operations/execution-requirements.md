# 필수 실행 조건

agent의 필수 조건은 PostgreSQL `agents.execution_requirements`가 소유한다.
Worker의 로컬 YAML이나 선호 label로 덮어쓰지 않는다. 정책을 저장하는 보호 API는
full 런타임의 `GET/PUT /api/agents/{id}/execution-requirements`이며 worker 프로파일에는
관리 route를 제공하지 않는다. PUT 본문은 아래와 같은 조건 객체이고 `{}`는 조건 해제다.

```json
{
  "os": ["windows"],
  "arch": ["x86_64"],
  "nodes": ["windows-worker"],
  "tools": ["git"],
  "repositories": ["kunkunGames/AgentDesk"],
  "backends": ["process"]
}
```

`os`, `arch`, `nodes`의 각 목록은 허용 값 중 하나를 만족해야 한다. `tools`,
`repositories`, `backends`는 모두 갖추어야 한다. 빈 목록은 해당 차원의 제한이 없다.
OS 값은 `windows/linux/macos`, CPU 값은 `x86_64/aarch64`, backend는 `process/tmux`다.
backend 조건은 기능의 존재를 요구하며 provider의 실행 backend 설정을 변경하지 않는다.
저장소는 논리 ID를 사용한다. `github.repo_dirs`에 각 노드의 실제 경로를 설정해야 한다.
도구 이름은 실행 파일 이름이고 추가 도구는 해당 노드의 `cluster.capabilities.tools`에서
probe하도록 설정한다. 알 수 없는 필드, 잘못된 값이나 절대 경로는 저장을 거절한다.

provider 인증 프로파일은 기존 channel → agent → primary/default 선택을 그대로 사용한다.
이 정책이 다른 계정으로 임의 전환하지 않는다. 준비 상태의 로컬 credential 존재 확인과
원격 인증·quota 검증의 차이는 [노드 관측과 제어](cluster-node-observability.md)에 설명한다.

필수 조건이 있는 agent는 intake routing `enforce`가 필요하다. `observe/disabled`로
바꿔 조건 검사를 우회할 수 없다. 적합한 노드가 없으면 사유를 반환하고 실행을 거절한다.
현재 정책은 무기한 대기 예약을 만들지 않는다. 노드의 복귀와 준비 상태를 확인한 뒤
사용자가 다시 요청할 수 있다. 이미 outbox에 저장된 재시도 가능 작업은 기존 제한된
자동 복구와 운영자 재시도 경로를 사용한다.

살아 있는 기존 세션의 소유자는 새 `/node`나 선호 label보다 우선한다. 기존 소유자가
필수 조건과 맞지 않으면 거절하며 같은 세션을 다른 OS에 생성하지 않는다. 선호 label은
필수 조건을 통과한 후보 사이에서 사용하고, 필수 조건이 없는 기존 동작은 유지한다.

배정 시 조건을 `intake_outbox.execution_requirements`에 저장한다. Worker는 수락 전에
현재 로컬 probe와 실제 provider 프로파일을 다시 검사한다. 오래된 Worker는
`execution_requirements_v1` 소비 능력을 광고하지 않으므로 필수 조건 작업을 받지 않는다.
자동 pre-accept 복구와 운영자 재시도 모두 원본 조건을 복사한다. 중앙 정책을 변경해도
이미 저장된 요청의 조건은 바뀌지 않는다. 정책 변경을 과거 요청에 적용하려면 기존
작업 상태를 확인하고 새 요청으로 제출한다.

dispatch는 기존 `required_capabilities`의 `execution` 필드에 같은 조건 객체를 넣는다.
`{"required":{"execution":{...}}}` 형태도 지원한다. intake와 dispatch는 같은 순수
조건 평가기를 사용하며 각 큐의 소유권, claim, 중복 방지와 수명 주기는 유지한다.
