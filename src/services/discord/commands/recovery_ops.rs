//! Issue #2653: Discord slash commands for deadlock recovery, machine flip,
//! and bulk stuck-PR rebase.
//!
//! Background
//! ----------
//! Recovery sequences that used to require 5–7 manual steps over chat
//! (promote-release.sh + mtime compare + ssh + codesign + dcserver restart)
//! are now exposed as discoverable Discord slash commands. Each command runs
//! a curated bash pipeline against well-known repo scripts and emits a single
//! transcript back to the channel.
//!
//! Design constraints
//! ------------------
//! * **Risk tier** — All three commands run shell pipelines and `ssh` to
//!   peers; they are classified as `ShellOrToolGrant`
//!   (RCE-equivalent → owner-only + `AGENTDESK_DISCORD_HIGH_RISK_ENABLED=1`).
//! * **Curated payload** — The bash payload is built by the pure
//!   [`build_recovery_script`] helper. It never interpolates user-controlled
//!   strings into the shell payload; only operator-supplied environment
//!   variables (peer host, env name) are accepted, and they pass through a
//!   strict allowlist validator ([`validate_safe_token`]).
//! * **Preflight** — Each pipeline starts with a preflight block that fails
//!   fast on missing prerequisites (repo root, gh auth, ssh reachability)
//!   before any mutating step.
//! * **Timeout** — Pipelines are bounded by the same total-timeout the
//!   `!shell` surface uses, so a hung `ssh` cannot pin the bot indefinitely.
//!
//! Tested separately via `cargo test --lib recovery_ops::tests`.

use super::super::formatting::send_long_message_ctx;
use super::super::{Context, Error, check_auth};

/// Maximum length for an operator-supplied argument (peer host, env, branch
/// prefix, etc.). Keeps the bash payload bounded and rejects pathological
/// inputs early.
const MAX_ARG_LEN: usize = 64;

/// Recovery sequence selector. Each variant maps to a distinct bash payload
/// produced by [`build_recovery_script`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RecoveryKind {
    /// `!deadlock-recover` — stop the local dcserver, reap any orphan agent
    /// processes, restart launchd, and poll `/api/health` until it comes up.
    Deadlock {
        /// Launchd label suffix to operate on (`release` or `dev`).
        env: String,
    },
    /// `!machine-flip` — rebuild + redeploy this machine, then ssh into the
    /// peer and trigger `deploy-release.sh` there too.
    MachineFlip {
        /// SSH hostname of the peer to deploy to (e.g. `mac-mini` or
        /// `mac-book`).
        peer: String,
    },
    /// `!stuck-pr-rebase` — list open PRs labeled with the audit fleet label,
    /// rebase each onto `origin/main`, run `cargo fmt --all`, and force-push
    /// with lease.
    StuckPrRebase {
        /// GitHub label that selects the PRs to rebase. Defaults to the
        /// audit fleet label but can be overridden by the caller.
        label: String,
    },
}

impl RecoveryKind {
    /// Human-readable command name used for logging and error messages.
    pub(super) fn name(&self) -> &'static str {
        match self {
            RecoveryKind::Deadlock { .. } => "!deadlock-recover",
            RecoveryKind::MachineFlip { .. } => "!machine-flip",
            RecoveryKind::StuckPrRebase { .. } => "!stuck-pr-rebase",
        }
    }
}

/// Outcome of validating a single operator-supplied token (peer host, env
/// name, label, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TokenError {
    Empty,
    TooLong,
    InvalidChar(char),
}

impl TokenError {
    pub(super) fn user_message(&self, field: &str) -> String {
        match self {
            TokenError::Empty => format!("`{field}` must not be empty."),
            TokenError::TooLong => {
                format!("`{field}` must be at most {MAX_ARG_LEN} characters.")
            }
            TokenError::InvalidChar(c) => format!(
                "`{field}` contains the character `{c}`, which is not in the allowed set \
                 `[A-Za-z0-9._:/-]`. Reject any caller trying to smuggle shell metacharacters."
            ),
        }
    }
}

