var triage = {
  name: "triage-rules",
  priority: 300,

  // Periodic: auto-assign unassigned cards based on labels
  onTick: function() {
    // Find backlog cards without assigned agent
    var unassigned = agentdesk.cards.list({
      status: "backlog",
      unassigned: true,
      metadata_present: true
    });

    for (var i = 0; i < unassigned.length; i++) {
      var card = unassigned[i];
      var metadata = card.metadata || {};
      if (!metadata || typeof metadata !== "object") continue;

      var labels = (metadata.labels || "").toLowerCase();

      // Auto-assign based on agent label in metadata
      var agentMatch = labels.match(/agent:([a-z0-9_-]+)/);
      if (agentMatch) {
        var agentId = agentMatch[1];
        // Try exact match first, then with ch- prefix
        var agent = agentdesk.agents.get(agentId) || agentdesk.agents.get("ch-" + agentId);
        if (agent) {
          agentId = agent.id;
          agentdesk.cards.assign(card.id, agentId);
          agentdesk.log.info("[triage] Auto-assigned card " + card.id + " to " + agentId);
        }
      }

      // If no agent label found, request PMD classification (async)
      if (!agentMatch) {
        // Check if we already requested classification (avoid spam)
        var triageKey = "triage_requested:" + card.id;
        var alreadyRequested = agentdesk.kv.get(triageKey);
        if (alreadyRequested === null) {
          agentdesk.kv.set(triageKey, new Date().toISOString());
          // Send classification request to PMD via announce bot
          if (card.github_issue_url) {
            var pmdCh = agentdesk.config.get("kanban_manager_channel_id");
            if (pmdCh) {
              var issueLink = "[" + card.title + " #" + (card.github_issue_number || "?") + "](<" + card.github_issue_url + ">)";
              agentdesk.message.queue(
                "channel:" + pmdCh,
                "📋 Triage 분류 요청: " + issueLink,
                "announce",
                "system"
              );
              agentdesk.log.info("[triage] PMD classification requested for " + card.id);
            }
          }
        }
      }

      // Auto-set priority based on labels
      if (labels.indexOf("priority:urgent") >= 0 || labels.indexOf("critical") >= 0) {
        if ((card.priority || "medium") === "medium") {
          agentdesk.cards.setPriority(card.id, "urgent");
        }
      } else if (labels.indexOf("priority:high") >= 0) {
        if ((card.priority || "medium") === "medium") {
          agentdesk.cards.setPriority(card.id, "high");
        }
      } else if (labels.indexOf("priority:low") >= 0) {
        if ((card.priority || "medium") === "medium") {
          agentdesk.cards.setPriority(card.id, "low");
        }
      }
    }
  }
};

agentdesk.registerPolicy(triage);
