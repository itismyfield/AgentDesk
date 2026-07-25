#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord::placeholder_live_events) enum CompletedKind {
    Foreground,
    Background,
}

impl CompletedKind {
    pub(super) fn from_background(background: bool) -> Self {
        if background {
            Self::Background
        } else {
            Self::Foreground
        }
    }
}
