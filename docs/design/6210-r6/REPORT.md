A 1228 / B 720 / C 133 / D 1047 (계 3128행) · r5 표 누락 2761 (A 1133, B 615, C 125, D 888) · 관리채널 도달 불명 661

# #6210 r6 선행 산출물: 기계적 인벤토리 레인 보고

- 대상: `/Users/itismyfield/.adk/release/worktrees/i6210-inv` @ `d834201262` (detached)
  - worktree는 그대로 두었다(메인 회수용). 작업 후 상태는 아래와 같고, 두 표지 사이가 빈 것이 출력이다:
    ```
    $ git -C /Users/itismyfield/.adk/release/worktrees/i6210-inv status --short; echo "[end]"
    [end]
    ```
- 산출물: `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i6210-inventory/`
  - `gen_inventory.py`, `inventory.tsv`, `wrappers.tsv`, `r5_citations.tsv`, `summary.md`, `pin_test_draft.py`
  - `derive_tables.py` → `coverage.tsv`, `unknown_by_fn.tsv`, `missing_by_fn.tsv`, `stats.txt`
- 하지 않은 것: push, 커밋, PR, GitHub 댓글, DB 조작, 배포, cargo 빌드·테스트.
  - 실행한 것은 python 스크립트와 읽기 전용 git(`diff`·`log`·`fetch`)뿐이다.

## 1. 결과

| 범주 | 행 | 도달 예/아니오/불명 | r5 누락 | 누락 중 리뷰 태그도 없음 |
|---|---|---|---|---|
| A Discord HTTP 송신·수정·삭제 | 1,228 | 523 / 335 / 370 | 1,133 | 1,084 |
| B inflight 행 삭제·재작성 | 720 | 668 / 0 / 52 | 615 | 530 |
| C transcript·spool 삭제·truncate | 133 | 57 / 12 / 64 | 125 | 97 |
| D offset·frontier·배달 경계·lease writer | 1,047 | 871 / 1 / 175 | 888 | 819 |

- site가 있는 파일은 438개다. 그중 363개는 r5 초안·리뷰 어디에도 인용되지 않았다.
- 누락이면서 도달 `예`인 행: A 435, B 563, C 49, D 713. 함수 단위 목록은 `missing_by_fn.tsv`에 있다.
- r5 초안·리뷰 인용 121행 중 96행이 적중했다.
  - 적중 0인 25행은 하나씩 사유를 확인했다(`summary.md` §3 표). 전부 reader·가드·정의·계산기, A~D 밖의 재시작 제어, 스캔 범위 밖(deploy-release.sh), 또는 메서드 간접 호출(S15)이다.
  - 스크립트 누락으로 남은 것은 0이다.
  - 확인 중 드러난 누락은 스크립트를 고쳐 포함시켰다: 한 단계 규칙, 함수 값 참조, `use` 재수출 제외, fn 경계 shape, D 축약 초기화, C truncate 범위.
- 교차 확인: B의 inflight 행 unlink는 16건이다(Rust 15 + 정책 op 구현 exec_ops.rs:303). 여기에 archive rename 1건(clear_store/mod.rs:274)이 더해지고, `log_inflight_remove` 대조와 일치한다.

## 2. 06f860d932 이후 relay 경로 변화

`git diff --stat 06f860d932 origin/main -- src/services/discord/ src/services/turn_orchestrator/ src/db/`:

- 인벤토리 기준 SHA로 실행한 결과는 **빈 출력**이다. relay 경로 변화가 없다:
  ```
  $ git -C <worktree> diff --stat 06f860d932 d834201262 -- src/services/discord/ src/services/turn_orchestrator/ src/db/
  (출력 없음)
  ```
- src 밖 관련 변경은 `scripts/deploy-release.sh` +14/−2다(1행 build token 중첩 거부 +11, 655행 cargo clean +1).
  - 그 결과 S20 인용 2847-2891은 d834에서 약 2859-2903이다. 내용(`.prev` 백업·승격, 재시작 제어)은 같다.
