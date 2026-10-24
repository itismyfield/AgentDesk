//! Operator notices for causes outside any turn, one per cause episode through the Unknown notice
//! owner: its receipt, retry and epoch rules decide when a notice is due and that it is sent once.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, Weak};

use tokio::time::Instant;

use super::{ExecutionIdentity, HERDR_CONFIG, NoticeObservation, Owner, Scope, lock};
use crate::services::session_host::managed_config::maintenance::OperatorNotice;

/// Operator faults have no presence to hold their owner, so it lives for the process.
static OPERATOR: LazyLock<Mutex<HashMap<Scope, Arc<Owner>>>> = LazyLock::new(Default::default);
/// Provider runtimes whose own bot posts operator notices to the channels they supervise.
static POSTERS: LazyLock<Mutex<Vec<Weak<super::super::LiveEffects>>>> =
    LazyLock::new(Default::default);

/// Lets operator notices post as this runtime's own bot, a post no runtime reads as turn input.
pub(in crate::services::discord::turn_presence) fn adopt(effects: &Arc<super::super::LiveEffects>) {
    let mut posters = lock(&POSTERS);
    posters.retain(|poster| poster.strong_count() > 0);
    posters.push(Arc::downgrade(effects));
}

/// One-line subtext naming the cause and what to do; never paths, backups or environment values.
fn text(cause: &str) -> String {
    let body = match cause {
        "herdr_config_restart_required" => {
            "herdr 서버 재시작 필요(설정 적용). 관리 config는 자동으로 맞췄지만 실행 중 서버의 E7은 미확인입니다. 계획된 서비스 관리자 재시작 뒤 doctor/E7을 확인하세요."
        }
        "herdr_job_env_mismatch" => {
            "실행 중 herdr 서버가 관리 home이 아닌 설정으로 떠 있습니다. herdr job의 HERDR_CONFIG_PATH·XDG_CONFIG_HOME을 확인한 뒤 계획 재시작하세요."
        }
        "herdr_managed_home_unsafe" => {
            "관리 herdr home을 안전하게 쓸 수 없어 config를 맞추지 못했습니다(권한·소유자·경로). agentdesk doctor로 원인을 확인하세요."
        }
        "herdr_config_record_unreadable" => {
            "관리 herdr home의 publication 기록을 읽을 수 없어 자동 보장을 멈췄습니다. agentdesk doctor의 수동 절차를 따르세요."
        }
        _ => "관리 herdr config를 맞추지 못했습니다. agentdesk doctor로 원인을 확인하세요.",
    };
    crate::services::discord::session_notice::subtext("⚠️", &format!("{body} (사유: {cause})"))
}

impl OperatorNotice {
    /// Moves this fault's episode to `cause` and returns where its notice stands. The runtime
    /// supervising the channel posts it; with none the notice stays pending and counts as unsent.
    pub(crate) async fn observe(&self, cause: Option<&'static str>) -> &'static str {
        let channel = self.channel;
        #[cfg(test)]
        if let Some(send) = tests::sender() {
            return self
                .observe_with(cause, move |text| std::future::ready(send(channel, text)))
                .await;
        }
        let poster = lock(&POSTERS)
            .iter()
            .filter_map(Weak::upgrade)
            .find(|poster| super::super::observes(&poster.provider, channel));
        match poster {
            Some(poster) => {
                let send = move |text| async move {
                    super::super::Effects::notice(&*poster, channel, text).await
                };
                self.observe_with(cause, send).await
            }
            // A recovery still ends the episode; only a post needs a poster.
            None if cause.is_none() => {
                self.observe_with(None, |_| async { Err("no poster".to_string()) })
                    .await
            }
            None => "pending",
        }
    }

    async fn observe_with<F, Fut>(&self, cause: Option<&'static str>, send: F) -> &'static str
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future<Output = Result<(), String>>,
    {
        let scope = Scope {
            root: self
                .root
                .clone()
                .or_else(crate::services::discord::runtime_store::runtime_root),
            provider: HERDR_CONFIG.into(),
            token_hash: self.fault.clone(),
            channel: self.channel,
        };
        let owner = lock(&OPERATOR)
            .entry(scope.clone())
            .or_insert_with(|| Owner::get(scope))
            .clone();
        let observed = cause.map(|reason| NoticeObservation {
            identity: ExecutionIdentity::Unresolved,
            target_label: None,
            reason,
            hold_leaf: None,
            progress: false,
        });
        if owner.observe(observed, true, None, Instant::now()) {
            owner.drive(move |reason| send(text(&reason))).await;
        }
        let state = lock(&owner.state);
        if state.key.is_none() {
            "none"
        } else {
            state.notice_state()
        }
    }
}

#[cfg(test)]
#[path = "operator_notice_tests.rs"]
mod tests;
