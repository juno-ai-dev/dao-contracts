# dao-voting-juno-staked

A thin DAO DAO voting module that reads staked-JUNO voting power from Juno's
chain-owned `x/voting-snapshot` module.

## Compatibility

This contract is Juno-specific. It requires the Juno v30 wasm custom-query
bindings and is intended for the v30 `uni-7` network. Chains and older Juno
releases without `x/voting-snapshot` cannot answer its queries.

The chain owns voting-power policy and storage:

- DAO DAO treats height `h` as beginning-of-block voting power. Juno persists
  settled staking snapshots in EndBlock, so both voting-power queries translate
  DAO height `h` to Juno snapshot height `h - 1` and return the requested DAO
  height in the response. This keeps proposal totals and later voter queries on
  one immutable basis even when staking changes later in the proposal block.
- Omitting `height` uses the beginning of the current block (the previous
  settled Juno snapshot).
- Liquid-staking exclusion applies only to addresses in Juno's
  governance-managed LST allowlist. Operators must verify the live allowlist;
  v30 does not generically detect liquid-staking contracts.

These historical semantics let DAO DAO proposal modules use power fixed at a
proposal's snapshot height, rather than recomputing from current stake.

## Deploy and instantiate

Store the wasm on a compatible Juno v30 chain, then instantiate it from the DAO
core with the core contract as admin. The instantiate payload is empty:

```json
{}
```

Configure the resulting address as the DAO's voting module. Users continue to
delegate, undelegate, and redelegate through Juno `x/staking`; this contract has
no user execute operations. Its standard DAO DAO query surface is:

- `VotingPowerAtHeight`
- `TotalPowerAtHeight`
- `Dao` (the DAO core that instantiated this voting module)
- `Info`

The contract has not been claimed as deployed or chain-tested by this
repository documentation.

## Why staking-delta hooks are not exposed

Synchronous hook fanout cannot be implemented correctly with Juno v30's query
API. `x/cw-hooks` invokes contract sudo synchronously before
`x/voting-snapshot` persists dirty snapshots in EndBlock, so a current-height
query from that sudo sees the prior settled value. Delegation-removal events
also do not identify a delegator's remaining power across other validators,
and slash or validator bond-status changes can affect many delegators without
enumerating them.

Consequently this module deliberately exposes no cw-hooks registration, sudo
staking-event interface, subscriber management, or per-event DAO hooks.
Downstream consumers must query voting power at the historical height they need
and must not expect lossless per-delegator callbacks from this module.

## Limitations

- The module is usable only where Juno's v30 custom wasm query API is present.
- Current queries deliberately use the previous settled Juno snapshot so DAO
  height semantics remain beginning-of-block and stable throughout the block.
- Voting-snapshot retention must exceed the maximum proposal lifetime plus an
  operational margin. If governance enables shorter pruning, old snapshots can
  become unavailable and at-or-before queries can return zero.
- The `Dao` query returns the instantiating address recorded at setup; changing
  the wasm admin does not change which DAO the voting module belongs to.
