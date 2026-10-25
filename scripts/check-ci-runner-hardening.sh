#!/usr/bin/env bash
set -euo pipefail

fail=0

error() {
  echo "ERROR: $*" >&2
  fail=1
}

python3 scripts/ci/nested_libtest.py check || error "nested libtest declarations must pass validation"
python3 scripts/ci/cargo_test_filters.py || error "main, nightly and trusted cargo test commands must not carry a name filter"

readonly REQUIRED_CONTEXTS_JSON='["Lint", "Script checks", "Fast check (ubuntu-latest)", "High-risk recovery", "Dashboard (Node 22)", "Fast targeted tests (ubuntu-latest)", "relay-authority-contract", "Library test sweep (ubuntu-latest)", "Fast check cross OS required context (ubuntu-latest)"]'
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
    error "$helper content hash mismatch: expected $REQUIRED_CHECK_MIRROR_SHA256, found $actual; review the helper and update this pin"
  fi
}

validate_pr_debug_envs() {
  if ! command -v ruby >/dev/null 2>&1; then
    error "ruby is required to validate $pr_workflow structurally"
    return
  fi

  if ! ruby - "$pr_workflow" "$REQUIRED_CONTEXTS_JSON" <<'RUBY'
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
required_contexts = JSON.parse(ARGV.fetch(1))
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

script_check_shard_jobs = {
  "scripts" => "cargo",
  "scripts_guards" => "guards",
  "scripts_contracts" => "contracts",
}
script_checks_job = jobs["scripts"]
unless script_checks_job.is_a?(Hash)
  warn "#{path}: Script checks runner job (scripts) must be a YAML mapping"
  exit 1
end
unless script_checks_job["if"] == "${{ !cancelled() }}"
  warn "#{path}: Script checks runner job must use !cancelled() for producer-failure fallback"
  exit 1
end
if script_checks_job["continue-on-error"]
  warn "#{path}: Script checks runner job must not be allowed to continue on error"
  exit 1
end
unless script_checks_job["needs"] == "changes"
  warn "#{path}: Script checks runner job must retain exact needs: changes"
  exit 1
end

# Every job the nine required contexts read, with the exact if that runs it; the judge
# accepts a skip only when the changes outputs say the job was not expected.
rust_tests_gate = "${{ !cancelled() && needs.changes.outputs.rust_tests == 'true' }}"
built_gate = "${{ !cancelled() && needs.rust_build.result == 'success' }}"
relay_gate = "${{ !cancelled() && needs.changes.outputs.rust_tests_skip != 'true' }}"
cross_os_gate = "needs.changes.outputs.rust_compile == 'true' && needs.changes.outputs.cross_os_rust == 'true'"
expected_closure_if = {
  "changes" => nil, "rust_build" => rust_tests_gate, "rust_tests" => built_gate, "test_evidence" => built_gate,
  "lint" => "needs.changes.outputs.rust_or_policy == 'true' || needs.changes.outputs.relay_contract == 'true'",
  "scripts" => "${{ !cancelled() }}", "scripts_guards" => nil, "scripts_contracts" => nil,
  "dashboard" => "needs.changes.outputs.dashboard == 'true'", "pg15_owner_proof" => rust_tests_gate,
  "relay_authority_mutations" => relay_gate,
  "check_fast_cross_os" => cross_os_gate, "check_fast_cross_os_targets" => cross_os_gate,
  "required_contexts" => "${{ !cancelled() }}",
}
closure = []
frontier = ["required_contexts"]
until frontier.empty?
  job_id = frontier.shift
  next if closure.include?(job_id)

  job = jobs[job_id]
  unless job.is_a?(Hash) && expected_closure_if.key?(job_id) && job["if"] == expected_closure_if[job_id] && !job.key?("continue-on-error")
    warn "#{path}: required needs closure job #{job_id.inspect} must exist with its exact if and no continue-on-error"
    exit 1
  end
  needs = job["needs"]
  unless needs.nil? || needs.is_a?(String) || (needs.is_a?(Array) && needs.all? { |dependency| dependency.is_a?(String) })
    warn "#{path}: required needs closure job #{job_id} has unsupported needs shape"
    exit 1
  end
  closure << job_id
  frontier.concat(Array(needs))
end
unless closure.sort == expected_closure_if.keys.sort
  warn "#{path}: required needs closure changed; expected #{expected_closure_if.keys.sort.inspect}, found #{closure.sort.inspect}"
  exit 1
end

judge_sha256 = Digest::SHA256.file("scripts/ci/nextest_required.py").hexdigest
expected_required_job = {
  "name" => "${{ matrix.context }}",
  "needs" => expected_closure_if.keys - ["required_contexts"],
  "if" => "${{ !cancelled() }}",
  "runs-on" => "ubuntu-latest",
  "timeout-minutes" => 10,
  "strategy" => {"fail-fast" => false, "matrix" => {"context" => required_contexts}},
  "steps" => [
    {"uses" => "actions/checkout@v4"},
    {"name" => "Verify pinned judge and CI structure", "if" => "${{ matrix.context == 'Script checks' || matrix.context == 'relay-authority-contract' }}",
     "env" => {"BASH_ENV" => "/dev/null"}, "shell" => "bash", "run" => <<~SH},
       pin() {
         actual="$(sha256sum "$1" | cut -d ' ' -f 1)"
         if [ "$actual" != "$2" ]; then
           echo "::error file=$1::content hash mismatch: expected $2, found $actual; review it and update every pin together"
           exit 1
         fi
       }
       pin scripts/ci/nextest_required.py #{judge_sha256}
       pin scripts/check-ci-runner-hardening.sh #{gate_sha256}
       scripts/check-ci-runner-hardening.sh
       python3 scripts/check_writer_gate_ci_wiring.py
     SH
    {"name" => "Judge required context", "env" => {"BASH_ENV" => "/dev/null", "NEEDS_JSON" => "${{ toJSON(needs) }}"},
     "run" => "python3 scripts/ci/nextest_ci.py required-context \"${{ matrix.context }}\""},
  ],
}
unless canonical_yaml(jobs["required_contexts"]) == canonical_yaml(expected_required_job)
  warn "#{path}: required_contexts must retain its exact judge, pins and needs; expected #{JSON.generate(canonical_yaml(expected_required_job))}"
  exit 1
end
required_key_node = jobs_node.children.each_slice(2).find { |key_node, _| key_node.is_a?(Psych::Nodes::Scalar) && key_node.value == "required_contexts" }&.first
required_source = required_key_node && raw_job_source(path, required_key_node).gsub(/\b[0-9a-f]{64}\b/, "<sha256>")
required_source_sha256 = required_source && Digest::SHA256.hexdigest(required_source)
unless required_source_sha256 == "385a40ff89e00c08686c8e204b6bb8d62667a1160b8db4f2b4b93412bc767a65"
  warn "#{path}: required_contexts source bytes changed (scalar tags/styles and exact step surface are pinned); found #{required_source_sha256 || '<missing>'}"
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
    "protected_indices" => [9, 10],
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
    "GFP_EVENT_NAME" => "${{ github.event_name }}",
    "GFP_REPOSITORY" => "${{ github.repository }}",
    "GFP_HEAD_REPOSITORY" => "${{ github.event.pull_request.head.repo.full_name }}",
    "GFP_CANDIDATE_SHA" => "${{ github.sha }}",
    "GFP_BASE_SHA" => "${{ github.event.pull_request.base.sha }}",
    "GFP_HEAD_SHA" => "${{ github.event.pull_request.head.sha }}",
    "TEST_LANE_BASELINE_REF" => "HEAD^1",
    "SCRIPT_CHECK_SHARD" => "cargo",
    "ADK_LIB_TEST_ARTIFACT" => "${{ runner.temp }}/lib-test-artifact",
    "CARGO_PROFILE_DEV_DEBUG" => "0",
    "CARGO_PROFILE_TEST_DEBUG" => "0",
  },
  "runtime_env_writes" => [],
  "runtime_path_writes" => [],
  "effective_env" => expected_workflow_env.merge(
    "BASH_ENV" => "/dev/null",
    "PYTHON" => "python3",
    "GFP_EVENT_NAME" => "${{ github.event_name }}",
    "GFP_REPOSITORY" => "${{ github.repository }}",
    "GFP_HEAD_REPOSITORY" => "${{ github.event.pull_request.head.repo.full_name }}",
    "GFP_CANDIDATE_SHA" => "${{ github.sha }}",
    "GFP_BASE_SHA" => "${{ github.event.pull_request.base.sha }}",
    "GFP_HEAD_SHA" => "${{ github.event.pull_request.head.sha }}",
    "TEST_LANE_BASELINE_REF" => "HEAD^1",
    "SCRIPT_CHECK_SHARD" => "cargo",
    "ADK_LIB_TEST_ARTIFACT" => "${{ runner.temp }}/lib-test-artifact",
    "CARGO_PROFILE_DEV_DEBUG" => "0",
    "CARGO_PROFILE_TEST_DEBUG" => "0",
  ),
}
unless script_check_execution["protected_step_inventory"] == expected_script_check_execution["protected_step_inventory"]
  warn "#{path}: Script checks protected step inventory changed; expected indices [9, 10] with no interstitial steps, found #{JSON.generate(script_check_execution["protected_step_inventory"])}"
  exit 1
