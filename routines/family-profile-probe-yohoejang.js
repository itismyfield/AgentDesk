const TARGET_KEY = "yohoejang";
const TARGET_DISCORD_ID = "429955158974136340";
const SLOT_MINUTES = [15, 45];
const WINDOW_START_HOUR = 12;
const WINDOW_END_HOUR = 20;

function kstParts(now) {
  const utcMs = Date.parse(now);
  const kst = new Date(utcMs + 9 * 60 * 60 * 1000);
  return {
    date: kst.toISOString().slice(0, 10),
    hour: kst.getUTCHours(),
    minute: kst.getUTCMinutes(),
    iso: kst.toISOString().replace("Z", "+09:00"),
  };
}

function dailyPlan(checkpoint, today) {
  const existing = checkpoint && checkpoint.plan;
  if (existing && existing.date === today) {
    return existing;
  }
  const hour =
    WINDOW_START_HOUR +
    Math.floor(Math.random() * (WINDOW_END_HOUR - WINDOW_START_HOUR + 1));
  const minute = SLOT_MINUTES[Math.floor(Math.random() * SLOT_MINUTES.length)];
  return { date: today, hour, minute };
}

function withHistory(checkpoint, item) {
  const history = Array.isArray(checkpoint.history) ? checkpoint.history.slice(-199) : [];
  history.push(item);
  return history;
}

function promptFor(targetKey) {
  const today = kstParts(new Date().toISOString()).date;
  return [
    "[family-profile-probe trigger - ADK routine, silent]",
    `target_key=${targetKey}`,
    `case_id=probe-${targetKey}-${today}`,
    "",
    "family-profile-probe skill workflow를 실행하라.",
    "이 턴은 ADK routine이 시작한 headless turn이다. Python launchd timing script는 실행하지 말 것.",
    "반드시 memento에서 오늘 caseId를 먼저 recall해서 message_id=가 이미 있으면 즉시 중단하라.",
    "아직 전송되지 않았으면 memento profile/gap recall -> 질문 작성 -> /api/senddm 전송 -> message_id 포함 probe-history 기록까지 수행하라.",
    "DM 전송 시 idempotency_key는 caseId와 동일하게 사용하고 source_agent/context/ttl_seconds는 넣지 말 것.",
    "채널에는 보이는 응답을 남기지 말 것. 완료/중단 후 최종 assistant 메시지는 NO_REPLY 한 줄만 출력하라.",
  ].join("\n");
}

agentdesk.routines.register({
  name: "family-profile-probe-yohoejang",
  metadata: {
    owner: "family-counsel",
    target_key: TARGET_KEY,
    target_discord_id: TARGET_DISCORD_ID,
    schedule_intent: "15,45 12-20 * * * Asia/Seoul",
  },
  tick(ctx) {
    const checkpoint = ctx.checkpoint || {};
    const now = kstParts(ctx.now);
    const plan = dailyPlan(checkpoint, now.date);
    const nextCheckpoint = Object.assign({}, checkpoint, { plan });

    if (checkpoint.lastTriggeredDate === now.date) {
      return {
        action: "skip",
        reason: "already_triggered_today",
        checkpoint: nextCheckpoint,
        lastResult: `already triggered for ${TARGET_KEY} on ${now.date}`,
      };
    }

    if (now.hour < plan.hour || (now.hour === plan.hour && now.minute < plan.minute)) {
      return {
        action: "skip",
        reason: "before_daily_slot",
        checkpoint: nextCheckpoint,
        result: { targetKey: TARGET_KEY, plan, now },
        lastResult: `waiting for ${TARGET_KEY} daily slot ${plan.hour}:${String(plan.minute).padStart(2, "0")} KST`,
      };
    }

    nextCheckpoint.lastTriggeredDate = now.date;
    nextCheckpoint.lastTriggeredAt = now.iso;
    nextCheckpoint.history = withHistory(checkpoint, {
      targetKey: TARGET_KEY,
      target: TARGET_DISCORD_ID,
      triggeredAt: now.iso,
      plan,
    });

    return {
      action: "agent",
      prompt: promptFor(TARGET_KEY),
      checkpoint: nextCheckpoint,
    };
  },
});
