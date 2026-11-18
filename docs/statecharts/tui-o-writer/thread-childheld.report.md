# Verified late thread ownership and old-run delivery

Base: `ffa099d4f4a6a68c26171fb464f7b5edffca2e5f`; review repair compares
`96ed06d36e4545a705345db874873c2c18ad5b43` with the repaired behavior.

A source-bound late refusal under a verified parent grants Legacy ownership of
that execution. Turn end, session end and a new execution bind reopen adoption,
but do not settle the old execution's delivery obligation. Ordinary late pin and
fresh takeover both require A's own delivery proof, even when successor B is
closed and delivered or has an output-free prefix. A later A receipt permits
progress; B's frontier cannot stand in for it. Publication rechecks the old source
and send counter under the adoption lock, using the existing Owed/Sample rules.

The model allows the execution to end before the body reaches its Legacy claim.
`legacy` counts one native offer of A, `aAck` is A-scoped delivery evidence, and
`debt` survives the boundary. Bind and fresh cases exercise the shared publication
contract. The Rust tests distinguish the two real activation paths and append A
output during the fresh intake fence. The Pending case observes frozen pane end
without a current source, reopens Deferred, and retains the Pending activation
fence. The owner case overlaps a Released snapshot with its Deferred transition;
state and proof must be read under the same state-to-child lock order.

| Config pair | Before violation | Repair |
| --- | --- | --- |
| BeforeBody / ProposedBody | CurrentRunAnswered | GREEN |
| BeforeBoot / ProposedBoot | NoBootChurn | GREEN |
| DebtBefore / DebtAfter | NoLostA | GREEN |
| FreshBefore / FreshAfter | NoLostA | GREEN |
| PendingBefore / PendingAfter | ReleasedResolves | GREEN |
| OwnerBefore / OwnerAfter | AtomicOwner | GREEN |

Seven mutants separately remove normalization, boot exclusion, boundary retry,
debt retention, the A-specific proof requirement, atomic owner reads and Pending
termination observation. Each breaks its declared invariant or liveness property.
Fairness assumes the single native offer, its A-scoped receipt, run end and Pending
resolution eventually occur. The repair does not manufacture missing evidence.

This is an ownership model, not a Legacy transport deduplication model. Repeated
native offers and the unconfirmed double-post incident are outside its guarantee.
Backlog cutoff, new receipt propagation/discard and unresolved/conflicting-parent
notifications remain deferred. The repair only reads existing historical receipt
identities; it does not rewrite offsets, frontiers or stored replies. Process
restart can remove in-memory state but does not prove durable transcript backlog
was cleared. Missing exact historical proof conservatively prevents O activation.
