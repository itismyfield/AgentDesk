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

function orderedRows(rows) {
  return rows.map((row, index) => ({
    ...row, idle_cursor_at: row.last_seen_at, idle_cursor_id: String(index + 1)
  }));
}

function readPage(rows, sql, params) {
  if (!sql.includes("INTERVAL '6 hours'")) return [];
  if (sql.startsWith("SELECT DISTINCT s.instance_id")) return [{ instance_id: null }];
  const selected = sql.includes("s.hosted_execution IS NULL") ? rows.filter((row) => !row.hosted_execution) : rows;
  const after = sql.includes("::bigint") ? Number(params.at(-1)) : 0;
  const limit = Number(sql.match(/LIMIT (\d+)$/)[1]);
  return selected.filter((row) => Number(row.idle_cursor_id) > after).slice(0, limit);
}

test("idle-kill tick selects legacy tmux beyond older hosted and marker-only Herdr pages", () => {
  const legacy = idleRow("legacy");
  const hosted = Array.from({ length: 18 }, (_, index) => ({
    ...idleRow("herdr-" + index), hosted_execution: { location: { host: "herdr" } }
  }));
  const markers = Array.from({ length: 52 }, (_, index) => idleRow("marker-" + index));
  const rows = orderedRows([...markers, ...hosted, legacy]);
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    config: { server_port: 8791 },
    dbQuery(sql, params) {
      return readPage(rows, sql, params);
    },
    idleCleanupCandidates({ keys }) {
      return { decisions: Object.fromEntries(keys.map((key) => [key, key === legacy.session_key
        ? { supported: true }
        : { supported: false, reason: "herdr_idle_cleanup_requires_core" }])) };
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
  assert.deepEqual(state.idleCleanupChecks.map(({ keys }) => keys.length), [64, 7]);
  assert.ok(state.logs.debug.some((line) => line.includes("herdr_idle_cleanup_requires_core")));
});

test("idle-kill resumes after the last classified page when the next page times out", () => {
  const legacy = idleRow("legacy");
  const rows = orderedRows([
    ...Array.from({ length: 64 }, (_, index) => idleRow("marker-" + index)), legacy
  ]);
  let pages = 0;
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    config: { server_port: 8791 },
    dbQuery(sql, params) { return readPage(rows, sql, params); },
    idleCleanupCandidates({ keys }) {
      pages++;
      if (pages === 1) return { decisions: {} };
      if (pages === 2) return { error: "bridge deadline passed before http.post started" };
      return { decisions: { [keys[0]]: { supported: true } } };
    },
    httpPost() { return { ok: true, tmux_killed: true }; }
  });

  policy.onTick5min({});
  assert.equal(state.httpPosts.length, 0);
  assert.deepEqual(state.logs.warn.filter((line) => line.includes("deadline reached")), [
    "[idle-kill] kill-tmux deadline reached; remaining=1"
  ]);
  assert.equal(state.logs.error.length, 0);
  policy.onTick5min({});

  assert.deepEqual(state.idleCleanupChecks.map(({ keys }) => keys.length), [64, 1, 1]);
  assert.equal(state.httpPosts.length, 1);
  assert.match(state.httpPosts[0].url, /-legacy\/kill-tmux$/);
});

test("idle-kill caps supported candidates at 50 across read pages", () => {
  const rows = orderedRows(Array.from({ length: 120 }, (_, index) => idleRow(String(index))));
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    config: { server_port: 8791 },
    dbQuery(sql, params) { return readPage(rows, sql, params); },
    idleCleanupCandidates({ keys }) {
      return { decisions: Object.fromEntries(keys.map((key) => [key, {
        supported: Number(key.split("-").at(-1)) >= 15
      }])) };
    },
    httpPost() { return { ok: true, tmux_was_alive: false }; }
  });

  policy.onTick5min({});

  assert.deepEqual(state.idleCleanupChecks.map(({ keys }) => keys.length), [64, 56]);
  assert.equal(state.httpPosts.length, 50);
  assert.match(state.httpPosts[0].url, /-15\/kill-tmux$/);
  assert.match(state.httpPosts.at(-1).url, /-64\/kill-tmux$/);
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
        return sql.startsWith("SELECT s.session_key") && sql.includes("INTERVAL '6 hours'")
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
