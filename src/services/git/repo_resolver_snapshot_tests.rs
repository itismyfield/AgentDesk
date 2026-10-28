use super::*;
use crate::config::snapshot_reader_tests::{Fixture, isolated};

const REPO: &str = "github:\n  repo_dirs: {'owner/repo': 'repos/project'}\n";

#[test]
fn relative_repo_dir_retains_legacy_snapshot_source_parent() {
    isolated(|| {
        let mut fixture = Fixture::new();
        fixture.path = fixture.root().join("agentdesk.yaml");
        let legacy_dir = fixture.root().join("repos/project");
        let canonical_dir = fixture.root().join("config/repos/project");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        std::fs::create_dir_all(&canonical_dir).unwrap();
        fixture.install(REPO, REPO);
        let canonical_path = fixture.root().join("config/agentdesk.yaml");
        fixture.write_at(&canonical_path, REPO);
        assert_eq!(
            configured_repo_dir("owner/repo"),
            Some(
                safe_canonicalize(&legacy_dir)
                    .to_string_lossy()
                    .into_owned()
            )
        );
        assert!(matches!(
            crate::config_live_reload::reload_from_path(&canonical_path),
            crate::config_live_reload::ReloadOutcome::Applied { .. }
        ));
        assert_eq!(
            configured_repo_dir("owner/repo"),
            Some(
                safe_canonicalize(&canonical_dir)
                    .to_string_lossy()
                    .into_owned()
            )
        );
    });
}

#[test]
fn snapshot_without_source_never_selects_an_unrelated_disk_parent() {
    isolated(|| {
        let fixture = Fixture::new();
        fixture.write(REPO);
        let mut config = crate::config::Config::default();
        config
            .github
            .repo_dirs
            .insert("owner/repo".into(), "repos/project".into());
        crate::config_live_reload::install(config);
        assert!(configured_repo_dir("owner/repo").is_none());
    });
}

#[test]
fn relative_repo_dir_preserves_standalone_legacy_fallback() {
    isolated(|| {
        let mut fixture = Fixture::new();
        fixture.path = fixture.root().join("agentdesk.yaml");
        fixture.write(REPO);
        let expected = fixture.root().join("repos/project");
        std::fs::create_dir_all(&expected).unwrap();
        assert_eq!(
            configured_repo_dir("owner/repo"),
            Some(safe_canonicalize(&expected).to_string_lossy().into_owned())
        );
    });
}
