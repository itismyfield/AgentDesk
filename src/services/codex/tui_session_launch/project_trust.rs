//! Trust only the exact managed cwd for this invocation, without persisting user config.
use std::path::Path;

pub(super) fn override_for(root: &Path, cwd: &Path) -> Option<String> {
    if !cwd.is_absolute() {
        return None;
    }
    let root = root.canonicalize().ok()?;
    let cwd = cwd.canonicalize().ok()?;
    if !cwd.is_dir() {
        return None;
    }
    let managed = [root.join("workspaces"), root.join("worktrees")]
        .into_iter()
        .find(|tree| {
            cwd != *tree
                && cwd.starts_with(tree)
                && tree.canonicalize().is_ok_and(|resolved| resolved == *tree)
        })?;
    // An inherited repository outside the managed tree must not acquire trust through its child.
    if cwd
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .is_some_and(|repo| !repo.starts_with(&managed))
    {
        return None;
    }
    // The JSON-quoted key also needs DEL escaped for TOML.
    let key = serde_json::to_string(cwd.to_str()?)
        .ok()?
        .replace('\u{7f}', "\\u007f");
    Some(format!("projects={{{key}={{trust_level=\"trusted\"}}}}"))
}
