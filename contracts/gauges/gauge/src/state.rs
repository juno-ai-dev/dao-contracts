use cosmwasm_schema::cw_serde;
use cosmwasm_std::{Addr, Decimal, Deps, Env, Order, StdResult, Storage, Uint128};
use cw_hooks::Hooks;
use cw_storage_plus::{Bound, Index, IndexList, IndexedMap, Item, Map, MultiIndex};
use cw_utils::maybe_addr;

use crate::error::ContractError;
use crate::msg::{CleanupProgress, EpochOutcome, EpochResponse, EpochSnapshotPolicy, VoteInfo};

/// Type alias for u64 to make the map types a bit more self-explanatory
pub type GaugeId = u64;

pub const CONFIG: Item<Config> = Item::new("config");
pub const GAUGES: Map<GaugeId, Gauge> = Map::new("gauges");
const LAST_ID: Item<GaugeId> = Item::new("last_id");

/// Hooks fired on `PlaceVotes`. Owner-managed list; failures auto-unregister
/// (see the `reply` entry-point).
pub const VOTE_HOOKS: Hooks = Hooks::new("vote_hooks");
/// Stable association between a namespaced reply ID and the hook address it
/// targets. Replies remove this entry regardless of success or failure.
pub const VOTE_HOOK_REPLIES: Map<u64, Addr> = Map::new("vote_hook_replies");
pub const NEXT_VOTE_HOOK_REPLY_ID: Item<u64> = Item::new("next_vote_hook_reply_id");
/// Last option processed by an in-progress reset. The gauge's existing
/// `last == next` marker records that a reset is active; this cursor ensures
/// later batches never revisit options already rewritten to zero.
pub const RESET_CURSOR: Map<GaugeId, String> = Map::new("reset_cursor");

#[cw_serde]
pub enum PowerSource {
    Hook,
    EpochSnapshot { guardian: Addr },
}

pub const POWER_SOURCE: Item<PowerSource> = Item::new("power_source");
pub const SNAPSHOT_POLICIES: Map<GaugeId, EpochSnapshotPolicy> = Map::new("snapshot_policies");
pub const SNAPSHOT_POLICY_VERSIONS: Map<GaugeId, u64> = Map::new("snapshot_policy_versions");
pub const CURRENT_EPOCH: Map<GaugeId, u64> = Map::new("current_epoch");
pub const NEXT_EPOCH_ID: Map<GaugeId, u64> = Map::new("next_epoch_id");

#[cw_serde]
pub struct SnapshotEpoch {
    pub gauge_id: GaugeId,
    pub epoch_id: u64,
    pub snapshot_height: u64,
    pub snapshot_total_power: Uint128,
    pub participating_power: Uint128,
    pub total_cast: Uint128,
    pub retained_option: Option<String>,
    pub retained_option_power: Uint128,
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
    pub receipt_count: u32,
    pub option_count: u32,
    pub outcome: EpochOutcome,
    pub cleanup: CleanupProgress,
}

impl SnapshotEpoch {
    pub fn response(&self) -> EpochResponse {
        EpochResponse {
            gauge_id: self.gauge_id,
            epoch_id: self.epoch_id,
            snapshot_height: self.snapshot_height,
            snapshot_total_power: self.snapshot_total_power,
            participating_power: self.participating_power,
            allocated_power: self.total_cast,
            total_cast: self.total_cast,
            retained_option: self.retained_option.clone(),
            retained_option_power: self.retained_option_power,
            unallocated_power: self.participating_power.saturating_sub(self.total_cast),
            selected_project_power: self.selected_project_power,
            emitted_value: self.emitted_value,
            retained_value: self.retained_value,
            min_turnout_bps: self.min_turnout_bps,
            policy_version: self.policy_version,
            epoch_budget: self.epoch_budget,
            denom: self.denom.clone(),
            opens_at: self.opens_at,
            closes_at: self.closes_at,
            execution_deadline: self.execution_deadline,
            voter_count: self.voter_count,
            option_count: self.option_count,
            outcome: self.outcome.clone(),
            cleanup: self.cleanup.clone(),
        }
    }
}

#[cw_serde]
pub struct SnapshotBallot {
    pub voter: Addr,
    pub power: Uint128,
    pub votes: Vec<Vote>,
    pub cast_at: u64,
    pub revised_at: u64,
    pub revisions: u32,
    pub receipt_index: u32,
}

pub const EPOCHS: Map<(GaugeId, u64), SnapshotEpoch> = Map::new("snapshot_epochs");
pub const EPOCH_OPTIONS: Map<(GaugeId, u64, &str), ()> = Map::new("snapshot_epoch_options");
pub const EPOCH_OPTION_INDEX: Map<(GaugeId, u64, u32), String> =
    Map::new("snapshot_epoch_option_index");
