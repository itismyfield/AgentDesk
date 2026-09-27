#!/usr/bin/env python3
"""Observe release Cargo wiring in an isolated, stub-only deployment fixture."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import subprocess
import sys
import tempfile

REPO = Path(__file__).resolve().parents[1]
DEPLOY_END = "\n_clean_release_build_cache_after_staging\n"
DEFAULTS = """
ADK_DEFAULT_PORT=1
setup_sccache_env() { return 1; }
_preflight_resource_contention() { return 0; }
wait_for_live_turns_to_drain_or_fail() { return 0; }
"""
SHELL_ENV = """
readonly HOME PATH TMPDIR AGENTDESK_ROOT_DIR
kill() { printf '%s\\n' 'builtin kill forbidden' >> "$DRY_ROOT/blocked"; return 97; }
command_not_found_handle() {
    printf 'unknown command: %s\\n' "$*" >> "$DRY_ROOT/blocked"
    return 97
}
readonly -f kill command_not_found_handle
"""
STUBS = """cargo python3 bash rustc dirname uname sed git curl jq npm node mkdir
cp rm rsync chmod touch mktemp xattr codesign ps launchctl ssh tmux nohup env
lockf flock security gh cat head tail tr awk find sort shasum sha256sum date
mv stat strings sleep id lsof chflags ruby psql cksum install pgrep pkill
basename ln wc hostname cmp""".split()


def prepare(root: Path, repo: Path, script: str) -> dict[str, str]:
    """Copy reviewed shell inputs; replace only the deploy tail and dependencies."""
    source = (repo / "scripts" / script).read_text()
    if script == "deploy-release.sh":
        if source.count(DEPLOY_END) != 1:
            raise ValueError("dry boundary: expected one deploy cleanup completion")
        source = source.split(DEPLOY_END)[0] + DEPLOY_END
    for name in ("bin", "home", "tmp", "repo/scripts", "repo/skills", "repo/policies",
                 "repo/dashboard/dist", "release/bin", "target"):
        (root / name).mkdir(parents=True)
    scripts = root / "repo/scripts"
    (scripts / script).write_text(source + '\nprintf complete > "$DRY_ROOT/completed"\n')
    (scripts / "_defaults.sh").write_text(DEFAULTS)
    (root / "shell-env").write_text(SHELL_ENV)
    (root / "repo/dashboard/dist/index.html").write_text("dry dashboard")
    token_source = (repo / "scripts/build_token.py").read_text()
    canonical = 'CANONICAL_TOKEN_PATH = "/tmp/adk-build-token.lock"'
    if token_source.count(canonical) != 1:
        raise ValueError("dry token: cannot relocate canonical token")
    (scripts / "build_token.py").write_text(token_source.replace(
        canonical, f"CANONICAL_TOKEN_PATH = {str(root / 'token')!r}"))
    stub = (Path(__file__).with_name("release_token_dry_stub.py")).read_text()
    for name in STUBS:
        path = root / "bin" / name
        path.write_text(f"#!{sys.executable} -I\n" + stub)
        path.chmod(0o700)
    return {
        "HOME": str(root / "home"), "PATH": str(root / "bin"),
        "TMPDIR": str(root / "tmp"), "DRY_ROOT": str(root),
        "BASH_ENV": str(root / "shell-env"), "LC_ALL": "C",
        "CARGO_TARGET_DIR": str(root / "target"), "RUSTC_WRAPPER": "",
        "CARGO_BUILD_RUSTC_WRAPPER": "", "ADK_BUILD_TOKEN_WAIT_TIMEOUT_SECS": "5",
        "AGENTDESK_ROOT_DIR": str(root / "release"),
        "AGENTDESK_DEPLOY_NO_DETACH": "1", "AGENTDESK_DEPLOY_LOCK_HELD": "1",
        "AGENTDESK_CODESIGN_IDENTITY": "-", "AGENTDESK_ALLOW_ADHOC_RELEASE_SIGN": "1",
        "AGENTDESK_DEPLOY_SKIP_FRESHNESS": "1",
    }


def observe(repo: Path, script: str, profile: str, *, evidence: Path | None = None,
            target: str | None = None) -> dict:
    with tempfile.TemporaryDirectory(prefix="adk-release-dry-") as temp:
        root = Path(temp).resolve()
        env = prepare(root, repo, script)
        args = ["--fast"] if profile == "release-fast" else []
        if script == "build-release.sh":
            target = target or "aarch64-apple-darwin"
            args = ["--profile", profile, "--target", target]
        result = subprocess.run(["/bin/bash", str(root / "repo/scripts" / script), *args],
                                cwd=root / "repo", env=env, text=True,
                                capture_output=True, timeout=30)
        log = root / "events.jsonl"
        events = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
        cargo = [event for event in events if event["command"] == "cargo"]
        errors = []
        if result.returncode or not (root / "completed").exists():
            errors.append(f"dry execution incomplete: exit={result.returncode}")
        if (root / "blocked").exists():
            errors.append("dry safety: " + (root / "blocked").read_text().strip())
        for call in cargo:
            if call["release"] and not call["held"]:
                errors.append(f"release cargo outside build token: {call['argv']}")
        phases = {call["argv"][0] for call in cargo if call["release"]}
        required = {"build", "clean"} if script == "deploy-release.sh" else {"build"}
        if not required <= phases:
            errors.append(f"dry coverage: missing Cargo phases {sorted(required - phases)}")
        report = {"script": script, "profile": profile, "target": target, "errors": errors,
                  "cargo": cargo, "events": events, "stdout": result.stdout, "stderr": result.stderr}
        if evidence:
            evidence.mkdir(parents=True, exist_ok=True)
            name = f"{script}-{profile}-{target or 'native'}.json"
            (evidence / name).write_text(json.dumps(report, indent=2) + "\n")
        return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=Path, default=REPO)
    parser.add_argument("--evidence", type=Path)
    args = parser.parse_args()
    failed = False
    for script in ("build-release.sh", "deploy-release.sh"):
        for profile in ("release", "release-fast"):
            report = observe(args.repo_root, script, profile, evidence=args.evidence)
            for error in report["errors"]:
                print(f"{script} {profile}: {error}", file=sys.stderr)
            failed |= bool(report["errors"])
            print(f"{script} {profile}: {len(report['cargo'])} Cargo calls, "
                  f"{'FAIL' if report['errors'] else 'PASS'}")
    return int(failed)


if __name__ == "__main__":
    raise SystemExit(main())
