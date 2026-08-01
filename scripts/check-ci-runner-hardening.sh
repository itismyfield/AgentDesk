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
      canonical[key.to_s] = canonical_yaml(value[key])
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

targets = {
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
        "continue_on_error" => nil,
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
    "job_sha256" => "45379287c8cd87d4dae3a9733b92cb65984dc042972e0e12046125f18664e3a9",
    "cargo_steps" => {
      "Footer-only marker regressions" => {
        "commands" => [
          "cargo test --lib task_notification -- --skip _pg --skip pg_ --skip postgres",
          "cargo test --lib services::discord::tmux::tmux_watcher::discrete_trigger_marker::tests -- --skip _pg --skip pg_ --skip postgres",
        ],
        "continue_on_error" => nil,
        "timeout_minutes" => 10,
      },
      "Trusted session forwarding tests" => {
        "commands" => ["env -u AGENTDESK_ROOT_DIR cargo test --lib services::session_forwarding -- --skip _pg --skip pg_ --skip postgres"],
        "continue_on_error" => nil,
        "timeout_minutes" => 10,
      },
      "Telemetry-only intake authority regressions" => {
        "commands" => ["env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::router::intake_dispatch::tests::telemetry_only_unopted -- --skip _pg --skip pg_ --skip postgres"],
        "continue_on_error" => nil,
        "timeout_minutes" => 10,
      },
      "Terminal delivery evidence regressions" => {
        "commands" => [
          "env -u AGENTDESK_ROOT_DIR cargo test --lib inflight::terminal_delivery_evidence_loss::tests",
          "env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::turn_bridge::terminal_outcome_delivery::delivery_epilogue_tests",
          "env -u AGENTDESK_ROOT_DIR cargo test --lib watcher_terminal_commit_identity_mismatch_skips_without_clobbering_newer_row",
          "env -u AGENTDESK_ROOT_DIR cargo test --lib identity_guarded_save_rejects_stale_write_against_newer_turn",
        ],
        "continue_on_error" => nil,
        "timeout_minutes" => 10,
      },
      "just test-postgres" => {
        "commands" => ["just test-postgres"],
        "continue_on_error" => nil,
        "timeout_minutes" => 20,
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
  errors << "#{label} must not be allowed to continue on error" unless job["continue-on-error"].nil?
  if job_id == "check_fast_cross_os"
    strategy = job["strategy"]
    unless strategy.is_a?(Hash)
      errors << "#{label} must retain its matrix strategy"
    else
      errors << "#{label} matrix must fail independently" unless strategy["fail-fast"] == false
      errors << "#{label} must retain the Windows matrix" unless strategy.dig("matrix", "os") == ["windows-latest"]
    end
  end

  env = job["env"]
  keys.each do |key|
    unless env.is_a?(Hash) && env[key] == "0"
      errors << "#{label} must set job-level #{key} to the string \"0\""
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
      errors << "#{label} #{name.inspect} must not be conditionally skipped" unless step["if"].nil?
      unless step["continue-on-error"] == step_spec.fetch("continue_on_error")
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
      if run.is_a?(String) && run.match?(/(^|[[:space:]])cargo[[:space:]]|(^|[[:space:]])just[[:space:]]+test-postgres/)
        errors << "#{label} step #{index + 1} adds an unprotected cargo/test boundary"
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
