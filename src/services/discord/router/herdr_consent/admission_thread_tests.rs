//! A thread's input asks its parent's consent only when it runs on Herdr now.

use super::*;
use crate::services::session_host::surface_policy::{Relation, force_relation_for_test};

// W2-1/W2-4: a thread under a local user_default Herdr parent whose input does not run on Herdr
// (no Bound row: a tmux thread, nothing running, or an undecided one, which its turn host holds)
// is not assessed: no server demand, no consent row or card. The parent itself still is.
#[tokio::test(flavor = "current_thread")]
async fn a_thread_input_not_on_herdr_asks_no_parent_consent() {
    let f = Fixture::new("disabled").await;
    let parent = 8_118_440;
    let _local = session_hosts::force_for_test(Some("node"), &[(parent, "node")])
        .with_mode(EndpointMode::UserDefault);
    let _thread = force_relation_for_test(
        discord_mock::CHANNEL_ID,
        Relation::Child { parent },
    );
    let before = ensured_on_this_thread();
    assert!(
        check(&f.deps(), &ProviderKind::Claude, &request(8_118_441))
            .await
            .is_none()
    );
    assert_eq!(ensured_on_this_thread(), before, "the parent's server is not demanded");
    let mut parent_input = request(8_118_442);
    parent_input.channel_id = ChannelId::new(parent);
    assert!(
        check(&f.deps(), &ProviderKind::Claude, &parent_input)
            .await
            .is_some(),
        "the parent's own input is assessed"
    );
}
