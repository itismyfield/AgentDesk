export const title = "Claude /stop Esc 전달 판정";
const tail = v => v.identity === "unread" ? "식별 미판독" : ({streaming:"응답 기록됨", user_submitted:"꼬리가 user 행", unknown:"꼬리가 세션 메타데이터", idle:"꼬리가 종료"})[v.tail];
const live = v => v.screen === "footer" ? "esc to interrupt" : "스피너";
const words = (v, where) => v.screen === "words" ? ` · ${where}에 chrome 문구` : "";
const done = v => v.screen === "idle" || v.screen === "words" ? `턴 종료${words(v, "응답")}` : `턴 종료 · 화면 갱신 전 · ${live(v)}`;
export const state = v => v.verdict === "sent" ? "Esc 보냄" : v.verdict === "refused" ? "Esc 안 보냄" : v.turn === "none" ? "유휴 입력창" : v.turn === "submitted" ? `제출됨 · 스피너 전${words(v, "프롬프트")}` : v.turn === "done" ? done(v) : `실행 중 · ${tail(v)} · ${live(v)}`;
export const step = a => ({Submit:"프롬프트 제출",Start:"작업 시작",Reply:"응답 기록",ToolReturns:"도구 즉시 반환",Metadata:"세션 메타데이터 기록",Outgrow:"꼬리 읽기보다 길어짐",Finish:"턴 종료",FinishLate:"종료 기록이 화면보다 먼저",Stop:"!stop 판정"}[a] ?? null);
