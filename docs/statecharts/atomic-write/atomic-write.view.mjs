export const title = "Atomic write: prepare, observe, publish";
const phases = {
  CreateDir: "Create the parent directory",
  CreateTemp: "Create a private temp file",
  Write: "Write the complete bytes to temp",
  FileSync: "Sync the temp file",
  Observe: "Record the upper inflight diagnostic",
  Rename: "Publish by rename",
};
export const state = (v) => v.pc === "Done"
  ? (v.published ? "Published; parent sync not requested" : `Not published: ${v.returnedStage}, ${v.ioError}`)
  : phases[v.pc];
export const group = (v) => v.published ? "Published" : v.returned === "Err" ? "NotPublished" : "Preparing";
export const colors = { Preparing: "blue", Published: "teal", NotPublished: "pink" };
export const step = (action) => action === "Done" ? null : action === "Fail" ? "Return the I/O error and record ENOSPC if needed" : phases[action] ?? action;
