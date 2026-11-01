//! A restart after a host reboot renumbered the device of a pane's verified source: the boot
//! restore re-pins it under the new device and binds it, but only in the execution that logged it.

use super::*;
use crate::services::claude_tui::source_verify::boot_time_for_tests;
use crate::services::tui_prompt_dedupe::binding_events::{
    CauseSource, Committed, Proposal, record_verified,
};
use chrono::Utc;
use std::os::unix::fs::MetadataExt;

fn identity(session: &str, path: &Path) -> SourceId {
    let meta = fs::metadata(path).unwrap();
    SourceId {
        session_id: session.to_owned(),
        path: path.to_path_buf(),
        dev: meta.dev(),
        ino: meta.ino(),
    }
}

/// S launched on `tmux` and pinned verified, then a reboot: the log names another device and the
/// process remembers nothing. Returns S, its path and a boot time after the pin.
fn rebooted(lane: &Lane, channel: u64, tmux: &str) -> (String, PathBuf, chrono::DateTime<Utc>) {
    stamp(tmux);
    let s = uuid();
    let path = lane.touch(&s);
    register_tmux_channel(tmux, channel);
    register_provider_session("claude", &s, tmux);
    register_launched_tmux_runtime_binding(tmux, claude(&path, &s));
    let shown = path.display().to_string();
    let proposal = Proposal {
        channel_id: channel,
        provider: "claude",
        tmux_session: tmux,
        session_id: Some(&s),
        path: &shown,
        replaced: None,
        cause: CauseSource::Observed,
        hook: None,
    };
    let pinned = record_verified(&proposal, &identity(&s, &path)).unwrap();
    assert!(matches!(pinned, Committed::Appended | Committed::Unchanged));
    let dev = identity(&s, &path).dev;
    let log = fs::read_to_string(lane.log(channel)).unwrap();
    let moved = log.replace(&format!("\"dev\":{dev},"), &format!("\"dev\":{},", dev + 1));
    fs::write(lane.log(channel), moved).unwrap();
    restart(channel);
    std::thread::sleep(std::time::Duration::from_millis(2));
    (s, path, Utc::now())
}

#[test]
fn a_boot_restore_re_pins_a_renumbered_source_only_in_its_own_execution() {
    let lane = Lane::new();
    for (case, channel) in [("own", 7_642), ("respawned", 7_643), ("no marker", 7_644)] {
        let tmux = format!("i7641-restore-{channel}");
        let (s, path, boot) = rebooted(&lane, channel, &tmux);
        let _boot = boot_time_for_tests(Some(boot));
        let before = pinned_source(channel, &tmux).unwrap();
        match case {
            "respawned" => drop(stamp(&tmux)),
            "no marker" => fs::remove_file(tc::session_temp_path(&tmux, "spawn_nonce")).unwrap(),
            _ => {}
        }
        let outcome = restore(channel, &tmux, &s, &path);
        let pin = pinned_source(channel, &tmux).unwrap();
        if case == "own" {
            assert_eq!(pin, Some(identity(&s, &path)), "{case}: {outcome:?}");
            assert_eq!(bound_session(&tmux).as_deref(), Some(s.as_str()), "{case}");
            restart(channel);
            assert_eq!(
                pinned_source(channel, &tmux).unwrap(),
                pin,
                "the re-pin is logged"
            );
        } else {
            assert_eq!(pin, before, "{case}: {outcome:?}");
            assert_eq!(bound_session(&tmux), None, "{case}");
        }
    }
}

#[test]
fn a_boot_restore_keeps_a_pin_renumbered_in_its_own_boot() {
    let lane = Lane::new();
    let (channel, tmux) = (7_645, "i7641-restore-same-boot");
    let (s, path, boot) = rebooted(&lane, channel, tmux);
    let _boot = boot_time_for_tests(Some(boot - chrono::Duration::hours(1)));
    let before = pinned_source(channel, tmux).unwrap();
    let outcome = restore(channel, tmux, &s, &path);
    assert!(
        matches!(outcome, PendingRestore::Anomaly { .. }),
        "{outcome:?}"
    );
    assert_eq!(pinned_source(channel, tmux).unwrap(), before);
}
