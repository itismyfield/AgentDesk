//! Complete, bounded launch-context census; an incomplete directory is never an empty census.

use super::*;

pub(super) fn read_hook_context(path: &Path) -> io::Result<BindingContext> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(CONTEXT_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > CONTEXT_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "context exceeds size limit",
        ));
    }
    serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Reads every canonical context, without pruning, creating or discarding failed entries.
/// The budget counts directory entries, including unfinished or unexpected entries.
pub(crate) fn inventory(provider: &str, limit: usize) -> Result<Vec<BindingContext>, String> {
    let own = context_path(provider, &"0".repeat(32)).map_err(|e| e.to_string())?;
    let parent = own.parent().ok_or("context directory unavailable")?;
    let entries = fs::read_dir(parent).map_err(|e| e.to_string())?;
    let mut contexts = Vec::new();
    for (index, entry) in entries.enumerate() {
        let entry = entry.map_err(|e| e.to_string())?;
        if index >= limit {
            #[cfg(test)]
            if std::env::var("ADK_CODEX_OBSERVATION_MUTANT")
                .is_ok_and(|name| name == "context_budget")
            {
                break;
            }
            return Err("context inventory entry budget exceeded".into());
        }
        let path = entry.path();
        let nonce = path
            .file_stem()
            .and_then(|name| name.to_str())
            .ok_or("context inventory entry has no nonce")?;
        if !entry.file_type().map_err(|e| e.to_string())?.is_file()
            || context_path(provider, nonce).map_err(|e| e.to_string())? != path
        {
            return Err("context inventory contains a noncanonical entry".into());
        }
        contexts.push(checked_context(&path, provider).map_err(|e| format!("{e:?}"))?);
    }
    contexts.sort_by(|a, b| a.execution_nonce.cmp(&b.execution_nonce));
    Ok(contexts)
}

#[cfg(all(test, unix))]
#[path = "inventory_tests.rs"]
mod tests;
