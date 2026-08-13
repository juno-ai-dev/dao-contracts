# Gauge event contract

Mutation responses use a stable `action` attribute. Indexers should select the
single Wasm event containing that attribute and then read the fields below.
Additional attributes may be added compatibly; existing names and meanings must
not change without an API-major release.

## Orchestrator

| Action | Stable fields |
|---|---|
| `create_gauge` | `sender`, `gauge_id`, `adapter` |
| `update_gauge` | `sender`, `gauge_id`, `epoch_size`, `min_percent_selected`, `max_options_selected`, `max_available_percentage` |
| `stop_gauge`, `resume_gauge` | `sender`, `gauge_id` |
| `add_option`, `remove_option` | `sender`, `gauge_id`, `option` |
| `place_vote` | `sender`, `gauge_id`, `option_count`, `voting_power` |
| `member_changed_hook` | `hook_caller`, `member_count`, `member` (repeated), `updated_votes` |
| `stake_change_hook` | `hook_caller`, `kind`, `voter`, `amount`, `updated_votes` |
| `nft_stake_change_hook` | `hook_caller`, `kind`, `voter`, `token_count`, `updated_votes`; NFT stake also has `token_id` |
| `reset_gauge` | `sender`, `gauge_id`, `processed`, `complete`, `next_reset` |
| `execute_tally` | `sender`, `gauge_id`, `next_epoch`, `selected_count`, `message_count` |
| `open_snapshot_epoch` | `sender`, `gauge_id`, `epoch_id`, `snapshot_height`, `snapshot_total_power`, `opens_at`, `closes_at`, `execution_deadline`, `policy_version`, `min_turnout_bps`, `epoch_budget`, `denom`, `retained_option`, `option_count` |
| `place_snapshot_vote` | `sender`, `gauge_id`, `epoch_id`, `snapshot_height`, `voting_power`, `option_count`, `participating_power`, `allocated_power`, `total_cast`, `retained_option_power`, `unallocated_power` |
| `execute_snapshot_epoch` | `sender`, `gauge_id`, `epoch_id`, `snapshot_height`, `snapshot_total_power`, `participating_power`, `allocated_power`, `total_cast`, `retained_option_power`, `unallocated_power`, `selected_project_power`, `emitted_value`, `retained_value`, `min_turnout_bps`, `policy_version`, `epoch_budget`, `denom`, `execution_deadline`, `outcome`, `message_count`; insufficient funds also has `required_value`, `available_balance` |
| `expire_snapshot_epoch` | same accounting fields as `execute_snapshot_epoch`; `outcome=expired`, `message_count=0` |
| `abort_snapshot_epoch` | same accounting fields as `execute_snapshot_epoch`, plus `reason`; `outcome=aborted`, `message_count=0` |
| `update_snapshot_policy` | `sender`, `gauge_id`, `policy_version`, `min_turnout_bps`, `epoch_budget`, `denom`, `retained_option`, `execution_window_seconds` |
| `cleanup_snapshot_epoch` | `sender`, `gauge_id`, `epoch_id`, `processed`, `phase`, `complete` |
| `add_hook`, `remove_hook` | `sender`, `hook` |
| `vote_hook_succeeded`, `remove_failed_vote_hook` | `hook`, `reply_id` |
| `migrate` | `from_version`, `to_version`, `migrated_records` |

`min_percent_selected` and `max_available_percentage` are `none` when
disabled. `updated_votes` counts voter/gauge records changed, not voters or
options.

Snapshot terminal `outcome` is one of `distributed`,
`no_distribution_turnout`, `no_distribution_zero_participation`,
`no_eligible_options`, `insufficient_funds`, `expired`, or `aborted`.
`retained_option=none` means legacy selection semantics are configured.

## Marketing adapter

| Action | Stable fields |
|---|---|
| `create_submission` | `sender`, `submission`, `bond_state`, `liabilities`; bonded rows also include `depositor`, `bond_denom`, `bond_amount` |
| `update_submission` | `sender`, `submission`, `bond_state` |
| `reject` | `sender`, `submission`, `kind`, `bond_state`, `bond_amount`, `liabilities`; bonded rows also include `bond_denom` |
| `return_deposits` | `sender`, `processed`, `complete`, `next_cursor`, `message_count`, `refunded_amount`, `liabilities` |
| `migrate` | `from_version`, `to_version`, `migrated_records`, `migrated_bonds`, `liabilities` |

Bond states are `none`, `active`, `refunded`, or `forfeited`. Amount fields are
base-unit integers. `next_cursor=none` means the refund scan is complete.

## Budget allocator

| Action | Stable fields |
|---|---|
| `add_option`, `remove_option` | `sender`, `option` |
| `update_budget` | `sender`, `denom`, `amount` |

Both ownable adapters emit `update_ownership` with stable fields `sender`,
`owner`, `pending_owner`, and `pending_expiry`. The last three retain the
canonical `cw-ownable` serialization; absent values are the string `none`.
