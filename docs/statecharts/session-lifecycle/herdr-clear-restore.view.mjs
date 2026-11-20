export const title = "Herdr 이전 세션 종료 기록과 복원 워크스페이스";

const clear = {
  idle: "clear 전",
  refused: "종료 기록을 확인할 수 없음",
  reported: "종료 기록 확정과 clear 완료",
};

export const state = ({ s }) => {
  if (s.request !== undefined) {
    if (clear[s.request]) {
      return s.request === "reported" && s.record !== "written"
        ? "종료 기록 없이 clear 완료"
        : clear[s.request];
    }
    if (s.record === "written") return "종료 기록 저장";
    if (s.row === "retired") return "이전 실행 은퇴";
    if (s.row === "ended") return s.record === "staged"
      ? "이전 실행 종료 확인"
      : "기록 없이 이전 실행 종료";
    return s.record === "staged" ? "종료 기록 보관" : "clear 요청";
  }
  if (s.workspace === "closed") return s.unsafe ? "검증이 어긋난 workspace 삭제" : "빈 workspace 정리";
  if (s.workspace === "missing") return "복원 대상 없음";
  if (s.workspace === "agent") return "기존 provider 실행 중";
  if (s.workspace === "occupied") return "다른 프로세스가 실행 중";
  if (s.owner !== "verified") return "소유를 증명할 수 없는 셸";
  if (s.authority === "held") return "정리 권한 또는 기능 확인 대기";
  if (s.ticket[1] === s.server && s.ticket[2] === s.shell) return "현재 빈 셸 확인";
  if (s.server === 2 && s.changed) return "두 번째 복원 뒤 빈 셸 확인 대기";
  if (s.changed) return "pane 변경 뒤 빈 셸 확인 대기";
  if (s.server === 2) return "다시 복원된 빈 셸";
  return "복원된 빈 셸";
};

const steps = {
  RequestClear: "clear 요청",
  LoseSource: "이전 소스 불명",
  StageEndRecord: "이전 실행 증거 보관",
  RefuseUnknownRecord: "이전 기록 확인 실패 표시",
  CloseExactOrObserveEnded: "이전 실행 종료 확인",
  RetireExact: "정확한 실행 은퇴 확정",
  RestartDuringClear: "종료 직후 재시작",
  CommitEndRecord: "이전 종료 기록 저장",
  ReportClear: "clear 결과 보고",
  RestartServer: "Herdr 서버 재시작",
  SurveyEmpty: "현재 소유와 빈 셸 검증",
  WorkspaceChanges: "pane 프로세스 또는 세대 변경",
  CompareAndClose: "현재 조건에서 닫기",
};

export const step = (action) => steps[action];
