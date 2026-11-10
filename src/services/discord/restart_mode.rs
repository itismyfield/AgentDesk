pub(crate) use crate::domain::lifecycle::InflightRestartMode;

#[cfg(test)]
mod tests {
    use super::InflightRestartMode;
    use crate::services::provider::CancelToken;
    use std::sync::atomic::Ordering;

    #[test]
    fn legacy_restart_mode_matches_cancel_token_storage() {
        let token = CancelToken::new();
        assert_eq!(token.restart_mode(), None);
        assert_eq!(token.restart_mode.load(Ordering::Relaxed), 0);

        for (mode, numeric) in [
            (InflightRestartMode::DrainRestart, 1),
            (InflightRestartMode::HotSwapHandoff, 2),
        ] {
            let canonical: crate::domain::lifecycle::InflightRestartMode = mode;
            let legacy: crate::services::discord::InflightRestartMode = canonical;
            token.set_restart_mode(Some(legacy));
            assert_eq!(token.restart_mode(), Some(mode));
            assert_eq!(token.restart_mode.load(Ordering::Relaxed), numeric);
        }

        token.set_restart_mode(None);
        assert_eq!(token.restart_mode.load(Ordering::Relaxed), 0);
        assert_eq!(token.restart_mode(), None);
        token.restart_mode.store(255, Ordering::Relaxed);
        assert_eq!(token.restart_mode(), None);
    }
}
