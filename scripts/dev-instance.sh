#!/usr/bin/env bash
unset TMUX TMUX_PANE
set +x
set -euo pipefail
cd "$(dirname "$0")/.."
exec python3 -E -s -m scripts.dev_instance "$@"
