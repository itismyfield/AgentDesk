# Claude Code shortcut
# Managed source: AgentDesk/scripts/session-anchor-mac-mini.zsh.
# Preserve this host's 1M compact window and bare Codex alias.
unalias cc cdx cdxt 2>/dev/null
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
alias codex='command codex --no-alt-screen'
cdx() { _adk_anchor_cli codex --no-alt-screen --dangerously-bypass-approvals-and-sandbox "$@" }
# Claude Code in tmux (cct = Anthropic 직결).
# Auto-compact window: defaults to the full 1M context; pass a first arg to shrink it
# (500k / 350000 / 1m). $CC_COMPACT_WINDOW overrides the default.
# Attaches from outside tmux, switches client from inside (avoids the nesting error).
_cc_tmux() {
  local sess=$1; shift
  local win=${CC_COMPACT_WINDOW:-1000000} explicit=
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
cct() { _cc_tmux claude "$@" }
_codex_tmux() {
  local sess=codex
  if ! tmux has-session -t $sess 2>/dev/null; then
    local -a launch=(codex --no-alt-screen --dangerously-bypass-approvals-and-sandbox "$@")
    local helper=${ADK_SESSION_ANCHOR_HELPER:-$HOME/.config/agentdesk/session-anchor.py}
    local anchor_python=${ADK_SESSION_ANCHOR_PYTHON:-/opt/homebrew/bin/python3}
    if [[ -f $helper && -x $anchor_python ]]; then
      launch=("$anchor_python" "$helper" codex --no-alt-screen --dangerously-bypass-approvals-and-sandbox "$@")
    fi
    tmux new-session -d -s $sess "${(j: :)${(@q)launch}}"
  fi
  if [[ -n $TMUX ]]; then
    tmux switch-client -t $sess
  else
    tmux attach -t $sess
  fi
}
cdxt() { _codex_tmux "$@" }
