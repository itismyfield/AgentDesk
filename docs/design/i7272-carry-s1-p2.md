# #7272 조각 1 리뷰 CLEAN(22:21) — 조각 2 재개 ack에 넣을 것

리뷰 원문 `review-impl-s1-r1-claude.out.md`

- **P2-1**: Codex proof의 `Recorded::matches` 필터를 고유하게 증명하는 시험이 없다. 조각 2 시험 보강으로 넣는다.
- **P2-2**: Unknown 자동 재전송 뮤턴트가 재시작 고유 단언이 아니라 공용 fixture 전제에서 죽는다. 조각 2에서 고유 단언으로 바꾼다.
- **P2-3**: Codex Herdr `provider_unsupported` veto가 durable MayHaveSent 뒤에 나온다. 새 veto 사유이므로 이월 분류표(조각 3~4 P1-1)에 넣는다. MayHaveSent 앞으로 옮길 수 있는지 조각 2 PLAN에서 판단한다.
- **P2-4**: attempt identity가 adapter 반환 타입에 드러나지 않는다. 조각 2 V2 owner 작업 때 타입으로 드러낸다.