pub const EPOCH_TALLY: Map<(GaugeId, u64, &str), u128> = Map::new("snapshot_epoch_tally");
pub const EPOCH_BALLOTS: Map<(GaugeId, u64, &Addr), SnapshotBallot> =
    Map::new("snapshot_epoch_ballots");
pub const EPOCH_BALLOT_INDEX: Map<(GaugeId, u64, u32), Addr> =
    Map::new("snapshot_epoch_ballot_index");
pub const EPOCH_BALLOT_SEEN: Map<(GaugeId, u64, &Addr), bool> =
    Map::new("snapshot_epoch_ballot_seen");
pub const EPOCH_BALLOT_POSITION: Map<(GaugeId, u64, &Addr), u32> =
    Map::new("snapshot_epoch_ballot_position");
pub const EPOCH_VOTER_POWER: Map<(GaugeId, u64, &Addr), Uint128> =
    Map::new("snapshot_epoch_voter_power");

/// Get ID for gauge registration and increment value in storage
pub fn fetch_last_id(storage: &mut dyn Storage) -> StdResult<u64> {
    let last_id = LAST_ID.load(storage).unwrap_or_default();
    LAST_ID.save(storage, &(last_id + 1u64))?;
    Ok(last_id)
}

/// This lets us find and update any vote given both voter and gauge.
/// It also lets us iterate over all votes by a given voter on all gauges
/// or by a given gauge id. This is needed when a voter weight changes
/// in order to update the guage.
pub fn votes() -> Votes<'static> {
    Votes::new("votes", "votes__gaugeid")
}

// settings for pagination
const MAX_LIMIT: u32 = 100;
const DEFAULT_LIMIT: u32 = 30;
pub const MAX_GAUGE_VOTES_PER_VOTER: usize = 100;

#[cw_serde]
pub struct Config {
    /// Address of contract to that contains all voting powers (where we query)
    pub voting_powers: Addr,
    /// Addres that will call voting power change hooks (often same as voting power contract)
    pub hook_caller: Addr,
    /// Address that can add new gauges or stop them
    pub owner: Addr,
    /// Address of DAO core module resposible for instantiation and execution of messages
    pub dao_core: Addr,
}

#[cw_serde]
pub struct Gauge {
    /// Descriptory label of gauge
    pub title: String,
    /// Address of contract to serve gauge-specific info (AdapterQueryMsg)
    pub adapter: Addr,
    /// Frequency (in seconds) the gauge executes messages, typically something like 7*86400
    pub epoch: u64,
    /// Minimum percentage of votes needed by a given option to be in the selected set
    pub min_percent_selected: Option<Decimal>,
    /// Maximum number of Options to make the selected set. Needed even with
    /// `min_percent_selected` to provide some guarantees on gas usage of this query.
    pub max_options_selected: u32,
    // Any votes above that percentage will be discarded
    pub max_available_percentage: Option<Decimal>,
    /// True if the gauge is stopped
    pub is_stopped: bool,
    /// UNIX time (seconds) when next epoch can be executed. If < env.block.time then Execute can be called
    pub next_epoch: u64,
    /// The last set of options selected by the gauge, `None` before the first execution
    pub last_executed_set: Option<Vec<(String, Uint128)>>,
    /// Set this in migration if the gauge should be periodically reset
    pub reset: Option<Reset>,
}

#[cw_serde]
pub struct Reset {
    /// until the first reset, this is None - needed for 0-cost migration from current state
    pub last: Option<u64>,
    /// seconds between reset
    pub reset_each: u64,
    /// next time we can reset
    pub next: u64,
}

impl Gauge {
    /// Returns `true` if the gauge is currently being reset
    pub fn is_resetting(&self) -> bool {
        self.reset
            .as_ref()
            .map(|r| r.last == Some(r.next))
            .unwrap_or_default()
    }
}

#[cw_serde]
pub struct WeightedVotes {
    /// The gauge these votes are for
    pub gauge_id: GaugeId,
    /// The voting power behind the vote.
    pub power: Uint128,
    /// the user's votes for this gauge
    pub votes: Vec<Vote>,
    /// Timestamp when vote was cast.
    /// Allow `None` for 0-cost migration from current data
    pub cast: Option<u64>,
}

impl WeightedVotes {
    /// Returns `true` if the vote is
    pub fn is_expired(&self, gauge: &Gauge) -> bool {
        // check if the vote is older than the last reset
        match &gauge.reset {
            Some(Reset {
                last: Some(expired),
                ..
            }) => {
                // votes with no timestamp are always considered too old once a reset happened
                // (they are legacy votes pre-first reset)
                self.cast.unwrap_or_default() < *expired
            }
            // everything is valid before the first reset (last = `None`) or if the gauge is not resettable
            _ => false,
        }
    }
}

