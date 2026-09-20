/* giant-file-exemption: reason=monitor-section-needs-further-split ticket=#1078 */
module.exports = function attachActiveMonitor(timeouts, helpers) {
  var findRecentInflightForSession = helpers.findRecentInflightForSession;

  timeouts._tmuxHasLivePane = function(tmuxName) {
      try {
        // "=" prefix prevents tmux prefix-matching (exact_target convention)
        var out = agentdesk.exec("tmux", ["list-panes", "-t", "=" + tmuxName, "-F", "#{pane_dead}"]);
        // Success: lines of "0" (alive) or "1" (dead). Any "0" = live pane exists.
        // Failure: "ERROR: ..." (session gone)
        return typeof out === "string" && out.indexOf("ERROR") === -1 && out.indexOf("0") !== -1;
      } catch(e) {
        return false;
      }
    };

  timeouts._section_I = function() {
      // Repair missing/dead sessions; accepted live turns have no silence budget.
      var STALE_SCAN_MINUTES = 30;

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
        var tmuxAlive = timeouts._tmuxHasLivePane(tmuxName);
        var inflight;
        try {
          inflight = findRecentInflightForSession(swKey, tmuxName);
        } catch (e) {
          agentdesk.log.warn("[deadlock] Transient error looking up inflight for " + swKey + ": " + e);
          continue; // transient error, retry next time
        }
        if (!tmuxAlive || !inflight) {
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
        var inflight;
        try {
          inflight = findRecentInflightForSession(sess.session_key, dlTmuxName);
        } catch (e) {
          continue;
        }
        agentdesk.kv.delete(deadlockKey);
        if (timeouts._tmuxHasLivePane(dlTmuxName) && inflight) continue;
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
