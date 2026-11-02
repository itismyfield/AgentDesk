/* giant-file-exemption: reason=monitor-section-needs-further-split ticket=#1078 */
module.exports = function attachActiveMonitor(timeouts, helpers) {
  var findRecentInflightForSession = helpers.findRecentInflightForSession;

  // Full-key host observation: live/dead only for a legacy tmux row whose probe answered.
  timeouts._sessionHost = function(sessionKey) {
      if (!sessionKey || !String(sessionKey).trim()) return { state: "unknown", reason: "session_missing" };
      try {
        var observed = agentdesk.timeouts.observeSessionHost(sessionKey);
        if (observed && (observed.state === "live" || observed.state === "dead")) return observed;
        return { state: "unknown", reason: (observed && observed.reason) || "unknown" };
      } catch(e) {
        return { state: "unknown", reason: "error: " + e };
      }
    };

  timeouts._section_I = function() {
      // Repair missing/dead sessions; accepted live turns have no silence budget.
      // A Herdr, unresolved or conflicting host defers the whole row, inflight or not.
      var STALE_SCAN_MINUTES = 30;
      var hosts = new Map();
      function sessionHost(key) {
        if (!hosts.has(key)) {
          var host = timeouts._sessionHost(key);
          hosts.set(key, host);
          if (host.state === "unknown") agentdesk.log.warn("[deadlock] Session host unknown (" + host.reason + "); deferring " + key);
        }
        return hosts.get(key);
      }
      function repair(key, host, row, opts) {
        var result = agentdesk.timeouts.repairStaleSession(key, {
          session_id: host.session_id,
          active_dispatch_id: row.active_dispatch_id || null,
          active_turn_nonce: row.active_turn_nonce,
          observed: host.state,
          fail_dispatch: !!opts.fail_dispatch,
          fail_reason: opts.fail_reason || "",
          clear_active_dispatch_id: !!opts.clear_active_dispatch_id
        });
        if (!result.repaired) {
          agentdesk.log.warn("[deadlock] Repair deferred (" + result.deferred + ") for " + key);
        }
        return result;
      }

      // 먼저: heartbeat가 신선한 working 세션의 카운터를 리셋 (비연속 스톨 누적 방지)
      agentdesk.timeouts.clearDeadlockCountersForFreshSessions(STALE_SCAN_MINUTES);

      // Fix stale working sessions: if status=working but no inflight file exists,
      // the turn has ended but DB wasn't updated. Fix to idle.
      // #219: Increased grace period from 3min to 10min — agents running long tool
      // calls (cargo build, subagents) may not send heartbeats for several minutes.
      var staleWorkingSessions = agentdesk.timeouts.listStaleWorkingSessions(10);
      for (var sw = 0; sw < staleWorkingSessions.length; sw++) {
        var swRow = staleWorkingSessions[sw];
        var swKey = swRow.session_key;
        // A live-pane probe, not session existence: has-session is true for zombie panes.
        var swHost = sessionHost(swKey);
        if (swHost.state === "unknown") continue;
        var inflight;
        try {
          inflight = findRecentInflightForSession(swKey, swHost.tmux_name);
        } catch (e) {
          agentdesk.log.warn("[deadlock] Transient error looking up inflight for " + swKey + ": " + e);
          continue; // transient error, retry next time
        }
        if (swHost.state === "dead" || !inflight) {
          // Fail a pending dispatch before idling, or it is re-delivered as an orphan.
          var swDispId = swRow.active_dispatch_id;
          var swDispStatus = swRow.active_dispatch_status;
          var failDispatch = !!swDispId && (swDispStatus === "pending" || swDispStatus === "dispatched");
          var swResult = repair(swKey, swHost, swRow, {
            fail_dispatch: failDispatch,
            fail_reason: "Stale working session recovery — no active tmux session after 10min",
            clear_active_dispatch_id: true
          });
          if (!swResult.repaired) continue;
          if (swResult.dispatch_error) {
            agentdesk.log.warn("[deadlock] Failed to mark dispatch for " + swKey + ": " + swResult.dispatch_error);
          } else if (failDispatch) {
            if (swResult.dispatch_rows_affected === 0) {
              agentdesk.log.warn("[dispatch.markFailed] no rows affected for " + swDispId + " — already terminal or missing");
            }
            agentdesk.log.warn("[deadlock] Failed stale dispatch " + swDispId + " for session " + swKey);
          }
          agentdesk.log.info("[deadlock] Fixed stale working session → idle: " + swKey);
        }
      }

      // 데드락 의심 세션: sessions.last_heartbeat 기반 판별
      // deadlock-manager 자신의 세션은 제외 (자기 자신을 오탐하는 무한 루프 방지)
      var staleSessions = agentdesk.timeouts.listDeadlockCandidates(STALE_SCAN_MINUTES, 50);
      for (var dl = 0; dl < staleSessions.length; dl++) {
        var sess = staleSessions[dl];
        var deadlockKey = "deadlock_check:" + sess.session_key;
        var dlHost = sessionHost(sess.session_key);
        if (dlHost.state === "unknown") continue;
        var inflight;
        try {
          inflight = findRecentInflightForSession(sess.session_key, dlHost.tmux_name);
        } catch (e) {
          continue;
        }
        agentdesk.kv.delete(deadlockKey);
        if (dlHost.state === "live" && inflight) continue;
        var dlResult = repair(sess.session_key, dlHost, sess, { clear_active_dispatch_id: false });
        if (dlResult.repaired) {
          agentdesk.log.info("[deadlock] Stale working session → idle (no active turn): " + sess.session_key);
        }
      }

      // Clean up deadlock counters for sessions no longer working
      agentdesk.timeouts.cleanupDeadlockCountersForInactiveSessions();

      // Clean up old deadlock history entries (7일 이상)
      var sevenDaysAgo = Date.now() - 7 * 24 * 60 * 60 * 1000;
      agentdesk.timeouts.cleanupDeadlockHistoryBefore(sevenDaysAgo);
    };
};
