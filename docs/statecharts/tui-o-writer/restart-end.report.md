# Same-source effort End witness

기준 소스는 `7d5900ca70ff6a1279a3fd93b4d88bf8f29a7f09`다. 기존 `WriterWake.tla`에 `RestartSpec` 투영을 더했다. 기존 세 Init은 `restart`를 dormant로 고정하고 기존 Next는 이를 보존한다. 기존 11 cfg의 규칙·상수는 바꾸지 않았다. `TmuxSession.tla`의 EffortCloseInterrupted/old-debt 투영은 confirmed end를 oracle로 이미 받으므로 수정하지 않았다. 이번 투영은 그 oracle 아래의 historical binding·old opener/cutoff·seal·Prepared replay 계약을 세분한다.

S2 원본은 공식 CI checker 22/22 기대 일치로 완료됐다. S2d는 확인/영수증/POST 응답/crash 전이를 보강한 RestartSpec 12 cfg를 공식 도구로 다시 실행해 4 GREEN과 8 expected RED를 확인했다. 아래 규칙 정의는 약화하지 않았다. root는 dabfc34dd38a10873f083362346e8b9dd9874331에서 기존 11개와 RestartSpec 12개, 총23 cfg의 CI 기대 집합 일치를 확인했다. 명령은 `python3 scripts/ci/statecharts_tlc.py --base 2031a2e9 --head HEAD --jobs 2`이며 원문은 lane artifact의 logs/s2e-statecharts-ci.log다.

## 값과 소스 경계

| 값/행동 | 코드 근거 | 축약 |
|---|---|---|
| 동일 native source | `src/services/tui_o/shadow/mod.rs`의 SourceId/SourceBinding | session/path와 file_match의 ino 판정을 source 동등성으로 축약. dev 재번호는 동일 prefix 증명과 함께 허용하므로 dev strict equality가 아님 |
| historical binding·nonce·cutoff·prefix·opener | `src/services/tui_o/store/restart_end.rs`의 RestartEnd, append/read_durable; `src/services/tui_o/writer/restart_end.rs`의 validated | exact 또는 nonce/cutoff/source/prefix/opener 불일치. 최신 incarnation nonce와 비교하지 않음 |
| old/new opener | `src/services/tui_o/writer/input_facts.rs`의 open_after, apply_restart_ends | old opener1, 뒤 prompt2의 동등성 부류. 실제 byte 크기나 native id를 바꾸지 않음 |
| 새 prompt 흡수 | 기준 `src/services/tui_o/shadow/seal.rs:94`의 TurnTracker::observe와 Prompt-in-Open 분기 | reset 없이 새 prompt를 읽으면 old Open에 흡수 |
| pre-fold | `src/services/tui_o/writer/input_facts.rs`의 poll_limited, record.start 앞 apply_restart_ends 및 EOF 적용 | Load→ReadOld→cutoff reset→ReadNew→Seal. 현재 file은 cutoff보다 길 수 있음 |
| native Closed | 같은 InputFacts의 Ordered::Closed / last_close_aborted | 실제 native row 읽기와 physical witness를 분리. witness가 native record를 추가하지 않음 |
| Prepared/Posted | `src/services/tui_o/store/mod.rs`의 ChannelStore::ledger; `src/services/tui_o/writer/actor.rs`의 unresolved/refilter_after_open/retry_local | 기존 Prepared identity·외부 POST effect·Created/Uncertain 응답·확인 verdict·durable Missing/Posted receipt를 분리 |
| writer 재시작 | 기존 WriterWake의 bounded Restart 및 actor boot replay | 한 번의 writer 재시작. 제품 재시작 횟수 한도가 아님 |

`currentNonce`1/2는 witness 소비 시 old/new binding 관측의 동등성 부류다. `newPrompt`는 writer가 재생할 때 cutoff 뒤 prompt가 이미 존재하는 fixture 조건이다. 물리 TUI 종료·새 process launch의 전체 시간선을 이 투영이 재현한다는 뜻이 아니다. 생성자는 실제 종료 시 complete EOF를 검증하고, 소비자는 동일 prefix0..cutoff를 검증하면서 len>=cutoff를 허용한다는 port 계약을 입력으로 받는다. OS 종료 확인·fsync crash 원자성·hash 구현은 Rust 수용 시험의 책임이다.

## 전/후 표

