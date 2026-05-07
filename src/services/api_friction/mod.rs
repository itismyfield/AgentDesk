mod core;
mod issue_body;
mod issues;
mod markers;
mod memory_sync;
mod patterns;
mod storage;

#[allow(unused_imports)]
pub(crate) use self::core::{
    ApiFrictionRecordContext, ApiFrictionRecordResult, record_api_friction_reports,
};
#[allow(unused_imports)]
pub(crate) use self::issues::{
    ApiFrictionPatternFailure, ApiFrictionProcessSummary, ProcessedApiFrictionIssue,
    process_api_friction_patterns,
};
#[allow(unused_imports)]
pub(crate) use self::markers::{
    ApiFrictionExtraction, ApiFrictionReport, extract_api_friction_reports,
};
#[allow(unused_imports)]
pub(crate) use self::patterns::ApiFrictionPattern;

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use self::patterns::{
    API_FRICTION_MIN_REPEAT_COUNT, DEFAULT_PATTERN_LIMIT, load_pattern_candidates_pg,
};
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use self::storage::load_dispatch_source_context_pg;

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests;