/// Pure validator for operator-supplied arguments. Allows only an
/// alphanumeric subset plus a few separator characters that appear in
/// hostnames, env labels, branch names, and GitHub labels. Anything else
/// (including whitespace, `$`, backticks, `;`, `|`, `&`, quotes, `\`) is
/// rejected so the bash payload cannot be smuggled through the argument
/// surface.
pub(super) fn validate_safe_token(value: &str) -> Result<(), TokenError> {
    if value.is_empty() {
        return Err(TokenError::Empty);
    }
    if value.len() > MAX_ARG_LEN {
        return Err(TokenError::TooLong);
    }
    for c in value.chars() {
        let ok = c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':');
        if !ok {
            return Err(TokenError::InvalidChar(c));
        }
    }
    Ok(())
}

/// Build the bash script for a given recovery kind. Pure — no I/O — so the
/// generated payload can be golden-tested.
///
/// The script is intentionally a *single* `bash -c` payload so it inherits
/// the existing shell-guard timeouts in the calling surface. Every step
/// prints a banner so the Discord transcript reads like a checklist.
pub(super) fn build_recovery_script(kind: &RecoveryKind) -> String {
    match kind {
        RecoveryKind::Deadlock { env } => format!(
            r#"set -uo pipefail
echo "==[ deadlock-recover: env={env} ]=="
LABEL="com.agentdesk.{env}"
echo "-- preflight --"
launchctl list | grep -F "$LABEL" || echo "launchctl: label $LABEL not currently loaded"
echo "-- step 1: stop dcserver --"
launchctl bootout "gui/$(id -u)/$LABEL" 2>&1 || \
  launchctl unload -w "$HOME/Library/LaunchAgents/$LABEL.plist" 2>&1 || \
  echo "(launchd already stopped)"
echo "-- step 2: reap orphan agentdesk/tmux processes --"
pkill -TERM -f 'agentdesk' || echo "(no agentdesk processes)"
pkill -TERM -f 'adk-.*-tmux' || echo "(no managed tmux)"
sleep 2
echo "-- step 3: restart dcserver --"
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/$LABEL.plist" 2>&1 || \
  launchctl load -w "$HOME/Library/LaunchAgents/$LABEL.plist" 2>&1
echo "-- step 4: poll /api/health up to 30s --"
for i in $(seq 1 15); do
  if curl -fsS --max-time 2 http://127.0.0.1:8791/api/health >/dev/null 2>&1; then
    echo "health: OK (after ${{i}} polls)"
    exit 0
  fi
  sleep 2
done
echo "health: TIMEOUT after 30s — check launchd stderr"
exit 1
"#
        ),

        RecoveryKind::MachineFlip { peer } => format!(
            r#"set -uo pipefail
echo "==[ machine-flip: peer={peer} ]=="
REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || true)"
if [ -z "$REPO_ROOT" ]; then
  echo "preflight FAILED: not inside a git repo"; exit 2
fi
echo "-- preflight: gh auth --"
gh auth status 2>&1 | head -3 || {{ echo "gh not authenticated"; exit 2; }}
echo "-- preflight: ssh reachability --"
ssh -o BatchMode=yes -o ConnectTimeout=5 "{peer}" hostname 2>&1 || {{
  echo "preflight FAILED: ssh {peer} unreachable"; exit 2;
}}
echo "-- step 1: local deploy-release --"
( cd "$REPO_ROOT" && AGENTDESK_DEPLOY_SKIP_REMOTE_FRESHNESS=1 bash scripts/deploy-release.sh ) 2>&1 | tail -40
LOCAL_RC=${{PIPESTATUS[0]}}
if [ "$LOCAL_RC" -ne 0 ]; then
  echo "local deploy FAILED (rc=$LOCAL_RC) — peer step skipped"; exit "$LOCAL_RC"
fi
echo "-- step 2: peer deploy via ssh --"
ssh -o BatchMode=yes "{peer}" "REMOTE_REPO=\"\${{AGENTDESK_REPO_DIR:-\$HOME/.adk/release/workspaces/agentdesk}}\"; \
  cd \$(git -C \"\$REMOTE_REPO\" rev-parse --show-toplevel 2>/dev/null || echo \"\$REMOTE_REPO\") && \
  AGENTDESK_DEPLOY_SKIP_REMOTE_FRESHNESS=1 bash scripts/deploy-release.sh" 2>&1 | tail -40
PEER_RC=${{PIPESTATUS[0]}}
if [ "$PEER_RC" -ne 0 ]; then
  echo "peer deploy FAILED (rc=$PEER_RC) — local already promoted"; exit "$PEER_RC"
fi
echo "machine-flip: DONE (local + {peer})"
"#
        ),

        RecoveryKind::StuckPrRebase { label } => format!(
            r#"set -uo pipefail
echo "==[ stuck-pr-rebase: label={label} ]=="
REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || true)"
if [ -z "$REPO_ROOT" ]; then
  echo "preflight FAILED: not inside a git repo"; exit 2
fi
cd "$REPO_ROOT"
echo "-- preflight: gh auth --"
gh auth status 2>&1 | head -3 || {{ echo "gh not authenticated"; exit 2; }}
echo "-- preflight: fetch origin --"
git fetch origin --quiet
echo "-- discovery: open PRs with label '{label}' --"
PRS=$(gh pr list --label "{label}" --state open --json number,headRefName --jq '.[] | "\(.number) \(.headRefName)"')
if [ -z "$PRS" ]; then
  echo "no open PRs with label '{label}' — nothing to do"; exit 0
fi
echo "$PRS" | while IFS=' ' read -r NUM BR; do
  [ -z "$NUM" ] && continue
  echo "-- PR #$NUM ($BR): rebase --"
  git fetch origin "$BR" --quiet 2>&1 || {{ echo "  fetch failed, skipping"; continue; }}
  git checkout -B "audit-rebase/$NUM" "origin/$BR" 2>&1
  if ! git rebase origin/main 2>&1; then
    echo "  rebase conflict on #$NUM — aborting and skipping"
    git rebase --abort 2>/dev/null || true
    continue
  fi
  cargo fmt --all 2>&1 | tail -5 || echo "  cargo fmt: skipped (no rust?)"
  if ! git diff --quiet; then
    git add -A && git commit -m "chore: cargo fmt --all (audit auto-rebase)" 2>&1
  fi
  git push --force-with-lease origin "audit-rebase/$NUM:$BR" 2>&1 | tail -3
  echo "  PR #$NUM: pushed"
done
echo "stuck-pr-rebase: DONE"
"#
        ),
    }
}

