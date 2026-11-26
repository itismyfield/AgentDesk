export const title = "Issue 8430: held human input is delivered once the session is Idle";
export const state = ({s}) => !s.up ? "재시작 중(보류 시계 초기화)" :
  s.sends > 1 ? "같은 입력 두 번 전달" :
  s.newCleared ? "옛 terminal이 새 턴 lease 해제" :
  s.boundBad ? "작업 중·초안·모달 화면에 상한 발동" :
  s.settle === "reacted" ? "상한 정산이 완료 반응을 냄" :
  s.front === "dropped" ? (s.notice === "identified" ? "묶음 전체를 특정한 안내와 함께 버림" : s.notice === "first" ? "첫 입력만 특정한 안내로 버림" : "어느 입력인지 모르는 안내로 버림") :
  s.front === "stalled" ? "남은 행에 막혀 깨움 없이 멈춤" :
  s.front === "done" ? "새 턴 완료" :
  s.front === "sent" ? (s.released ? "상한 정리 뒤 1회 전달" : "1회 전달") :
  s.front === "held" ? (s.released ? (s.tickOwed ? "행 정리됨, 기한 tick 대기" : s.kick ? "lease 정리됨, 완료 kick 대기" : "정리됨, 깨움 없음") : s.due !== "none" ? "보류(Readiness 기한 있음)" : s.clock < 2 ? "보류(시계 진행)" : s.streak >= 2 ? "보류(Idle 연속 관측됨)" : "보류(상한 시각 경과, Idle 확인 중)") :
  s.released ? "옛 턴 정리됨, 전달 대기" :
  s.clock >= 2 ? "상한 시각 뒤 재구동" : s.clock > 0 ? "보류 뒤 재구동" : "사람 입력 front 대기";
export const step = (action) => ({
  Drive: "owner drive", Rewake: "held_watch 2초 깨움", RewakeSaturated: "같은 보류 반복",
  Observe: "화면·transcript 관측", TurnEndWake: "QueueEligible 완료 kick",
  DueExpire: "Readiness 10초 기한 경과", DueDrop: "기한 만료로 front 버림",
  BoundFire: "상한: 옛 episode·행 정리(mute)", OldEnds: "옛 턴 실제 종료",
  OldTerminal: "옛 terminal 원장 정산", LateTerminal: "늦은 terminal 도착",
  NewEnds: "새 턴 종료", Flicker: "관측 실패/복구", ToolGap: "도구 사이 Idle처럼 보임",
  ToolResume: "도구 재개", Crash: "dcserver 재시작", Boot: "부팅·lease 재등록",
  DeadlineTick: "owner 기한 tick 깨움", ProducerEnds: "headless producer 종료",
})[action] ?? action;
