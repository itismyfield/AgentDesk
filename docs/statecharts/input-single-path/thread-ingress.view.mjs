export const title = "API 입력을 같은 스레드 세션으로 전달";
export const state = (v) => ({
 request: "외부 입력 도착", authorized: "대상 권한 확인", held: v.session === "live" ? "준비됐지만 보관 입력 미기상" : "부팅 완료 대기",
 queued: v.delivered ? "시작한 입력 재주입 대기" : "원본 인계 보관",
 submitted: "세션에 한 번 입력", done: "스레드 답 게시", failed: "보이는 실패",
 denied: v.status === 503 ? "부모 확인 실패로 거절" : "403 거절",
 failurePending: "실패 알림 보관", uncertain: "재시작 뒤 전송 불명", dropped: "원본 유실"
}[v.phase]);
const labels = {
 Authorize: "부모와 에이전트 바인딩 확인", Admit: "검증한 원본 인계",
 Ready: "세션 준비와 보관 입력 깨움", BootFails: "부팅 실패를 원본에 돌려줌", ColdStart: "첫 메시지 시작 경로가 입력 소유",
 WarmSubmit: "살아 있는 실제 세션에 입력", PublishReply: "같은 스레드에 답 게시",
 StartOrSubmitFails: "시작 또는 주입 실패", ShowFailure: "실패를 실제로 알림",
 Restart: "재시작 후 보관 원본 복구", Done: "종료 상태 유지"
};
export const step = (action) => labels[action] ?? action;
