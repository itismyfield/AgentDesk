use serde_json::{Value, json};

use super::{
    inventory::EndpointDoc,
    taxonomy::{
        GROUP_NAMES, categories_for_group, category_description, category_to_group,
        effective_category, group_description,
    },
};

pub(super) fn resolve(endpoints: &[EndpointDoc], segment: &str, flat: bool) -> Option<Value> {
    if !GROUP_NAMES.contains(&segment) {
        return None;
    }
    if flat {
        let matching: Vec<&EndpointDoc> = endpoints
            .iter()
            .filter(|endpoint| category_to_group(effective_category(endpoint)) == segment)
            .collect();
        return Some(json!({ "endpoints": matching }));
    }

    let categories: Vec<Value> = categories_for_group(endpoints, segment)
        .into_iter()
        .map(|(name, count)| {
            json!({
                "name": name,
                "description": category_description(name),
                "endpoint_count": count,
                "canonical_path": format!("/api/docs/{segment}/{name}"),
            })
        })
        .collect();
    let mut body = json!({
        "group": segment,
        "description": group_description(segment),
        "categories": categories,
    });
    if endpoints
        .iter()
        .any(|endpoint| endpoint.category == segment || effective_category(endpoint) == segment)
    {
        let canonical_path = format!("/api/docs/{}/{segment}", category_to_group(segment));
        body["name_collision"] = json!({
            "resolved_as": "group",
            "category": segment,
            "canonical_path": canonical_path,
            "next_request": format!("GET {canonical_path}"),
        });
    }
    Some(body)
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::super::tests::{docs_router, request_docs_json};
    use super::*;

    #[tokio::test]
    async fn colliding_group_paths_advertise_resolvable_category_details() {
        let router = docs_router();
        for (segment, category_path) in [
            ("kanban", "/api/docs/kanban/kanban"),
            ("agents", "/api/docs/agents/agents"),
            ("integrations", "/api/docs/internal/integrations"),
            ("internal", "/api/docs/internal/internal"),
        ] {
            let (status, body) = request_docs_json(&router, &format!("/api/docs/{segment}")).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["group"], segment);
            assert!(body["categories"].is_array());
            assert_eq!(body["name_collision"]["resolved_as"], "group");
            assert_eq!(body["name_collision"]["category"], segment);
            assert_eq!(body["name_collision"]["canonical_path"], category_path);
            assert_eq!(
                body["name_collision"]["next_request"],
                format!("GET {category_path}")
            );

            let (status, detail) = request_docs_json(&router, category_path).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(detail["category"], segment);
            assert!(
                !detail["endpoints"]
                    .as_array()
                    .expect("category endpoints")
                    .is_empty()
            );
        }

        for group in ["runtime", "automation", "config", "observability"] {
            let (status, body) = request_docs_json(&router, &format!("/api/docs/{group}")).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body.as_object().expect("group body").len(), 3);
            assert!(body.get("name_collision").is_none());
        }
    }

    #[tokio::test]
    async fn kanban_category_docs_include_single_card_get() {
        let (status, body) = request_docs_json(&docs_router(), "/api/docs/kanban/kanban").await;
        assert_eq!(status, StatusCode::OK);
        let cards: Vec<&Value> = body["endpoints"]
            .as_array()
            .expect("kanban endpoints")
            .iter()
            .filter(|endpoint| {
                endpoint["method"] == "GET" && endpoint["path"] == "/api/kanban-cards/{id}"
            })
            .collect();
        assert_eq!(
            cards.len(),
            1,
            "kanban docs must list the single-card GET exactly once"
        );
        assert_eq!(cards[0]["params"]["id"]["location"], "path");
        assert_eq!(cards[0]["example"]["response"]["card"]["id"], "card-1");
        assert!(
            cards[0]["curl_example"]
                .as_str()
                .expect("single-card curl")
                .contains("/api/kanban-cards/card-1")
        );

        let (status, axum::Json(query_body)) =
            super::super::api_docs(axum::extract::Query(super::super::ApiDocsQuery {
                category: Some("kanban".to_owned()),
                ..Default::default()
            }))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            query_body["endpoints"]
                .as_array()
                .expect("query endpoints")
                .contains(cards[0])
        );
    }
}
