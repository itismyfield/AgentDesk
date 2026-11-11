export const title = "Codex marker-only A-admission r6: 반환되는 DB I/O";
export const state = v => {
  const a = v.handoff;
  if (a.phase === "done") return a.raw === "delivered" ? "원래 입력 전달 완료" : a.raw === "settled" ? "이미 정산한 wake 확인" : "원문 보존·명시 통지";
  if (a.phase === "retry_wait") return a.controller ? "같은 callback 재관찰" : "controller 유실";
  return ({observe:"부재·소유권 관찰",intent:"정리 intent 저장",recheck:"fence 아래 재검증",unlink_canonical:"canonical 표식 정리",unlink_alias:"legacy 표식 정리",commit:"정리 receipt 저장",outbox:"wake debt 저장",wake:"owner 깨우기",admit:"typed 부재 증거 소비",grant:"같은 front grant",create:"Pending·새 실행",prompt:"원래 입력 전달",ack:"결과 확인·debt 정산",notice:"원문 보존·통지 정산",boot:"부팅 discovery",recover:"intent 복구·재관찰",wait:"Legacy 증거 미소비"})[a.phase] ?? a.phase;
};
export const step = (action, from, to) => {
  const a = from.handoff, b = to.handoff;
  if (a.phase === b.phase) return "증거 변경 또는 wake 재배송";
  if (b.phase === "boot") return "서버 crash";
  if (b.phase === "retry_wait") return "반환된 I/O 오류, 실패 예산 소비";
  if (a.phase === "retry_wait") return "같은 callback 계속";
  if (b.phase === "notice") return "stale 또는 부재 증거 거절";
  if (a.phase === "notice") return "원문 보존·명시 통지";
  return ({intent:"부재 증거 확인",recheck:"intent durable",unlink_canonical:"현재 owner·행·파일 일치",unlink_alias:"같은 canonical만 unlink",commit:"같은 legacy만 unlink",outbox:"정리 receipt durable",wake:"안정 event id 저장",admit:"기존 owner drive",grant:"좁은 Legacy/Missing 증거 소비",create:"현재 front claim",prompt:"기존 Pending CAS",ack:"원래 입력 전달 또는 기존 정산 확인",done:"debt ack",recover:"부팅 intent 복구",wait:"Legacy 증거 미소비"})[b.phase] ?? action;
};
