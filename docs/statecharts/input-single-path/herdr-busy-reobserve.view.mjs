export const title = "Slice2: 같은 원문의 재관측과 clear 위임";
export const state = ({r}) => r.terminal && !["unknownKeep", "discarded"].includes(r.phase)
  ? "처분된 원문이 부활함" : r.effects > 1 ? "같은 원문을 두 번 전달함" :
  r.wakeIssued && (!r.owned || r.terminal || [r.nonce,r.pane,r.front,r.generation].some(x => x !== 1))
  ? "소유하지 않은 대상·원문을 깨움" :
  ({queued: r.wakeIssued ? "같은 원문의 재평가" : "소유된 원문 도착",
    held: r.published ? r.notice ? "새 근거를 관측했으나 보류 표시 유지" : "새 근거 재관측 대기"
                     : r.notice ? "Unknown 보류 표시" : "Unknown 원문 보류",
    idle: "검증된 clear를 기존 Idle로 위임", may: "Busy 송신 경계",
    sent: "본문·Enter 기록, 영수증 대기", done: "정확히 한 번 정산",
    unknownKeep: "UnknownKeep 원문은 재송신하지 않음", discarded: "clear가 버린 원문은 부활하지 않음"})[r.phase];
export const step = action => ({Drive:"공통 대상 근거로 분기",Notify:"보류 표시",
  Publish:"같은 operation의 새 근거 도착",Reobserve:"현재 원문·대상으로 재관측",
  Reevaluate:"같은 원문 재평가 wake",Discard:"clear가 원문 처분",
  Write:"기존 gate로 본문과 Enter 기록",Receipt:"native 영수증 관측",Settle:"원문 정산",
  Wait:"현재 상태 유지"})[action] ?? action;
