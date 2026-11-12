# Same-source effort End witness

기준 소스는 `7d5900ca70ff6a1279a3fd93b4d88bf8f29a7f09`다. 기존 `WriterWake.tla`에 `RestartSpec` 투영을 더했다. 기존 세 Init은 `restart`를 dormant로 고정하고 기존 Next는 이를 보존한다. 기존 11 cfg의 규칙·상수는 바꾸지 않았다. `TmuxSession.tla`의 EffortCloseInterrupted/old-debt 투영은 confirmed end를 oracle로 이미 받으므로 수정하지 않았다. 이번 투영은 그 oracle 아래의 historical binding·old opener/cutoff·seal·Prepared replay 계약을 세분한다.

공식 statechart 실행 11개에서 4 GREEN과 7 expected RED를 확인했다. root의 최종 커밋 뒤 `python3 scripts/ci/statecharts_tlc.py --base 7d5900ca70 --head HEAD --jobs 2`는 아직 이 하위 작업에서 실행하지 않았다. 이 checker가 기존 11개와 새 11개, 총22 cfg의 기대 집합을 확인해야 한다.

## 값과 소스 경계

| 값/행동 | 코드 근거 | 축약 |
|---|---|---|
| 동일 native source | `src/services/tui_o/shadow/mod.rs`의 SourceId/SourceBinding | session/path/dev/ino 전체 equality를 exact/source 불일치로 축약 |
| historical binding·nonce·cutoff·prefix·opener | `src/services/tui_o/store/restart_end.rs`의 RestartEnd, append/read_durable; `src/services/tui_o/writer/restart_end.rs`의 validated | exact 또는 nonce/cutoff/source/prefix/opener 불일치. 최신 incarnation nonce와 비교하지 않음 |
| old/new opener | `src/services/tui_o/writer/input_facts.rs`의 open_after, apply_restart_ends | old opener1, 뒤 prompt2의 동등성 부류. 실제 byte 크기나 native id를 바꾸지 않음 |
| 새 prompt 흡수 | 기준 `src/services/tui_o/shadow/seal.rs:94`의 TurnTracker::observe와 Prompt-in-Open 분기 | reset 없이 새 prompt를 읽으면 old Open에 흡수 |
| pre-fold | `src/services/tui_o/writer/input_facts.rs`의 poll_limited, record.start 앞 apply_restart_ends 및 EOF 적용 | Load→ReadOld→cutoff reset→ReadNew→Seal. 현재 file은 cutoff보다 길 수 있음 |
| native Closed | 같은 InputFacts의 Ordered::Closed / last_close_aborted | 실제 native row 읽기와 physical witness를 분리. witness가 native record를 추가하지 않음 |
| Prepared/Posted | `src/services/tui_o/store/mod.rs`의 ChannelStore::ledger; `src/services/tui_o/writer/actor.rs`의 unresolved/refilter_after_open/retry_local | 기존 Prepared identity·외부 POST effect·Posted ack를 분리 |
| writer 재시작 | 기존 WriterWake의 bounded Restart 및 actor boot replay | 한 번의 writer 재시작. 제품 재시작 횟수 한도가 아님 |

`currentNonce`1/2는 witness 소비 시 old/new binding 관측의 동등성 부류다. `newPrompt`는 writer가 재생할 때 cutoff 뒤 prompt가 이미 존재하는 fixture 조건이다. 물리 TUI 종료·새 process launch의 전체 시간선을 이 투영이 재현한다는 뜻이 아니다. 생성자는 실제 종료 시 complete EOF를 검증하고, 소비자는 동일 prefix0..cutoff를 검증하면서 len>=cutoff를 허용한다는 port 계약을 입력으로 받는다. OS 종료 확인·fsync crash 원자성·hash 구현은 Rust 수용 시험의 책임이다.

## 전/후 표

| cfg | source projection | 상태 | 결과 |
|---|---|---:|---|
| Current | 7d590의 witness 없는 no-closer Open | 28 | RestartExactEndEventuallyCloses RED |
| Fixed | 새 prompt+new current nonce가 있는 boot, 기존 Prepared 보존 | 104 | 모든 규칙·2개 PROPERTY GREEN |
| Prefix | no-closer old prefix를 witness 뒤 seal | 76 | GREEN |
| Native | 실제 native closer 대조 | 76 | GREEN |
| Guarded | nonce/cutoff/source/prefix/opener 불일치 | 60 | fail-closed GREEN |
| Unscoped | nonce/cutoff 검사 제거 | 152 | RestartExactEndOnly RED |
| Late | witness를 source replay·seal 뒤 설치 | 42 | RestartNewPromptNotAbsorbed, RestartWitnessBeforeSeal RED |
| NewOpen | old End 재관측이 새 opener guard를 무시 | 92 | RestartOldEndCannotCloseNewOpen RED |
| DropPrepared | writer crash가 미확인 Prepared를 잃음 | 104 | RestartPreparedNotLost RED |
| Repost | POST effect 후 crash, 확인 없이 동일 Prepared 재게시 | 112 | RestartReplyOnce RED |
| CurrentNonce | historical 대신 새 current nonce와 비교 | 12 | RestartExactEndEventuallyCloses RED |

