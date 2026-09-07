const test = require("node:test");
const assert = require("node:assert/strict");
const { loadPolicy } = require("./support/harness");

const NOW = Date.parse("2026-09-07T00:00:00Z");
class Clock extends Date { static now() { return NOW; } }
function node(owner, age = 0, status = "online") {
  return { instance_id: owner, status, last_heartbeat_at: new Date(NOW - age).toISOString(),
    api_base_url: "https://advertised.example:8791", capabilities: { agentdesk_api: { session_forwarding: true } } };
}
function row(owner, index = 0) {
  return { instance_id: owner, session_key: `codex/host:AgentDesk-codex-idle-${index}`,
    provider: "codex", thread_channel_id: null, last_seen_at: "2000-01-01T00:00:00Z" };
}
function fixture(rows, configured = [], nodes = []) {
  const data = { rows, now: NOW, snapshot: { cluster: { local_instance_id: "leader",
    configured_forward_owner_ids: configured, lease_ttl_secs: 30 }, nodes } };
  const harness = loadPolicy("policies/timeouts.js", {
    config: { server_port: 8791 }, globals: { Date: class extends Clock { static now() { return data.now; } } },
    httpGet(url) {
      assert.equal(url, "http://127.0.0.1:8791/api/cluster/nodes");
      if (data.error) throw new Error("unavailable");
      return data.snapshot;
    },
    dbQuery(sql, params) {
      if (sql.startsWith("SELECT DISTINCT s.instance_id")) {
        assert.doesNotMatch(sql, /LIMIT/);
        return [...new Set(data.rows.map(s => s.instance_id))].map(instance_id => ({ instance_id }));
      }
      if (sql.includes("FROM sessions s")) {
        assert.match(sql, /ORDER BY latest.last_seen_at ASC LIMIT 50$/);
        if (params.length) assert.match(sql, /BTRIM\(s.instance_id\) NOT IN \(\?(, \?)*\)/);
        return data.rows.filter(s => !params.includes((s.instance_id || "").trim())).slice(0, 50);
      }
      return [];
    },
    httpPost() { return { ok: true, tmux_killed: true }; }
  });
  return { ...harness, data };
}

test("missing trusted origin warns once per owner, never trusts registry, and recovers", () => {
  const f = fixture([row("worker", 1), row("worker", 2)], [], [node("worker")]);
  for (let i = 0; i < 288; i++) f.policy._section_O();
  assert.equal(f.state.httpPosts.length, 0);
  assert.equal(f.state.logs.warn.length, 1);
  assert.match(f.state.logs.warn[0], /no cluster.nodes trusted origin/);
  assert.deepEqual(f.state.logs.error, []);
  f.data.snapshot.cluster.configured_forward_owner_ids = ["worker"];
  f.policy._section_O();
  assert.equal(f.state.httpPosts.length, 2);
  f.data.snapshot.cluster.configured_forward_owner_ids = [];
  f.policy._section_O();
  assert.equal(f.state.logs.warn.length, 2);
});

test("offline, expired and missing heartbeat owners warn once then resume when heartbeat returns", () => {
  for (const peer of [node("worker", 30001), node("worker", 0, "offline"),
    { ...node("worker"), last_heartbeat_at: null }, null]) {
    const f = fixture([row("worker")], ["worker"], peer ? [peer] : []);
    f.policy._section_O(); f.policy._section_O();
    assert.equal(f.state.httpPosts.length, 0);
    assert.equal(f.state.logs.warn.length, 1);
    assert.match(f.state.logs.warn[0], /heartbeat unavailable or expired/);
    f.data.snapshot.nodes = [node("worker")];
    f.policy._section_O();
    assert.equal(f.state.httpPosts.length, 1);
    f.data.snapshot.nodes = [node("worker", 30001)];
    f.policy._section_O();
    assert.equal(f.state.logs.warn.length, 2);
    assert.deepEqual(f.state.logs.error, []);
  }
});

