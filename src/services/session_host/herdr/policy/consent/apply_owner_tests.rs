use super::super::preview::{self, tests::fixture};
use super::*;
use std::fs;

#[tokio::test]
async fn consent_apply_owner_uses_candidate_and_final_full_checker() {
    let (dir, source) = fixture(
        "printf 'check\\n' >> \"$HOME/checks\"\n/usr/bin/grep -q 'resume_agents_on_restore=false' \"$HERDR_CONFIG_PATH\" || exit 17\nprintf 'config: ok\\n'",
    );
    let prepared = preview::prepare(source.clone(), 1).await.unwrap();
    let expected = prepared.token.clone();
    let applied = apply(prepared, &expected, source).await;
    assert!(
        applied.is_ok(),
        "apply_owner_full_checker_contract: {applied:?}"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("checks")).unwrap(),
        "check\ncheck\ncheck\n",
        "apply_owner_candidate_and_final_checker_contract"
    );
    assert!(
        fs::read_to_string(dir.path().join("config.toml"))
            .unwrap()
            .contains("=false")
    );
}

#[tokio::test]
async fn consent_apply_owner_refuses_stale_expected_or_live_preview() {
    for changed in 0..5 {
        let (dir, source) =
            fixture("printf 'check\\n' >> \"$HOME/checks\"; printf 'config: ok\\n'");
        let prepared = preview::prepare(source.clone(), 1).await.unwrap();
        let mut expected = prepared.token.clone();
        fs::remove_file(dir.path().join("checks")).unwrap();
        match changed {
            0 => expected.revision += 1,
            1 => fs::write(
                dir.path().join("config.toml"),
                "# edited\n[session]\nresume_agents_on_restore=true\n",
            )
            .unwrap(),
            2 => source.0.lock().unwrap().witness.pid += 1,
            3 => source.0.lock().unwrap().config = dir.path().join("other.toml"),
            _ => fs::write(
                dir.path().join("fake-local-checker"),
                "#!/bin/sh\nprintf 'config: ok\\n'\n# changed",
            )
            .unwrap(),
        }
        let result = apply(prepared, &expected, source).await;
        assert!(
            result.as_ref().is_err_and(|failure| !failure.published),
            "apply_owner_preview_fence_contract: {changed} {result:?}"
        );
        assert!(
            !dir.path().join("checks").exists(),
            "apply_owner_stale_preview_runs_checker_zero_contract: {changed}"
        );
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            2,
            "apply_owner_stale_preview_creates_zero_contract"
        );
    }
}

#[tokio::test]
async fn consent_apply_owner_refuses_missing_and_symlink_replacement_cas() {
    for missing in [true, false] {
        let (dir, source) = fixture("printf 'config: ok\\n'");
        let config = dir.path().join("config.toml");
        fs::remove_file(&config).unwrap();
        if !missing {
            let target = dir.path().join("target.toml");
            fs::write(&target, "[session]\nresume_agents_on_restore=true\n").unwrap();
            std::os::unix::fs::symlink(&target, &config).unwrap();
        }
        let prepared = preview::prepare(source.clone(), 1).await.unwrap();
        let expected = prepared.token.clone();
        if !missing {
            fs::remove_file(&config).unwrap();
        }
        fs::write(
            &config,
            "# replacement\n[session]\nresume_agents_on_restore=true\n",
        )
        .unwrap();
        let before = fs::read(&config).unwrap();
        let result = apply(prepared, &expected, source).await;
        assert!(
            result.as_ref().is_err_and(|failure| !failure.published),
            "apply_owner_missing_or_chain_cas_contract: {missing} {result:?}"
        );
        assert_eq!(fs::read(&config).unwrap(), before);
    }
}

#[tokio::test]
async fn consent_apply_owner_bounds_checker_diagnostics_and_timeout() {
    for (body, reason) in [
        (
            "printf 'config: ok\\nunknown key: theme.typo\\n'",
            "Diagnostics",
        ),
        ("while :; do :; done", "Timeout"),
    ] {
        let (dir, source) = fixture(&format!(
            "if [ -f \"$HOME/apply\" ]; then {body}; else printf 'config: ok\\n'; fi"
        ));
        let prepared = preview::prepare(source.clone(), 1).await.unwrap();
        fs::write(dir.path().join("apply"), "apply").unwrap();
        let expected = prepared.token.clone();
        let before = fs::read(dir.path().join("config.toml")).unwrap();
        let result = apply(prepared, &expected, source).await;
        assert!(
            result
                .as_ref()
                .is_err_and(|failure| !failure.published && failure.detail.contains(reason)),
            "apply_owner_checker_failure_never_publishes_contract: {result:?}"
        );
        assert_eq!(fs::read(dir.path().join("config.toml")).unwrap(), before);
    }
}

#[tokio::test]
async fn consent_apply_owner_rechecks_config_peer_and_candidate_after_checker() {
    for change in 0..3 {
        let body = match change {
            0 => "printf '# checker mutation\\n' >> \"$HOME/config.toml\"; printf 'config: ok\\n'",
            1 => {
                "printf '# candidate mutation\\n' >> \"$HERDR_CONFIG_PATH\"; printf 'config: ok\\n'"
            }
            _ => "printf '# checker replacement\\n' >> \"$0\"; printf 'config: ok\\n'",
        };
        let (dir, source) = fixture(&format!(
            "if [ -f \"$HOME/apply\" ]; then {body}; else printf 'config: ok\\n'; fi"
        ));
        let prepared = preview::prepare(source.clone(), 1).await.unwrap();
        fs::write(dir.path().join("apply"), "apply").unwrap();
        let expected = prepared.token.clone();
        let result = apply(prepared, &expected, source).await;
        assert!(
            result.as_ref().is_err_and(|failure| !failure.published),
            "apply_owner_after_checker_fences_contract: {change} {result:?}"
        );
        assert!(
            fs::read_to_string(dir.path().join("config.toml"))
                .unwrap()
                .contains("=true")
        );
    }
}

#[tokio::test]
async fn consent_apply_owner_refuses_peer_replacement_during_checker() {
    struct ReplacedSource {
        source: Arc<preview::tests::Source>,
        marker: std::path::PathBuf,
    }
    impl PeerSource for ReplacedSource {
        fn snapshot(
            &self,
        ) -> Result<super::super::super::peer::PeerSnapshot, super::super::super::Unverified>
        {
            let mut peer = self.source.snapshot()?;
            if self.marker.exists() {
                peer.witness.pid += 1;
            }
            Ok(peer)
        }
    }
    let (dir, source) = fixture(
        "if [ -f \"$HOME/apply\" ]; then printf 'changed' > \"$HOME/peer-replaced\"; fi; printf 'config: ok\\n'",
    );
    let source = Arc::new(ReplacedSource {
        source,
        marker: dir.path().join("peer-replaced"),
    });
    let prepared = preview::prepare(source.clone(), 1).await.unwrap();
    let expected = prepared.token.clone();
    fs::write(dir.path().join("apply"), "apply").unwrap();
    let result = apply(prepared, &expected, source).await;
    assert!(
        result
            .as_ref()
            .is_err_and(|failure| !failure.published && failure.detail.contains("peer changed")),
        "apply_owner_after_checker_peer_fence_contract: {result:?}"
    );
    assert!(
        fs::read_to_string(dir.path().join("config.toml"))
            .unwrap()
            .contains("=true")
    );
}
