# Gauge production-readiness evidence

Status date: 2026-07-14. This document classifies the current working tree
against `GOAL.md`; it is not a production approval or security audit.
The direct evidence map is [`GOAL-AUDIT.md`](./GOAL-AUDIT.md).

Update (2026-08-05): the epoch-snapshot power mode added after this readiness
review has focused unit, randomized-model, migration, compatibility, package,
schema, and release-build checks. The independent audit, maximum-path gas,
optimized-artifact chain exercise, and public-testnet evidence below do not yet
cover that extension. Nothing in this document should be read as production
approval for the new mode until those gates are recorded.

Update (2026-08-12): the Juno Voice v1 review identified snapshot-allocation
renormalization and epoch-liveness defects in that extension. The current
candidate adds the participating-power denominator, explicit retained-option
sink, funded opening, adapter emitted/retained accounting, terminal
insufficient-funds outcome, deadline expiry, and reasoned owner abort. Focused
unit and model tests are local evidence only. The earlier checksums, readiness
conclusion, audit statement, and claim that no repository-local gate remains
do not approve this candidate; all clean-build, gas, chain, independent-review,
migration, and release-evidence gates must be rerun for its exact commit.

Update (2026-08-13): Juno Voice v2 uses fresh instantiation and imports no v1
state, so its cutover must not call this contract's generic `migrate` entry
point. That does not remove the upstream compatibility surface: migration from
the explicitly supported historical gauge versions remains identity-guarded,
bounded, atomic, and covered by unit and multitest regressions. The complete
locked workspace suite and exact Rust 1.81 schema regeneration pass locally.
A dirty-tree Rust 1.81 Wasm build, optimized diagnostically with Binaryen 132,
produces a 676,970-byte orchestrator that passes the export allowlist,
`wasm-tools 1.254.0`, and `cosmwasm-check 1.5.11`. Those results neither turn
migration into a Juno deployment step nor replace a clean digest-pinned build.

The dirty-tree Rust 1.81 validation build (linker metadata stripped, not the
optimizer-built release) passed `cosmwasm-check 1.5.11`, Wasm validation, the
800,000-byte cap, and exact export checks: orchestrator
`ea49e9b80c8432892c895c30911a081f1523b5ecd19814f2ebe1efff9fae38d4`
(758,956 bytes), marketing adapter
`09d23451af9d676c18499eef75c9a8d6aae058d6ce797329d75831dd54891414`
(416,411 bytes), and allocator
`d7663e3de952e5fbf1e056bc8cdb7532366d6e65cac94a988a6a1faece67a2bc`
(265,656 bytes). These checksums are diagnostic only and must be reproduced
from the clean reviewed commit before release.

## Locally verified

- Vote inputs reject duplicate, empty, zero-weight, oversized, over-100%,
  round-to-zero, and extreme-decimal payloads before accounting mutation.
- Tally, total-cast, and sorted-index arithmetic uses checked operations. A
  randomized reference-model test audits all three structures after vote and
  power-change sequences.
- Removed options are immediately non-selectable. Zero-power/restake cannot
  reactivate a tombstone, adapter-invalid leaders cannot consume valid
  selection slots, and bounded reset cleanup removes tombstones. Removal is
  covered with zero, one, and many active voters; replacement and abstention;
  real CW4, CW20-staked, native/token-factory-staked, and CW721-staked power
  hooks; and interrupted multi-call reset cleanup.
- Power-change hooks process a complete, capped gauge set and reject oversized
  cw4/NFT batches. Real cw4, CW20-staked, native/token-factory-staked, and
  CW721-staked hook stacks are exercised in multitests. The shared active-gauge
  iterator is tested immediately below, at, and above 100 records; cw4 member
  and CW721 token payload handlers likewise accept 99/100 and reject 101 before
  mutation.
- Reset has a persistent cursor, rejects batch sizes outside 1–100, catches up
  its schedule with checked arithmetic, and is tested at 0, 1, batch-1, batch,
  batch+1, and multi-batch option counts.
- Vote-hook replies use stable, namespaced address mappings. Multiple mixed
  failures leave successful subscribers registered and do not revert voting.
- Marketing bonds store asset, amount, depositor, and lifecycle. Aggregate
  liabilities, native/CW20 solvency, one-time rejection/refund transitions,
  unsolicited surplus, ownership changes, and resumable 50-row wind-down are
  covered.
- Caps burn/unallocate excess rather than renormalizing. A real-stack
  allocation matrix covers no votes, partial turnout, one/many winners, caps
  above/below actual shares, threshold exclusion, cap-to-zero, and a non-empty
  selection whose adapter intentionally returns no messages. The shared
  adapter validator enforces unique, positive global shares totaling at most
  one; allocator tests cover proportional distribution and integer dust.
- Gauges, options, vote records, subscribers, adapter messages, hook payloads,
  strings, submissions, attachment, refunds, and public list responses have
  structural bounds. Non-receiving execute endpoints reject funds.
- The adapter protocol is shared through `gauge-interface`; config/health,
  ownership, liabilities, and batch progress are queryable.
- Orchestrator and marketing migrations accept only source versions `2.4.2`
  and `2.5.0`, preserve populated state, and reject other identities/versions.
  The budget allocator has no historical artifact and exports no migrate entry
  point.
- Stable mutation events and indexer assertions cover gauge creation/update,
  option add/remove, vote, CW4/CW20/native/CW721 power updates, reset,
  execution, hook registration/removal and success/failure replies, marketing
  submission/update/rejection/refund, allocator changes, ownership, stop/resume,
  and both supported migrations.
