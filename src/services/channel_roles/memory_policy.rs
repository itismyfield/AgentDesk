use crate::domain::memory::{
    DEFAULT_MEMORY_CAPTURE_TIMEOUT_MS, DEFAULT_MEMORY_RECALL_TIMEOUT_MS, MemoryBackendKind,
    MemoryConfigOverride, ResolvedMemorySettings,
};

#[derive(Clone, Copy, Default)]
pub(super) struct BackendAvailability {
    pub active: bool,
    pub unavailable_state: Option<(bool, u8)>,
}

const MIN_MEMORY_RECALL_TIMEOUT_MS: u64 = 100;
const MAX_MEMORY_RECALL_TIMEOUT_MS: u64 = 2_000;
const MIN_MEMORY_CAPTURE_TIMEOUT_MS: u64 = 500;
const MAX_MEMORY_CAPTURE_TIMEOUT_MS: u64 = 30_000;

fn clamp_timeout(name: &str, value: u64, min: u64, max: u64, default: u64) -> u64 {
    let clamped = value.clamp(min, max);
    if value != clamped {
        eprintln!(
            "  [memory] Warning: {name}={} is out of range; clamping to {clamped}",
            value
        );
    }
    if clamped == 0 { default } else { clamped }
}

fn normalize_memory_backend_name(raw: Option<&str>) -> Option<&'static str> {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        None => None,
        Some(value) if value.eq_ignore_ascii_case("auto") => Some("auto"),
        Some(value) if value.eq_ignore_ascii_case("file") => Some("file"),
        Some(value) if value.eq_ignore_ascii_case("local") => Some("file"),
        Some(value) if value.eq_ignore_ascii_case("mem0") => Some("file"),
        Some(value) if value.eq_ignore_ascii_case("memento") => Some("memento"),
        Some(value) => {
            eprintln!(
                "  [memory] Warning: unknown memory.backend '{value}', falling back to auto-detect"
            );
            None
        }
    }
}

pub(super) fn requested_backend(raw: Option<&str>, configured: Option<&str>) -> &'static str {
    normalize_memory_backend_name(raw)
        .or_else(|| normalize_memory_backend_name(configured))
        .unwrap_or("auto")
}

struct ResolvedBackend {
    kind: MemoryBackendKind,
    memento_fallback: bool,
}

fn resolve_memory_backend(requested: &str, availability: BackendAvailability) -> ResolvedBackend {
    match requested {
        "auto" => ResolvedBackend {
            kind: if availability.active {
                MemoryBackendKind::Memento
            } else {
                MemoryBackendKind::File
            },
            memento_fallback: false,
        },
        "memento" => resolve_explicit_memory_backend(MemoryBackendKind::Memento, availability),
        _ => ResolvedBackend {
            kind: MemoryBackendKind::File,
            memento_fallback: false,
        },
    }
}

fn resolve_explicit_memory_backend(
    kind: MemoryBackendKind,
    availability: BackendAvailability,
) -> ResolvedBackend {
    if availability.active {
        return ResolvedBackend {
            kind,
            memento_fallback: false,
        };
    }
    if let Some((configured, consecutive_failures)) = availability.unavailable_state {
        eprintln!(
            "  [memory] Warning: requested backend '{}' unavailable (configured={}, failures={}); falling back to file",
            kind.as_str(),
            configured,
            consecutive_failures
        );
    } else {
        eprintln!(
            "  [memory] Warning: requested backend '{}' unavailable; falling back to file",
            kind.as_str()
        );
    }
    ResolvedBackend {
        kind: MemoryBackendKind::File,
        memento_fallback: kind == MemoryBackendKind::Memento,
    }
}

pub(super) fn merge_memory_config(
    base: Option<&MemoryConfigOverride>,
    override_cfg: Option<&MemoryConfigOverride>,
) -> MemoryConfigOverride {
    MemoryConfigOverride {
        backend: override_cfg
            .and_then(|cfg| cfg.backend.clone())
            .or_else(|| base.and_then(|cfg| cfg.backend.clone())),
        recall_timeout_ms: override_cfg
            .and_then(|cfg| cfg.recall_timeout_ms)
            .or_else(|| base.and_then(|cfg| cfg.recall_timeout_ms)),
        capture_timeout_ms: override_cfg
            .and_then(|cfg| cfg.capture_timeout_ms)
            .or_else(|| base.and_then(|cfg| cfg.capture_timeout_ms)),
    }
}

