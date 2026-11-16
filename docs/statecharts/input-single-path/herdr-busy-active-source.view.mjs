export const title = "활성 Claude 턴 입력의 검증된 현재 source 선택";
export const state = (v) => {
  const r = v.r;
  if (r.phase === "running") return "기존 세션으로 턴 실행";
  if (r.phase === "changed") return "세션 교체, 현재 결속 확인 대기";
  if (r.phase === "published") return "현재 source 정상 발행 완료";
  if (r.phase === "ready") return r.knownBusy ? "현재 transcript에서 Busy 관측" : "현재 transcript 턴 관측 Unknown";
  if (r.phase === "queued") return r.inflight.path === 0 ? "사람 입력 도착, 활성 턴 행의 경로 없음" : "사람 입력 도착, 활성 턴 행은 옛 경로";
  if (r.phase === "captured") {
    if (r.request.path === 0) return "빈 transcript로 주입 준비";
    if (r.request.session !== r.historyCurrent.session) return "옛 transcript로 주입 준비";
    return r.observed ? "현재 source 관측으로 주입 준비" : "관측을 건너뛴 주입 준비";
  }
  if (r.phase === "held") return "입력 보류, 키 0회";
  if (r.phase === "done") return "입력 1회 전달·정산";
  if (r.used.session !== r.historyCurrent.session) return "옛 source로 키 실행";
  if (!r.knownBusy) return "턴 관측 Unknown인데 키 실행";
  return "검증된 현재 source로 키 1회";
};
export const step = (action) => ({
  ChangeSession: "compaction·resume으로 세션 교체",
  PublishSource: "기존 정상 producer가 현재 결속 발행",
  InspectTranscript: "발행 뒤 같은 파일의 첫 record·턴 상태 재독",
  PersonInput: "턴이 끝나기 전 사람 입력",
  SelectActive: "활성 mailbox 입력 경로에서 source 선택",
  SubmitActive: "첫 키 직전 source 검사",
  ActiveReceipt: "같은 transcript의 수신 증거",
  ActiveSettle: "한 번 정산",
  ActiveWait: "대기",
}[action] ?? action);
