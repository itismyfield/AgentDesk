use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

struct FakeContext {
    root: tempfile::TempDir,
    authorized: bool,
    refused: bool,
    kind: Option<RuntimeHandoffKind>,
    global: bool,
    fences: AtomicUsize,
    replies: Mutex<Vec<(String, bool)>>,
}

impl FakeContext {
    fn new(kind: Option<RuntimeHandoffKind>) -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
            authorized: true,
            refused: false,
            kind,
            global: true,
            fences: AtomicUsize::new(0),
            replies: Mutex::new(Vec::new()),
        }
    }
}

impl CommandContext for FakeContext {
    async fn authorized(&self) -> bool {
        self.authorized
    }
    fn admit_home(&self) -> Result<Option<CommandPermit>, String> {
        self.fences.fetch_add(1, Ordering::SeqCst);
        if self.refused {
            Err("다른 노드가 맡고 있어요".into())
        } else {
            Ok(None)
        }
    }
    fn channel(&self) -> u64 {
        123
    }
    fn user(&self) -> u64 {
        456
    }
    fn kind(&self) -> Option<RuntimeHandoffKind> {
        self.kind
    }
    fn root(&self) -> Option<PathBuf> {
        Some(self.root.path().to_path_buf())
    }
    fn global_enabled(&self) -> bool {
        self.global
    }
    async fn reply(&self, content: String, ephemeral: bool) -> Result<(), Error> {
        self.replies.lock().unwrap().push((content, ephemeral));
        Ok(())
    }
}

