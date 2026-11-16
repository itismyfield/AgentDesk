export const title = "Peer deploy restart during a gateway yield wait";
const peers = {
  yield: "peer booting, yielding the gateway",
  up: "peer running with its marker poller",
  exiting: "peer exiting after a restart",
};
const deploys = {
  idle: "no deploy request",
  drain: "deploy waiting for its proof",
  acked: "deploy proof acknowledged",
  failed: "deploy refused",
  done: "peer on the new binary",
};
const markers = { none: "", deploy: ", deploy request pending", self: ", self restart pending" };
export const state = (v) => `${peers[v.peer]}; ${deploys[v.dphase]}${markers[v.marker]}`;
export const group = (v) =>
  v.dphase === "failed" ? "Refused" : v.dphase === "done" || v.dphase === "acked" ? "Confirmed" : "Pending";
export const colors = { Refused: "pink", Confirmed: "teal", Pending: "yellow" };
const steps = {
  DeployRequest: "the cluster deploy writes restart_pending",
  SelfRequest: "the peer writes its own restart (promotion or handback)",
  YieldEnds: "the 90s yield wait ends",
  YieldStandsDown: "the yield wait sees a pending request and stands by",
  PollerConsumes: "the marker poller persists the request and exits",
  Relaunch: "launchd restarts the peer",
  Ack: "the deploy sees its own proof",
  Timeout: "the deploy's 20s wait runs out and clears its marker",
  Swap: "the deploy restarts the peer on the staged binary",
  Tick: "about 10 seconds pass",
  Settled: "nothing left to do",
};
export const step = (action) => steps[action] ?? action;
