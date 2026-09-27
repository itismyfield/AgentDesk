use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use poise::serenity_prelude as serenity;

tokio::task_local! {
    static AMBIGUOUS: Arc<AtomicBool>;
}

pub(super) async fn observe<F: Future>(future: F) -> (F::Output, bool) {
    let ambiguous = Arc::new(AtomicBool::new(false));
    let result = AMBIGUOUS.scope(ambiguous.clone(), future).await;
    (result, ambiguous.load(Ordering::Acquire))
}

struct Mutation {
    ambiguous: Option<Arc<AtomicBool>>,
    settled: bool,
}

impl Drop for Mutation {
    fn drop(&mut self) {
        if !self.settled {
            if let Some(ambiguous) = &self.ambiguous {
                ambiguous.store(true, Ordering::Release);
            }
        }
    }
}

/// Observe wire uncertainty without changing transport results or cancellation authority.
/// A cancelled await or uncertain response stays ambiguous even after a later success.
pub(in crate::services::discord) async fn track_mutation<T>(
    future: impl Future<Output = serenity::Result<T>>,
) -> serenity::Result<T> {
    let mut mutation = Mutation {
        ambiguous: AMBIGUOUS.try_with(Arc::clone).ok(),
        settled: false,
    };
    let result = future.await;
    mutation.settled = match &result {
        Ok(_) => true,
        Err(serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(response))) => {
            response.status_code.is_client_error() && response.status_code.as_u16() != 408
        }
        Err(_) => false,
    };
    result
}
