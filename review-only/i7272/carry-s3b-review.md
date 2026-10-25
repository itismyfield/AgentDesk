# S3b 리뷰(Claude Opus, CLEAN) 이월 — S4 착수 전에 닫을 것
- P2-1: Custody 완료 판정이 `owner.bytes == candidate`에만 기댄다. S4가 boot·tick 복구에서 `recover()`를 부르면 영구 Pending이나 중복 재접수가 생길 수 있다. 권고: 접수 seq 기록(원문 내용 검색은 같은 synthetic raw 두 번을 구분하지 못한다).
- P2-3: `RetainedOpaque`가 보내지 않은 것이 확실한 원문을 `Unknown`+`Unconfirmed` 알림으로 보관한다. S4 연결 뒤 "전달 미확인"이라는 틀린 안내가 된다(실제는 "보관됨, 재전송 필요"). 유실은 없다.
- 원문: review-impl-s3b-r1-claude.out.md
