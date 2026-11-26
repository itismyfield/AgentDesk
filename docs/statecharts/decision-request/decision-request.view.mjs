export const title = "선택 기록은 보존하고 주입은 한 작업으로 끝냄";
export const state = (v) => ({
 new:"글 게시 전", posting:"글 게시 중", open:"선택 대기", selected:"선택 저장",
 claimed:"현재 세션 해석", normalizing:"한 번 정상화", queued:"정상 busy 입력 대기",
 prepared:"현재 세션에 쓰기 준비", sending:"한 번 주입", unknown:"결과 확인",
 closing:"실행권 종결", failed:"전달 실패·입력 폐기", unknown_closed:"결과 불명·재전송 금지",
 delivered:"전달 확인", post_failed:"게시 실패"
})[v.phase] ?? v.phase;
export const step = (action) => ({
 Click:"세션 생존 검사 없이 유효 선택 저장", Claim:"시도 전 CAS 한 번",
 ResolvePresent:"현재 세션 해석", NormalizeAbsent:"세션 기동 정상화",
 NormalizeSuccess:"정상화 성공", NormalizeFailure:"실패 이유 확정",
 RefusalNormalize:"미전송 증거로 같은 작업 안에서 정상화", RefusalFinal:"정상화 뒤 실패 종결",
 TurnBoundary:"정상 턴 경계", DrainCurrent:"owner/non-owner drain에서 다시 현재 세션 해석",
 BeginSend:"원래 요청 세션이 아닌 현재 실행기의 physical fence",
 RemoteEffect:"기존 한 번의 쓰기가 적용", Observe:"기존 시도의 전달 증거",
 CloseUnknown:"전송 중 재시작: 다시 쓰지 않고 닫음", Finalize:"원본·재큐잉 권한 제거",
 ChangeSession:"선택 전 또는 대기 중 세션 교체", EndSession:"현재 세션 종료",
 Crash:"dcserver 재시작", Boot:"선택만 저장됐으면 한 번 시작",
 Archive:"글 자동 archive", ArchiveLocked:"글 잠김·archive", Unlock:"기존 권한으로 unlock",
 Unarchive:"같은 글 unarchive", Render:"같은 starter message에 최신 상태 PATCH",
 HandoffDecision:"잘못된 handoff 우회 대조", WakeFailed:"실패 원본 재큐잉 대조",
 SelfDeclared:"승인 event 없이 source=decision을 자기 선언한 요청 대조",
 Journal:"쓰기 직전 may_have_sent 기록 한 번", ReleaseHeld:"입력창 사용 중: 미전송 증명 뒤 같은 원본으로 다시 대기",
 OwnerPending:"owner 수용 응답 불명: 닫고 그 원본만 제거", PreserveApproved:"승인된 의견의 원문 보존 frame",
 PeerRecover:"다른 노드의 부팅 복구: holder 실행이 끝난 시도만 종결", Sweep:"종결된 event의 남은 원본을 requeue·안내 전에 제거",
 Notice:"종결된 Decision 원본의 재전송 안내 대조", OtherNotice:"같은 채널 다른 사용자 원본의 안내",
 PostRejected:"게시 확정 거절", Reconcile:"응답 유실 뒤 같은 글 회수", Activate:"선택 처리 배선 뒤 버튼 활성",
 ThreadMessage:"decision 글의 메시지가 일반 intake로 새는 대조", MarkOffline:"죽은 노드를 offline으로 표시",
 OfflineSettle:"offline 전환 때 그 노드의 미종결 시도 정산", Bootstrap:"cluster 등록으로 노드 이름 확정",
 RefuseKept:"만료된 대기 원본 거절: bind는 지워지고 안내 기록만 남음"
})[action] ?? action;
