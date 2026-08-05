#!/usr/bin/env bash
set -euo pipefail

fail=0

error() {
  echo "ERROR: $*" >&2
  fail=1
}

validate_pr_debug_envs() {
  if ! command -v ruby >/dev/null 2>&1; then
    error "ruby is required to validate $pr_workflow structurally"
    return
  fi

  # Parse the workflow as YAML instead of slicing it as text. That keeps
  # quoted job IDs, flow mappings, escaped keys, and sibling job mappings from
  # satisfying a different job's requirement. Each protected cargo step must
  # also pin the exact values, disable BASH_ENV startup hooks, and retain the
  # exact command inventory; all other step-level copies are rejected.
  if ! ruby - "$pr_workflow" <<'RUBY'
require "yaml"
require "json"
require "digest"

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

path = ARGV.fetch(0)
begin
  document = YAML.load_file(path)
rescue StandardError => error
  warn "#{path}: cannot parse YAML: #{error.message}"
  exit 1
end

jobs = document.is_a?(Hash) ? document["jobs"] : nil
unless jobs.is_a?(Hash)
  warn "#{path}: jobs must be a YAML mapping"
  exit 1
end

expected_concurrency = {
  "group" => 'ci-pr-${{ github.repository }}-${{ github.event.pull_request.number || github.ref }}',
  "cancel-in-progress" => true,
}
unless document["concurrency"] == expected_concurrency
  warn "#{path}: top-level concurrency must retain the exact fork-safe cancellation policy"
  exit 1
end

trigger = document[true] || document["on"]
trigger_events = case trigger
when Hash
  trigger.keys.map(&:to_s)
when Array
  trigger.map(&:to_s)
when String
  [trigger]
else
  []
end
unless trigger_events == ["pull_request"]
  warn "#{path}: required PR contexts must be triggered only by pull_request"
  exit 1
end

# The required Script checks job is intentionally high-churn: concurrent lanes
# regularly add gates to its step inventory. Protect only the job and aggregate
# step fields that can silently disable the required context, rather than
# whole-job hashing that would force unrelated hash re-pins for every new check.
script_checks_job = jobs["scripts"]
unless script_checks_job.is_a?(Hash)
  warn "#{path}: Script checks job (scripts) must be a YAML mapping"
  exit 1
end
if script_checks_job.key?("if")
  warn "#{path}: Script checks job must not define a job-level if condition"
  exit 1
end
if script_checks_job["continue-on-error"]
  warn "#{path}: Script checks job must not be allowed to continue on error"
  exit 1
end
script_checks_needs = script_checks_job["needs"]
unless script_checks_needs == "changes" || script_checks_needs == ["changes"]
  warn "#{path}: Script checks job must retain exact needs: changes"
  exit 1
end
script_check_steps = Array(script_checks_job["steps"]).select do |step|
  step.is_a?(Hash) && step["name"] == "Run script checks"
end
unless script_check_steps.length == 1
  warn "#{path}: Script checks job must retain exactly one \"Run script checks\" step"
  exit 1
end
script_check_step = script_check_steps.fetch(0)
if script_check_step.key?("if")
  warn "#{path}: Script checks job \"Run script checks\" step must not define if"
  exit 1
end
if script_check_step["continue-on-error"]
  warn "#{path}: Script checks job \"Run script checks\" step must not continue on error"
  exit 1
end
script_check_commands = if script_check_step["run"].is_a?(String)
  script_check_step["run"].lines.map(&:strip).reject(&:empty?)
else
  []
end
unless script_check_commands == ["./scripts/ci-script-checks.sh"]
  warn "#{path}: Script checks job \"Run script checks\" step must run exactly ./scripts/ci-script-checks.sh"
  exit 1
end

# The aggregate job is intentionally excluded from the high-churn cargo-job
# `targets` hash below, but its inventory verifier has a hard prerequisite:
# cargo must be available before ci-script-checks.sh starts. Pin the setup shape
# here so removing the toolchain/cache silently cannot recreate D2.
script_steps = Array(script_checks_job["steps"])
setup_specs = {
  "Install Rust toolchain for lib inventory" => {
    "uses" => "dtolnay/rust-toolchain@master",
    "toolchain" => "1.94.1",
  },
  "Setup sccache for lib inventory" => {
    "uses" => "mozilla-actions/sccache-action@v0.0.10",
  },
  "Cache Cargo dependencies for lib inventory" => {
    "uses" => "Swatinem/rust-cache@v2",
    "cache-targets" => false,
    "cache-bin" => false,
    "shared-key" => "cargo-dependencies-v2",
  },
}
setup_specs.each do |name, spec|
  matches = script_steps.select { |step| step.is_a?(Hash) && step["name"] == name }
  unless matches.length == 1
    warn "#{path}: Script checks must retain exactly one #{name.inspect} step"
    exit 1
  end
  step = matches.fetch(0)
  expected_uses = spec.fetch("uses")
  unless step["uses"] == expected_uses
    warn "#{path}: Script checks #{name.inspect} must use #{expected_uses}"
    exit 1
  end
  if spec.key?("toolchain") && step.dig("with", "toolchain") != spec["toolchain"]
    warn "#{path}: Script checks #{name.inspect} must pin Rust 1.94.1"
    exit 1
  end
  spec.each do |key, expected|
    next if key == "uses" || key == "toolchain"
    unless step.dig("with", key) == expected
      warn "#{path}: Script checks #{name.inspect} must retain #{key}=#{expected.inspect}"
      exit 1
    end
  end
end

targets = {
  # The independent explicit step-inventory layer was removed. What remains
  # splits into two mechanisms of very different strength, and conflating them
  # has produced a wrong claim in this comment five rounds running.
  #
  #   1. The whole-job semantic hash only *detects* structural change. Re-pinning
  #      it in the same diff accepts anything. Measured on test_fast: adding an
  #      unregistered step, deleting "Start PostgreSQL service", and swapping two
  #      cache steps each fail against the stale pin (rc=1) and pass after a
  #      re-pin (rc=0), with no expected-step edit needed. Treat the hash as a
  #      review trigger, not a guarantee.
  #   2. The hash is compared in exactly one place. Every other assertion in
  #      this file is independent of it and survives a re-pin.
  #
  # This comment does not enumerate what mechanism 2 covers. Five rounds of
  # trying produced an incomplete or wrong list every time. To find out whether
  # a specific tamper is caught, apply it, re-pin the hash, and run this
  # script -- that answer does not go stale.
  #
  # This registry does not discover new jobs automatically, and there is no
  # invocation-floor replacement in the selection-evidence verifier.
  "check_fast_cross_os" => {
    "label" => "cross-OS job",
    "name" => 'Fast check + non-PG tests (${{ matrix.os }})',
    "needs" => "changes",
    "if" => "needs.changes.outputs.rust_compile == 'true' && needs.changes.outputs.cross_os_rust == 'true'",
    "runs_on" => '${{ matrix.os }}',
    # #4747 (opt.2) re-pins the compile-only PR lane after moving long-pole
    # Windows runtime tests to nightly. Option 3 keeps PR cache access restore-only.
    "job_sha256" => "d27244ced15d0bb13f89e680de42978cb74452af4b02457ab034d462f4fa103a",
    "cargo_steps" => {
      "cargo check" => {
        "commands" => ["cargo check --workspace --all-targets"],
        "timeout_minutes" => nil,
      },
    },
  },
  "test_fast" => {
    "label" => "PostgreSQL job",
    "name" => "PostgreSQL tests (ubuntu-postgres)",
    "needs" => "changes",
    "if" => "needs.changes.outputs.pg_db == 'true'",
    "runs_on" => "ubuntu-latest",
    # #4979 S2 re-pins after adding the AGENTDESK_REQUIRE_PG=1 job env so a PG
    # connection failure in this PG-backed lane hard-fails instead of
    # soft-skipping green; the command inventory itself is unchanged.
    # #5040 re-pins after adding the telemetry-only intake authority regressions
    # to this existing toolchain-provisioned lane whose mirror is required.
    # #5025 and #4985 retain their production bridge and footer-marker coverage
    # in the same job block, so the pin covers the merged command inventory.
    "job_sha256" => "0995b8496416accb68d636a3d115508298b457d035be9dc48e07b9fac6e2a51b",
    "cargo_steps" => {
      "Observe curated lane selections" => {
        "commands" => [
          "set -o pipefail",
          "python3 scripts/check_test_target_integrity.py --observe-selection --workflow .github/workflows/ci-pr.yml --job test_fast --job high-risk-recovery | tee \"$RUNNER_TEMP/selection-evidence-test-fast.log\"",
        ],
        "timeout_minutes" => 20,
      },
      "Require observer summary" => {
        "commands" => [
          "set -euo pipefail",
          "python3 scripts/check_test_target_integrity.py --verify-selection-evidence \"$RUNNER_TEMP/selection-evidence-test-fast.log\"",
        ],
        "timeout_minutes" => 1,
        "if_condition" => "always()",
      },
      "Footer-only marker regressions" => {
        "commands" => [
          "cargo test --lib task_notification -- --skip _pg --skip pg_ --skip postgres",
          "cargo test --lib services::discord::tmux::tmux_watcher::discrete_trigger_marker::tests -- --skip _pg --skip pg_ --skip postgres",
        ],
        "timeout_minutes" => 10,
      },
      "Trusted session forwarding tests" => {
        "commands" => ["env -u AGENTDESK_ROOT_DIR cargo test --lib services::session_forwarding -- --skip _pg --skip pg_ --skip postgres"],
        "timeout_minutes" => 10,
      },
      "Telemetry-only intake authority regressions" => {
        "commands" => ["env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::router::intake_dispatch::tests::telemetry_only_unopted -- --skip _pg --skip pg_ --skip postgres"],
        "timeout_minutes" => 10,
      },
      "Terminal delivery evidence regressions" => {
        "commands" => [
          "env -u AGENTDESK_ROOT_DIR cargo test --lib inflight::terminal_delivery_evidence_loss::tests",
          "env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::turn_bridge::terminal_outcome_delivery::delivery_epilogue_tests",
          "env -u AGENTDESK_ROOT_DIR cargo test --lib watcher_terminal_commit_identity_mismatch_skips_without_clobbering_newer_row",
          "env -u AGENTDESK_ROOT_DIR cargo test --lib identity_guarded_save_rejects_stale_write_against_newer_turn",
        ],
        "timeout_minutes" => 10,
      },
      "just test-postgres" => {
        "commands" => ["just test-postgres"],
        "timeout_minutes" => 20,
      },
    },
  },
  "relay-authority-contract" => {
    "label" => "relay-authority contract job",
    "name" => "relay-authority-contract",
    "needs" => nil,
    "if" => nil,
    "runs_on" => "ubuntu-latest",
    # #5071 registers this unconditional candidate in the existing semantic
    # hardening registry so order-independent job keys cannot disable it silently.
    "job_sha256" => "20faba743fc3c5007680dba1c5b78938d6a82922c9ef879cbe45c043c1a2ee95",
    "cargo_steps" => {
      "Verify named relay-authority targets and selection floors" => {
        "commands" => ["python3 scripts/check_relay_authority_contract.py"],
        "timeout_minutes" => 30,
      },
      "Run named relay-authority contract targets" => {
        "commands" => [
          "env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::session_relay_sink -- --test-threads=1",
          "env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::relay_recovery::tests -- --test-threads=1",
          "env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::tui_prompt_relay::local_model_queue_wake_e2e -- --test-threads=1",
        ],
        "timeout_minutes" => 30,
      },
      "Require relay-authority mutations to be killed" => {
        "commands" => ["bash scripts/run_relay_authority_mutations.sh"],
        "timeout_minutes" => 30,
      },
    },
  },
  "high-risk-recovery" => {
    "label" => "High-risk recovery job",
    "name" => "High-risk recovery",
    "needs" => "changes",
    "if" => "needs.changes.outputs.high_risk_recovery == 'true'",
    "runs_on" => "ubuntu-latest",
    # #5034 re-pins after adding the attachment-delivery and catch-up
    # operational-alert targets to the path-filtered required high-risk lane.
    "job_sha256" => "29c7a0c33753933e50c446f073da942bebd0c53881460260562cd9cabaef9c44",
    "require_debug_env" => false,
    "cargo_steps" => {
      "Observe curated lane selections" => {
        "commands" => [
          "set -o pipefail",
          "python3 scripts/check_test_target_integrity.py --observe-selection --workflow .github/workflows/ci-pr.yml --job high-risk-recovery | tee \"$RUNNER_TEMP/selection-evidence-high-risk.log\"",
        ],
        "timeout_minutes" => 20,
      },
      "Require observer summary" => {
        "commands" => [
          "set -euo pipefail",
          "python3 scripts/check_test_target_integrity.py --verify-selection-evidence \"$RUNNER_TEMP/selection-evidence-high-risk.log\"",
        ],
        "timeout_minutes" => 1,
        "if_condition" => "always()",
      },
    },
  },
}
keys = %w[CARGO_PROFILE_DEV_DEBUG CARGO_PROFILE_TEST_DEBUG]
protected_step_env = {
  "BASH_ENV" => "/dev/null",
  "CARGO_PROFILE_DEV_DEBUG" => "0",
  "CARGO_PROFILE_TEST_DEBUG" => "0",
}
errors = []

targets.each do |job_id, spec|
  label = spec.fetch("label")
  job = jobs[job_id]
  unless job.is_a?(Hash)
    errors << "#{label} (#{job_id}) must be a YAML mapping"
    next
  end

  job_sha256 = Digest::SHA256.hexdigest(JSON.generate(canonical_yaml(job)))
  unless job_sha256 == spec.fetch("job_sha256")
    errors << "#{label} semantic structure or command inventory changed"
  end

  {
    "name" => spec.fetch("name"),
    "needs" => spec.fetch("needs"),
    "if" => spec.fetch("if"),
    "runs-on" => spec.fetch("runs_on"),
  }.each do |field, expected|
    errors << "#{label} must retain exact #{field}" unless job[field] == expected
  end
  if job["continue-on-error"]
    errors << "#{label} must not be allowed to continue on error"
  end
  if job_id == "check_fast_cross_os"
    strategy = job["strategy"]
    unless strategy.is_a?(Hash)
      errors << "#{label} must retain its matrix strategy"
    else
      errors << "#{label} matrix must fail independently" unless strategy["fail-fast"] == false
      errors << "#{label} must retain the Windows matrix" unless strategy.dig("matrix", "os") == ["windows-latest"]
    end
  end

  unless spec["require_debug_env"] == false
    env = job["env"]
    keys.each do |key|
      unless env.is_a?(Hash) && env[key] == "0"
        errors << "#{label} must set job-level #{key} to the string \"0\""
      end
    end
  end

  expected_steps = spec.fetch("cargo_steps")
  seen_steps = []
  Array(job["steps"]).each_with_index do |step, index|
    next unless step.is_a?(Hash)

    name = step["name"]
    run = step["run"]
    step_env = step["env"]
    if expected_steps.key?(name)
      step_spec = expected_steps.fetch(name)
      seen_steps << name
      unless run.is_a?(String)
        errors << "#{label} #{name.inspect} must use a shell run block"
        next
      end
      unless step["shell"] == "bash"
        errors << "#{label} #{name.inspect} must use explicit bash"
      end
      unless step["if"] == step_spec.fetch("if_condition", nil)
        errors << "#{label} #{name.inspect} must retain exact if policy"
      end
      actual_continue_on_error = step["continue-on-error"] || nil
      expected_continue_on_error = step_spec.fetch("continue_on_error", nil) || nil
      unless actual_continue_on_error == expected_continue_on_error
        errors << "#{label} #{name.inspect} must retain exact continue-on-error policy"
      end
      unless step["timeout-minutes"] == step_spec.fetch("timeout_minutes")
        errors << "#{label} #{name.inspect} must retain exact timeout policy"
      end
      unless step_env == protected_step_env
        errors << "#{label} #{name.inspect} must pin exact step env and disable BASH_ENV"
      end
      lines = run.lines.map(&:strip).reject(&:empty?)
      unless lines == step_spec.fetch("commands")
        errors << "#{label} #{name.inspect} must retain the exact cargo/test command list"
      end
    else
      forbidden = keys + ["BASH_ENV"]
      forbidden.each do |key|
        errors << "#{label} step #{index + 1} must not set #{key}" if step_env.is_a?(Hash) && step_env.key?(key)
        errors << "#{label} step #{index + 1} must not mutate #{key} at runtime" if run.is_a?(String) && run.include?(key)
      end
    end
  end
  expected_steps.each_key do |name|
    errors << "#{label} must retain exactly one #{name.inspect} step" unless seen_steps.count(name) == 1
  end
end

errors.each { |message| warn "#{path}: #{message}" }
exit(errors.empty? ? 0 : 1)
RUBY
  then
    error "$pr_workflow must preserve target-job debug stripping without step overrides"
  fi
}

