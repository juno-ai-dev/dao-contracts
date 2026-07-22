# Changelog

All notable changes to the `juno-ai-dev/dao-contracts` fork are documented in
this file.

The retained history begins with the `cw721-roles` security fixups merged into
`v3` on 2026-07-14. Earlier upstream history remains available in Git.

This changelog follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
The repository is still on the `v3` development line, so merged work remains
under **Unreleased** until a versioned release is cut.

## Maintenance rule

Every pull request targeting `v3` that changes contract behavior, state,
migrations, schemas, deployment artifacts, or operator-facing behavior must add
a concise entry under **Unreleased**. Entries must:

- describe the user, operator, or security impact rather than only naming files;
- link the pull request;
- call out migrations, compatibility constraints, and breaking changes;
- avoid claiming production readiness unless the relevant release gates are
  complete.

Documentation-only, test-only, and internal refactors may omit an entry when
the pull request explains why no externally relevant behavior changed. When a
release is cut, move the applicable entries into a dated version section and
add the release or comparison link.

## Unreleased

### Security

- Enforced soulbound behavior in `cw721-roles` and
  `dao-voting-cw721-roles`, completed the two-phase ownership handover, and
  added guarded migration support. Follow-up tests assert error-chain behavior
  without relying on concrete downcasts.
  ([JakeHartnell/dao-contracts#3](https://github.com/JakeHartnell/dao-contracts/pull/3))
- Hardened gauge accounting and lifecycle invariants: vote inputs are bounded
  and validated, voting-power hooks process all gauges, reset work is bounded,
  removed options cannot be recreated by stale votes or hooks, and removed
  tally weight is reclaimed from total capacity.
  ([#1](https://github.com/juno-ai-dev/dao-contracts/pull/1))
- Hardened augmented bonding curve lifecycle, escrow, migration, arithmetic,
  and implementation trust: unsafe transferable-token vesting configurations
  are rejected; failed-Hatch gross refunds remain solvent; caps, deadlines,
  slippage, and phase transitions are enforced; curve arithmetic is checked
  and bounded; factory code IDs are verified against live checksums; and legacy
  Hatch contribution reconstruction is resumable with fixed-size batches.
  ([#2](https://github.com/juno-ai-dev/dao-contracts/pull/2))

### Added

- Added `dao-voting-juno-staked`, a thin DAO DAO voting module backed by
  Juno's historical `x/voting-snapshot` queries. It requires Juno v30 (including
  the `uni-7` deployment target), translates DAO heights to the previous settled
  Juno snapshot for consistent beginning-of-block proposal power, and leaves
  liquid-staking-token exclusion chain-owned. Synchronous staking-delta hooks
  are intentionally not exposed because snapshots settle in EndBlock and
  validator-wide changes cannot be translated losslessly into per-delegator
  callbacks. Deployments must begin after the chain's snapshot backfill boundary
  and must not migrate unreleased same-version hook-enabled builds
  ([#4](https://github.com/juno-ai-dev/dao-contracts/pull/4)).
- Added the gauge orchestrator, gauge adapter, budget allocator, shared gauge
  interface, release scripts, operational documentation, schemas, and
  production-readiness checks.
  ([#1](https://github.com/juno-ai-dev/dao-contracts/pull/1))
- Added `cw-abc`, `cw-curves`, and `dao-abc-factory` with schemas, differential
  curve tests, audit regressions, and test-tube integration support.
  ([#2](https://github.com/juno-ai-dev/dao-contracts/pull/2))

### Changed

- Preserved Cargo lockfile v3 compatibility across the combined gauge and ABC
  workspace so the gauge Rust/Cargo 1.81 path and the pinned ABC nightly remain
  reproducible.
  ([#2](https://github.com/juno-ai-dev/dao-contracts/pull/2))
