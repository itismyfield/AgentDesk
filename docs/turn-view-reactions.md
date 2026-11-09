# Turn view reactions

A live queued input uses 📬. Once a start failure durably retains an input,
its source generations clear their matching 📬 through the turn view reconciler;
a different queued input or a newer generation keeps its marker.

Persisted queue state uses version 4 (`LIVE_QUEUED_MARKER_STATE_VERSION`).
Versions 1–3 identify legacy queue reactions that must be removed before the
current state is applied, even though both old and current standalone markers
use 📬.

Rolling back to the version 3 reader deletes an unrecognized version 4 state
file and leaves its Discord 📬 in place until a later existing cleanup path;
renaming the version 4 constant does not change that rollback behavior.