end
unless execution_contract(script_check_execution, expected_script_check_execution)
  expected = JSON.generate(canonical_yaml(expected_script_check_execution))
  found = JSON.generate(canonical_yaml(script_check_execution))
  warn "#{path}: Script checks aggregate effective execution changed; expected #{expected}; found #{found}"
  exit 1
end
expected_cap = {"name" => "Production PR cap", "shell" => "bash", "run" => "bash scripts/pr_cap_check.sh", "env" => {"PR_CAP_CI" => "1", "PR_CAP_MODE" => "${{ vars.PR_CAP_MODE || 'enforce' }}", "BASH_ENV" => "/dev/null"}}
cap_steps = Array(script_checks_job["steps"]).select { |step| step.is_a?(Hash) && step["name"] == "Production PR cap" }
unless cap_steps == [expected_cap] && Array(script_checks_job["steps"]).index(expected_cap) == script_check_step_index + 1
  warn "#{path}: production PR cap must retain its unconditional exact post-aggregate execution"
  exit 1
end
evidence_steps = Array(script_checks_job["steps"]).select { |step| step.is_a?(Hash) && step["name"] == "Upload giant-file progress evidence" }
unless evidence_steps == [{"name" => "Upload giant-file progress evidence", "if" => "always()", "uses" => "actions/upload-artifact@v4", "with" => {"path" => "target/giant-file-progress/evidence.json"}}]
  warn "#{path}: giant-file progress evidence upload must remain exact and unconditional"
  exit 1
end

script_check_shard_jobs.each do |job_id, shard|
  next if job_id == "scripts"

  expected_shard_step_env = expected_script_check_execution["step_env"].reject { |key, _| %w[ADK_LIB_TEST_ARTIFACT CARGO_PROFILE_DEV_DEBUG CARGO_PROFILE_TEST_DEBUG].include?(key) }.merge("SCRIPT_CHECK_SHARD" => shard)
  expected_raw_shard_job = {
    "name" => "Script checks runner (#{shard})",
    "needs" => "changes",
    "runs-on" => "ubuntu-latest",
    "timeout-minutes" => "45",
    "steps" => [
      {"uses" => "actions/checkout@v4", "with" => {"fetch-depth" => "0"}},
      {"name" => "Setup Python for script checks", "uses" => "actions/setup-python@v5", "with" => {"python-version" => "3.11"}},
      {"name" => "Install script-test Python deps", "run" => "python3 -m pip install --disable-pip-version-check pyyaml"},
      {"name" => "Install shellcheck", "run" => "sudo apt-get install -y shellcheck zsh"},
      {"name" => "Run script checks", "shell" => "bash", "run" => "./scripts/ci-script-checks.sh", "env" => expected_shard_step_env},
    ],
  }
  unless raw_jobs.is_a?(Hash) && raw_jobs[job_id] == expected_raw_shard_job
    warn "#{path}: Script checks shard job #{job_id} must retain the exact fixed job surface (raw YAML scalars; checkout/setup/install/run inventory, action refs, timeout-minutes 45, no permissions/env/defaults/if/continue-on-error)"
    exit 1
  end
  shard_execution = effective_execution(document, job_id, expected_raw_shard_job["steps"].length - 1)
  shard_execution.delete("protected_step_inventory")
  expected_shard_execution = expected_script_check_execution.reject { |key, _| key == "protected_step_inventory" }
  expected_shard_execution = expected_shard_execution.merge(
    "step_env" => expected_shard_step_env,
    "effective_env" => expected_shard_execution["effective_env"].reject { |key, _| %w[ADK_LIB_TEST_ARTIFACT CARGO_PROFILE_DEV_DEBUG CARGO_PROFILE_TEST_DEBUG].include?(key) }.merge("SCRIPT_CHECK_SHARD" => shard),
  )
  unless canonical_yaml(shard_execution) == canonical_yaml(expected_shard_execution)
    expected = JSON.generate(canonical_yaml(expected_shard_execution))
    found = JSON.generate(canonical_yaml(shard_execution))
    warn "#{path}: Script checks shard #{job_id} effective execution changed; expected #{expected}; found #{found}"
    exit 1
  end
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
    "protected_indices" => [9, 10],
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
  warn "#{path}: Script checks protected step inventory changed; expected indices [9, 10] with no interstitial steps, found #{JSON.generate(writer_wiring_execution["protected_step_inventory"])}"
  exit 1
end
unless execution_contract(writer_wiring_execution, expected_writer_wiring_execution)
  expected = JSON.generate(canonical_yaml(expected_writer_wiring_execution))
  found = JSON.generate(canonical_yaml(writer_wiring_execution))
  warn "#{path}: writer gate aggregate wiring effective execution changed; expected #{expected}; found #{found}"
  exit 1
end

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

