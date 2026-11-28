use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::LazyLock,
};

use super::taxonomy::{canonical_category, effective_category};
use crate::queue_contract::THREAD_GROUP_SERIAL_LANE_CONTRACT;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ParamDoc {
    pub location: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub required: bool,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none", rename = "enum")]
    pub enum_values: Option<Vec<String>>,
    #[serde(
        default,
        deserialize_with = "deserialize_param_default",
        skip_serializing_if = "Option::is_none"
    )]
    pub default: Option<Value>,
}

fn deserialize_param_default<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ExampleDoc {
    pub request: Value,
    pub response: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scenario: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EndpointDoc {
    pub method: String,
    pub path: String,
    pub category: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subcategory: Option<String>,
    pub description: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, ParamDoc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub example: Option<ExampleDoc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_example: Option<ExampleDoc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dry_run_example: Option<ExampleDoc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partial_success_example: Option<ExampleDoc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub curl_example: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub deprecated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_path: Option<String>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

static ENDPOINTS: LazyLock<Vec<EndpointDoc>> = LazyLock::new(|| {
    let endpoints: Vec<EndpointDoc> =
        serde_json::from_str(include_str!("../../../../docs/data/api-endpoints.json"))
            .unwrap_or_else(|error| panic!("invalid API endpoint inventory JSON: {error}"));
    validate_inventory(&endpoints);
    endpoints
});

fn validate_inventory(endpoints: &[EndpointDoc]) {
    assert!(!endpoints.is_empty(), "empty API endpoint inventory");
    let mut routes = BTreeSet::new();
    for endpoint in endpoints {
        let route = (&endpoint.method, &endpoint.path);
        assert!(
            !endpoint.method.is_empty() && endpoint.path.starts_with('/'),
            "invalid API endpoint route: {route:?}"
        );
        assert!(
            routes.insert(route),
            "duplicate API endpoint route: {route:?}"
        );
        assert_eq!(
            endpoint.category,
            canonical_category(effective_category(endpoint)),
            "invalid API endpoint category for {route:?}"
        );

        let thread_group_param = match (endpoint.method.as_str(), endpoint.path.as_str()) {
            ("POST", "/api/queue/generate") => "entries[].thread_group",
            ("POST", "/api/queue/runs/{id}/entries") => "thread_group",
            _ => continue,
        };
        assert_eq!(
            endpoint
                .params
                .get(thread_group_param)
                .map(|param| param.description.as_str()),
            Some(THREAD_GROUP_SERIAL_LANE_CONTRACT),
            "invalid thread group contract for {route:?}"
        );
    }
}

pub(super) fn all_endpoints() -> &'static [EndpointDoc] {
    &ENDPOINTS
}

#[cfg(test)]
#[path = "inventory/endpoints_tests.rs"]
mod endpoints;
