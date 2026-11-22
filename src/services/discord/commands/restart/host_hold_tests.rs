//! `/restart` on a thread whose host is not decided restarts nothing.

use super::*;
use crate::services::session_host::surface_policy::{Relation, force_relation_for_test};

// An undecided thread's restart is refused with its reason: no Herdr or tmux restart and no
// greeting seed, which the test entry would panic on.
#[tokio::test]
async fn an_undecided_threads_restart_is_refused_and_runs_no_host() {
    let (parent, child, unresolved) = (8_118_430, 8_118_431, 8_118_432);
    let _hosts =
        crate::config::session_hosts::force_for_test(Some("test-node"), &[(parent, "test-node")]);
    let _child = force_relation_for_test(child, Relation::Child { parent });
    let _held = force_relation_for_test(unresolved, Relation::Unresolved);
    let data = Data {
        shared: crate::services::discord::make_shared_data_for_tests(),
        token: "restart-hold".into(),
        provider: ProviderKind::Claude,
    };
    let http = Arc::new(serenity::Http::new(""));
    for (channel, reason) in [
        (unresolved, "스레드 부모 미확인"),
        (child, "현재 실행 미확인: no database"),
    ] {
        let identity = ("restart-hold", "!restart", 8_118_433);
        let replies =
            restart_channel_for_tests(&http, &data, serenity::ChannelId::new(channel), identity)
                .await
                .unwrap();
        assert_eq!(
            replies,
            [
                "♻ 세션 재시작 중...".to_string(),
                format!("♻ 실행 호스트를 확인하지 못해 재시작하지 않았어요({reason})"),
            ],
            "{channel}"
        );
    }
}
