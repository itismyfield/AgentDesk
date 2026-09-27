# Claude Code shortcut
unalias cc 2>/dev/null
# Managed source: AgentDesk/scripts/session-anchor.py; installed copy is independent
# of development worktrees. Refresh it when that script changes.
_adk_anchor_cli() {
  local helper=${ADK_SESSION_ANCHOR_HELPER:-$HOME/.config/agentdesk/session-anchor.py}
  local anchor_python=${ADK_SESSION_ANCHOR_PYTHON:-/opt/homebrew/bin/python3}
  if [[ -f $helper && -x $anchor_python ]]; then
    "$anchor_python" "$helper" "$@"
  else
    command "$@"
  fi
}
# claude 는 백그라운드 세션 명령을 첫 인자일 때만 인식하므로 앞에 옵션을 붙이지 않는다.
_adk_claude_bg_cmd() { case ${1-} in attach|logs|stop|kill|rm|respawn) return 0 ;; esac; return 1 }
cc() {
  _adk_claude_bg_cmd "$@" && { command claude "$@"; return }
  _adk_anchor_cli claude --dangerously-skip-permissions "$@"
}
# bare `claude`: bypass 모드를 Shift+Tab 순회에 "선택지로만" 노출한다.
# 기본 모드는 settings.json의 permissions.defaultMode(auto) 그대로 유지된다.
# 이미 권한 관련 플래그가 붙은 호출(cc 등)에는 중복으로 넣지 않는다.
claude() {
  local a
  _adk_claude_bg_cmd "$@" && { command claude "$@"; return }
  for a in "$@"; do
    case $a in
      --dangerously-skip-permissions|--allow-dangerously-skip-permissions|--permission-mode)
        command claude "$@"; return ;;
    esac
  done
  command claude --allow-dangerously-skip-permissions "$@"
}
# Mosh는 로컬 터미널 scrollback을 보존하지 않으므로, 대화형 Mosh에서만
# Codex를 tmux 세션에 넣어 서버 측 출력 history와 마우스 스크롤을 사용한다.
_codex_tmux() {
  local sess=codex
  if ! tmux has-session -t $sess 2>/dev/null; then
    local -a launch=(/opt/homebrew/bin/codex --no-alt-screen "$@")
    local helper=${ADK_SESSION_ANCHOR_HELPER:-$HOME/.config/agentdesk/session-anchor.py}
    local anchor_python=${ADK_SESSION_ANCHOR_PYTHON:-/opt/homebrew/bin/python3}
    if [[ $ADK_CLI_ANCHORS == 1 && -f $helper && -x $anchor_python ]]; then
      launch=("$anchor_python" "$helper" codex --no-alt-screen "$@")
    fi
    tmux new-session -d -s $sess "${(j: :)${(@q)launch}}"
  fi
  if [[ -n $TMUX ]]; then
    tmux switch-client -t $sess
  else
    tmux attach -t $sess
  fi
}
codex() {
  if [[ -z $TMUX && -t 0 && -t 1 && ( -n $MOSH_CONNECTION || -n $MOSH_IP ) ]]; then
    _codex_tmux "$@"
  else
    if [[ $ADK_CLI_ANCHORS == 1 ]]; then
      _adk_anchor_cli codex --no-alt-screen "$@"
    else
      command /opt/homebrew/bin/codex --no-alt-screen "$@"
    fi
  fi
}
cdx()  { local ADK_CLI_ANCHORS=1; codex --dangerously-bypass-approvals-and-sandbox "$@" }
cdxt() { local ADK_CLI_ANCHORS=1; _codex_tmux --dangerously-bypass-approvals-and-sandbox "$@" }
# Claude Code in tmux (cct = Anthropic 직결).
# Auto-compact window: defaults to 500K tokens; pass a first arg to override it
# (500k / 350000). Keep this below the routed models' measured 872K/922K ceilings;
# $CC_COMPACT_WINDOW overrides the default when an explicit diagnostic run needs it.
# Attaches from outside tmux, switches client from inside (avoids the nesting error).
_cc_tmux() {
  local sess=$1; shift
  local win=${CC_COMPACT_WINDOW:-500000} explicit=
  case "${1-}" in
    <->)     win=$1;                          explicit=1; shift ;;
    <->[kK]) win=$(( ${1%[kK]} * 1000 ));     explicit=1; shift ;;
    <->[mM]) win=$(( ${1%[mM]} * 1000000 ));  explicit=1; shift ;;
  esac
  if tmux has-session -t $sess 2>/dev/null; then
    [[ -n $explicit ]] && print -u2 "note: '$sess' 세션이 이미 있어 재접속합니다 (window=$win 미적용). 새로 띄우려면 tmux kill-session -t $sess"
  else
    local -a launch=(claude --dangerously-skip-permissions "$@")
    local helper=${ADK_SESSION_ANCHOR_HELPER:-$HOME/.config/agentdesk/session-anchor.py}
    local anchor_python=${ADK_SESSION_ANCHOR_PYTHON:-/opt/homebrew/bin/python3}
    if [[ -f $helper && -x $anchor_python ]]; then
      launch=("$anchor_python" "$helper" claude --dangerously-skip-permissions "$@")
    fi
    tmux new-session -d -s $sess "env CLAUDE_CODE_AUTO_COMPACT_WINDOW=$win ${(j: :)${(@q)launch}}"
  fi
  if [[ -n $TMUX ]]; then
    tmux switch-client -t $sess
  else
    tmux attach -t $sess
  fi
}
cct()  { _cc_tmux claude "$@" }
