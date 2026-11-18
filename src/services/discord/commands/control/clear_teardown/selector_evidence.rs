//! Retire evidence of the exact local selector a guarded clear committed.

use super::{ClearReport, ProviderKind, SharedData};
use crate::db::dispatched_sessions::ClearedSelector;
use crate::services::discord::adk_session::build_session_key_candidates;
use crate::services::tmux_common::{
    runtime_hint_fence::RuntimeHintFence, session_evidence, with_tmux_source_authority,
};

pub(super) async fn retire(
    shared: &SharedData,
    key: &str,
    provider: &ProviderKind,
    tmux: &str,
    cleared: Vec<ClearedSelector>,
    report: &mut ClearReport,
) {
    if !matches!(provider, ProviderKind::Claude | ProviderKind::Codex)
        || !build_session_key_candidates(&shared.token_hash, provider, tmux)
            .iter()
            .any(|candidate| candidate == key)
    {
        return;
    }
    let provider = provider.as_str();
    let retired: Vec<_> = cleared
        .into_iter()
        .filter(|row| {
            row.provider
                .as_deref()
                .is_some_and(|actual| actual.eq_ignore_ascii_case(provider))
        })
        .filter_map(|row| row.selector)
        .filter(|selector| !selector.trim().is_empty())
        .collect();
    if retired.is_empty() {
        return;
    }
    let forget = || async {
        let _fence = RuntimeHintFence::acquire_async(tmux)
            .await
            .map_err(anyhow::Error::msg)?;
        with_tmux_source_authority(tmux, |authority| {
            for selector in &retired {
                session_evidence::retire(authority, provider, selector)
                    .map_err(anyhow::Error::msg)?;
            }
            Ok(())
        })
    };
    report.attempt("session_evidence", forget).await;
}
