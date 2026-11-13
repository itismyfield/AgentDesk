export const title = "연속 실패 알림과 한 번의 복구 요청";
const seq = (value) => Object.values(value);
const program = (n) => [...Array(n+2).fill("failed"), "interrupted", "succeeded", ...Array(n+1).fill("failed"), "skipped", "failed", "paused"];
export const state = ({s}) => {
 const requests = seq(s.requests), native = seq(s.nativeKeys), at = seq(s.requestCounts);
 const plan = program(s.threshold);
 if (!s.allowedBot && native.length > 0) return "허용되지 않은 봇의 owner 효과";
 let expected = 0;
 for (const result of plan.slice(0, s.cursor-1)) expected = result === "interrupted" ? expected : result === "failed" ? Math.min(expected+1,s.threshold+2) : 0;
 const breaks = [];
 if (s.badNoticeTurn) breaks.push("일반 알림이 턴 시작");
 if (s.blindRepost) breaks.push("불확실 전송 재게시");
 if (new Set(requests).size !== requests.length) breaks.push("실패 묶음 중복 요청");
 if (new Set(native).size !== native.length) breaks.push("복구 턴 중복");
 if (at.some(count => count !== s.threshold+1)) breaks.push("N+1 경계 오류");
 if (expected !== s.count) breaks.push("실패 집계 불일치");
 if (s.cursor > plan.length && requests.length !== 2) breaks.push("새 묶음 요청 수 오류");
 if (s.maxMessages > 1) breaks.push("Discord 중복 게시");
 if (breaks.length) return breaks.join(" · ");
 if (s.phase === "pending") return "기존 outbox 전송 대기";
 if (s.phase === "posted" && s.outcome === "completion_unconfirmed") return "전송 불확실·재게시 없이 종결";
 if (s.phase === "posted") return "명령봇 메시지 도착";
 if (s.phase === "accepted") return s.allowedBot ? "허용 봇 입력 수용" : "허용 검사를 우회한 입력";
 if (s.phase === "closed") return "복구 요청 결과 기록";
 if (s.worker <= requests.length) return "N+1 복구 요청 저장됨";
 if (s.count === 0) return s.cursor > 1 ? "성공·비실패로 카운터 초기화" : "예약 실행 이력 대기";
 return s.count > s.threshold+1 ? "추가 실패·알림만" : "실패 누적";
};
const labels = {RecordRun:"terminal run 결과 반영", RelogRun:"같은 run producer 재진입", NotifyIntakeStarts:"일반 알림을 잘못 입력으로 수용", DuplicateTrigger:"다른 키로 중복 복구 요청", LoadOutbox:"기존 outbox가 전송 시작", PostOutbox:"명령봇으로 게시", BlindUnknownRepost:"확인 없이 같은 메시지 재게시", ExistingBotIntake:"기존 Discord intake 통과", NoAcceptedBotInput:"수용할 봇 메시지 없음", ExistingOwnerStarts:"기존 owner가 별도 복구 턴 시작", RecordAttemptOutcome:"전송 결과 종결", Done:null};
export const step = (action, from, to) => {
 if (action === "RelogRun") return seq(to.s.requests).length > seq(from.s.requests).length ? "같은 run을 키 없이 다시 요청" : "동일 사건 키 재진입·추가 요청 0";
 if (action === "ExistingBotIntake" && !to.s.allowedBot) return "허용 검사를 잘못 우회";
 if (action === "RecordRun") {
  if (seq(to.s.requests).length > seq(from.s.requests).length) return seq(to.s.requestCounts).at(-1) === to.s.threshold+1 ? "N+1회·명령봇 복구 요청" : "N회에 조기 복구 요청";
  if (to.s.notices > from.s.notices) return "실패 알림만·턴 0";
  return program(from.s.threshold)[from.s.cursor-1] === "succeeded" && to.s.count > 0 ? "성공 뒤 카운터를 잘못 유지" : to.s.count === 0 ? "비실패 terminal·카운터 0" : "interrupted·카운터 유지";
 }
 return Object.hasOwn(labels, action) ? labels[action] : action;
};
export const group = ({s}) => s.outcome === "completion_unconfirmed" ? "uncertain" : s.phase === "accepted" ? "accepted" : "normal";
export const colors = {uncertain:"orange", accepted:"teal", normal:"blue"};