- Checked-in schemas regenerate successfully. Package contents include the
  required license and NOTICE files. Release-evidence and production-approval
  validators pass their positive and adversarial fixtures. All four normalized
  crate tarballs build and pass their tests on Rust 1.81; the shared interface
  pins pre-edition-2024 crypto dependencies so verification also works without
  the workspace lockfile.
- The complete Rust 1.81 `cargo test --workspace --locked` suite passes,
  including all workspace and doc tests (the eight pre-existing chain
  integration tests remain explicitly ignored by their crate). The local host
  lacked clang, so the run used Ubuntu Noble's signed `libclang1-18`,
  `libllvm18`, and `libclang-common-18-dev` extracted under `/tmp`; no system
  packages or repository files were altered.
- Dependency advisories, licenses, and sources pass `cargo-deny` using the
  Wasm-target graph and the documented RustSec exception.
- Focused Rust 1.81 coverage records 11,599/11,820 lines (98.13%). More
  importantly, scoped mutation runs cover critical orchestrator accounting,
  reset, removal, selection, hooks, execution, reply, and migration paths;
  marketing submission, rejection, refund, and migration paths; and allocator
  mutation/authorization. Final runs report no surviving mutants: orchestrator
  tranches caught 45/57 and 44/64 with the remainder compiler-unviable,
  marketing caught 32/32, and allocator caught 3/3.
- Fresh Rust 1.81 release Wasm for all three contracts passes
  `cosmwasm-check 1.5.11`, the explicit 800,000-byte limit, and exact export
  allowlists. Only orchestrator exports `reply`; only orchestrator and
  marketing export `migrate`. The current-tree, dirty-build verification
  produced these non-release SHA-256 values and sizes: orchestrator
  `29f6eccbc2f2380d6d866a9af377d96546b17e812248e60b7e0bd3fd6ed39a2a`
  (545,139 bytes), marketing
  `5dadeff66f05277eb5f5fbdb9edb8959e89a7be29311a391c8e38216a2dae60f`
  (354,222 bytes), and allocator
  `9f0625b2a881a703647e60380fc82dcdaa8ce8d7366fee30b336e5b7d5a6b7cc`
  (225,898 bytes). Release checksums must be reproduced from the clean tagged
  commit and may differ.

## Local work not yet proved complete

The 2026-08-12 candidate still needs accepted clean commits and their required
clean-checkout CI, deterministic optimized artifacts from the digest-pinned
builder, maximum-bound gas evidence, and exact-artifact integration with the
Juno Voice registry adapter. The Juno deployment remains fresh-only. Passing
the working-tree suites, schema checks, or diagnostic artifact checks is not a
substitute for those gates or independent review.

## Genuinely external release gates

These cannot be satisfied by unit tests or local repository edits:

- Review and commit the candidate, then obtain green required clean-checkout CI
  for that exact SHA. Retain the PR's original red/green regression evidence;
  current mutation results do not recreate historical CI against `d2da47e60`.
- Measure worst-case vote, every power hook, selection/execution,
  reset/removal, attachment, and refund gas/response sizes on the target
  chain/VM with an approved safety margin.
- Deploy the exact optimized artifacts on a representative local chain and a
  public testnet; execute all required real DAO/voting/keeper scenarios and a
  conservative maximum-state soak of at least two epochs.
- Commission and complete an independent CosmWasm security audit, resolve all
  critical/high findings, and record accepted lower-severity risk.
- Independently reproduce optimized checksums from the clean tagged commit and
  obtain maintainer sign-off on the audit attestation, chain report, manifest,
  deployment payloads, dashboards, alerts, and runbooks.
- Run the low-value canary for at least two complete epochs, including a power
  change and multi-call reset, then obtain the hash-bound governance approval
  for production limits and residual risk.

The validators in `scripts/` deliberately reject placeholders for these
documents. A green local build does not satisfy any external gate.

## Reproduction commands

Run with Rust 1.81.0 and the committed lockfile:

```sh
cargo +1.81.0 fmt --all -- --check
cargo +1.81.0 test --locked \
  -p gauge-interface -p gauge-orchestrator \
  -p gauge-adapter -p gauge-budget-allocator
cargo +1.81.0 test --workspace --locked
cargo +1.81.0 clippy --locked --all-targets \
  -p gauge-interface -p gauge-orchestrator \
  -p gauge-adapter -p gauge-budget-allocator -- -D warnings
cargo +1.81.0 llvm-cov --locked --json \
  -p gauge-interface -p gauge-orchestrator \
  -p gauge-adapter -p gauge-budget-allocator
cargo-deny --exclude-dev check advisories licenses sources \
  --hide-inclusion-graph
scripts/test-gauge-release-evidence.sh
scripts/test-gauge-production-approval.sh
scripts/test-gauge-packages.sh
GAUGE_ALLOW_DIRTY_BUILD=1 \
  scripts/build-gauge-release.sh /tmp/gauge-release-current
cosmwasm-check /tmp/gauge-release-current/*.wasm
scripts/validate-gauge-wasm.sh /tmp/gauge-release-current
```

The workspace test's `osmosis-test-tube` dependency requires libclang and its
Clang resource headers for bindgen. The CI workflow additionally regenerates
schemas, checks package contents, and performs the pinned artifact build. Omit
`GAUGE_ALLOW_DIRTY_BUILD=1` for a release build; the build script then requires
a clean worktree and permits manifest generation.
