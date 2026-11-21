"""E-54 on Herdr cells: Discord !stop while generating (A) and while idle (B), then busy input (C).

Each case runs in its own setup window through the Herdr runner. A passes on one stop
acknowledgement, an idle reply well inside the hold and no late sentinel; that reply is B.
C stays a known gap (GAP) until a follow-up sent during a hold turn is delivered.
"""

GAP = "#8128 U"
# Herdr's sent-key reply (turn_bridge/tmux_runtime/codex_stop_delivery.rs); key delivery is not settlement.
STOPPING = "중지 키를 보냈어요. 작업 종료 기록을 기다려요."
INTERRUPTED = "⏹ 중단"
NO_ACTIVE = "중지할 활성 턴이 없어요."
LATE = "S15_LATE_SHOULD_NOT_APPEAR"
REFUSED = "이 세션의 호스트를 확인하지 못해 중지하지 않았어요."
# Input notices as worded in queue_io/retained_notice.rs and commands/tui_passthrough.rs.
DROPPED = ("전달하지 않", "원문은 보관하지 않")
RETAINED = "원문을 보관했습니다"
QUEUED = "큐에 들어갔습니다"
HOLD_S = 90
LIVE = {"agent_mode": "real_live", "coverage_class": "live", "destructive": True}


def hold_prompt(tag, seconds, done):
    return (f"E2E S15 {tag} probe. First output [E2E:{tag}:{{run_id}}:HOLD].\n"
            "Then call Bash in the foreground with run_in_background=false:\n"
            f'python3 -c "import time; time.sleep({seconds})"\n'
            "Keep the turn active while that command runs. Do not launch a background task.\n"
            f"Only after the command finishes normally, output exactly {done}.\n"
            "If interrupted or cancelled, stop immediately and output nothing more.")


def cases():
    hold_ab, hold_c = "[E2E:S15H:{run_id}:HOLD]", "[E2E:S15C:{run_id}:HOLD]"
    done_c, follow = "[E2E:S15C:{run_id}:HOLD_DONE]", "[E2E:S15C:{run_id}:FOLLOW]"
    ab = {"id": "E-54-AB", **LIVE, "settle_s": HOLD_S,
          "steps": [{"send_discord_prompt": hold_prompt("S15H", HOLD_S, LATE)},
                    {"wait_for_discord_text": hold_ab, "timeout_s": 120}, {"wait_idle_s": 2},
                    {"send_text_stop": {"ack": STOPPING}}, {"wait_idle_s": 15},
                    # Sent long before the hold could end, so its idle reply is the early-idle witness.
                    {"send_discord_prompt": "!stop [E2E:S15H:{run_id}:IDLE_STOP]"},
                    {"wait_for_raw_discord_text": NO_ACTIVE, "timeout_s": 20}, {"wait_idle_s": 80}],
          "assertions": [{"text_present": hold_ab}, {"no_duplicate_marker": hold_ab},
                         {"marker_absent": {"marker": LATE, "surface": "relay"}},
                         {"chrome_count": {"text": NO_ACTIVE, "exact": 1}},
                         {"raw_text_absent": REFUSED}, {"no_duplicate_content": True}],
          "report_marker_counts": [hold_ab, LATE], "report_raw_text_counts": [STOPPING, NO_ACTIVE, INTERRUPTED]}
    c = {"id": "E-54-C", **LIVE,
         "steps": [{"send_discord_prompt": hold_prompt("S15C", HOLD_S, done_c)},
                   {"wait_for_discord_text": hold_c, "timeout_s": 120}, {"wait_idle_s": 3},
                   {"send_discord_prompt": f"E2E S15C busy follow-up. Reply with exactly {follow}"},
                   {"wait_for_discord_text": follow, "timeout_s": 240}],
         "assertions": [{"text_present": follow}, {"no_duplicate_marker": follow},
                        *({"raw_text_absent": needle} for needle in DROPPED), {"no_duplicate_content": True}],
         "report_marker_counts": [done_c, follow], "report_raw_text_counts": [*DROPPED, RETAINED, QUEUED]}
    return [ab, c]