# Test results travel only as the plan artifact and per-attempt worker artifacts the audit reads.
expected_uploads = {
  "rust_build" => [{"name" => "Upload nextest plan", "uses" => "actions/upload-artifact@v4", "with" => {"name" => "nextest-plan", "path" => "${{ runner.temp }}/nextest", "if-no-files-found" => "error", "retention-days" => 1}}],
  "rust_tests" => [{"name" => "Upload worker results", "if" => "always()", "timeout-minutes" => 10, "uses" => "actions/upload-artifact@v4", "with" => {"name" => "nextest-worker-${{ matrix.worker }}-attempt-${{ github.run_attempt }}", "path" => "${{ runner.temp }}/nextest-result", "if-no-files-found" => "warn", "retention-days" => 7}}],
}
jobs.each do |job_id, job|
  uploads = Array(job["steps"]).select { |step| step.is_a?(Hash) && step["uses"].to_s.start_with?("actions/upload-artifact") }
  next if job_id == "scripts" || (uploads.empty? && !expected_uploads.key?(job_id))
  unless uploads == expected_uploads[job_id]
    warn "#{path}: artifact uploads must stay in approved jobs with their exact names"
    exit 1
  end
end

targets = {
  "rust_build" => {
    "label" => "nextest build and plan job", "name" => "Rust tests build and plan",
    "needs" => "changes", "if" => rust_tests_gate, "runs_on" => "ubuntu-latest",
    "job_timeout_minutes" => 100, "job_sha256" => "18f738247b0c02b9cac5cd225bcb4ce366c733f7378d1b6aaa1c4cc38706ba9a",
    "cargo_steps" => {
      "Check Rust test source wiring" => {"commands" => ["python3 scripts/ci/rust_test_sources.py check"], "timeout_minutes" => nil, "env" => {"BASH_ENV" => "/dev/null"}},
      "Archive, list, compare and plan" => {
        "commands" => [
          "set -euo pipefail", 'NEXTEST_DIR="$RUNNER_TEMP/nextest"', 'mkdir -p "$NEXTEST_DIR"',
          'cargo nextest archive --lib --archive-file "$NEXTEST_DIR/lib-tests.tar.zst"',
          'cargo nextest list --archive-file "$NEXTEST_DIR/lib-tests.tar.zst" --workspace-remap "$GITHUB_WORKSPACE" \\',
          '--ignore-default-filter --message-format json > "$NEXTEST_DIR/universe.json"',
          'python3 scripts/check_relay_authority_contract.py --nextest-list "$NEXTEST_DIR/universe.json"',
          'python3 scripts/ci/rust_test_sources.py compare --platform linux --list "$NEXTEST_DIR/universe.json" \\',
          '--json-out "$NEXTEST_DIR/compare.json"',
          'python3 scripts/ci/nextest_ci.py plan --universe "$NEXTEST_DIR/universe.json" \\',
          '--archive "$NEXTEST_DIR/lib-tests.tar.zst" --workers 6 --sha "$GITHUB_SHA" --scope linux-lib \\',
          "--rust-version 1.94.1 --nextest-version \"$(cargo nextest --version | awk 'NR==1 {print $2}')\" \\",
          "--target x86_64-unknown-linux-gnu --build-option CARGO_PROFILE_DEV_DEBUG=0 \\",
          '--build-option CARGO_PROFILE_TEST_DEBUG=0 --output "$NEXTEST_DIR/plan"',
          'python3 scripts/ci/nextest_ci.py selector-audit --plan "$NEXTEST_DIR/plan/plan.json"',
        ],
        "timeout_minutes" => 45, "env" => {"BASH_ENV" => "/dev/null"},
      },
      # Dropping this step or changing its argv (a report-only diff) is rejected here.
      "Compare tests with the comparison base" => {
        "commands" => ['bash scripts/ci/nextest-base-diff.sh "$RUNNER_TEMP/nextest"'], "timeout_minutes" => 40,
        "env" => {"BASH_ENV" => "/dev/null", "GH_TOKEN" => "${{ github.token }}", "PR_HEAD_SHA" => "${{ github.event.pull_request.head.sha }}"},
      },
    },
  },
  "rust_tests" => {
    "label" => "nextest worker job", "name" => "Rust tests (worker ${{ matrix.worker }})",
    "needs" => %w[changes rust_build], "if" => built_gate, "runs_on" => "ubuntu-latest",
    "job_timeout_minutes" => 75, "job_sha256" => "8f3b3092e27904b1fa84db80039bf5da6a1da78cd7709b71199dd44e49453e7f", "require_debug_env" => false,
    "cargo_steps" => {
      "Verify archive and plan" => {
        "commands" => ['python3 scripts/ci/nextest_ci.py verify-archive --plan "$RUNNER_TEMP/nextest/plan/plan.json" --worker ${{ matrix.worker }} --archive "$RUNNER_TEMP/nextest/lib-tests.tar.zst" --config "$RUNNER_TEMP/nextest/plan/worker-${{ matrix.worker }}.toml" --sha "$GITHUB_SHA" --record "$RUNNER_TEMP/nextest-result"'],
        "timeout_minutes" => nil, "env" => {"BASH_ENV" => "/dev/null"},
      },
      "Run planned tests" => {
        "commands" => [
          "set +e", 'NEXTEST_DIR="$RUNNER_TEMP/nextest"', 'RESULT_DIR="$RUNNER_TEMP/nextest-result"', 'mkdir -p "$RUNNER_TEMP/nextest-extract"',
          'cargo nextest run --archive-file "$NEXTEST_DIR/lib-tests.tar.zst" --workspace-remap "$GITHUB_WORKSPACE" \\',
          '--extract-to "$RUNNER_TEMP/nextest-extract" --extract-overwrite \\',
          '--config-file "$NEXTEST_DIR/plan/worker-${{ matrix.worker }}.toml" --profile ci --no-tests fail',
          "code=$?", 'echo "$code" > "$RESULT_DIR/exit-code"',
          %q(junit="$(find "$RUNNER_TEMP/nextest-extract" "$GITHUB_WORKSPACE/target" -path '*/nextest/ci/junit.xml' 2>/dev/null | head -n 1)"),
          '[ -n "$junit" ] && cp "$junit" "$RESULT_DIR/junit.xml"', 'exit "$code"',
        ],
        "timeout_minutes" => 60, "env" => {"BASH_ENV" => "/dev/null"},
      },
    },
  },
  "test_evidence" => {
    "label" => "nextest evidence job", "name" => "Rust tests evidence",
    "needs" => %w[changes rust_build rust_tests], "if" => built_gate, "runs_on" => "ubuntu-latest",
    "job_timeout_minutes" => 15, "job_sha256" => "14d385240c0d4d6438308e41efd0bf308f0ba963bde15592d5c1615b9c61cf45", "require_debug_env" => false,
    "cargo_steps" => {
      "Audit worker results" => {
        "commands" => [
          "set -euo pipefail", 'plan="$RUNNER_TEMP/nextest/plan/plan.json"',
          'python3 scripts/ci/nextest_ci.py collect-results --plan "$plan" --artifacts "$RUNNER_TEMP/nextest-artifacts" \\',
          '--output "$RUNNER_TEMP/nextest-results"',
          'python3 scripts/ci/nextest_ci.py selector-audit --plan "$plan"', "status=0",
          'python3 scripts/ci/nextest_ci.py audit --plan "$plan" --results "$RUNNER_TEMP/nextest-results" \\',
          '--json-out "$RUNNER_TEMP/verdict.json" --summary-out "$RUNNER_TEMP/summary.md" || status=$?',
          'cat "$RUNNER_TEMP/summary.md" >> "$GITHUB_STEP_SUMMARY"', 'exit "$status"',
        ],
        "timeout_minutes" => nil, "env" => {"BASH_ENV" => "/dev/null"},
      },
    },
  },
  "check_fast_cross_os" => {
    "label" => "cross-OS job",
    "name" => 'Fast check + non-PG tests (${{ matrix.os }})',
    "needs" => "changes",
    "if" => "needs.changes.outputs.rust_compile == 'true' && needs.changes.outputs.cross_os_rust == 'true'",
    "runs_on" => '${{ matrix.os }}',
    "job_sha256" => "79745b96e7fc4d02c7744ac9141e4da962ea56c603b5926401294c1af8a56cee",
    "cargo_steps" => {
      "cargo check" => {
        "commands" => ["cargo check --workspace --all-targets"],
        "timeout_minutes" => nil,
      },
    },
  },
  "check_fast_cross_os_targets" => {
    "label" => "cross-OS exact targets job",
    "name" => 'Windows exact targets (${{ matrix.os }})',
    "needs" => "changes",
    "if" => "needs.changes.outputs.rust_compile == 'true' && needs.changes.outputs.cross_os_rust == 'true'",
    "runs_on" => '${{ matrix.os }}',
    "job_sha256" => "6a7671032f461e68dfba34df04f7cc3ee3cb2e31a7c692703979fbdca20fd0c8",
    "cargo_steps" => {
      "Writer namespace exact Windows targets" => {
        "commands" => ["./scripts/ci/run-writer-namespace-windows-targets.sh"],
        "timeout_minutes" => 60,
        "if_condition" => "runner.os == 'Windows'",
      },
      "Compare Windows Rust test sources with the nextest list" => {
        "commands" => [
          "set -euo pipefail",
          "cargo nextest list --lib --ignore-default-filter --message-format json > target/universe-windows.json",
          "python scripts/ci/rust_test_sources.py compare --platform windows --list target/universe-windows.json --json-out target/compare-windows.json",
        ],
        "timeout_minutes" => 30,
        "if_condition" => "runner.os == 'Windows'",
        "env" => {"BASH_ENV" => "/dev/null"},
      },
    },
  },
  "pg15_owner_proof" => {
    "label" => "PostgreSQL 15 owner proof job",
    "name" => "PostgreSQL 15 owner proof",
    "needs" => "changes",
    "if" => rust_tests_gate,
    "runs_on" => "ubuntu-latest",
    "job_timeout_minutes" => 50,
    "job_sha256" => "8969b3636cef97c616239bf3aff95df073f8d2a36afb5fe368f120013a7d78ca",
    "cargo_steps" => {
      "PostgreSQL 15 exact owner proof" => {
        "commands" => ["python3 scripts/run_test_lane.py --lane pg --max-summaries 4 --expected-ids scripts/ci/pg15_required_ids.txt -- env -u AGENTDESK_ROOT_DIR python3 scripts/ci/lib-test-artifact.py run -- cargo test --lib -- --exact db::auto_queue::run_owner_guard_pg_tests::cancel_rereads_dispatch_claimed_mid_cancel_pg db::auto_queue::run_owner_guard_pg_tests::concurrent_phase_gate_saves_serialize_on_dispatch_row_pg db::auto_queue::run_owner_guard_pg_tests::force_pause_cleanup_waits_for_run_token_pg db::auto_queue::run_owner_guard_pg_tests::legacy_context_only_retry_cannot_escape_cancel_pg db::auto_queue::run_owner_guard_pg_tests::legacy_context_only_retry_waits_on_run_token_not_origin_row_pg db::auto_queue::run_owner_guard_pg_tests::phase_gate_context_create_refused_after_cancel_pg db::auto_queue::run_owner_guard_pg_tests::phase_gate_transfer_foreign_to_target_refused_after_cancel_pg db::auto_queue::run_owner_guard_pg_tests::phase_gate_transfer_target_to_foreign_serializes_with_cancel_pg db::auto_queue::run_owner_guard_pg_tests::rest_revival_refused_for_cancelled_owner_pg db::auto_queue::run_owner_guard_pg_tests::review_recovery_cas_rejects_direct_cancel_pg db::auto_queue::run_owner_guard_pg_tests::review_recovery_refuses_failed_dispatch_of_cancelled_run_pg db::auto_queue::run_owner_guard_pg_tests::review_recovery_waits_for_run_cancel_and_refuses_pg db::auto_queue::run_owner_guard_pg_tests::verdict_rollback_revival_refused_for_cancelled_owner_pg db::postgres::tests::postgres_json_extract_0058_up_and_down_preserve_array_path_behavior dispatch::dispatch_cancel::pg_observability_tests::cancel_run_lookup_ignores_malformed_legacy_context services::auto_queue::cancel_run::malformed_context_pg_tests::owner_context_extract_matrix_pg services::auto_queue::cancel_run::malformed_context_pg_tests::public_cancel_census_failure_rolls_back_pg services::auto_queue::cancel_run::malformed_context_pg_tests::public_cancel_counts_surviving_associated_dispatch_pg services::auto_queue::cancel_run::malformed_context_pg_tests::public_cancel_ignores_unrelated_malformed_context_pg services::auto_queue::cancel_run::malformed_context_pg_tests::public_end_ignores_unrelated_malformed_context_pg --test-threads=1"],
        "timeout_minutes" => 40,
        "env" => {"ADK_LIB_TEST_ARTIFACT" => "${{ runner.temp }}/lib-test-artifact", "BASH_ENV" => "/dev/null"},
      },
    },
  },
  "relay_authority_mutations" => {
    "label" => "relay-authority mutations job",
    "name" => "Relay authority mutations (${{ matrix.shard }})",
    "needs" => "changes",
    "if" => "${{ !cancelled() && needs.changes.outputs.rust_tests_skip != 'true' }}",
    "runs_on" => "ubuntu-latest",
    "job_sha256" => "43f7e213658325ad0454f1176bdb2673ba797c129c0acb873a8fc389faa81205",
    "job_timeout_minutes" => 45,
    "cargo_steps" => {
      "Fetch Cargo dependencies" => {
        "commands" => ["cargo fetch --locked"],
        "timeout_minutes" => 10,
        "if_condition" => "steps.mutation_paths.outputs.mutation_sources != 'false' || steps.mutation_wiring.outputs.wiring_changed != 'false'",
      },
      "Require relay-authority mutations to be killed" => {
        "commands" => ["bash scripts/run_relay_authority_mutations.sh"],
        "timeout_minutes" => 45,
        "if_condition" => "steps.mutation_paths.outputs.mutation_sources != 'false' || steps.mutation_wiring.outputs.wiring_changed != 'false'",
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
proof_owners = [
  "scripts/ci/run-writer-namespace-windows-targets.sh",
  "scripts/exact_rust_test_proof.py",
]
filter_step = Array(jobs.dig("changes", "steps")).find { |step| step.is_a?(Hash) && step["uses"] == "dorny/paths-filter@v3" }
path_filters = YAML.safe_load(filter_step&.dig("with", "filters").to_s) || {}
proof_owners.product(%w[rust_compile cross_os_rust]).each do |owner, filter|
  errors << "exact Rust proof owner #{owner} must select #{filter} exactly once" unless Array(path_filters[filter]).count(owner) == 1
end

targets.each do |job_id, spec|
  label = spec.fetch("label")
  job = jobs[job_id]
  raw_job = raw_jobs.is_a?(Hash) ? raw_jobs[job_id] : nil
  unless job.is_a?(Hash)
    errors << "#{label} (#{job_id}) must be a YAML mapping"
    next
  end

  canonical_job = canonical_yaml(job)
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
  if %w[check_fast_cross_os check_fast_cross_os_targets].include?(job_id)
    strategy = job["strategy"]
    unless strategy.is_a?(Hash)
      errors << "#{label} must retain its matrix strategy"
    else
      errors << "#{label} matrix must fail independently" unless strategy["fail-fast"] == false
      errors << "#{label} must retain the Windows matrix" unless strategy.dig("matrix", "os") == ["windows-latest"]
    end
  end
  if job_id == "rust_tests"
    expected_strategy = {"fail-fast" => false, "matrix" => {"worker" => [0, 1, 2, 3, 4, 5]}}
    errors << "#{label} must use the validated sweep matrix without fail-fast" unless job["strategy"] == expected_strategy
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
      unless step["shell"] == step_spec.fetch("shell", "bash")
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
      expected_env = step_spec.fetch("env", protected_step_env)
      unless step_env == expected_env
        errors << "#{label} #{name.inspect} must pin exact step env and disable BASH_ENV"
      end
      lines = run.lines.map(&:strip).reject { |line| line.empty? || line.start_with?("#") }
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
main_workflow=".github/workflows/ci-main.yml"
nightly_workflow=".github/workflows/ci-nightly.yml"
validate_main_script_check_shards() {
  if ! ruby - "$main_workflow" <<'RUBY'
require "yaml"
path = ARGV.fetch(0)
document = YAML.load_file(path)
jobs = document.fetch("jobs", {})
errors = []
expected_shards = {
  "scripts" => ["cargo", "Main script checks"],
  "scripts_guards" => ["guards", "Main script checks (guards)"],
  "scripts_contracts" => ["contracts", "Main script checks (contracts)"],
}
base_env = {
  "GFP_EVENT_NAME" => "${{ github.event_name }}",
  "GFP_REF" => "${{ github.ref }}",
  "GFP_REF_PROTECTED" => "${{ github.ref_protected }}",
  "GFP_REPOSITORY" => "${{ github.repository }}",
  "GFP_CANDIDATE_SHA" => "${{ github.sha }}",
  "TEST_LANE_BASELINE_REF" => "HEAD",
}
selector_key = ->(key) { key.to_s.start_with?("SCRIPT_CHECK_", "GFP_", "TEST_LANE_") }
errors << "workflow env must not set script-check selector variables" if (document["env"] || {}).keys.any?(&selector_key)
runners = jobs.select do |_, job|
  job.is_a?(Hash) && Array(job["steps"]).any? { |step| step.is_a?(Hash) && step["run"].to_s.include?("ci-script-checks.sh") }
end
errors << "jobs running ci-script-checks.sh must be exactly #{expected_shards.keys.sort}, found #{runners.keys.sort}" unless runners.keys.sort == expected_shards.keys.sort
expected_shards.each do |job_id, (shard, name)|
  job = jobs[job_id]
  label = "job #{job_id}"
  unless job.is_a?(Hash)
    errors << "#{label} is missing"
    next
  end
  errors << "#{label} must be named #{name.inspect}" unless job["name"] == name
  errors << "#{label} must run on ubuntu-latest" unless job["runs-on"] == "ubuntu-latest"
  %w[if needs continue-on-error].each { |key| errors << "#{label} must not set #{key}" if job.key?(key) }
  errors << "#{label} job env must not set script-check selector variables" if (job["env"] || {}).keys.any?(&selector_key)
  steps = Array(job["steps"])
  checkout = steps.first
  errors << "#{label} must check out full history first" unless checkout.is_a?(Hash) && checkout["uses"] == "actions/checkout@v4" && checkout["with"] == {"fetch-depth" => 0}
  runs = steps.select { |step| step.is_a?(Hash) && step["run"].to_s.include?("ci-script-checks.sh") }
  run = runs.first
  unless runs.length == 1 && run["name"] == "Run script checks" && run["run"] == "./scripts/ci-script-checks.sh"
    errors << "#{label} must run ./scripts/ci-script-checks.sh in exactly one Run script checks step"
    next
  end
  %w[if continue-on-error].each { |key| errors << "#{label} Run script checks must not set #{key}" if run.key?(key) }
  expected_env = base_env.merge("SCRIPT_CHECK_SHARD" => shard)
  errors << "#{label} Run script checks env must equal #{expected_env}" unless run["env"] == expected_env
end
uploads = jobs.flat_map do |job_id, job|
  next [] unless job.is_a?(Hash)
  Array(job["steps"]).select { |step| step.is_a?(Hash) && step["uses"].to_s.start_with?("actions/upload-artifact") }.map { |step| [job_id, step] }
end
evidence = uploads.select { |_, step| step["name"] == "Upload giant-file progress evidence" }
unless evidence == [["scripts", {"name" => "Upload giant-file progress evidence", "if" => "always()", "uses" => "actions/upload-artifact@v4", "with" => {"path" => "target/giant-file-progress/evidence.json"}}]]
  errors << "giant-file progress evidence upload must remain exact, unconditional and only in the cargo shard job"
end
clippy_upload = {"name" => "Upload Clippy observation", "uses" => "actions/upload-artifact@v4", "with" => {"name" => "clippy-observation-${{ github.sha }}", "path" => "target/clippy-observation/", "if-no-files-found" => "error"}}
nextest_upload = ->(job_id, step) { %w[rust_build rust_tests].include?(job_id) && step["uses"] == "actions/upload-artifact@v4" && step["with"].to_h["name"].to_s.start_with?("nextest-") }
errors << "artifact uploads must stay in their approved jobs" unless uploads.all? { |job_id, step| job_id == "scripts" || (job_id == "lint" && step == clippy_upload) || nextest_upload.(job_id, step) }
errors.each { |message| warn "#{path}: #{message}" }
exit(errors.empty? ? 0 : 1)
RUBY
  then
    error "$main_workflow must preserve fail-closed giant-file selector and evidence wiring across every script-check shard"
  fi
}
validate_main_script_check_shards

validate_main_lint_and_nextest() {
  if ! ruby - "$main_workflow" "$pr_workflow" "$nightly_workflow" <<'RUBY'
require "yaml"
require "json"
require "digest"
main_path, pr_path, nightly_path = ARGV
main_jobs = YAML.load_file(main_path).fetch("jobs", {})
pr_jobs = YAML.load_file(pr_path).fetch("jobs", {})
nightly_jobs = YAML.load_file(nightly_path).fetch("jobs", {})
errors = []
lint_tests_name = "Non-lib tests and doctests"
cleanup = ->(step) { step["name"] == "sccache stats" || step["run"] == "./scripts/ci/postgres-service.sh stop" }
{"lint" => "Main lint and non-lib tests (ubuntu-latest)"}.each do |job_id, name|
  job = main_jobs[job_id]
  unless job.is_a?(Hash)
    errors << "job #{job_id} is missing"
    next
  end
  errors << "job #{job_id} must be named #{name.inspect}" unless job["name"] == name
  errors << "job #{job_id} must run on ubuntu-latest" unless job["runs-on"] == "ubuntu-latest"
  %w[if needs continue-on-error strategy].each { |key| errors << "job #{job_id} must not set #{key}" if job.key?(key) }
  Array(job["steps"]).each do |step|
    next unless step.is_a?(Hash)
    label = "job #{job_id} step #{(step["name"] || step["uses"] || step["run"]).to_s.inspect}"
    errors << "#{label} must not set continue-on-error" if step.key?("continue-on-error")
    errors << "#{label} may only set if: always() as a cleanup step" if step.key?("if") && !(step["if"] == "always()" && cleanup.(step))
  end
end
lint = main_jobs["lint"]
if lint.is_a?(Hash)
  lint_steps = Array(lint["steps"]).select { |step| step.is_a?(Hash) }
  ["Policy JS unit tests", "just fmt-check", "just lint", lint_tests_name].each do |name|
    errors << "job lint must have exactly one #{name.inspect} step" unless lint_steps.count { |step| step["name"] == name } == 1
  end
  expected_lint_run = <<~CLIPPY
    set -o pipefail
    mkdir -p target/clippy-observation
    cargo clippy --workspace --all-targets --all-features --message-format=json -- -W clippy::all | tee target/clippy-observation/diagnostics.jsonl
    python3 scripts/check_clippy_warning_count.py --input target/clippy-observation/diagnostics.jsonl --output target/clippy-observation/report.json --baseline scripts/clippy_warning_baseline.json --base-ref HEAD^1
  CLIPPY
  expected_lint = {"name" => "just lint", "shell" => "bash", "run" => expected_lint_run}
  errors << "main Clippy observation must retain exact argv and fail-closed compilation" unless lint_steps.select { |step| step["name"] == "just lint" } == [expected_lint]
  expected_upload = {"name" => "Upload Clippy observation", "uses" => "actions/upload-artifact@v4", "with" => {"name" => "clippy-observation-${{ github.sha }}", "path" => "target/clippy-observation/", "if-no-files-found" => "error"}}
  errors << "main Clippy observation upload must remain exact" unless lint_steps.select { |step| step["name"] == "Upload Clippy observation" } == [expected_upload]
  {"main" => [lint, expected_lint, 2], "PR" => [pr_jobs["lint"], expected_lint.merge("run" => expected_lint_run.sub("--base-ref HEAD^1", '--base-ref "$CLIPPY_BASE_REF"'), "env" => {"CLIPPY_BASE_REF" => "${{ github.event.pull_request.base.sha }}"}), 0]}.each do |label, (job, expected, depth)|
    steps = Array(job && job["steps"])
    errors << "#{label} warning ceiling must retain exact fail-closed execution" unless steps.select { |step| step["name"] == "just lint" } == [expected]
    errors << "#{label} warning baseline comparison requires the base commit checkout" unless steps.first == {"uses" => "actions/checkout@v4", "with" => {"fetch-depth" => depth}}
    toolchain = steps.find { |step| step["name"] == "Install Rust toolchain" }
    errors << "#{label} warning ceiling requires the measured toolchain" unless toolchain && toolchain.dig("with", "toolchain") == "1.94.1"
  end
  tests = lint_steps.find { |step| step["name"] == lint_tests_name }
  %w[CARGO_PROFILE_DEV_DEBUG CARGO_PROFILE_TEST_DEBUG].each do |key|
    errors << "job lint #{lint_tests_name} must keep #{key}=0" unless tests && (tests["env"] || {})[key] == "0"
  end
end
# PR N == main N == nightly N: the jobs differ only in needs, the PR base comparison and the main baseline upload.
built_gate = "${{ !cancelled() && needs.rust_build.result == 'success' }}"
baseline_upload = {"name" => "Upload nextest baseline", "uses" => "actions/upload-artifact@v4", "with" => {"name" => "nextest-baseline", "path" => "${{ runner.temp }}/nextest/plan/plan.json", "if-no-files-found" => "error", "retention-days" => 14}}
role_free = ->(job, role_step) { job.reject { |key, _| %w[needs if].include?(key) }.merge("steps" => Array(job["steps"]).reject { |step| step.is_a?(Hash) && step["name"] == role_step }) }
{"rust_build" => [nil, nil], "rust_tests" => ["rust_build", built_gate], "test_evidence" => [%w[rust_build rust_tests], built_gate]}.each do |job_id, (needs, gate)|
  main_job, pr_job = main_jobs[job_id], pr_jobs[job_id]
  unless main_job.is_a?(Hash) && pr_job.is_a?(Hash)
    errors << "job #{job_id} must exist in both workflows"
    next
  end
  errors << "main #{job_id} must keep needs #{needs.inspect} and if #{gate.inspect}" unless main_job["needs"] == needs && main_job["if"] == gate
  errors << "main #{job_id} must equal the PR job apart from needs, if and the role step" unless role_free.(main_job, "Upload nextest baseline") == role_free.(pr_job, "Compare tests with the comparison base")
  nightly_job = nightly_jobs[job_id]
  errors << "nightly #{job_id} must equal the main job apart from the baseline upload" unless nightly_job.is_a?(Hash) && nightly_job == role_free.(main_job, "Upload nextest baseline").merge(main_job.slice("needs", "if"))
end
main_baseline = Array(main_jobs.dig("rust_build", "steps")).select { |step| step.is_a?(Hash) && step["name"] == "Upload nextest baseline" }
errors << "main rust_build must upload the plan as nextest-baseline for 14 days, exactly once" unless main_baseline == [baseline_upload]
errors << "only rust_build, rust_tests and test_evidence may run the main lib tests" unless main_jobs.none? { |job_id, job| !%w[rust_build rust_tests test_evidence].include?(job_id) && Array(job.is_a?(Hash) && job["steps"]).any? { |step| step.is_a?(Hash) && step["run"].to_s.match?(/cargo nextest|test-selection\.py|test-postgres|cargo test --lib(?! --no-run)/) } }
lib_run = /cargo nextest|test-selection\.py|test-postgres|cargo test --lib(?! --no-run)/
errors << "nightly lib tests run only in rust_build, rust_tests, test_evidence and the native nextest-native.sh steps" unless nightly_jobs.none? { |job_id, job| !%w[rust_build rust_tests test_evidence].include?(job_id) && Array(job.is_a?(Hash) && job["steps"]).any? { |step| step.is_a?(Hash) && step["run"].to_s.match?(lib_run) } }
errors.each { |message| warn "#{main_path}: #{message}" }
exit(errors.empty? ? 0 : 1)
RUBY
  then
    error "$main_workflow and $nightly_workflow must keep their lint contract and run the same nextest jobs as $pr_workflow"
  fi
}
validate_main_lint_and_nextest

validate_main_windows_cache_warm() {
  if ! ruby - "$main_workflow" "$pr_workflow" <<'RUBY'
require "yaml"
main_path, pr_path = ARGV
main = YAML.load_file(main_path)
pr = YAML.load_file(pr_path)
errors = []
label = "#{main_path} job windows_cache_warm"
job = main.fetch("jobs", {})["windows_cache_warm"]
unless job.is_a?(Hash)
  warn "#{label} is missing; PR Windows jobs would compile cold"
  exit 1
end

# rust-cache and sccache hash these env prefixes, so both workflows must agree.
hashed = ->(env) { (env || {}).select { |key, _| key.to_s.match?(/\A(CARGO|CC|CFLAGS|CXX|CMAKE|RUST)/) } }
errors << "#{label}: workflow CARGO/RUST env must equal #{pr_path}" unless hashed.(main["env"]) == hashed.(pr["env"])

setup = ->(steps) do
  Array(steps).take_while { |step| step["uses"].to_s.start_with?("actions/checkout", "actions/setup-python", "dtolnay/", "mozilla-actions/", "Swatinem/") || step["if"] == "runner.os == 'macOS'" }
    .reject { |step| step["if"] == "runner.os == 'macOS'" }
    .map { |step| step.reject { |key, _| %w[name if].include?(key) } }
end
compile = ->(steps) { Array(steps).select { |step| step["run"].to_s.include?("cargo") } }
main_compile = compile.(job["steps"])

pr_compile_steps = {
  "check_fast_cross_os" => "cargo check",
  "check_fast_cross_os_targets" => "Writer namespace exact Windows targets",
}
pr_compile_steps.each do |pr_id, step_name|
  pr_job = pr.fetch("jobs", {})[pr_id]
  unless pr_job.is_a?(Hash)
    errors << "#{pr_path} job #{pr_id} is missing"
    next
  end
  errors << "#{label}: env must equal #{pr_id}" unless job["env"] == pr_job["env"]
  errors << "#{label}: setup steps before the compile must equal #{pr_id}" unless setup.(job["steps"]) == setup.(pr_job["steps"])
  pr_shape = Array(pr_job["steps"]).select { |step| step["name"] == step_name }.map { |step| step.slice("shell", "env") }
  errors << "#{label}: compile step shell/env must equal #{pr_id} #{step_name.inspect}" unless pr_shape.length == 1 && main_compile.map { |step| step.slice("shell", "env") }.uniq == pr_shape
end
pr_check = compile.(pr.dig("jobs", "check_fast_cross_os", "steps")).map { |step| step["run"].strip }
errors << "#{label}: cargo check must equal check_fast_cross_os" unless main_compile.first&.fetch("run", "")&.strip == pr_check.first

expected_cache = {
  "cache-targets" => false,
  "cache-bin" => false,
  "shared-key" => "cargo-dependencies-v2",
  "save-if" => "${{ github.ref == 'refs/heads/main' }}",
}
cache_steps = Array(job["steps"]).select { |step| step["uses"] == "Swatinem/rust-cache@v2" }
errors << "#{label}: rust-cache must save the shared key from main only" unless cache_steps.map { |step| step["with"] } == [expected_cache]
commands = main_compile.map { |step| step["run"].strip }
errors << "#{label}: must build exactly check + lib test binary without running tests" unless commands == ["cargo check --workspace --all-targets", "cargo test --lib --no-run"]
errors << "#{label}: must not gate or depend on other jobs" if job.key?("needs") || job.key?("if")
errors << "#{label}: must stay advisory (continue-on-error: true)" unless job["continue-on-error"] == true
errors << "#{label}: must run on windows-latest" unless job["runs-on"] == "windows-latest"

errors.each { |message| warn message }
exit(errors.empty? ? 0 : 1)
RUBY
  then
    error "$main_workflow must warm the Windows cargo cache the PR Windows jobs restore"
  fi
}
validate_main_windows_cache_warm

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
# Keep this explicit: deriving approval from workflow values would admit custom runners.
HOSTED_RUNNER_LABELS = %w[ubuntu-latest ubuntu-22.04 macos-15 macos-latest windows-latest].freeze

def retired_runner_reference?(value, implicit_expression = false)
  case value
  when Hash
    value.values.any? { |item| retired_runner_reference?(item) }
  when Array
    value.any? { |item| retired_runner_reference?(item) }
  when String
    expressions = value.scan(/\$\{\{((?:'(?:[^']|'')*'|(?!\}\}).)*)\}\}/m).flatten
    expressions << value if implicit_expression && !value.include?("${{")
    expressions.any? do |expression|
      # Quoted expression literals are one token, so documentation is not a variable read.
      tokens = expression.scan(/'(?:[^']|'')*'|[A-Za-z_][A-Za-z0-9_-]*|[^\s]/)
      tokens.each_index.any? do |index|
        next false unless tokens[index].casecmp("vars").zero? && tokens[index - 1] != "."

        property = tokens[index + 1, 2].map(&:downcase)
        property == [".", "macos_runner"] ||
          (property == ["[", "'macos_runner'"] && tokens[index + 3] == "]")
      end
    end
  else
    false
  end
end

def static_matrix_value?(value)
  case value
  when Hash then value.all? { |key, item| static_matrix_value?(key) && static_matrix_value?(item) }
  when Array then value.all? { |item| static_matrix_value?(item) }
  when String then !value.include?("${{")
  else true
  end
end

# Repository policy requires explicit static runner values, including in every include row.
# Exclude cannot approve forbidden candidates; matrix merge semantics are not evaluated.
def matrix_runner_labels(job, axis)
  strategy = job["strategy"]
  matrix = strategy.is_a?(Hash) ? strategy["matrix"] : nil
  return [] unless matrix.is_a?(Hash) && static_matrix_value?(matrix)

  dimensions = matrix.reject { |key, _value| %w[include exclude].include?(key) }
  return [] unless dimensions.all? { |key, values| key.is_a?(String) && values.is_a?(Array) && !values.empty? }
  return [] unless dimensions.key?(axis) || dimensions.empty?

  included = matrix.fetch("include", [])
  excluded = matrix.fetch("exclude", [])
  return [] unless included.is_a?(Array) && excluded.is_a?(Array)
  return [] unless excluded.all? { |row| row.is_a?(Hash) }
  return [] unless included.all? { |row| row.is_a?(Hash) && row.key?(axis) }

  dimensions.fetch(axis, []) + included.map { |row| row[axis] }
end

def hosted_runner?(runner, job)
  case runner
  when String
    return true if HOSTED_RUNNER_LABELS.include?(runner)

    reference = /\A\$\{\{\s*matrix\.([A-Za-z_][A-Za-z0-9_-]*)\s*\}\}\z/.match(runner)
    return false unless reference

    labels = matrix_runner_labels(job, reference[1])
    !labels.empty? && labels.all? { |label| label.is_a?(String) && HOSTED_RUNNER_LABELS.include?(label) }
  when Array
    # Multiple labels require one runner to match all of them, so allow only one selector.
    runner.length == 1 && runner.all? { |label| label.is_a?(String) && hosted_runner?(label, job) }
  when Hash
    labels = runner["labels"]
    runner.keys == ["labels"] && (labels.is_a?(String) || labels.is_a?(Array)) && hosted_runner?(labels, job)
  else
    false
  end
end

implicit_retired_reference = jobs.values.any? do |job|
  next false unless job.is_a?(Hash)

  [job, *job.fetch("steps", [])].any? do |entry|
    entry.is_a?(Hash) && retired_runner_reference?(entry["if"], true)
  end
end
if retired_runner_reference?(document) || implicit_retired_reference
  warn "#{path}: hosted runner policy forbids vars.MACOS_RUNNER references"
  exit 1
end
jobs.each do |job_id, job|
  unless job.is_a?(Hash) && hosted_runner?(job["runs-on"], job)
    warn "#{path}: jobs.#{job_id}.runs-on violates repository hosted runner policy: use one approved label (scalar or singleton array), optionally under labels, or an explicitly enumerated static matrix runner axis; every include row must specify that axis and exclude cannot approve forbidden candidates; each matrix runner value must be an approved scalar label (#{HOSTED_RUNNER_LABELS.join(', ')})"
    exit 1
  end
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
  judge = jobs["required_contexts"]
  unless judge.is_a?(Hash) && judge["name"] == "${{ matrix.context }}" && Array(judge.dig("strategy", "matrix", "context")).include?(required_context)
    warn "#{path}: required Script checks context must be a literal matrix value of jobs.required_contexts"
    exit 1
  end
  unless scripts.is_a?(Hash) && scripts["name"] != required_context
    warn "#{path}: jobs.scripts must not publish the required Script checks context"
    exit 1
  end
  if required_context_jobs.any?
    warn "#{path}: required Script checks context must belong only to jobs.required_contexts"
    exit 1
  end
  unsafe_dynamic_name_jobs.delete("required_contexts")
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

# The nine branch-protection names come only from the required_contexts judge matrix in ci-pr.yml;
# any other job or workflow that can render one would publish a result the judge never saw.
validate_required_context_publishers() {
  if ! command -v ruby >/dev/null 2>&1; then
    error "ruby is required to validate required context publishers structurally"
    return
  fi

  local workflows=()
  while IFS= read -r -d '' workflow; do
    workflows+=("$workflow")
  done < <(workflow_files)

  if ! ruby - "$pr_workflow" "$REQUIRED_CONTEXTS_JSON" "${workflows[@]}" <<'RUBY'
require "yaml"
require "json"

pr_path, contexts_json, *workflow_paths = ARGV
contexts = JSON.parse(contexts_json)

# Mirrors the Script checks rule: one matrix substitution is allowed only when
# its static prefix/suffix cannot render the required context.
def can_render_context?(name, context)
  return name.strip == context unless name.include?("${{")

  shape_valid = name.scan("${{").length == 1 && name.scan("}}").length == 1
  matrix_name = shape_valid &&
    /\A([^{}]*)\$\{\{\s*matrix\.[A-Za-z_][A-Za-z0-9_.-]*\s*\}\}([^{}]*)\z/.match(name)
  return true unless matrix_name

  [matrix_name[1], matrix_name[2]].any? { |fragment| fragment.include?(context) } ||
    (context.start_with?(matrix_name[1]) && context.end_with?(matrix_name[2]))
end

# Effective check name: an unnamed job publishes its job ID; a matrix job
# without a name expression may add any " (<values>)" suffix (fails closed).
def publishes_context?(job_id, job, context)
  return false unless job.is_a?(Hash)

  name = job["name"].nil? ? job_id.to_s : job["name"].to_s
  return true if can_render_context?(name, context)

  matrix = job["strategy"].is_a?(Hash) ? job["strategy"]["matrix"] : job["strategy"]
  !matrix.nil? && !name.include?("${{") &&
    context.start_with?("#{name.strip} (") && context.end_with?(")")
end

errors = []
workflow_paths.each do |path|
  begin
    document = YAML.safe_load(File.read(path), aliases: false, filename: path)
  rescue StandardError => error
    errors << "#{path}: cannot parse YAML: #{error.message}"
    next
  end
  jobs = document.is_a?(Hash) && document["jobs"].is_a?(Hash) ? document["jobs"] : {}
  # Workflow names do not namespace check names, so any trigger that can
  # report on a candidate SHA (push, dispatch, schedule) counts.
  contexts.each do |context|
    publishers = jobs.select { |job_id, job| publishes_context?(job_id, job, context) }.keys.map(&:to_s)
    expected = path == pr_path ? ["required_contexts"] : []
    next if publishers == expected

    errors << "#{path}: required #{context} context must be published only by #{pr_path} jobs.required_contexts; publishers: #{publishers.inspect}"
  end
end

errors.each { |message| warn message }
exit(errors.empty? ? 0 : 1)
RUBY
  then
    error "$pr_workflow must publish every required context only from the required_contexts judge"
  fi
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
  if grep -q 'RUSTC_WRAPPER=' "$workflow" && ! grep -q 'SCCACHE_GHA_ENABLED=' "$workflow"; then
    error "$workflow clears RUSTC_WRAPPER but not SCCACHE_GHA_ENABLED"
  fi
done < <(workflow_files)

validate_required_context_uniqueness
validate_required_context_publishers

# Trusted macOS runs on a timer or by hand only and no run cancels another:
# a per-push run outlasts the gap between main pushes and never finishes.
if [ -f "$trusted_workflow" ] && ! ruby -ryaml -e '
  doc = YAML.safe_load(File.read(ARGV[0])); trigger = doc[true] || doc["on"]; group = doc["concurrency"] || {}
  crons = trigger.is_a?(Hash) ? Array(trigger["schedule"]) : []
  exit(trigger.is_a?(Hash) && trigger.keys.sort == %w[schedule workflow_dispatch] && crons.any? &&
    crons.all? { |entry| entry.is_a?(Hash) && entry["cron"].is_a?(String) } &&
    group["cancel-in-progress"] == false && group["group"].to_s.include?("github.event_name"))' "$trusted_workflow"; then
  error "$trusted_workflow must trigger only on schedule and workflow_dispatch, and no run may cancel another"
fi

# Superseded PR heads must release hosted runners immediately. Required
# contexts remain fail-closed on the newest exact SHA; branch protection never
# consumes the cancelled stale SHA's results.
if [ -f "$pr_workflow" ]; then
  validate_pr_debug_envs
fi

exit "$fail"
