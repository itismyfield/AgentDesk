#!/usr/bin/env bash
# The Rust test workers install the pinned Herdr test binary in their own step, so a
# release download outage fails that step as infrastructure instead of failing tests.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FIXTURE="$ROOT_DIR/tests/fixtures/missing_marker_nextest.sh"
CLASSIFIER="$ROOT_DIR/scripts/ci/infra-failure-rerun.sh"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/herdr-test-binary-ci.XXXXXX")"
trap 'rm -rf "$WORK_DIR"' EXIT
FAILED=0

fail() { echo "not ok - $1" >&2; FAILED=1; }
pass() { echo "ok - $1"; }
check() { if eval "$2"; then pass "$1"; else fail "$1"; fi; }

expected="$(sed -n 's/^expected=\([0-9a-f]\{64\}\)$/\1/p' "$FIXTURE")"
[[ ${#expected} -eq 64 ]] || { echo "not ok - fixture sha256 pin not found" >&2; exit 1; }

# Writes the worker's install step script to $3 after checking its wiring in workflow $1.
check_wiring() {
  local workflow="$1" label="$2" out="$3"
  if ruby -ryaml - "$ROOT_DIR/$workflow" "$expected" "$out" <<'RUBY'; then pass "$label wiring"; else fail "$label wiring"; fi
path, expected, out = ARGV
steps = Array(YAML.load_file(path).dig("jobs", "rust_tests", "steps"))
find = ->(&pred) { steps.index { |step| step.is_a?(Hash) && pred.call(step) } }
cache_at = find.call { |step| step["uses"] == "actions/cache@v4" }
install_at = find.call { |step| step["name"] == "Install herdr test binary" }
tests_at = find.call { |step| step["name"] == "Run planned tests" }
cache, install, tests = [cache_at, install_at, tests_at].map { |at| at && steps[at] }
errors = []
unless cache && install && tests && cache_at < install_at && install_at < tests_at
  errors << "rust_tests must restore the cache, then install herdr, then run the tests"
else
  key = "herdr-0.9.5-#{expected[0, 12]}-linux-x86_64"
  errors << "cache key must be #{key}" unless cache.dig("with", "key") == key
  errors << "cache must hold the install directory" unless cache.dig("with", "path") == "${{ runner.temp }}/herdr-test-bin"
  errors << "install must fail the job" if install.key?("continue-on-error") || install.key?("if")
  errors << "install must read the cache hit" unless install.dig("env", "HERDR_CACHE_HIT") == "${{ steps.#{cache["id"]}.outputs.cache-hit }}"
  errors << "install must pin the fixture sha256" unless install["run"].to_s.include?("expected=#{expected}\n")
  bin = "${{ steps.#{install["id"]}.outputs.bin }}"
  errors << "Run planned tests must receive ADK_TEST_HERDR_BIN from the install step" unless tests.dig("env", "ADK_TEST_HERDR_BIN") == bin
  File.write(out, install["run"])
end
errors.each { |error| warn "#{path}: #{error}" }
exit(errors.empty? ? 0 : 1)
RUBY
}

pr_step="$WORK_DIR/pr-step.sh"
main_step="$WORK_DIR/main-step.sh"
check_wiring .github/workflows/ci-pr.yml "PR worker" "$pr_step"
check_wiring .github/workflows/ci-main.yml "main worker" "$main_step"
check "main worker installs herdr with the PR worker's script" '[[ -s "$pr_step" ]] && cmp -s "$pr_step" "$main_step"'

fakebin="$WORK_DIR/bin"
mkdir -p "$fakebin"
# Records each call; FAKE_CURL is an HTTP status, or "junk" for a 200 with wrong bytes.
cat >"$fakebin/curl" <<'SH'
#!/usr/bin/env bash
echo "$*" >>"$FAKE_CURL_CALLS"
while [[ $# -gt 0 ]]; do [[ "$1" == --output ]] && output="$2"; shift; done
case "$FAKE_CURL" in
  junk) echo junk >"$output"; printf 200 ;;
  copy) cp "$FAKE_CURL_SOURCE" "$output"; printf 200 ;;
  *) printf '%s' "$FAKE_CURL"; echo "curl: (22) The requested URL returned error: $FAKE_CURL" >&2; exit 22 ;;
esac
SH
printf '#!/bin/sh\nexit 0\n' >"$fakebin/flock"
chmod +x "$fakebin/curl" "$fakebin/flock"
# macOS sha256sum has no --check; shasum reads the same check lines.
if ! sha256sum --version 2>/dev/null | grep -q coreutils; then
  printf '#!/bin/sh\nexec shasum -a 256 "$@"\n' >"$fakebin/sha256sum"
  chmod +x "$fakebin/sha256sum"
fi

# Runs the step as Actions does; the log starts with the script, as the job log echoes it.
run_step() {
  local case_dir="$WORK_DIR/$1" script="$2" hit="$3" mode="$4"
  mkdir -p "$case_dir/tmp"
  : >"$case_dir/out"
  cp "$script" "$case_dir/log"
  set +e
  PATH="$fakebin:$PATH" RUNNER_TEMP="$case_dir/tmp" GITHUB_OUTPUT="$case_dir/out" HERDR_CACHE_HIT="$hit" \
    FAKE_CURL="$mode" FAKE_CURL_CALLS="$case_dir/calls" FAKE_CURL_SOURCE="$WORK_DIR/fake-herdr" \
    bash --noprofile --norc -eo pipefail "$script" >>"$case_dir/log" 2>&1
  echo "$?" >"$case_dir/rc"
  set -e
}
classify() { "$CLASSIFIER" --classify-job "Rust tests (worker 0)" "$WORK_DIR/$1/log" 9; }

if [[ -s "$pr_step" ]]; then
  run_step outage "$pr_step" "" 504
  check "a 504 after retries fails the install step" '[[ $(cat "$WORK_DIR/outage/rc") != 0 && ! -s "$WORK_DIR/outage/out" ]]'
  check "a 504 install failure is classified as infrastructure" '[[ $(classify outage) == infra-unrelated ]]'

  run_step missing "$pr_step" "" 404
  check "a 404 is not classified as infrastructure" '[[ $(cat "$WORK_DIR/missing/rc") != 0 && $(classify missing) == unrelated-failure ]]'

  run_step wrong-bytes "$pr_step" "" junk
  check "a checksum mismatch fails and is not infrastructure" '[[ $(cat "$WORK_DIR/wrong-bytes/rc") != 0 && $(classify wrong-bytes) == unrelated-failure ]]'

  mkdir -p "$WORK_DIR/poisoned/tmp/herdr-test-bin"
  echo junk >"$WORK_DIR/poisoned/tmp/herdr-test-bin/herdr-0.9.5"
  run_step poisoned "$pr_step" true 504
  check "a restored binary with other bytes fails without downloading" '[[ $(cat "$WORK_DIR/poisoned/rc") != 0 && ! -e "$WORK_DIR/poisoned/calls" && ! -s "$WORK_DIR/poisoned/out" ]]'

  # Only the pinned digest is swapped, so the remaining success path runs as written.
  echo fake-herdr >"$WORK_DIR/fake-herdr"
  fake_sha="$(sha256sum "$WORK_DIR/fake-herdr" | cut -d ' ' -f 1)"
  sed "s/$expected/$fake_sha/" "$pr_step" >"$WORK_DIR/repinned-step.sh"
  run_step download "$WORK_DIR/repinned-step.sh" "" copy
  check "a miss downloads the release once and outputs the binary" \
    '[[ $(cat "$WORK_DIR/download/rc") == 0 && $(cat "$WORK_DIR/download/out") == "bin=$WORK_DIR/download/tmp/herdr-test-bin/herdr-0.9.5" && -x "$WORK_DIR/download/tmp/herdr-test-bin/herdr-0.9.5" && $(wc -l <"$WORK_DIR/download/calls") -eq 1 ]] && grep -q "https://github.com/herdrdev/herdr/releases/download/v0.9.5/herdr-linux-x86_64" "$WORK_DIR/download/calls"'
  mkdir -p "$WORK_DIR/hit/tmp/herdr-test-bin"
  cp "$WORK_DIR/fake-herdr" "$WORK_DIR/hit/tmp/herdr-test-bin/herdr-0.9.5"
  run_step hit "$WORK_DIR/repinned-step.sh" true 504
  check "a cache hit outputs the binary without downloading" '[[ $(cat "$WORK_DIR/hit/rc") == 0 && ! -e "$WORK_DIR/hit/calls" && $(cat "$WORK_DIR/hit/out") == "bin=$WORK_DIR/hit/tmp/herdr-test-bin/herdr-0.9.5" ]]'
fi

# The fixture uses ADK_TEST_HERDR_BIN without downloading and still rejects other bytes.
mkdir -p "$WORK_DIR/fixture/tmp"
echo junk >"$WORK_DIR/fixture/herdr"
set +e
PATH="$fakebin:$PATH" RUNNER_TEMP="$WORK_DIR/fixture/tmp" ADK_TEST_HERDR_BIN="$WORK_DIR/fixture/herdr" \
  FAKE_CURL=504 FAKE_CURL_CALLS="$WORK_DIR/fixture/calls" \
  bash "$FIXTURE" touch "$WORK_DIR/fixture/ran" >"$WORK_DIR/fixture/log" 2>&1
echo "$?" >"$WORK_DIR/fixture/rc"
set -e
check "the fixture refuses a provided binary with other bytes without downloading" \
  '[[ $(cat "$WORK_DIR/fixture/rc") != 0 && ! -e "$WORK_DIR/fixture/calls" && ! -e "$WORK_DIR/fixture/ran" ]]'

if [[ "$FAILED" -ne 0 ]]; then
  for log in "$WORK_DIR"/*/log; do echo "--- $log" >&2; cat "$log" >&2; done
  exit 1
fi
