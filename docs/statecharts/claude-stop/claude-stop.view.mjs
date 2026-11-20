export const title = "Claude /stop Esc 전달 판정";
const running = v => `실행 중 · ${v.identity === "unread" ? "식별 미판독" : v.tail === "streaming" ? "응답 기록됨" : "꼬리가 user 행"} · ${v.screen === "footer" ? "esc to interrupt" : "스피너"}`;
export const state = v => v.verdict === "sent" ? "Esc 보냄" : v.verdict === "refused" ? "Esc 안 보냄" : v.turn === "none" ? "유휴 입력창" : v.turn === "submitted" ? "제출됨 · 스피너 전" : v.turn === "done" ? (v.screen === "idle" ? "턴 종료" : "턴 종료 · 화면 갱신 전") : running(v);
export const step = a => ({Submit:"프롬프트 제출",Start:"작업 시작",Reply:"응답 기록",ToolReturns:"도구 즉시 반환",Outgrow:"꼬리 읽기보다 길어짐",Finish:"턴 종료",FinishLate:"종료 기록이 화면보다 먼저",Stop:"!stop 판정"}[a] ?? null);
