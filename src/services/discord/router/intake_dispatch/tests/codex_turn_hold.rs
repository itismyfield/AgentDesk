//! While a selected Codex channel's takeover turn still runs on Legacy, a live message waits queued
//! in the mailbox instead of starting a Legacy turn; once turn mode is confirmed it is admitted.

use super::*;
use crate::services::tui_o::channel_policy::{Adoption, BootChannels};
use crate::services::tui_o::cutover::{intake_route::test_probe, test_override};
use crate::services::tui_o::turn_mode::{self, TestConfirmation};

const CHANNEL: u64 = 4_370_001;

fn committed_and_selected() -> test_override::ChannelsGuard {
    let config = serde_json::from_value(serde_json::json!({
        "server": {},
        "agents": [{"id": "w", "name": "W",
            "channels": {"codex": {"id": CHANNEL.to_string(), "runtime": "tui"}}}],
        "tui_o": {"writer": {"channels": [CHANNEL]}, "turn": {"channels": [CHANNEL]}}
    }))
    .unwrap();
    let boot = BootChannels::validate(&config).unwrap();
    test_override::force_boot(boot.adopted(Adoption::Committed))
}

fn codex_message(message_id: u64) -> IntakeSubmission {
    IntakeSubmission {
        provider: ProviderKind::Codex,
        ..submission_for_admission(ChannelId::new(CHANNEL), message_id)
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_message_during_the_codex_takeover_turn_waits_queued_until_turn_mode() {
    let env = ScopedIntakeTestEnv::with_mode("disabled");
    let config = crate::runtime_layout::config_file_path(env._root.path());
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, "server: {}\n").unwrap();
    let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &config,
    );
    let shared = crate::services::discord::make_shared_data_for_tests();
    let http = Arc::new(serenity::Http::new("Bot intake-dispatch-test"));
    let deps = deps(&http, &shared);
    let _boot = committed_and_selected();
    let _ready = test_probe::answer_with(|_| true);

    let held = codex_message(4_370_011);
    let admission = super::super::admit_text_intake(&deps, &held).await;
    assert!(
        matches!(admission, IntakeAdmission::DeferredTurnMode),
        "{admission:?}"
    );
    super::super::finish_text_intake_admission(&deps, admission, held)
        .await
        .unwrap();
    let mailbox = crate::services::discord::mailbox_snapshot(&shared, ChannelId::new(CHANNEL));
    let mailbox = mailbox.await;
    let queued: Vec<_> = mailbox
        .intervention_queue
        .iter()
        .map(|i| i.message_id.get())
        .collect();
    assert_eq!(
        queued,
        [4_370_011],
        "the message waits in the mailbox queue"
    );

    assert!(turn_mode::confirm(CHANNEL));
    let _confirmed = TestConfirmation::confirmed(CHANNEL);
    let next = codex_message(4_370_012);
    let admission = super::super::admit_text_intake(&deps, &next).await;
    assert!(
        matches!(admission, IntakeAdmission::Local(_)),
        "{admission:?}"
    );
}
