from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]


def test_release_fast_profile_is_opt_in_and_conservative():
    cargo_toml = (REPO_ROOT / "Cargo.toml").read_text()
    assert "[profile.release-fast]" in cargo_toml
    assert 'inherits = "release"' in cargo_toml
    assert "lto = false" in cargo_toml
    assert "strip = false" in cargo_toml

    deploy_script = (REPO_ROOT / "scripts" / "deploy-release.sh").read_text()
    assert "--fast)" in deploy_script
    assert "AGENTDESK_DEPLOY_FAST=1" in deploy_script
    assert 'DEPLOY_BUILD_PROFILE="release-fast"' in deploy_script
    assert 'cargo build --profile "$DEPLOY_BUILD_PROFILE" --bin agentdesk' in deploy_script
    assert '_resolve_default_release_binary "$DEPLOY_BUILD_PROFILE"' in deploy_script
