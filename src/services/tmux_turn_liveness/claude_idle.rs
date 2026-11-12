//! A live provider cannot prove the absence of a person's draft or selector.
use crate::services::claude_tui::activity::{
    self, Background,
    processes::{self, Descendants},
};

pub(super) const UNPROVEN: &str = "UnprovenInteractiveState";
pub(super) fn applies(session: &str) -> bool {
    matches!(
        crate::services::provider::parse_provider_and_channel_from_tmux_name(session),
        Some((crate::services::provider::ProviderKind::Claude, _))
    )
}
pub(super) fn preserved(session: &str) -> super::IdlePreserved {
    let observation = activity::observe(session);
    let children = if observation.current() {
        observation.background
    } else {
        Background::Unknown
    };
    (reason(children, processes::observe(session)), None)
}
fn reason(children: Background, processes: Descendants) -> &'static str {
    match (children, processes) {
        (Background::ChildPending, _) => "ChildPending",
        (_, Descendants::Present) => "WorkloadPresent",
        _ => UNPROVEN,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matching_stop_only_releases_child_veto_not_interactive_veto() {
        assert_eq!(
            reason(Background::ChildPending, Descendants::NoneObserved),
            "ChildPending"
        );
        for processes in [
            Descendants::Present,
            Descendants::NoneObserved,
            Descendants::Unknown,
        ] {
            for children in [Background::Clear, Background::Unknown] {
                assert_ne!(reason(children, processes), "idle");
            }
        }
        assert_eq!(
            reason(Background::Clear, Descendants::NoneObserved),
            "UnprovenInteractiveState"
        );
        assert_eq!(reason(Background::Clear, Descendants::Unknown), UNPROVEN);
    }
}