impl Default for WeightedVotes {
    fn default() -> Self {
        WeightedVotes {
            gauge_id: 0,
            power: Uint128::zero(),
            votes: vec![],
            cast: None,
        }
    }
}

#[cw_serde]
pub struct Vote {
    /// Option voted for.
    pub option: String,
    /// The weight of the power given to this vote
    pub weight: Decimal,
}

struct VoteIndexes<'a> {
    // Last type param defines the pk deserialization type
    pub vote: MultiIndex<'a, GaugeId, WeightedVotes, (&'a Addr, GaugeId)>,
}

impl IndexList<WeightedVotes> for VoteIndexes<'_> {
    fn get_indexes(&'_ self) -> Box<dyn Iterator<Item = &'_ dyn Index<WeightedVotes>> + '_> {
        Box::new(std::iter::once(&self.vote as &dyn Index<WeightedVotes>))
    }
}

pub struct Votes<'a> {
    // Votes are indexed by `(addr, gauge_id, weight)` triplet
    votes: IndexedMap<'a, (&'a Addr, GaugeId), WeightedVotes, VoteIndexes<'a>>,
}

impl<'a> Votes<'a> {
    pub fn new(storage_key: &'a str, vote_subkey: &'a str) -> Self {
        let indexes = VoteIndexes {
            vote: MultiIndex::new(|_, vote| vote.gauge_id, storage_key, vote_subkey),
        };
        let votes = IndexedMap::new(storage_key, indexes);
        Self { votes }
    }

    pub fn save(
        &self,
        storage: &mut dyn Storage,
        voter: &'a Addr,
        gauge_id: GaugeId,
        vote: &WeightedVotes,
    ) -> StdResult<()> {
        self.votes.save(storage, (voter, gauge_id), vote)
    }

    pub fn set_votes(
        &self,
        storage: &mut dyn Storage,
        env: &Env,
        voter: &'a Addr,
        gauge_id: GaugeId,
        votes: Vec<Vote>,
        power: impl Into<Uint128>,
    ) -> StdResult<()> {
        let power = power.into();
        self.votes.save(
            storage,
            (voter, gauge_id),
            &WeightedVotes {
                gauge_id,
                power,
                votes,
                cast: Some(env.block.time.seconds()),
            },
        )
    }

    pub fn remove_votes(
        &self,
        storage: &mut dyn Storage,
        voter: &'a Addr,
        gauge_id: GaugeId,
    ) -> StdResult<()> {
        self.votes.remove(storage, (voter, gauge_id))
    }

    pub fn load(
        &self,
        storage: &dyn Storage,
        voter: &'a Addr,
        gauge_id: GaugeId,
    ) -> StdResult<WeightedVotes> {
        self.votes.load(storage, (voter, gauge_id))
    }

    pub fn may_load(
        &self,
        storage: &dyn Storage,
        voter: &'a Addr,
        gauge_id: GaugeId,
    ) -> StdResult<Option<WeightedVotes>> {
        self.votes.may_load(storage, (voter, gauge_id))
    }

    pub fn query_votes_by_voter(
        &self,
        deps: Deps,
        voter_addr: &'a Addr,
        start_after: Option<GaugeId>,
        limit: Option<u32>,
    ) -> StdResult<Vec<WeightedVotes>> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;
        let start = start_after.map(Bound::exclusive);

        self.votes
            .prefix(voter_addr)
            .range(deps.storage, start, None, Order::Ascending)
            .map(|index| {
                let (_, vote) = index?;
                Ok(vote)
            })
            .take(limit)
            .collect()
    }

    /// Loads the complete set a power-change hook must process, with one
    /// lookahead record to detect unsupported legacy state. This must be used
    /// by every state-changing power hook; the paginated public query is not
    /// safe for accounting updates.
    pub fn power_change_votes(
        &self,
        deps: Deps,
        voter_addr: &'a Addr,
    ) -> Result<Vec<WeightedVotes>, ContractError> {
        let records = self
            .votes
            .prefix(voter_addr)
            .range(deps.storage, None, None, Order::Ascending)
            .take(MAX_GAUGE_VOTES_PER_VOTER + 1)
            .map(|item| item.map(|(_, vote)| vote))
            .collect::<StdResult<Vec<_>>>()?;
        if records.len() > MAX_GAUGE_VOTES_PER_VOTER {
            return Err(ContractError::TooManyGaugeVotes {
                count: records.len(),
                max: MAX_GAUGE_VOTES_PER_VOTER,
            });
        }
        Ok(records)
    }

    pub fn query_votes_by_gauge(
        &self,
        deps: Deps,
        gauge_id: GaugeId,
        start_after: Option<String>,
        limit: Option<u32>,
    ) -> StdResult<Vec<VoteInfo>> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;
        let addr = maybe_addr(deps.api, start_after)?;
        let start = addr.as_ref().map(|a| Bound::exclusive((a, gauge_id)));

        let gauge = GAUGES.load(deps.storage, gauge_id)?;

        self.votes
            .idx
            .vote
            .prefix(gauge_id)
            .range(deps.storage, start, None, Order::Ascending)
            .take(limit)
            .filter(|r| match r {
                Ok((_, v)) => !v.is_expired(&gauge), // filter out expired votes
                Err(_) => true,                      // keep the error
            })
            .map(|r| {
                let ((voter, _gauge), votes) = r?;
                Ok(VoteInfo {
                    voter: voter.into_string(),
                    votes: votes.votes,
                    cast: votes.cast,
                })
            })
            // NIT: collect and into_iter is a bit inefficient... guess it was too complex/confusing otherwise, so fine
            .collect()
    }
}

