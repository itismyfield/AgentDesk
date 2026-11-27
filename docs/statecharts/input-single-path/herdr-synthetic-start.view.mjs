export const title = "8422: 죽은 Bound 실행과 synthetic start (rev2)";
export const state = ({s}) => s.badRetire ? "살아 있거나 증명 안 된 실행을 자동 retire함" :
  s.turnRetire ? "턴이 잡은 실행을 자동 retire함" :
  s.front === "rejected" && s.outcome !== "failed" ? "거절 입력을 completed로 정산함" :
  ({queued: s.awake ? (s.row === "Retired" ? "retire 뒤 재판정 대기" : "start front 도착(claim 전)") : "Event 대기(깨울 것 기다림)",
    held: s.watch ? (["missing","shell"].includes(s.exec) ? "죽은 Bound에 held_watch 보류" : "held_watch 보류(실행 판독 대기)") : "watch 없이 보류(깨울 것 없음)",
    retiring: s.lease ? "transition lease 아래 retire 진행" : "lease 없이 retire 진행",
    claimed: s.refusing ? "거절로 claim" : "claim 됨",
    delivered: s.cold ? "새 실행(Cold)으로 전달" : "기존 실행으로 전달",
    rejected: "이유와 함께 Failed로 정산"})[s.front];
export const step = action => ({Drive:"start_waits 판정", WatchLook:"held_watch 2초 판독", Cas:"retire CAS",
  Launch:"claim 뒤 실행", NativeEnds:"native 턴 끝", Readable:"판독 회복", InflightRead:"inflight 판독 회복",
  Die:"실행 소멸", TurnStart:"다른 start의 token 등록", TurnEnd:"턴 경계 wake",
  OperatorRetire:"운영자 terminate(retired wake)", Done:"정산 유지"})[action] ?? action;