pub(super) fn resolve_memory_settings(
    merged: &MemoryConfigOverride,
    requested: &str,
    availability: BackendAvailability,
) -> ResolvedMemorySettings {
    let resolved_backend = resolve_memory_backend(requested, availability);
    ResolvedMemorySettings {
        memory_group: None,
        backend: resolved_backend.kind,
        memento_fallback: resolved_backend.memento_fallback,
        recall_timeout_ms: clamp_timeout(
            "memory.recall_timeout_ms",
            merged
                .recall_timeout_ms
                .unwrap_or(DEFAULT_MEMORY_RECALL_TIMEOUT_MS),
            MIN_MEMORY_RECALL_TIMEOUT_MS,
            MAX_MEMORY_RECALL_TIMEOUT_MS,
            DEFAULT_MEMORY_RECALL_TIMEOUT_MS,
        ),
        capture_timeout_ms: clamp_timeout(
            "memory.capture_timeout_ms",
            merged
                .capture_timeout_ms
                .unwrap_or(DEFAULT_MEMORY_CAPTURE_TIMEOUT_MS),
            MIN_MEMORY_CAPTURE_TIMEOUT_MS,
            MAX_MEMORY_CAPTURE_TIMEOUT_MS,
            DEFAULT_MEMORY_CAPTURE_TIMEOUT_MS,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_overrides_merge_per_field_and_clamp_both_timeout_boundaries() {
        let base = MemoryConfigOverride {
            backend: Some("memento".into()),
            recall_timeout_ms: Some(1500),
            capture_timeout_ms: Some(8000),
        };
        let channel = MemoryConfigOverride {
            backend: Some("file".into()),
            recall_timeout_ms: None,
            capture_timeout_ms: Some(9000),
        };
        let merged = merge_memory_config(Some(&base), Some(&channel));
        let settings = resolve_memory_settings(
            &merged,
            requested_backend(merged.backend.as_deref(), Some("memento")),
            BackendAvailability::default(),
        );
        assert_eq!(settings.backend, MemoryBackendKind::File);
        assert!(!settings.memento_fallback);
        assert_eq!(
            (settings.recall_timeout_ms, settings.capture_timeout_ms),
            (1500, 9000)
        );
        for (recall, capture, expected) in [
            (0, 0, (100, 500)),
            (99, 499, (100, 500)),
            (100, 500, (100, 500)),
            (2000, 30000, (2000, 30000)),
            (2001, 30001, (2000, 30000)),
            (u64::MAX, u64::MAX, (2000, 30000)),
        ] {
            let settings = resolve_memory_settings(
                &MemoryConfigOverride {
                    recall_timeout_ms: Some(recall),
                    capture_timeout_ms: Some(capture),
                    ..Default::default()
                },
                "file",
                BackendAvailability::default(),
            );
            assert_eq!(
                (settings.recall_timeout_ms, settings.capture_timeout_ms),
                expected
            );
        }
        let defaults = resolve_memory_settings(
            &MemoryConfigOverride::default(),
            "file",
            BackendAvailability::default(),
        );
        assert_eq!(
            (defaults.recall_timeout_ms, defaults.capture_timeout_ms),
            (500, 5000)
        );
    }

    #[test]
    fn backend_aliases_unknown_values_and_explicit_fallback_remain_distinct() {
        for name in ["file", " FILE ", "local", "MEM0"] {
            assert_eq!(requested_backend(Some(name), Some("memento")), "file");
        }
        assert_eq!(requested_backend(Some("MEMENTO"), Some("file")), "memento");
        assert_eq!(
            requested_backend(Some("unrecognized"), Some("file")),
            "file"
        );
        assert_eq!(requested_backend(Some(" "), Some("unrecognized")), "auto");
        for state in [None, Some((true, 3)), Some((false, 0))] {
            let availability = BackendAvailability {
                active: false,
                unavailable_state: state,
            };
            let explicit =
                resolve_memory_settings(&MemoryConfigOverride::default(), "memento", availability);
            assert_eq!(explicit.backend, MemoryBackendKind::File);
            assert!(explicit.memento_fallback);
            let auto =
                resolve_memory_settings(&MemoryConfigOverride::default(), "auto", availability);
            assert_eq!(auto.backend, MemoryBackendKind::File);
            assert!(!auto.memento_fallback);
        }
        let active = resolve_memory_settings(
            &MemoryConfigOverride::default(),
            "memento",
            BackendAvailability {
                active: true,
                unavailable_state: None,
            },
        );
        assert_eq!(active.backend, MemoryBackendKind::Memento);
        assert!(!active.memento_fallback);
    }
}
