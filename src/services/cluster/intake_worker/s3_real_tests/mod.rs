//! S3 acceptance: the real holder drain, two-node intake and force consumers over one PG, each
//! node a child test process booted the way its provider runtime boots.

#[path = "drain_pg_tests.rs"]
mod drain_pg;
#[path = "force_pg_tests.rs"]
mod force_pg;
#[path = "harness_tests.rs"]
mod harness;
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
            "th8_holder" => route_pg::holder_node(&env).await,
            "th8_gateway" => route_pg::gateway_node(&env).await,
            "th8_branches" => route_pg::branches_node(&env).await,
            "th8_moves" => route_pg::moves_node(&env).await,
            "th8_select_moves" => route_pg::select_moves_node(&env).await,
            "th8_candidate" => route_pg::candidate_node(&env).await,
            "th9_holder" => force_pg::holder_node(&env).await,
            "th9_consumer" => force_pg::consumer_node(&env).await,
            other => panic!("unknown S3 scenario {other}"),
        }
    });
    env.finished();
}
