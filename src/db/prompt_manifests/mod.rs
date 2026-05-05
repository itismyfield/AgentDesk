mod builder;
mod model;
mod redaction;
mod repository;
mod retention;
mod storage_stats;

#[cfg(test)]
mod tests;

#[allow(unused_imports)]
pub use builder::{PromptManifestBuilder, estimate_tokens_from_chars};
#[allow(unused_imports)]
pub use model::{PromptContentVisibility, PromptManifest, PromptManifestLayer};
#[allow(unused_imports)]
pub use repository::{fetch_prompt_manifest, save_prompt_manifest, spawn_save_prompt_manifest};
#[allow(unused_imports)]
pub use retention::{
    PROMPT_MANIFEST_RETENTION_CONFIG_APPLIED_AT, PROMPT_MANIFEST_RETENTION_CONFIG_SOURCE,
    PromptManifestRetentionReport, apply_retention_policy, install_retention_config,
};
#[allow(unused_imports)]
pub use storage_stats::{PromptManifestStorageStats, manifest_storage_stats};

#[cfg(test)]
use redaction::sha256_hex;
