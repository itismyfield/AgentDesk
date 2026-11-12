//! Hooks and channel launch choices share the single session settings payload.
use super::{HookBundleConfig, render_claude_hook_settings};
use serde_json::Value;
use std::path::Path;

pub fn write_claude_launch_settings(
    path: &Path,
    config: &HookBundleConfig,
    fast_mode: Option<bool>,
    ultracode: Option<bool>,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create hook settings dir {}: {error}", parent.display()))?;
    }
    let mut settings = render_claude_hook_settings(config);
    if let Some(object) = settings.as_object_mut() {
        for (key, value) in [("fastMode", fast_mode), ("ultracode", ultracode)] {
            if let Some(value) = value {
                object.insert(key.into(), Value::Bool(value));
            }
        }
    }
    let rendered = serde_json::to_string_pretty(&settings)
        .map_err(|error| format!("render hook settings: {error}"))?;
    std::fs::write(path, rendered)
        .map_err(|error| format!("write hook settings {}: {error}", path.display()))
}
