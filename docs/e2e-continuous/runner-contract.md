# Continuous runner 소비 계약

기본 off와 null 주기·예산은 그대로다. S2는 S1 위의 파일 소비 계약이며 정기 배선·배포 companion·실제 operator 전송은 S3에서 연결한다. 레그의 scheduled 실행과 실패 알림 증거는 모두 fixture이며 운영 실행으로 세지 않는다.

`python3 -m scripts.e2e.continuous --config <시험 config> inspect --head <배포 SHA>`는 공유 순수 판정의 `E2E_CHECK`와 `E2E_MONITOR`를 출력한다. enabled일 때 `summary.json`·`status.json`만 읽으며 스케줄·inbox·예산을 바꾸지 않는다. off 관측은 상태 읽기·쓰기 없이 DISABLED와 관측 heartbeat를 반환한다. S3 supervisor는 이 관측을 비밀 없는 외부 mirror로 발행할 수 있다. off tick 자체는 잠금·상태·inbox IO가 없다.

실행 중에는 `status.json`에 running과 heartbeat를, 완료 시에는 `summaries/<run-id>.json`·최신 `summary.json`·outbox pending·마지막 완료 run을 발행한다. 정상 NOT_DUE tick도 heartbeat를 갱신한다. schema2 driver report는 `outputs/e2e/<run-id>/<occurrence hash>.json`에 원래 schema 그대로 보존한다. continuous summary는 별도 schema1이다.

## Driver와 예산

provider 자동 실행에는 `usage:<provider>`·`turn_budget:<provider>` capability와 측정된 turn/token/time 예약 및 일일 한도가 필요하다. null 한도·예약은 잠금 전에 반환하며 파일을 바꾸지 않는다. 일일 예산 소진으로 실행 전 보류된 배포 ID는 다음 창의 후보로 남는다. 최신 요청이 오면 기존 보류 요청도 superseded 처리한다.

driver는 `--budget <JSON>`의 남은 turns·tokens·time_s·부모의 절대 monotonic deadline을 받는다. `budget.TurnGuard.start(max_tokens)`를 매 native turn 직전에 호출하고 False면 입력을 보내지 않는다. `finish(actual_tokens)`로 정산하기 전 다음 turn은 허용되지 않는다. unknown 정산은 후속 입력을 막는다. capability 선언은 이 프로토콜을 실제 호출하는 driver adapter에만 부여한다.

driver report는 `native_turns`, 정규화된 총 `tokens`, `budget_checked` 및 usage의 native_turns/input_tokens/output_tokens/cache_read_tokens/cache_write_tokens를 보고한다. tokens는 네 token 항목 합계이며 native_turns·budget_checked도 일치해야 한다. 구성요소 누락·불일치는 unknown이다. 원래 FAIL은 예산 witness 부족 때문에 낮추지 않는다. unknown이 생기면 해당 provider의 남은 occurrence와 이후 자동 실행을 보류하며 예약을 유지한다. `budget/paused.json`의 해제는 실측을 복원한 운영자의 명시적 복구 몫이며 자동 clear는 없다.

예약 전 crash는 실행 0이므로 요청을 보존한다. 예약 후 미완료 crash는 ABANDONED와 provider pause로 남긴다. 새로운 kill/reset/reap 권한은 없다. 시간 초과여도 살아 있는 driver가 lease를 보유하며, 이미 시작한 native turn의 종료와 실제 idle 검증은 driver/#8124 수명 계약이다.

## Summary와 알림

계획 key는 manifest의 scenario/provider/host/surface/state/origin/busy_basis/placement/output_profile/operation/occurrence 필드로 만든다. config `occurrence_contracts[scenario]`는 누락 필드만 채운다. 누락 qualifier는 unknown으로 남고 PASS가 아니다. driver의 `actual_occurrence_key`는 계획 key와 일치해야 하며 계획 자체를 덮지 않는다.

`required_assertions[scenario]`가 필수 단언의 독립 정본이다. driver가 보고한 기대 목록으로 이를 줄일 수 없다. 각 단언에는 key/status/complete가 필요하다. 실제 contact·instance/boot/head/binary/profile 일치·evidence·모든 필수 단언·완전한 usage가 있어야 PASS다. fixture/skip/0건/누락/timeout/dirty/옛 head/미지원 schema는 PASS가 아니다. 첫 실패를 재시도 PASS로 덮지 않는다.

완료 run 파일을 먼저 내구화하고 예산 정산·cursor/배포 결과·summary/outbox/status를 멱등 반영한다. 마지막 applied 기록 전 crash는 다음 holder가 재실행 없이 복구한다. off/null 설정 중에는 복구도 쓰기를 하지 않고 유효한 설정을 기다린다. 이 대기 중에도 내구화된 완료 결과는 후퇴하지 않는다.

outbox는 단일 supervisor/channel lock 안에서 사용한다. `dispatch(state, send, now)`의 전송 함수는 실제 전달을 확인했을 때 True를 반환해야 한다. 동일 event_id를 전송 멱등키로 쓴다. 제한 재시도 소진·전송 실패는 pending으로 남고 status에 보인다. 전송 성공과 ACK 기록 사이 crash는 같은 event_id를 재전송할 수 있으므로 transport도 멱등키를 지켜야 한다.
