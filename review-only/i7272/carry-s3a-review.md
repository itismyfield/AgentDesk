# S3a 리뷰(Opus, CLEAN) P2 이월 — 다음 조각 브리프에 그대로 넣는다

- 출처: `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i7272/review-impl-s3a-r1-claude.out.md` (head 81fb8841a, 16:02)
- 배정
  - P2-1(보내지 않은 등록이 retained로 남아 같은 본문 확인을 막음) → 3c/3d 수용 기준. plan-s3.md:93의 Reserved cold 권한·KnownNotSent 단위와 같다. 리뷰의 재현 1~5와 `abandoned` 권고안, 뮤턴트를 그대로 쓴다.
  - P2-3(접수 실패 타입 없음 → 불명 publish 뒤 synthetic identity 재접수 중복) → 3b. plan-s3.md:150,372의 접수 실패 custody와 같은 축이다.
  - P2-2(변화 없는 history transaction도 전체 게시·fsync) → 3d. 비용 계측 축(r2 P2-2)과 같다.
  - P2-4(PG 재시작 시험이 MayHaveSent 복원을 지나지 않음) → 3c/3d 재시작 시험에서 고친다.
  - P2-5(위생) → 다음 조각에서 손대는 파일에 한해 같이 고친다.
- 3b 착수 조건: #7314 묶음(A1+A2+B)이 main에 들어간 뒤(plan-s3.md:275,283). B의 raw/선택 prelude 타입을 재사용하기 때문이다.
