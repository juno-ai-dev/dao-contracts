# Gauge operations and recovery runbook

The required production alert signals, thresholds, labels, and clearing rules
are specified in [`MONITORING.md`](./MONITORING.md). An alert is not resolved
until its stated clear condition is met and recorded.

## Deployment preflight

1. Verify the Wasm checksum, cw2 identity, schema, optimizer image digest, and
   audit version against the signed release manifest.
2. Query DAO core, voting-power source, hook caller, adapter config/ownership,
   budget denom, and marketing liabilities. Validate every address on the
   target chain and confirm DAO/adapter balances cover the intended epoch and
   all bonds.
3. Register the orchestrator on the actual staking or membership hook source.
   Perform a small power change and confirm the gauge tally changes before
   funding a production budget.
4. Attach only governance-reviewed adapter code and configuration. Treat it as
   treasury execution authority.

## Epoch keepers

Call `Execute { gauge }` only after `next_epoch`. Duplicate callers are safe:
the first committed transaction advances the epoch and later transactions see
`EpochNotReached`. An empty selected set is a successful no-op. If execution
fails, check DAO balance, adapter validity/ownership, returned message count,
and destination/denom validity; no epoch state commits on failure.

Alert when `next_epoch` remains overdue beyond the keeper retry window. Do not
blindly retry an adapter error that deterministically returns an invalid or
underfunded message.

For epoch-snapshot gauges, first call permissionless `OpenEpoch { gauge }` only
after confirming the Program Vault holds at least the complete configured
budget. The contract enforces the same gate. Record the returned snapshot
height, policy version, close, deadline, budget, denomination, and retained
option. A rejected open must leave the current epoch, counter, and schedule
unchanged.

From `closes_at` until (but not including) `execution_deadline`, call
`Execute { gauge }`. Reconcile `allocated_power <= participating_power`, the
retained-option and unallocated signals, selected project power, and
`emitted_value + retained_value == epoch_budget`. The Vault needs only emitted
value at this point. `insufficient_funds` is terminal; do not top up and retry
the stale ballot. At or after the deadline anyone calls
`ExpireEpoch { gauge }`. For a deterministic adapter or migration failure the
owner may call `AbortEpoch { gauge, reason }`; retain the reason and incident
evidence. The guardian cannot abort or resume. After any terminal outcome,
verify that a second execute/expire/abort fails and the next epoch can open
exactly once.

## Health monitoring and reconciliation

Query `GaugeHealth { gauge }` after deployment, migration, reset completion,
and periodically for every active gauge. Alert on `consistent=false`,
`scan_complete=false`, a nonzero `mismatch_count`, or a reset cursor that does
not advance while reset continuation calls succeed. Store the full response,
including `first_mismatch`, with block height and chain ID.

A healthy gauge has a tally sum equal to `TOTAL_CAST`, one sorted-index entry
for every active option, and no index entry for a tombstoned option. A
tombstoned option may still have a tally included in `TOTAL_CAST` while stored
votes reference it; that is expected and does not make the health query
inconsistent. A zero tally also remains tombstoned until reset so a later
restake cannot reactivate it. Reconcile anomalies with paginated option and
vote queries and transaction history. Stop the gauge and halt keepers if
selection or totals could be wrong; never repair raw storage manually.

## Reset continuation

Call `ResetGauge { gauge, batch_size }` with `batch_size` from 1 through 100.
Continue while the response has `complete=false`. Record `processed` and
`next_reset`; a nonempty call must advance its persistent cursor. Voting and
execution are blocked while reset is active. If progress stops, verify the
same gauge ID and a valid batch size, then inspect `Gauge.reset` and option
pagination before proposing a migration.

Reset fully removes tombstoned options in bounded batches, regardless of
remaining expired vote records. Never delete raw tally/index storage manually.
For a gauge created without periodic reset, tombstones are intentionally not
garbage-collected: preserving them is required to keep a zero-power voter from
reactivating a removed option on restake. Before repeated removals exhaust the
100-option storage bound, stop the gauge and use a reviewed migration to add a
reset schedule or compact vote references. Do not work around the bound by
reusing a removed option string or editing storage.

## Marketing bond wind-down

Before starting, query `Liabilities {}` and verify `escrow_balance` covers the
reported asset. The owner calls `ReturnDeposits {}` repeatedly; each call
processes at most 50 rows. Continue until `complete=true`,
`refunds_complete=true`, and liabilities are zero. Wind-down is one-way: new
submissions remain rejected after completion, and later refund calls are
complete no-ops.

For a single submission, use soft rejection to refund its active bond or hard
rejection to forfeit it to the configured community pool. Do not send a second
deposit for metadata changes. On escrow shortfall, stop and reconcile on-chain
transfers and submission bond states before adding exactly the missing asset;
unrelated funds are not proof of a liability.

## Vote-hook recovery

Notification hook failures remove only the failing subscriber and do not
revert votes. Monitor `remove_failed_vote_hook`, repair the subscriber, then
have the orchestrator owner call `AddHook` again. This is distinct from the
authenticated staking hook: if staking power does not update tallies, verify
the voting module's hook registration and the orchestrator `hook_caller`
configuration immediately.

## Stop and resume

The owner calls `StopGauge` to freeze direct voting, reset, and execution.
Authenticated power hooks continue updating existing votes. Confirm the gauge
reports `is_stopped=true`, investigate, and avoid changing adapter authority
unless governance explicitly approves it. The owner calls `ResumeGauge` after
the fault is resolved; votes and the existing epoch schedule are preserved.

## Migration and rollback

Capture pre-migration cw2 info, configs, ownership, gauge/vote/tally samples,
adapter submissions, liabilities, balances, and cursors. Use only a manifest
approved source version and payload. After migration, compare all preserved
fields and exercise a low-value query/execute path.

CosmWasm code migration has no automatic binary rollback. Prepare a governance
proposal for a separately audited recovery code ID and state transform before
deployment. Never migrate back to an older semantic version or bypass cw2 by
editing storage. If a migration fails, retain its full chain error and verify
that cw2 and state stayed unchanged before retrying.

## Incident handling

For suspected loss of funds, tally corruption, unauthorized adapter output, or
hook-induced staking outage: stop affected gauges where possible, halt keepers,
preserve transaction/query evidence, notify the DAO security contacts, and use
governance-approved containment. Reconcile `TALLY`, `TOTAL_CAST`, sorted option
indices, active votes, bond liabilities, and balances. Publish a postmortem
covering root cause, impact, recovery, invariant checks, and preventive tests.
