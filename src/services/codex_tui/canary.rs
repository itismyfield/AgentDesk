//! Verified source selection is pinned only for the designated test channel.

use super::session::source_observation::{
    CodexSourceMode, SourceLaunchPolicy, codex_source_mode_snapshot,
};

pub(crate) const CANARY_CHANNEL: u64 = 1_509_350_778_043_895_902;
pub(crate) const CANARY_TMUX: &str = "AgentDesk-codex-adk-codex-tui-e2e";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchTarget<'a> {
    DirectTmux { tmux: &'a str, channel: Option<u64> },
    OutsideDirectTmux,
}

fn classify_target(tmux: &str, channel: Option<u64>) -> LaunchTarget<'_> {
    // The existing Herdr caller uses this sentinel; other inputs retain their direct target.
    if tmux.is_empty() && channel.is_none() {
        LaunchTarget::OutsideDirectTmux
    } else {
        LaunchTarget::DirectTmux { tmux, channel }
    }
}

pub(crate) fn enabled_for(tmux: &str) -> bool {
    matches!(classify_target(tmux, None), LaunchTarget::DirectTmux { tmux, .. } if tmux == CANARY_TMUX)
        && codex_source_mode_snapshot() == CodexSourceMode::Verified
}

pub(crate) fn launch_policy(
    tmux: &str,
    channel: Option<u64>,
) -> Result<&'static str, &'static str> {
    launch_decision(codex_source_mode_snapshot(), classify_target(tmux, channel))
        .map(SourceLaunchPolicy::as_str)
}

fn launch_decision(
    mode: CodexSourceMode,
    target: LaunchTarget<'_>,
) -> Result<SourceLaunchPolicy, &'static str> {
    match mode {
        CodexSourceMode::Verified
            if matches!(target, LaunchTarget::DirectTmux { tmux, channel }
                if tmux == CANARY_TMUX && channel == Some(CANARY_CHANNEL)) =>
        {
            Ok(SourceLaunchPolicy::Verified)
        }
        CodexSourceMode::Verified => Ok(SourceLaunchPolicy::Legacy),
        mode => mode.launch_decision(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::session::source_observation::SOURCE_MODE_TEST;
    use super::*;

    struct ModeOverride(Option<CodexSourceMode>);

    impl ModeOverride {
        fn new(mode: CodexSourceMode) -> Self {
            Self(SOURCE_MODE_TEST.with(|slot| slot.replace(Some(mode))))
        }
    }

    impl Drop for ModeOverride {
        fn drop(&mut self) {
            SOURCE_MODE_TEST.with(|slot| slot.set(self.0));
        }
    }

    #[test]
    fn typed_launch_wrappers_preserve_existing_mode_target_matrix() {
        let targets = [
            (CANARY_TMUX, Some(CANARY_CHANNEL)),
            (CANARY_TMUX, None),
            (CANARY_TMUX, Some(CANARY_CHANNEL + 1)),
            ("AgentDesk-codex-other", Some(CANARY_CHANNEL)),
            ("AgentDesk-codex-other", None),
            ("", None),
            ("", Some(CANARY_CHANNEL)),
        ];
        for value in [
            None,
            Some("legacy"),
            Some("shadow"),
            Some("verified"),
            Some("invalid"),
        ] {
            let mode = CodexSourceMode::parse(value);
            let _override = ModeOverride::new(mode);
            for (tmux, channel) in targets {
                let expected = match value {
                    None | Some("legacy") => Ok("legacy"),
                    Some("shadow") => Ok("shadow"),
                    Some("verified") if tmux == CANARY_TMUX && channel == Some(CANARY_CHANNEL) => {
                        Ok("verified")
                    }
                    Some("verified") => Ok("legacy"),
                    _ => Err("SourceModeInvalid"),
                };
                assert_eq!(
                    launch_policy(tmux, channel),
                    expected,
                    "mode={value:?}, tmux={tmux}, channel={channel:?}"
                );
                assert_eq!(
                    enabled_for(tmux),
                    value == Some("verified") && tmux == CANARY_TMUX,
                    "probe mode={value:?}, tmux={tmux}"
                );
            }
        }
    }

    #[test]
    fn herdr_sentinel_is_classified_outside_direct_tmux() {
        assert_eq!(classify_target("", None), LaunchTarget::OutsideDirectTmux);
        assert_eq!(
            classify_target("", Some(CANARY_CHANNEL)),
            LaunchTarget::DirectTmux {
                tmux: "",
                channel: Some(CANARY_CHANNEL)
            }
        );
        assert_eq!(
            classify_target(CANARY_TMUX, None),
            LaunchTarget::DirectTmux {
                tmux: CANARY_TMUX,
                channel: None
            }
        );
    }

    #[test]
    fn typed_base_policy_preserves_verified_not_landed() {
        assert_eq!(
            CodexSourceMode::Verified.launch_decision(),
            Err("SourceModeVerifiedNotLanded")
        );
        assert_eq!(
            CodexSourceMode::Verified.launch_policy(),
            Err("SourceModeVerifiedNotLanded")
        );
        assert_eq!(
            CodexSourceMode::Invalid.launch_decision(),
            Err("SourceModeInvalid")
        );
        assert_eq!(
            CodexSourceMode::Invalid.launch_policy(),
            Err("SourceModeInvalid")
        );
        let _override = ModeOverride::new(CodexSourceMode::Verified);
        assert_eq!(
            launch_policy(CANARY_TMUX, Some(CANARY_CHANNEL)),
            Ok("verified")
        );
        assert_eq!(launch_policy("", None), Ok("legacy"));
    }
}