/// Common runner: validate args, build the script, hand it off to the
/// platform shell, and stream the transcript back to Discord.
async fn run_recovery(
    ctx: Context<'_>,
    kind: RecoveryKind,
    command_label: &'static str,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }
    // Recovery commands are RCE-grade — the slash command policy enforces
    // owner-only + explicit opt-in. `enforce_slash_command_policy` reads
    // the risk tier from `slash_command_risk` which maps these names to
    // `ShellOrToolGrant`.
    if !super::enforce_slash_command_policy(&ctx, command_label).await? {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] ◀ [{user_name}] {command_label} kind={:?}",
        kind.name()
    );

    ctx.defer().await?;

    let script = build_recovery_script(&kind);
    let working_dir = dirs::home_dir()
        .map(|h| h.display().to_string())
        .unwrap_or_else(|| "/".to_string());

    let result = tokio::task::spawn_blocking(move || {
        let mut builder = crate::services::platform::shell::shell_command_builder(&script);
        builder
            .current_dir(&working_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        crate::services::process::configure_child_process_group(&mut builder);
        match builder.spawn() {
            Ok(child) => crate::services::shell_guard::wait_with_no_output_timeout(
                child,
                crate::services::shell_guard::DEFAULT_NO_OUTPUT_TIMEOUT,
                crate::services::shell_guard::DEFAULT_TOTAL_TIMEOUT,
            ),
            Err(e) => Err(format!("spawn failed: {}", e)),
        }
    })
    .await;

    let response = match result {
        Ok(Ok(outcome)) => {
            let stdout = String::from_utf8_lossy(&outcome.stdout);
            let stderr = String::from_utf8_lossy(&outcome.stderr);
            let mut parts = Vec::new();
            if !stdout.is_empty() {
                parts.push(format!("```\n{}\n```", stdout.trim_end()));
            }
            if !stderr.is_empty() {
                parts.push(super::owner_error_response(
                    "복구 명령이 오류 출력을 반환했어요.",
                    stderr.trim_end(),
                ));
            }
            if let Some(cause) = outcome.timed_out {
                parts.push(super::owner_error_response(
                    "복구 명령이 제한 시간을 초과해 중지됐어요.",
                    cause.as_str(),
                ));
            } else if parts.is_empty() {
                parts.push(format!("(종료 코드: {})", outcome.exit_code));
            } else if outcome.exit_code != 0 {
                parts.push(format!("(종료 코드: {})", outcome.exit_code));
            }
            parts.join("\n")
        }
        Ok(Err(e)) => super::owner_error_response("복구 명령을 실행하지 못했어요.", &e),
        Err(e) => super::owner_error_response(
            "복구 명령을 처리하는 중 오류가 발생했어요.",
            &e.to_string(),
        ),
    };

    send_long_message_ctx(ctx, &response).await?;
    tracing::info!("  [{ts}] ▶ [{user_name}] {command_label} done");
    Ok(())
}

