# Live Herdr row-end notice

Code baseline `b04ea56650e5397989d33f1be199be21a83c8d0e`. This projection of [HerdrRetiredRow](HerdrRetiredRow.tla) follows notice delivery through repeated live end passes. The [state chart](herdr-retired-row-notice.chart.mmd) shows the target flow.

| Configuration | TLC result |
| --- | --- |
| Current | RED. `Retire → LiveTick → Publish → SettleFailed → LiveTick → Publish` violates `NoticeExactlyOnce`. |
| Target | GREEN. `NoticeExactlyOnce` bounds successful notices to one per execution nonce and save generation and requires one on entry to settlement. `LiveNoticeProgress` checks progress with fair ticks and eventual delivery. |
| NoRecord | RED. Publishing without recording recreates the Current trace. |

The channel transition serializes publication and recording. Delivery failure keeps the row eligible for another attempt. After an acknowledged write, a failed settlement keeps the actor and its receipt so later ticks go straight to settlement. Both a fresh POST and a `tui_o` placeholder PATCH are instances of `Publish`.

Restart drops the live actor and its receipts. The previous process's row fails the live-generation check, so the live path publishes zero times for that key after restart. A new save generation or execution nonce is a separate model key. The boot owner's routing, recovery notices and row retirement are handled separately. The model counts acknowledged deliveries and assumes the transport reports their outcome. It does not prove uniqueness when Discord has committed a write and the response was lost before acknowledgment.
