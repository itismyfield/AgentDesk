# Native input B2 — 활성화 전 경계

기준 B1 `ccd3752cbae7b1a5c451809dddf5c509d88bc72c`. B1 리뷰 P2 세 건의 fixture는 같은 O capture를 재생한다.

| 경로 | 수정 전 Rust | 수정 뒤 Rust | 모델 RED → GREEN |
|---|---|---|---|
| receipt=two, one 먼저 close | foreign one 결과 | 첫 close0, 전체 two1 | ReceiptBefore: NoForeignReceiptProof·EvidenceEventuallyOwn 위반 → ReceiptFixed 위반0 |
| popAll 뒤 다음 원본의 dequeue | 제거된 원본이 dequeue 소비 | popAll FIFO 해제, 다음 원본1 | PopAllBefore: PopAllNoDebt·EvidenceEventuallyOwn 위반 → PopAllFixed 위반0 |
| Taken/Attached 뒤 다른 본문의 opener | Pending 지속 | 보존된 take/attachment 범위로 다음 run 가입 | OpenBefore: EvidenceEventuallyOwn 위반 → OpenFixed 위반0 |

수정 전 선택3=실행3=실패3, ignored0. 수정 후 native 선택21=실행21=통과21, ignored0. 실제 PG17.11, 단일 시험 스레드. 일반 remove의 늦은 dequeue fixture는 유지한다. popAll에만 넣던 인위적인 추가 dequeue를 없애고 추가 dequeue 없는 독립 fixture를 더했다.

`NativeInputPreparation.tla`의 review는 receipt one/two·FIFO 잔여·Taken/Attached 가입을 유한 추상화한다. source/receipt 유일성의 구체 조건은 Rust fixture와 뮤턴트가 검증하며 모델의 무조건적인 입장 허가로 대체하지 않는다. 기존 pre-key Prepare/AnyKey/NotSent·explicit primary pin 네 cfg도 계속 검사한다.

`statecharts_tlc.py --root <동일 바이트 scratch> --jobs 2`: NativeInputAttribution20 + NativeInputPreparation10 = 30cfg, rc0, 기대 위반 집합 일치, 121.7초. 원시 로그 `/Users/itismyfield/.adk/lane-runs/i8148/logs/b2-tlc.txt`, 원본 바이트 SHA 목록 `b2-tlc-source-sha.json`.

NativeInputAttribution의 결과→CAS→row→wake·boot·WARN 추상 전이는 B2 후속 활성화 대상이다. 현재 운영 완료 actor가 이를 소비했다고 주장하지 않는다. no-key 원본은 완료/철회가 아니며 시간은 종결 증거가 아니다.
