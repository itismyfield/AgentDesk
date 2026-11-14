export const title = '정확한 시도의 비공개 증거와 분류';
export const state = v => {
 const s = v.s;
 if (s.restored) return '기존 보류 notice 복원 완료';
 if (s.withheld && !s.recoveryAccepted) return '기존 보류 row';
 if (s.recoveryAccepted) return '새 보류 settlement';
 if (s.localProof) return 'local snapshot 증거 보유';
 if (s.observed) return '같은 root에서 snapshot 관측';
 if (s.proof.kind !== 'none' && !s.proof.sealedOrigin) return '임의 증거가 발급됨';
 if (s.accepted && !s.namesAtAccept) return '다른 시도가 분류됨';
 if (s.retrySpends > 0) return '확인된 재시도 권한을 한 번 소비';
 if (s.ack) return '분류 commit 응답 확인';
 if (s.accepted) return '일치한 시도 분류';
 if (s.proof.kind !== 'none') return '관측에 묶인 증거 보유';
 if (s.takes > 0) return '한 번만 결정에 넘긴 관측';
 return '아직 증거 없음';
};
export const step = action => ({
 TakeDecision: '결정자가 시도를 가져옴',
 IssueEvidence: '완결된 관측으로 증거 발급',
 ForgeEvidence: '관측 없이 임의 생성 허용 변이',
 Settle: '현재 행과 일치하면 분류',
 CommitAck: 'commit 응답 확인',
 SpendRetry: '재시도 권한 소비',
 RestoreWithheld: 'proof 없이 기존 notice 복원',
})[action] ?? action;
