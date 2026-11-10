// Counterexample view for the configs that are expected to break: every step has a label.
import { step as chartStep } from "./verification.view.mjs";
export { title, state, group, colors } from "./verification.view.mjs";

const bookkeeping = {
  Materialize: "the server binds the sealed output to the check",
  BindInput: "the server binds a phase input",
  BindPlan: "the server binds the test plan",
  Claim: "the server claims a check phase",
  Launch: "the server hands the approved start to the gate",
  ApplyNodeFence: "the gate applies the task's stop mark",
  ProcExit: "the check process exits",
  GateFinish: "the gate keeps the phase result",
  Deliver: "the gate marks the result as taken by the server",
  CrashMidInitiate: "the server dies while the gate is starting a phase",
  ObserveNonce: "the gate finds the process of the unconfirmed start",
  SettleIndet: "the gate finds no process for the unconfirmed start",
  AckFence: "the gate acknowledges the stop",
  ConfirmFence: "the stop mark is confirmed",
  Crash: "the server dies",
  Restart: "the server starts again",
  Idle: "nothing happens",
};
export const step = (action) => chartStep(action) ?? bookkeeping[action] ?? action;
