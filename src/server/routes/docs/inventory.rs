use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

use super::taxonomy::canonical_category;

#[derive(Debug, Clone, Serialize)]
pub(super) struct ParamDoc {
    pub location: &'static str,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub required: bool,
    pub description: &'static str,
    #[serde(skip_serializing_if = "Option::is_none", rename = "enum")]
    pub enum_values: Option<Vec<&'static str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
}

impl ParamDoc {
    fn with_enum(mut self, values: &[&'static str]) -> Self {
        self.enum_values = Some(values.iter().copied().collect());
        self
    }

    fn with_default(mut self, value: impl Into<Value>) -> Self {
        self.default = Some(value.into());
        self
    }
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct ExampleDoc {
    pub request: Value,
    pub response: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scenario: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct EndpointDoc {
    pub method: &'static str,
    pub path: &'static str,
    pub category: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subcategory: Option<&'static str>,
    pub description: &'static str,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, ParamDoc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub example: Option<ExampleDoc>,
    /// #1068 (904-6) paired-scenario companion: canonical 4xx failure example.
    /// When present, surfaces alongside the happy-path `example` so callers can
    /// see both the success shape and the most common error shape at once.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_example: Option<ExampleDoc>,
    /// Successful preview-only scenario that validates and renders the response
    /// contract without performing the endpoint's external side effects.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dry_run_example: Option<ExampleDoc>,
    /// Nested operation-level failure that can still be returned with a
    /// successful HTTP status. Used for partial-success contracts where callers
    /// must inspect nested result fields instead of trusting the status code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partial_success_example: Option<ExampleDoc>,
    /// #1068 (904-6) curl 1-liner reference. Intentionally a single physical
    /// line so it can be copy-pasted directly into a terminal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub curl_example: Option<&'static str>,
    #[serde(skip_serializing_if = "is_false")]
    pub deprecated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_path: Option<&'static str>,
}

impl EndpointDoc {
    fn with_params<const N: usize>(mut self, params: [(&'static str, ParamDoc); N]) -> Self {
        self.params = params
            .into_iter()
            .map(|(name, param)| (name.to_string(), param))
            .collect();
        self
    }

    fn with_example(mut self, request: Value, response: Value) -> Self {
        self.example = Some(ExampleDoc {
            request,
            response,
            status: Some(200),
            scenario: Some("happy"),
        });
        self
    }

    /// #1068 (904-6) paired-scenario companion. Records the canonical 4xx
    /// failure response (shape + status) so callers can see both the success
    /// shape from `with_example` and the most common mistake at once.
    fn with_error_example(mut self, status: u16, request: Value, response: Value) -> Self {
        self.error_example = Some(ExampleDoc {
            request,
            response,
            status: Some(status),
            scenario: Some("error"),
        });
        self
    }

    fn with_dry_run_example(mut self, request: Value, response: Value) -> Self {
        self.dry_run_example = Some(ExampleDoc {
            request,
            response,
            status: Some(200),
            scenario: Some("dry_run"),
        });
        self
    }

    fn with_partial_success_example(mut self, request: Value, response: Value) -> Self {
        self.partial_success_example = Some(ExampleDoc {
            request,
            response,
            status: Some(200),
            scenario: Some("partial_success"),
        });
        self
    }

    /// #1068 (904-6) curl 1-liner. Must stay a single physical line so it can
    /// be copy-pasted into a terminal without escaping line breaks.
    fn with_curl(mut self, curl: &'static str) -> Self {
        self.curl_example = Some(curl);
        self
    }
}

fn ep(
    method: &'static str,
    path: &'static str,
    category: &'static str,
    description: &'static str,
) -> EndpointDoc {
    let canonical_category = canonical_category(category);
    EndpointDoc {
        method,
        path,
        category: canonical_category,
        subcategory: (canonical_category != category).then_some(category),
        description,
        params: BTreeMap::new(),
        example: None,
        error_example: None,
        dry_run_example: None,
        partial_success_example: None,
        curl_example: None,
        deprecated: false,
        canonical_path: None,
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn path_param(description: &'static str) -> ParamDoc {
    ParamDoc {
        location: "path",
        kind: "string",
        required: true,
        description,
        enum_values: None,
        default: None,
    }
}

fn header_param(kind: &'static str, required: bool, description: &'static str) -> ParamDoc {
    ParamDoc {
        location: "header",
        kind,
        required,
        description,
        enum_values: None,
        default: None,
    }
}

fn query_param(kind: &'static str, required: bool, description: &'static str) -> ParamDoc {
    ParamDoc {
        location: "query",
        kind,
        required,
        description,
        enum_values: None,
        default: None,
    }
}

fn body_param(kind: &'static str, required: bool, description: &'static str) -> ParamDoc {
    ParamDoc {
        location: "body",
        kind,
        required,
        description,
        enum_values: None,
        default: None,
    }
}

// ---------------------------------------------------------------------------
// #1068 (904-6) — top-40 paired-scenario endpoints.
//
// These are the high-traffic runtime/kanban/agent/queue/integration endpoints
// that must ship with BOTH a happy-path example AND an error example plus a
// curl 1-liner so agents can learn the contract without reading source.
//
// The test `api_docs_exposes_paired_examples_for_top_40` iterates this slice
// and asserts `example`, `error_example`, and `curl_example` are all populated.
// Endpoints not in this list may rely on `// TODO: example` markers in their
// description text while paired coverage expands in follow-up issues.
// ---------------------------------------------------------------------------
// reason: fixture table asserted by the `#[cfg(test)]` api_docs paired-coverage
// test; it has no production reader, so the lib build flags it as dead. See #3034.
#[allow(dead_code)]
pub(crate) const TOP_40_PAIRED_PATHS: &[(&str, &str)] = &[
    ("GET", "/api/health"),
    ("POST", "/api/discord/send"),
    ("POST", "/api/discord/send-to-agent"),
    ("POST", "/api/discord/send-dm"),
    ("GET", "/api/agents"),
    ("POST", "/api/agents"),
    ("GET", "/api/agents/{id}"),
    ("PATCH", "/api/agents/{id}"),
    ("POST", "/api/agents/setup"),
    ("POST", "/api/agents/{id}/handoff"),
    ("POST", "/api/agents/{id}/turn/start"),
    ("POST", "/api/agents/{id}/turn/stop"),
    ("GET", "/api/agents/{id}/quality"),
    ("GET", "/api/agents/quality/ranking"),
    ("GET", "/api/kanban-cards"),
    ("POST", "/api/kanban-cards"),
    ("GET", "/api/kanban-cards/{id}"),
    ("PATCH", "/api/kanban-cards/{id}"),
    ("POST", "/api/kanban-cards/{id}/assign"),
    ("POST", "/api/kanban-cards/{id}/transition"),
    ("POST", "/api/kanban-cards/{id}/retry"),
    ("POST", "/api/kanban-cards/{id}/redispatch"),
    ("POST", "/api/kanban-cards/{id}/resume"),
    ("POST", "/api/kanban-cards/{id}/reopen"),
    ("POST", "/api/kanban-cards/assign-issue"),
    ("GET", "/api/dispatches"),
    ("POST", "/api/dispatches"),
    ("GET", "/api/dispatches/{id}"),
    ("GET", "/api/dispatches/{id}/events"),
    ("GET", "/api/dispatches/delivery-events/reconcile-stats"),
    ("PATCH", "/api/dispatches/{id}"),
    ("POST", "/api/queue/generate"),
    ("POST", "/api/queue/dispatch-next"),
    ("GET", "/api/queue/status"),
    ("POST", "/api/queue/pause"),
    ("POST", "/api/queue/resume"),
    ("POST", "/api/queue/runs/{id}/phase-gates/repair"),
    ("POST", "/api/queue/cancel"),
    ("PATCH", "/api/queue/reorder"),
    ("GET", "/api/queue/phase-gates/catalog"),
    ("POST", "/api/queue/request-generate"),
    ("POST", "/api/github/issues/create"),
    ("GET", "/api/pipeline/cards/{card_id}"),
    ("GET", "/api/analytics/observability"),
    ("GET", "/api/analytics/invariants"),
    ("POST", "/api/reviews/verdict"),
    ("GET", "/api/docs"),
];

mod endpoints;

pub(super) fn all_endpoints() -> Vec<EndpointDoc> {
    endpoints::all()
}