- 레인 진행 중 origin/main이 두 커밋 전진했다(보고 시점 `7c776629cb`).
  - d8b2e72f6c(#6266): `scripts/ci/h2_measure.py`와 그 테스트만 바뀌었다.
  - 7c776629cb(#6274): `src/db/campaigns.rs`에 `NodeInput` 필드 선언 +5, 그 테스트 +40, routes/docs 문서 문자열 1줄이 바뀌었다.
  - 이 때문에 위 diff 명령을 지금 origin/main에 돌리면 `src/db/campaigns.rs | 5`, `src/db/campaigns/tests.rs | 40`이 나온다.
- 두 커밋이 건드린 파일에는 inventory 행이 0개다. 추가된 코드도 seed 패턴에 걸리지 않는다.
  - 이 판단은 diff를 읽고 내린 것이다. 그 SHA로 생성기를 다시 돌리지는 않았다(worktree를 옮기지 않기 위해서).

## 3. 생성 명령과 재현

```
cd /Users/itismyfield/.adk/release/lane-runs/hl-20260925/i6210-inventory
python3 gen_inventory.py /Users/itismyfield/.adk/release/worktrees/i6210-inv \
    --citations r5_citations.tsv --meta wrappers.tsv > inventory.tsv   # 약 60~110초
python3 derive_tables.py                                              # coverage/unknown/missing/stats
```

재현 확인:

- 생성기를 두 번 새로 실행했다. `inventory.tsv`·`wrappers.tsv` 모두 두 실행끼리, 그리고 기존 산출물과 `cmp` 결과가 **IDENTICAL**이다.
- `derive_tables.py` 재실행 결과 4개 파일도 바이트 동일하다.

sha256:

```
12ddc634110b9915bb9df923bc3a5e04fa323595bd4d4205ccf53392b1bbd96d  gen_inventory.py
d1c880cbf6f895b8cc617a6aaa0f436e405d5d4de9173f5f077d1b8349975eeb  inventory.tsv
a33ad0023aa879444286af6304cc181c99ce1a968cade9cb8e5ef1dd4f010606  wrappers.tsv
aeb5c28acd325addb58b0c1e748c84c19c6e6f2cd778d1bae1c3ebb63310a7d3  r5_citations.tsv
de00eb0f9d28d4515ec58bc434cc6ebda7841845512984ce027d5be82a1af10f  derive_tables.py
65cb513baeacdf2b437c09d863ab4ec3b1a44bc21fa04e66c522601bb2573427  pin_test_draft.py
```

pin 초안 확인:

- `RELAY_INVENTORY_REPO=<worktree> RELAY_INVENTORY_DIR=<산출물> RELAY_INVENTORY_GEN=<산출물>/gen_inventory.py python3 pin_test_draft.py`
  - 결과 `relay side-effect inventory: OK`, exit 0.
- baseline 교란 복사본으로 한 번 더 돌렸다(A 1행 삭제, 가짜 키 1행 추가, wrapper 1개 삭제).
  - 결과: 새 site `src/cli/dcserver.rs:1270`, 사라진 키, 새 wrapper `adopt_watcher_singleton_panel_after_fresh_bind`. 3 problems, exit 1.
  - 교란 복사본은 산출물 디렉터리 안에 만들었고 확인 후 삭제했다.
- 3-1 근거 줄은 초안 docstring과 `summary.md` §7에 있다.

## 4. 불명 661행과 판정에 필요한 것

| 도달 사유 | 함수/행 | 필요한 것 |
|---|---|---|
| services 기타(dispatches·codex·qwen·auto_queue·routines 등) | 121 / 180 | 채널 ID 인자 출처 추적. dispatch·routine 대상 채널이 TUI-direct 관리 채널과 겹칠 수 있는지에 대한 설계 결정 |
| discord 기타 모듈(gateway·tmux_placeholder_suppression·terminal_ui_obligation 등) | 72 / 103 | 모듈별 규칙 1줄씩. gateway는 전 채널 공용이라 호출별 판정 |
| services 밖(server/routes 58·cli/discord) | 52 / 88 | 운영자 API의 channel_id가 관리 채널일 수 있는지. R(수동 재생) 허용 여부에 대한 r6 결정 |
| router: 같은 채널의 Discord 발 턴 | 51 / 147 | 관리 채널에 Discord 사용자 메시지가 올 때 합성 턴 행·placeholder ID를 건드리는지. r6의 관리 채널 소유권 정의 |
| wrapper 내부, 추적된 호출자 없음(outbound 52) | 30 / 55 | 한 단어 이름·메서드 간접 호출로 끊긴 호출부를 손으로 찾으면 유도 규칙으로 정해진다 |
| 호출자 도달이 불명 포함 | 16 / 17 | 위 불명 묶음이 정해지면 자동으로 정해진다 |
| idle recap(idle_recap_interaction·idle_recap) | 13 / 37 | recap이 TUI-direct 세션 채널에도 게시되는지(설정·게이트 확인) |
| Discord 발 큐 턴(queue_dispatch·queue_io·queue_marker) | 11 / 14 | 관리 채널 Discord 큐와 합성 턴이 같은 mailbox·큐 마커를 쓰는지 |
| placeholder_controller 공용 경로 | 9 / 10 | 합성 턴 경로가 이 API를 쓰는지 호출자 확인 |
| reaction_lifecycle | 6 / 10 | 합성 턴 prompt anchor에 reaction이 붙는지 |

추정으로 채운 불명은 없다. 함수 단위 전체 목록은 `unknown_by_fn.tsv`에 있다.

## 5. 한계와 참고

- 이름 기반 매칭이다(rustc 타입 해석 없음). 다음은 추적하지 않는다:
  - 메서드 간접 호출(S15 `.note_turn_completed(`)
  - actor·queue 디스패치
  - `macro_rules!` 호출부
- 한 단어·generic 이름 wrapper 50개는 `<모듈>::NAME(` 호출만 센다. 목록은 `summary.md` §6에 있다.
- 동명 정의는 호출부가 합쳐진다. 예: `clear_inflight_by_tmux_name` 정의 3곳.
- 무조건 전체 폐포는 9,765행·깊이 36으로 부트스트랩까지 번져 기각했다. 대신 한 단계 규칙을 쓴다: `one-level` 509, `stop` 471.
- 스캔 밖(src/**/*.rs 아님) 부작용 스크립트: scripts/e2e/run_tui_relay.py, scripts/e2e/tui_relay/discord.py, scripts/voice-channel-migrate.sh, scripts/deploy-release.sh.
- Memento: `agentdesk` 워크스페이스 recall에 관련 항목이 없었다. 새로 저장한 것도 없다.
- DAG 갱신: 이 레인의 범위 밖이다. 읽기 전용 선행 산출물 레인이고 캠페인·노드 갱신은 메인 코디네이터 몫이다. 이 레인은 DAG를 조회·변경하지 않았다.
- 스크래치: 이 레인이 만든 `/tmp/i6210_*`와 `/tmp/r5_citations.tsv`는 삭제했다. `/tmp/i6210r5-*`는 다른 레인 것이라 건드리지 않았다.

INVENTORY_READY
