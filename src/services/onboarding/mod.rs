// Onboarding service.
//
// The implementation was originally housed in a single 5,287-line file. To
// keep the public API identical while making the code easier to navigate, the
// source is now split into fragments included into this module via the
// `include!` macro. Each fragment is a slice of the original file (function
// bodies are byte-for-byte identical and no visibility was altered), so all
// inter-function references continue to resolve at the same module scope.
//
// Public API entry points (see `src/server/routes/onboarding.rs`):
//   - status, draft_get, draft_put, draft_delete
//   - validate_token, channels, channels_post
//   - complete, check_provider, generate_prompt
// Public request types: OnboardingDraft, ValidateTokenBody, ChannelsQuery,
//   ChannelsBody, CompleteBody, ChannelMapping, CheckProviderBody,
//   GeneratePromptBody.
//
// Fragment layout:
//   draft_types.rs       — OnboardingDraft & related types, helpers
//   status_handlers.rs   — status() / draft_get() / draft_put() / draft_delete()
//   channel_handlers.rs  — validate_token() / channels() / channels_post()
//   completion_state.rs  — CompleteBody, ChannelMapping, completion state types,
//                          channel resolution, draft/completion persistence
//   config_writes.rs     — config / credential writes, owner_id parsing
//   conflicts_verify.rs  — role-map / channel-binding writes & conflict checks
//   complete_impl.rs     — persist_onboarding_pg + complete + complete_with_options
//   tests.rs             — integration tests (legacy-sqlite-tests feature)
//   provider_handlers.rs — check_provider() / generate_prompt()

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::server::routes::AppState;
use crate::services::provider::ProviderKind;
use crate::services::provider_exec;

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn legacy_db(state: &AppState) -> &crate::db::Db {
    state
        .engine
        .legacy_db()
        .or_else(|| state.legacy_db())
        .expect("legacy SQLite DB is only available in no-PG tests")
}

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";
const ONBOARDING_DRAFT_VERSION: u8 = 1;
const MAX_ONBOARDING_DRAFT_BYTES: usize = 128 * 1024;
const MAX_ONBOARDING_DRAFT_COMMAND_BOTS: usize = 4;
const MAX_ONBOARDING_DRAFT_AGENTS: usize = 64;
const MAX_ONBOARDING_DRAFT_CHANNEL_ASSIGNMENTS: usize = 64;
const MAX_ONBOARDING_DRAFT_PROVIDER_STATUSES: usize = 8;
const MAX_ONBOARDING_DRAFT_FUTURE_SKEW_MS: i64 = 5 * 60 * 1000;

include!("draft_types.rs");
include!("status_handlers.rs");
include!("channel_handlers.rs");
include!("completion_state.rs");
include!("config_writes.rs");
include!("conflicts_verify.rs");
include!("complete_impl.rs");
include!("tests.rs");
include!("provider_handlers.rs");
