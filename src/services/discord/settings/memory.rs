use super::*;

fn normalize_memory_backend_name(raw: Option<&str>) -> Option<&'static str> {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        None => None,
        Some(value) if value.eq_ignore_ascii_case("auto") => Some("auto"),
        Some(value) if value.eq_ignore_ascii_case("file") => Some("file"),
        Some(value) if value.eq_ignore_ascii_case("local") => Some("file"),
        Some(value) if value.eq_ignore_ascii_case("mem0") => Some("mem0"),
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

fn configured_auto_remember_enabled() -> bool {
    runtime_memory_backend_config()
        .map(|config| config.auto_remember.enabled)
        .unwrap_or(false)
}

fn configured_auto_remember_improver_mode() -> String {
    runtime_memory_backend_config()
        .map(|config| config.auto_remember.improver.mode)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "local_llm".to_string())
}

fn configured_auto_remember_agent_provider() -> Option<String> {
    runtime_memory_backend_config().and_then(|config| config.auto_remember.improver.agent.provider)
}

fn configured_auto_remember_agent_model() -> Option<String> {
    runtime_memory_backend_config().and_then(|config| config.auto_remember.improver.agent.model)
}

fn configured_auto_remember_agent_label() -> Option<String> {
    runtime_memory_backend_config().and_then(|config| config.auto_remember.improver.agent.label)
}

fn configured_query_recall_after_bootstrap() -> bool {
    runtime_memory_backend_config()
        .map(|config| config.query_recall_after_bootstrap)
        .unwrap_or(false)
}

fn memento_backend_available() -> bool {
    crate::services::memory::backend_is_active(MemoryBackendKind::Memento)
}

fn mem0_backend_available() -> bool {
    crate::services::memory::backend_is_active(MemoryBackendKind::Mem0)
}

fn auto_detect_memory_backend() -> MemoryBackendKind {
    if memento_backend_available() {
        MemoryBackendKind::Memento
    } else if mem0_backend_available() {
        MemoryBackendKind::Mem0
    } else {
        MemoryBackendKind::File
    }
}

fn resolve_memory_backend(raw: Option<&str>) -> MemoryBackendKind {
    let configured = configured_memory_backend_name();
    let requested = normalize_memory_backend_name(raw)
        .or_else(|| normalize_memory_backend_name(configured.as_deref()))
        .unwrap_or("auto");

    match requested {
        "auto" => auto_detect_memory_backend(),
        "file" => MemoryBackendKind::File,
        "mem0" => resolve_explicit_memory_backend(MemoryBackendKind::Mem0),
        "memento" => resolve_explicit_memory_backend(MemoryBackendKind::Memento),
        _ => MemoryBackendKind::File,
    }
}

fn resolve_explicit_memory_backend(kind: MemoryBackendKind) -> MemoryBackendKind {
    if crate::services::memory::backend_is_active(kind) {
        return kind;
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

    MemoryBackendKind::File
}

fn resolve_mem0_profile(raw: Option<&str>) -> String {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        None => DEFAULT_MEM0_PROFILE.to_string(),
        Some(value)
            if KNOWN_MEM0_PROFILES
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(value)) =>
        {
            value.to_ascii_lowercase()
        }
        Some(value) => {
            eprintln!(
                "  [memory] Warning: unknown memory.mem0.profile '{value}', falling back to {DEFAULT_MEM0_PROFILE}"
            );
            DEFAULT_MEM0_PROFILE.to_string()
        }
    }
}

fn resolve_confidence_threshold(raw: Option<f64>) -> Option<f64> {
    match raw {
        Some(value) if (0.0..=1.0).contains(&value) => Some(value),
        Some(value) => {
            eprintln!(
                "  [memory] Warning: memory.mem0.ingestion.confidence_threshold={} is invalid; dropping override",
                value
            );
            None
        }
        None => None,
    }
}

fn merge_mem0_config(
    base: Option<&Mem0ConfigOverride>,
    override_cfg: Option<&Mem0ConfigOverride>,
) -> Mem0ConfigOverride {
    let base_ingestion = base.and_then(|cfg| cfg.ingestion.as_ref());
    let override_ingestion = override_cfg.and_then(|cfg| cfg.ingestion.as_ref());
    Mem0ConfigOverride {
        profile: override_cfg
            .and_then(|cfg| cfg.profile.clone())
            .or_else(|| base.and_then(|cfg| cfg.profile.clone())),
        ingestion: Some(Mem0IngestionConfigOverride {
            infer: override_ingestion
                .and_then(|cfg| cfg.infer)
                .or_else(|| base_ingestion.and_then(|cfg| cfg.infer)),
            custom_instructions: override_ingestion
                .and_then(|cfg| cfg.custom_instructions.clone())
                .or_else(|| base_ingestion.and_then(|cfg| cfg.custom_instructions.clone())),
            confidence_threshold: override_ingestion
                .and_then(|cfg| cfg.confidence_threshold)
                .or_else(|| base_ingestion.and_then(|cfg| cfg.confidence_threshold)),
        }),
    }
}

