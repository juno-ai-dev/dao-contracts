use cosmwasm_schema::{cw_serde, QueryResponses};
use cosmwasm_std::{Decimal, Uint128};
use cw4::MemberChangedHookMsg;
use dao_hooks::{nft_stake::NftStakeChangedHookMsg, stake::StakeChangedHookMsg};
pub use gauge_interface::{
    AdapterQueryMsg, AllOptionsResponse, CheckOptionResponse, SampleGaugeMsgsResponse,
};

use crate::state::{Reset, Vote};

type GaugeId = u64;

#[cw_serde]
pub struct InstantiateMsg {
    /// Address of contract to that contains all voting powers (where we query)
    pub voting_powers: String,
    /// Addres that will call voting power change hooks (often same as voting power contract)
    pub hook_caller: String,
    /// When set, hooks are disabled and every gauge uses one historical
    /// voting-power snapshot per explicitly opened epoch.
    pub epoch_snapshot: Option<EpochSnapshotModeConfig>,
    /// Address that can add new gauges or stop them
    pub owner: String,
    /// Allow attaching multiple adaptors during instantiation.
    /// Important, as instantiation and CreateGauge both come from DAO proposals
    /// and without this argument, you need 2 cycles to create and configure a gauge
    pub gauges: Option<Vec<GaugeConfig>>,
}

#[cw_serde]
pub struct GaugeConfig {
    /// Name of the gauge (for UI)
    pub title: String,
    /// Address of contract to serve gauge-specific info (AdapterQueryMsg)
    pub adapter: String,
    /// Frequency (in seconds) the gauge executes messages, typically something like 7*86400
    pub epoch_size: u64,
    /// Minimum percentage of votes needed by a given option to be in the selected set.
    /// If unset, there is no minimum percentage, just the `max_options_selected` limit.
    pub min_percent_selected: Option<Decimal>,
    /// Maximum number of Options to make the selected set. Needed even with
    /// `min_percent_selected` to provide some guarantees on gas usage of this query.
    pub max_options_selected: u32,
    // Any votes above that percentage will be discarded
    pub max_available_percentage: Option<Decimal>,
    /// If set, the gauge can be reset periodically, every `reset_epoch` seconds.
    pub reset_epoch: Option<u64>,
    /// Required in epoch-snapshot mode and rejected in hook mode.
    pub snapshot_policy: Option<EpochSnapshotPolicy>,
}

#[cw_serde]
pub struct EpochSnapshotModeConfig {
    /// Stop-only safety authority. Only the owner may resume.
    pub guardian: String,
}

#[cw_serde]
pub struct EpochSnapshotPolicy {
    pub min_turnout_bps: u16,
    pub epoch_budget: Uint128,
    pub denom: String,
    /// Optional non-project sink. It counts as an affirmative ballot signal
    /// but is excluded from project selection and adapter execution.
    pub retained_option: Option<String>,
    /// Bounded interval after voting closes during which execution may occur.
    pub execution_window_seconds: u64,
}

#[cw_serde]
pub enum PowerSourceResponse {
    Hook { hook_caller: String },
    EpochSnapshot { guardian: String },
}