test("heartbeat TTL boundary is inclusive, one millisecond older is excluded", () => {
  for (const [age, calls] of [[29999, 1], [30000, 1], [30001, 0]]) {
    const f = fixture([row("worker")], ["worker"], [node("worker", age)]);
    f.policy._section_O();
    assert.equal(f.state.httpPosts.length, calls, `heartbeat age ${age}`);
  }
});

test("excluded owners do not fill the 50-row window or spend the three-kill budget", () => {
  const f = fixture([...Array.from({ length: 60 }, (_, i) => row("offline", i)),
    row("leader", 60), row(null, 61), row("healthy", 62), row("healthy", 63)],
    ["offline", "healthy"], [node("offline", 30001), node("healthy")]);
  f.policy._section_O();
  assert.equal(f.state.httpPosts.length, 3);
  assert.ok(f.state.httpPosts.every(p => /idle-6[012]/.test(p.url)));
  const query = f.state.queries.find(q => q.sql.startsWith("SELECT s.session_key"));
  assert.deepEqual(query.params, ["offline"]);
});

test("local, ownerless and non-cluster sessions retain their existing cleanup behavior", () => {
  const f = fixture([row("leader", 1), row(null, 2), row("  ", 3)]);
  f.policy._section_O();
  assert.equal(f.state.httpPosts.length, 3);
  assert.equal(f.state.logs.warn.length, 0);
  f.data.snapshot.cluster.local_instance_id = null;
  f.data.rows = [row("historical-owner", 4)];
  f.policy._section_O();
  assert.equal(f.state.httpPosts.length, 4);
});

test("owner IDs are bound as SQL data, including quotes and SQL-looking text", () => {
  const owner = "peer'); SELECT 'unexpected'; --";
  const f = fixture([row(owner), row("leader", 1)]);
  f.policy._section_O();
  const query = f.state.queries.find(q => q.sql.startsWith("SELECT s.session_key"));
  assert.deepEqual(query.params, [owner]);
  assert.ok(!query.sql.includes(owner));
  assert.equal(f.state.httpPosts.length, 1);
});

test("preflight failure suppresses calls, warns once and retries without caching failure", () => {
  const f = fixture([row("leader")]);
  f.data.error = true;
  f.policy._section_O(); f.policy._section_O();
  assert.equal(f.state.httpPosts.length, 0);
  assert.equal(f.state.logs.warn.length, 1);
  f.data.error = false;
  f.policy._section_O();
  assert.equal(f.state.httpPosts.length, 1);
  f.data.snapshot = { nodes: [node("leader")] };
  f.policy._section_O(); f.policy._section_O();
  assert.equal(f.state.logs.warn.length, 2);
});

test("warning state follows current owners and resets after absent-owner recovery", () => {
  const f = fixture([row("old")]);
  f.policy._section_O();
  f.data.rows = [row("new")]; f.policy._section_O();
  f.data.rows = [row("old")]; f.policy._section_O();
  assert.equal(f.state.logs.warn.length, 3, "owners removed from candidate pool release their warning latch");
  f.data.rows = [];
  f.data.snapshot.cluster.configured_forward_owner_ids = ["old"];
  f.data.snapshot.nodes = [node("old")];
  f.policy._section_O();
  f.data.rows = [row("old")];
  f.data.snapshot.nodes = [];
  f.policy._section_O();
  assert.equal(f.state.logs.warn.length, 4);
});

test("an old binary missing http.get repeats unavailable WARN hourly and resets after recovery", () => {
  const f = fixture([row("leader")]);
  const get = f.agentdesk.http.get;
  f.agentdesk.http.get = undefined;
  f.policy._section_O();
  f.data.now = NOW + 60 * 60 * 1000 - 1;
  f.policy._section_O();
  assert.equal(f.state.logs.warn.length, 1);
  f.data.now++;
  f.policy._section_O();
  assert.equal(f.state.logs.warn.length, 2);
  assert.equal(f.state.httpPosts.length, 0);
  f.agentdesk.http.get = get;
  f.policy._section_O();
  assert.equal(f.state.httpPosts.length, 1);
  f.agentdesk.http.get = undefined;
  f.policy._section_O();
  assert.equal(f.state.logs.warn.length, 3);
  assert.deepEqual(f.state.logs.error, []);
});
