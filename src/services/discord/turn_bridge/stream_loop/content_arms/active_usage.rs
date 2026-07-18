use super::*;

pub(in super::super) async fn compact_lower_bound_tokens(
    provider: &ProviderKind,
    tmux_session_name: Option<&str>,
    api_port: u16,
) -> u64 {
    if matches!(provider, ProviderKind::Claude) && tmux_session_name.is_some() {
        super::super::super::super::adk_session::fetch_context_thresholds(api_port)
            .await
            .compact_lower_bound_tokens
    } else {
        crate::services::claude_compact_context::DEFAULT_CONTEXT_COMPACT_LOWER_BOUND_TOKENS
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn observe_active_usage_from_status(
    channel_id: ChannelId,
    tmux_session_name: Option<&str>,
    provider: &ProviderKind,
    model: Option<&str>,
    input_tokens: Option<u64>,
    cache_create_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    compact_percent: u64,
    lower_bound_tokens: u64,
    observe: impl FnOnce(
        u64,
        &str,
        &ProviderKind,
        Option<&str>,
        Option<u64>,
        Option<u64>,
        Option<u64>,
        u64,
        u64,
    ) -> bool,
) -> bool {
    let Some(tmux_session_name) = tmux_session_name else {
        return false;
    };
    observe(
        channel_id.get(),
        tmux_session_name,
        provider,
        model,
        input_tokens,
        cache_create_tokens,
        cache_read_tokens,
        compact_percent,
        lower_bound_tokens,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn complete_status_snapshot_reaches_active_usage_observer() {
        type Observation = (
            u64,
            String,
            ProviderKind,
            Option<String>,
            Option<u64>,
            Option<u64>,
            Option<u64>,
            u64,
            u64,
        );
        let observed: Arc<Mutex<Option<Observation>>> = Arc::new(Mutex::new(None));
        let recorded = Arc::clone(&observed);

        let accepted = observe_active_usage_from_status(
            ChannelId::new(42),
            Some("tmux-active-4631"),
            &ProviderKind::Claude,
            Some("routed-sonnet[1m]"),
            Some(560_000),
            Some(0),
            Some(0),
            50,
            300_000,
            move |channel_id,
                  tmux_session_name,
                  provider,
                  model,
                  input_tokens,
                  cache_create_tokens,
                  cache_read_tokens,
                  compact_percent,
                  lower_bound_tokens| {
                *recorded.lock().expect("observation lock") = Some((
                    channel_id,
                    tmux_session_name.to_string(),
                    provider.clone(),
                    model.map(str::to_string),
                    input_tokens,
                    cache_create_tokens,
                    cache_read_tokens,
                    compact_percent,
                    lower_bound_tokens,
                ));
                true
            },
        );

        assert!(
            accepted,
            "the production status seam must call the observer"
        );
        assert_eq!(
            observed.lock().expect("observation lock").as_ref(),
            Some(&(
                42,
                "tmux-active-4631".to_string(),
                ProviderKind::Claude,
                Some("routed-sonnet[1m]".to_string()),
                Some(560_000),
                Some(0),
                Some(0),
                50,
                300_000,
            )),
            "the raw complete snapshot and launch identity must cross the content arm unchanged"
        );
    }

    #[test]
    fn status_snapshot_without_physical_tmux_does_not_call_observer() {
        let accepted = observe_active_usage_from_status(
            ChannelId::new(42),
            None,
            &ProviderKind::Claude,
            Some("routed-sonnet[1m]"),
            Some(560_000),
            Some(0),
            Some(0),
            50,
            300_000,
            |_, _, _, _, _, _, _, _, _| panic!("observer must not run without tmux identity"),
        );
        assert!(!accepted);
    }
}
