pub(crate) use crate::services::provider_auth_profile::repository::{
    append_provider_auth_profile_at, remove_provider_auth_profile_at, set_agent_auth_profile_at,
    set_channel_auth_profile_at, set_provider_primary_profile_at,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::provider_auth_profile::ProviderAuthProfileDef;
    use std::collections::BTreeMap;
    use std::fs;

    #[test]
    fn fallback_policy_survives_writes_and_protects_referenced_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("org.yaml");
        for field in ["fallback_profile: work", "priority: [work]"] {
            fs::write(
                &path,
                format!(
                    "version: 1\nagents: {{}}\nprovider_auth_fallbacks:\n  codex:\n    {field}\n"
                ),
            )
            .unwrap();
            append_provider_auth_profile_at(
                &path,
                "work",
                ProviderAuthProfileDef {
                    provider: "codex".into(),
                    home: Some("~/.adk/profiles/codex/work".into()),
                    env: BTreeMap::new(),
                },
            )
            .unwrap();
            assert!(
                remove_provider_auth_profile_at(&path, "work", "codex")
                    .unwrap_err()
                    .contains("still bound")
            );
            assert!(
                fs::read_to_string(&path)
                    .unwrap()
                    .contains("provider_auth_fallbacks:")
            );
        }
    }

    #[test]
    fn test_012_catalog_append_round_trips_extra_and_skips_secrets() {
        let dir = tempfile::tempdir().expect("org dir");
        let path = dir.path().join("org.yaml");
        fs::write(
            &path,
            "version: 1\nname: desk\ncustom_flag: true\nagents:\n  coder:\n    display_name: Coder\n    provider: codex\n",
        )
        .unwrap();

        append_provider_auth_profile_at(
            &path,
            "work",
            ProviderAuthProfileDef {
                provider: "codex".into(),
                home: Some("~/.adk/profiles/codex/work".into()),
                env: BTreeMap::new(),
            },
        )
        .unwrap();

        let rendered = fs::read_to_string(&path).unwrap();
        assert!(rendered.contains("provider_auth_profiles:"));
        assert!(rendered.contains("work:"));
        assert!(rendered.contains("custom_flag: true"));
        assert!(!rendered.contains("sk-"));
        assert!(!rendered.contains("access_token"));

        set_agent_auth_profile_at(&path, "coder", Some("work")).unwrap();
        let rendered = fs::read_to_string(&path).unwrap();
        assert!(rendered.contains("auth_profile: work"));
        set_agent_auth_profile_at(&path, "coder", None).unwrap();
        let rendered = fs::read_to_string(&path).unwrap();
        assert!(!rendered.contains("auth_profile: work"));
    }

    #[test]
    fn extra_login_does_not_overwrite_missing_catalog_on_unknown_agent() {
        let dir = tempfile::tempdir().expect("org dir");
        let path = dir.path().join("org.yaml");
        let err = set_agent_auth_profile_at(&path, "missing", Some("work")).unwrap_err();
        assert!(err.contains("not found"));
        assert!(
            !path.exists()
                || !fs::read_to_string(&path)
                    .unwrap_or_default()
                    .contains("auth_profile")
        );
    }

    #[test]
    fn named_profile_unlink_requires_detach_and_keeps_catalog_operation_scoped() {
        let dir = tempfile::tempdir().expect("org dir");
        let path = dir.path().join("org.yaml");
        fs::write(
            &path,
            "version: 1\nagents:\n  coder:\n    display_name: Coder\n    provider: codex\n",
        )
        .unwrap();
        let profile = ProviderAuthProfileDef {
            provider: "codex".into(),
            home: Some("~/.adk/profiles/codex/work".into()),
            env: BTreeMap::new(),
        };
        append_provider_auth_profile_at(&path, "work", profile.clone()).unwrap();
        set_agent_auth_profile_at(&path, "coder", Some("work")).unwrap();
        let err = remove_provider_auth_profile_at(&path, "work", "codex").unwrap_err();
        assert!(err.contains("still bound"));

        set_agent_auth_profile_at(&path, "coder", None).unwrap();
        remove_provider_auth_profile_at(&path, "work", "codex").unwrap();
        let rendered = fs::read_to_string(&path).unwrap();
        assert!(!rendered.contains("auth_profile: work"));
        assert!(!rendered.contains("\n  work:"));
    }

    #[test]
    fn named_profile_rejects_provider_mismatch_before_persisting_binding() {
        let dir = tempfile::tempdir().expect("org dir");
        let path = dir.path().join("org.yaml");
        fs::write(
            &path,
            "version: 1\nagents:\n  coder:\n    display_name: Coder\n    provider: codex\n",
        )
        .unwrap();
        append_provider_auth_profile_at(
            &path,
            "claude-work",
            ProviderAuthProfileDef {
                provider: "claude".into(),
                home: Some("~/.adk/profiles/claude/claude-work".into()),
                env: BTreeMap::new(),
            },
        )
        .unwrap();
        let err = set_agent_auth_profile_at(&path, "coder", Some("claude-work")).unwrap_err();
        assert!(err.contains("does not match"));
    }

    #[test]
    fn primary_profile_cannot_be_unlinked_until_reset_to_system_default() {
        let dir = tempfile::tempdir().expect("org dir");
        let path = dir.path().join("org.yaml");
        fs::write(&path, "version: 1\nagents: {}\n").unwrap();
        append_provider_auth_profile_at(
            &path,
            "work",
            ProviderAuthProfileDef {
                provider: "codex".into(),
                home: Some("~/.adk/profiles/codex/work".into()),
                env: BTreeMap::new(),
            },
        )
        .unwrap();
        set_provider_primary_profile_at(&path, "codex", "work").unwrap();
        let err = remove_provider_auth_profile_at(&path, "work", "codex").unwrap_err();
        assert!(err.contains("provider primary"));
        set_provider_primary_profile_at(&path, "codex", "default").unwrap();
        remove_provider_auth_profile_at(&path, "work", "codex").unwrap();
    }
}

#[cfg(test)]
mod krx_discord_root_tests;
#[cfg(all(test, unix))]
mod krx_discord_root_unix_tests;
