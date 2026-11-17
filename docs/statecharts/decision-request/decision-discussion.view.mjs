export const title = "선택지 밖 의견과 요청자의 후속";
export const state = (v) => ({
 open:"열린 결정", selecting:"선택 저장·신규 입력 봉인", closed:"닫힘·새 writer 금지"
})[v.gate] ?? v.gate;
export const step = (action) => ({
 ObserveMessage:"gateway/scan/button의 고유 source 기록·분류",
 ScanSeen:"스캔 구간의 관측 사실 확인", AdvanceCursor:"낮은 ID까지 기록된 구간만 완료",
 Begin:"입력 event의 최초 workflow", TurnBoundary:"정상 busy 턴 경계",
 Write:"닫힘/중복 검사 후 한 번 쓰기", RemoteEffect:"이미 나간 단일 write의 적용",
 Confirm:"실제 전달 확인", NormalizeFailureDiscard:"R2 정상화 실패·입력 폐기",
 OpinionNotice:"실제 전달 뒤 의견 전달됨", FailureNotice:"실패 사유 표시",
 ChoiceCloses:"선택 terminal 뒤 닫기", AgentReply:"requester reply·논리 상태 불변",
 AgentClose:"requester 결론·닫힘 fence", MakeEditable:"같은 글의 표시 정비",
 PostReply:"provider 답글", PostConclusion:"provider 결론", RepairLifecycle:"archive+lock 복원",
 Crash:"재시작", Boot:"동일 source 원장 복원"
})[action] ?? action;
