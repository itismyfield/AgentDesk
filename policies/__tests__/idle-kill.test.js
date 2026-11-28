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
      if (pages === 2) return { error: "read response: Resource temporarily unavailable (os error 35)", deadline_exceeded: true };
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

test("idle-kill reports a preflight deadline without classifying or posting candidates", () => {
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    config: { server_port: 8791 },
    httpGet() { return { error: "socket timeout", deadline_exceeded: true }; }
  });

  policy.onTick5min({});

  assert.equal(state.idleCleanupChecks.length, 0);
  assert.equal(state.httpPosts.length, 0);
  assert.deepEqual(state.logs.warn.filter((line) => line.includes("[idle-kill]")), [
    "[idle-kill] kill-tmux deadline reached; remaining=unknown"
  ]);
  assert.equal(state.logs.error.length, 0);
});

test("onTick5min settles a slow G query before idle-kill can issue a POST", () => {
  let protectedWork = false;
  let consumed = false;
  let hitG = false;
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    config: { server_port: 8791 },
    withWorkDeadline(reserveMs, work) {
      assert.equal(reserveMs, 200);
      if (consumed) return true;
      protectedWork = true;
      try { return work() || consumed; }
      finally { protectedWork = false; }
    },
    dbQuery(sql) {
      if (!sql.includes("INTERVAL '6 hours'") && sql.includes("FROM task_dispatches WHERE status IN")) {
        hitG = true;
        consumed = true;
        throw Object.assign(new Error("PG deadline"), { deadline_exceeded: protectedWork });
      }
      return [];
    },
    httpGet() {
      assert.equal(consumed, false, "QuickJS has no time left for owner preflight");
    }
  });

  policy.onTick5min({});

  assert.equal(hitG, true);
  assert.deepEqual(state.logs.warn, ["[idle-kill] kill-tmux deadline reached; remaining=unknown"]);
  assert.equal(state.httpGets.length, 0);
  assert.equal(state.httpPosts.length, 0);
});

test("idle-kill advances past a repeatedly timed-out POST and rechecks it on the next sweep", () => {
  const rows = orderedRows([idleRow("slow"), idleRow("ready")]);
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    config: { server_port: 8791 },
    dbQuery(sql, params) { return readPage(rows, sql, params); },
    httpPost(url) {
      if (url.includes("-slow/")) {
        return { error: "read response: Resource temporarily unavailable (os error 35)", deadline_exceeded: true, request_started: true };
      }
      return { ok: true, tmux_was_alive: true, tmux_killed: true };
    }
  });

  const [slow, ready] = rows.map(({ session_key }) => session_key);
  const targets = () => state.httpPosts.map(({ url }) => decodeURIComponent(url.split("/").at(-2)));
  policy.onTick5min({});
  assert.deepEqual(targets(), [slow]);
  policy.onTick5min({});
  assert.deepEqual(targets(), [slow, ready]);
  policy.onTick5min({});
  assert.deepEqual(targets(), [slow, ready, slow]);
  assert.deepEqual(state.idleCleanupChecks.map(({ keys }) => keys), [[slow, ready], [ready], [slow, ready]]);
  assert.deepEqual(state.logs.warn.filter((line) => line.includes("deadline reached")), [
    "[idle-kill] kill-tmux deadline reached; remaining=1",
    "[idle-kill] kill-tmux deadline reached; remaining=1"
  ]);
  assert.equal(state.logs.error.length, 0);
});

test("idle-kill retries the unsent candidate first on the next tick", () => {
  const rows = orderedRows([idleRow("first"), idleRow("deferred"), idleRow("later")]);
  let actualPosts = [];
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    config: { server_port: 8791 },
    dbQuery(sql, params) { return readPage(rows, sql, params); },
    httpPost(url) {
      const key = decodeURIComponent(url.split("/").at(-2));
      if (key === rows[1].session_key && actualPosts.length) {
        return { error: "work cutoff before send", deadline_exceeded: true, request_started: false };
      }
      actualPosts.push(key);
      return { ok: true, skipped_provider_activity_guard: true };
    }
  });

  for (let cycle = 0; cycle < 3; cycle++) {
    actualPosts = [];
    policy.onTick5min({});
    assert.deepEqual(actualPosts, [rows[0].session_key]);
    actualPosts = [];
    policy.onTick5min({});
    assert.deepEqual(actualPosts, [rows[1].session_key, rows[2].session_key]);
  }
  assert.equal(state.logs.warn.filter((line) => line.includes("deadline reached")).length, 3);
  assert.equal(state.logs.error.length, 0);
});

for (const phase of ["agents", "owners", "page", "auxiliary"]) {
  test(`idle-kill settles the ${phase} DB deadline and resumes on the next tick`, () => {
    const rows = orderedRows([
      { ...idleRow("lookup"), active_dispatch_id: "fixture-dispatch" },
      idleRow("ready")
    ]);
    let delayed = false;
    const { policy, state } = loadPolicy("policies/timeouts.js", {
      config: { server_port: 8791 },
      dbQuery(sql, params, _state, reserveMs) {
        const stage = sql.includes("FROM agents") ? "agents"
          : sql.startsWith("SELECT DISTINCT s.instance_id") ? "owners"
          : sql.includes("idle_cursor_at") ? "page"
          : sql.startsWith("SELECT to_agent_id FROM task_dispatches") ? "auxiliary" : null;
        if (reserveMs === 200 && stage === phase && !delayed) {
          delayed = true;
          throw Object.assign(new Error("work cutoff"), { deadline_exceeded: true });
        }
        return readPage(rows, sql, params);
      },
      httpPost() { return { ok: true, tmux_was_alive: true, tmux_killed: true }; }
    });
    const targets = () => state.httpPosts.map(({ url }) => decodeURIComponent(url.split("/").at(-2)));
    const summaries = () => state.logs.warn.filter((line) => line.includes("deadline reached"));

    policy.onTick5min({});
    assert.equal(delayed, true);
    assert.equal(summaries().length, 1);
    assert.deepEqual(targets(), phase === "auxiliary" ? [rows[0].session_key] : []);
    policy.onTick5min({});
    assert.deepEqual(targets(), rows.map(({ session_key }) => session_key));
    assert.equal(summaries().length, 1);
    assert.equal(state.logs.error.length, 0);
  });
}

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
        return { error, deadline_exceeded: true };
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
