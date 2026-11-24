export const title = "One channel's transition lock in two bot runtimes";
const holds = (v, bot) => {
  const lease = v.leases.find((l) => l.maker === bot);
  if (lease) return lease.hold === "borrowed" ? "lease on a borrowed guard" : "lease held";
  if (v.unplaced.some((u) => u.by === bot)) return "unplaced lease held";
  if (v.raws.some((g) => g.by === bot)) return "guard held";
  return v.pins.some((p) => p.by === bot) ? "free, handle kept" : "free";
};
export const state = (v) => ({ "Bot A": holds(v, "botA"), "Bot B": holds(v, "botB") });
const steps = {
  AcquireRaw: "a caller takes the lock",
  AcquireLease: "a caller takes a transition lease",
  AcquireUnplaced: "a caller takes an unplaced lease",
  Probe: "a lookup finds no live lock and drops its handle",
  Pin: "a caller keeps the lock handle",
  Borrow: "a held guard is lent to a lease",
  LockPin: "the kept handle takes the lock",
  Unpin: "the kept handle is dropped",
  DropRaw: "the guard is released",
  DropLease: "the lease is dropped",
  DropUnplaced: "the unplaced lease is dropped",
  Exhausted: "the model's lock budget is spent",
};
// TLC names a step by its inner action or by the wrapper that quantifies it.
const wrappers = {
  HostTakesGuard: "AcquireRaw", HostTakesLease: "AcquireLease", HostTakesUnplaced: "AcquireUnplaced",
  HostProbes: "Probe", HostKeepsHandle: "Pin", HostBorrows: "Borrow", HandleLocks: "LockPin",
  HandleDrops: "Unpin", GuardDrops: "DropRaw", LeaseDrops: "DropLease", UnplacedDrops: "DropUnplaced",
};
export const step = (action) => steps[wrappers[action] ?? action] ?? action;
