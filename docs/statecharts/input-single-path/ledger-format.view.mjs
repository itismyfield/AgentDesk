// Production base ab2d0dcb81 (+ slice 4 repair 1); projection of the shared frame ledger file.
export const title = "Frame ledger file: V2 owner and out-of-scope writer";
export const state = (v) => {
  const st = v.st ?? {};
  const rec = v.rec ?? {};
  const bad = Object.keys(st).some((i) => ["pasted", "unknown", "done"].includes(st[i]) && rec[i] !== 1);
  if (bad) return `RED: a pasted input without exactly one record (file ${v.fmt})`;
  const x = st.x, o = st.o;
  return `file ${v.fmt} · owner input ${o} · out-of-scope input ${x}${v.held === true ? " (read outside the lock)" : ""}`;
};
export const step = (action) => action;