fn merge_auto_remember_config(
    base: Option<&AutoRememberConfigOverride>,
    override_cfg: Option<&AutoRememberConfigOverride>,
    base_legacy_enabled: Option<bool>,
    override_legacy_enabled: Option<bool>,
) -> AutoRememberConfigOverride {
    AutoRememberConfigOverride {
        enabled: override_cfg
            .and_then(|cfg| cfg.enabled)
            .or(override_legacy_enabled)
            .or_else(|| base.and_then(|cfg| cfg.enabled))
            .or(base_legacy_enabled),
        // P0 keeps the improver contract on runtime/env config only. Binding-level
        // memory overrides may disable auto-remember, but they do not introduce a
        // separate provider/model/mode surface.
        improver: None,
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
        query_recall_after_bootstrap: override_cfg
            .and_then(|cfg| cfg.query_recall_after_bootstrap)
            .or_else(|| base.and_then(|cfg| cfg.query_recall_after_bootstrap)),
        recall_timeout_ms: override_cfg
            .and_then(|cfg| cfg.recall_timeout_ms)
            .or_else(|| base.and_then(|cfg| cfg.recall_timeout_ms)),
        capture_timeout_ms: override_cfg
            .and_then(|cfg| cfg.capture_timeout_ms)
            .or_else(|| base.and_then(|cfg| cfg.capture_timeout_ms)),
        auto_remember_enabled: override_cfg
            .and_then(|cfg| cfg.auto_remember_enabled)
            .or_else(|| base.and_then(|cfg| cfg.auto_remember_enabled)),
        auto_remember: Some(merge_auto_remember_config(
            base.and_then(|cfg| cfg.auto_remember.as_ref()),
            override_cfg.and_then(|cfg| cfg.auto_remember.as_ref()),
            base.and_then(|cfg| cfg.auto_remember_enabled),
            override_cfg.and_then(|cfg| cfg.auto_remember_enabled),
        )),
        mem0: Some(merge_mem0_config(
            base.and_then(|cfg| cfg.mem0.as_ref()),
            override_cfg.and_then(|cfg| cfg.mem0.as_ref()),
        )),
    }
}

pub(crate) fn resolve_memory_settings(
    base: Option<&MemoryConfigOverride>,
    override_cfg: Option<&MemoryConfigOverride>,
) -> ResolvedMemorySettings {
    let merged = merge_memory_config(base, override_cfg);
    let mem0_override = merged.mem0.as_ref();
    let auto_remember_override = merged.auto_remember.as_ref();
    let auto_remember_enabled = auto_remember_override
        .and_then(|cfg| cfg.enabled)
        .or(merged.auto_remember_enabled)
        .unwrap_or_else(configured_auto_remember_enabled);
    ResolvedMemorySettings {
        backend: resolve_memory_backend(merged.backend.as_deref()),
        query_recall_after_bootstrap: merged
            .query_recall_after_bootstrap
            .unwrap_or_else(configured_query_recall_after_bootstrap),
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
        auto_remember_enabled,
        auto_remember: ResolvedAutoRememberSettings {
            enabled: auto_remember_enabled,
            improver: ResolvedAutoRememberImproverSettings {
                mode: configured_auto_remember_improver_mode(),
                agent: ResolvedAutoRememberAgentSettings {
                    provider: configured_auto_remember_agent_provider(),
                    model: configured_auto_remember_agent_model(),
                    label: configured_auto_remember_agent_label(),
                },
            },
        },
        mem0: Mem0ResolvedSettings {
            profile: resolve_mem0_profile(mem0_override.and_then(|cfg| cfg.profile.as_deref())),
            ingestion: Mem0IngestionSettings {
                infer: mem0_override
                    .and_then(|cfg| cfg.ingestion.as_ref())
                    .and_then(|cfg| cfg.infer),
                custom_instructions: mem0_override
                    .and_then(|cfg| cfg.ingestion.as_ref())
                    .and_then(|cfg| cfg.custom_instructions.clone()),
                confidence_threshold: resolve_confidence_threshold(
                    mem0_override
                        .and_then(|cfg| cfg.ingestion.as_ref())
                        .and_then(|cfg| cfg.confidence_threshold),
                ),
            },
        },
    }
}

pub(crate) fn memory_settings_for_binding(
    role_binding: Option<&RoleBinding>,
) -> ResolvedMemorySettings {
    role_binding
        .map(|binding| binding.memory.clone())
        .unwrap_or_default()
}
