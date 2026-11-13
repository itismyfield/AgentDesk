pub(crate) fn prepare_restart_handoff(
    context: &crate::domain::restart_report::RestartReportContext,
    claim: Option<&crate::domain::restart_report::RestartAttempt>,
) -> Result<crate::domain::restart_report::RestartAttempt, String> {
    crate::services::restart_handoff::store::prepare_restart_handoff(context, claim, &|| {
        crate::services::restart_handoff::sample_metadata(
            super::doctor::startup::latest_startup_doctor_health_json,
        )
    })
}
