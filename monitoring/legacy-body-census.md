# Legacy body census routine

`legacy-body-census.js` is a deployment template. Deploy it as
`monitoring/legacy-body-census.js` under an existing configured routine script
root, then use the existing routine registration workflow. The deployment
owner chooses the Discord destination and enables the daily `0 9 * * *`
schedule in `Asia/Seoul`. Adding this template does not register or enable it.

Only that exact script reference receives the Rust health adapter's report.
The adapter resolves the fixed `mac-mini-release` and `mac-book-release`
worker identities, checks their configured trusted origins against their
registry advertisements, and uses the live in-memory control API credential.
It requests only `GET /api/health/detail`, concurrently, with a 10-second
per-host deadline and a 30-second collection deadline. It does not retry.
Missing registry entries, trust failures, HTTP/authentication errors, incomplete
JSON, and missing observations remain explicit unknown evidence.

The native evaluator owns every verdict. Its interval starts at the recorded
coverage epoch and ends at the paired collection cut; there is no fixed-length
observation window. PASS requires an unbroken boot chain, at least one restart
per applicable host (at least two in total), at least ten TUI turns per
applicable host, O ACK evidence, and no Legacy TUI body violation. Verified
absence of TUI bindings throughout a host's interval is `NO_TUI_BINDING` (N/A).
Unknown selection or incomplete collection cannot establish N/A.

Known violations take precedence as FAIL, including when other evidence is
incomplete. Otherwise incomplete evidence is INCONCLUSIVE, insufficient traffic
is NO_TRAFFIC, and complete eligible evidence is PASS. PASS plus N/A is PASS;
two N/A hosts yield NO_TUI_BINDING. Any N/A host prevents DELETE_READY. A site
also needs positive opportunities and no body events; unresolved source
provenance remains DELETE_NOT_READY.

The policy copies the native report into the successful Complete action's
`result`, including FAIL, INCONCLUSIVE, and NO_TRAFFIC. These verdicts do not
pause the routine or discard its next schedule. The Discord logger sends one
final line; the detailed host/site evidence stays in `routine_runs.result_json`.
Delivery failure is recorded separately in the existing run Discord-log status.

Routine static instrumentation evidence and full execution coverage are separate.
The gate validates the per-boot static manifest/build/census evidence. CI binary
evidence is not a routine-verdict prerequisite. Full coverage still requires
verified execution evidence linked to every manifest row. Pending-zero or a
VERIFIED label alone cannot establish it. Unlinked rows keep coverage false;
this routine does not authorize deleting a producer or change output ownership.
