//! The slot-reset port: the host checks a KE auto-queue slot reset runs before it changes a
//! thread, and the verdict type its managed reset reads.

use futures::future::BoxFuture;
use sqlx::PgPool;

pub(crate) trait SlotResetPort {
    /// The managed-reset verdict a slot reset applies per thread.
    type Verdict;

    /// Why the verdict refuses the reset, if it does.
    fn verdict_refusal(verdict: &Self::Verdict) -> Option<&str>;

    /// Why the host guard refuses a teardown of this stored row, if it does.
    fn row_host_refusal<'a>(
        pool: &'a PgPool,
        stored_provider: Option<&'a str>,
        channel_id: Option<&'a str>,
        session_key: &'a str,
        tmux_name: &'a str,
        caller: &'a str,
    ) -> BoxFuture<'a, Option<String>>;
}