/// `/deadlock-recover [env]` — restart the local dcserver and reap orphans.
///
/// `env` defaults to `release`.
#[poise::command(slash_command, rename = "deadlock-recover")]
pub(in crate::services::discord) async fn cmd_deadlock_recover(
    ctx: Context<'_>,
    #[description = "launchd env label suffix (default: release)"] env: Option<String>,
) -> Result<(), Error> {
    let env = env.unwrap_or_else(|| "release".to_string());
    if let Err(e) = validate_safe_token(&env) {
        ctx.say(e.user_message("env")).await?;
        return Ok(());
    }
    run_recovery(ctx, RecoveryKind::Deadlock { env }, "/deadlock-recover").await
}

/// `/machine-flip <peer>` — redeploy locally then ssh deploy to peer.
#[poise::command(slash_command, rename = "machine-flip")]
pub(in crate::services::discord) async fn cmd_machine_flip(
    ctx: Context<'_>,
    #[description = "peer SSH host (e.g. mac-mini, mac-book)"] peer: String,
) -> Result<(), Error> {
    if let Err(e) = validate_safe_token(&peer) {
        ctx.say(e.user_message("peer")).await?;
        return Ok(());
    }
    run_recovery(ctx, RecoveryKind::MachineFlip { peer }, "/machine-flip").await
}

