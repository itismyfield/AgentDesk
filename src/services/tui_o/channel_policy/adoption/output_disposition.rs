//! Output identity and terminal choice, kept behind the D1 installation boundary.
use super::FirstProof;
use std::sync::{Arc, Mutex};
use tokio::time::{Duration, Instant};

pub(crate) use crate::db::tui_output_disposition::{Identity, Reason};
impl Identity {
    pub(crate) fn from_proof(proof: &FirstProof, input: u64) -> Self {
        Self {
            channel: proof.channel,
            input,
            nonce: super::digest(&proof.nonce),
            execution: super::digest(&proof.context.execution_nonce),
            source: super::digest(&serde_json::to_string(&proof.source).expect("source identity")),
        }
    }
}
impl Reason {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Deadline => {
                "출력 연결 정상화 시간이 만료되어 응답을 게시하지 않았습니다. 답은 터미널 세션에 남아 있습니다."
            }
            Self::Invalidated => {
                "이 실행이 취소되거나 교체되어 응답을 게시하지 않았습니다. 답은 터미널 세션에 남아 있습니다."
            }
            Self::SourceRejected => {
                "응답의 실행·출처 연결을 검증하지 못해 게시하지 않았습니다. 답은 터미널 세션에 남아 있습니다."
            }
            Self::TrustUnavailable => {
                "작업 폴더 신뢰 또는 로그인을 정상화하지 못해 응답을 게시하지 않았습니다. 터미널 세션을 확인해 주세요."
            }
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Normalizing,
    Adopted,
    RejectPendingNotice(Reason),
    Rejected(Reason),
}
#[derive(Clone)]
pub(crate) struct Output {
    id: Identity,
    deadline: Instant,
    state: Arc<Mutex<(Phase, bool)>>,
}
impl Output {
    pub(crate) fn new(id: Identity, admitted_at: Instant) -> Self {
        // One total budget includes every retry and wake; reserve five seconds for notice delivery.
        Self {
            id,
            deadline: admitted_at + Duration::from_secs(25),
            state: Arc::new(Mutex::new((Phase::Normalizing, false))),
        }
    }
    pub(crate) fn id(&self) -> &Identity {
        &self.id
    }
    pub(crate) fn phase(&self) -> Phase {
        self.state.lock().unwrap().0
    }
    pub(crate) fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
    pub(crate) fn expire(&self) -> bool {
        if Instant::now() < self.deadline {
            return false;
        }
        self.reject(Reason::Deadline)
    }
    pub(crate) fn reject(&self, reason: Reason) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.0 != Phase::Normalizing {
            return false;
        }
        state.0 = Phase::RejectPendingNotice(reason);
        true
    }
    pub(crate) fn adopt(&self, proof: &FirstProof) -> Result<(), String> {
        if Self::identity_mismatch(self.id(), proof) {
            return Err("output proof identity changed".into());
        }
        let mut state = self.state.lock().unwrap();
        if state.0 != Phase::Normalizing || Instant::now() >= self.deadline {
            return Err("output is terminal or expired".into());
        }
        proof.with_current(|| {
            state.0 = Phase::Adopted;
            Ok(())
        })
    }
    fn identity_mismatch(id: &Identity, proof: &FirstProof) -> bool {
        *id != Identity::from_proof(proof, id.input)
    }
    pub(crate) fn take_repair(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.0 != Phase::Normalizing || state.1 || Instant::now() >= self.deadline {
            return false;
        }
        state.1 = true;
        true
    }
    pub(crate) fn confirm(&self, receipt: &crate::db::tui_output_disposition::Receipt) -> bool {
        let mut state = self.state.lock().unwrap();
        let Phase::RejectPendingNotice(reason) = state.0 else {
            return false;
        };
        if receipt.key() != self.id.key() {
            return false;
        }
        state.0 = Phase::Rejected(reason);
        true
    }
}

#[path = "output_disposition_tests.rs"]
mod tests;
