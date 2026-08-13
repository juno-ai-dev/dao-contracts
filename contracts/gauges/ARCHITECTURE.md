# Gauge architecture and threat model

This document covers `gauge-orchestrator`, the marketing `gauge-adapter`, the
`gauge-budget-allocator`, and their shared `gauge-interface` protocol. It is an
engineering threat model, not an independent security audit.

## Components and authority

The DAO core instantiates the orchestrator as a proposal module. A configured
voting-power source answers power queries, and its staking or membership
contract is the only accepted hook caller. Each gauge points to an adapter.
At an epoch, the orchestrator asks that adapter for arbitrary `CosmosMsg`
values and forwards them to DAO core through `ProposalExecuteHook`.

An adapter therefore has DAO execution authority for every epoch of every
gauge that references it. Adapter code IDs, migrations, ownership, and
configuration must receive the same review as a treasury-spending proposal.
The orchestrator does not constrain message types, recipients, denoms, or
amounts.

Epoch-snapshot mode intentionally differs from hook mode. It disables power
hooks, fixes one historical power height per explicitly opened epoch, and
requires the configured DAO core (the Program Vault) to hold the full fixed
budget before opening. The snapshot policy is versioned and copied into the
epoch, including its optional retained option and bounded execution window.

## Accounting invariants

For each gauge and option, `TALLY` equals the sum of current, unexpired voter
contributions. `TOTAL_CAST` equals the sum of all option tallies. Every tally
has exactly one `(gauge, points, option)` entry in `OPTION_BY_POINTS`, and no
stale sorted entry exists. Vote replacement and power-change hooks update all
three structures with checked arithmetic.

Votes may allocate less than 100% of a user's power; unused weight stays
unallocated. Duplicate, empty, zero-weight, over-100%, and round-to-zero vote
entries are rejected before mutation. Selection caps burn excess rather than
renormalizing it. Empty or all-zero selected sets are successful no-ops.

Option removal is a tombstone operation when active power remains. A tombstoned
option is immediately excluded from selection. Its tally continues to follow
stored vote power, including zero-power voters that later stake again, but it
never regains a sorted-index entry. The tombstone is retained conservatively
until bounded reset cleanup; a zero tally alone does not prove that no stored
vote still references the option.
Candidate selection also pull-checks adapter validity, so adapter-side removal
cannot remain payable solely because orchestrator state is stale.

In epoch-snapshot mode, `participating_power` is the full snapshot power of
each voter with a nonempty ballot. `total_cast`/`allocated_power` is only the
sum of the ballot's integer option allocations. The contract maintains:

```text
allocated_power <= participating_power <= snapshot_total_power
```

Turnout, selection thresholds, per-project caps, and adapter shares all use
`participating_power`; partial ballots are never normalized by their allocated
subset. A configured retained option is tallied and reported but removed
before validity checks, project top-N selection, caps, and adapter execution.
Its power, unallocated ballot power, invalid/threshold-excluded allocations,
cap overflow, and dust remain unspent.

## Hook liveness and failure domains

Power hooks process a voter's complete active gauge set. Creating a 101st
active gauge vote is rejected; hook processing never silently truncates at
100. Stopped gauges continue accepting authenticated power hooks so tallies do
not drift and unrelated staking is not blocked. Direct voting, reset, and
epoch execution remain frozen until the owner calls `ResumeGauge`.

Vote-notification subscribers are optional and capped at 10. Each callback has
a stable namespaced reply ID. A failing subscriber is removed by address; its
failure does not revert the vote or remove a healthy subscriber.

The staking transaction still depends on the orchestrator hook succeeding.
Arithmetic corruption, an unsupported legacy voter with more than 100 active
gauge records, or an incorrectly wired caller can reject staking. Operators
must monitor hook wiring and test migrations against populated state.

## Bounded work

The orchestrator supports at most 100 gauges, 100 options per gauge, 100 vote
entries per voter/gauge, 100 active gauge votes per voter, 100 adapter messages
per epoch, 10 notification hooks, and reset batches of 1–100 options. Titles
and option strings are capped at 128 bytes. Public list queries default to 30
and cap at 100 rows. Migration schedule updates contain at most 100 unique
gauge IDs and validate completely before writes.

The marketing adapter supports 1,000 submissions, 100 selected recipients,
128-byte names, 512-byte URLs, 100-row query pages, and fixed refund batches of
50. The allocator supports 100 validated address options and 100 selected
recipients. These are logical bounds; target-chain worst-case gas measurements
remain a release gate.

## Bond escrow

Each marketing submission stores the exact asset, amount, depositor, and bond
state. The synthetic community-pool row has no bond. Metadata updates preserve
the original bond and reject another deposit. Refund, soft rejection, and hard
rejection are exclusive transitions; liabilities are reduced before outbound
messages are emitted. `TOTAL_LIABILITIES` and `Liabilities {}` support
reconciliation, and every liability-changing path checks contract escrow.

Bulk refund is cursor-based. New submissions are paused while it is active,
and repeated calls make bounded progress. Unexpected transfers are not
liabilities and must not be used to justify paying a record twice.

## Economic and chain risks

Hook mode has no turnout quorum and retains its historical cast-power
denominator. Epoch-snapshot mode has an explicit turnout policy and preserves
unallocated participant power in its denominator. Integer multiplication
floors dust. DAO budgets, adapter caps, and governance policy must still
account for low-turnout capture and rounding.

Addresses are validated by the active chain API before they become bank-send
destinations. Native denoms and cw20 addresses are chain-specific. A schema or
payload valid on one chain is not proof that its address prefix, token, custom
messages, or required CosmWasm capabilities are valid on another.

## Liveness assumptions

Anyone may execute a due epoch or continue reset; an owner must resume a
stopped gauge and continue marketing wind-down. Concurrent keepers are safe
because each transaction observes committed state, but failed DAO messages
leave the epoch transaction uncommitted. Keepers need alerts for overdue
epochs, reset/refund cursors without progress, escrow shortfall, and repeated
adapter execution failure.

An epoch-snapshot execution is allowed only from voting close until its fixed
deadline. The adapter reports emitted and retained value; the orchestrator
requires their sum to equal the epoch budget and compares the current Vault
balance only with emitted value. A shortfall terminalizes the ballot as
`INSUFFICIENT_FUNDS`, preventing a later top-up from activating it. Anyone may
terminalize at the deadline as `EXPIRED`, and only the owner may use a
reasoned `ABORTED` recovery. The snapshot guardian remains stop-only. Every
terminal outcome advances scheduling once and can never be executed again.

## Migration boundary

Orchestrator and marketing migrations require their exact historical cw2
identity and source version `2.4.2` or `2.5.0`; all other versions and
identities are rejected. The budget allocator has no older artifact and exports
no migration entry point in the first release. Marketing migration reconstructs
legacy owner, bond, liability, and sender-index state and refuses underfunded
escrow before writing. Orchestrator migration validates all requested schedule
changes before updating cw2 or any gauge. Release proposals must query cw2
identity, config, ownership, populated state, and liabilities both before and
after migration.