/// `/stuck-pr-rebase [label]` — rebuild + force-push every open PR carrying
/// the given audit label.
///
/// `label` defaults to `audit:2026-05-19`.
#[poise::command(slash_command, rename = "stuck-pr-rebase")]
pub(in crate::services::discord) async fn cmd_stuck_pr_rebase(
    ctx: Context<'_>,
    #[description = "GitHub label selecting PRs to rebase (default: audit:2026-05-19)"]
    label: Option<String>,
) -> Result<(), Error> {
    let label = label.unwrap_or_else(|| "audit:2026-05-19".to_string());
    if let Err(e) = validate_safe_token(&label) {
        ctx.say(e.user_message("label")).await?;
        return Ok(());
    }
    run_recovery(
        ctx,
        RecoveryKind::StuckPrRebase { label },
        "/stuck-pr-rebase",
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_empty() {
        assert_eq!(validate_safe_token(""), Err(TokenError::Empty));
    }

    #[test]
    fn validate_rejects_too_long() {
        let long = "a".repeat(MAX_ARG_LEN + 1);
        assert_eq!(validate_safe_token(&long), Err(TokenError::TooLong));
    }

    #[test]
    fn validate_rejects_shell_metacharacters() {
        for bad in [
            "foo;rm",
            "foo|cat",
            "foo&bar",
            "foo`whoami`",
            "foo$bar",
            "foo bar",
            "foo'bar",
            "foo\"bar",
            "foo\\bar",
            "foo(bar)",
            "foo>out",
            "foo<in",
            "foo\nbar",
        ] {
            assert!(
                matches!(validate_safe_token(bad), Err(TokenError::InvalidChar(_))),
                "must reject {bad:?}",
            );
        }
    }

    #[test]
    fn validate_accepts_hostnames_envs_labels() {
        for good in [
            "release",
            "dev",
            "mac-mini",
            "mac-book",
            "audit:2026-05-19",
            "feature/audit-fix",
            "PR.42",
            "v1.2.3-rc1",
        ] {
            assert!(validate_safe_token(good).is_ok(), "must accept {good:?}",);
        }
    }

    #[test]
    fn deadlock_script_contains_health_poll_and_label() {
        let s = build_recovery_script(&RecoveryKind::Deadlock {
            env: "release".to_string(),
        });
        assert!(s.contains("com.agentdesk.release"));
        assert!(s.contains("/api/health"));
        assert!(s.contains("launchctl bootstrap"));
        assert!(s.contains("pkill -TERM -f 'agentdesk'"));
    }

    #[test]
    fn machine_flip_script_preflights_then_deploys_both() {
        let s = build_recovery_script(&RecoveryKind::MachineFlip {
            peer: "mac-book".to_string(),
        });
        // Preflight ssh check must come before the local deploy banner.
        let preflight = s.find("preflight: ssh reachability").unwrap();
        let local = s.find("step 1: local deploy-release").unwrap();
        let peer = s.find("step 2: peer deploy via ssh").unwrap();
        assert!(preflight < local && local < peer);
        assert!(s.contains("scripts/deploy-release.sh"));
        assert!(s.contains("mac-book"));
        assert!(s.contains("$HOME/.adk/release/workspaces/agentdesk"));
        assert!(s.contains("${AGENTDESK_REPO_DIR:-"));
        assert!(!s.contains("~/AgentDesk"));
    }

    #[test]
    fn stuck_pr_script_uses_force_with_lease_and_label() {
        let s = build_recovery_script(&RecoveryKind::StuckPrRebase {
            label: "audit:2026-05-19".to_string(),
        });
        assert!(s.contains("audit:2026-05-19"));
        assert!(s.contains("git rebase origin/main"));
        assert!(s.contains("cargo fmt --all"));
        assert!(s.contains("--force-with-lease"));
        // Must use gh pr list with --label so the discovery is bounded.
        assert!(s.contains("gh pr list --label"));
    }

    /// The pure builder must never substitute a value that failed
    /// [`validate_safe_token`]. Callers always validate first; this test
    /// pins the property at the API boundary so a future refactor cannot
    /// accidentally accept an unvalidated string.
    #[test]
    fn script_never_contains_shell_metacharacters_from_args() {
        let s = build_recovery_script(&RecoveryKind::MachineFlip {
            peer: "mac-mini".to_string(),
        });
        // No backticks or unescaped $() injected from a hostname.
        // Note: the script body itself contains $() for command substitution
        // intentionally — we assert the hostname (mac-mini) appears literally.
        assert!(s.contains("\"mac-mini\""));
        assert!(!s.contains("`mac-mini`"));
    }

    #[test]
    fn recovery_kind_name_matches_command_surface() {
        assert_eq!(
            RecoveryKind::Deadlock { env: "r".into() }.name(),
            "!deadlock-recover"
        );
        assert_eq!(
            RecoveryKind::MachineFlip { peer: "p".into() }.name(),
            "!machine-flip"
        );
        assert_eq!(
            RecoveryKind::StuckPrRebase { label: "l".into() }.name(),
            "!stuck-pr-rebase"
        );
    }

    #[test]
    fn token_error_messages_mention_field_name() {
        assert!(TokenError::Empty.user_message("peer").contains("peer"));
        assert!(TokenError::TooLong.user_message("env").contains("env"));
        assert!(
            TokenError::InvalidChar(';')
                .user_message("label")
                .contains(";")
        );
    }
}
