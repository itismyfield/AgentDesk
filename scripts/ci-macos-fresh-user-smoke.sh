#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "fresh user smoke is macOS-only; skipping on $(uname -s)"
  exit 0
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python3 -m unittest tests.test_install_bootstrap_portable

runner_temp="${RUNNER_TEMP:-${TMPDIR:-/tmp}}"
smoke_base="$(mktemp -d "$runner_temp/agentdesk-fresh-user.XXXXXX")"
smoke_base="$(cd "$smoke_base" && pwd -P)"
trap 'rm -rf "$smoke_base"' EXIT

fresh_home="$smoke_base/home"
fresh_root="$smoke_base/release"
mkdir -p "$fresh_home" "$fresh_root/bin" "$fresh_home/Library/LaunchAgents"

runner_home_was_set="${HOME+x}"
runner_home="${HOME:-}"
export HOME="$fresh_home"
export AGENTDESK_ROOT_DIR="$fresh_root"
export AGENTDESK_CONFIG="$fresh_root/config/agentdesk.yaml"

python3 scripts/operator-init-portable.py \
  --root "$fresh_root" \
  --config "$AGENTDESK_CONFIG"

test -f "$AGENTDESK_CONFIG"
test -d "$fresh_root/workspaces"

if [[ -n "$runner_home_was_set" ]]; then
  export HOME="$runner_home"
else
  unset HOME
fi

cargo run --quiet -- emit-launchd-plist \
  --flavor release \
  --home "$fresh_home" \
  --root-dir "$fresh_root" \
  --agentdesk-bin "$fresh_root/bin/agentdesk" \
  --output "$fresh_home/Library/LaunchAgents/com.agentdesk.release.plist"

plutil -lint "$fresh_home/Library/LaunchAgents/com.agentdesk.release.plist"

python3 - "$fresh_home" "$fresh_root" "$AGENTDESK_CONFIG" <<'PY'
from pathlib import Path
import sys

home = Path(sys.argv[1])
root = Path(sys.argv[2])
config = Path(sys.argv[3])
plist = home / "Library" / "LaunchAgents" / "com.agentdesk.release.plist"

private_markers = ("/Users/itismyfield", "/Users/kunkun")
for path in (config, plist):
    text = path.read_text(encoding="utf-8")
    for marker in private_markers:
        if marker in text:
            raise SystemExit(f"{path} contains private path marker {marker}")

plist_text = plist.read_text(encoding="utf-8")
for expected in (str(home), str(root), str(root / "bin" / "agentdesk")):
    if expected not in plist_text:
        raise SystemExit(f"plist missing expected fresh-user path: {expected}")

print("fresh-user portable config and launchd plist are path-clean")
PY

cargo test cli::init::launchd_plist_tests::generate_launchd_plist_uses_requested_fresh_home_and_root_only -- --exact