/// Total amount of votes in all options, used to calculate min percentage.
pub const TOTAL_CAST: Map<GaugeId, u128> = Map::new("total_power");

/// Count how many points each option has per gauge
pub const TALLY: Map<(GaugeId, &str), u128> = Map::new("tally");
/// Removal tombstones retained while stored voter records still reference an
/// option. Tombstoned options are never selectable or open to new votes.
pub const INVALID_OPTIONS: Map<(GaugeId, &str), bool> = Map::new("invalid_options");
/// Sorted index of options by points, separated by gauge - data field is a placeholder
pub const OPTION_BY_POINTS: Map<(GaugeId, u128, &str), u8> = Map::new("tally_points");

/// Updates the tally for one option.
/// The first time a user votes, they get `{old_vote: 0, new_vote: power}`
/// If they change options, call old option with `{old_vote: power, new_vote: 0}` and new option with `{old_vote: 0, new_vote: power}`
/// If a user changes power (member update hook), call existing option with `{old_vote: old_power, new_vote: new_power}`
pub fn update_tally(
    storage: &mut dyn Storage,
    gauge: GaugeId,
    option: &str,
    old_vote: u128,
    new_vote: u128,
) -> Result<(), ContractError> {
    update_tallies(storage, gauge, vec![(option, old_vote, new_vote)])
}

