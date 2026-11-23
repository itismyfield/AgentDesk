# frozen_string_literal: true

# Every install-action step in a self-hosted job must sit behind a preinstalled-tool check for the same pinned spec.
# A version bump on the install step alone would otherwise let the check skip it and run the old host tool.
require "yaml"

CHECK_IF = "runner.environment == 'self-hosted'"

def check_errors(path)
  errors = []
  YAML.load_file(path).fetch("jobs", {}).each do |job_id, job|
    next unless job.is_a?(Hash) && job["runs-on"].to_s.include?("self-hosted")

    steps = Array(job["steps"]).select { |step| step.is_a?(Hash) }
    steps.each_with_index do |step, index|
      next unless step["uses"].to_s.start_with?("taiki-e/install-action")

      spec = step.dig("with", "tool").to_s
      check = index.positive? ? steps[index - 1] : {}
      id = check["id"].to_s
      expected = {"name" => "Check preinstalled #{spec.split("@").first}", "id" => id, "if" => CHECK_IF, "shell" => "bash",
                  "env" => {"BASH_ENV" => "/dev/null", "TOOL" => spec}, "run" => 'bash scripts/ci/preinstalled-tool.sh "$TOOL"'}
      label = "#{path}: job #{job_id} step #{step["name"].inspect}"
      errors << "#{label} must pin tool@version" unless spec.match?(/\A[\w-]+@[\w.-]+\z/)
      errors << "#{label} must follow the preinstalled check #{expected.inspect}" unless id.start_with?("preinstalled_") && check == expected
      errors << "#{label} must run only when the check found nothing" unless step["if"] == "steps.#{id}.outputs.found != 'true'"
    end
  end
  errors
end

errors = ARGV.flat_map { |path| check_errors(path) }
errors.each { |message| warn message }
exit(errors.empty? ? 0 : 1)
