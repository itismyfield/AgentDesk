# Herdr machine start wait

기준 main `56fad66678`, lane before `df269ec14`. 코디네이터의 r2 결정에 따라 start 대기의 관측 출처만 Herdr exact reader로 교체한다. `owner_input.rs:723-748,795-820,887-901`이 non-person API/routine start의 관측·Watch·hold 소비자다. `held_watch.rs:121-158`은 같은 front/generation의 Ready만 wake하며 Unknown은 Keep한다. 다른 tmux/late 경로는 그대로다. 변경 전 전수표는 lane `report-impl-t1-r2.md`와 `logs/r2-wait-entrypoints-before.txt`에 고정했다.

모델 값은 위 start/held_watch와 `claude_handoff.rs:442-484`의 exact Bound host, `busy/herdr.rs:210-258`의 Idle/Busy/Held 관측에서 가져온다. Unknown은 Held에 대응한다. 사람 입력은 기존 Busy 합류를 한 번 하며 machine의 native effect와 분리한다. 앞 own-turn의 terminal 뒤 source evidence가 따라잡는 건강한 reader와 fair owner/watch 실행을 가정한다. Unknown을 영구히 읽을 때 자동 시작한다는 보장이 아니며 unknown custody는 보존된다. 재시작 시에는 production watcher가 host/target을 새로 읽고 effect driver의 fresh reservation이 최종 검증한다.

## 수정 전/후

`ThreadIngressMachineWait` prefix, `.cfg.example` suffix. before는 Herdr 제어군의 Mutation=none만 tmuxOnHerdr로 바꾸고 같은 fixture/PROPERTY로 실행한다. 다른 mutant/tmux 제어군은 같은 bytes다. L은 실제 temporal PROPERTY `MachineOwnTurn`이다.

| cfg | before broken | after broken |
|---|---|---|
| HerdrBusy | L | - |
| HerdrUnknown | L | - |
| TmuxBusy | - | - |
| TmuxUnknown | - | - |
| Before | L | L |
| TmuxOnHerdr | L | L |
| UnknownIdle | MachineWaits | MachineWaits |
| NoWake | L | L |

before 2 GREEN/6 expected RED, after 4 GREEN/4 expected RED. MachineWaits·NoDuplicates·NoSilentLoss 유지, MachineOwnTurn·EachReply를 레인에서 직접 TLC로 실행한다. 최종 harness의 actual broken 집합이 위 표와 모두 일치했다. 진단 중 초기 boolean Init 미할당과 terminal state의 stutter 누락을 고쳤으며 그 오류 실행은 최종 증거에서 제외한다.

기존 ingress/race39·terminal11·live-end10을 합쳐 after 전체68cfg(34 GREEN/34 expected RED, mismatch/tool error/skip0)를 공식 `scripts/ci/statecharts_tlc.py`로 직접 실행했다. 신규 모델과 cfg는 CI 자동 등록이 아닌 materialized examples이며 lane expected.tsv로 전수 실행한다. raw logs: `/home/adk/adk/lane-runs/i8125/logs/tlc-t1-r2-final.log`, before `tlc-t1-r2-wait-before.log`.

## 구현 수용

실제 HTTP에서 앞 own-turn이 끝난 뒤 queued machine이 native own-turn을 시작하며 Busy 합류0, 답 게시1, token/inflight0이다. Unknown 상태에서 시작0·원본 보존을 먼저 관측한 뒤 기존 native terminal/Idle evidence만 공급한다. 새 API 입력이나 수동 wake 없이 Watch가 실제 owner를 깨워 시작해야 한다. Herdr를 tmux 관측으로 복원, Unknown을 Idle로 낮춤, Herdr Watch wake 제거의 생산 mutant가 각각 RED여야 한다. 모델 mutant는 생산 mutant를 대체하지 않는다.

모델링은 2026-11-16 약08:41–08:47 KST에 마쳤다(30분 이내). final log 전체68cfg rc0, before8cfg rc0. 초기 진단 오류 로그는 최종 PASS에 포함하지 않는다.