기존 초안의 Current/CurrentNonce exit2는 liveness loop의 재시작 가능/소진 상태를 같은 view label로 합친 오류였다. 최종 view는 이를 구분해 둘 다 기대 temporal RED(exit1)로 완료했다. Late는 prompt 흡수만 깨진 상태와 witness 없는 seal까지 깨진 상태를 다른 label로 그린다. Repost는 첫 POST effect가 crash 전에 존재해야만 중복 분기에 진입한다. 위 수치는 이 보완 뒤 최종 소스의 재실행 결과이며 이전 초안 상태 수를 재사용하지 않았다.

## 반례와 코드 대조

- Current, 8 steps: old physical end→writer crash→boot→witness 없이 old opener 재생→closer 없음→seal 시도→old Open 유지의 영구 loop. 기준 InputFacts에는 physical End 소비가 없고 기준 `src/services/discord/turn_presence/activity.rs:685` 부근은 stale Open을 native closer 없이 Idle로 바꾸지 않는다. main gap으로 코드 확인됐다.
- Unscoped, 4 steps: end receipt→설치→old opener→불일치 nonce/cutoff로 reset. 새 validated/apply 경계의 검사 제거에 해당하는 반사실 변형이다. Rust `wrong_nonce_or_cutoff_cannot_close` 및 제거 mutant가 실제 구현을 검증해야 한다.
- Late, 5/6 steps: 설치 없이 old opener→cutoff에서 Open 유지→새 prompt 흡수→seal. TurnTracker의 Prompt-in-Open 무시 분기와 source replay 전 설치 순서를 단계별 대조했다. 설치를 seal 뒤로 이동한 Rust mutant의 검증 대상이다.
- NewOpen, 7 steps: old reset→새 prompt가 Open2 생성→old prefix seal→old End 재관측으로 Open2 삭제. 실제 InputFacts의 open_after/native id exact guard가 이 분기를 막아야 한다. 모델 변형을 main의 기존 버그로 주장하지 않는다.
- DropPrepared, 2 steps: end receipt→writer crash에서 outstanding Prepared 삭제. 기존 ledger를 초기화하는 가상의 변형이다. 실제 구현이 기존 Prepared를 그대로 보유하는지 reopen 시험으로 확인한다.
- Repost, 10 steps: end receipt→POST effect→crash→boot→witness 설치→old/new replay→seal→확인 없는 두 번째 POST. 외부 effect와 ledger ack 사이 crash 경계이며 기존 writer의 확인·dedupe 계약이 막는다. 새 witness가 Prepared/Posted identity를 초기화해서는 안 된다.
- CurrentNonce, 4 steps: valid old receipt→crash→boot에서 새 current nonce로 old receipt 거절→영구 Held. 새 incarnation2의 존재는 historical binding1의 정확한 종료 증거를 무효화하지 않는다.

모델의 guard mutants는 의도한 검사 제거를 설명한다. 이 하위 작업은 Rust 시험·실제 Discord·프로세스 kill을 실행하지 않았으며 모델 RED를 Rust RED로 세지 않는다. code-confirmed current gap과 새 구현의 acceptance/mutant 의무를 구분한다.

## 규칙과 범위

- 답 중복0: RestartReplyOnce; Prepared/Posted identity와 외부 effect/ack를 별도 상태로 둔다.
- 다른 End 닫기0: RestartExactEndOnly 및 RestartOldEndCannotCloseNewOpen.
- 설치 순서: RestartWitnessBeforeSeal 및 RestartNewPromptNotAbsorbed.
- 재시작 보존: RestartPreparedNotLost, RestartWitnessSurvivesBoot. durable receipt/ledger write는 원자적 port 입력이다.
- native 사실: RestartNoNativeForgery. physical Idle과 actual native Closed는 다르다.
- 공정성: 설치·source replay·seal·기존 POST/확인 경로가 계속 가능하면 결국 처리된다는 WF 가정. 실제 Discord 영구 장애·owner/lease 상실·끝없는 재시작의 진행성을 약속하지 않는다.

현재 source의 old opener를 처음부터 재생하는 경로를 범위로 한다. mid-offset/LargeTail에서 old opener를 관측하지 못한 Unknown은 fail-closed이며 이 모델이 Idle로 복구된다고 증명하지 않는다. source rotation/GC/archive 파서·notice·queue custody·continuation C·새 TUI launcher는 별도 모델과 이후 조각의 경계다.

차트는 각 cfg의 `restart-end-<Case>.chart.mmd`, 상세 판정/최단 경로는 같은 이름의 report.md에 있다. 동일 label을 유지하는 step은 도구가 자동 생략하며 수동으로 숨긴 중요한 전이는 없다. Repost의 crash/POST 확인 경계를 유지해 해당 차트는33 arrows다.

## 재실행

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar JAVA=/opt/homebrew/opt/openjdk/bin/java
~/.claude/skills/statechart/scripts/statechart docs/statecharts/tui-o-writer/WriterWake.tla --config docs/statecharts/tui-o-writer/WriterRestartEndFixed.cfg --view docs/statecharts/tui-o-writer/restart-end.view.mjs --out /tmp/i8109-s2-writer-new
```

원시 로그는 lane artifact의 logs/s2-sc-*-final.log 및 sc-out/s2-*-final/tlc*.out에 있다. 구조화 요약은 s2-statechart-summary.json이다. 이전 초안의 exit2 로그도 남겼지만 최종 판정으로 세지 않는다. parent가 커밋 뒤 base7d590 CI checker 결과를 구현 보고서에 추가해야 한다.
