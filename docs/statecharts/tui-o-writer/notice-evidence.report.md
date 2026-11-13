# Notice evidence lifetime: implementation contract

Baseline: `79bebdd4eaa29b289bf88412825baa7d70a6ac77`.
Independent design review: CLEAN; the four P2 decisions are reflected in the implementation and model.
Model SHA256: `83382a2948dbcafa0e62216be0bcff1205a6cd57249737a45927f6407b7f7602`.
[설계와 진입점 전수표](../../notice-settle.design.md).

NoticeEvidence EXTENDS the existing WriterWake Route model. Notice transitions leave bootAll
unchanged, preserving custody/checkpoint/execute-once and blocked self-wake rules. The existing
WriterWake BootDesign/RouteDesign and InputNoticeSettlement Before/After were also rerun unchanged.
The Mermaid chart is a hand-authored overview. Raw traces and counts below come from TLC.

## 수정 전/후 cfg 표

| Config | D1 결과 | 구현 계약 결과 | 기대와 실제 |
|---|---|---|---|
| NoticeEvidenceAfterDirect.cfg | GREEN | GREEN | 일치 |
| NoticeEvidenceAfterOwned.cfg | GREEN | GREEN | 일치 |
| NoticeEvidenceAfterPolicy.cfg | GREEN | GREEN | 일치 |
| NoticeEvidenceBefore.cfg | EveryPendingTerminates | EveryPendingTerminates | 일치 |
| NoticeEvidenceBoundary.cfg | 신규 | GREEN | 일치 |
| NoticeEvidenceDropLog.cfg | PendingLoggedOnce | GREEN | 일치 |
| NoticeEvidenceDropReplay.cfg | ReplayKeepsTerminal | ReplayKeepsTerminal | 일치 |
| NoticeEvidenceExpireUnproven.cfg | DirectEventuallyPosted, NoExpireOwedDirect, NoQuietLoss | DirectEventuallyPosted, NoExpireOwedDirect, NoQuietLoss | 일치 |
| NoticeEvidenceNoSettle.cfg | EveryPendingTerminates | EveryPendingTerminates | 일치 |
| NoticeEvidenceReasonLog.cfg | PendingLoggedOnce | PendingLoggedOnce | 일치 |
| NoticeEvidenceSameSource.cfg | 신규 | GREEN | 일치 |
| NoticeEvidenceUnavailable.cfg | EveryPendingTerminates | EveryPendingTerminates | 일치 |
| NoticeEvidenceWaitBoundary.cfg | 신규 | EveryPendingTerminates | 일치 |

총17 config, TLC 59회 + fairness precheck17회, 예상과 다른 실패0.
RED가 예상대로 나온 것은 checker 성공이며 production의 liveness PASS라는 뜻이 아니다.
각 temporal property는 singleton run으로 검증했고 SYMMETRY를 사용하지 않았다.

## 독립 리뷰 후 바뀐 계약

- NoticeSpec에서 WF(CloseSession)을 제거했다. SameSource는 SessionBoundary=FALSE라 source가
  영원히 유지되어도 정상 Settle이 GREEN이다. WaitBoundary는 Settle에 closed를 잘못 요구하는
  원인 제거 뮤턴트로 EveryPendingTerminates가 RED다.
- Boundary config만 별도 BoundarySpec의 WF(CloseSession)을 사용하며 BoundaryHappens도 검사한다.
  경계 전/후 직접 입력은 NoExpireOwedDirect와 DirectEventuallyPosted로 계속 보호한다.
- logged bit는 프로세스 메모리다. Crash는 logged와 bootLogs를 초기화하고 누적 logs는 유지한다.
  PendingLoggedOnce는 실행별 최대1회이며 모델의 한 번 restart 뒤 누적최대2회를 허용한다.
  DropLog는 이 수용 계약에서 GREEN이고, reason마다 다시 로깅하는 ReasonLog는 계속 RED다.
- 증거가 영구히 사라지는 Unavailable은 여전히 RED이다. no-obligation proof 없는 직접 입력을
  만료시키지 않으면서 무조건 pending0을 보장한다고 주장하지 않는다.

## Counterexample 대응과 증명 범위

Before/NoSettle은 retained mismatch/incomplete record의 종결 연결 부재를 나타낸다.
메모리 세대 캐시는 증거를 만들지 않으며 이 한계를 숨기지 않는다. ExpireUnproven은 source 경계만
보고 direct를 지우는 금지안이며 NoQuietLoss/NoExpireOwedDirect/DirectEventuallyPosted가 모두 RED다.
DropReplay는 durable Settled를 재시작 때 잃는 금지안이다. 모델의 proof 값은 실제 source/UUID
검증 결과를 추상화한다. 실제 파서·append 오류·snapshot/context 경쟁·cache 비용·구 reader 거절은
Rust 회귀/뮤턴트 시험으로 확인하며 모델 GREEN만으로 그 시험을 대체하지 않는다.

GREEN은 필요한 귀속 증거가 결국 복구되고, 기존 actor/권한/전송 확인이 공정하게 진행하며,
재시작이 유한하다는 환경 전제를 둔다. 정상 정착에는 source 교체의 공정성 전제가 없다.
Prepared/history confirmation은 기존 계약의 추상화이며 네트워크 전체를 다시 증명하지 않는다.

## 재현과 원문

TLA2TOOLS `/home/adk/.cache/tla2tools/tla2tools.jar`, SHA256
`936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88`.
Java21.0.13, jar1.7.4(TLC banner2.19), worker1/config, 동시 config2.

    unset TMUX TMUX_PANE
    python3 /home/adk/adk/lane-runs/i8112/logs/impl-b1/check_models.py

이 wrapper는 repository check_cfg/check_fairness를 호출하며 각 raw stdout와 PROCESS_EXIT를 보존한다.
`/home/adk/adk/lane-runs/i8112/logs/impl-b1/model-matrix.log`, `model-results.json`,
`*_NoticeEvidence*-NN.log`가 위17-config 결과의 정본이다. commit 뒤 변경 선택을 검증하는 공식 명령:

    TLA2TOOLS=/home/adk/.cache/tla2tools/tla2tools.jar python3 scripts/ci/statecharts_tlc.py --base origin/main --jobs 2

최종 Rust 바이너리·선택 모듈·PG·뮤턴트·clippy/static/source-scan 증거는 외부 report-impl-b1.md에 기록한다.
