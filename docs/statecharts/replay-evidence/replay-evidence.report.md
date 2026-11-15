# Replay evidence authority

모델의 기준 코드는 `68cd533396df10378f8402320ed94eea6ddca7a2`다. S15a는 live proof를,
S15b는 `0e0d2586b06f9548d79fe796a29236099e24aca7` 위에서 recovery/root 값을 이동한다.
아래의 current/proposed 전이를 각 실제 소유자에 대응시킨다. S15b 리뷰 보완은 rootless
spec에서도 전체 Next를 탐색하며, 복원이 local proof/ack를 새로 만드는 변이를 추가한다.

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
| S15a / S15b 보완 전 | 12 | 21 | GREEN6·기대 RED6. 복원만 허용하는 좁은 RootlessSpec |
| S15b 보완 후 | 13 | 25 | GREEN6·기대 RED7. 전체 Next 탐색, RootlessAuthority가 RestorationNoStart·RootlessNoNewAuthority RED |

네 Rootless cfg의 fairness 해석도 저장소 checker가 실제 확인한다.
RootlessNoNewAuthority는 localProof와 ack가 없음을 직접 검사한다. 이전에는 Next가
RestoreWithheld만 포함해 항진이었으나, 이제 전체 Next를 사용하므로 RecoverHold를
비롯한 다른 전이도 guard가 허용하면 탐색한다. restore-authority 변이는 RestoreWithheld가
localProof/ack를 새로 만드는 결함이며 Init → RestoreWithheld 두 상태에서 두 불변식을 깬다.
RecoverHold의 withheld guard만 제거하면 root/proof/current-owner guard가 남아 rootless
초기 상태에서 도달할 수 없으므로, 그 변이만으로 유효성을 주장하지 않는다.
 다음 표의 Rootless 행은 보완 후 첫 run이며, 나머지는 S15a와 동일한 모델의 첫 run이다.

| cfg | generated / distinct | 기대한 결과 |
|---|---:|---|
| CurrentLive | 74905 / 22072 | GREEN |
| CurrentRecovery | 44328 / 16168 | GREEN |
| Forge | 5 / 5 | ClassificationNeedsEvidence, PrivateIssueOnly |
| OwnProcess | 43067 / 18979 | RecoveryStaleOnly |
| ProposedLive | 166768 / 43118 | GREEN |
| ProposedRecovery | 56616 / 20776 | GREEN |
| Reuse | 121050 / 34862 | ProofNamesAttempt |
| RootlessAuthority | 18 / 18 | RestorationNoStart, RootlessNoNewAuthority (각각 검사) |
| RootlessCurrent | 112 / 64 | GREEN |
| RootlessNeedsProof | 64 / 32 | RestoreEventually |
| RootlessProposed | 112 / 64 | GREEN |
| TakeTwice | 21 / 21 | OneDecision |
| WrongRoot | 86633 / 45147 | RecoveryBoundRoot |

## 코드 대조와 negative controls

- Forge: private proof fields와 원래 observer scope의 private key가 임의 발급을 막는다.
  `src/domain/replay_evidence/live.rs`, `src/services/discord/replay_policy/live/issuer_key.rs`.
- Reuse: `src/db/replay_disposition/write.rs`의 evidence.names와 `write/settle.rs`의 owner CAS가
  다른 attempt를 거절한다. 같은 receipt의 새 nonce에 old terminal/no-effect를 쓰는 PG도 있다.
- TakeTwice: `LiveAttempt`의 Mutex<Option<ExactAttempt>>::take가 두 번째 결정을 거절한다.
- WrongRoot/OwnProcess: service의 `LocalSnapshot::load`가 adapter root의 `covers`를
  호출한 뒤에만 snapshot key를 만든다. DB `write/hold_notice.rs`는 신규 settlement 안에서
  snapshot의 S24 incarnation을 읽어 현재 owner를 거절한다. 기존 withheld 복원에는 적용하지 않는다.
- RootlessNeedsProof: `recovery_engine/restore_inflight/replay_decision.rs`의 PG-only 호출은
  proof=None이고 DB `write/hold_notice.rs`는 기존 withheld를 복원한다. 잘못 추가한
  proof 요구는 해당 경로의 진행성을 깨므로 현재 버그로 보고하지 않는다.

## S15b 코드 대응

| 모델 변수·전이 | 실제 소유와 가드 |
|---|---|
| rootHeld / ObserveLocal | CLI `replay_recovery::Singleton::acquire`의 기존 open/flock/PID/canonicalize. adapter `recovery::root::OwnedRuntimeRoot::covers`와 service loader의 guard |
| PrivateRecoveryKey / IssueLocalProof | 원래 recovery loader가 `LoadedSnapshotKey`를 만들고 `recovery::snapshot::issuer::loaded`를 호출. 파일 관측은 계속 service에 있음 |
| rootRetained / DropBootHandle | adapter root의 실제 `Arc<File>`과 `LocalSnapshot::_root`. boot/loader handle을 내려도 sealed snapshot이 같은 lock을 보유 |
| understood / sameIdentity | snapshot의 원문 owned identity와 version/unknown/native metadata. DB의 기존 row-lock 안 identity/projection/turn nonce 비교 |
| storedOwner / RecoverHold | snapshot이 보관한 같은 S24 UUID와 `disposition != "withheld"` 안의 current-owner 거절 |
| RestoreWithheld | 동일 tx에서 기존 withheld notice read/backfill. proof=None·current owner·missing legacy hold nonce를 허용하고 실행 권한은 만들지 않음 |

새 root lifetime PG는 loader handle drop과 global root 변경 뒤에도 원래 lock과 원문 capture가
유지되는지 검사한다. 별도의 rootless/current-owner PG는 기존 notice의 missing hold nonce를
복원하면서 event를 재발급하지 않는지 확인한다. rollback PG는 enqueue 실패와 receipt-reference
실패를 모두 주입한다. root key의 private scope는 실제-source compiler probe로 검사하며,
모델의 rootHeld는 기존 singleton acquire가 성립시킨 custody라는 경계 가정이다.

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

README와 expected.tsv가 13 cfg를 discovery한다. PR 모드는 checker에 `--base`/`--head`를 준다.
bounded safety와 Rootless restoration의 좁은 liveness를 검사한다. 실제 SQL serialization,
filesystem replacement, serde 전체·provider matrix·일반 relay 진행성의 증명은 아니다.
Rust privacy compile probes와 PG/observer 시험은 별도로 필요하다.
