//! `providers.claude.transcript_homes` entries as declared, and the validation warning for unusable ones.

use std::path::PathBuf;

/// Each declared Claude transcript home with its tilde-expanded path.
pub fn declared(config: &super::Config) -> impl Iterator<Item = (&str, PathBuf)> {
    let entries = config
        .providers
        .get("claude")
        .map(|claude| &claude.transcript_homes);
    entries.into_iter().flatten().map(|entry| {
        let home = PathBuf::from(crate::utils::format::expand_tilde_string(entry));
        (entry.as_str(), home)
    })
}

/// Name each declared transcript home `/resume` can never read when the config is validated.
pub(crate) fn warn_unusable_transcript_homes(config: &super::Config) {
    for (entry, home) in declared(config) {
        let problem = match () {
            _ if !home.is_absolute() => "not an absolute path",
            _ if !home.is_dir() => "not an existing directory",
            _ => continue,
        };
        tracing::warn!(
            "providers.claude.transcript_homes entry {entry:?} is {problem}; /resume ignores it"
        );
    }
}
