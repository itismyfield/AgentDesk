//! Shared helpers used across the voice module.
//!
//! Currently exposes [`expand_tilde`], a small path normalizer that resolves
//! a leading `~` / `~/` to the user's home directory. STT, TTS (mod + edge
//! backends), and the receiver historically each carried an identical copy of
//! this helper; consolidating it here removes the drift risk noted by the
//! voice module audit (#2046 follow-up).

use std::path::{Component, Path, PathBuf};

/// Expand a leading `~` or `~/` in `path` to the current user's home
/// directory. Returns the input untouched when the path doesn't start with
/// `~` or when the home directory cannot be resolved.
pub(crate) fn expand_tilde(path: &Path) -> PathBuf {
    if !matches!(path.components().next(), Some(Component::Normal(first)) if first == "~") {
        return path.to_path_buf();
    }

    let raw = path.to_string_lossy();
    crate::runtime_layout::expand_user_path(&raw).unwrap_or_else(|| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_input_when_no_tilde() {
        let input = PathBuf::from("/tmp/voice");
        assert_eq!(expand_tilde(&input), input);
    }

    #[test]
    fn passes_through_relative_paths_without_tilde() {
        let input = PathBuf::from("relative/path");
        assert_eq!(expand_tilde(&input), input);
    }

    #[test]
    fn preserves_non_tilde_path_text_exactly() {
        let input = PathBuf::from(" relative/path ");
        assert_eq!(expand_tilde(&input), input);
    }

    #[test]
    fn expands_bare_tilde_to_home() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        assert_eq!(expand_tilde(Path::new("~")), home);
    }

    #[test]
    fn expands_tilde_slash_prefix() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        assert_eq!(
            expand_tilde(Path::new("~/.adk/voice/tmp")),
            home.join(".adk/voice/tmp"),
        );
    }
}
