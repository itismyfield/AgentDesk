key: turn-mailbox-recovery-kickoff-identity
| `turn-mailbox` 재시작 복구 kickoff 식별자 (#8125) | `9a12a379b9` | Before: 메시지 id 없는 행의 복구 kickoff가 active id 0으로 잡혀 finalizer 가드가 영영 맞지 않음(liveness RED); 채널 단위 대안은 늦은 terminal이 복구 턴을 푼다(safety RED) | `RecoveryKickoffIdentity` 5cfg: GREEN 3, 기대 RED 2. kickoff를 finalizer 키에 맞춤. [보고서](turn-mailbox/recovery-kickoff-identity.report.md) |
