# #7272 설계 r2 이월 계약 (코디네이터 결정 10-23 21:0x)

설계 리뷰는 2라운드 상한에 도달했다. r2 리뷰(`review-design-r2-claude.out.md`) 결과는 ISSUES, P1 1건과 P2 9건이다. r1의 P1 2건은 해소됐다(P1 2→1). 남은 P1은 P0가 아니므로 설계를 다시 열지 않는다. 리뷰어 권고대로 구현 조각의 수용 기준과 전제로 넘긴다. 설계 정본은 `design-r2.md`이고, 이 파일은 그 위에 얹는 이월 계약이다.

## 조각 1 (진행 중, 리뷰 확인 항목)
- adapter는 `(effect outcome, attempt identity)`만 반환한다. settle을 직접 부르지 않는다(P2-9-2). r2의 exact-attempt 정산(A04)과 EffectPermit을 조각 2에서 다시 고치지 않게 하려는 것이다.

## 조각 2
- V1 바이너리용 V2 읽기 전용 호환 reader(쓰기는 거절)를 넣는다. A24로 증명한다(P2-9-1).
- history V2(§3)를 조각 3으로 미룰 수 있는지 PLAN에서 먼저 잰다(P2-9-4).
- retained 종료 경로와 비교 비용(P2-2), 같은 본문 retained의 head 보류(P2-3), Parked 의미(P2-6), in-memory injected-steer 원장(P2-8)을 PLAN에 명시한다.

## 조각 3~4 (P1-1 수용 기준)
- 어댑터의 모든 `Veto`/NotSent 사유를 아래 세 부류로 나눈 표를 고정한다. 표 누락은 리뷰 차단 사유다.
  - (가) 준비 대기
    - 예: Modal, Draft, LockContended, PaneUnavailable, transition, ledger full
    - wake 신호(backstop 포함)와 사용자에게 보이는 대기 사유를 함께 둔다.
    - HumanAttached는 idle·cold submit에 legacy와 같은 draft 검사만 적용한다. attach 거절은 busy steer에만 유지한다.
  - (나) 능력 공백
    - 예: UnpredictableRender(빈 줄, 1000자 초과, 제어 문자), `NOT_TYPED_LEAD`(`<@`, `/`, `DISPATCH:`)
    - 조각 5 전에 그 provider×host 조합의 전달 수단을 갖춘다.
  - (다) 영구 입력 오류: 64 KiB 초과, 빈 본문 → Rejected와 알림
- 권고 구현: idle·cold submit은 busy 주입 화면 예측 대신 기존 legacy submit primitive를 owner의 permit·frame history·exact receipt로 감싸서 쓴다.
  - Claude: `send_followup_prompt_or_idle_transcript_owned`와 fresh paste
  - Codex: argv와 warm submit
  - busy 주입 어댑터는 busy steer에만 쓴다.
- 30초 확인 상한은 cold 부팅·준비 대기 시간을 빼고 effect 이후부터 잰다(P2-1). 그러지 않으면 느린 부팅마다 거짓 보관 알림이 간다.
- Codex `absence_launch` 내부 분기(P2-4)와 §8 누락 경로(P2-5)를 진입점표에 더한다. paste 잔재 draft 대기는 사유를 표시한다(P2-7).

## 조각 5 (fallback 제거) 전제
- (나) 부류가 남은 provider×host 조합은 제거 대상에서 뺀다.
- 수용 시험으로 다음을 실제 진입점에서 고정한다.
  - Codex tmux에서 두 문단 메시지가 전달된다.
  - attach 중 idle 입력이 전달된다.
  - 뒤 입력이 막히지 않는다.

## 착지 순서
- Codex 종료 봉인 레인(A2a/A2b, `codex/absence_launch.rs`·`codex_tui/warm_followup*` 소유)을 §7 착지 순서에 넣는다(P2-9-3).
