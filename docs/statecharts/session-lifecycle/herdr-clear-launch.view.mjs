export const title = "Herdr clear: one fresh session, token release, then normal restart resume";
const base = (s) => {
  if (s.phase === "ready") return "Prior provider session available";
  if (s.phase === "cleared") return s.row === "held" ? "Clear committed; old execution held" : "Clear committed; old execution ended";
  if (s.phase === "down1") return "Restart before the first launch";
  if (s.phase === "down2") return "Restart after verified provider startup";
  if (["load", "fence", "judge", "repin"].includes(s.phase)) return s.turn === 1 ? "Selecting the first post-clear session" : "Selecting the session after restart";
  if (["select", "launch"].includes(s.phase)) return s.turn === 1 ? "First provider command prepared" : "Next provider command prepared";
  if (["write-selector", "verify"].includes(s.phase)) return s.turn === 1 ? "Provider starts and rewrites selector" : "Next provider starts";
  if (s.phase === "release") return s.epoch === 2 ? "New clear token protects the next session" : "Fresh provider session verified";
  if (s.phase === "released") return s.fence !== "none" ? "Clear fence remains active" : (s.rolledOver ? "Provider session changed after release" : "Clear fence released by matching token");
  if (s.phase === "done") return s.resume ? "Current provider session resumed" : "Another fresh provider session launched";
  return s.phase;
};
export const state = ({s}) => {
  const broken = [];
  if (s.apiLeak) broken.push("selector exposed");
  if (s.unsafeRepins > 0) broken.push("journal changed under fence");
  if (s.wrongReleaseChanged) broken.push("wrong token released");
  if (s.lastLaunch.first && s.lastLaunch.epoch === s.epoch && s.epoch > 0 &&
      (s.lastLaunch.session === s.clearedSession || s.lastLaunch.resume)) broken.push("pre-clear session resumed");
  if (s.phase === "done" && s.epoch === 1 && s.firstSession === "fresh1" &&
      (s.lastLaunch.session !== s.expectedResume || !s.lastLaunch.resume)) broken.push("verified session lost on restart");
  return base(s) + (broken.length ? " — " + broken.join(", ") : "");
};
export const group = ({s}) => s.epoch === 2 ? "SecondClear" : (s.fence !== "none" ? "Fenced" : "Normal");
export const colors = { Fenced: "teal", Normal: "blue", SecondClear: "pink" };
const labels = {
 ClearRetired: "you clear and the old execution ends", ClearHeld: "you clear while teardown is held",
 OldExecutionEnds: "later recovery confirms the old execution ended",
 LoadSelector: "the router consumes its volatile clear flag", ReadFence: "the launch reads the persistent clear token",
 Judge: "the launch checks the native binding", RepinRetired: "the launch verifies and rewrites the retired resume binding",
 Select: "the launch selects fresh or resume", NotSent: "the first create is not sent; input retries",
 Launch: "the provider command starts", Launch2: "the next provider command starts",
 LaunchWritesSelector: "provider startup writes the session selector",
 FreshVerified: "the fresh SessionStart is verified", ResumeVerified: "the resumed SessionStart is verified",
 Release: "the captured token releases the clear fence", ClearAgain: "you clear again before release",
 WrongTokenRelease: "the old launch tries to release a different token",
 Rollover: "the provider moves to a new session", DcRestart: "the server restarts before launch",
 Boot: "the server boots without the volatile clear flag", SecondRestart: "the server restarts after verification",
 Boot2: "the next turn reads durable state", ReadApi: "the session selector API is read", Done: "the launch completes",
};
export const step = (action) => labels[action] ?? action;
