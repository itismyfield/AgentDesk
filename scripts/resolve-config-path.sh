#!/usr/bin/env bash
set -euo pipefail
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
compiler="$(command -v rustc || true)"
if [ -z "$compiler" ] && [ -x "$HOME/.cargo/bin/rustc" ]; then
    compiler="$HOME/.cargo/bin/rustc"
fi
if [ -z "$compiler" ]; then
    echo "Cannot resolve deployment config: Rust compiler unavailable" >&2
    exit 1
fi
resolver_dir="$(mktemp -d "${TMPDIR:-/tmp}/agentdesk-config-path.XXXXXX")"
resolver="$resolver_dir/resolver"
trap 'rm -f "$resolver_dir"/*; rmdir "$resolver_dir"' EXIT
"$compiler" --edition 2024 --crate-name agentdesk_config_path \
    "$script_dir/resolve_config_path.rs" -o "$resolver"
"$resolver" "${1:?runtime root required}"
