mod core;
mod markers;
mod memory_sync;
mod storage;

#[allow(unused_imports)]
pub(crate) use self::core::{
    ApiFrictionPattern, ApiFrictionPatternFailure, ApiFrictionProcessSummary,
    ApiFrictionRecordContext, ApiFrictionRecordResult, ProcessedApiFrictionIssue,
    process_api_friction_patterns, record_api_friction_reports,
};
#[allow(unused_imports)]
pub(crate) use self::markers::{
    ApiFrictionExtraction, ApiFrictionReport, extract_api_friction_reports,
};

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use self::core::{
    API_FRICTION_MIN_REPEAT_COUNT, DEFAULT_PATTERN_LIMIT, load_pattern_candidates_pg,
};
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use self::storage::load_dispatch_source_context_pg;

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests;
