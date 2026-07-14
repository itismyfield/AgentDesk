# Code Comment Language Policy

All new or modified code comments and doc-comments must be written in English.

## Existing Comments

Do not bulk-translate existing comments; doing so pollutes `git blame`. Translate
an existing comment only when you are already substantively editing that
specific line or block for another reason.

## Rationale

A substantial minority of non-test source files mix Korean and English comments
(~11% of non-test `.rs` files contain Korean in comments; ~25% contain Korean
anywhere including string literals), creating inconsistency and making
cross-model review harder.

## Scope

This is a human and agent authoring policy, not a CI-enforced gate. It does not
retroactively enforce changes to existing comments. Test files and user-facing
strings or messages are out of scope; this policy covers only code comments and
doc-comments.
