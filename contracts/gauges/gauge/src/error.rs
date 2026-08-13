use cosmwasm_std::{Decimal, StdError, Uint128};
use cw_hooks::HookError;
use cw_utils::PaymentError;
use thiserror::Error;

#[derive(Error, Debug, PartialEq)]
pub enum ContractError {
    #[error("{0}")]
    Std(#[from] StdError),

    #[error(transparent)]
    Hooks(#[from] HookError),

    #[error(transparent)]
    Payment(#[from] PaymentError),

    #[error("Unauthorized")]
    Unauthorized {},

    #[error("Power-change hooks are disabled in epoch-snapshot mode")]
    HooksDisabledInSnapshotMode {},

    #[error("This operation requires epoch-snapshot mode")]
    SnapshotModeRequired {},

    #[error("Snapshot policy is required in epoch-snapshot mode and forbidden in hook mode")]
    InvalidSnapshotPolicy {},

    #[error("Snapshot turnout basis points must be at most 10000")]
    InvalidTurnoutBps {},

    #[error("Snapshot epoch budget and native denomination must be nonzero")]
    InvalidEpochBudget {},

    #[error("Snapshot execution window must be greater than zero")]
    InvalidExecutionWindow {},

    #[error("Snapshot retained option is invalid")]
    InvalidRetainedOption {},

    #[error("Configured retained option {option} is absent from gauge {gauge}")]
    RetainedOptionMissing { gauge: u64, option: String },

    #[error("Program Vault balance {available} is below epoch budget {required} {denom}")]
    InsufficientEpochFunding {
        required: Uint128,
        available: Uint128,
        denom: String,
    },

    #[error("Snapshot epoch {epoch} for gauge {gauge} is not open")]
    EpochNotOpen { gauge: u64, epoch: u64 },

    #[error("Gauge {0} already has an open snapshot epoch")]
    EpochAlreadyOpen(u64),

    #[error("Snapshot epoch voting closes at {closes_at}; current time is {current}")]
    SnapshotVotingClosed { closes_at: u64, current: u64 },

    #[error("Snapshot epoch voting remains open until {closes_at}; current time is {current}")]
    SnapshotVotingOpen { closes_at: u64, current: u64 },

    #[error(
        "Snapshot epoch execution deadline {deadline} has been reached; current time is {current}"
    )]
    ExecutionDeadlineReached { deadline: u64, current: u64 },

    #[error("Snapshot epoch cannot expire before deadline {deadline}; current time is {current}")]
    ExecutionDeadlineNotReached { deadline: u64, current: u64 },

    #[error("Epoch abort reason must contain 1 to 2048 bytes")]
    InvalidAbortReason {},

    #[error("Historical total voting power at height {height} is zero")]
    ZeroSnapshotTotalPower { height: u64 },

    #[error("Voting module answered snapshot height {actual}; expected {expected}")]
    SnapshotHeightMismatch { expected: u64, actual: u64 },

    #[error("Snapshot height or schedule arithmetic overflowed")]
    SnapshotArithmetic {},

    #[error("Snapshot epoch cleanup limit must be between 1 and 100")]
    InvalidCleanupLimit {},

    #[error("Snapshot epoch is not terminal")]
    EpochNotTerminal {},

    #[error("Gauge {gauge_id} selection configuration is locked while a snapshot epoch is open")]
    SnapshotGaugeConfigLocked { gauge_id: u64 },

    #[error("Gauge with ID {0} does not exists")]
    GaugeMissing(u64),

    #[error("Voted for {0} times total voting power. Limit 1.0")]
    TooMuchVotingWeight(Decimal),

    #[error("Vote-weight sum overflowed")]
    VoteWeightOverflow {},

    #[error("Vote weight must be greater than zero for option {option}")]
    ZeroVoteWeight { option: String },

    #[error("Vote option must not be empty")]
    EmptyVoteOption {},

    #[error("Duplicate vote option: {option}")]
    DuplicateVoteOption { option: String },

    #[error("Too many vote entries: {count}; maximum is {max}")]
    TooManyVoteEntries { count: usize, max: usize },

    #[error("Voter has too many gauge vote records: {count}; maximum is {max}")]
    TooManyGaugeVotes { count: usize, max: usize },

    #[error("Power-change hook contains too many members: {count}; maximum is {max}")]
    TooManyHookMembers { count: usize, max: usize },

    #[error("NFT unstake hook contains too many token IDs: {count}; maximum is {max}")]
    TooManyNftHookTokens { count: usize, max: usize },

    #[error("Voting-power arithmetic overflow for voter {voter}")]
    VotingPowerOverflow { voter: String },

    #[error("Voting-power arithmetic underflow for voter {voter}")]
    VotingPowerUnderflow { voter: String },

    #[error("Tally arithmetic overflow for gauge {gauge_id}, option {option}")]
    TallyOverflow { gauge_id: u64, option: String },

    #[error("Tally arithmetic underflow for gauge {gauge_id}, option {option}")]
    TallyUnderflow { gauge_id: u64, option: String },

    #[error("Total-cast arithmetic overflow for gauge {gauge_id}")]
    TotalCastOverflow { gauge_id: u64 },

    #[error("Total-cast arithmetic underflow for gauge {gauge_id}")]
    TotalCastUnderflow { gauge_id: u64 },

    #[error("Unknown vote-hook reply ID {0}")]
    UnknownVoteHookReply(u64),

    #[error("Vote-hook reply ID space exhausted")]
    VoteHookReplyIdExhausted {},

    #[error("Gauge limit reached; maximum is {max}")]
    TooManyGauges { max: u64 },

    #[error("Gauge has too many options: {count}; maximum is {max}")]
    TooManyOptions { count: usize, max: usize },

    #[error("Gauge adapter option pagination did not advance")]
    AdapterPaginationStalled {},

    #[error("Too many vote-hook subscribers; maximum is {max}")]
    TooManyHooks { max: u32 },

    #[error("Adapter returned too many execution messages: {count}; maximum is {max}")]
    TooManyAdapterMessages { count: usize, max: usize },

    #[error("Snapshot adapter omitted emitted/retained value accounting")]
    MissingAdapterAccounting {},

    #[error("Snapshot adapter returned inconsistent emitted/retained value accounting")]
    InvalidAdapterAccounting {},

    #[error("{field} exceeds maximum byte length {max}")]
    StringTooLong { field: String, max: usize },

    #[error("User {0} has no voting power")]
    NoVotingPower(String),

    #[error(
        "Vote weight {weight} rounds to zero against voting power {voting_power} — \
         your voting power is too small to split this finely; vote for fewer options \
         or use larger per-option weights"
    )]
    VoteWeightRoundsToZero {
        weight: Decimal,
        voting_power: Uint128,
    },

    #[error("Option {option} already exists for gauge ID {gauge_id}")]
    OptionAlreadyExists { option: String, gauge_id: u64 },

    #[error("Option {option} has been judged as invalid by gauge adapter of gauge ID {gauge_id}")]
    OptionInvalidByAdapter { option: String, gauge_id: u64 },

    #[error("Option {option} has been judged as valid by gauge adapter of gauge ID {gauge_id} and cannot be removed")]
    OptionValidByAdapter { option: String, gauge_id: u64 },

    #[error("Option {option} does not exists for gauge ID {gauge_id}")]
    OptionDoesNotExists { option: String, gauge_id: u64 },

    #[error("Gauge ID {gauge_id} cannot execute because next_epoch is not yet reached: current {current_epoch}, next_epoch: {next_epoch}")]
    EpochNotReached {
        gauge_id: u64,
        current_epoch: u64,
        next_epoch: u64,
    },

    #[error("Reset epoch has not passed yet")]
    ResetEpochNotPassed {},

    #[error("Reset batch size must be between 1 and {max}; got {size}")]
    InvalidResetBatchSize { size: u32, max: u32 },

    #[error("Reset interval must be greater than zero")]
    InvalidResetInterval {},

    #[error("Reset schedule arithmetic overflowed")]
    ResetScheduleOverflow {},

    #[error("Gauge epoch schedule arithmetic overflowed")]
    EpochScheduleOverflow {},

    #[error("Gauge ID {0} cannot execute because it is stopped")]
    GaugeStopped(u64),

    #[error("Gauge ID {0} is currently resetting, please try again later")]
    GaugeResetting(u64),

    #[error("Trying to remove vote that does not exists")]
    CannotRemoveNonexistingVote {},

    #[error("Epoch size must be bigger then 60 seconds")]
    EpochSizeTooShort {},

    #[error("Minimum percent selected parameter needs to be smaller then 1.0")]
    MinPercentSelectedTooBig {},

    #[error("Maximum options selected parameter needs to be bigger then 0")]
    MaxOptionsSelectedTooSmall {},

    #[error("Maximum percentage available parameter needs to be smaller then 1.0")]
    MaxAvailablePercentTooBig {},

    #[error("Migration config contains {count} gauges; maximum is {max}")]
    TooManyGaugeMigrationConfigs { count: usize, max: usize },

    #[error("Migration config contains duplicate gauge ID {gauge_id}")]
    DuplicateGaugeMigrationConfig { gauge_id: u64 },

    #[error("Unsupported migration source version {version}")]
    UnsupportedMigrationSource { version: String },
}
