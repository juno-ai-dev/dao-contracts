# dao-voting-juno-staked

A thin DAO DAO voting module that reads staked-JUNO voting power from Juno's
chain-owned `x/voting-snapshot` module.

## Compatibility

This contract is Juno-specific. It requires the Juno v30 wasm custom-query
bindings and is intended for the v30 `uni-7` network. Chains and older Juno
releases without `x/voting-snapshot` cannot answer its queries.

The chain owns voting-power policy and storage:

- `VotingPowerAtHeight` reads the latest delegator snapshot recorded at or
  before the requested height.
- `TotalPowerAtHeight` reads total eligible bonded power at or before the
  requested height.
- Omitting `height` asks at the current block height. Because snapshots are
  persisted in EndBlock, a query made earlier in that same block can still
  observe the previous settled snapshot.
- Liquid-staked-token exclusion is enforced by Juno's module configuration,
  not by this contract. Operators must verify the chain's LST configuration.

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
- Current-block reads are subject to EndBlock settlement; historical reads of a
  settled height are the stable interface for proposal voting.
- The `Dao` query returns the instantiating address recorded at setup; changing
  the wasm admin does not change which DAO the voting module belongs to.
