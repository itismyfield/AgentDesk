export const title = "Antigravity 채널의 턴 소유와 게시";

export function state({ agy }) {
  switch (agy.phase) {
    case "waiting": return agy.oldDebt ? "이전 턴 영수증 대기" : "채택 가능";
    case "ready": return "새 입력 가능";
    case "reserved": return "스풀과 턴 소유권 확보";
    case "running": return "프로세스 출력 기록 중";
    case "sealed": return "최종 기록 보관됨";
    case "delivery":
      if (!agy.authority) return "새 게시 권한 대기";
      if (agy.alarm) return "실패 안내 기록됨";
      if (agy.offset === 3) return "모든 조각 확인됨";
      return "본문 분할 게시 중";
    case "finishing": return agy.authority ? "종료 의무 적용 중" : "새 게시 권한 대기";
    case "ended": return agy.spool ? "턴 종료됨" : "정리 완료";
    case "headless": return "루틴 기존 실행 중";
    case "headlessDone": return "루틴 기존 응답 완료";
  }
}

const labels = {
  AgyLegacyReceipt: "이전 턴의 자체 영수증 확보",
  AgyAdopt: "채택 커밋",
  AgyReserve: "입력 구속",
  AgySpawn: "한 턴 프로세스 시작",
  AgyAppendDelta: "응답 조각 기록",
  AgyAppendDone: "authoritative Done 내구 기록",
  AgyChildFailed: "종료 원인 내구 기록",
  AgyReadSeal: "확정 본문과 조각 계획 읽기",
  AgyLoseAuthority: "게시 권한 회수",
  AgyRestore: "현재 소유자가 복원",
  AgyRejectFinish: "종료 저장 거부",
  AgyPrepare: "게시 시도 기록",
  AgyPost: "메시지 생성",
  AgyReceipt: "조각 영수증과 오프셋 기록",
  AgyNotifyFailure: "실패를 운영 경로에 표시",
  AgyFinish: "종료를 한 번 확정",
  AgyApplyEnd: "종료 효과의 다음 항목 적용",
  AgyArchive: "정리 자격 검증 뒤 삭제",
  AgyNextInput: "다음 입력 수용",
  AgyHeadlessAnswer: "기존 루틴 경로에서 응답",
  AgyLegacyRace: "이전 게시자가 같은 턴을 송신",
  AgyBlindRepost: "확인 없이 같은 조각을 다시 게시",
  AgyRepeatDone: "Done을 다시 읽음",
  AgyStaleChild: "이전 프로세스의 이벤트를 수용",
  AgyPrematurePost: "확정 전 델타를 게시",
  AgyRest: "기록 변화 없음",
};

export const step = (action) => labels[action] ?? null;
