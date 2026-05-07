mod core;

#[allow(unused_imports)]
pub(crate) use self::core::{
    ApiFrictionExtraction, ApiFrictionPattern, ApiFrictionPatternFailure,
    ApiFrictionProcessSummary, ApiFrictionRecordContext, ApiFrictionRecordResult,
    ApiFrictionReport, ProcessedApiFrictionIssue, extract_api_friction_reports,
    process_api_friction_patterns, record_api_friction_reports,
};

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use self::core::{
    API_FRICTION_MIN_REPEAT_COUNT, DEFAULT_PATTERN_LIMIT, load_dispatch_source_context_pg,
    load_pattern_candidates_pg,
};

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests;
