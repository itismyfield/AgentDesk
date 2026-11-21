# 루틴 전달 기한과 lease 복구 (#8216 E0)

\`RoutineSettlement.tla\`는 \`RoutineRun.Submit\`을 claim, first-key 승인, payload commit으로 나누고 \`RecoverWithdraw\`에 owner commit 전후를 추가한다. Herdr 채널 idle 대기는 \`RoutineHerdrStart.JudgeBuilt\`에 들어가기 전 untried 상태다. \`HerdrBusyReobserve.RHArm/RHWrite\`와 같이 승인 뒤에는 실제 첫 key가 늦게 도달할 수 있다.

| 표본 | Before에서 깨지는 규칙 | Fixed |
|---|---|---|
| untried | \`EveryRunCloses\` | 철회 commit 뒤 failed, key 0 |
| first key 전 claim | \`WithdrawsBeforeKey\`, \`EveryRunCloses\` | 기한이 승인에 앞서며 key 0 |
| 승인 뒤 기한 | \`EveryRunCloses\` | unconfirmed로 종결하고 같은 payload commit을 허용 |
| 채널 key Herdr idle 대기 | \`EveryRunCloses\` | untried를 철회하고 failed |
| lease 복구 | \`CloseAfterOwner\`, \`WithdrawsBeforeKey\` | 네 시작 상태 모두 같은 owner 판정 뒤 interrupted |

Before의 복구 반례는 기한 만료 뒤 queued 원문을 남겨 둔 채 row를 닫는다. 별도 반례에서는 pre-key claim이 기한 뒤 첫 key 승인을 얻고 payload를 쓴다. Fixed에서는 두 행동이 모두 차단된다.

한 run과 한 원문을 모델링한다. 동일한 actor가 판정하며 디스크·DB 장애 시 다음 tick이 성공할 수 있다고 가정한다. 여러 owner의 takeover, provider 완료 결과, DB 동시성은 이 모델 범위 밖이다.

다시 실행할 때는 지정된 statechart 명령에 \`RoutineSettlement.tla --config RoutineSettlementRecoveryBefore.cfg --out <새 디렉터리>\`를 넘긴다. 나머지 cfg는 같은 이름 규칙의 \`Before\`와 \`Fixed\` 쌍이다.
