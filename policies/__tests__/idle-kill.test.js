const test = require("node:test");
const assert = require("node:assert/strict");

const { loadPolicy } = require("./support/harness");

function idleRow(name) {
  return {
    session_key: "provider:AgentDesk-claude-" + name,
    provider: "claude",
    instance_id: null,
    thread_channel_id: null,
    last_seen_at: "2000-01-02 00:00:00"
  };
}

test("idle-kill tick selects legacy tmux behind a full batch of older hosted Herdr rows", () => {
  const legacy = idleRow("legacy");
  const hosted = Array.from({ length: 50 }, (_, index) => ({
    ...idleRow("herdr-" + index),
    hosted_execution: { location: { host: "herdr" } },
    last_seen_at: "2000-01-01 00:00:00"
  }));
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    config: { server_port: 8791 },
    dbQuery(sql) {
      if (!sql.includes("INTERVAL '6 hours'")) return [];
      const rows = sql.includes("AND s.hosted_execution IS NULL ") ? [legacy] : [...hosted, legacy];
      if (sql.startsWith("SELECT DISTINCT s.instance_id")) return [{ instance_id: null }];
      return rows.slice(0, 50);
    },
    httpPost(url) {
      if (url.includes("-herdr-")) {
        return { ok: true, tmux_was_alive: null, skipped_provider_activity_guard: true,
          preserved_reason: "host_not_legacy_tmux" };
      }
      return { ok: true, tmux_was_alive: true, tmux_killed: true };
    }
  });

  policy.onTick5min({});

  assert.deepEqual(state.httpPosts.map(({ url }) => url), [
    `http://127.0.0.1:8791/api/sessions/${encodeURIComponent(legacy.session_key)}/kill-tmux`
  ]);
  const queries = state.queries.map(({ sql }) => sql).filter((sql) => sql.includes("INTERVAL '6 hours'"));
  assert.equal(queries.length, 2);
  assert.match(queries[0], /^SELECT DISTINCT s\.instance_id .*AND s\.hosted_execution IS NULL /);
  assert.match(queries[1], /AND s\.hosted_execution IS NULL .*ORDER BY latest\.last_seen_at ASC LIMIT 50$/);
});

for (const error of [
  "bridge deadline passed before http.post started",
  "response deadline exceeded",
  "connect deadline exceeded (last attempt: timed out)"
]) {
  test(`idle-kill tick stops on ${error} and reports the remaining candidates once`, () => {
    const { policy, state } = loadPolicy("policies/timeouts.js", {
      config: { server_port: 8791 },
      dbQuery(sql) {
        return sql.endsWith("LIMIT 50") && sql.includes("INTERVAL '6 hours'")
          ? [1, 2, 3, 4, 5].map((index) => idleRow("deadline-" + index))
          : [];
      },
      httpPost(url) {
        if (url.includes("-deadline-1") || url.includes("-deadline-2")) {
          return { error: "status code 500" };
        }
        return { error };
      }
    });

    policy.onTick5min({});

    assert.equal(state.httpPosts.length, 3);
    const errors = state.logs.error.filter((line) => line.includes("[idle-kill] kill-tmux API failed"));
    assert.equal(errors.length, 1);
    assert.match(errors[0], /deadline-1.*status code 500/);
    const summaries = state.logs.warn.filter((line) => line.includes("[idle-kill] kill-tmux deadline reached"));
    assert.equal(summaries.length, 1);
    assert.match(summaries[0], /remaining=2$/);
  });
}
