// Preflight only: kill-tmux still validates the configured origin, capability,
// DNS addresses, and owner. Registry advertisements never establish trust.
module.exports = function createIdleKillOwnerGuard() {
  var warnedOwners = Object.create(null);
  var warnedUnavailable = false;

  return function loadOwnerGuard(apiPort) {
    var snapshot;
    try {
      snapshot = agentdesk.http.get("http://127.0.0.1:" + apiPort + "/api/cluster/nodes");
      if (!snapshot || !snapshot.cluster || !Array.isArray(snapshot.nodes)
          || !Array.isArray(snapshot.cluster.configured_forward_owner_ids)
          || !Object.prototype.hasOwnProperty.call(snapshot.cluster, "local_instance_id")) {
        throw new Error("cluster owner configuration unavailable");
      }
    } catch (error) {
      if (!warnedUnavailable) {
        agentdesk.log.warn("[idle-kill] owner preflight unavailable; skipping idle-kill until recovered: " + error);
        warnedUnavailable = true;
      }
      return null;
    }
    warnedUnavailable = false;

    var cluster = snapshot.cluster;
    var now = Date.now();
    var configured = cluster.configured_forward_owner_ids;
    var nodes = Object.create(null);
    snapshot.nodes.forEach(function(node) { nodes[node.instance_id] = node; });

    function skipReason(owner) {
      // Match session-forwarding's handling of blank owners and non-cluster mode.
      owner = typeof owner === "string" ? owner.trim() : "";
      if (!owner || !cluster.local_instance_id || owner === cluster.local_instance_id) return null;
      if (configured.indexOf(owner) < 0) {
        return "owner node has no cluster.nodes trusted origin; skipping idle-kill until configured";
      }
      var node = nodes[owner];
      var heartbeat = node && node.last_heartbeat_at ? Date.parse(node.last_heartbeat_at) : NaN;
      var ttl = Number(cluster.lease_ttl_secs);
      if (!node || node.status !== "online" || !isFinite(heartbeat) || !isFinite(ttl)
          || ttl < 1 || heartbeat < now - ttl * 1000) {
        return "owner node heartbeat unavailable or expired; skipping idle-kill until recovered";
      }
      return null;
    }

    // Recovery clears the latch even if that owner's sessions left the idle pool.
    Object.keys(warnedOwners).forEach(function(owner) {
      if (!skipReason(owner)) delete warnedOwners[owner];
    });

    function eligible(owner) {
      var reason = skipReason(owner);
      if (!reason) return true;
      owner = owner.trim();
      if (!warnedOwners[owner]) {
        agentdesk.log.warn("[idle-kill] " + owner + ": " + reason);
        warnedOwners[owner] = true;
      }
      return false;
    }
    eligible.retainOwners = function(owners) {
      var active = Object.create(null);
      owners.forEach(function(s) {
        if (typeof s.instance_id === "string") active[s.instance_id.trim()] = true;
      });
      Object.keys(warnedOwners).forEach(function(owner) {
        if (!active[owner]) delete warnedOwners[owner];
      });
    };
    return eligible;
  };
};
