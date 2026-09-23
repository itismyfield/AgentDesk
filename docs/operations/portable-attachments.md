# 노드 간 첨부파일 전달

구현 상태: 개발 브랜치에서 연결 중이며 운영 배포 전 검증이 필요하다.

첨부가 포함된 요청은 기존 `AttachmentBundleV1` 계약으로 모든 파일을 확보한 뒤
PostgreSQL에 바이트와 manifest를 저장한다. `provider/channel/message` 식별자와
파일별 SHA-256을 검증하며 CDN URL이나 송신 노드의 절대 경로를 전송하지 않는다.

| 경계 | 계약 |
| --- | --- |
| 한 메시지 | 최대 10개, 파일별 8 MiB, 전체 24 MiB |
| 다운로드 | HTTPS Discord CDN만 허용, 사용자 정보·임의 포트·redirect 거절. 파일 30초, bundle 90초. portable download 동시 4개 |
| 저장 | PostgreSQL BYTEA + JSON manifest, 전체 활성 payload 예산 1 GiB. 예산 계산·동일 메시지 중복 검사는 transaction advisory lock으로 직렬화 |
| 보관 | 생성 후 24시간. 중복 수신으로 보관 기한을 연장하지 않음. 만료 bundle은 허브 maintenance와 신규 저장 시 정리 |
| 큐·재시도 | typed `Upload::Bundle` 참조를 기존 `pending_uploads` 필드로 전달. 병합·재큐잉·복구 시 동일 참조 보존. 기존 문자열 기록은 `Upload::Local`로 읽는 호환 계약 유지 |
| outbox | `attachment_refs`에 식별자와 저장 참조 snapshot 보존. 자동 pre-accept retry와 명시적 operator retry에서도 복사 |
| 소비 | 실행 노드가 accept하기 전에 저장·식별자·digest·크기를 확인. 실제 prompt 준비 때 다시 확인하고 안전한 임시 디렉터리에 복원 |
| 임시 파일 수명 | provider 실행 closure가 소유. 단순 outbox 전달 완료나 busy requeue로 파일 수명을 판정하지 않음. 실제 실행 종료 시 guard 해제. 강제 종료 후 orphan은 24시간 뒤 정리 대상 |
| 실패 | 일부 첨부를 버리고 text-only로 성공하지 않음. 만료·손상된 queued 참조는 명확한 안내와 함께 거절. 일시적 DB 장애는 큐를 보존 |

bundle 저장이 commit된 뒤에만 참조를 outbox 또는 로컬 durable queue에 게시한다.
라우팅이 거절되거나 producer가 중간 종료되어 참조되지 않은 bundle도 동일 예산과
24시간 정리 규칙을 따른다. consumer는 참조가 없어졌다고 빈 파일 목록으로 바꾸지 않는다.

임시 파일명은 ordinal과 검증된 digest로 만들며 원래 이름에서 짧은 영숫자 확장자만
유지한다. 원래 파일명은 경로에 쓰지 않고 prompt에서 JSON 문자열로 escape한다.
바이트 검증이 모두 끝나기 전에는 provider를 시작하지 않는다.

구버전 실행 노드에는 `attachment_bundle_v1`을 광고하는 consumer가 없으므로 원격 첨부
배정을 하지 않는다. 이미 저장된 과거 로컬 경로 기록은 다른 노드로 전달하지 않는다.
이 경우 원본 파일을 새 메시지에 다시 첨부해야 portable 계약으로 처리된다.

첨부 저장 기한이 지난 작업은 원본 파일을 다시 보내야 한다. provider가 이미 시작해
소유한 임시 파일의 수명은 실행 guard가 관리하므로 DB 정리가 실행 중인 파일을 지우지 않는다.
`runtime/portable_attachments`의 각 임시 디렉터리는 OS 파일 잠금을 보유한다.
노드 시작 및 매시간 수행하는 정리는 24시간이 지난 디렉터리 중 잠금 획득이 가능한
orphan만 제거한다. 활성 실행, 다른 이름의 디렉터리, symlink와 전용 root 밖 경로는
삭제하지 않는다. 실제 실행 노드 재시작과 파일 잠금의 OS별 실행 검증은 배포 전 확인 항목이다.

관련 소스: [bundle·실행 노드 검증](../../src/services/cluster/attachment_transfer.rs),
[저장](../../src/services/cluster/attachment_transfer/store.rs),
[typed 참조](../../src/services/cluster/attachment_transfer/uploads.rs),
[materialization](../../src/services/cluster/attachment_transfer/materialize.rs).
