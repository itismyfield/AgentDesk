const TRANSITION_CLOCK_COLUMNS: &[&str] = &[
    "requested_at",
    "started_at",
    "review_entered_at",
    "completed_at",
    "suggestion_pending_at",
    "awaiting_dod_at",
];

pub(crate) fn validate_set(state: &str, field: &str) -> anyhow::Result<()> {
    // Creation, update and soft-delete timestamps have separate writers.
    if !TRANSITION_CLOCK_COLUMNS.contains(&field) {
        anyhow::bail!(
            "clock for state '{}' has invalid clock.set {:?}; expected one of {:?}",
            state,
            field,
            TRANSITION_CLOCK_COLUMNS
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::pipeline::{ClockConfig, PipelineConfig, parse_override_strict};

    fn default_pipeline() -> PipelineConfig {
        serde_yaml::from_str(include_str!("../../../policies/default-pipeline.yaml")).unwrap()
    }

    #[test]
    fn validate_rejects_invalid_clock_columns() {
        for field in [
            "",
            "on_enter",
            "status",
            "missing_at",
            "Started_at",
            "STARTED_AT",
            " started_at",
            "started_at ",
            "started_at\n",
            "\"started_at\"",
            "'started_at'",
            "started_at = NULL, title",
            "started_at; DROP TABLE kanban_cards; --",
            "created_at",
            "updated_at",
            "deleted_at",
        ] {
            let mut config = default_pipeline();
            config.clocks.get_mut("in_progress").unwrap().set = field.to_string();
            let result = config.validate();
            assert!(
                result.is_err(),
                "invalid clock.set must be rejected: {field:?}"
            );
            let error = result.unwrap_err();
            let message = error.to_string();
            assert!(message.contains("in_progress"), "{message}");
            assert!(message.contains("clock.set"), "{message}");
        }
    }

    #[test]
    fn validate_accepts_transition_clock_columns() {
        for field in [
            "requested_at",
            "started_at",
            "review_entered_at",
            "completed_at",
            "suggestion_pending_at",
            "awaiting_dod_at",
        ] {
            for mode in [None, Some("coalesce"), Some("reset")] {
                let mut config = default_pipeline();
                config.clocks.insert(
                    "in_progress".to_string(),
                    ClockConfig {
                        set: field.to_string(),
                        mode: mode.map(str::to_string),
                    },
                );
                config.validate().unwrap();
            }
        }
    }

    #[test]
    fn default_and_repository_pipeline_fixtures_validate() {
        for yaml in [
            include_str!("../../../policies/default-pipeline.yaml"),
            include_str!("../../../policies/examples/simple-pipeline.yaml"),
            include_str!("../../../policies/examples/qa-pipeline.yaml"),
        ] {
            let config: PipelineConfig = serde_yaml::from_str(yaml).unwrap();
            config.validate().unwrap();
        }
    }

    #[test]
    fn validate_rejects_invalid_clocks_in_both_override_layers() {
        let invalid =
            parse_override_strict(r#"{"clocks":{"review":{"set":"review_entered_at, title"}}}"#)
                .unwrap()
                .unwrap();
        let inherited = parse_override_strict(r#"{"events":{}}"#).unwrap().unwrap();
        for (repo, agent) in [(&invalid, &inherited), (&inherited, &invalid)] {
            let effective = default_pipeline().merge(repo).merge(agent);
            let error = effective
                .validate()
                .expect_err("merged repo and agent clock.set must be validated");
            assert!(error.to_string().contains("clock.set"), "{error}");
        }
    }
}
