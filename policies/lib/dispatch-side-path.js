/** @module policies/lib/dispatch-side-path
 *
 * #3605 (T2): canonical "inert side-path" dispatch-type set, shared across the
 * JS policy layer (kanban-rules, timeouts/*). A side-path dispatch records
 * information about a card without ever advancing, completing, or failing it: it
 * stays pinned in `requested` and its completion must not flow into the
 * implementation / review / PM-gate lifecycle.
 *
 * `consultation` (#256) was the first such type; `scope-assessment` (#3605) is
 * the second. Centralizing the set here means every `=== "consultation"`
 * side-path guard is replaced by `isSidePathDispatch(type)` so the two are
 * mirrored consistently and future side-path types are added in exactly one
 * place. This is the JS counterpart of Rust
 * `dispatch::SIDE_PATH_DISPATCH_TYPES` / `dispatch_is_side_path`.
 *
 * NB: this set is intentionally about lifecycle inertness only. It is NOT the
 * "counter-model channel" set (review|e2e-test|consultation) — scope-assessment
 * routes to the assigned agent's primary channel, so channel routing is a
 * separate concern and must not read this predicate.
 */

var SIDE_PATH_DISPATCH_TYPES = ["consultation", "scope-assessment"];

function isSidePathDispatch(dispatchType) {
  return SIDE_PATH_DISPATCH_TYPES.indexOf(dispatchType) !== -1;
}

module.exports = {
  SIDE_PATH_DISPATCH_TYPES: SIDE_PATH_DISPATCH_TYPES,
  isSidePathDispatch: isSidePathDispatch
};
