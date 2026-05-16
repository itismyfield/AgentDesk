#!/usr/bin/env bash
# Graceful sccache rustc wrapper.
#
# `.cargo/config.toml` points `build.rustc-wrapper` at this script so that
# Cargo invocations always succeed regardless of whether sccache is installed
# on the host. Previously the wrapper was hard-coded to `sccache`, which broke
# bare `cargo` runs on every developer machine that had not yet installed
# sccache and forced agents/subagents to prefix every command with
# `RUSTC_WRAPPER=`.
#
# Behaviour:
#   * If `RUSTC_WRAPPER_DISABLE=1`, exec rustc directly (escape hatch).
#   * If `sccache` is on PATH (or at the conventional Homebrew location),
#     exec `sccache rustc ...` so caching engages.
#   * Otherwise, exec rustc directly. No warning is printed because Cargo
#     invokes this wrapper hundreds of times per build and noise would drown
#     real diagnostics. The opt-in `sccache --show-stats` step in CI and the
#     README troubleshooting section are the documented signal channels.
#
# The first argument from Cargo is the path to `rustc`. We pass the entire
# argument vector through unchanged.

set -eu

if [ "${RUSTC_WRAPPER_DISABLE:-0}" = "1" ]; then
    exec "$@"
fi

# Prefer an explicit binary if the caller set SCCACHE_BIN; this lets release
# scripts pin a known-good sccache install without depending on PATH order.
sccache_bin="${SCCACHE_BIN:-}"

if [ -z "$sccache_bin" ]; then
    if command -v sccache >/dev/null 2>&1; then
        sccache_bin="$(command -v sccache)"
    elif [ -x "/opt/homebrew/bin/sccache" ]; then
        sccache_bin="/opt/homebrew/bin/sccache"
    fi
fi

if [ -n "$sccache_bin" ]; then
    exec "$sccache_bin" "$@"
fi

exec "$@"
