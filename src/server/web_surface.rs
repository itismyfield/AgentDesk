//! Browser entry points for both runtime profiles. Runner guidance is embedded
//! in the binary and does not provision or serve the full dashboard assets.

use std::path::Path;

use axum::{Router, http::header, response::Html, routing::get};

use super::{dashboard_auth::DashboardAccess, dashboard_provision, routes, ws};

mod peer_filter;

pub(super) fn router(
    state: routes::AppState,
    dashboard_dir: &Path,
    include_hook_receiver: bool,
) -> Router {
    let dashboard_enabled = state.config.cluster.runtime_profile.modules().dashboard;
    let peer_filter = peer_filter::PeerFilter::new(state.config.server.peer_filter);
    let access = DashboardAccess::new(&state.config);
    let mut app = Router::new();
    if dashboard_enabled {
        app = app.route(
            "/ws",
            get(ws::ws_handler).with_state((state.broadcast_tx.clone(), access.clone())),
        );
    }
    app = app.nest(
        "/api",
        routes::api_router_with_dashboard_access(state.clone(), access),
    );
    if include_hook_receiver {
        app = app.merge(
            crate::services::claude_tui::hook_server::hook_receiver_router().route_layer(
                axum::middleware::from_fn_with_state(state.clone(), routes::auth::auth_middleware),
            ),
        );
    }
    // The event-driven TUI wait path is needed even before hook publication.
    // Apply auth only to matched routes so unknown paths retain their 404.
    app = app.merge(
        crate::services::claude_tui::tui_relay::router().route_layer(
            axum::middleware::from_fn_with_state(state, routes::auth::auth_middleware),
        ),
    );
    let app = if dashboard_enabled {
        dashboard_provision::serve_dashboard(app, dashboard_dir, true)
    } else {
        app.route("/", get(runner_entry))
            .route("/settings", get(runner_entry))
    };
    // Outermost, so static files, fallbacks, /ws and hook routes are filtered too.
    peer_filter.wrap(app)
}

async fn runner_entry() -> impl axum::response::IntoResponse {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(include_str!("../../assets/runner-entry.html")),
    )
}

#[cfg(test)]
mod tests;
