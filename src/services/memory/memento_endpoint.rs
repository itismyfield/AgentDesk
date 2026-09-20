//! Endpoint normalization shared by Memento session initialization and tool calls.

const MEMENTO_MCP_PATH: &str = "/mcp";

pub(super) fn normalize_memento_endpoint(endpoint: &str) -> String {
    let trimmed = endpoint.trim().trim_end_matches('/');
    trimmed
        .strip_suffix(MEMENTO_MCP_PATH)
        .unwrap_or(trimmed)
        .to_string()
}

pub(super) fn mcp_url(endpoint: &str) -> String {
    format!(
        "{}{}",
        normalize_memento_endpoint(endpoint),
        MEMENTO_MCP_PATH
    )
}