| cfg | source projection | S2 상태 | S2d 상태 | 결과 |
|---|---|---:|---:|---|
| Current | witness 없는 no-closer Open | 28 | 28 | RestartExactEndEventuallyCloses RED |
| Fixed | boot + 기존 Prepared의 외부 effect 있음/없음 두 fixture | 104 | 384 | 모든 규칙·2개 PROPERTY GREEN |
| Prefix | old prefix를 witness 뒤 seal | 76 | 120 | GREEN |
| Native | 실제 native closer 대조 | 76 | 120 | GREEN |
| Guarded | nonce/cutoff/source/prefix/opener 불일치 | 60 | 60 | fail-closed GREEN |
| Unscoped | nonce/cutoff 검사 제거 | 152 | 240 | RestartExactEndOnly RED |
| Late | witness를 replay·seal 뒤 설치 | 42 | 42 | RestartNewPromptNotAbsorbed, RestartWitnessBeforeSeal RED |
| NewOpen | old End 재관측이 새 opener guard를 무시 | 92 | 164 | RestartOldEndCannotCloseNewOpen RED |
| DropPrepared | crash가 미확인 Prepared를 잃음 | 104 | 312 | RestartPreparedNotLost RED |
| Repost | 실제 POST→crash→확인 gate 우회 | 112 | 356 | RestartReplyOnce RED |
| CurrentNonce | historical 대신 새 current nonce 비교 | 12 | 12 | RestartExactEndEventuallyCloses RED |
| WitnessLost | durable witness 보존 port 실패 | — | 122 | RestartWitnessSurvivesBoot, RestartWitnessBeforeSeal RED |

기존 초안의 Current/CurrentNonce exit2는 liveness loop의 재시작 가능/소진 상태를 같은 view label로 합친 오류였다. 최종 view는 이를 구분해 둘 다 기대 temporal RED(exit1)로 완료했다. Late는 prompt 흡수만 깨진 상태와 witness 없는 seal까지 깨진 상태를 다른 label로 그린다. Repost는 첫 POST effect가 crash 전에 존재해야만 중복 분기에 진입한다. S2d 상태 수는 최종 모델 소스의 새 실행 결과다. S2 열은 기존 검증과 비교하기 위한 역사적 수치다.

## 반례와 코드 대조

- Current, 8 steps: old physical end→writer crash→boot→witness 없이 old opener 재생→closer 없음→seal 시도→old Open 유지의 영구 loop. 기준 InputFacts에는 physical End 소비가 없고 기준 `src/services/discord/turn_presence/activity.rs:685` 부근은 stale Open을 native closer 없이 Idle로 바꾸지 않는다. main gap으로 코드 확인됐다.
- Unscoped, 4 steps: end receipt→설치→old opener→불일치 nonce/cutoff로 reset. 새 validated/apply 경계의 검사 제거에 해당하는 반사실 변형이다. Rust `wrong_nonce_or_cutoff_cannot_close` 및 제거 mutant가 실제 구현을 검증해야 한다.
- Late, 5/6 steps: 설치 없이 old opener→cutoff에서 Open 유지→새 prompt 흡수→seal. TurnTracker의 Prompt-in-Open 무시 분기와 source replay 전 설치 순서를 단계별 대조했다. 설치를 seal 뒤로 이동한 Rust mutant의 검증 대상이다.
- NewOpen, 7 steps: old reset→새 prompt가 Open2 생성→old prefix seal→old End 재관측으로 Open2 삭제. 실제 InputFacts의 open_after/native id exact guard가 이 분기를 막아야 한다. 모델 변형을 main의 기존 버그로 주장하지 않는다.
- DropPrepared, 2 steps: end receipt→writer crash에서 outstanding Prepared 삭제. 기존 ledger를 초기화하는 가상의 변형이다. 실제 구현이 기존 Prepared를 그대로 보유하는지 reopen 시험으로 확인한다.
- Repost, 13 steps: 기존 미확인 Prepared→readable history에서 없음 확인→old incarnation의 physical End 기록→durable Missing receipt→새 Prepared 허가→POST effect→crash→boot→witness 설치→old/new replay→seal→확인 없는 두 번째 POST. 외부 effect와 ledger ack 사이 crash 경계이며 기존 writer의 확인·dedupe 계약이 막는다. 새 witness가 Prepared/Posted identity를 초기화해서는 안 된다.
- CurrentNonce, 4 steps: valid old receipt→crash→boot에서 새 current nonce로 old receipt 거절→영구 Held. 새 incarnation2의 존재는 historical binding1의 정확한 종료 증거를 무효화하지 않는다.

모델의 guard mutants는 의도한 검사 제거를 설명한다. 이 하위 작업은 Rust 시험·실제 Discord·프로세스 kill을 실행하지 않았으며 모델 RED를 Rust RED로 세지 않는다. code-confirmed current gap과 새 구현의 acceptance/mutant 의무를 구분한다.

## 규칙과 범위

