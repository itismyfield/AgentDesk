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
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct EffortRestart<Original> {
    pub(crate) operation: String,
    pub(crate) native_session: String,
    pub(crate) interrupted_episode: Option<String>,
    pub(crate) desired_revision: u64,
    pub(crate) ready: bool,
    pub(crate) continuation: Option<Continuation<Original>>,
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
        self.continuation.as_ref().filter(|c| !c.submitted)
    }

    pub(crate) fn context(&self) -> serde_json::Value {
        serde_json::json!({"origin":"headless","source":SOURCE,"metadata":{
            "effort_restart":{"operation":self.operation,"native_session":self.native_session,
                "interrupted_episode":self.interrupted_episode}}})
    }
}
