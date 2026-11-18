# Fixed campaign host worker

This extends [CampaignNodeLifecycle](CampaignNodeLifecycle.tla) at the M2 base `a900a7e142d73989704e09a90068e19e54ce4f6f`. The [host configuration](CampaignHostWorker.cfg) models one private journal, two worker contenders, one saved action, a resource decision and a bounded restart. The original lifecycle configurations still check approval and stage transitions.

| Configuration | Result | Contract |
|---|---|---|
| HostWorker | GREEN, 147 distinct states | One worker per node, a durable fence before initiation, at most one start, headroom before initiation, eventual progress under the assumptions below |
| HostWorkerNoLock | RED `SingleHostWorker` | Both contenders can hold the same node |
| HostWorkerNoFence | RED `HostWritesFenced` | An effect can start before the host applied its fence |
| HostWorkerNoHeadroom | RED `HostRespectsHeadroom` | A waiting effect can start |
| HostWorkerNoReplay | RED `HostStartsOnce` | A crash after initiation and before its receipt allows a second start |

`HostProgresses` assumes the fixed host eventually reopens its journal, headroom becomes available or explicitly unavailable, and started or indeterminate effects eventually yield an outcome or direct recovery evidence. The model does not assign a successful exit to an absent process. In Rust, ambiguous observations wait for direct evidence. A known result is delivered through the existing verification fact transaction. Scope is one configured ledger and one host-local journal root. Public execution remains `Unavailable`.
