#!/usr/bin/env bash
set -euo pipefail
# CI's exact selected IDs run the same real server fixture on PR, main and nightly workers.
[[ "$#" -gt 0 ]]
expected=14d9e8bc8b4a9e0118d77c32266f76d461f09b40342eb23752960bd89f59929d
if [[ -n "${ADK_TEST_HERDR_BIN:-}" ]]; then
  fixture_bin="$ADK_TEST_HERDR_BIN"
else
  : "${RUNNER_TEMP:?CI fixture directory required}"
  fixture_dir="$RUNNER_TEMP/missing-marker-herdr"
  mkdir -p "$fixture_dir"
  chmod 700 "$fixture_dir"
  fixture_bin="$fixture_dir/herdr-0.9.5"
  exec 9>"$fixture_dir/install.lock"
  flock 9
  if [[ ! -f "$fixture_bin" ]]; then
    download=$(mktemp "$fixture_dir/download.XXXXXX")
    trap '[[ -z ${download:-} ]] || unlink "$download"' EXIT
    curl --fail --location --retry 3 --silent --show-error --output "$download" https://github.com/herdrdev/herdr/releases/download/v0.9.5/herdr-linux-x86_64
    echo "$expected  $download" | sha256sum --check
    chmod 700 "$download"
    mv "$download" "$fixture_bin"
    download=""
  fi
  flock -u 9
  exec 9>&-
fi
echo "$expected  $fixture_bin" | sha256sum --check
export ADK_TEST_HERDR_BIN="$fixture_bin"
exec "$@"
