use crate::services::routines::loader::{RoutineValidationReport, validate_routine_tree};
use std::path::Path;

pub(crate) fn handle_validate_routines(
    root: &Path,
    runtime_root: &Path,
    json: bool,
) -> std::result::Result<bool, String> {
    let report = validate_routine_tree(root, runtime_root).map_err(|error| error.to_string())?;
    render_report(&report, json)?;
    Ok(report.valid)
}

fn render_report(report: &RoutineValidationReport, json: bool) -> std::result::Result<(), String> {
    if json {
        let rendered = serde_json::to_string_pretty(report)
            .map_err(|error| format!("serialize routine validation report: {error}"))?;
        println!("{rendered}");
        return Ok(());
    }

    if report.valid {
        println!(
            "validated {} routine script(s) under {}",
            report.validated_files.len(),
            report.root.display()
        );
        return Ok(());
    }

    eprintln!(
        "routine validation failed: {} invalid script(s), {} valid script(s)",
        report.failures.len(),
        report.validated_files.len()
    );
    for failure in &report.failures {
        eprintln!("{}: {}", failure.script_ref, failure.message);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_report_exposes_authoritative_validity_bit() {
        let report = RoutineValidationReport {
            valid: false,
            root: "/tmp/routines".into(),
            runtime_root: "/tmp/release".into(),
            validated_files: Vec::new(),
            failures: Vec::new(),
        };

        let value = serde_json::to_value(report).unwrap();

        assert_eq!(value["valid"], false);
        assert_eq!(value["runtimeRoot"], "/tmp/release");
    }
}