#[tokio::test]
async fn auth_rejection_has_no_reply_or_home_check_or_files() {
    let mut ctx = FakeContext::new(Some(RuntimeHandoffKind::ClaudeTui));
    ctx.authorized = false;
    for action in [
        ToolCallAction::On,
        ToolCallAction::Off,
        ToolCallAction::Status,
    ] {
        run(&ctx, action).await.unwrap();
    }
    assert!(ctx.replies.lock().unwrap().is_empty());
    assert_eq!(ctx.fences.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read_dir(ctx.root.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn wrong_home_refuses_mutations_but_status_is_ephemeral_and_read_only() {
    let mut ctx = FakeContext::new(Some(RuntimeHandoffKind::ClaudeTui));
    ctx.refused = true;
    for action in [
        ToolCallAction::On,
        ToolCallAction::Off,
        ToolCallAction::Status,
    ] {
        run(&ctx, action).await.unwrap();
    }
    let replies = ctx.replies.lock().unwrap();
    assert_eq!(
        replies
            .iter()
            .map(|(_, ephemeral)| *ephemeral)
            .collect::<Vec<_>>(),
        [false, false, true]
    );
    assert!(replies[2].0.contains("기본값"));
    assert_eq!(ctx.fences.load(Ordering::SeqCst), 2);
    assert_eq!(std::fs::read_dir(ctx.root.path()).unwrap().count(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn home_permit_stays_live_through_persistence_and_reply_while_intake_drains() {
    use crate::db::o_channel_homes::HomeState;
    use crate::services::cluster::channel_home::{self, HomeGate};

    struct AdmittedContext {
        base: FakeContext,
        home: std::sync::Arc<HomeGate>,
    }
    impl CommandContext for AdmittedContext {
        async fn authorized(&self) -> bool {
            self.base.authorized().await
        }
        fn admit_home(&self) -> Result<Option<CommandPermit>, String> {
            let permit = super::super::control::home_fence::admit(
                poise::serenity_prelude::ChannelId::new(self.channel()),
                "tool_call",
            )
            .map_err(|error| error.to_string())?;
            assert_eq!(self.home.commands_in_flight(), 1);
            self.home.close_intake();
            Ok(permit)
        }
        fn channel(&self) -> u64 {
            584_639
        }
        fn user(&self) -> u64 {
            self.base.user()
        }
        fn kind(&self) -> Option<RuntimeHandoffKind> {
            self.base.kind()
        }
        fn root(&self) -> Option<PathBuf> {
            assert!(!self.home.intake_open(), "drain began after admission");
            assert_eq!(
                self.home.commands_in_flight(),
                1,
                "admitted write blocks drain completion"
            );
            self.base.root()
        }
        fn global_enabled(&self) -> bool {
            self.base.global_enabled()
        }
        async fn reply(&self, content: String, ephemeral: bool) -> Result<(), Error> {
            assert_eq!(self.home.commands_in_flight(), 1);
            assert!(
                !tool_card_switch::status(self.base.root.path(), self.channel())
                    .unwrap()
                    .unwrap()
                    .enabled
            );
            tokio::task::yield_now().await;
            assert_eq!(
                self.home.commands_in_flight(),
                1,
                "permit survives the reply future"
            );
            self.base.reply(content, ephemeral).await
        }
    }
    let home = channel_home::register_for_test(584_639, Some(HomeState::Worker));
    let ctx = AdmittedContext {
        base: FakeContext::new(Some(RuntimeHandoffKind::ClaudeTui)),
        home: home.clone(),
    };
    run(&ctx, ToolCallAction::Off).await.unwrap();
    assert_eq!(
        home.commands_in_flight(),
        0,
        "only completion releases the admitted command"
    );
    assert!(!ctx.base.replies.lock().unwrap()[0].1);
    assert!(
        channel_home::admit_command("584639", "tool_call").is_err(),
        "draining still rejects a new command"
    );
    channel_home::unregister("584639");
}

#[tokio::test]
async fn non_o_channel_creates_no_hidden_override() {
    let ctx = FakeContext::new(None);
    for action in [
        ToolCallAction::On,
        ToolCallAction::Off,
        ToolCallAction::Status,
    ] {
        run(&ctx, action).await.unwrap();
    }
    assert_eq!(std::fs::read_dir(ctx.root.path()).unwrap().count(), 0);
    assert!(
        ctx.replies
            .lock()
            .unwrap()
            .iter()
            .all(|(text, _)| text.contains("O 출력 채널이 아니라"))
    );
}

#[tokio::test]
async fn claude_and_codex_save_public_toggles_and_report_private_status_metadata() {
    for kind in [RuntimeHandoffKind::ClaudeTui, RuntimeHandoffKind::CodexTui] {
        let ctx = FakeContext::new(Some(kind));
        run(&ctx, ToolCallAction::Off).await.unwrap();
        let stored = tool_card_switch::status(ctx.root.path(), 123)
            .unwrap()
            .unwrap();
        assert!(!stored.enabled);
        assert_eq!(stored.by, 456);
        run(&ctx, ToolCallAction::Status).await.unwrap();
        run(&ctx, ToolCallAction::On).await.unwrap();
        let replies = ctx.replies.lock().unwrap();
        assert!(!replies[0].1);
        assert!(replies[0].0.contains("진행 중인 묶음"));
        assert!(replies[1].1);
        assert!(replies[1].0.contains("채널 설정"));
        assert!(replies[1].0.contains("<@456>"));
        assert!(!replies[2].1);
        assert!(
            tool_card_switch::status(ctx.root.path(), 123)
                .unwrap()
                .unwrap()
                .enabled
        );
    }
}

#[tokio::test]
async fn published_sync_failure_response_and_status_match_the_live_value() {
    let mut ctx = FakeContext::new(Some(RuntimeHandoffKind::ClaudeTui));
    ctx.global = false;
    let failure = crate::services::tui_o::store::fault::plant(
        ctx.root.path(),
        crate::services::tui_o::store::fault::Step::ReplaceDirSync,
        std::io::ErrorKind::Other,
        Some(1),
    );
    run(&ctx, ToolCallAction::On).await.unwrap();
    drop(failure);
    run(&ctx, ToolCallAction::Status).await.unwrap();
    let replies = ctx.replies.lock().unwrap();
    assert!(replies[0].0.contains("현재 설정은 적용됐지만"));
    assert!(replies[0].0.contains("실효값은 끔"));
    assert!(
        replies[1]
            .0
            .contains("도구 카드: 끔 · 근거: 설정 전체 차단")
    );
}

#[test]
fn command_is_slash_only_with_three_choices_and_a_mutating_policy_entry() {
    let command = cmd_tool_call();
    assert_eq!(command.name, "tool-call");
    assert!(command.slash_action.is_some());
    assert!(command.prefix_action.is_none());
    assert_eq!(command.parameters.len(), 1);
    assert_eq!(
        command.parameters[0]
            .choices
            .iter()
            .map(|choice| choice.name.as_ref())
            .collect::<Vec<_>>(),
        ["on", "off", "status"]
    );
    assert!(command.description.as_ref().unwrap().chars().count() <= 100);
    assert_eq!(
        super::super::slash_command_risk("/tool-call"),
        super::super::CommandRisk::Mutating
    );
}
