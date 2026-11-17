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
            nonce: super::first_prompt_digest(&proof.nonce),
            execution: super::first_prompt_digest(&proof.context.execution_nonce),
            source: super::first_prompt_digest(
                &serde_json::to_string(&proof.source).expect("source identity"),
            ),
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
        // One total budget includes every retry and wake; notice delivery has a separate metadata-only timeout.
        Self {
            id,
            deadline: admitted_at + Duration::from_secs(25),
            state: Arc::new(Mutex::new((Phase::Normalizing, false))),
        }
    }
    pub(crate) async fn restore(
        pool: &sqlx::PgPool,
        id: Identity,
        admitted_at: Instant,
    ) -> Result<Self, String> {
        let recorded = crate::db::tui_output_disposition::record(pool, &id).await?;
        let output = Self::new(id, admitted_at);
        if let Some((reason, receipt)) = recorded {
            output.reject(reason);
            if let Some(receipt) = receipt {
                output.confirm(&receipt);
            }
        }
        Ok(output)
    }
    pub(crate) async fn persist_rejection(&self, pool: &sqlx::PgPool) -> Result<(), String> {
        let reason = match self.phase() {
            Phase::RejectPendingNotice(reason) | Phase::Rejected(reason) => reason,
            _ => return Err("only a terminal rejection may persist a tombstone".into()),
        };
        crate::db::tui_output_disposition::persist_intent(pool, &self.id, reason).await
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
    pub(crate) fn adopt(
        &self,
        proof: &FirstProof,
        validate: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        if Self::identity_mismatch(self.id(), proof) {
            return Err("output proof identity changed".into());
        }
        let mut state = self.state.lock().unwrap();
        if state.0 != Phase::Normalizing || Instant::now() >= self.deadline {
            return Err("output is terminal or expired".into());
        }
        proof.with_current(|| {
            validate()?;
            if Instant::now() >= self.deadline {
                return Err("normalization deadline expired".into());
            }
            state.0 = Phase::Adopted;
            Ok(())
        })
    }
    pub(crate) fn adopt_resumed(
        &self,
        proof: &crate::services::tui_o::writer::normalization::continuation::ContinuationProof,
        candidate: &super::Candidate,
    ) -> Result<(), String> {
        let guard = candidate.lock();
        if *guard != super::Adoption::Committed {
            return Err("resumed output has no O authority".into());
        }
        if &proof.identity() != self.id() {
            return Err("resumed output proof identity changed".into());
        }
        let mut state = self.state.lock().unwrap();
        if state.0 != Phase::Normalizing || Instant::now() >= self.deadline {
            return Err("resumed output is terminal or expired".into());
        }
        proof.with_current(|| {
            if Instant::now() >= self.deadline {
                return Err("normalization deadline expired".into());
            }
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
        if receipt.key() != self.id.key()
            || receipt.reason() != reason
            || receipt.template_version() != 1
            || receipt.message() == 0
        {
            return false;
        }
        state.0 = Phase::Rejected(reason);
        true
    }
}

#[path = "output_disposition_tests.rs"]
mod tests;
