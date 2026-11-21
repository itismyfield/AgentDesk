require "yaml"
require "json"
require_relative "generated_artifact_steps"
path = ARGV.fetch(0)
memory_specs = JSON.parse(ARGV.fetch(1))
document = YAML.load_file(path)
jobs = document.fetch("jobs", {})
errors = []
expected_shards = {
  "scripts" => ["cargo", "Main script checks"],
  "scripts_guards" => ["guards", "Main script checks (guards)"],
  "scripts_census" => ["census", "Main script checks (census)"],
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
selector_key = ->(key) { key.to_s.start_with?("SCRIPT_CHECK_", "GFP_", "TEST_LANE_", "GENERATED_ARTIFACT_") }
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
  expected_env = base_env.merge("SCRIPT_CHECK_SHARD" => shard).merge(GeneratedArtifactSteps.env(shard))
  if shard == "contracts" && steps.drop(steps.index(run) + 1) != GeneratedArtifactSteps.tail(shard, "main")
    errors << "#{label} must publish the configured artifacts after Run script checks"
  end
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
census_upload = {"name" => "Upload census execution evidence", "if" => "${{ steps.audit.outputs.census_evidence_dir != '' }}", "continue-on-error" => true, "uses" => "actions/upload-artifact@v4", "with" => {"name" => "legacy-body-census-test-evidence-${{ github.run_attempt }}", "path" => "${{ steps.audit.outputs.census_evidence_dir }}", "if-no-files-found" => "warn", "retention-days" => 7}}
generated_uploads = GeneratedArtifactSteps.uploads("main")
nextest_upload = ->(job_id, step) { %w[rust_build rust_tests].include?(job_id) && step["uses"] == "actions/upload-artifact@v4" && step["with"].to_h["name"].to_s.start_with?("nextest-") }
errors << "artifact uploads must stay in their approved jobs" unless uploads.all? { |job_id, step| job_id == "scripts" || (job_id == "lint" && step == clippy_upload) || (job_id == "scripts_contracts" && generated_uploads.include?(step)) || nextest_upload.(job_id, step) || (job_id == "test_evidence" && step == census_upload) || (%w[rust_build lint scripts].include?(job_id) && step == memory_specs.fetch("Upload runner memory")) || (job_id == "lint" && step == memory_specs.fetch("Upload runner memory checkpoint")) }
errors.each { |message| warn "#{path}: #{message}" }
exit(errors.empty? ? 0 : 1)
