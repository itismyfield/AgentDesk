# Replay evidence authority

기준 코드: `68cd533396df10378f8402320ed94eea6ddca7a2`와 이 변경의 S15a live proof 이동이다.
`LocalSnapshot`/runtime root production 구현 이동은 다음 조각이며 이 변경에 포함하지 않는다.

## 상태와 권한

live observation은 reader claim/support/close, EOF/wait/stderr, activity/terminal을 보관한다.
완결 guard 뒤 private key를 소비해 receipt/nonce 증거를 발급한다. ExactAttempt의
한 번 take, DB의 receipt/nonce/incarnation CAS, commit 응답 뒤 retry 소비를 별도로 둔다.
proof는 실행 owner나 delivery lease를 대체하지 않는다.

recovery는 두 행동을 구별한다. `RecoverHold`는 아직 withheld가 아닌 row의 신규
settlement이며 local proof, 같은 root, 이전 process, identity/projection/turn nonce가
필요하다. `RestoreWithheld`는 기존 withheld row의 notice 복원이다. readable notice와
missing/valid hold nonce는 root/proof 없이 복원할 수 있고, empty/mismatched/malformed는
거절한다. 이 복원으로 live proof/attempt/start/retry 권한을 새로 만들지 않는다.
Rootless cfg는 root 없음/current owner/missing legacy hold nonce/proof 없음에서 복원이
끝나는지 fairness와 `RestoreEventually`로 확인한다. proof를 다시 요구하는 변이는
이 진행성을 깨뜨린다. root/current-owner 불변식은 신규 settlement에만 적용한다.

## 모델 전후

| 모델 | cfg | TLC safety/liveness runs | 결과 |
|---|---:|---:|---|
| 설계 v2 | 9 | 15 | GREEN4·기대 RED5. 기존 withheld의 root 없는 진입점은 없었음 |
| S15a | 12 | 21 | GREEN6·기대 RED6. Rootless Current/Proposed GREEN, proof 요구 회귀는 RestoreEventually RED |

세 Rootless cfg의 fairness 해석도 저장소 checker가 실제 확인한다. 다음은 각 cfg의 첫 run이다.

| cfg | generated / distinct | 기대한 결과 |
|---|---:|---|
| CurrentLive | 74905 / 22072 | GREEN |
| CurrentRecovery | 44328 / 16168 | GREEN |
| Forge | 5 / 5 | ClassificationNeedsEvidence, PrivateIssueOnly |
| OwnProcess | 43067 / 18979 | RecoveryStaleOnly |
| ProposedLive | 166768 / 43118 | GREEN |
| ProposedRecovery | 56616 / 20776 | GREEN |
| Reuse | 121050 / 34862 | ProofNamesAttempt |
| RootlessCurrent | 48 / 32 | GREEN |
| RootlessNeedsProof | 32 / 16 | RestoreEventually |
| RootlessProposed | 48 / 32 | GREEN |
| TakeTwice | 21 / 21 | OneDecision |
| WrongRoot | 86633 / 45147 | RecoveryBoundRoot |

## 코드 대조와 negative controls

- Forge: private proof fields와 원래 observer scope의 private key가 임의 발급을 막는다.
  `src/domain/replay_evidence/live.rs`, `src/services/discord/replay_policy/live/issuer_key.rs`.
- Reuse: `src/db/replay_disposition/write.rs`의 evidence.names와 `write/settle.rs`의 owner CAS가
  다른 attempt를 거절한다. 같은 receipt의 새 nonce에 old terminal/no-effect를 쓰는 PG도 있다.
- TakeTwice: `LiveAttempt`의 Mutex<Option<ExactAttempt>>::take가 두 번째 결정을 거절한다.
- WrongRoot/OwnProcess: 기존 `recovery.rs`의 root covers와 DB `write/hold_notice.rs`의
  process incarnation 판정이 신규 settlement를 거절한다. 이 production 코드는 그대로다.
- RootlessNeedsProof: `recovery_engine/restore_inflight/replay_decision.rs`의 PG-only 호출은
  proof=None이고 DB `write/hold_notice.rs`는 기존 withheld를 복원한다. 잘못 추가한
  proof 요구는 해당 경로의 진행성을 깨므로 현재 버그로 보고하지 않는다.

## 차트

[root 없는 기존 notice 복원](rootless-restore.chart.mmd)은 보류 row→복원 완료 두 상태다.
[다른 nonce 재사용 변이](reuse.chart.mmd)는 최단 실패 경로이며 reader 내부 fact는 축약한다.
두 그림 모두 저장소의 statechart 스킬로 생성했다.

## 재현과 한계

```sh
JAVA_TOOL_OPTIONS=-Xmx768m python3 scripts/ci/statecharts_tlc.py --jobs 2 \
  --java /opt/homebrew/opt/openjdk/bin/java \
  --jar /Users/itismyfield/.local/share/tla2tools/tla2tools-1.7.4.jar
```

README와 expected.tsv가 12 cfg를 discovery한다. PR 모드는 checker에 `--base`/`--head`를 준다.
bounded safety와 Rootless restoration의 좁은 liveness를 검사한다. 실제 SQL serialization,
filesystem replacement, serde 전체·provider matrix·일반 relay 진행성의 증명은 아니다.
Rust privacy compile probes와 PG/observer 시험은 별도로 필요하다.
