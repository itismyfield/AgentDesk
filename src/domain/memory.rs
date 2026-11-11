pub(crate) const DEFAULT_MEMORY_RECALL_TIMEOUT_MS: u64 = 500;
pub(crate) const DEFAULT_MEMORY_CAPTURE_TIMEOUT_MS: u64 = 5_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum MemoryBackendKind {
    #[default]
    File,
    Memento,
}

impl MemoryBackendKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Memento => "memento",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ResolvedMemorySettings {
    pub memory_group: Option<String>,
    pub backend: MemoryBackendKind,
    /// True only when the resolved `backend` is `File` because memento was the
    /// requested/configured backend but is degraded (unreachable). Lets the
    /// guidance layer distinguish a deliberate file backend from a transparent
    /// memento fallback, which have different write policies. Always false for a
    /// deliberately configured file backend and for an active memento backend.
    pub memento_fallback: bool,
    pub recall_timeout_ms: u64,
    pub capture_timeout_ms: u64,
}

impl Default for ResolvedMemorySettings {
    fn default() -> Self {
        Self {
            memory_group: None,
            backend: MemoryBackendKind::File,
            memento_fallback: false,
            recall_timeout_ms: DEFAULT_MEMORY_RECALL_TIMEOUT_MS,
            capture_timeout_ms: DEFAULT_MEMORY_CAPTURE_TIMEOUT_MS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MemoryBackendKind, ResolvedMemorySettings};

    #[test]
    fn memory_backend_labels_and_unconfigured_defaults_are_stable() {
        assert_eq!(MemoryBackendKind::File.as_str(), "file");
        assert_eq!(MemoryBackendKind::Memento.as_str(), "memento");
        assert_eq!(MemoryBackendKind::default(), MemoryBackendKind::File);

        let settings = ResolvedMemorySettings::default();
        assert_eq!(settings.memory_group, None);
        assert_eq!(settings.backend, MemoryBackendKind::File);
        assert!(!settings.memento_fallback);
        assert_eq!(settings.recall_timeout_ms, 500);
        assert_eq!(settings.capture_timeout_ms, 5_000);
    }
}
