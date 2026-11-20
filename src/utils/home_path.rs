use std::path::PathBuf;

pub fn current_home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

pub fn expand_user_path(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(stripped) = trimmed
        .strip_prefix("~/")
        .or_else(|| trimmed.strip_prefix("~\\"))
    {
        let home = current_home_dir()?;
        let stripped = stripped.trim_start_matches(|ch| ch == '/' || ch == '\\');
        return Some(home.join(stripped));
    }
    if trimmed == "~" {
        return current_home_dir();
    }
    Some(PathBuf::from(trimmed))
}

#[cfg(test)]
mod tests {
    use super::{current_home_dir, expand_user_path};

    #[test]
    fn test_expand_user_path_keeps_double_slash_under_home() {
        let home = current_home_dir().unwrap();
        let expanded = expand_user_path("~//agentdesk").unwrap();
        assert!(expanded.starts_with(&home));
        assert!(expanded.ends_with("agentdesk"));
    }
}