#[cw_serde]
pub enum ExecuteMsg {
    /// Updates gauge voting power in Token DAOs when a user stakes or unstakes
    StakeChangeHook(StakeChangedHookMsg),
    /// Updates gauge voting power in NFT DAOs when a user stakes or unstakes
    NftStakeChangeHook(NftStakeChangedHookMsg),
    /// Updates gauge voting power for membership changes
    MemberChangedHook(MemberChangedHookMsg),
    /// This creates a new Gauge, returns CreateGaugeReply JSON-encoded in the data field.
    /// Can only be called by owner
    CreateGauge(GaugeConfig),
    /// Allows owner to update certain parameters of GaugeConfig.
    /// If you want to change next_epoch value, you need to use migration.
    UpdateGauge {
        gauge_id: u64,
        epoch_size: Option<u64>,
        // Some<0> would set min_percent_selected to None
        min_percent_selected: Option<Decimal>,
        max_options_selected: Option<u32>,
        max_available_percentage: Option<Decimal>,
    },
    /// Freezes voting, reset, and epoch execution for a gauge. Voting-power
    /// hooks continue updating existing votes so accounting remains current.
    StopGauge { gauge: u64 },
    /// Owner-only: resumes voting, reset, and epoch execution for a stopped gauge.
    ResumeGauge { gauge: u64 },
    /// Publicly opens the next epoch at one historical power height.
    OpenEpoch { gauge: u64 },
    /// Publicly terminalizes an unexecuted epoch at or after its deadline.
    ExpireEpoch { gauge: u64 },
    /// Owner-only terminal recovery for an unrecoverable adapter or migration
    /// failure. The reason is persisted in the terminal outcome.
    AbortEpoch { gauge: u64, reason: String },
    /// Owner-only future-epoch policy update.
    UpdateSnapshotPolicy {
        gauge: u64,
        policy: EpochSnapshotPolicy,
    },
    /// Public bounded cleanup of one terminal epoch.
    CleanupEpoch { gauge: u64, epoch: u64, limit: u32 },
    /// Resets all votes on a given gauge if it is configured to be periodically reset and the epoch has passed.
    /// One call to this will only clear `batch_size` votes to prevent gas exhaustion. Call repeatedly to clear all votes.
    ResetGauge { gauge: u64, batch_size: u32 },
    // WISH: make this implicit - call it inside PlaceVote.
    // If not, I would just make it invisible to user in UI (smart client adds it if needed)
    /// Try to add an option. Error if no such gauge, or option already registered.
    /// Otherwise check adapter and error if invalid.
    /// Can be called by anyone, not just owner
    AddOption { gauge: u64, option: String },
    /// Allows the owner to remove an option. This is useful if the option is no longer valid
    /// or if the owner wants to remove all votes from a valid option.
    RemoveOption { gauge: u64, option: String },
    /// Place your vote on the gauge. Can be updated anytime
    PlaceVotes {
        /// Gauge to vote on
        gauge: u64,
        /// The options to put my vote on, along with positive weights whose sum
        /// is at most 1.0. Any unused weight is intentionally unallocated.
        /// "None" means remove existing votes and abstain
        votes: Option<Vec<Vote>>,
    },
    /// Takes a sample of the current tally and execute the proper messages to make it work
    Execute { gauge: u64 },
    /// Owner-only: register a contract address to receive
    /// [`GaugeVoteHookMsg`](crate::hooks::GaugeVoteHookMsg) submessages on
    /// every `PlaceVotes`. Subscribers receive `reply_on_error`-style
    /// submessages — a hook that errors is auto-unregistered.
    AddHook { addr: String },
    /// Owner-only: drop a previously-registered hook subscriber.
    RemoveHook { addr: String },
}

#[cw_serde]
pub struct CreateGaugeReply {
    /// Id of the gauge that was just created
    pub id: u64,
}

/// Queries the gauge exposes
#[cw_serde]
#[derive(QueryResponses)]
pub enum QueryMsg {
    #[returns(ConfigResponse)]
    Config {},
    #[returns(dao_interface::voting::InfoResponse)]
    Info {},
    #[returns(GaugeResponse)]
    Gauge { id: u64 },
    #[returns(ListGaugesResponse)]
    ListGauges {
        start_after: Option<u64>,
        limit: Option<u32>,
    },
    #[returns(VoteResponse)]
    Vote { gauge: u64, voter: String },
    #[returns(ListVotesResponse)]
    ListVotes {
        gauge: u64,
        start_after: Option<String>,
        limit: Option<u32>,
    },
    #[returns(ListOptionsResponse)]
    ListOptions {
        gauge: u64,
        start_after: Option<String>,
        limit: Option<u32>,
    },
    #[returns(SelectedSetResponse)]
    SelectedSet { gauge: u64 },
    #[returns(LastExecutedSetResponse)]
    LastExecutedSet { gauge: u64 },
    /// Bounded reconciliation of the tally primary map, sorted index,
    /// aggregate total, tombstones, and reset cursor for one gauge.
    #[returns(GaugeHealthResponse)]
    GaugeHealth { gauge: u64 },
    /// List the currently-registered `GaugeVoteHook` subscribers.
    #[returns(GetHooksResponse)]
    GetHooks {},
    #[returns(EpochResponse)]
    Epoch { gauge: u64, epoch: u64 },
    #[returns(ListEpochsResponse)]
    ListEpochs {
        gauge: u64,
        start_after: Option<u64>,
        limit: Option<u32>,
    },
    #[returns(EpochBallotResponse)]
    EpochBallot {
        gauge: u64,
        epoch: u64,
        voter: String,
    },
    #[returns(ListEpochBallotsResponse)]
    ListEpochBallots {
        gauge: u64,
        epoch: u64,
        start_after: Option<u32>,
        limit: Option<u32>,
    },
    #[returns(EpochAllocationsResponse)]
    EpochAllocations {
        gauge: u64,
        epoch: u64,
        start_after: Option<String>,
        limit: Option<u32>,
    },
}

