export const title = "Busy Claude input uses the same owner on either host";
export const state = ({s}) => !s.up ? "Restarting with original input" :
  s.wrongTarget ? "Wrong pane received input" :
  s.effects > 1 ? "Input delivered twice" :
  s.phase === "lost" ? "Input silently lost" :
  ({ queued: "Original owned front", may: "May have sent, never replay",
     sent: "본문·Enter 기록, 영수증 대기", done: "Transcript receipt settled",
     held: s.notice ? "Held visibly, before any key" : "Held before any key",
     uncertain: s.notice ? "Uncertain delivery shown" : "Original delivery uncertain" })[s.phase];
export const step = (action) => ({ Drive: "try existing busy writer",
  Effect: "본문과 Enter 전송", NativeProof: "observe user or queued row",
  Settle: "settle original once", Notify: "show existing held status",
  ClearBlock: "composer becomes ready", RetryHeld: "wake original front",
  Crash: "process restarts after may-send boundary", Boot: "restore and wake owner",
  Replace: "pane incarnation changes", Replay: "unsafe replay of uncertain input",
  Drop: "unsafe loss of held original", Wait: "remain in current state" })[action] ?? action;
