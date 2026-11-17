export const title = "외부 Herdr 입력의 Idle 위임과 Busy 관측";
export const state = v => `${v.hphase} · ${v.hactivity} · epoch${v.hepoch}/proof${v.hreadEpoch} · 읽기예약${v.hreserved ? 1 : 0} · 턴${v.hstarted ? 1 : 0} 입력${v.hdelivered} 답${v.hreply}`;
export const step = action => ({
 HAuthorize: "외부 API 권한 확인", HAdmit: "효과 없는 사전검사 후 원본 채택",
 HObserve: "exact incarnation의 Idle/Busy 관측", HReturnRead: "Idle 읽기예약 반환",
 HBegin: "Idle 시작 또는 현재 Busy 턴 합류", HReceipt: "native 수신 증거",
 HPost: "원래 목적지로 답 게시", HReady: "현 incarnation 준비 후 원본 깨움",
 HNativeEnd: "머신 own-turn은 현재 native 턴 끝을 기다림",
 HRestart: "재시작: 옛 proof 폐기 후 재관측", HFail: "채택 뒤 명시 실패", HDone: "종료 유지"
}[action] ?? action);
