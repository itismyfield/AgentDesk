# Exact workflow settings for the generated reports published by contracts jobs.
module GeneratedArtifactSteps
  DIRECTORY = "target/generated-contracts"
  REPORTS = {"statechart-index" => "statechart index", "user-feature-map" => "user feature map"}.freeze
  LANDING_CHECK = {
    "name" => "Check landed user feature map",
    "run" => "python3 scripts/generate_user_feature_map.py --check --base-ref HEAD",
  }.freeze

  def self.env(shard)
    shard == "contracts" ? {"GENERATED_ARTIFACT_REPORT_DIR" => DIRECTORY} : {}
  end

  def self.uploads(workflow, raw: false)
    REPORTS.map do |slug, label|
      {
        "name" => "Upload #{label}",
        "uses" => "actions/upload-artifact@v4",
        "with" => {
          "name" => "#{slug}-#{workflow}-${{ github.run_attempt }}",
          "path" => "#{DIRECTORY}/#{slug}/",
          "if-no-files-found" => "error",
          "retention-days" => raw ? "14" : 14,
        },
      }
    end
  end

  def self.tail(shard, workflow, raw: false)
    return [] unless shard == "contracts"

    steps = uploads(workflow, raw: raw)
    steps << LANDING_CHECK if workflow == "main"
    steps
  end
end
