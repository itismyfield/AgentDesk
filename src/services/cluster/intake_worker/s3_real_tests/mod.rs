//! S3 acceptance: the real holder drain, two-node intake and force consumers over one PG, each
//! node a child test process booted the way its provider runtime boots.

#[path = "drain_pg_tests.rs"]
mod drain_pg;
#[path = "force_pg_tests.rs"]
mod force_pg;
#[path = "fresh_drain_pg_tests.rs"]
mod fresh_drain_pg;
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
    // Keep the selected scenario state on the heap before runtime entry.
    let node: futures::future::LocalBoxFuture<'_, ()> = match env.scenario.as_str() {
        "th7_tail" => Box::pin(drain_pg::tail_node(&env)),
        "th7_torn" => Box::pin(drain_pg::torn_node(&env)),
        "th7_last_post" => Box::pin(drain_pg::last_post_node(&env)),
        "th7_recheck" => Box::pin(drain_pg::recheck_node(&env)),
        "th7_unknown" => Box::pin(drain_pg::unknown_node(&env)),
        "th7_release" => Box::pin(drain_pg::release_node(&env)),
        "th7_running_post" => Box::pin(drain_pg::running_post_node(&env)),
        "th7_posts_unreadable" => Box::pin(drain_pg::posts_unreadable_node(&env)),
        "th7_unsealed" => Box::pin(drain_pg::unsealed_node(&env)),
        "th7_binding" => Box::pin(drain_pg::binding_node(&env)),
        "th7_prepared_seed" => Box::pin(drain_pg::prepared_seed_node(&env)),
        "th7_prepared" => Box::pin(drain_pg::prepared_node(&env)),
        "th7_unsent_seed" => Box::pin(drain_pg::unsent_seed_node(&env)),
        "th7_settles_not_found" => Box::pin(drain_pg::settles_not_found_node(&env)),
        "th7_settles_unresolved" => Box::pin(drain_pg::settles_unresolved_node(&env)),
        "th7_waiting_boundary" => Box::pin(drain_pg::waiting_boundary_node(&env)),
        "th7_first_check" => Box::pin(drain_pg::first_check_node(&env)),
        "th7_fresh_unhosted" => Box::pin(fresh_drain_pg::fresh_unhosted_node(&env)),
        "th7_fresh_parked" => Box::pin(fresh_drain_pg::fresh_parked_node(&env)),
        "th7_store_without_actor" => Box::pin(fresh_drain_pg::store_without_actor_node(&env)),
        "th6_restart_first" => Box::pin(holder_pg::first_generation_node(&env)),
        "th6_restart_second" => Box::pin(holder_pg::second_generation_node(&env)),
        "th6_release_first" => Box::pin(holder_pg::release_first_node(&env)),
        "th6_release_second" => Box::pin(holder_pg::release_second_node(&env)),
        "th6_role_holder" => Box::pin(holder_pg::role_holder_node(&env)),
        "th6_role_gateway" => Box::pin(holder_pg::role_gateway_node(&env)),
        "th6_both_holder" => Box::pin(holder_pg::both_holder_node(&env)),
        "th6_both_gateway" => Box::pin(holder_pg::both_gateway_node(&env)),
        "th6_return_gateway" => Box::pin(holder_pg::return_gateway_node(&env)),
        "th6_return_worker" => Box::pin(holder_pg::return_worker_node(&env)),
        "th6_first_gateway_before" => Box::pin(holder_pg::first_gateway_before_node(&env)),
        "th6_first_gateway" => Box::pin(holder_pg::first_gateway_node(&env)),
        "th6_first_worker" => Box::pin(holder_pg::first_worker_node(&env)),
        "th6_adopt_target" => Box::pin(holder_pg::adopt_target_node(&env)),
        "th8_holder" => Box::pin(route_pg::holder_node(&env)),
        "th8_gateway" => Box::pin(route_pg::gateway_node(&env)),
        "th8_branches" => Box::pin(route_pg::branches_node(&env)),
        "th8_moves" => Box::pin(route_pg::moves_node(&env)),
        "th8_select_moves" => Box::pin(route_pg::select_moves_node(&env)),
        "th8_candidate" => Box::pin(route_pg::candidate_node(&env)),
        "th8_home_recheck" => Box::pin(route_pg::home_recheck_node(&env)),
        "th9_holder" => Box::pin(force_pg::holder_node(&env)),
        "th9_consumer" => Box::pin(force_pg::consumer_node(&env)),
        "th9_owed_holder" => Box::pin(force_pg::owed_holder_node(&env)),
        "th6_live_target" => Box::pin(force_pg::live_target_node(&env)),
        "th9_seed_prepared" => Box::pin(force_pg::seed_prepared_node(&env)),
        "th9_new_target" => Box::pin(force_pg::new_target_node(&env)),
        "th9w_holder" => Box::pin(force_pg::h9w_holder_node(&env)),
        "lifecycle_writer" => Box::pin(harness::lifecycle_writer_node(&env)),
        other => panic!("unknown S3 scenario {other}"),
    };
    runtime.block_on(node);
    env.finished();
}
