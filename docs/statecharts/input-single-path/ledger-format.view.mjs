// Production base 6b91594c5c (slice 4 repair 1); projection of the shared frame ledger file.
export const title = "Frame ledger file: V2 owner, out-of-scope writer and the 5b ops procedure";
export const state = (v) => {
  const st = v.st ?? {};
  const rec = v.rec ?? {};
  const arch = v.arch ?? {};
  const n = (i) => (rec[i] ?? 0) + (arch[i] ?? 0);
  if (Object.keys(st).some((i) => ["pasted", "unknown", "done"].includes(st[i]) && n(i) !== 1))
    return `RED: a pasted input without exactly one record (file ${v.fmt})`;
  if (st.x === "unknown" && rec.x !== 1) return `RED: Unknown head's proof archived (file ${v.fmt})`;
  if (v.bin === "new" && v.bootfmt === "v1") return "RED: 5b binary booted on a V1 ledger; Claude runtime stops before intake";
  const bin = v.bin && v.bin !== "run" ? `[${v.bin}] ` : "";
  return `${bin}file ${v.fmt} · owner input ${st.o} · out-of-scope input ${st.x}${v.held === true ? " (read outside the lock)" : ""}`;
};
export const step = (action) => action;
