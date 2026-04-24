module.exports = function attachWorkspaceBranchGuard(timeouts, helpers) {
  var sendDeadlockAlert = helpers.sendDeadlockAlert;
  var MAX_DISPATCH_RETRIES = helpers.MAX_DISPATCH_RETRIES;
  var getTimeoutInterval = helpers.getTimeoutInterval;
  var latestCardActivityExpr = helpers.latestCardActivityExpr;
  var parseLocalTimestampMs = helpers.parseLocalTimestampMs;
  var normalizedText = helpers.normalizedText;
  var parseSessionTmuxName = helpers.parseSessionTmuxName;
  var parseSessionChannelName = helpers.parseSessionChannelName;
  var parseParentChannelName = helpers.parseParentChannelName;
  var parseSessionThreadId = helpers.parseSessionThreadId;
  var loadAgentDirectory = helpers.loadAgentDirectory;
  var agentDisplayName = helpers.agentDisplayName;
  var findAgentById = helpers.findAgentById;
  var channelMatchesCandidate = helpers.channelMatchesCandidate;
  var findAgentByChannelValue = helpers.findAgentByChannelValue;
  var buildChannelTarget = helpers.buildChannelTarget;
  var resolveAgentNotifyTarget = helpers.resolveAgentNotifyTarget;
  var lookupDispatchTargetAgentId = helpers.lookupDispatchTargetAgentId;
  var lookupThreadTargetAgentId = helpers.lookupThreadTargetAgentId;
  var resolveSessionAgentContext = helpers.resolveSessionAgentContext;
  var backfillMissingSessionAgentIds = helpers.backfillMissingSessionAgentIds;
  var findRecentInflightForSession = helpers.findRecentInflightForSession;
  var inspectInflightProgress = helpers.inspectInflightProgress;
  var requestTurnWatchdogExtension = helpers.requestTurnWatchdogExtension;
  var _queuePMDecision = helpers._queuePMDecision;
  var _flushPMDecisions = helpers._flushPMDecisions;

  timeouts._section_M = function() {
      // Get unique workspace paths from sessions table
      var workspaces = agentdesk.db.query(
        "SELECT DISTINCT json_extract(metadata, '$.workspace') as ws FROM sessions " +
        "WHERE json_extract(metadata, '$.workspace') IS NOT NULL"
      );
      // Also check known workspaces from agents table
      var agentWorkspaces = agentdesk.db.query(
        "SELECT DISTINCT workspace FROM agents WHERE workspace IS NOT NULL AND workspace != ''"
      );
      // Deduplicate
      var seen = {};
      var paths = [];
      for (var w = 0; w < workspaces.length; w++) {
        if (workspaces[w].ws && !seen[workspaces[w].ws]) {
          seen[workspaces[w].ws] = true;
          paths.push(workspaces[w].ws);
        }
      }
      for (var aw = 0; aw < agentWorkspaces.length; aw++) {
        if (agentWorkspaces[aw].workspace && !seen[agentWorkspaces[aw].workspace]) {
          seen[agentWorkspaces[aw].workspace] = true;
          paths.push(agentWorkspaces[aw].workspace);
        }
      }
      for (var p = 0; p < paths.length; p++) {
        var ws = paths[p];
        try {
          var branch = agentdesk.exec("git", JSON.stringify(["-C", ws, "branch", "--show-current"]));
          if (!branch) continue;
          branch = branch.replace(/\s+/g, "");
          if (branch.indexOf("wt/") === 0) {
            agentdesk.log.warn("[branch-guard] Workspace " + ws + " on worktree branch '" + branch + "' — recovering to main");
            // Stash any changes before switching
            agentdesk.exec("git", JSON.stringify(["-C", ws, "stash", "--include-untracked", "-m", "auto-stash before branch-guard recovery"]));
            var checkoutResult = agentdesk.exec("git", JSON.stringify(["-C", ws, "checkout", "main"]));
            agentdesk.exec("git", JSON.stringify(["-C", ws, "pull", "--ff-only"]));
            agentdesk.exec("git", JSON.stringify(["-C", ws, "worktree", "prune"]));
            agentdesk.log.warn("[branch-guard] Recovered " + ws + " to main (was: " + branch + ")");
            sendDeadlockAlert(
              "🔧 [branch-guard] Workspace 브랜치 자동 복구\n" +
              "경로: `" + ws + "`\n" +
              "이탈 브랜치: `" + branch + "` → `main`\n" +
              "원인: 에이전트가 worktree 브랜치를 메인 repo에서 checkout (#181)"
            );
          }
        } catch(e) {
          agentdesk.log.warn("[branch-guard] Error checking " + ws + ": " + e);
        }
      }
    };
};