#[cw_serde]
pub struct ConfigResponse {
    pub owner: String,
    pub dao_core: String,
    pub voting_powers: String,
    pub hook_caller: String,
    pub power_source: PowerSourceResponse,
}

#[cw_serde]
pub struct GetHooksResponse {
    pub hooks: Vec<String>,
}

/// Information about one gauge
#[cw_serde]
pub struct GaugeResponse {
    pub id: u64,
    /// Name of the gauge (for UI)
    pub title: String,
    /// Address of contract to serve gauge-specific info (AdapterQueryMsg)
    pub adapter: String,
    /// Frequency (in seconds) the gauge executes messages, typically something like 7*86400
    pub epoch_size: u64,
    /// Minimum percentage of votes needed by a given option to be in the selected set.
    /// If unset, there is no minimum percentage, just the `max_options_selected` limit.
    pub min_percent_selected: Option<Decimal>,
    /// Maximum number of Options to make the selected set. Needed even with
    /// `min_percent_selected` to provide some guarantees on gas usage of this query.
    pub max_options_selected: u32,
    // Any votes above that percentage will be discarded
    pub max_available_percentage: Option<Decimal>,
    /// True if the gauge is stopped
    pub is_stopped: bool,
    /// UNIX time (seconds) when next epoch may be executed. May be future or past
    pub next_epoch: u64,
    /// Set this in migration if the gauge should be periodically reset
    pub reset: Option<Reset>,
    pub snapshot_policy: Option<EpochSnapshotPolicy>,
    pub current_epoch: Option<u64>,
}

#[cw_serde]
pub enum EpochOutcome {
    Open,
    Distributed {
        message_count: u32,
    },
    NoDistributionTurnout,
    NoDistributionZeroParticipation,
    NoEligibleOptions,
    InsufficientFunds {
        required: Uint128,
        available: Uint128,
    },
    Expired,
    Aborted {
        reason: String,
    },
}

#[cw_serde]
pub enum CleanupPhase {
    Ballots,
    Options,
    Complete,
}

#[cw_serde]
pub struct CleanupProgress {
    pub phase: CleanupPhase,
    pub cursor: u32,
    pub complete: bool,
}

#[cw_serde]
pub struct EpochResponse {
    pub gauge_id: u64,
    pub epoch_id: u64,
    pub snapshot_height: u64,
    pub snapshot_total_power: Uint128,
    pub participating_power: Uint128,
    /// Raw per-option allocation sum, including the retained option. Kept in
    /// addition to the historical `total_cast` name for a loud v2 interface.
    pub allocated_power: Uint128,
    pub total_cast: Uint128,
    pub retained_option: Option<String>,
    pub retained_option_power: Uint128,
    pub unallocated_power: Uint128,
    pub selected_project_power: Uint128,
    pub emitted_value: Uint128,
    pub retained_value: Uint128,
    pub min_turnout_bps: u16,
    pub policy_version: u64,
    pub epoch_budget: Uint128,
    pub denom: String,
    pub opens_at: u64,
    pub closes_at: u64,
    pub execution_deadline: u64,
    pub voter_count: u32,
    pub option_count: u32,
    pub outcome: EpochOutcome,
    pub cleanup: CleanupProgress,
}

#[cw_serde]
pub struct ListEpochsResponse {
    pub epochs: Vec<EpochResponse>,
}

#[cw_serde]
pub struct EpochBallotInfo {
    pub voter: String,
    pub power: Uint128,
    pub votes: Vec<Vote>,
    pub cast_at: u64,
    pub revised_at: u64,
    pub revisions: u32,
    /// Stable, epoch-scoped cursor assigned on the voter's first ballot.
    pub receipt_index: u32,
}

#[cw_serde]
pub struct EpochBallotResponse {
    pub ballot: Option<EpochBallotInfo>,
}

#[cw_serde]
pub struct ListEpochBallotsResponse {
    pub ballots: Vec<EpochBallotInfo>,
    /// Last scanned receipt index when another bounded page remains. A page
    /// can contain fewer ballots than `limit` because abstentions remove the
    /// active ballot while retaining its stable index.
    pub next_start_after: Option<u32>,
}

#[cw_serde]
pub struct EpochAllocationsResponse {
    pub allocations: Vec<(String, Uint128)>,
}

/// Information about one gauge
#[cw_serde]
pub struct ListGaugesResponse {
    pub gauges: Vec<GaugeResponse>,
}

