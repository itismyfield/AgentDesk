# claude-e decision log (historical)

The rollout record is retired under #5706 option c. Earlier experiments do not
provide current deployment or runtime-selection guidance. See the
[removal status and remaining order](README.md).

This file temporarily preserves source references to the former runtime-selection
rationale: explicit channel runtime overrides provider runtime, which overrides
legacy `tui_hosting`. Keeping this link does not mean runtime refusal or durable
row preservation has already been implemented; those are the next removal units.

Shared cleanup, watchdog, transcript parsing and ProcessBackend/TUI behavior remain.
The [Claude TUI follow-up retry boundary](../tui-o-writer-channels.md#claude-tui-follow-up-retry-boundary)
is documented with the current TUI behavior. Remove this tombstone after the final
source references are cleaned up.
