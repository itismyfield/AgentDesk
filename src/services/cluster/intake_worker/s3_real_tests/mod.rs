//! S3 acceptance: the real holder drain, two-node intake and force consumers over one PG, each
//! node a child test process booted the way its provider runtime boots.

#[path = "drain_pg_tests.rs"]
mod drain_pg;
#[path = "force_pg_tests.rs"]
mod force_pg;
#[path = "harness_tests.rs"]
mod harness;
#[path = "holder_pg_tests.rs"]
mod holder_pg;
#[path = "route_pg_tests.rs"]
mod route_pg;

/// The one child entry every scenario re-executes; outside a parent's spawn it has nothing to run.
#[test]
#[ignore = "child node of the S3 two-node harness; its parent test runs it by exact name"]
fn s3_node_child() {
    let Some(env) = harness::Env::read() else {
        return;
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        match env.scenario.as_str() {
            "th7_tail" => drain_pg::tail_node(&env).await,
            "th7_torn" => drain_pg::torn_node(&env).await,
            "th7_last_post" => drain_pg::last_post_node(&env).await,
            "th7_recheck" => drain_pg::recheck_node(&env).await,
            "th7_unknown" => drain_pg::unknown_node(&env).await,
            "th7_release" => drain_pg::release_node(&env).await,
            "th7_running_post" => drain_pg::running_post_node(&env).await,
            "th7_posts_unreadable" => drain_pg::posts_unreadable_node(&env).await,
            "th7_unsealed" => drain_pg::unsealed_node(&env).await,
            "th7_binding" => drain_pg::binding_node(&env).await,
            "th7_prepared_seed" => drain_pg::prepared_seed_node(&env).await,
            "th7_prepared" => drain_pg::prepared_node(&env).await,
            "th7_unsent_seed" => drain_pg::unsent_seed_node(&env).await,
            "th7_settles_not_found" => drain_pg::settles_not_found_node(&env).await,
            "th7_settles_unresolved" => drain_pg::settles_unresolved_node(&env).await,
            "th7_waiting_boundary" => drain_pg::waiting_boundary_node(&env).await,
            "th7_first_check" => drain_pg::first_check_node(&env).await,
            "th6_restart_first" => holder_pg::first_generation_node(&env).await,
            "th6_restart_second" => holder_pg::second_generation_node(&env).await,
            "th6_role_holder" => holder_pg::role_holder_node(&env).await,
            "th6_role_gateway" => holder_pg::role_gateway_node(&env).await,
            "th6_both_holder" => holder_pg::both_holder_node(&env).await,
            "th6_both_gateway" => holder_pg::both_gateway_node(&env).await,
            "th8_holder" => route_pg::holder_node(&env).await,
            "th8_gateway" => route_pg::gateway_node(&env).await,
            "th8_branches" => route_pg::branches_node(&env).await,
            "th8_moves" => route_pg::moves_node(&env).await,
            "th8_select_moves" => route_pg::select_moves_node(&env).await,
            "th8_candidate" => route_pg::candidate_node(&env).await,
            "th8_home_recheck" => route_pg::home_recheck_node(&env).await,
            "th9_holder" => force_pg::holder_node(&env).await,
            "th9_consumer" => force_pg::consumer_node(&env).await,
            "th9_owed_holder" => force_pg::owed_holder_node(&env).await,
            "th6_live_target" => force_pg::live_target_node(&env).await,
            "th9_seed_prepared" => force_pg::seed_prepared_node(&env).await,
            "th9_new_target" => force_pg::new_target_node(&env).await,
            other => panic!("unknown S3 scenario {other}"),
        }
    });
    env.finished();
}