- 답 중복0: RestartReplyOnce. `effects=0`을 POST 허가 guard로 사용하지 않는다. fresh Prepared만 POST할 수 있고, 복원된 미확인 Prepared는 먼저 exact-history verdict를 받아 durable receipt를 기록한다. Created 응답 또는 Posted verdict는 durable Posted로 닫고, 읽기 실패/unknown은 POST 허가를 주지 않는다. Missing receipt 뒤 별도 새 Prepared는 아래 재시도 port 가정에서만 허가된다.
- 다른 End 닫기0: RestartExactEndOnly 및 RestartOldEndCannotCloseNewOpen.
- 설치 순서: RestartWitnessBeforeSeal 및 RestartNewPromptNotAbsorbed.
- 재시작 보존: RestartPreparedNotLost, RestartWitnessSurvivesBoot의 정의를 그대로 유지했다. 둘은 실제 파일 시스템을 모델링한 증명이 아니라 durable receipt/ledger/witness 저장 port 가정이다. Crash는 volatile 허가·verdict·응답을 지우고 durable 행을 보존한다. DropPrepared/WitnessLost cfg는 그 저장 port를 의도적으로 깨면 규칙이 RED임을 보여준다. fsync/rename/디스크 오류의 정확성은 Rust reopen/오류 주입 시험에서 검증해야 한다.
- native 사실: RestartNoNativeForgery. physical Idle과 actual native Closed는 다르다.
- 공정성: 설치·source replay·seal·기존 POST/확인 경로가 계속 가능하면 결국 처리된다는 WF 가정. 실제 Discord 영구 장애·owner/lease 상실·끝없는 재시작의 진행성을 약속하지 않는다.

## 확인·영수증 port와 남은 한계

S2d는 fresh Prepared→POST(Created 또는 Uncertain)→durable Posted, 또는 복원/Uncertain Prepared→history 확인→durable Posted/Missing→허가된 새 Prepared를 별도 행동으로 둔다. Crash는 POST 뒤 Posted receipt 전에도 가능하며, 응답과 허가는 없어지고 외부 effect는 남는다. Fixed는 외부 effect가 이미 있는/없는 미확인 Prepared를 모두 시작 fixture로 둔다. Repost fault는 실제 POST 후 crash한 경로에서만 확인 gate를 우회한다. `RestartReplyOnce`의 효과 수 guard로 규칙을 미리 보장하지 않는다.

확인 verdict의 진실성은 `writer/confirm.rs:24`의 Posted/NotFound/Unresolved 판정과 읽기 가능한 exact-history port 가정이다. 모델이 Discord history의 일관성이나 timeout/페이지 상한을 증명하지 않는다. `writer/deliver.rs:822`의 settle_open이 먼저 durable receipt를 기록하고, `writer/local_retry.rs:22` 및 `:70`의 기존 budget/시간/ownership 조건을 충족하는 경우에만 새 Prepared가 허가된다고 축약한다. **NotFound만으로 임의 재전송을 허가한다는 제품 계약이 아니다.** 재시도 budget·지연·lease·ledger serial 전체는 이 투영 밖이다. 모든 새 POST가 외부 effect를 만들고 응답만 Created/Uncertain으로 갈리는 범위이며, HTTP effect 없음은 복원 Prepared의 effect0 fixture로 검사한다.

WitnessLost 최단2steps(physical End→crash에서 witness 유실)는 저장 가정 위반이고, 같은 경로의8steps에는 witness 없는 seal도 깨진다. 둘의 view label을 분리했다. 이 fault를 현재 Rust의 실제 유실 버그라고 주장하지 않는다. Prepared/Witness 보존 GREEN을 디스크 구현 검증으로 세지 않으며, 저장 port 실패 반례와 source/pre-fold/confirm 프로토콜 검증을 구분한다.

현재 source의 old opener를 처음부터 재생하는 경로를 범위로 한다. mid-offset/LargeTail에서 old opener를 관측하지 못한 Unknown은 fail-closed이며 이 모델이 Idle로 복구된다고 증명하지 않는다. source rotation/GC/archive 파서·notice·queue custody·continuation C·새 TUI launcher는 별도 모델과 이후 조각의 경계다.

차트는 각 cfg의 `restart-end-<Case>.chart.mmd`, 상세 판정/최단 경로는 같은 이름의 report.md에 있다. 동일 label을 유지하는 step은 도구가 자동 생략하며 수동으로 숨긴 중요한 전이는 없다. Repost의 crash/POST/확인/receipt 경계를 유지해 해당 차트는49 arrows다.

## 재실행

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar JAVA=/opt/homebrew/opt/openjdk/bin/java
~/.claude/skills/statechart/scripts/statechart docs/statecharts/tui-o-writer/WriterWake.tla --config docs/statecharts/tui-o-writer/WriterRestartEndFixed.cfg --view docs/statecharts/tui-o-writer/restart-end.view.mjs --out /tmp/i8109-s2-writer-new
```

S2d 원시 로그는 lane artifact의 logs/s2d-sc-*-checked.log 및 sc-out/s2d-*-checked/tlc*.out에 있다. WitnessLost의 view 표시 최종 실행은 logs/s2d-sc-WitnessLost-view.log 및 sc-out/s2d-WitnessLost-view/다. 구조화 요약은 s2d-statechart-results.json이다. 이전 S2 CI22/22와 보강 모델의 root CI23/23 기대 집합 일치를 구분한다. 이 문서 정정은 모델·cfg·view를 바꾸지 않는다.
