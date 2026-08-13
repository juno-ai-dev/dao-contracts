# Gauge monitoring contract

This document defines the minimum production alerts for the gauge stack. It is
backend-neutral: an indexer, scheduled chain querier, or both may implement the
rules, but each rule must retain `chain_id`, block height/time, contract
address, code ID/checksum, and (where applicable) `gauge_id` as labels. Store
the raw query response or transaction error used to fire an alert.

Thresholds below are deployment inputs and must be recorded in the signed
chain-test report and canary proposal. They must not exceed one half of the
relevant epoch/reset/keeper interval unless explicitly approved.

| Alert | Severity | Signal and firing condition | Clear condition |
|---|---|---|---|
| `GaugeEpochOverdue` | warning, critical after three retry windows | `Gauge.next_epoch < chain_time - keeper_retry_window` while `is_stopped=false`; critical when no successful `execute_tally` follows three configured retries | A successful `execute_tally`, a future `next_epoch`, or an explicitly recorded stop |
| `GaugeSnapshotDeadline` | warning before deadline, critical after | An open snapshot epoch remains unexecuted within one keeper retry window of `execution_deadline`, or remains open at/after the deadline | A terminal `execute_snapshot_epoch`, `expire_snapshot_epoch`, or `abort_snapshot_epoch` event and matching terminal query |
| `GaugeExecutionFailed` | warning, critical when deterministic/repeated | Failed `Execute { gauge }`; group by normalized error and fire critical after three identical errors or immediately for authorization, invalid message, or insufficient-funds errors | A successful execution and future `next_epoch`; do not auto-clear merely because retries stop |
| `GaugeBudgetUnderfunded` | critical | Before snapshot opening, Vault balance is below the full epoch budget; after opening, it is below adapter-reported `emitted_value`; also fire on terminal `insufficient_funds` | Before opening, verified balance covers the full fixed budget. After `insufficient_funds`, manual acknowledgement and a fresh epoch are required; a later top-up never clears the stale outcome |
| `GaugeAllocationMismatch` | critical | Snapshot query violates `allocated_power <= participating_power <= snapshot_total_power`, retained plus unallocated signals disagree with paginated tallies/ballots, emitted plus retained differs from the budget, or the retained option appears in the selected/message set | Two finalized, fully reconciled query samples and incident acknowledgement; stop the gauge while unresolved |
| `GaugeHookWiringDrift` | critical | `Config.hook_caller` or voting-power source differs from the signed deployment inventory, the orchestrator is absent from the actual source's hook query, or a canary power change produces no corresponding power-hook event/tally change | Inventory and both on-chain directions agree, followed by a successful low-value power-change probe |
| `GaugeVoteSubscriberRemoved` | warning | Any `remove_failed_vote_hook` event; label the removed `hook` and `reply_id` | Manual acknowledgement after subscriber repair and owner `AddHook`; never clear solely because the address disappears |
| `GaugeResetStalled` | warning, critical after three calls | A reset cursor remains unchanged across one successful continuation interval, a nonempty call reports `processed=0` and `complete=false`, or `GaugeHealth.reset_cursor` does not advance across successful reset calls | `complete=true`, or a later response advances the cursor; critical requires operator acknowledgement |
| `GaugeInvariantMismatch` | critical | `GaugeHealth.consistent=false`, `scan_complete=false`, or `mismatch_count>0` | Two consecutive complete, consistent queries at distinct finalized heights after investigation |
| `GaugeEscrowShortfall` | critical | Marketing `Liabilities.escrow_balance < Liabilities.asset.amount`; also fire when an active liability asset exists but its balance cannot be queried | Balance covers liabilities at two finalized heights and the underlying transfer history is reconciled |
| `GaugeRefundStalled` | warning | `refund_cursor` and liabilities do not change across a successful `return_deposits` interval while `refunds_complete=false` | Cursor/liabilities advance or `refunds_complete=true` with zero liability |
| `GaugeVersionMismatch` | critical | cw2 name/version, stored code checksum, or API-major differs from the signed deployment manifest; also fire after a migration event whose `to_version` does not match cw2 | All manifest, code, cw2, and schema identities agree at a finalized height |

## Polling and finality

Poll configuration, cw2, code information, balances, `GaugeHealth`, gauge
state, adapter liabilities, ownership, and hook lists at least once per keeper
retry window. Use finalized blocks according to the target chain's operational
policy. Event-driven alerts may fire immediately, but state-based clearing must
wait for finality. A query/RPC failure is telemetry failure, not a healthy
sample; alert after two consecutive polling failures.

## Canary dashboard and exit evidence

The canary dashboard must show policy version, snapshot height, close and
execution deadline, all raw/retained/unallocated/selected/emitted accounting,
execution outcome and message counts, all health fields, voting-power hook events, reset cursor
progress, adapter/DAO balances, liabilities, ownership, hook membership, cw2
version, code ID/checksum, and RPC freshness. Retain at least two complete
epochs including one power change and one multi-call reset. Canary exit
requires no unresolved critical alert, no suppressed alert without a signed
risk acceptance, and an exported dashboard/alert history attached to the
governance decision.

See [OPERATIONS.md](./OPERATIONS.md) for recovery actions and [EVENTS.md](./EVENTS.md)
for stable event fields.
