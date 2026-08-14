#!/usr/bin/env bash
set -euo pipefail

fail=0

error() {
  echo "ERROR: $*" >&2
  fail=1
}

readonly REQUIRED_CHECK_MIRROR_SHA256="57c78a2ea1d5587ff1c74d5d25e2e32d25814198c5ee966e2297845c6230a30d"

verify_required_check_mirror_hash() {
  local helper="scripts/required-check-mirror.sh"
  local actual
  if [ ! -f "$helper" ]; then
    error "missing $helper"
    return
  fi
  if ! actual="$(ruby -rdigest -e 'print Digest::SHA256.file(ARGV.fetch(0)).hexdigest' "$helper")"; then
    error "cannot hash $helper"
    return
  fi
  if [ "$actual" != "$REQUIRED_CHECK_MIRROR_SHA256" ]; then
    error "$helper content hash mismatch: expected $REQUIRED_CHECK_MIRROR_SHA256, found $actual; review the helper and update all three #5321 pins together"
  fi
}

validate_pr_debug_envs() {
  if ! command -v ruby >/dev/null 2>&1; then
    error "ruby is required to validate $pr_workflow structurally"
    return
  fi

  # Parse the workflow as YAML instead of slicing it as text. That keeps
  # quoted job IDs, flow mappings, escaped keys, and sibling job mappings from
  # satisfying a different job's requirement. The execution contract below
  # resolves the shell/env precedence chain for each protected Script checks
  # step before comparing the complete calculated surface.
  if ! ruby - "$pr_workflow" "$REQUIRED_CHECK_MIRROR_SHA256" <<'RUBY'
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

def normalize_required_check_pin(value)
  case value
  when Hash
    value.transform_values { |item| normalize_required_check_pin(item) }
  when Array
    value.map { |item| normalize_required_check_pin(item) }
  when String
    value.gsub(/expected=[0-9a-f]{64}/, "expected=<required-check-pin-sha256>")
  else
    value
  end
end

# Preserve scalar lexemes exactly as GitHub's YAML 1.2-facing workflow surface
# sees them. Psych's YAML 1.1 resolver turns `yes` into true and `012` into 10;
# comparing those resolved Ruby values would accept a different Actions value.
def raw_yaml_node(node)
  case node
  when Psych::Nodes::Mapping
    node.children.each_slice(2).each_with_object({}) do |(key, value), mapped|
      unless key.is_a?(Psych::Nodes::Scalar)
        raise "mapping keys must be scalar"
      end
      mapped[key.value] = raw_yaml_node(value)
    end
  when Psych::Nodes::Sequence
    node.children.map { |item| raw_yaml_node(item) }
  when Psych::Nodes::Scalar
    node.value
  else
    raise "unsupported YAML node: #{node.class}"
  end
end

# Pin a top-level job's exact source bytes so scalar tags and styles remain part
# of the contract instead of disappearing during Psych value resolution.
def raw_job_source(path, key_node)
  lines = File.binread(path).lines
  start_line = key_node.start_line
  end_line = ((start_line + 1)...lines.length).find do |index|
    lines[index].match?(/\A  (?:[A-Za-z0-9_-]+|["'][^"']+["']):[ \t]*(?:#.*)?(?:\r?\n)?\z/)
  end || lines.length
  selected = lines[start_line...end_line]
  selected.pop while selected.last&.match?(/\A(?:[ \t]*|  #.*)(?:\r?\n)?\z/)
  selected.join
end

def string_map(value)
  return {} unless value.is_a?(Hash)

  value.each_with_object({}) do |(key, item), mapped|
    mapped[key.to_s] = item
  end
end

def nested_value(value, *keys)
  keys.reduce(value) do |current, key|
    current.is_a?(Hash) ? current[key] : nil
  end
end

def default_shell_for(runs_on)
  runs_on.to_s.match?(/windows/i) ? "pwsh" : "bash"
end

def shell_candidates(document, job, step)
  {
    "step" => step.key?("shell") ? step["shell"] : nil,
    "job_defaults" => nested_value(job, "defaults", "run", "shell"),
    "workflow_defaults" => nested_value(document, "defaults", "run", "shell"),
    "runner_default" => default_shell_for(job["runs-on"]),
  }
end

def working_directory_candidates(document, job, step)
  {
    "step" => step.key?("working-directory") ? step["working-directory"] : nil,
    "job_defaults" => nested_value(job, "defaults", "run", "working-directory"),
    "workflow_defaults" => nested_value(document, "defaults", "run", "working-directory"),
  }
end

def effective_shell(candidates)
  candidates.fetch("step") || candidates.fetch("job_defaults") ||
    candidates.fetch("workflow_defaults") || candidates.fetch("runner_default")
end

def effective_working_directory(candidates)
  candidates.fetch("step") || candidates.fetch("job_defaults") ||
    candidates.fetch("workflow_defaults")
end

def protected_step_inventory(steps)
  protected_names = [
    "Protect writer gate aggregate wiring (#5308)",
    "Run script checks",
  ]
  protected_indices = protected_names.map do |name|
    steps.each_index.find { |index| steps[index].is_a?(Hash) && steps[index]["name"] == name }
  end
  between = if protected_indices.length == 2 && protected_indices.all?
    first, second = protected_indices
    first < second ? steps[(first + 1)...second].map { |step| canonical_yaml(step) } : nil
  end
  {
    "protected_indices" => protected_indices,
    "steps_between_protected" => between || ["<invalid protected-step order>"],
  }
end

def quoted_outputs(run)
  return [] unless run.is_a?(String)

  run.scan(/["']([^"']*)["']/).flatten
end

def runtime_writes(run, marker)
  return [] unless run.is_a?(String)

  escaped_marker = Regexp.escape(marker)
  redirect = /(?:>>|>)\s*["']?\$(?:\{#{escaped_marker}\}|#{escaped_marker})["']?(?:\s*(?:#.*)?)?\z/
  write_lines = run.lines.select { |line| line.strip.match?(redirect) }
  return [] if write_lines.empty?

  outputs = quoted_outputs(write_lines.join)
  if marker == "GITHUB_ENV"
    outputs = outputs.select { |output| output.match?(/\A[A-Za-z_][A-Za-z0-9_]*=/) }
  else
    outputs = outputs.reject { |output| output.include?("GITHUB_PATH") }
  end
  outputs = ["<unparsed write>"] if outputs.empty?
  outputs.map do |output|
    if marker == "GITHUB_ENV" && output.match?(/\A[A-Za-z_][A-Za-z0-9_]*=/)
      key, value = output.split("=", 2)
      {"key" => key, "value" => value}
    elsif marker == "GITHUB_PATH"
      {"path" => output}
    else
      {"unparsed" => output}
    end
  end
end

def effective_execution(document, job_id, step_index)
  jobs = document.fetch("jobs")
  job = jobs.fetch(job_id)
  steps = Array(job["steps"])
  step = steps.fetch(step_index)
  workflow_env = string_map(document["env"])
  job_env = string_map(job["env"])
  env = workflow_env.merge(job_env)
  env_writes = []
  path_writes = []

  steps[0...step_index].each_with_index do |prior_step, prior_index|
    next unless prior_step.is_a?(Hash)

    runtime_writes(prior_step["run"], "GITHUB_ENV").each do |write|
      env_writes << {"step" => prior_index, "write" => write}
      if write["key"]
        env[write["key"]] = write["value"]
      end
    end
    runtime_writes(prior_step["run"], "GITHUB_PATH").each do |write|
      path_writes << {"step" => prior_index, "write" => write}
    end
  end
  unless path_writes.empty?
    env["PATH"] = path_writes.map { |event| event.dig("write", "path") || "<unparsed>" }.join(":")
  end
  step_env = string_map(step["env"])
  effective_env = env.merge(step_env)
  candidates = shell_candidates(document, job, step)
  working_directory = working_directory_candidates(document, job, step)
  {
    "runs-on" => job["runs-on"],
    "protected_step_inventory" => protected_step_inventory(steps),
    "shell_candidates" => candidates,
    "effective_shell" => effective_shell(candidates),
    "working_directory_candidates" => working_directory,
    "effective_working_directory" => effective_working_directory(working_directory),
    "workflow_env" => workflow_env,
    "job_env" => job_env,
    "step_env" => step_env,
    "runtime_env_writes" => env_writes,
    "runtime_path_writes" => path_writes,
    "effective_env" => effective_env,
  }
end

def execution_contract(snapshot, expected)
  canonical_yaml(snapshot) == canonical_yaml(expected)
end

path = ARGV.fetch(0)
helper_sha256 = ARGV.fetch(1)
gate_sha256 = Digest::SHA256.file("scripts/check-ci-runner-hardening.sh").hexdigest
begin
  document = YAML.load_file(path)
  yaml_root = Psych.parse_file(path).root
  raw_document = raw_yaml_node(yaml_root)
rescue StandardError => error
  warn "#{path}: cannot parse YAML: #{error.message}"
  exit 1
end

jobs = document.is_a?(Hash) ? document["jobs"] : nil
raw_jobs = raw_document.is_a?(Hash) ? raw_document["jobs"] : nil
jobs_node = if yaml_root.is_a?(Psych::Nodes::Mapping)
  yaml_root.children.each_slice(2).find do |key_node, _value_node|
    key_node.is_a?(Psych::Nodes::Scalar) && key_node.value == "jobs"
  end&.last
end
unless jobs.is_a?(Hash)
  warn "#{path}: jobs must be a YAML mapping"
  exit 1
end

job_ids = if jobs_node.is_a?(Psych::Nodes::Mapping)
  jobs_node.children.each_slice(2).each_with_object([]) do |(key_node, _value_node), ids|
    ids << key_node.value if key_node.is_a?(Psych::Nodes::Scalar)
  end
else
  []
end
job_id_counts = job_ids.each_with_object(Hash.new(0)) { |job_id, counts| counts[job_id] += 1 }
duplicate_job_ids = job_id_counts.select { |_job_id, count| count > 1 }.keys
unless duplicate_job_ids.empty?
  warn "#{path}: duplicate job IDs are forbidden: #{duplicate_job_ids.sort.join(', ')}"
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

# The Script checks execution job is intentionally high-churn: concurrent lanes
# regularly add gates to its step inventory. Protect only the job and aggregate
# step fields that can silently disable execution, rather than whole-job hashing
# that would force unrelated hash re-pins for every new check. Its required
# branch-protection context is published by the separate result mirror below.
script_checks_job = jobs["scripts"]
unless script_checks_job.is_a?(Hash)
  warn "#{path}: Script checks runner job (scripts) must be a YAML mapping"
  exit 1
end
if script_checks_job.key?("if")
  warn "#{path}: Script checks runner job must not define a job-level if condition"
  exit 1
end
if script_checks_job["continue-on-error"]
  warn "#{path}: Script checks runner job must not be allowed to continue on error"
  exit 1
end
script_checks_needs = script_checks_job["needs"]
unless script_checks_needs == "changes" || script_checks_needs == ["changes"]
  warn "#{path}: Script checks runner job must retain exact needs: changes"
  exit 1
end

changes_job = jobs["changes"]
unless changes_job.is_a?(Hash)
  warn "#{path}: changes job must exist for the Script checks result mirror"
  exit 1
end

# Keep both branch-protection publishers and their finite needs closure on the
# exact fail-closed scheduling policy for their roles. The needs-bearing result
# publisher must use `if: always()` so upstream failure still reaches its
# mirror; the independent publisher and internal execution jobs must not gain
# job-level conditions. Every closure job forbids `continue-on-error`, including
# an explicit false value, because the key is an unreviewed failure-masking
# channel. Keep this closed set exact as the needs graph evolves.
expected_unconditional_closure = %w[
  changes
  relay-authority-contract
  scripts
  scripts_required_context
].sort
unconditional_roots = %w[scripts_required_context relay-authority-contract]
unconditional_closure = []
frontier = unconditional_roots.dup
until frontier.empty?
  job_id = frontier.shift
  next if unconditional_closure.include?(job_id)

  job = jobs[job_id]
  unless job.is_a?(Hash)
    warn "#{path}: required unconditional needs closure references missing job #{job_id.inspect}"
    exit 1
  end
  case job_id
  when "scripts_required_context"
    unless job.key?("if") && job["if"] == "always()"
      warn "#{path}: Script checks publisher must carry `if: always()` so upstream failure still runs the fail-closed mirror"
      exit 1
    end
  when "relay-authority-contract"
    if job.key?("if")
      warn "#{path}: relay-authority-contract must not define an if key so the independent publisher always runs"
      exit 1
    end
  else
    if job.key?("if")
      warn "#{path}: required closure execution job #{job_id} must not define an if key"
      exit 1
    end
  end
  if job.key?("continue-on-error")
    warn "#{path}: required unconditional job #{job_id} must not define a continue-on-error key"
    exit 1
  end

  unconditional_closure << job_id
  needs = job["needs"]
  case needs
  when nil
    nil
  when String
    frontier << needs
  when Array
    unless needs.all? { |dependency| dependency.is_a?(String) }
      warn "#{path}: required unconditional job #{job_id} has non-string needs"
      exit 1
    end
    frontier.concat(needs)
  else
    warn "#{path}: required unconditional job #{job_id} has unsupported needs shape"
    exit 1
  end
end
unless unconditional_closure.sort == expected_unconditional_closure
  warn "#{path}: required unconditional needs closure changed; expected #{expected_unconditional_closure.inspect}, found #{unconditional_closure.sort.inspect}"
  exit 1
end

# The required Script checks context is an unconditional result mirror. It
# reads both upstream results and delegates the fail-closed skipped/failure
# policy to required-check-mirror.sh. The mirror is a single fixed node, so its
# complete job and step surface is pinned below; no alternate execution surface
# is permitted to hide behind the result policy.
script_checks_context_job = jobs["scripts_required_context"]
unless script_checks_context_job.is_a?(Hash)
  warn "#{path}: Script checks required-context mirror job must be a YAML mapping"
  exit 1
end
expected_mirror_step = {
  "name" => "Mirror script checks result for branch protection",
  "env" => {
    "BASH_ENV" => "/dev/null",
    "PYTHON" => "python3",
    "CHANGED_PATHS_RESULT" => "${{ needs.changes.result }}",
    "FILTER_NAME" => "scripts",
    "FILTER_OUTPUT" => "true",
    "UPSTREAM_JOB_NAME" => "scripts",
    "UPSTREAM_RESULT" => "${{ needs.scripts.result }}",
  },
  "run" => "./scripts/required-check-mirror.sh",
}
expected_mirror_contract_step = {
  "name" => "Verify Script checks mirror contract (#5321)",
  "env" => {"BASH_ENV" => "/dev/null"},
  "shell" => "bash",
  "timeout-minutes" => 10,
  "run" => [
    "helper_path=scripts/required-check-mirror.sh",
    "expected=#{helper_sha256}",
    'actual="$(sha256sum "$helper_path" | cut -d \' \' -f 1)"',
    'if [ "$actual" != "$expected" ]; then',
    '  echo "::error file=$helper_path::content hash mismatch: expected $expected, found $actual; review the helper and update all three #5321 pins together"',
    "  exit 1",
    "fi",
    "gate_path=scripts/check-ci-runner-hardening.sh",
    "expected=#{gate_sha256}",
    'actual="$(sha256sum "$gate_path" | cut -d \' \' -f 1)"',
    'if [ "$actual" != "$expected" ]; then',
    '  echo "::error file=$gate_path::content hash mismatch: expected $expected, found $actual; review the gate and update both #5321 gate pins together"',
    "  exit 1",
    "fi",
    "scripts/check-ci-runner-hardening.sh",
    "python3 scripts/check_writer_gate_ci_wiring.py",
  ].join("\n") + "\n",
}
expected_mirror_steps = [
  {"uses" => "actions/checkout@v4"},
  expected_mirror_contract_step,
  expected_mirror_step,
]
expected_mirror_job = {
  "name" => "Script checks",
  "needs" => ["changes", "scripts"],
  "if" => "always()",
  "runs-on" => "ubuntu-latest",
  "steps" => expected_mirror_steps,
}
unless expected_mirror_job.reject { |key, _| key == "steps" }.all? do |key, value|
  script_checks_context_job[key] == value
end
  warn "#{path}: Script checks required-context mirror must retain its exact job wiring"
  exit 1
end
if script_checks_context_job["continue-on-error"]
  warn "#{path}: Script checks required-context mirror must not continue on error"
  exit 1
end
mirror_steps = Array(script_checks_context_job["steps"]).select do |step|
  step.is_a?(Hash) && step["name"] == "Mirror script checks result for branch protection"
end
unless mirror_steps.length == 1
  warn "#{path}: Script checks required-context mirror must retain exactly one result-mirror step"
  exit 1
end
mirror_step = mirror_steps.fetch(0)
normalized_mirror_step = canonical_yaml(mirror_step)
if normalized_mirror_step.dig("env", "FILTER_OUTPUT")
  normalized_mirror_step["env"]["FILTER_OUTPUT"] =
    normalized_mirror_step["env"]["FILTER_OUTPUT"].to_s
end
unless normalized_mirror_step == expected_mirror_step
  warn "#{path}: Script checks result-mirror step must retain the exact fail-closed wiring"
  exit 1
end
raw_mirror_job = raw_jobs.is_a?(Hash) ? raw_jobs["scripts_required_context"] : nil
expected_raw_mirror_job = canonical_yaml(expected_mirror_job)
expected_raw_mirror_job["steps"][1]["timeout-minutes"] = "10"
unless raw_mirror_job == expected_raw_mirror_job
  warn "#{path}: Script checks required-context mirror must retain the exact fixed job surface (raw YAML scalars; defaults/env/environment/strategy/container and three-step checkout/contract/mirror inventory)"
  exit 1
end
mirror_key_node = if jobs_node.is_a?(Psych::Nodes::Mapping)
  jobs_node.children.each_slice(2).find do |key_node, _value_node|
    key_node.is_a?(Psych::Nodes::Scalar) && key_node.value == "scripts_required_context"
  end&.first
end
mirror_source = mirror_key_node && raw_job_source(path, mirror_key_node)
mirror_source = mirror_source&.gsub(
  /expected=[0-9a-f]{64}/,
  "expected=<required-check-pin-sha256>",
)
mirror_source_sha256 = mirror_source && Digest::SHA256.hexdigest(mirror_source)
unless mirror_source_sha256 == "b271d53eb9eca9f1b3c9c9e257979655fa723fb9f6883840e74c7a86acf07fe0"
  warn "#{path}: Script checks required-context source bytes changed (scalar tags/styles and exact three-step surface are pinned); found #{mirror_source_sha256 || '<missing>'}"
  exit 1
end
script_check_steps = Array(script_checks_job["steps"]).select do |step|
  step.is_a?(Hash) && step["name"] == "Run script checks"
end
unless script_check_steps.length == 1
  warn "#{path}: Script checks runner job must retain exactly one \"Run script checks\" step"
  exit 1
end
script_check_step = script_check_steps.fetch(0)
if script_check_step.key?("if")
  warn "#{path}: Script checks runner job \"Run script checks\" step must not define if"
  exit 1
end
if script_check_step["continue-on-error"]
  warn "#{path}: Script checks runner job \"Run script checks\" step must not continue on error"
  exit 1
end
script_check_commands = if script_check_step["run"].is_a?(String)
  script_check_step["run"].lines.map(&:strip).reject(&:empty?)
else
  []
end
unless script_check_commands == ["./scripts/ci-script-checks.sh"]
  warn "#{path}: Script checks runner job \"Run script checks\" step must run exactly ./scripts/ci-script-checks.sh"
  exit 1
end

# #5308: the external step runs the writer-wiring checker, its unittest, and
# this hardening guard. The checker pins the aggregate's hardening and fast
# wiring-unittest invocations; the aggregate hardening invocation validates the
# external step shape, and the aggregate fast wiring unittest exercises that
# validation. Removing only the external step or only either aggregate observer
# therefore leaves another observer statically invoked. This static invocation
# chain ends when one diff removes the external step together with both
# aggregate observer invocations; branch-protection configuration is not part
# of this contract.
writer_wiring_steps = Array(script_checks_job["steps"]).select do |step|
  step.is_a?(Hash) && step["name"] == "Protect writer gate aggregate wiring (#5308)"
end
unless writer_wiring_steps.length == 1
  warn "#{path}: Script checks runner job must retain exactly one writer gate aggregate wiring step"
  exit 1
end
writer_wiring_step = writer_wiring_steps.fetch(0)
if writer_wiring_step.key?("if")
  warn "#{path}: writer gate aggregate wiring step must not define if"
  exit 1
end
if writer_wiring_step["continue-on-error"]
  warn "#{path}: writer gate aggregate wiring step must not continue on error"
  exit 1
end
writer_wiring_commands = if writer_wiring_step["run"].is_a?(String)
  writer_wiring_step["run"].lines.map(&:strip).reject(&:empty?)
else
  []
end
expected_writer_wiring_commands = [
  "python3 scripts/check_writer_gate_ci_wiring.py",
  "python3 -m unittest tests.test_writer_gate_ci_wiring",
  "scripts/check-ci-runner-hardening.sh",
]
unless writer_wiring_commands == expected_writer_wiring_commands
  warn "#{path}: writer gate aggregate wiring step must retain the exact external protection command list"
  exit 1
end

# This is one calculated contract, not one assertion per environment key. The
# expected workflow environment is copied from the parsed CI PR workflow so a
# mutation at any contributing scope changes the observed execution surface.
expected_workflow_env = {
  "CARGO_TERM_COLOR" => "always",
  "RUSTC_WRAPPER" => "sccache",
  "SCCACHE_CACHE_SIZE" => "10G",
  "SCCACHE_GHA_ENABLED" => "true",
  "SCCACHE_GHA_RW_MODE" => "${{ github.event_name == 'pull_request' && 'READ_ONLY' || 'READ_WRITE' }}",
  "POSTGRES_SERVICE_IMAGE" => "${{ vars.AGENTDESK_POSTGRES_SERVICE_IMAGE }}",
}
script_check_step_index = Array(script_checks_job["steps"]).index(script_check_step)
script_check_execution = effective_execution(
  document,
  "scripts",
  script_check_step_index,
)
expected_script_check_execution = {
  "runs-on" => "ubuntu-latest",
  "protected_step_inventory" => {
    "protected_indices" => [8, 9],
    "steps_between_protected" => [],
  },
  "shell_candidates" => {
    "step" => "bash",
    "job_defaults" => nil,
    "workflow_defaults" => nil,
    "runner_default" => "bash",
  },
  "effective_shell" => "bash",
  "working_directory_candidates" => {
    "step" => nil,
    "job_defaults" => nil,
    "workflow_defaults" => nil,
  },
  "effective_working_directory" => nil,
  "workflow_env" => expected_workflow_env,
  "job_env" => {},
  "step_env" => {
    "BASH_ENV" => "/dev/null",
    "PYTHON" => "python3",
    "TEST_LANE_BASELINE_REF" => "HEAD^1",
  },
  "runtime_env_writes" => [],
  "runtime_path_writes" => [],
  "effective_env" => expected_workflow_env.merge(
    "BASH_ENV" => "/dev/null",
    "PYTHON" => "python3",
    "TEST_LANE_BASELINE_REF" => "HEAD^1",
  ),
}
unless script_check_execution["protected_step_inventory"] == expected_script_check_execution["protected_step_inventory"]
  warn "#{path}: Script checks protected step inventory changed; expected indices [8, 9] with no interstitial steps, found #{JSON.generate(script_check_execution["protected_step_inventory"])}"
  exit 1
end
unless execution_contract(script_check_execution, expected_script_check_execution)
  expected = JSON.generate(canonical_yaml(expected_script_check_execution))
  found = JSON.generate(canonical_yaml(script_check_execution))
  warn "#{path}: Script checks aggregate effective execution changed; expected #{expected}; found #{found}"
  exit 1
end

writer_wiring_step_index = Array(script_checks_job["steps"]).index(writer_wiring_step)
writer_wiring_execution = effective_execution(
  document,
  "scripts",
  writer_wiring_step_index,
)
expected_writer_wiring_execution = {
  "runs-on" => "ubuntu-latest",
  "protected_step_inventory" => {
    "protected_indices" => [8, 9],
    "steps_between_protected" => [],
  },
  "shell_candidates" => {
    "step" => "bash",
    "job_defaults" => nil,
    "workflow_defaults" => nil,
    "runner_default" => "bash",
  },
  "effective_shell" => "bash",
  "working_directory_candidates" => {
    "step" => nil,
    "job_defaults" => nil,
    "workflow_defaults" => nil,
  },
  "effective_working_directory" => nil,
  "workflow_env" => expected_workflow_env,
  "job_env" => {},
  "step_env" => {},
  "runtime_env_writes" => [],
  "runtime_path_writes" => [],
  "effective_env" => expected_workflow_env,
}
unless writer_wiring_execution["protected_step_inventory"] == expected_writer_wiring_execution["protected_step_inventory"]
  warn "#{path}: Script checks protected step inventory changed; expected indices [8, 9] with no interstitial steps, found #{JSON.generate(writer_wiring_execution["protected_step_inventory"])}"
  exit 1
end
unless execution_contract(writer_wiring_execution, expected_writer_wiring_execution)
  expected = JSON.generate(canonical_yaml(expected_writer_wiring_execution))
  found = JSON.generate(canonical_yaml(writer_wiring_execution))
  warn "#{path}: writer gate aggregate wiring effective execution changed; expected #{expected}; found #{found}"
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
    # #5230 re-pins after replacing repeated PostgreSQL skip literals with the
    # shared non-pg-test-filter source; job names, conditions, and timeouts are
    # unchanged, and the exact commands below pin each source/use pair.
    "job_sha256" => "1e10a6a98f3e9a9b1f89001ccc260f6759a36238bb4c36dda0d08f10fe17e406",
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
          "source scripts/ci/non-pg-test-filter.sh",
          'cargo test --lib task_notification -- "${NON_PG_SKIP_ARGS[@]}"',
          'cargo test --lib services::discord::tmux::tmux_watcher::discrete_trigger_marker::tests -- "${NON_PG_SKIP_ARGS[@]}"',
        ],
        "timeout_minutes" => 10,
      },
      "Trusted session forwarding tests" => {
        "commands" => [
          "source scripts/ci/non-pg-test-filter.sh",
          'env -u AGENTDESK_ROOT_DIR cargo test --lib services::session_forwarding -- "${NON_PG_SKIP_ARGS[@]}"',
        ],
        "timeout_minutes" => 10,
      },
      "Telemetry-only intake authority regressions" => {
        "commands" => [
          "source scripts/ci/non-pg-test-filter.sh",
          'env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::router::intake_dispatch::tests::telemetry_only_unopted -- "${NON_PG_SKIP_ARGS[@]}"',
        ],
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
  # #5185: the PR-side whole-library sweep. Registering it here pins its step
  # inventory, so removing the adjudicator and leaving a bare `cargo test --lib`
  # -- which exits 0 on a zero-match filter -- is a diff that fails this script
  # rather than one that quietly restores the false green the job exists to
  # close. Read the two-layer caveat above before treating the hash as a
  # guarantee: it detects change, it does not prevent it.
  "library_sweep" => {
    "label" => "PR library sweep job",
    "name" => "Library test sweep",
    "needs" => "changes",
    "if" => "needs.changes.outputs.rust_or_policy == 'true'",
    "runs_on" => "ubuntu-latest",
    # #5185 re-pins after giving this lane the PostgreSQL service its own
    # selection requires: the canonical filters are substring matches over
    # ids, and 55 PG-dependent tests carry none of those substrings, so the
    # job selected a database it never provisioned.
    # The re-pin is a review trigger only; the property is enforced without a
    # hash by `[rule5]` in scripts/check_pg_test_lane_membership.py.
    # #5230 re-pins after sourcing the shared filter and replaying its 15
    # source-verified non-PG false positives after the adjudicated sweep.
    "job_sha256" => "1e8147f0eb1a23e3b49336953e8c0cd5d1214e94dd3d517444d4e35f1ef98ed8",
    "cargo_steps" => {
      "Library sweep (selection-set gated)" => {
        "commands" => [
          "source scripts/ci/non-pg-test-filter.sh",
          'python3 scripts/run_test_lane.py --lane non-pg-sweep --max-summaries 2 "${NON_PG_SKIP_ARGS[@]}" -- env -u AGENTDESK_ROOT_DIR cargo test --lib -- "${NON_PG_SKIP_ARGS[@]}"',
          "run_non_pg_filter_false_positives",
        ],
        "timeout_minutes" => 45,
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
    # #5321 re-pins after making the independent backstop verify both the
    # result helper and the gate before executing that verified gate.
    "job_sha256" => "4038e039649d5f7736e9bdeb03f047072db1f0db92243d42f0e8581f0703bb42",
    "job_timeout_minutes" => 30,
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
      "Pin required-check mirror content (#5321)" => {
        "commands" => [
          "helper_path=scripts/required-check-mirror.sh",
          "expected=#{helper_sha256}",
          'actual="$(sha256sum "$helper_path" | cut -d \' \' -f 1)"',
          'if [ "$actual" != "$expected" ]; then',
          'echo "::error file=$helper_path::content hash mismatch: expected $expected, found $actual; review the helper and update all three #5321 pins together"',
          "exit 1",
          "fi",
          "gate_path=scripts/check-ci-runner-hardening.sh",
          "expected=#{gate_sha256}",
          'actual="$(sha256sum "$gate_path" | cut -d \' \' -f 1)"',
          'if [ "$actual" != "$expected" ]; then',
          'echo "::error file=$gate_path::content hash mismatch: expected $expected, found $actual; review the gate and update both #5321 gate pins together"',
          "exit 1",
          "fi",
          "scripts/check-ci-runner-hardening.sh",
        ],
        "timeout_minutes" => 10,
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
    # #5170 re-pins after wiring the transition-busy requeue oracles, which the
    # lib inventory manifest counted while no curated filter executed them.
    # #5181 re-pins after widening that lane from the two named #5170 oracles to
    # the whole `services::discord::queue_io::` module, now that the module's
    # pre-existing #4270/#4893 failures are fixed rather than filtered around.
    # #5147 re-pins after adding the hang-forensics and health-diagnostics
    # test steps to this lane. Steps were only added -- none removed,
    # reordered or given a relaxed env -- and the value is recomputed from
    # the workflow with this script's own canonical_yaml, never copied.
    "job_sha256" => "131ff4835b5b0811ceeb28a2a1b11efbf0d9f1dc6bf7ad87ab62da8d1dcd02bf",
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
  raw_job = raw_jobs.is_a?(Hash) ? raw_jobs[job_id] : nil
  unless job.is_a?(Hash)
    errors << "#{label} (#{job_id}) must be a YAML mapping"
    next
  end

  canonical_job = canonical_yaml(job)
  canonical_job = normalize_required_check_pin(canonical_job) if job_id == "relay-authority-contract"
  job_sha256 = Digest::SHA256.hexdigest(JSON.generate(canonical_job))
  unless job_sha256 == spec.fetch("job_sha256")
    errors << "#{label} semantic structure or command inventory changed; found #{job_sha256}"
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
  if spec.key?("job_timeout_minutes") &&
      (!raw_job.is_a?(Hash) || raw_job["timeout-minutes"] != spec["job_timeout_minutes"].to_s)
    errors << "#{label} must retain exact raw timeout-minutes"
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
  raw_steps = raw_job.is_a?(Hash) ? Array(raw_job["steps"]) : []
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
      raw_step = raw_steps.find { |candidate| candidate.is_a?(Hash) && candidate["name"] == name }
      expected_raw_timeout = step_spec.fetch("timeout_minutes")&.to_s
      unless raw_step.is_a?(Hash) && raw_step["timeout-minutes"] == expected_raw_timeout
        errors << "#{label} #{name.inspect} must retain exact raw timeout policy"
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
non_string_job_ids = jobs.keys.reject { |job_id| job_id.is_a?(String) }
unless non_string_job_ids.empty?
  rendered_ids = non_string_job_ids.map(&:inspect).join(", ")
  warn "#{path}: job IDs must be strings; non-string YAML job keys: #{rendered_ids}"
  exit 1
end
ambiguous_plain_job_ids = []
duplicate_job_ids = []
yaml_root = Psych.parse(File.read(path)).root
if yaml_root.is_a?(Psych::Nodes::Mapping)
  jobs_node = yaml_root.children.each_slice(2).find do |key_node, _value_node|
    key_node.is_a?(Psych::Nodes::Scalar) && key_node.value == "jobs"
  end&.last
  if jobs_node.is_a?(Psych::Nodes::Mapping)
    raw_job_ids = jobs_node.children.each_slice(2).each_with_object([]) do |(key_node, _value_node), ids|
      ids << key_node.value if key_node.is_a?(Psych::Nodes::Scalar)
    end
    job_id_counts = raw_job_ids.each_with_object(Hash.new(0)) { |job_id, counts| counts[job_id] += 1 }
    duplicate_job_ids = job_id_counts.select { |_job_id, count| count > 1 }.keys
    jobs_node.children.each_slice(2) do |key_node, _value_node|
      next unless key_node.is_a?(Psych::Nodes::Scalar) && key_node.respond_to?(:plain)
      next unless key_node.plain &&
        %w[yes no on off true false y n].include?(key_node.value.downcase)

      ambiguous_plain_job_ids << key_node.value
    end
  end
end
unless duplicate_job_ids.empty?
  warn "#{path}: duplicate job IDs are forbidden: #{duplicate_job_ids.sort.join(', ')}"
  exit 1
end
unless ambiguous_plain_job_ids.empty?
  rendered_ids = ambiguous_plain_job_ids.map(&:inspect).join(", ")
  warn "#{path}: ambiguous YAML plain job keys must be quoted or renamed: #{rendered_ids}"
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
  mirror = jobs["scripts_required_context"]
  unless mirror.is_a?(Hash) && mirror["name"] == required_context
    warn "#{path}: required Script checks context must be the exact literal name of jobs.scripts_required_context"
    exit 1
  end
  unless scripts.is_a?(Hash) && scripts["name"] != required_context
    warn "#{path}: jobs.scripts must not publish the required Script checks context"
    exit 1
  end
  unexpected_required = required_context_jobs - ["scripts_required_context"]
  if unexpected_required.any?
    warn "#{path}: required Script checks context must belong only to jobs.scripts_required_context"
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

verify_required_check_mirror_hash

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