trusted_workflow=".github/workflows/ci-macos-trusted.yml"
pr_workflow=".github/workflows/ci-pr.yml"

workflow_files() {
  find .github/workflows -maxdepth 1 -type f \
    \( -name '*.yml' -o -name '*.yaml' \) -print0
}

validate_workflow_entries() {
  while IFS= read -r -d '' workflow; do
    error "$workflow must not be a symlink; workflow hardening requires regular files"
  done < <(find .github/workflows -type l -print0)
}

validate_required_context_uniqueness() {
  if ! command -v ruby >/dev/null 2>&1; then
    error "ruby is required to validate required workflow contexts structurally"
    return
  fi

  while IFS= read -r -d '' workflow; do
    if ! ruby - "$workflow" "$pr_workflow" <<'RUBY'
require "yaml"

path, pr_path = ARGV
begin
  # Workflow aliases are intentionally unsupported. Rejecting them keeps every
  # audited job definition local and explicit instead of expanding YAML graphs.
  document = YAML.safe_load(File.read(path), aliases: false, filename: path)
rescue StandardError => error
  warn "#{path}: cannot parse YAML: #{error.message}"
  exit 1
end
jobs = document.is_a?(Hash) ? document["jobs"] : nil
unless jobs.is_a?(Hash)
  warn "#{path}: jobs must be a YAML mapping"
  exit 1
end
required_context = "Script checks"
required_context_jobs = []
unsafe_dynamic_name_jobs = []
jobs.each do |job_id, job|
  next unless job.is_a?(Hash)

  name = job["name"]
  required_context_jobs << job_id.to_s if name.to_s.strip == required_context
  next unless name.is_a?(String) && name.include?("${{")

  # Do not evaluate Actions expressions. Permit exactly one matrix substitution
  # whose static prefix/suffix make the required context impossible to render.
  expression_shape_valid = name.scan("${{").length == 1 && name.scan("}}").length == 1
  matrix_name = if expression_shape_valid
    /\A([^{}]*)\$\{\{\s*matrix\.[A-Za-z_][A-Za-z0-9_.-]*\s*\}\}([^{}]*)\z/.match(name)
  end
  can_render_required = if matrix_name
    static_fragments = [matrix_name[1], matrix_name[2]]
    static_fragments.any? { |fragment| fragment.include?(required_context) } ||
      (required_context.start_with?(matrix_name[1]) && required_context.end_with?(matrix_name[2]))
  else
    true
  end
  unsafe_dynamic_name_jobs << job_id.to_s if can_render_required
end
if path == pr_path
  scripts = jobs["scripts"]
  unless scripts.is_a?(Hash) && scripts["name"] == required_context
    warn "#{path}: required Script checks context must be the exact literal name of jobs.scripts"
    exit 1
  end
  unexpected_required = required_context_jobs - ["scripts"]
  if unexpected_required.any?
    warn "#{path}: required Script checks context must belong only to jobs.scripts"
    exit 1
  end
elsif required_context_jobs.any?
  warn "#{path}: must not publish required Script checks context (jobs: #{required_context_jobs.join(', ')})"
  exit 1
end
if unsafe_dynamic_name_jobs.any?
  warn "#{path}: dynamic job names must not be able to publish required Script checks context (jobs: #{unsafe_dynamic_name_jobs.join(', ')})"
  exit 1
end
RUBY
    then
      error "$workflow violates required Script checks context uniqueness"
    fi
  done < <(workflow_files)
}

