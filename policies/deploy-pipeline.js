/**
 * deploy-pipeline.js — ADK Policy: Dev Deploy + E2E Test Pipeline (#197)
 * priority: 45 (between kanban-rules=10 and review-automation=50)
 *
 * Post-review pipeline stages:
 *   1. dev-deploy: Build and deploy to dev (self-hosted, no agent turn)
 *   2. e2e-test:   Counter-model agent runs E2E tests against dev server
 *
 * Hooks:
 *   onDispatchCompleted — e2e-test dispatch result → advance or rework
 *   onTick30s           — Deploy queue: start/monitor deploys (max 1 concurrent)
 */

var REPO_DIR = "/Users/itismyfield/.adk/release/workspaces/agentdesk";
var DEPLOY_MAX_RETRIES = 2;

// ── Deploy execution via tmux ────────────────────────────────

function parseJsonObject(raw) {
  if (!raw) return {};
  try {
    var parsed = JSON.parse(raw);
    return parsed && typeof parsed === "object" ? parsed : {};
  } catch (e) {
    return {};
  }
}

function firstNonEmptyString(values) {
  for (var i = 0; i < values.length; i++) {
    if (typeof values[i] === "string" && values[i].trim() !== "") {
      return values[i];
    }
  }
  return null;
}

function shellQuote(value) {
  return "'" + String(value).replace(/'/g, "'\\''") + "'";
}

function getWorktreeForCard(cardId) {
  var rows = agentdesk.db.query(
    "SELECT result, context FROM task_dispatches " +
    "WHERE kanban_card_id = ? " +
    "  AND dispatch_type IN ('implementation', 'rework') " +
    "  AND status = 'completed' " +
    "ORDER BY updated_at DESC, rowid DESC LIMIT 1",
    [cardId]
  );

  if (rows.length === 0) {
    return {
      path: REPO_DIR,
      branch: null,
      source: "repo-root:no-completed-work-dispatch"
    };
  }

  var result = parseJsonObject(rows[0].result);
  var context = parseJsonObject(rows[0].context);
  var path = firstNonEmptyString([
    result.completed_worktree_path,
    result.worktree_path,
    context.worktree_path
  ]);
  var branch = firstNonEmptyString([
    result.completed_branch,
    result.worktree_branch,
    result.branch,
    context.worktree_branch,
    context.branch
  ]);

  if (path) {
    return {
      path: path,
      branch: branch,
      source: "card-dispatch-context"
    };
  }

  return {
    path: REPO_DIR,
    branch: branch,
    source: "repo-root:no-worktree-in-dispatch-context"
  };
}

function startDeploySession(cardId) {
  var sessionName = "adk-deploy-" + cardId.substring(0, 8);

  // Check if session already exists
  var check = agentdesk.exec("tmux", ["has-session", "-t", sessionName]);
  if (check.indexOf("ERROR") < 0) {
    agentdesk.log.info("[deploy-pipeline] Deploy session " + sessionName + " already exists");
    return sessionName;
  }

  var worktree = getWorktreeForCard(cardId);
  agentdesk.log.info(
    "[deploy-pipeline] Worktree for card " + cardId + ": " + worktree.path +
    " (source=" + worktree.source +
    (worktree.branch ? " branch=" + worktree.branch : "") + ")"
  );

  // Spawn deploy in detached tmux session.
  // Build the workspace for release, then promote directly into the release runtime.
  // After script exits, store exit code in tmux env for the tick to read.
  var cmd = "cd " + shellQuote(worktree.path) + " && scripts/build-release.sh 2>&1 && scripts/promote-release.sh --skip-review 2>&1; " +
    "tmux set-environment -t " + sessionName + " DEPLOY_RESULT $?; " +
    "sleep 600";

  var result = agentdesk.exec("tmux", ["new-session", "-d", "-s", sessionName, cmd]);
  if (result && result.indexOf("ERROR") >= 0) {
    agentdesk.log.error("[deploy-pipeline] Failed to start deploy session: " + result);
    return null;
  }

  agentdesk.log.info("[deploy-pipeline] Deploy session started: " + sessionName);
  return sessionName;
}

function checkDeployStatus(sessionName) {
  var check = agentdesk.exec("tmux", ["has-session", "-t", sessionName]);
  if (check.indexOf("ERROR") >= 0) return "gone";

  var envResult = agentdesk.exec("tmux", [
    "show-environment", "-t", sessionName, "DEPLOY_RESULT"
  ]);
  if (envResult && envResult.indexOf("DEPLOY_RESULT=") >= 0) {
    var exitCode = envResult.split("=")[1].trim();
    return exitCode === "0" ? "success" : "failed:" + exitCode;
  }
  return "running";
}

function cleanupDeploySession(sessionName) {
  agentdesk.exec("tmux", ["kill-session", "-t", sessionName]);
}

// ── Pipeline stage advancement ───────────────────────────────

function advancePipelineStage(cardId) {
  var card = agentdesk.db.query(
    "SELECT pipeline_stage_id, repo_id, assigned_agent_id FROM kanban_cards WHERE id = ?",
    [cardId]
  );
  if (card.length === 0) return;

  var currentStageId = card[0].pipeline_stage_id;
  if (!currentStageId) return;

  var currentStage = agentdesk.db.query(
    "SELECT stage_order FROM pipeline_stages WHERE id = ?",
    [currentStageId]
  );
  if (currentStage.length === 0) return;

  var nextStage = agentdesk.db.query(
    "SELECT id, stage_name, provider, agent_override_id FROM pipeline_stages " +
    "WHERE repo_id = ? AND stage_order > ? ORDER BY stage_order ASC LIMIT 1",
    [card[0].repo_id, currentStage[0].stage_order]
  );

  if (nextStage.length > 0) {
    var stage = nextStage[0];
    agentdesk.db.execute(
      "UPDATE kanban_cards SET pipeline_stage_id = ?, blocked_reason = NULL, updated_at = datetime('now') WHERE id = ?",
      [stage.id, cardId]
    );
    agentdesk.log.info("[deploy-pipeline] Card " + cardId + " advancing to stage: " + stage.stage_name);

    if (stage.provider === "counter") {
      // Skip e2e if DoD doesn't mention it
      var dodText = (card[0].description || "").toLowerCase();
      if (dodText.indexOf("e2e") === -1 && dodText.indexOf("end-to-end") === -1 && dodText.indexOf("end to end") === -1) {
        agentdesk.log.info("[deploy-pipeline] Skipping e2e-test for card " + cardId + " — DoD has no e2e item");
        agentdesk.db.execute(
          "UPDATE kanban_cards SET pipeline_stage_id = NULL, blocked_reason = NULL, updated_at = datetime('now') WHERE id = ?",
          [cardId]
        );
        var skipCfg = agentdesk.pipeline.resolveForCard(cardId);
        var skipTerminal = agentdesk.pipeline.terminalState(skipCfg);
        agentdesk.kanban.setStatus(cardId, skipTerminal);
        return;
      }
      createE2eTestDispatch(cardId, card[0].assigned_agent_id);
    } else if (stage.provider === "self") {
      agentdesk.db.execute(
        "UPDATE kanban_cards SET blocked_reason = 'deploy:waiting' WHERE id = ?",
        [cardId]
      );
    } else {
      var agent = stage.agent_override_id || card[0].assigned_agent_id;
      if (agent) {
        try {
          agentdesk.dispatch.create(cardId, agent, "implementation",
            "[Pipeline: " + stage.stage_name + "] " + cardId);
        } catch (e) {
          agentdesk.log.warn("[deploy-pipeline] Stage dispatch failed: " + e);
        }
      }
    }
  } else {
    // No more stages — done
    agentdesk.db.execute(
      "UPDATE kanban_cards SET pipeline_stage_id = NULL, blocked_reason = NULL, updated_at = datetime('now') WHERE id = ?",
      [cardId]
    );
    var cfg = agentdesk.pipeline.resolveForCard(cardId);
    var terminalState = agentdesk.pipeline.terminalState(cfg);
    agentdesk.kanban.setStatus(cardId, terminalState);
    agentdesk.log.info("[deploy-pipeline] Card " + cardId + " completed all pipeline stages -> " + terminalState);
  }
}

// ── E2E test dispatch ────────────────────────────────────────

function createE2eTestDispatch(cardId, assignedAgentId) {
  if (!assignedAgentId) return;
  var card = agentdesk.db.query(
    "SELECT title, github_issue_number, github_issue_url FROM kanban_cards WHERE id = ?",
    [cardId]
  );
  if (card.length === 0) return;

  var issueNum = card[0].github_issue_number || "?";
  var devPort = agentdesk.config.get("server_port") || 8799;

  try {
    agentdesk.dispatch.create(
      cardId, assignedAgentId, "e2e-test",
      "[E2E Test] #" + issueNum + " " + card[0].title
    );
    agentdesk.log.info("[deploy-pipeline] E2E test dispatched for card " + cardId);
  } catch (e) {
    agentdesk.log.warn("[deploy-pipeline] E2E test dispatch failed: " + e);
  }
}

// ── Failure rework dispatches ────────────────────────────────

function createDeployFailRework(cardId) {
  var card = agentdesk.db.query(
    "SELECT assigned_agent_id, title, github_issue_number FROM kanban_cards WHERE id = ?",
    [cardId]
  );
  if (card.length === 0 || !card[0].assigned_agent_id) return;

  agentdesk.db.execute(
    "UPDATE kanban_cards SET pipeline_stage_id = NULL, blocked_reason = NULL, updated_at = datetime('now') WHERE id = ?",
    [cardId]
  );

  var issueNum = card[0].github_issue_number || "?";
  try {
    agentdesk.dispatch.create(
      cardId, card[0].assigned_agent_id, "rework",
      "[Deploy Fail] #" + issueNum + " " + card[0].title +
      "\n\ndev-deploy failed after " + DEPLOY_MAX_RETRIES + " retries. Check build/deploy errors."
    );
  } catch (e) {
    agentdesk.log.warn("[deploy-pipeline] Deploy fail rework dispatch failed: " + e);
  }

  var cfg = agentdesk.pipeline.resolveForCard(cardId);
  agentdesk.kanban.setStatus(cardId, agentdesk.pipeline.kickoffState(cfg));
}

function createE2eFailRework(cardId, assignedAgentId, details) {
  var card = agentdesk.db.query(
    "SELECT title, github_issue_number FROM kanban_cards WHERE id = ?",
    [cardId]
  );
  if (card.length === 0 || !assignedAgentId) return;

  agentdesk.db.execute(
    "UPDATE kanban_cards SET pipeline_stage_id = NULL, blocked_reason = NULL WHERE id = ?",
    [cardId]
  );

  var issueNum = card[0].github_issue_number || "?";
  try {
    agentdesk.dispatch.create(
      cardId, assignedAgentId, "rework",
      "[E2E Fail] #" + issueNum + " " + card[0].title +
      (details ? "\n\nE2E failure:\n" + details : "")
    );
  } catch (e) {
    agentdesk.log.warn("[deploy-pipeline] E2E fail rework dispatch failed: " + e);
  }

  var cfg = agentdesk.pipeline.resolveForCard(cardId);
  agentdesk.kanban.setStatus(cardId, agentdesk.pipeline.kickoffState(cfg));
}

// ── Policy ──────────────────────────────────────────────────

var deployPipeline = {
  name: "deploy-pipeline",
  priority: 45,

  // ── e2e-test completion ────────────────────────────────────
  onDispatchCompleted: function(payload) {
    var dispatches = agentdesk.db.query(
      "SELECT id, kanban_card_id, dispatch_type, result FROM task_dispatches WHERE id = ?",
      [payload.dispatch_id]
    );
    if (dispatches.length === 0) return;
    var dispatch = dispatches[0];
    if (dispatch.dispatch_type !== "e2e-test") return;
    if (!dispatch.kanban_card_id) return;

    var result = {};
    try { result = JSON.parse(dispatch.result || "{}"); } catch(e) {}

    var card = agentdesk.db.query(
      "SELECT assigned_agent_id FROM kanban_cards WHERE id = ?",
      [dispatch.kanban_card_id]
    );
    if (card.length === 0) return;

    var verdict = result.verdict || result.status;
    if (verdict === "pass" || verdict === "success") {
      agentdesk.log.info("[deploy-pipeline] E2E passed for " + dispatch.kanban_card_id);
      advancePipelineStage(dispatch.kanban_card_id);
    } else if (verdict === "fail" || verdict === "failed") {
      agentdesk.log.info("[deploy-pipeline] E2E failed for " + dispatch.kanban_card_id);
      createE2eFailRework(
        dispatch.kanban_card_id,
        card[0].assigned_agent_id,
        result.details || result.notes || result.feedback
      );
    } else if (result.auto_completed) {
      agentdesk.log.info("[deploy-pipeline] E2E auto-completed for " + dispatch.kanban_card_id + " -> pass");
      advancePipelineStage(dispatch.kanban_card_id);
    } else {
      agentdesk.log.warn("[deploy-pipeline] E2E no verdict for " + dispatch.kanban_card_id);
    }
  },

  // ── Deploy queue tick ─────────────────────────────────────
  onTick30s: function() {
    // Find dev-deploy stage ID
    var deployStages = agentdesk.db.query(
      "SELECT id FROM pipeline_stages WHERE stage_name = 'dev-deploy'"
    );
    if (deployStages.length === 0) return;
    var stageId = deployStages[0].id;

    // Cards in dev-deploy stage with deploy tracking
    var cards = agentdesk.db.query(
      "SELECT id, blocked_reason FROM kanban_cards WHERE pipeline_stage_id = ? AND blocked_reason LIKE 'deploy:%'",
      [stageId]
    );
    if (cards.length === 0) return;

    var activeDeploy = null;
    var waitingQueue = [];

    for (var i = 0; i < cards.length; i++) {
      var reason = cards[i].blocked_reason || "";
      if (reason.indexOf("deploy:deploying:") === 0) {
        activeDeploy = cards[i];
      } else {
        waitingQueue.push(cards[i]);
      }
    }

    // ── Monitor active deploy ──
    if (activeDeploy) {
      var parts = activeDeploy.blocked_reason.split(":");
      var sessionName = parts[2];
      var retryCount = parseInt(parts[3] || "0");
      var status = checkDeployStatus(sessionName);

      if (status === "success") {
        agentdesk.log.info("[deploy-pipeline] Deploy succeeded: " + activeDeploy.id);
        cleanupDeploySession(sessionName);
        agentdesk.db.execute(
          "UPDATE kanban_cards SET blocked_reason = NULL WHERE id = ?",
          [activeDeploy.id]
        );
        advancePipelineStage(activeDeploy.id);
      } else if (status.indexOf("failed") === 0 || status === "gone") {
        agentdesk.log.warn("[deploy-pipeline] Deploy failed: " + activeDeploy.id +
          " (status=" + status + " retry=" + retryCount + "/" + DEPLOY_MAX_RETRIES + ")");
        cleanupDeploySession(sessionName);

        if (retryCount < DEPLOY_MAX_RETRIES) {
          agentdesk.db.execute(
            "UPDATE kanban_cards SET blocked_reason = ? WHERE id = ?",
            ["deploy:waiting:" + (retryCount + 1), activeDeploy.id]
          );
        } else {
          createDeployFailRework(activeDeploy.id);
        }
      }
      // "running" — wait for next tick
      return; // Only one deploy at a time
    }

    // ── Start next queued deploy ──
    if (waitingQueue.length > 0) {
      var next = waitingQueue[0];
      var reason = next.blocked_reason || "";
      var retryCount = 0;
      if (reason.indexOf("deploy:waiting:") === 0) {
        retryCount = parseInt(reason.split(":")[2] || "0");
      }

      var sessionName = startDeploySession(next.id);
      if (sessionName) {
        agentdesk.db.execute(
          "UPDATE kanban_cards SET blocked_reason = ? WHERE id = ?",
          ["deploy:deploying:" + sessionName + ":" + retryCount, next.id]
        );
        agentdesk.log.info("[deploy-pipeline] Deploy started: " + next.id + " session=" + sessionName);
      } else {
        agentdesk.log.error("[deploy-pipeline] Failed to start deploy: " + next.id);
      }
    }
  }
};

agentdesk.registerPolicy(deployPipeline);
