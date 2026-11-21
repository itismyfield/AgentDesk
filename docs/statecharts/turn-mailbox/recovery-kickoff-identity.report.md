# 재시작 복구 kickoff 식별자와 finalizer 가드

코드 기준: `9a12a379b93049c5849900c80795b5ddf446a65f`.

## 질문

시작한 턴은 결국 메일박스를 놓는가(`MailboxEventuallyReleased == <>[](~token)`).
`TurnFinalizer identity-guarded mailbox release skipped ... active_user_message_id=0`이 찍힌 뒤 메일박스가 풀리지 않는 경로를 찾는다.

## 모델

변수 `phase`(idle → live → down → recovered → finished), `token`, `active`(메일박스 `active_user_message_id`), `holder`, `wrong`, `staleDone`.

- `RowKind = "real"`: 행에 Discord 메시지 id가 있다(`user_msg_id = effective_finalizer_turn_id = 1`).
- `RowKind = "zero"`: `user_msg_id = 0`(TUI 직접 입력, turn/deliver 주입 입력). finalizer 키는 `effective_finalizer_turn_id`(주입 id·finalizer id·합성 id, 모델 값 2). `inflight/model.rs:1241`
- `Policy = "before"`: 복구 kickoff가 `optional_message_id(row.user_msg_id)`로 메일박스를 잡는다(0 → None).
- `Policy = "after"`: 복구 kickoff가 행의 finalizer 키로 잡는다.
- `Policy = "weak"`: 버린 대안. guarded miss에서 active id가 0이면 채널 단위로 푼다.

| 전이 | 코드 |
|---|---|
| `Start` | `turn_orchestrator.rs:2006`(try_start_turn), TUI 직접 `synthetic_start/claim/admitted.rs:121` |
| `Restart` | 메일박스 actor 상태는 메모리, inflight 행은 디스크에 남는다 |
| `Reattach`(창 살아 있음) | `recovery_engine/runtime.rs:193`,`:325` → `turn_orchestrator.rs:2006` |
| `Kickoff`(창 없음) | `restore_inflight.rs:2245` + `restore_inflight/kickoff_identity.rs:20` → `turn_orchestrator.rs:2078` |
| `Finalize` | `turn_bridge/post_loop_finalize.rs:686` → `turn_finalizer/episode.rs:93` → `episode_identity.rs:345`, miss 로그 `turn_finalizer/finalize.rs:254` |
| `Stale` | 앞 턴의 늦은 terminal, `turn_finalizer/finalize.rs:188` |

## 결과 (TLC 1.7.4, `scripts/ci/statecharts_tlc.py`)

| cfg | 깨진 규칙 | 뜻 |
|---|---|---|
| `BeforeZero` | `MailboxEventuallyReleased` | 반례: Start → Restart → Kickoff(active 0) → Finalize miss → 토큰 영구 보유 |
| `BeforeReal` | - | 메시지 id가 있는 행은 수정 전에도 풀린다 |
| `AfterZero` | - | kickoff가 finalizer 키로 잡으면 풀린다 |
| `AfterReal` | - | 수정 후에도 그대로 |
| `WeakZero` | `NoWrongRelease` | 채널 단위 대안은 liveness는 지키지만 앞 턴의 늦은 terminal이 복구된 턴의 토큰을 푼다 |

## 범위 밖

- active id 0 로그의 다른 상태(메일박스가 이미 비어 토큰도 없음)는 중복 finalize이며 미해제가 아니다. 모델은 토큰을 쥔 채 active id가 0인 상태만 다룬다.
- watchdog·lease 세대는 모델에 없다.
