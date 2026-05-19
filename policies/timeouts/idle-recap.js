// Idle-recap notification — every 5-minute cycle (the prompt cache TTL),
// scan main-channel sessions that have been ready-for-input for ≥5 minutes
// and trigger a recap card on each. The dcserver-side handler
// (POST /api/sessions/{key}/idle-recap) does the actual posting: it deletes
// the previous notification (if any), captures the last ~100 lines of the
// tmux scrollback, asks opencode/Haiku for a short summary, and posts the
// new card with the token usage panel. The message is auto-deleted the
// next time the user sends a turn in that channel (see
// `clear_idle_recap_for_channel` in src/services/discord/router/message_handler.rs).
//
// Scope matches idle-kill: main channels only — thread-suffixed sessions
// (auto-queue / manual threads) are excluded both server-side and client-side.
module.exports = function attachIdleRecap(timeouts, helpers) {
  var parseSessionThreadId = helpers.parseSessionThreadId;

  timeouts._section_R_idle_recap = function() {
      var apiPort = agentdesk.config.get("server_port");
      if (!apiPort) {
        agentdesk.log.error("[idle-recap] server_port missing — cannot call recap API");
        return;
      }

      // Candidates: idle sessions where the active dispatch is finished
      // (or never started) and the last heartbeat is >5 minutes old.
      // 5min = Anthropic prompt-cache TTL, so this is the natural beat
      // to refresh the user with a recap + token snapshot.
      var mainChannelSqlGuard =
        "AND thread_channel_id IS NULL " +
        "AND session_key !~ '-t[0-9]{15,}(-dev)?$' ";
      // Cadence: fire at most once per idle period. We treat a recap as
      // "still current" when it was posted *after* the most recent user
      // activity (last_heartbeat), and only re-arm once the user actually
      // comes back — at which point dispatch bumps last_heartbeat past
      // idle_recap_posted_at, making the next idle period eligible again.
      //
      // Why: the original design re-posted every 5 min (deleting the
      // previous card), which felt like a recurring notification to the
      // user even though only one card was visible at a time. A single
      // recap per idle period is the desired UX.
      var candidates = agentdesk.db.query(
        "SELECT session_key, provider, thread_channel_id, last_heartbeat, " +
        "idle_recap_message_id, idle_recap_posted_at " +
        "FROM sessions " +
        "WHERE status = 'idle' " +
        "AND provider IN ('claude', 'codex', 'qwen') " +
        "AND active_dispatch_id IS NULL " +
        mainChannelSqlGuard +
        "AND COALESCE(last_heartbeat, created_at) <= NOW() - INTERVAL '5 minutes' " +
        // Re-arm only after the user has been active since the last recap.
        // last_heartbeat advances on every dispatch (see dispatch_cancel /
        // dispatch_status / lifecycle watchers), so once the user sends a
        // turn, heartbeat > idle_recap_posted_at and the next idle period
        // becomes eligible. Until then, the existing card is treated as
        // still serving its purpose.
        "AND (idle_recap_posted_at IS NULL " +
        "     OR idle_recap_posted_at < COALESCE(last_heartbeat, created_at)) " +
        "ORDER BY COALESCE(last_heartbeat, created_at) ASC LIMIT 50"
      );

      // Defense-in-depth: even if the SQL guard regresses, drop any
      // thread-suffixed rows client-side.
      candidates = candidates.filter(function(s) {
        return !s.thread_channel_id
          && !parseSessionThreadId(s.session_key, s.provider);
      });

      var processed = {};
      var triggeredCount = 0;
      var maxTriggers = 10;

      for (var i = 0; i < candidates.length; i++) {
        if (triggeredCount >= maxTriggers) {
          agentdesk.log.info(
            "[idle-recap] Reached max " + maxTriggers + " triggers per cycle. Breaking early."
          );
          break;
        }
        var s = candidates[i];
        if (!s.session_key || processed[s.session_key]) continue;
        processed[s.session_key] = true;

        try {
          var url = "http://127.0.0.1:" + apiPort +
            "/api/sessions/" + encodeURIComponent(s.session_key) + "/idle-recap";
          var resp = agentdesk.http.post(url, { retry: false });
          if (resp && resp.ok) {
            triggeredCount++;
            if (resp.posted) {
              agentdesk.log.info(
                "[idle-recap] Posted recap for " + s.session_key
              );
            } else if (resp.accepted) {
              agentdesk.log.info(
                "[idle-recap] Accepted recap job for " + s.session_key
              );
            } else if (resp.skipped) {
              agentdesk.log.info(
                "[idle-recap] Skipped " + s.session_key + " — " + (resp.reason || "no recap needed")
              );
            }
          } else {
            agentdesk.log.error(
              "[idle-recap] API failed for " + s.session_key + ": " + JSON.stringify(resp)
            );
          }
        } catch (e) {
          agentdesk.log.error(
            "[idle-recap] API exception for " + s.session_key + ": " + e
          );
        }
      }
    };
};
