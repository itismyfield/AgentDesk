// Production base 6b91594c5c (slice 4 repair 1); projection of the shared frame ledger file.
export const title = "Frame ledger file: V2 owner, out-of-scope writer and the 5b ops procedure";
export const state = (v) => {
  const st = v.st ?? {};
  const rec = v.rec ?? {};
  if (Object.keys(st).some((i) => ["pasted", "unknown", "done"].includes(st[i]) && rec[i] !== 1))
    return `RED: a pasted input without exactly one record (file ${v.fmt})`;
  if (st.x === "unknown" && v.xbind !== v.binding)
    return "RED: Unknown head's transcript binding replaced; its proof cannot be used";
  if (v.binding === "replaced" && (v.retryDebt === true || v.keylessObl === true))
    return "RED: sessions replaced while a retry debt or keyless obligation still needs them";
  if (v.bin === "new" && v.bootfmt === "v1") return "RED: 5b binary booted on a V1 ledger; Claude runtime stops before intake";
  const bin = v.bin && v.bin !== "run" ? `[${v.bin}${v.binding === "replaced" ? ", new sessions" : ""}] ` : "";
  const debt = [v.retryDebt === true ? "retry debt" : "", v.keylessObl === true ? "keyless debt" : ""].filter(Boolean).join(", ");
  return `${bin}${debt ? `(${debt}) ` : ""}file ${v.fmt} · owner input ${st.o} · out-of-scope input ${st.x}${v.held === true ? " (read outside the lock)" : ""}`;
};
export const step = (action) => action;
