# Session name dual read: reviewed r3 boundary projection

Source: `refs/review/i8119-design-1` (`2c056204cf`), [Appendix B of the reviewed design](https://github.com/itismyfield/AgentDesk/blob/2c056204cf2e9b94731f556e68f2a6e39364dcd9/docs/design/8119-session-names.design.md). The TLA module is copied unchanged. Eight modes each have four independent invariant configs, preserving the reviewed 32-case matrix. The `fixed` mode is the design contract; it does not assert that every production migration phase already implements that contract.

| Mode | NoDuplicate | NoForeignAdoption | NoForeignTouch | NoLiveCut |
|---|---|---|---|---|
| fixed | GREEN | GREEN | GREEN | GREEN |
| generator_first | RED | GREEN | GREEN | GREEN |
| name_adopt | GREEN | RED | GREEN | GREEN |
| probe_absent | RED | GREEN | GREEN | GREEN |
| prefix_trust | GREEN | RED | GREEN | GREEN |
| mixed_builder | RED | GREEN | GREEN | GREEN |
| early_legacy_removal | GREEN | GREEN | GREEN | RED |
| name_reap | GREEN | GREEN | RED | GREEN |

Old live sessions are selected before generating a successor, failed observations defer admission, and user-owned names are not adopted or reaped. Restart forgets the in-memory adoption while retaining the stored primary. Each mutant removes the corresponding design guard and is expected to break exactly the indicated invariant. The module is a bounded safety projection of names, ownership markers, primary rows, observation failure and restart. It does not prove the complete runtime, database concurrency, or liveness after an observation failure. Existing `SessionNaming`, `TmuxSession` and `HerdrSession` configs cover the S2 registration/promotion projection separately.

Run the official checker after committing these configs:

```sh
python3 scripts/ci/statecharts_tlc.py --jar "$TLA2TOOLS" --base origin/main --head HEAD
```

The lane report records the executed command, result sets and raw logs; `expected.tsv` lists each config's expected violations independently.
