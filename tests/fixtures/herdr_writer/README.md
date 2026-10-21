# Pinned herdr config writer

These fixtures come from herdr commit `7b116c05bfda646af39d2524c54e70c751f57ee8`.
`provenance.json` records the primary source URLs, original source SHA256 values,
excerpt line ranges, and fixture SHA256 values. The Rust excerpts retain the
upstream function bodies and the three upstream writer unit tests verbatim.

`upstream_unit_outputs.json` records the complete output for those unit-test
inputs. AgentDesk tests execute the pinned functions to validate those outputs,
then compare its writer replica against the same outputs. The upstream unit
tests are retained verbatim so their inputs and assertions can be audited.

The boundary differential test additionally exercises a line beginning with `[`
inside a multiline string. The pinned writer only treats that line as a section
boundary when it also ends with `]`. Tests model `update_file_at` by removing a
leading UTF-8 BOM before invoking the pinned editing functions.
