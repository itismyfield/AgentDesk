use std::fmt::Write;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::get,
};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use super::{api_docs, api_docs_group_category, api_docs_group_or_category, api_help};

#[tokio::test]
async fn api_docs_response_bytes_match_snapshots() {
    let router = Router::new()
        .route("/api/docs", get(api_docs))
        .route("/api/help", get(api_help))
        .route("/api/docs/{segment}", get(api_docs_group_or_category))
        .route("/api/docs/{group}/{category}", get(api_docs_group_category));

    let mut snapshot = String::new();
    for uri in [
        "/api/docs",
        "/api/docs?format=flat",
        "/api/help",
        "/api/docs/runtime",
        "/api/docs/runtime/dispatches",
        "/api/docs/config/settings",
        "/api/docs/automation/campaigns",
        "/api/docs/integrations/kakao-calendar",
        "/api/docs/observability/cluster",
    ] {
        let response = router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{uri}");

        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        writeln!(
            snapshot,
            "{uri}\t{}\t{:x}",
            bytes.len(),
            Sha256::digest(&bytes)
        )
        .unwrap();
    }
    assert_eq!(snapshot, include_str!("testdata/responses.snap"));
}