if [ ! -f "$trusted_workflow" ]; then
  error "missing $trusted_workflow"
fi
if [ ! -f "$pr_workflow" ]; then
  error "missing $pr_workflow"
fi

validate_workflow_entries

while IFS= read -r -d '' workflow; do
  if grep -Eq '^[[:space:]]+pull_request(_target)?:' "$workflow"; then
    if grep -Eq 'MACOS_RUNNER|self-hosted' "$workflow"; then
      error "$workflow is pull_request-triggered and must not reference self-hosted macOS routing"
    fi
  fi

  if [ "$workflow" != "$trusted_workflow" ] && grep -q 'MACOS_RUNNER' "$workflow"; then
    error "$workflow references MACOS_RUNNER outside $trusted_workflow"
  fi

  if grep -q 'RUSTC_WRAPPER=' "$workflow" && ! grep -q 'SCCACHE_GHA_ENABLED=' "$workflow"; then
    error "$workflow clears RUSTC_WRAPPER but not SCCACHE_GHA_ENABLED"
  fi
done < <(workflow_files)

validate_required_context_uniqueness

if [ -f "$trusted_workflow" ]; then
  if grep -Eq '^[[:space:]]+pull_request(_target)?:' "$trusted_workflow"; then
    error "$trusted_workflow must not have a pull_request or pull_request_target trigger"
  fi
  grep -Eq '^[[:space:]]+push:' "$trusted_workflow" \
    || error "$trusted_workflow must have a trusted push trigger"
  grep -Eq '^[[:space:]]+workflow_dispatch:' "$trusted_workflow" \
    || error "$trusted_workflow must have a trusted workflow_dispatch trigger"
  grep -Eq '^[[:space:]]+merge_group:' "$trusted_workflow" \
    || error "$trusted_workflow must have a merge_group trigger"
  grep -q 'MACOS_RUNNER_GROUP' "$trusted_workflow" \
    || error "$trusted_workflow must require MACOS_RUNNER_GROUP for self-hosted routing"
fi

# Superseded PR heads must release hosted runners immediately. Required
# contexts remain fail-closed on the newest exact SHA; branch protection never
# consumes the cancelled stale SHA's results.
if [ -f "$pr_workflow" ]; then
  validate_pr_debug_envs
fi

exit "$fail"
