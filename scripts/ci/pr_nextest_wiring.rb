# Exact PR phase gates and worker wiring. Main and nightly keep their full sweep.
module PrNextestWiring
  BUILD_GATE = "${{ !github.event.pull_request.draft && !cancelled() && needs.changes.outputs.rust_build == 'true' }}"
  TEST_READY = "!github.event.pull_request.draft && !cancelled() && needs.changes.outputs.rust_tests == 'true' && needs.rust_build.result == 'success'"
  BUILT_GATE = "${{ #{TEST_READY} }}"
  SCOPE_GATE = "${{ #{TEST_READY} && needs.rust_build.outputs.scope_workers != '[]' }}"
  SWEEP_GATE = "${{ #{TEST_READY} && (needs.rust_scope.result == 'success' || (needs.rust_scope.result == 'skipped' && needs.rust_build.outputs.scope_workers == '[]')) }}"
  EVIDENCE_GATE = "${{ !github.event.pull_request.draft && !cancelled() }}"
  MAIN_JOB_SHA = {
    "rust_build" => "c01152a824550c7742535e4bddcb2484ecc23588262ee5d0d4e6b82fd58854ff",
    "rust_tests" => "dd4ddd1b124bfc72c0eeb9fe6738682ec413b4de3dee08a9973f123866fd18a0",
    "test_evidence" => "76e76c159c32a8899fa5b7b11b1396fc609cbfd232a3400e669990f3633660ba",
  }.freeze

  def self.matrix(job_id)
    output = job_id == "rust_scope" ? "scope_workers" : "full_workers"
    {"fail-fast" => false, "matrix" => {"worker" => "${{ fromJSON(needs.rust_build.outputs.#{output}) }}"}}
  end

  def self.targets(runner)
    {
      "rust_build" => {
        "label" => "nextest build and plan job", "name" => "Rust tests build and plan",
        "needs" => "changes", "if" => BUILD_GATE, "runs_on" => runner,
        "job_timeout_minutes" => 100, "job_sha256" => "ba5e649f2a68374fdd6e9fbd795216afaeefa06156d1318460266e34a074bc07",
        "cargo_steps" => {
          "Disable remote compiler cache" => {
            "commands" => [%q{printf 'RUSTC_WRAPPER=\nSCCACHE_GHA_ENABLED=\nADK_CARGO_PRIORITY=1\n' >> "$GITHUB_ENV"}], "timeout_minutes" => nil,
            "if_condition" => "runner.environment == 'self-hosted'", "env" => {"BASH_ENV" => "/dev/null"},
          },
          "Check preinstalled cargo-nextest" => {
            "commands" => ['bash scripts/ci/preinstalled-tool.sh "$TOOL"'], "timeout_minutes" => nil,
            "if_condition" => "runner.environment == 'self-hosted'", "env" => {"BASH_ENV" => "/dev/null", "TOOL" => "cargo-nextest@0.9.149"},
          },
          "Check Rust test source wiring" => {"commands" => ["python3 scripts/ci/rust_test_sources.py check"], "timeout_minutes" => nil, "env" => {"BASH_ENV" => "/dev/null"}},
          "Archive, list, compare and plan" => {
            "commands" => [
              "set -euo pipefail", 'NEXTEST_DIR="$RUNNER_TEMP/nextest"', 'mkdir -p "$NEXTEST_DIR" "$RUNNER_TEMP/nextest-census"',
              'cargo nextest archive --lib --workspace --archive-file "$NEXTEST_DIR/lib-tests.tar.zst"',
              %q{${{ runner.environment == 'self-hosted' && 'ADK_CARGO_SLOT_HELD=1 ' || '' }}cargo nextest list --archive-file "$NEXTEST_DIR/lib-tests.tar.zst" --workspace-remap "$GITHUB_WORKSPACE" \\},
              '--extract-to "$RUNNER_TEMP/nextest-census" --ignore-default-filter --message-format json > "$NEXTEST_DIR/universe.json"',
              'python3 scripts/check_relay_authority_contract.py --nextest-list "$NEXTEST_DIR/universe.json"',
              'python3 scripts/ci/rust_test_sources.py compare --platform linux --list "$NEXTEST_DIR/universe.json" \\',
              '--json-out "$NEXTEST_DIR/compare.json"',
              'mapfile -t timings < <(bash scripts/ci/nextest-timings.sh "$RUNNER_TEMP/nextest-timings" 6)',
              'python3 scripts/ci/nextest_ci.py plan --universe "$NEXTEST_DIR/universe.json" \\',
              '--archive "$NEXTEST_DIR/lib-tests.tar.zst" --workers 6 --sha "$GITHUB_SHA" --scope linux-lib \\',
              %q{--rust-version 1.94.1 --nextest-version "$(${{ runner.environment == 'self-hosted' && 'ADK_CARGO_SLOT_HELD=1 ' || '' }}cargo nextest --version | awk 'NR==1 {print $2}')" \\},
              "--target x86_64-unknown-linux-gnu --build-option CARGO_PROFILE_DEV_DEBUG=0 \\",
              '--build-option CARGO_PROFILE_TEST_DEBUG=0 --retry-mode all-once --pr-scope "$GITHUB_OUTPUT" --output "$NEXTEST_DIR/plan" "${timings[@]}"',
              'python3 scripts/ci/nextest_ci.py selector-audit --plan "$NEXTEST_DIR/plan/plan.json"',
            ],
            "timeout_minutes" => 45, "env" => {"BASH_ENV" => "/dev/null", "GH_TOKEN" => "${{ github.token }}", "PR_HEAD_SHA" => "${{ github.event.pull_request.head.sha }}"},
          },
        },
      },
      # The workers do not wait for it; the judge still requires it through N.
      "base_compare" => {
        "label" => "nextest base comparison job", "name" => "Rust tests base comparison",
        "needs" => %w[changes rust_build], "if" => BUILT_GATE, "runs_on" => "ubuntu-latest",
        "job_timeout_minutes" => 60, "job_sha256" => "7a866d100f516abb953a142fb9c4f517a7c7fd53d7e95ac401bcc5a8f02c1c8b",
        "cargo_steps" => {
          # Dropping this step or changing its argv (a report-only diff) is rejected here.
          "Compare tests with the comparison base" => {
            "commands" => ['bash scripts/ci/nextest-base-diff.sh "$RUNNER_TEMP/nextest"'], "timeout_minutes" => 40,
            "env" => {"BASH_ENV" => "/dev/null", "GH_TOKEN" => "${{ github.token }}", "PR_HEAD_SHA" => "${{ github.event.pull_request.head.sha }}"},
          },
        },
      },
      "rust_scope" => {
        "label" => "nextest worker job", "name" => "Rust tests (worker ${{ matrix.worker }})",
        "needs" => %w[changes rust_build], "if" => SCOPE_GATE, "runs_on" => "ubuntu-latest",
        "job_timeout_minutes" => 75, "job_sha256" => "9fd6d257c5d4851e8e58cc03969b0bfd2ca9ef4e0caf8bcb824a38f819961b7a", "require_debug_env" => false,
        "cargo_steps" => {
          "Verify archive and plan" => {
            "commands" => ['python3 scripts/ci/nextest_ci.py verify-archive --plan "$RUNNER_TEMP/nextest/plan/plan.json" --worker ${{ matrix.worker }} --archive "$RUNNER_TEMP/nextest/lib-tests.tar.zst" --config "$RUNNER_TEMP/nextest/plan/worker-${{ matrix.worker }}.toml" --sha "$GITHUB_SHA" --record "$RUNNER_TEMP/nextest-result"'],
            "timeout_minutes" => nil, "env" => {"BASH_ENV" => "/dev/null"},
          },
          "Run planned tests" => {
            "commands" => ['bash scripts/ci/nextest-pr-worker.sh ${{ matrix.worker }} scope'],
            "timeout_minutes" => 60, "env" => {"BASH_ENV" => "/dev/null", "ADK_TEST_HERDR_BIN" => "${{ steps.setup.outputs.herdr-bin }}"},
          },
        },
      },
      "rust_tests" => {
        "label" => "nextest worker job", "name" => "Rust tests (worker ${{ matrix.worker }})",
        "needs" => %w[changes rust_build rust_scope], "if" => SWEEP_GATE, "runs_on" => "ubuntu-latest",
        "job_timeout_minutes" => 75, "job_sha256" => "2a372be9809653ad974a69acf76149773fc0abce0432e0803467e57dc481b71b", "require_debug_env" => false,
        "cargo_steps" => {
          "Verify archive and plan" => {
            "commands" => ['python3 scripts/ci/nextest_ci.py verify-archive --plan "$RUNNER_TEMP/nextest/plan/plan.json" --worker ${{ matrix.worker }} --archive "$RUNNER_TEMP/nextest/lib-tests.tar.zst" --config "$RUNNER_TEMP/nextest/plan/worker-${{ matrix.worker }}.toml" --sha "$GITHUB_SHA" --record "$RUNNER_TEMP/nextest-result"'],
            "timeout_minutes" => nil, "env" => {"BASH_ENV" => "/dev/null"},
          },
          "Run planned tests" => {
            "commands" => ['bash scripts/ci/nextest-pr-worker.sh ${{ matrix.worker }}'],
            "timeout_minutes" => 60, "env" => {"BASH_ENV" => "/dev/null", "ADK_TEST_HERDR_BIN" => "${{ steps.setup.outputs.herdr-bin }}"},
          },
        },
      },
      "test_evidence" => {
        "label" => "nextest evidence job", "name" => "Rust tests evidence",
        "needs" => %w[changes rust_build rust_scope rust_tests], "if" => EVIDENCE_GATE, "runs_on" => "ubuntu-latest",
        "job_timeout_minutes" => 15, "job_sha256" => "27e2ef32e230071b757d85900e1ac2b8e479dc03093338b4a0cf75de3008e0ad", "require_debug_env" => false,
        "cargo_steps" => {
          "Audit worker results" => {
            "commands" => [
              "set -euo pipefail", 'plan="$RUNNER_TEMP/nextest/plan/plan.json"',
              'python3 scripts/ci/nextest_ci.py collect-results --plan "$plan" --artifacts "$RUNNER_TEMP/nextest-artifacts" \\',
              '--output "$RUNNER_TEMP/nextest-results"',
              'python3 scripts/ci/nextest_ci.py selector-audit --plan "$plan"', "status=0",
              'python3 scripts/ci/nextest_ci.py audit --plan "$plan" --results "$RUNNER_TEMP/nextest-results" \\',
              '--json-out "$RUNNER_TEMP/verdict.json" --summary-out "$RUNNER_TEMP/summary.md" \\',
              '--pr-scope-out "$RUNNER_TEMP/scope-observation.json" --pr-scope-result "$SCOPE_RESULT" \\',
              '--pr-full-result "$FULL_RESULT" || status=$?',
              'cat "$RUNNER_TEMP/summary.md" >> "$GITHUB_STEP_SUMMARY"', 'exit "$status"',
            ],
            "timeout_minutes" => nil, "if_condition" => "${{ !cancelled() && steps.plan.outcome == 'success' }}",
            "env" => {"BASH_ENV" => "/dev/null", "SCOPE_RESULT" => "${{ needs.rust_scope.result }}",
                      "FULL_RESULT" => "${{ needs.rust_tests.result }}"},
          },
          "Record unavailable Rust evidence" => {
            "commands" => [
              '[ ! -f "$RUNNER_TEMP/scope-observation.json" ] || exit 0',
              "reason=rust-build-unavailable",
              'if [ "$RUST_TESTS" = false ] && { [ "$BUILD_RESULT" = success ] || [ "$BUILD_RESULT" = skipped ]; }; then',
              "reason=rust-path-filter", 'elif [ "$BUILD_RESULT" = success ]; then',
              "reason=plan-unavailable", "fi",
              'python3 scripts/ci/nextest_ci.py pr-scope-unavailable --reason "$reason" --output "$RUNNER_TEMP/scope-observation.json"',
            ],
            "timeout_minutes" => nil, "if_condition" => "${{ !cancelled() && steps.audit.outcome != 'success' }}",
            "env" => {"BASH_ENV" => "/dev/null", "RUST_TESTS" => "${{ needs.changes.outputs.rust_tests }}",
                      "BUILD_RESULT" => "${{ needs.rust_build.result }}"},
          },
        },
      },
    }
  end
end

def canonical_yaml(value)
  case value
  when Hash
    value.keys.sort_by(&:to_s).each_with_object({}) do |key, canonical|
      item = value[key]
      next if key.to_s == "continue-on-error" && (item.nil? || item == false)

      canonical[key.to_s] = canonical_yaml(item)
    end
  when Array
    value.map { |item| canonical_yaml(item) }
  else
    value
  end
end
