export const title = "Herdr clear 종료와 복원된 빈 워크스페이스";

const evidence = (s) => s.source === "verified" ? "기록 소스 확인" : "기록 소스 불명";

export const state = ({ s }) => {
  if (s.request !== undefined) {
    if (s.request === "idle") return "clear 전";
    if (s.request === "refused") return s.original === "unknown"
      ? "종료 대상 불명으로 거절" : "기록 소스가 없어 종료 거절";
    if (s.request === "interrupted") return "실행 종료 뒤 clear 응답 중단";
    if (s.request === "reported") return s.record === "warned"
      ? "실행 종료와 기록 경고 응답" : s.record === "written" ? "실행 종료와 종료 기록 완료"
      : s.original === "bound" ? "종료 기록 없이 clear 응답" : "종료 기록 대상 없는 clear 응답";
    if (s.record === "written") return "확정된 종료 기록 저장";
    if (s.record === "warned") return "종료 기록 경고";
    if (s.row === "retired") return s.original === "bound"
      ? "이전 실행 은퇴, " + evidence(s) : "이전 실행 종료 또는 기존 은퇴";
    if (s.row === "ended") return "정확한 실행 종료 확인";
    return s.row === "bound" ? "clear 요청, " + evidence(s) : "clear 요청";
  }
  if (s.workspace === "closed") return s.unsafe ? "증거가 충족되지 않은 창 삭제" : "빈 창 정리";
  if (s.workspace === "missing") return "복원 대상 없음";
  if (s.workspace === "agent") return "기존 provider 실행 중";
  if (s.workspace === "occupied") return s.activity === "used" ? "복원 창 사용 중" : "다른 프로세스가 실행 중";

  const label = s.capability !== "confirmed" ? "조건부 닫기 지원 불명"
    : s.lineage !== "verified" ? "복원 계보 불명"
    : s.activity === "used" ? "사용한 뒤 돌아온 셸"
    : s.activity !== "unused" ? "복원 이후 활동 불명"
    : s.work !== "empty" ? "남은 작업이 있거나 불명"
    : "미사용 빈 셸";
  const observed = s.ticket[1] === s.server && s.ticket[2] === s.revision;
  return label + (observed ? ", 현재 관측" : "");
};

const steps = {
  RequestClear: "clear 요청",
  LoseSource: "이전 소스 불명",
  RefuseUnknownRecord: "기록 소스가 없어 종료 거절",
  RefuseUnknownTarget: "실행 식별 불가로 거절",
  CloseExactOrObserveEnded: "이전 실행 종료 확인",
  RetireExact: "정확한 실행 은퇴 확정",
  RecordAfterEnd: "기록 저장 또는 경고",
  ReportClear: "clear 결과 응답",
  InterruptPostlude: "종료 직후 서버 중단",
  RestartServer: "Herdr 서버 재시작",
  UseRestoredShell: "복원 창에서 작업",
  ReturnToShell: "작업 뒤 셸로 복귀",
  LoseLineageEvidence: "이전 회복 근거 소멸",
  LoseNoWorkProof: "남은 작업 없음 증거 소멸",

  SurveyEmpty: "현재 복원 계보와 빈 셸 관측",
  CompareAndClose: "효과 시점 조건으로 닫기",
};

export const step = (action) => steps[action];
