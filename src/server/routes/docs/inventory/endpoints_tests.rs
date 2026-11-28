mod part_06 {
    mod tests {
        use super::super::super::all_endpoints;
        use crate::queue_contract::THREAD_GROUP_SERIAL_LANE_CONTRACT;

        #[test]
        fn queue_generate_docs_use_canonical_thread_group_contract() {
            let endpoint = all_endpoints()
                .iter()
                .find(|endpoint| endpoint.path == "/api/queue/generate")
                .expect("queue generate endpoint");
            let param = endpoint
                .params
                .get("entries[].thread_group")
                .expect("entries[].thread_group parameter");

            assert_eq!(param.description, THREAD_GROUP_SERIAL_LANE_CONTRACT);
        }
    }
}

mod part_08 {
    mod tests {
        use super::super::super::all_endpoints;
        use crate::queue_contract::THREAD_GROUP_SERIAL_LANE_CONTRACT;

        #[test]
        fn docs_group_example_advertises_canonical_paths() {
            let response = &all_endpoints()
                .iter()
                .find(|endpoint| endpoint.path == "/api/docs/{group}")
                .expect("group docs endpoint")
                .example
                .as_ref()
                .expect("group docs example")
                .response;

            for (name, canonical_path) in [
                ("kanban", "/api/docs/kanban/kanban"),
                ("reviews", "/api/docs/kanban/reviews"),
            ] {
                let category = response["categories"]
                    .as_array()
                    .expect("group categories array")
                    .iter()
                    .find(|category| category["name"] == name)
                    .unwrap_or_else(|| panic!("missing {name} category"));
                assert_eq!(category["canonical_path"], canonical_path);
            }
        }

        #[test]
        fn add_run_entry_docs_use_canonical_thread_group_contract() {
            let endpoint = all_endpoints()
                .iter()
                .find(|endpoint| endpoint.path == "/api/queue/runs/{id}/entries")
                .expect("add run entry endpoint");
            let param = endpoint
                .params
                .get("thread_group")
                .expect("thread_group parameter");

            assert_eq!(param.description, THREAD_GROUP_SERIAL_LANE_CONTRACT);
        }
    }
}

mod public_surfaces {
    mod tests {
        #[test]
        fn root_mounted_control_surfaces_are_listed_with_their_auth_boundary() {
            let endpoints = super::super::super::all_endpoints();
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
}
