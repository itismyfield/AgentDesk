//! KE queue, session and maintenance tests that call ROOT discord health or server routes, declared here so
//! they leave the KE crate. Each `#[path]` file stays in place and keeps the cfg chain of its original declaration.

#[path = "../../auto_queue/cleanup_tasks_pg_tests.rs"]
mod auto_queue_cleanup_tasks_pg;
#[path = "../../maintenance/jobs/db_retention/record_caps_reader_pg_tests.rs"]
mod db_retention_record_caps_reader_pg;
#[path = "../../dispatched_sessions/listing_tests.rs"]
mod dispatched_sessions_listing;
#[path = "../../hang_forensics/tests/mod.rs"]
mod hang_forensics;
#[path = "../../queue/force_purge_tests.rs"]
mod queue_force_purge;
