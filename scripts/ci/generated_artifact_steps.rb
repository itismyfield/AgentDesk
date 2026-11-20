# Exact workflow settings for the generated reports published by contracts jobs.
module GeneratedArtifactSteps
  DIRECTORY = "target/generated-contracts"
  REPORTS = {"statechart-index" => "statechart index"}.freeze

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
    shard == "contracts" ? uploads(workflow, raw: raw) : []
  end
end
