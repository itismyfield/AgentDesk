//! Body ETags for JSON GETs, so a client already holding the body gets 304 instead of the payload.

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

/// Weak ETag shared across content encodings. Sorted serde_json object keys
/// keep equal values hashing equally.
pub(crate) fn json_etag(serialized: &str) -> String {
    format!("W/\"{}\"", blake3::hash(serialized.as_bytes()).to_hex())
}

/// 304 when `If-None-Match` already names `etag` (weak or strong) or `*`, otherwise `full()`.
/// Both carry the ETag and content-encoding variation headers.
pub(crate) fn revalidate(
    headers: &HeaderMap,
    etag: &str,
    full: impl FnOnce() -> Response,
) -> Response {
    let mut response = if if_none_match_hits(headers, etag) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        full()
    };
    if let Ok(value) = HeaderValue::from_str(etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
        .headers_mut()
        .append(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    response
}

fn if_none_match_hits(headers: &HeaderMap, etag: &str) -> bool {
    let etag = etag.trim_start_matches("W/");
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|tags| {
            tags.split(',')
                .map(|tag| tag.trim().trim_start_matches("W/"))
                .any(|tag| tag == etag || tag == "*")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_etag_ignores_key_insertion_order() {
        let mut first = serde_json::Map::new();
        first.insert("b".into(), json!(1));
        first.insert("a".into(), json!({"y": 2, "x": 3}));
        let mut second = serde_json::Map::new();
        second.insert("a".into(), json!({"x": 3, "y": 2}));
        second.insert("b".into(), json!(1));
        assert_eq!(
            json_etag(&serde_json::Value::Object(first).to_string()),
            json_etag(&serde_json::Value::Object(second).to_string())
        );
    }

    #[test]
    fn revalidate_answers_304_only_for_a_held_tag() {
        let etag = json_etag("{}");
        let with = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::IF_NONE_MATCH, HeaderValue::from_str(value).unwrap());
            headers
        };
        let status = |headers: &HeaderMap| {
            revalidate(headers, &etag, || StatusCode::OK.into_response()).status()
        };
        assert_eq!(status(&HeaderMap::new()), StatusCode::OK);
        assert_eq!(status(&with("\"other\"")), StatusCode::OK);
        assert_eq!(
            status(&with(&format!("\"other\", W/{etag}"))),
            StatusCode::NOT_MODIFIED
        );
        assert_eq!(status(&with("*")), StatusCode::NOT_MODIFIED);
        let response = revalidate(&with(&etag), &etag, || StatusCode::OK.into_response());
        assert_eq!(response.headers()[header::ETAG], etag.as_str());
    }
}
