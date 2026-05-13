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
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::Normal(first)) if first == "~") {
        return path.to_path_buf();
    }

    let Some(mut expanded) = crate::runtime_layout::expand_user_path("~") else {
        return path.to_path_buf();
    };
    for component in components {
        expanded.push(component.as_os_str());
    }
    expanded
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

    #[test]
    fn preserves_tilde_path_component_trailing_space() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        assert_eq!(expand_tilde(Path::new("~/voice ")), home.join("voice "));
    }
}
