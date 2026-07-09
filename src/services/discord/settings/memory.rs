use super::*;

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

fn runtime_memory_backend_config() -> Option<runtime_layout::MemoryBackendConfig> {
    crate::config::runtime_root().map(|root| runtime_layout::load_memory_backend(&root))
}

fn configured_memory_backend_name() -> Option<String> {
    runtime_memory_backend_config().map(|config| config.backend)
}

fn memento_backend_available() -> bool {
    crate::services::memory::backend_is_active(MemoryBackendKind::Memento)
}

fn auto_detect_memory_backend() -> MemoryBackendKind {
    if memento_backend_available() {
        MemoryBackendKind::Memento
    } else {
        MemoryBackendKind::File
    }
}

/// Outcome of backend resolution. `memento_fallback` is true only when memento
/// was requested but degraded, so `kind` collapsed to `File`; it lets the
/// guidance layer tell a deliberate file backend apart from a memento fallback.
struct ResolvedBackend {
    kind: MemoryBackendKind,
    memento_fallback: bool,
}

fn resolve_memory_backend(raw: Option<&str>) -> ResolvedBackend {
    let configured = configured_memory_backend_name();
    let requested = normalize_memory_backend_name(raw)
        .or_else(|| normalize_memory_backend_name(configured.as_deref()))
        .unwrap_or("auto");

    match requested {
        "auto" => ResolvedBackend {
            kind: auto_detect_memory_backend(),
            memento_fallback: false,
        },
        "file" => ResolvedBackend {
            kind: MemoryBackendKind::File,
            memento_fallback: false,
        },
        "memento" => resolve_explicit_memory_backend(MemoryBackendKind::Memento),
        _ => ResolvedBackend {
            kind: MemoryBackendKind::File,
            memento_fallback: false,
        },
    }
}

fn resolve_explicit_memory_backend(kind: MemoryBackendKind) -> ResolvedBackend {
    if crate::services::memory::backend_is_active(kind) {
        return ResolvedBackend {
            kind,
            memento_fallback: false,
        };
    }

    if let Some(state) = crate::services::memory::backend_state(kind) {
        eprintln!(
            "  [memory] Warning: requested backend '{}' unavailable (configured={}, failures={}); falling back to file",
            kind.as_str(),
            state.configured,
            state.consecutive_failures
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

fn merge_memory_config(
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

pub(crate) fn resolve_memory_settings(
    base: Option<&MemoryConfigOverride>,
    override_cfg: Option<&MemoryConfigOverride>,
) -> ResolvedMemorySettings {
    let merged = merge_memory_config(base, override_cfg);
    let resolved_backend = resolve_memory_backend(merged.backend.as_deref());
    ResolvedMemorySettings {
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

pub(crate) fn memory_settings_for_binding(
    role_binding: Option<&RoleBinding>,
) -> ResolvedMemorySettings {
    role_binding
        .map(|binding| binding.memory.clone())
        .unwrap_or_default()
}