/// Updates the tally for one option.
/// The first time a user votes, they get `{old_vote: 0, new_vote: power}`
/// If they change options, call old option with `{old_vote: power, new_vote: 0}` and new option with `{old_vote: 0, new_vote: power}`
/// If a user changes power (member update hook), call existing option with `{old_vote: old_power, new_vote: new_power}`
pub fn update_tallies(
    storage: &mut dyn Storage,
    gauge: GaugeId,
    // (option, old, new)
    updates: Vec<(&str, u128, u128)>,
) -> Result<(), ContractError> {
    let mut old_votes = 0u128;
    let mut new_votes = 0u128;

    for (option, old_vote, new_vote) in updates {
        old_votes = old_votes
            .checked_add(old_vote)
            .ok_or(ContractError::TotalCastOverflow { gauge_id: gauge })?;
        new_votes = new_votes
            .checked_add(new_vote)
            .ok_or(ContractError::TotalCastOverflow { gauge_id: gauge })?;

        // get old and new values
        let old_count = TALLY.may_load(storage, (gauge, option))?;
        let count = old_count
            .unwrap_or_default()
            .checked_add(new_vote)
            .ok_or_else(|| ContractError::TallyOverflow {
                gauge_id: gauge,
                option: option.to_string(),
            })?
            .checked_sub(old_vote)
            .ok_or_else(|| ContractError::TallyUnderflow {
                gauge_id: gauge,
                option: option.to_string(),
            })?;

        // delete old secondary index (if any)
        if let Some(old) = old_count {
            OPTION_BY_POINTS.remove(storage, (gauge, old, option));
        }

        let invalid = INVALID_OPTIONS.has(storage, (gauge, option));
        // A zero tally does not prove no stored vote references the option:
        // a zero-power voter may later stake again. Keep the tombstone until
        // reset performs bounded cleanup, otherwise that later power hook
        // would recreate the removed option as active and selectable.
        TALLY.save(storage, (gauge, option), &count)?;
        if !invalid {
            OPTION_BY_POINTS.save(storage, (gauge, count, option), &1u8)?;
        }
    }

    // update total count
    let total = TOTAL_CAST.may_load(storage, gauge)?.unwrap_or_default();
    let total = total
        .checked_add(new_votes)
        .ok_or(ContractError::TotalCastOverflow { gauge_id: gauge })?
        .checked_sub(old_votes)
        .ok_or(ContractError::TotalCastUnderflow { gauge_id: gauge })?;
    TOTAL_CAST.save(storage, gauge, &total)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmwasm_std::Order;
    use proptest::prelude::*;
    use std::collections::{BTreeMap, BTreeSet};

    use cosmwasm_std::testing::{mock_dependencies, mock_env};

    const GAUGE: u64 = 2;

    /// Let's keep them all the same length for less surprising iteration
    const OPTION1: &str = "one";
    const OPTION2: &str = "two";
    // make sure it is alphabetically last
    const OPTION3: &str = "zzz";

    // demonstate how to call update tally and how to query the tallies,
    // both by pk and by secondary index
    #[test]
    fn update_tally_initial_votes_work() {
        let mut mock_deps = mock_dependencies();
        let deps = mock_deps.as_mut();

        update_tally(deps.storage, GAUGE, OPTION1, 0, 250).unwrap();
        update_tally(deps.storage, GAUGE, OPTION2, 0, 400).unwrap();
        update_tally(deps.storage, GAUGE, OPTION3, 0, 100).unwrap();

        // data in some other tally shouldn't mix with this gauge
        update_tally(deps.storage, 17, OPTION3, 0, 55).unwrap();
        update_tally(deps.storage, 16, OPTION1, 0, 123).unwrap();

        // get all options with primary index (ordered by string value of option)
        let options: Vec<_> = TALLY
            .prefix(GAUGE)
            .range(deps.storage, None, None, Order::Ascending)
            .collect::<StdResult<Vec<_>>>()
            .unwrap();
        let expected = vec![
            (OPTION1.to_string(), 250u128),
            (OPTION2.to_string(), 400u128),
            (OPTION3.to_string(), 100u128),
        ];
        assert_eq!(options, expected);

        // get them by secondary index, top to bottom
        let options: Vec<_> = OPTION_BY_POINTS
            .sub_prefix(GAUGE)
            .keys(deps.storage, None, None, Order::Descending)
            .collect::<StdResult<Vec<_>>>()
            .unwrap();
        let expected = vec![
            (400u128, OPTION2.to_string()),
            (250u128, OPTION1.to_string()),
            (100u128, OPTION3.to_string()),
        ];
        assert_eq!(options, expected);

        // total is properly set
        let total = TOTAL_CAST.load(deps.storage, GAUGE).unwrap();
        assert_eq!(total, 750u128);
    }

    #[test]
    fn tally_arithmetic_returns_domain_errors_at_numeric_boundaries() {
        let mut deps = mock_dependencies();
        update_tally(&mut deps.storage, GAUGE, OPTION1, 0, u128::MAX).unwrap();
        assert_eq!(
            update_tally(&mut deps.storage, GAUGE, OPTION1, 0, 1).unwrap_err(),
            ContractError::TallyOverflow {
                gauge_id: GAUGE,
                option: OPTION1.to_owned()
            }
        );

        update_tally(&mut deps.storage, 77, OPTION2, 0, 0).unwrap();
        assert_eq!(
            update_tally(&mut deps.storage, 77, OPTION2, 1, 0).unwrap_err(),
            ContractError::TallyUnderflow {
                gauge_id: 77,
                option: OPTION2.to_owned()
            }
        );

        // A second option can fit its own tally but not the aggregate total.
        assert_eq!(
            update_tally(&mut deps.storage, GAUGE, OPTION2, 0, 1).unwrap_err(),
            ContractError::TotalCastOverflow { gauge_id: GAUGE }
        );
    }

    fn to_vote_info(voter: &Addr, votes: &[Vote], cast: impl Into<Option<u64>>) -> VoteInfo {
        VoteInfo {
            voter: voter.to_string(),
            votes: votes.to_vec(),
            cast: cast.into(),
        }
    }

    #[test]
    fn votes_works() {
        let mut deps = mock_dependencies();
        let env = mock_env();
        let votes = votes();

        // setup gauges
        for gauge_id in 1..=3 {
            GAUGES
                .save(
                    &mut deps.storage,
                    gauge_id,
                    &Gauge {
                        title: "test".to_string(),
                        adapter: Addr::unchecked("gauge_adapter"),
                        epoch: 100,
                        min_percent_selected: None,
                        max_options_selected: 10,
                        max_available_percentage: None,
                        is_stopped: false,
                        next_epoch: env.block.time.seconds(),
                        last_executed_set: None,
                        reset: None,
                    },
                )
                .unwrap();
        }

        let user1 = Addr::unchecked("user1");
        let votes1 = vec![Vote {
            option: "someoption".to_owned(),
            weight: Decimal::percent(100),
        }];
        let vote1 = WeightedVotes {
            gauge_id: 1,
            power: Uint128::new(3),
            votes: votes1.clone(),
            cast: Some(env.block.time.seconds()),
        };
        votes
            .votes
            .save(&mut deps.storage, (&user1, 1), &vote1)
            .unwrap();

        let user2 = Addr::unchecked("user2");
        let votes2 = vec![Vote {
            option: "otheroption".to_owned(),
            weight: Decimal::percent(50),
        }];
        let vote2 = WeightedVotes {
            gauge_id: 1,
            power: Uint128::new(6),
            votes: votes2.clone(),
            cast: Some(env.block.time.seconds()),
        };
        votes
            .votes
            .save(&mut deps.storage, (&user2, 1), &vote2)
            .unwrap();

        let user3 = Addr::unchecked("user3");
        let votes3 = vec![Vote {
            option: "otheroption".to_owned(),
            weight: Decimal::percent(70),
        }];
        let vote3 = WeightedVotes {
            gauge_id: 1,
            power: Uint128::new(9),
            votes: votes3.clone(),
            cast: Some(env.block.time.seconds()),
        };
        votes
            .votes
            .save(&mut deps.storage, (&user3, 1), &vote3)
            .unwrap();

        let votes4 = vec![Vote {
            option: "otheroption".to_owned(),
            weight: Decimal::percent(75),
        }];
        let vote4 = WeightedVotes {
            gauge_id: 2,
            power: Uint128::new(12),
            votes: votes4,
            cast: Some(env.block.time.seconds()),
        };
        votes
            .votes
            .save(&mut deps.storage, (&user1, 2), &vote4)
            .unwrap();

        let votes5 = vec![Vote {
            option: "otheroption".to_owned(),
            weight: Decimal::percent(100),
        }];
        let vote5 = WeightedVotes {
            gauge_id: 3,
            power: Uint128::new(15),
            votes: votes5,
            cast: Some(env.block.time.seconds()),
        };
        votes
            .votes
            .save(&mut deps.storage, (&user1, 3), &vote5)
            .unwrap();

        let vote_result = votes.votes.load(&deps.storage, (&user2, 1)).unwrap();
        assert_eq!(vote_result, vote2);

        let result = votes
            .query_votes_by_gauge(deps.as_ref(), 1, None, None)
            .unwrap();
        assert_eq!(
            result,
            vec![
                to_vote_info(&user1, &votes1, env.block.time.seconds()),
                to_vote_info(&user2, &votes2, env.block.time.seconds()),
                to_vote_info(&user3, &votes3, env.block.time.seconds()),
            ]
        );

        let result = votes
            .query_votes_by_voter(deps.as_ref(), &user1, None, None)
            .unwrap();
        assert_eq!(result, vec![vote1, vote4, vote5]);
    }

    #[test]
    fn query_votes_by_gauge_paginated() {
        let mut deps = mock_dependencies();
        let env = mock_env();
        let votes = votes();

        let gauge_id = 1;

        GAUGES
            .save(
                &mut deps.storage,
                gauge_id,
                &Gauge {
                    title: "test".to_string(),
                    adapter: Addr::unchecked("gauge_adapter"),
                    epoch: 100,
                    min_percent_selected: None,
                    max_options_selected: 10,
                    max_available_percentage: None,
                    is_stopped: false,
                    next_epoch: env.block.time.seconds(),
                    last_executed_set: None,
                    reset: None,
                },
            )
            .unwrap();

        let user1 = Addr::unchecked("user1");
        let votes1 = vec![Vote {
            option: "someoption".to_owned(),
            weight: Decimal::percent(100),
        }];
        let vote1 = WeightedVotes {
            gauge_id: 1,
            power: Uint128::new(3),
            votes: votes1.clone(),
            cast: Some(env.block.time.seconds()),
        };
        votes
            .votes
            .save(&mut deps.storage, (&user1, 1), &vote1)
            .unwrap();

        let user2 = Addr::unchecked("user2");
        let votes2 = vec![Vote {
            option: "otheroption".to_owned(),
            weight: Decimal::percent(50),
        }];
        let vote2 = WeightedVotes {
            gauge_id: 1,
            power: Uint128::new(6),
            votes: votes2.clone(),
            cast: Some(env.block.time.seconds()),
        };
        votes
            .votes
            .save(&mut deps.storage, (&user2, 1), &vote2)
            .unwrap();

        let user3 = Addr::unchecked("user3");
        let votes3 = vec![Vote {
            option: "otheroption".to_owned(),
            weight: Decimal::percent(70),
        }];
        let vote3 = WeightedVotes {
            gauge_id: 1,
            power: Uint128::new(9),
            votes: votes3.clone(),
            cast: Some(env.block.time.seconds()),
        };
        votes
            .votes
            .save(&mut deps.storage, (&user3, 1), &vote3)
            .unwrap();

        // limit to 2 results
        let result = votes
            .query_votes_by_gauge(deps.as_ref(), gauge_id, None, Some(2))
            .unwrap();
        assert_eq!(
            result,
            vec![
                to_vote_info(&user1, &votes1, env.block.time.seconds()),
                to_vote_info(&user2, &votes2, env.block.time.seconds())
            ]
        );

        // start from second user (start_after user1)
        let result = votes
            .query_votes_by_gauge(deps.as_ref(), gauge_id, Some(user1.to_string()), None)
            .unwrap();
        assert_eq!(
            result,
            vec![
                to_vote_info(&user2, &votes2, env.block.time.seconds()),
                to_vote_info(&user3, &votes3, env.block.time.seconds())
            ]
        );
    }

    #[test]
    fn query_votes_by_user_paginated() {
        let mut deps = mock_dependencies();
        let env = mock_env();
        let votes = votes();
        let user1 = Addr::unchecked("user1");

        let vote1 = WeightedVotes {
            gauge_id: 2,
            power: Uint128::new(3),
            votes: vec![Vote {
                option: "someoption".to_owned(),
                weight: Decimal::percent(100),
            }],
            cast: Some(env.block.time.seconds()),
        };
        votes
            .votes
            .save(&mut deps.storage, (&user1, 2), &vote1)
            .unwrap();

        let vote2 = WeightedVotes {
            gauge_id: 3,
            power: Uint128::new(6),
            votes: vec![Vote {
                option: "otheroption".to_owned(),
                weight: Decimal::percent(100),
            }],
            cast: Some(env.block.time.seconds()),
        };
        votes
            .votes
            .save(&mut deps.storage, (&user1, 3), &vote2)
            .unwrap();

        let vote3 = WeightedVotes {
            gauge_id: 4,
            power: Uint128::new(9),
            votes: vec![Vote {
                option: "otheroption".to_owned(),
                weight: Decimal::percent(100),
            }],
            cast: Some(env.block.time.seconds()),
        };
        votes
            .votes
            .save(&mut deps.storage, (&user1, 4), &vote3)
            .unwrap();

        // limit to 2 results
        let result = votes
            .query_votes_by_voter(deps.as_ref(), &user1, None, Some(2))
            .unwrap();
        assert_eq!(result, vec![vote1, vote2.clone()]);

        // start from second user (start_after gauge_id 2)
        let result = votes
            .query_votes_by_voter(deps.as_ref(), &user1, Some(2), None)
            .unwrap();
        assert_eq!(result, vec![vote2, vote3]);
    }

    #[test]
    fn power_change_votes_enforces_complete_iteration_boundary() {
        let mut deps = mock_dependencies();
        let voter = Addr::unchecked("boundary-voter");
        let store = votes();
        let vote = |gauge_id| WeightedVotes {
            gauge_id,
            power: Uint128::one(),
            votes: vec![Vote {
                option: "option".to_owned(),
                weight: Decimal::one(),
            }],
            cast: Some(mock_env().block.time.seconds()),
        };

        for gauge_id in 0..(MAX_GAUGE_VOTES_PER_VOTER - 1) as u64 {
            store
                .save(&mut deps.storage, &voter, gauge_id, &vote(gauge_id))
                .unwrap();
        }
        assert_eq!(
            store
                .power_change_votes(deps.as_ref(), &voter)
                .unwrap()
                .len(),
            MAX_GAUGE_VOTES_PER_VOTER - 1
        );

        let at = (MAX_GAUGE_VOTES_PER_VOTER - 1) as u64;
        store
            .save(&mut deps.storage, &voter, at, &vote(at))
            .unwrap();
        assert_eq!(
            store
                .power_change_votes(deps.as_ref(), &voter)
                .unwrap()
                .len(),
            MAX_GAUGE_VOTES_PER_VOTER
        );

        let over = MAX_GAUGE_VOTES_PER_VOTER as u64;
        store
            .save(&mut deps.storage, &voter, over, &vote(over))
            .unwrap();
        assert_eq!(
            store.power_change_votes(deps.as_ref(), &voter).unwrap_err(),
            ContractError::TooManyGaugeVotes {
                count: MAX_GAUGE_VOTES_PER_VOTER + 1,
                max: MAX_GAUGE_VOTES_PER_VOTER
            }
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        /// Randomized reference-model test for the core accounting state
        /// machine. Each operation replaces/clears a vote or changes its power,
        /// then independently reconstructs all contributions from stored voter
        /// records and audits both primary and secondary indices.
        #[test]
        fn randomized_vote_and_power_sequences_preserve_all_indices(
            operations in prop::collection::vec(
                (0usize..4, 0u8..5, 0usize..4, 0usize..4, 1u64..10_000, 1u64..100),
                1..200,
            )
        ) {
            const OPTIONS: [&str; 4] = ["alpha", "beta", "gamma", "delta"];
            let mut deps = mock_dependencies();
            let env = mock_env();
            let store = votes();
            let voters = (0..4)
                .map(|index| Addr::unchecked(format!("property-voter-{index}")))
                .collect::<Vec<_>>();

            for option in OPTIONS {
                update_tally(&mut deps.storage, GAUGE, option, 0, 0).unwrap();
            }

            for (voter_index, action, first, second, raw_power, split) in operations {
                let voter = &voters[voter_index];
                let old = store.may_load(&deps.storage, voter, GAUGE).unwrap();
                let old_power = old.as_ref().map(|vote| vote.power).unwrap_or_default();
                let old_votes = old.as_ref().map(|vote| vote.votes.clone()).unwrap_or_default();

                let (new_power, new_votes) = match action {
                    0 => (Uint128::zero(), vec![]),
                    1 => (
                        Uint128::new(raw_power as u128),
                        vec![Vote {
                            option: OPTIONS[first].to_owned(),
                            weight: Decimal::one(),
                        }],
                    ),
                    2 if first != second => (
                        Uint128::new(raw_power as u128),
                        vec![
                            Vote {
                                option: OPTIONS[first].to_owned(),
                                weight: Decimal::percent(split),
                            },
                            Vote {
                                option: OPTIONS[second].to_owned(),
                                weight: Decimal::percent(100 - split),
                            },
                        ],
                    ),
                    // Power-change operation keeps the current allocation.
                    3 if old.is_some() => (Uint128::new(raw_power as u128), old_votes.clone()),
                    _ => (
                        Uint128::new(raw_power as u128),
                        vec![Vote {
                            option: OPTIONS[first].to_owned(),
                            weight: Decimal::percent(split),
                        }],
                    ),
                };

                let contribution = |votes: &[Vote], power: Uint128, option: &str| {
                    votes
                        .iter()
                        .find(|vote| vote.option == option)
                        .map(|vote| (power * vote.weight).u128())
                        .unwrap_or_default()
                };
                let updates = OPTIONS
                    .iter()
                    .map(|option| {
                        (
                            *option,
                            contribution(&old_votes, old_power, option),
                            contribution(&new_votes, new_power, option),
                        )
                    })
                    .collect::<Vec<_>>();
                update_tallies(&mut deps.storage, GAUGE, updates).unwrap();

                if new_votes.is_empty() {
                    store.remove_votes(&mut deps.storage, voter, GAUGE).unwrap();
                } else {
                    store
                        .set_votes(
                            &mut deps.storage,
                            &env,
                            voter,
                            GAUGE,
                            new_votes,
                            new_power,
                        )
                        .unwrap();
                }

                let mut expected = OPTIONS
                    .iter()
                    .map(|option| ((*option).to_owned(), 0u128))
                    .collect::<BTreeMap<_, _>>();
                for item in store
                    .votes
                    .range(&deps.storage, None, None, Order::Ascending)
                {
                    let ((_voter, gauge), record) = item.unwrap();
                    if gauge == GAUGE {
                        for vote in record.votes {
                            *expected.get_mut(&vote.option).unwrap() +=
                                (record.power * vote.weight).u128();
                        }
                    }
                }

                let tallies = TALLY
                    .prefix(GAUGE)
                    .range(&deps.storage, None, None, Order::Ascending)
                    .collect::<StdResult<BTreeMap<_, _>>>()
                    .unwrap();
                prop_assert_eq!(&tallies, &expected);
                prop_assert_eq!(
                    TOTAL_CAST.load(&deps.storage, GAUGE).unwrap(),
                    expected.values().copied().sum::<u128>()
                );

                let index = OPTION_BY_POINTS
                    .sub_prefix(GAUGE)
                    .keys(&deps.storage, None, None, Order::Ascending)
                    .collect::<StdResult<Vec<_>>>()
                    .unwrap();
                let expected_index = expected
                    .iter()
                    .map(|(option, points)| (*points, option.clone()))
                    .collect::<BTreeSet<_>>();
                prop_assert_eq!(index.len(), OPTIONS.len());
                prop_assert_eq!(index.into_iter().collect::<BTreeSet<_>>(), expected_index);
            }
        }
    }
}
