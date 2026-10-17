use super::super::{EndpointDoc, ep, query_param};

pub(super) fn endpoints() -> Vec<EndpointDoc> {
    vec![
        ep(
            "GET",
            "/ws",
            "auth",
            "Mounted when the dashboard is enabled. Dashboard WebSocket upgrade. A browser may use a one-use `ticket` query parameter obtained from `POST /api/auth/ws-ticket`; native clients may use `Authorization: Bearer <server.auth_token>`. When `server.auth_token` is unset, tokenless upgrades are allowed subject to the configured peer filter.",
        )
        .with_params([
            (
                "ticket",
                query_param(
                    "string",
                    false,
                    "One-use, 15-second WebSocket ticket bound to the request Origin and Host; obtain it from POST /api/auth/ws-ticket.",
                ),
            ),
            (
                "since",
                query_param("string", false, "Replay WebSocket events after this event id."),
            ),
        ]),
        ep(
            "POST",
            "/tui/send",
            "auth",
            "Send user input to the active TUI session. Only loopback peers or a valid configured Bearer token are accepted; when `server.auth_token` is unset, loopback is required.",
        ),
        ep(
            "POST",
            "/tui/wait",
            "auth",
            "Wait for the active TUI session's next event. Only loopback peers or a valid configured Bearer token are accepted; when `server.auth_token` is unset, loopback is required.",
        ),
        ep(
            "POST",
            "/hooks/{provider}/{event}",
            "auth",
            "Mounted when the provider hook receiver is configured. Receive a provider lifecycle hook. Only loopback peers or a valid configured Bearer token are accepted; when `server.auth_token` is unset, loopback is required.",
        )
        .with_params([
            ("provider", super::super::path_param("Provider name, such as claude or codex.")),
            ("event", super::super::path_param("Provider lifecycle event name.")),
            (
                "session_id",
                query_param("string", false, "Provider session identifier."),
            ),
        ]),
    ]
}

#[cfg(test)]
mod tests {
    #[test]
    fn root_mounted_control_surfaces_are_listed_with_their_auth_boundary() {
        let endpoints = super::super::all();
        for (method, path, required_phrases) in [
            (
                "GET",
                "/ws",
                &[
                    "Mounted when the dashboard is enabled",
                    "one-use `ticket` query parameter",
                    "Authorization: Bearer",
                    "configured peer filter",
                ][..],
            ),
            (
                "POST",
                "/tui/send",
                &[
                    "Only loopback peers",
                    "valid configured Bearer token",
                    "when `server.auth_token` is unset, loopback is required",
                    "Send user input",
                ][..],
            ),
            (
                "POST",
                "/tui/wait",
                &[
                    "Only loopback peers",
                    "valid configured Bearer token",
                    "when `server.auth_token` is unset, loopback is required",
                    "Wait for the active TUI session",
                ][..],
            ),
            (
                "POST",
                "/hooks/{provider}/{event}",
                &[
                    "Mounted when the provider hook receiver is configured",
                    "Only loopback peers",
                    "valid configured Bearer token",
                    "when `server.auth_token` is unset, loopback is required",
                    "Receive a provider lifecycle hook",
                ][..],
            ),
        ] {
            let endpoint = endpoints
                .iter()
                .find(|endpoint| endpoint.method == method && endpoint.path == path)
                .unwrap_or_else(|| panic!("missing {method} {path}"));
            for phrase in required_phrases {
                assert!(
                    endpoint.description.contains(phrase),
                    "{method} {path} description missing {phrase:?}"
                );
            }
            if path == "/ws" {
                let ticket = endpoint
                    .params
                    .get("ticket")
                    .expect("missing ticket parameter");
                assert!(ticket.description.contains("One-use, 15-second"));
                assert!(
                    ticket
                        .description
                        .contains("bound to the request Origin and Host")
                );
            }
        }
    }
}
