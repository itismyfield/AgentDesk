/* giant-file-exemption: reason=monitor-section-needs-further-split ticket=#1078 */
module.exports = function attachActiveMonitor(timeouts, helpers) {
  var findRecentInflightForSession = helpers.findRecentInflightForSession;

  timeouts._tmuxPaneLiveness = function(tmuxName) {
      if (!tmuxName || !tmuxName.trim()) return "unknown";
      try {
        var state = agentdesk.session.hasLivePane(tmuxName);
        return state === "live" || state === "dead" ? state : "unknown";
      } catch(e) {
        return "unknown";
      }
    };

  timeouts._section_I = function() {
      // Repair missing/dead sessions; accepted live turns have no silence budget.
      var STALE_SCAN_MINUTES = 30;
      var liveness = new Map();
      function paneLiveness(name) {
        if (!liveness.has(name)) {
          var state = timeouts._tmuxPaneLiveness(name);
          liveness.set(name, state);
          if (state === "unknown") agentdesk.log.warn("[deadlock] Pane liveness unknown; deferring " + name);
        }
        return liveness.get(name);
      }

      // 먼저: heartbeat가 신선한 working 세션의 카운터를 리셋 (비연속 스톨 누적 방지)
      agentdesk.timeouts.clearDeadlockCountersForFreshSessions(STALE_SCAN_MINUTES);

      // Fix stale working sessions: if status=working but no inflight file exists,
      // the turn has ended but DB wasn't updated. Fix to idle.
      // #219: Increased grace period from 3min to 10min — agents running long tool
      // calls (cargo build, subagents) may not send heartbeats for several minutes.
      var staleWorkingSessions = agentdesk.timeouts.listStaleWorkingSessions(10);
      for (var sw = 0; sw < staleWorkingSessions.length; sw++) {
        var swKey = staleWorkingSessions[sw].session_key;
        var tmuxName = (swKey || "").split(":").pop();
        // #219: Check if tmux session has a live pane (not just session existence).
        // has-session returns true for zombie sessions with dead panes;
        // list-panes #{pane_dead} distinguishes live vs dead workers.
        var tmuxState = paneLiveness(tmuxName);
        if (tmuxState === "unknown") continue;
        var inflight;
        try {
          inflight = findRecentInflightForSession(swKey, tmuxName);
        } catch (e) {
          agentdesk.log.warn("[deadlock] Transient error looking up inflight for " + swKey + ": " + e);
          continue; // transient error, retry next time
        }
        if (tmuxState === "dead" || !inflight) {
          // #219: Fail any pending dispatch before transitioning to idle.
          // Without this, the dispatch stays "pending" as an orphan and gets
          // re-delivered or auto-completed, causing the failure loop.
          try {
            if (staleWorkingSessions[sw].active_dispatch_id) {
              var swDispId = staleWorkingSessions[sw].active_dispatch_id;
              var swDispStatus = staleWorkingSessions[sw].active_dispatch_status;
              if (swDispStatus === "pending" || swDispStatus === "dispatched") {
                agentdesk.dispatch.markFailed(swDispId, "Stale working session recovery — no active tmux session after 10min");
                agentdesk.log.warn("[deadlock] Failed stale dispatch " + swDispId + " for session " + swKey);
              }
            }
          } catch(dispErr) {
            agentdesk.log.warn("[deadlock] Failed to mark dispatch for " + swKey + ": " + dispErr);
          }
          agentdesk.timeouts.markSessionIdle(swKey, { clear_active_dispatch_id: true });
          agentdesk.log.info("[deadlock] Fixed stale working session → idle: " + swKey);
        }
      }

      // 데드락 의심 세션: sessions.last_heartbeat 기반 판별
      // deadlock-manager 자신의 세션은 제외 (자기 자신을 오탐하는 무한 루프 방지)
      var staleSessions = agentdesk.timeouts.listDeadlockCandidates(STALE_SCAN_MINUTES, 50);
      for (var dl = 0; dl < staleSessions.length; dl++) {
        var sess = staleSessions[dl];
        var deadlockKey = "deadlock_check:" + sess.session_key;
        var dlTmuxName = (sess.session_key || "").split(":").pop();
        var dlState = paneLiveness(dlTmuxName);
        if (dlState === "unknown") continue;
        var inflight;
        try {
          inflight = findRecentInflightForSession(sess.session_key, dlTmuxName);
        } catch (e) {
          continue;
        }
        agentdesk.kv.delete(deadlockKey);
        if (dlState === "live" && inflight) continue;
        agentdesk.timeouts.markSessionIdle(sess.session_key, { clear_active_dispatch_id: false });
        agentdesk.log.info("[deadlock] Stale working session → idle (no active turn): " + sess.session_key);
      }

      // Clean up deadlock counters for sessions no longer working
      agentdesk.timeouts.cleanupDeadlockCountersForInactiveSessions();

      // Clean up old deadlock history entries (7일 이상)
      var sevenDaysAgo = Date.now() - 7 * 24 * 60 * 60 * 1000;
      agentdesk.timeouts.cleanupDeadlockHistoryBefore(sevenDaysAgo);
    };
};