/// Information about a vote that was cast.
#[cw_serde]
pub struct VoteInfo {
    /// The address that voted.
    pub voter: String,
    /// List of all votes with power
    pub votes: Vec<Vote>,
    /// Timestamp when vote was cast.
    /// Allow `None` for 0-cost migration from current data
    pub cast: Option<u64>,
}

/// Information about a vote.
#[cw_serde]
pub struct VoteResponse {
    /// None if no such vote, Some otherwise.
    pub vote: Option<VoteInfo>,
}

/// Information about all votes on the gauge
#[cw_serde]
pub struct ListVotesResponse {
    pub votes: Vec<VoteInfo>,
}

/// List all available options ordered by the option string.
/// Also returns the current voting power assigned to that option.
/// You will need to paginate to collect them all.
#[cw_serde]
pub struct ListOptionsResponse {
    pub options: Vec<(String, Uint128)>,
}

#[cw_serde]
pub struct GaugeHealthResponse {
    pub gauge_id: u64,
    pub option_count: u32,
    pub active_option_count: u32,
    pub invalid_option_count: u32,
    pub indexed_option_count: u32,
    pub tally_sum: Uint128,
    pub total_cast: Uint128,
    pub mismatch_count: u32,
    pub first_mismatch: Option<String>,
    pub reset_cursor: Option<String>,
    /// False if legacy/corrupt state exceeded the enforced option bound and
    /// the reconciliation deliberately stopped rather than silently truncating.
    pub scan_complete: bool,
    pub consistent: bool,
}

/// List the options that were selected in the last executed set.
#[cw_serde]
pub struct LastExecutedSetResponse {
    /// `None` if no vote has been executed yet
    pub votes: Option<Vec<(String, Uint128)>>,
}

/// List the top options by power that would make it into the selected set.
/// Ordered from highest votes to lowest
#[cw_serde]
pub struct SelectedSetResponse {
    pub votes: Vec<(String, Uint128)>,
}

#[cw_serde]
pub struct MigrateMsg {
    /// Optional per-gauge schedule updates. At most 100 unique gauge IDs.
    /// The whole migration is rejected before writes if any update is invalid.
    pub gauge_config: Option<Vec<(GaugeId, GaugeMigrationConfig)>>,
}

#[cw_serde]
#[derive(Default)]
pub struct GaugeMigrationConfig {
    /// When the next epoch should be executed
    pub next_epoch: Option<u64>,
    /// If set, the gauge will be reset periodically
    pub reset: Option<ResetMigrationConfig>,
}

#[cw_serde]
pub struct ResetMigrationConfig {
    /// How often to reset the gauge (in seconds)
    pub reset_epoch: u64,
    /// When to start the first reset
    pub next_reset: u64,
}

#[cfg(test)]
mod schema_smoke_tests {
    use super::*;
    use cosmwasm_std::from_json;

    #[test]
    fn representative_external_payloads_deserialize() {
        let _: InstantiateMsg = from_json(
            br#"{"voting_powers":"voting","hook_caller":"hooks","owner":"owner","gauges":null}"#,
        )
        .unwrap();
        let vote: ExecuteMsg = from_json(
            br#"{"place_votes":{"gauge":7,"votes":[{"option":"alpha","weight":"0.5"}]}}"#,
        )
        .unwrap();
        assert!(matches!(vote, ExecuteMsg::PlaceVotes { gauge: 7, .. }));
        let _: QueryMsg =
            from_json(br#"{"list_options":{"gauge":7,"start_after":"alpha","limit":25}}"#).unwrap();
        let _: QueryMsg = from_json(br#"{"gauge_health":{"gauge":7}}"#).unwrap();
        let _: GaugeHealthResponse = from_json(
            br#"{"gauge_id":7,"option_count":1,"active_option_count":1,"invalid_option_count":0,"indexed_option_count":1,"tally_sum":"42","total_cast":"42","mismatch_count":0,"first_mismatch":null,"reset_cursor":null,"scan_complete":true,"consistent":true}"#,
        )
        .unwrap();
        let _: SelectedSetResponse = from_json(br#"{"votes":[["alpha","42"]]}"#).unwrap();
        let _: CreateGaugeReply = from_json(br#"{"id":7}"#).unwrap();
        let _: MigrateMsg = from_json(
            br#"{"gauge_config":[[7,{"next_epoch":1234,"reset":{"reset_epoch":600,"next_reset":1800}}]]}"#,
        )
        .unwrap();
    }
}
