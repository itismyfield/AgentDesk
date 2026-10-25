# S2b 리뷰 P2 이월 (Opus, 2fca26599, VERDICT CLEAN, 10-24 07:39)

원문: `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i7272/review-impl-s2b-r1-claude.out.md`. 다음 조각 브리프에 넣는다. V2를 운영 유입에 연결하기 전에 닫아야 할 항목은 P2-1~P2-4다.
- P2-1. 정상 경로에서 디렉터리 sync 실패를 확인하지 않은 채 candidate를 채택하고 permit을 발급한다
- P2-2. 늦은 결과를 정산할 때 arm 시각을 settled_at으로 쓴다
- P2-3. tick과 settle이 head 한 개가 아니라 같은 identity를 가진 item 전부를 지운다
- P2-4. `enqueue`·`migrate_parked`는 실패하면 새 item을 돌려주지 않는다
- P2-5. read-only clear 응답이 다른 실패 원인과 세션 초기화 사실을 가린다
- P2-6. writer-gate skip pin은 cfg(test) 해제를 감지하지 못한다
- P2-7. legacy 복원이 attempt의 provider와 target을 고정값으로 채운다
- P2-8. 위생(묶음)
