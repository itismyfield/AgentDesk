#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/launchd-migrated/_portable-resolver.sh
source "$SCRIPT_DIR/_portable-resolver.sh"
agentdesk_source_portable_resolver

NOW_KST="$(TZ=Asia/Seoul date '+%Y-%m-%d %H:%M:%S %Z')"
TODAY_FROM="$(TZ=Asia/Seoul date '+%Y-%m-%d')T00:00:00+09:00"
TOMORROW_FROM="$(TZ=Asia/Seoul date -v+1d '+%Y-%m-%d')T00:00:00+09:00"

SCRIPTS_DIR="$AGENTDESK_OBSIDIAN_SKILL_ROOT/family-morning-briefing/scripts"
DATA_DIR="$(mktemp -d)"
PROMPT_FILE="$(mktemp)"
trap 'rm -rf "$DATA_DIR" "$PROMPT_FILE"' EXIT
agentdesk_optional_dir_or_skip "family-morning-briefing scripts" "$SCRIPTS_DIR"

# 1. Weather + AQI
python3 "$SCRIPTS_DIR/daily_ai_briefing_data.py" \
  --location "서울 삼성동" --location2 "성남시 수정구 고등동" \
  --max-items 0 \
  > "$DATA_DIR/weather.json" 2>/dev/null || echo '{"error":"weather fetch failed"}' > "$DATA_DIR/weather.json"

# 2. Calendar (본인=janelley94 / 배우자=primary / 가족)
gog cal events janelley94@gmail.com --from "$TODAY_FROM" --to "$TOMORROW_FROM" --account itismyfield@gmail.com --client my-personal --plain --no-input \
  > "$DATA_DIR/cal_self.txt" 2>/dev/null || echo "조회 실패" > "$DATA_DIR/cal_self.txt"

gog cal events primary --from "$TODAY_FROM" --to "$TOMORROW_FROM" --account itismyfield@gmail.com --client my-personal --plain --no-input \
  > "$DATA_DIR/cal_spouse.txt" 2>/dev/null || echo "조회 실패" > "$DATA_DIR/cal_spouse.txt"

gog cal events family03910667166220074979@group.calendar.google.com --from "$TODAY_FROM" --to "$TOMORROW_FROM" --account itismyfield@gmail.com --client my-personal --plain --no-input \
  > "$DATA_DIR/cal_family.txt" 2>/dev/null || echo "조회 실패" > "$DATA_DIR/cal_family.txt"

# 3. Family reminders
python3 "$SCRIPTS_DIR/fetch_family_reminders.py" --list "가족" --timeout 8 \
  > "$DATA_DIR/reminders.json" 2>/dev/null || echo '{"ok":false,"error":"script failed"}' > "$DATA_DIR/reminders.json"

# Build prompt with all pre-fetched data
cat >"$PROMPT_FILE" <<'HEREDOC_START'
아래 데이터를 바탕으로 요회장 모닝 브리핑 메시지를 작성해.
데이터 수집은 이미 완료되었으니, 추가 명령을 실행하지 마.
포맷팅 규칙만 따라서 메시지를 작성해.

## 포맷팅 규칙
- 한국어, 친근하고 자연스러운 톤 (약 12~24줄)
- 짧은 아침 인사로 시작
- 가벼운 이모지 사용 (과하지 않게): ☀️🌤️📅👨‍💼👩‍💼👨‍👩‍👦✅😷
- 모바일 가독성 (한 줄에 한 아이디어, 28자 이내 목표)
- 섹션 구성: 삼성동 날씨, 고등동 날씨, 미세먼지 2지역, 본인 일정, 남편분 일정, 가족 일정, 가족 할 일
- 회사 일정 섹션은 포함하지 않음
- 일정 없으면: "등록된 일정 없음"
- 캘린더 조회 실패 시: "조회 실패"
- 리마인더 ok=false 이면: "가족 할 일: 조회 실패"
- 리마인더 ok=true, 0건이면: "가족 할 일: 할일 없음"
- 리마인더 항목 있으면: 기한 있는 할 일 / 시간 될 때 챙길 할 일로 분리 (각 최대 3건, 초과 시 "외 N건")
- 마지막에 "윤호 케어 한줄 팁" 추가
- 장소 있는 일정은 자연스럽게 괄호로 표시: (장소: XX)

HEREDOC_START

# Append actual data
{
  echo "## 현재 시각"
  echo "$NOW_KST"
  echo ""
  echo "## 날씨/미세먼지 데이터 (JSON)"
  cat "$DATA_DIR/weather.json"
  echo ""
  echo ""
  echo "## 본인 캘린더 (janelley94)"
  cat "$DATA_DIR/cal_self.txt"
  echo ""
  echo "## 남편분 캘린더 (primary)"
  cat "$DATA_DIR/cal_spouse.txt"
  echo ""
  echo "## 가족 캘린더"
  cat "$DATA_DIR/cal_family.txt"
  echo ""
  echo "## 가족 리마인더 (JSON)"
  cat "$DATA_DIR/reminders.json"
  echo ""
} >> "$PROMPT_FILE"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec "$SCRIPT_DIR/run-claude-message-job.sh" \
  --source "family-morning-briefing:yohoejang" \
  --target "channel:1478248518950060183" \
  --prompt-file "$PROMPT_FILE"
