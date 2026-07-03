const TARGET_KEY = "obujang";
const TARGET_DISCORD_ID = "343742347365974026";
const SLOT_MINUTES = [0, 30];
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

function withPendingDelivery(checkpoint, pendingDelivery) {
  return Object.assign({}, checkpoint, {
    plan: pendingDelivery.plan,
    // The routine executor turns this into lastTriggeredDate only after
    // assistant delivery is confirmed; failed/no-reply runs must retry today.
    pendingDelivery,
  });
}

function promptFor(targetKey) {
  const today = kstParts(new Date().toISOString()).date;
  return [
    "[family-profile-probe trigger - ADK routine, silent]",
    `target_key=${targetKey}`,
    `case_id=probe-${targetKey}-${today}`,
    "",
    "family-profile-probe skill workflow를 실행하라.",
    "이 턴은 ADK routine이 대상 사용자의 DM 채널에서 직접 시작한 headless turn이다. Python launchd timing script와 /api/senddm은 실행하지 말 것.",
    "반드시 memento에서 오늘 caseId를 먼저 recall해서 dm_bound_turn 또는 message_id=가 이미 있으면 질문을 다시 보내지 말고 NO_REPLY로 중단하라.",
    "아직 전송되지 않았으면 memento profile/gap recall -> 질문 작성 -> probe-history에 dm_bound_turn 기록 -> 최종 assistant 메시지로 질문 한 줄만 출력하라.",
    "최종 assistant 메시지가 곧 사용자에게 보이는 DM 질문이다. 질문 외 설명, 진행 로그, NO_REPLY를 함께 출력하지 말 것.",
    "사용자의 답변은 같은 DM 세션에서 이어질 수 있으므로 방금 물은 질문을 세션 문맥으로 유지하고, 그래도 답변 처리 때는 memento caseId로 재확인하라.",
  ].join("\n");
}

agentdesk.routines.register({
  name: "family-profile-probe-obujang",
  metadata: {
    owner: "family-counsel",
    target_key: TARGET_KEY,
    target_discord_id: TARGET_DISCORD_ID,
    schedule_intent: "0,30 12-20 * * * Asia/Seoul",
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

    const pendingDelivery = {
      kind: "family-profile-probe",
      targetKey: TARGET_KEY,
      target: TARGET_DISCORD_ID,
      triggerDate: now.date,
      triggeredAt: now.iso,
      plan,
    };

    return {
      action: "agent",
      dmUserId: TARGET_DISCORD_ID,
      prompt: promptFor(TARGET_KEY),
      checkpoint: withPendingDelivery(nextCheckpoint, pendingDelivery),
    };
  },
});
