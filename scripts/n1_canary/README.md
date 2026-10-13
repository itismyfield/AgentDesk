# N1 oracle evidence

The CLI reads the pinned manifest and captured inputs. It never repairs stores or sends
Discord messages. Its shipped support table still declares unknown outcomes unproduced;
that condition always prevents PASS. The hypothetical supported profile lives only in
`tests/test_n1_canary_audit.py`, in a temporary CLI copy and a subprocess observation shim.
There is no production support-table option or environment override.

The native transcript, O init and ledger retain their independent native/store formats.
Unknown ledger variants, malformed rows and incomplete results yield `[U]`. Existing init
records may belong to an earlier build. The current process/build proof is a separate input.

The Legacy, debt and residue inputs use capture schema 1. Each capture has exactly
`schema_version`, `complete`, `scope` and the fields below. Scope binds the boot, channel,
provider, full SourceId, closing observation seq and closing source offset. `complete` must
be true. Captures whose inventory or observation boundaries cannot be established must
report incompleteness; a bare empty list is insufficient evidence of absence.

| Input | Capture fields |
|---|---|
| Legacy | `inputs`, `confirmed_deliveries`, `delivered_frontier`, `owner`, `allowed_messages` |
| Debt | `undelivered`, `abandoned`, `baseline_messages`, `ledger_sha256` |
| Inflight/pending/abort | `records` |

Input provenance records bind a physical native observation, its normalized record digest,
the execution identity, a positive input kind (`tty`, `monitor`, `autonomous`, `discord`)
and the Discord input ID when applicable. They are input-ledger evidence, not an origin
inferred from a missing PG row. Every physical observation must have matching provenance;
every in-window human Discord input and PG intake row must be explained. Native completion
and publication expectations always come from the transcript, never these records or O.
`owner` records the observed O writer and N1 mode under the same execution identity.

Legacy receipts and the frontier retain their delivery-record fields. A receipt must bind
one independently derived piece and an actual bot message with the same digest. Shared
O/Legacy receipt identity remains uncertain rather than proving an extra POST. The supported
allowed-message capture is a tool panel with an excluded native Tool key, empty text and
a matching embed footer identity. Other card/panel formats remain `[U]`.

Debt ranges record SourceId, start/end offsets and a reason. Known prefix units and historical
O outcomes are also audited even without an inventory alarm. Old missing/duplicate pieces
are reported separately and prevent PASS while unresolved. A new duplicate of an old unit
inside the observation window is a current violation. Unknown source genealogies cannot be
settled by this profile. Baseline message records bind an individually fetched message ID,
digest and last changed seq preceding confirmation; baseline membership does not excuse
later placeholder effects. The complete debt inventory pins the full O ledger digest.

Closure needs successful native/origin, delivery, effects, reverse and debt stages plus all
input/observation/collection guards. A definite independently proved violation takes priority
over incomplete unrelated evidence. Exact effect totals become null while any uncertainty
remains; confirmed effects are retained as lower bounds. The result file and stdout use the
same composed verdict. Before enabling a production profile, its capture/provenance producers
and compatibility must be verified alongside the separate observation producer work.
The unconditional shipped observation guard also needs a reviewed update when that
producer support is introduced; flipping the table alone does not enable production PASS.
