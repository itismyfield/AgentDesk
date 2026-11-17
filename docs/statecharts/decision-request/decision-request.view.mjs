export const title = "선택 기록은 보존하고 주입은 한 작업으로 끝냄";
export const state = (v) => ({
 new:"글 게시 전", posting:"글 게시 중", open:"선택 대기", selected:"선택 저장",
 claimed:"현재 세션 해석", normalizing:"한 번 정상화", queued:"정상 busy 입력 대기",
 prepared:"현재 세션에 쓰기 준비", sending:"한 번 주입", unknown:"결과 확인",
 closing:"실행권 종결", failed:"전달 실패·입력 폐기", unknown_closed:"결과 불명·재전송 금지",
 delivered:"전달 확인", post_failed:"게시 실패"
})[v.phase] ?? v.phase;
export const step = (action) => ({
 Click:"세션 생존 검사 없이 유효 선택 저장", Claim:"시도 전 CAS 한 번",
 ResolvePresent:"현재 세션 해석", NormalizeAbsent:"세션 기동 정상화",
 NormalizeSuccess:"정상화 성공", NormalizeFailure:"실패 이유 확정",
 RefusalNormalize:"미전송 증거로 같은 작업 안에서 정상화", RefusalFinal:"정상화 뒤 실패 종결",
 TurnBoundary:"정상 턴 경계", DrainCurrent:"owner/non-owner drain에서 다시 현재 세션 해석",
 BeginSend:"원래 요청 세션이 아닌 현재 실행기의 physical fence",
 RemoteEffect:"기존 한 번의 쓰기가 적용", Observe:"기존 시도의 전달 증거",
 CloseUnknown:"전송 중 재시작: 다시 쓰지 않고 닫음", Finalize:"원본·재큐잉 권한 제거",
 ChangeSession:"선택 전 또는 대기 중 세션 교체", EndSession:"현재 세션 종료",
 Crash:"dcserver 재시작", Boot:"선택만 저장됐으면 한 번 시작",
 Archive:"글 자동 archive", ArchiveLocked:"글 잠김·archive", Unlock:"기존 권한으로 unlock",
 Unarchive:"같은 글 unarchive", Render:"같은 starter message에 최신 상태 PATCH",
 HandoffDecision:"잘못된 handoff 우회 대조", WakeFailed:"실패 원본 재큐잉 대조"
})[action] ?? action;
