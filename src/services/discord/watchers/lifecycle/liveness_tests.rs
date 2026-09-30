//! The watcher's liveness answer and its death handler against local and stored host evidence.

use std::path::Path;
use std::sync::Arc;

use poise::serenity_prelude::{self as serenity, ChannelId};

use super::*;
use crate::services::discord::host_teardown_gate::test_support::{
    Stored, channel_key, seed, shared_on,
};
use crate::services::provider::ProviderKind;
use crate::services::session_host::test_support::InjectedLivenessGuard;
use crate::services::session_host::{HostLiveness, HostSessionRef};
use crate::services::tmux_common::{session_dead_marker_path, session_temp_path};

fn write(path: &str, text: &str) {
    std::fs::create_dir_all(Path::new(path).parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

// Only a pane tmux confirms dead, or the wrapper's `.pane_dead` after a failed probe, reads
// dead. A session whose marker names another host is never probed and keeps its files.
#[tokio::test]
async fn watcher_probe_reads_dead_only_on_a_confirmed_tmux_death() {
    use HostLiveness::{DeadOrAbsent, Live, ProbeError};
    let _root = crate::config::TestRuntimeRootGuard::new();
    // (`.host_kind`, injected pane, `.pane_dead` present, alive, `.pane_dead` left)
    let cases = [
        (None, Live, false, true, false),
        (None, DeadOrAbsent, false, false, false),
        (None, ProbeError, false, true, false),
        (None, ProbeError, true, false, true),
        (None, Live, true, true, false),
        (Some("tmux"), DeadOrAbsent, false, false, false),
        (Some("herdr"), DeadOrAbsent, true, true, true),
        (Some("zellij"), DeadOrAbsent, false, true, false),
    ];
    for (n, (host, pane, dead_marker, alive, marker_left)) in cases.into_iter().enumerate() {
        let name = format!("AgentDesk-claude-p4b1-watch-{n}");
        let _pane = InjectedLivenessGuard::set(HostSessionRef::tmux(&name), pane);
        if let Some(host) = host {
            write(&session_temp_path(&name, "host_kind"), host);
        }
        let dead_path = session_dead_marker_path(&name);
        if dead_marker {
            write(&dead_path, "");
        }
        let label = format!("{n}: {host:?} {pane:?} pane_dead={dead_marker}");
        assert_eq!(probe_tmux_session_liveness(&name).await, alive, "{label}");
        assert_eq!(Path::new(&dead_path).exists(), marker_left, "{label}");
    }
}

// A watcher-observed death reads the stored rows first: only a found legacy row, or no row
// with no other-host trace, consumes the cancel record the death is matched against.
#[tokio::test]
async fn watcher_death_takes_the_host_guard_before_consuming_the_cancel_record_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared = shared_on(&pool).await;
    let http = Arc::new(serenity::Http::new("Bot test-token"));
    let provider = ProviderKind::Claude;
    for (n, stored) in Stored::ALL.into_iter().enumerate() {
        let channel = ChannelId::new(1_479_671_301_387_070_000 + n as u64);
        let name = provider.build_tmux_session_name(&format!("p4b1-death-{n}"));
        seed(
            &pool,
            &channel_key(&shared, &name),
            &name,
            channel.get(),
            stored,
        )
        .await;
        crate::services::discord::tmux::record_recent_turn_stop(channel, Some(&name), "p4b1").await;
        let output = "/nonexistent/p4b1-death.jsonl";
        handle_tmux_watcher_observed_death(
            channel, &http, &shared, &name, output, &provider, false, false,
        )
        .await;
        let kept = crate::services::discord::tmux::recent_turn_stop_for_channel(channel);
        let admitted = matches!(stored, Stored::Legacy | Stored::Missing);
        assert_eq!(kept.is_none(), admitted, "{stored:?}");
    }
    pool.close().await;
    db.drop().await;
}
