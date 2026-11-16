//! One restart's continuation identity survives admission retries and daemon generations.
use serde::{Deserialize, Serialize};

pub(crate) const SOURCE: &str = "agentdesk_effort_continue";
pub(crate) const PROMPT: &str =
    "effort 변경 처리를 위해 세션을 다시 시작했으니 중단된 작업을 이어서 진행하라";

#[derive(Clone, Deserialize)]
pub(crate) struct Request {
    pub(crate) operation: String,
    pub(crate) native_session: String,
    pub(crate) interrupted_episode: Option<String>,
    #[serde(default)]
    pub(crate) desired_revision: u64,
    #[serde(default)]
    pub(crate) ready: bool,
    #[serde(default)]
    pub(crate) cut: Option<Cut>,
}

/// The process a restart is about to end, so a restarted server can tell whether it did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Cut {
    pub(crate) tmux: String,
    #[serde(default)]
    pub(crate) bind_nonce: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct EffortRestart<Original> {
    pub(crate) operation: String,
    pub(crate) native_session: String,
    pub(crate) interrupted_episode: Option<String>,
    pub(crate) desired_revision: u64,
    pub(crate) ready: bool,
    pub(crate) continuation: Option<Continuation<Original>>,
    /// Closed without its continuation: the user dropped it, the end failed, or its start failed
    /// before any key was sent. Kept so a late ready cannot reopen the same operation.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) cancelled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cut: Option<Cut>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Continuation<Original> {
    pub(crate) ticket: String,
    pub(crate) original: Original,
    #[serde(default)]
    pub(crate) submitted: bool,
}

impl<Original> EffortRestart<Original> {
    pub(crate) fn pending(&self) -> Option<&Continuation<Original>> {
        let open = !self.cancelled;
        self.continuation.as_ref().filter(|c| open && !c.submitted)
    }

    pub(crate) fn context(&self) -> serde_json::Value {
        serde_json::json!({"origin":"headless","source":SOURCE,"metadata":{
            "effort_restart":{"operation":self.operation,"native_session":self.native_session,
                "interrupted_episode":self.interrupted_episode}}})
    }
}
