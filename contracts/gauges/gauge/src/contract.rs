#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    ensure, to_json_binary, Addr, Binary, Decimal, Deps, DepsMut, Env, MessageInfo, Order,
    QueryRequest, Reply, Response, StdError, StdResult, Uint128, Uint256, WasmMsg, WasmQuery,
};
use cw2::{ensure_from_older_version, get_contract_version, set_contract_version};
use cw_storage_plus::Bound;
use cw_utils::nonpayable;
use dao_interface::{
    msg::ExecuteMsg as DaoExecuteMsg,
    voting::{Query as DaoQuery, TotalPowerAtHeightResponse, VotingPowerAtHeightResponse},
};

use crate::hooks::new_vote_hook_msgs;
use crate::msg::{
    AdapterQueryMsg, AllOptionsResponse, CheckOptionResponse, CleanupPhase, CleanupProgress,
    ConfigResponse, CreateGaugeReply, EpochAllocationsResponse, EpochBallotInfo,
    EpochBallotResponse, EpochOutcome, EpochSnapshotPolicy, ExecuteMsg, GaugeConfig, GaugeResponse,
    GetHooksResponse, InstantiateMsg, ListEpochBallotsResponse, ListEpochsResponse,
    ListGaugesResponse, ListOptionsResponse, ListVotesResponse, MigrateMsg, PowerSourceResponse,
    QueryMsg, SampleGaugeMsgsResponse, SelectedSetResponse,
};
use crate::state::{
    fetch_last_id, update_tally, votes, Config, Gauge, GaugeId, PowerSource, SnapshotBallot,
    SnapshotEpoch, CONFIG, CURRENT_EPOCH, EPOCHS, EPOCH_BALLOTS, EPOCH_BALLOT_INDEX,
    EPOCH_BALLOT_POSITION, EPOCH_BALLOT_SEEN, EPOCH_OPTIONS, EPOCH_OPTION_INDEX, EPOCH_TALLY,
    EPOCH_VOTER_POWER, GAUGES, INVALID_OPTIONS, MAX_GAUGE_VOTES_PER_VOTER, NEXT_EPOCH_ID,
    OPTION_BY_POINTS, POWER_SOURCE, RESET_CURSOR, SNAPSHOT_POLICIES, SNAPSHOT_POLICY_VERSIONS,
    TALLY, TOTAL_CAST, VOTE_HOOKS, VOTE_HOOK_REPLIES,
};
use crate::{error::ContractError, state::Reset};

// version info for migration info
const CONTRACT_NAME: &str = "crates.io:gauge";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");
const SUPPORTED_MIGRATION_SOURCES: &[&str] = &["2.4.2", "2.5.0"];
const MAX_GAUGES: u64 = 100;
const MAX_OPTIONS_PER_GAUGE: usize = 100;
const MAX_VOTE_HOOKS: u32 = 10;
const MAX_ADAPTER_MESSAGES: usize = 100;
const MAX_HOOK_MEMBERS: usize = 100;
const MAX_NFT_HOOK_TOKENS: usize = 100;
const MAX_TITLE_BYTES: usize = 128;
const MAX_OPTION_BYTES: usize = 128;
const MAX_ABORT_REASON_BYTES: usize = 2_048;

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    mut deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    let voting_powers = deps.api.addr_validate(&msg.voting_powers)?;
    let (hook_caller, power_source) = match &msg.epoch_snapshot {
        Some(snapshot) => {
            if !msg.hook_caller.is_empty() {
                return Err(ContractError::HooksDisabledInSnapshotMode {});
            }
            (
                env.contract.address.clone(),
                PowerSource::EpochSnapshot {
                    guardian: deps.api.addr_validate(&snapshot.guardian)?,
                },
            )
        }
        None => (deps.api.addr_validate(&msg.hook_caller)?, PowerSource::Hook),
    };
    let owner = deps.api.addr_validate(&msg.owner)?;
    let config = Config {
        voting_powers,
        hook_caller,
        owner,
        dao_core: info.sender,
    };
    CONFIG.save(deps.storage, &config)?;
    POWER_SOURCE.save(deps.storage, &power_source)?;

    for gauge in msg.gauges.unwrap_or_default() {
        execute::attach_gauge(deps.branch(), env.clone(), gauge)?;
    }

    Ok(Response::new()
        .add_attribute("action", "instantiate")
        .add_attribute("dao_core", &config.dao_core)
        .add_attribute("owner", &msg.owner)
        .add_attribute("voting_powers", &msg.voting_powers)
        .add_attribute(
            "power_source",
            match power_source {
                PowerSource::Hook => "hook",
                PowerSource::EpochSnapshot { .. } => "epoch_snapshot",
            },
        )
        .add_attribute("hook_caller", &msg.hook_caller))
}

#[cfg(test)]
mod health_query_tests {
    use super::*;
    use cosmwasm_std::testing::{mock_dependencies, mock_env};
    use cosmwasm_std::{
        from_json, to_json_binary, BankMsg, ContractResult, CosmosMsg, SystemResult, WasmQuery,
    };

    fn test_gauge(reset: Option<Reset>) -> Gauge {
        Gauge {
            title: "regression".to_owned(),
            adapter: Addr::unchecked("adapter"),
            epoch: 600,
            min_percent_selected: None,
            max_options_selected: 10,
            max_available_percentage: None,
            is_stopped: false,
            next_epoch: 1_000,
            last_executed_set: None,
            reset,
        }
    }

    fn test_config(hook_caller: &Addr) -> Config {
        Config {
            voting_powers: Addr::unchecked("voting-powers"),
            hook_caller: hook_caller.clone(),
            owner: Addr::unchecked("owner"),
            dao_core: Addr::unchecked("dao"),
        }
    }

    #[test]
    fn due_reset_rejects_same_deadline_and_delayed_replacement_without_corrupting_hooks() {
        use crate::state::{Vote, WeightedVotes};
        use cw4::MemberDiff;

        const GAUGE_ID: u64 = 7;
        const RESET_DEADLINE: u64 = 10_000;

        for now in [RESET_DEADLINE, RESET_DEADLINE + 500] {
            let mut deps = mock_dependencies();
            let mut env = mock_env();
            env.block.time = cosmwasm_std::Timestamp::from_seconds(now);
            let voter = Addr::unchecked("alice");
            let hook = Addr::unchecked("membership-hook");
            CONFIG
                .save(deps.as_mut().storage, &test_config(&hook))
                .unwrap();
            GAUGES
                .save(
                    deps.as_mut().storage,
                    GAUGE_ID,
                    &test_gauge(Some(Reset {
                        last: None,
                        reset_each: 100,
                        next: RESET_DEADLINE,
                    })),
                )
                .unwrap();
            for option in ["old", "replacement"] {
                update_tally(deps.as_mut().storage, GAUGE_ID, option, 0, 0).unwrap();
            }
            update_tally(deps.as_mut().storage, GAUGE_ID, "old", 0, 100).unwrap();
            votes()
                .save(
                    deps.as_mut().storage,
                    &voter,
                    GAUGE_ID,
                    &WeightedVotes {
                        gauge_id: GAUGE_ID,
                        power: Uint128::new(100),
                        votes: vec![Vote {
                            option: "old".to_owned(),
                            weight: Decimal::one(),
                        }],
                        cast: Some(RESET_DEADLINE - 1),
                    },
                )
                .unwrap();
            deps.querier.update_wasm(|_| {
                SystemResult::Ok(ContractResult::Ok(
                    to_json_binary(&VotingPowerAtHeightResponse {
                        power: Uint128::new(200),
                        height: 1,
                    })
                    .unwrap(),
                ))
            });

            assert_eq!(
                execute::place_votes(
                    deps.as_mut(),
                    env,
                    voter.clone(),
                    GAUGE_ID,
                    Some(vec![Vote {
                        option: "replacement".to_owned(),
                        weight: Decimal::one(),
                    }]),
                )
                .unwrap_err(),
                ContractError::GaugeResetting(GAUGE_ID),
                "now={now}"
            );
            assert_eq!(
                TALLY
                    .load(deps.as_ref().storage, (GAUGE_ID, "old"))
                    .unwrap(),
                100
            );
            assert_eq!(
                TALLY
                    .load(deps.as_ref().storage, (GAUGE_ID, "replacement"))
                    .unwrap(),
                0
            );

            execute::member_changed(
                deps.as_mut(),
                hook,
                vec![MemberDiff {
                    key: voter.to_string(),
                    old: Some(100),
                    new: Some(150),
                }],
            )
            .unwrap();
            assert_eq!(
                TALLY
                    .load(deps.as_ref().storage, (GAUGE_ID, "old"))
                    .unwrap(),
                150
            );
            assert_eq!(
                TALLY
                    .load(deps.as_ref().storage, (GAUGE_ID, "replacement"))
                    .unwrap(),
                0
            );
        }
    }

    #[test]
    fn removing_zero_tally_stays_removed_across_restake() {
        use crate::state::{Vote, WeightedVotes};
        use dao_hooks::stake::StakeChangedHookMsg;

        const GAUGE_ID: u64 = 7;
        let mut deps = mock_dependencies();
        let hook = Addr::unchecked("staking-hook");
        let voter = Addr::unchecked("alice");
        CONFIG
            .save(deps.as_mut().storage, &test_config(&hook))
            .unwrap();
        GAUGES
            .save(deps.as_mut().storage, GAUGE_ID, &test_gauge(None))
            .unwrap();
        update_tally(deps.as_mut().storage, GAUGE_ID, "removed", 0, 0).unwrap();
        votes()
            .save(
                deps.as_mut().storage,
                &voter,
                GAUGE_ID,
                &WeightedVotes {
                    gauge_id: GAUGE_ID,
                    power: Uint128::zero(),
                    votes: vec![Vote {
                        option: "removed".to_owned(),
                        weight: Decimal::one(),
                    }],
                    cast: Some(mock_env().block.time.seconds()),
                },
            )
            .unwrap();

        execute::remove_option(
            deps.as_mut(),
            Addr::unchecked("owner"),
            GAUGE_ID,
            "removed".to_owned(),
        )
        .unwrap();
        assert!(!TALLY.has(deps.as_ref().storage, (GAUGE_ID, "removed")));
        assert!(!INVALID_OPTIONS.has(deps.as_ref().storage, (GAUGE_ID, "removed")));

        execute::stake_changed(
            deps.as_mut(),
            MessageInfo {
                sender: hook,
                funds: vec![],
            },
            StakeChangedHookMsg::Stake {
                addr: voter,
                amount: Uint128::new(50),
            },
        )
        .unwrap();
        assert!(!TALLY.has(deps.as_ref().storage, (GAUGE_ID, "removed")));
        assert!(!INVALID_OPTIONS.has(deps.as_ref().storage, (GAUGE_ID, "removed")));
        assert!(!OPTION_BY_POINTS.has(deps.as_ref().storage, (GAUGE_ID, 50, "removed")));
    }

    #[test]
    fn non_resetting_gauge_reclaims_option_capacity_without_tombstones() {
        use crate::state::{Vote, WeightedVotes};
        use dao_hooks::stake::StakeChangedHookMsg;

        const GAUGE_ID: u64 = 7;
        let mut deps = mock_dependencies();
        let hook = Addr::unchecked("staking-hook");
        let voter = Addr::unchecked("alice");
        CONFIG
            .save(deps.as_mut().storage, &test_config(&hook))
            .unwrap();
        GAUGES
            .save(deps.as_mut().storage, GAUGE_ID, &test_gauge(None))
            .unwrap();
        for index in 0..MAX_OPTIONS_PER_GAUGE {
            update_tally(
                deps.as_mut().storage,
                GAUGE_ID,
                &format!("option-{index:03}"),
                0,
                0,
            )
            .unwrap();
        }
        let removed = "option-000";
        votes()
            .save(
                deps.as_mut().storage,
                &voter,
                GAUGE_ID,
                &WeightedVotes {
                    gauge_id: GAUGE_ID,
                    power: Uint128::zero(),
                    votes: vec![Vote {
                        option: removed.to_owned(),
                        weight: Decimal::one(),
                    }],
                    cast: Some(mock_env().block.time.seconds()),
                },
            )
            .unwrap();

        execute::remove_option(
            deps.as_mut(),
            Addr::unchecked("owner"),
            GAUGE_ID,
            removed.to_owned(),
        )
        .unwrap();
        deps.querier.update_wasm(|query| match query {
            WasmQuery::Smart { contract_addr, .. } if contract_addr == "adapter" => {
                SystemResult::Ok(ContractResult::Ok(
                    to_json_binary(&CheckOptionResponse { valid: true }).unwrap(),
                ))
            }
            WasmQuery::Smart { contract_addr, .. } if contract_addr == "voting-powers" => {
                SystemResult::Ok(ContractResult::Ok(
                    to_json_binary(&VotingPowerAtHeightResponse {
                        power: Uint128::one(),
                        height: 1,
                    })
                    .unwrap(),
                ))
            }
            _ => unreachable!(),
        });

        execute::add_option(
            deps.as_mut(),
            voter.clone(),
            GAUGE_ID,
            "replacement".to_owned(),
        )
        .unwrap();
        assert!(!TALLY.has(deps.as_ref().storage, (GAUGE_ID, removed)));
        assert!(!INVALID_OPTIONS.has(deps.as_ref().storage, (GAUGE_ID, removed)));
        assert!(OPTION_BY_POINTS.has(deps.as_ref().storage, (GAUGE_ID, 0, "replacement")));

        execute::stake_changed(
            deps.as_mut(),
            MessageInfo {
                sender: hook,
                funds: vec![],
            },
            StakeChangedHookMsg::Stake {
                addr: voter,
                amount: Uint128::new(50),
            },
        )
        .unwrap();
        assert!(!TALLY.has(deps.as_ref().storage, (GAUGE_ID, removed)));
        assert!(!INVALID_OPTIONS.has(deps.as_ref().storage, (GAUGE_ID, removed)));
        assert!(!OPTION_BY_POINTS.has(deps.as_ref().storage, (GAUGE_ID, 50, removed)));
        assert!(OPTION_BY_POINTS.has(deps.as_ref().storage, (GAUGE_ID, 0, "replacement")));
        assert_eq!(
            OPTION_BY_POINTS
                .sub_prefix(GAUGE_ID)
                .keys(deps.as_ref().storage, None, None, Order::Ascending)
                .count(),
            MAX_OPTIONS_PER_GAUGE
        );
    }

    #[test]
    fn removing_option_removes_its_points_from_total_cast() {
        const GAUGE_ID: u64 = 7;
        let mut deps = mock_dependencies();
        CONFIG
            .save(
                deps.as_mut().storage,
                &test_config(&Addr::unchecked("hook")),
            )
            .unwrap();
        update_tally(deps.as_mut().storage, GAUGE_ID, "removed", 0, 0).unwrap();
        update_tally(deps.as_mut().storage, GAUGE_ID, "removed", 0, 50).unwrap();

        execute::remove_option(
            deps.as_mut(),
            Addr::unchecked("owner"),
            GAUGE_ID,
            "removed".to_owned(),
        )
        .unwrap();

        assert_eq!(TOTAL_CAST.load(&deps.storage, GAUGE_ID).unwrap(), 0);
        assert!(!TALLY.has(&deps.storage, (GAUGE_ID, "removed")));
    }

    #[test]
    fn expired_votes_skip_checked_power_arithmetic_in_all_stake_hooks() {
        use crate::state::{Vote, WeightedVotes};
        use dao_hooks::{nft_stake::NftStakeChangedHookMsg, stake::StakeChangedHookMsg};

        let mut deps = mock_dependencies();
        let hook = Addr::unchecked("staking-hook");
        CONFIG
            .save(deps.as_mut().storage, &test_config(&hook))
            .unwrap();

        let cases = [
            (1u64, "fungible-stake", Uint128::MAX),
            (2, "fungible-unstake", Uint128::zero()),
            (3, "nft-stake", Uint128::MAX),
            (4, "nft-unstake", Uint128::zero()),
        ];
        for (gauge_id, voter, power) in cases {
            GAUGES
                .save(
                    deps.as_mut().storage,
                    gauge_id,
                    &test_gauge(Some(Reset {
                        last: Some(100),
                        reset_each: 100,
                        next: 200,
                    })),
                )
                .unwrap();
            votes()
                .save(
                    deps.as_mut().storage,
                    &Addr::unchecked(voter),
                    gauge_id,
                    &WeightedVotes {
                        gauge_id,
                        power,
                        votes: vec![Vote {
                            option: "stale".to_owned(),
                            weight: Decimal::one(),
                        }],
                        cast: Some(99),
                    },
                )
                .unwrap();
        }

        let info = MessageInfo {
            sender: hook,
            funds: vec![],
        };
        execute::stake_changed(
            deps.as_mut(),
            info.clone(),
            StakeChangedHookMsg::Stake {
                addr: Addr::unchecked("fungible-stake"),
                amount: Uint128::one(),
            },
        )
        .unwrap();
        execute::stake_changed(
            deps.as_mut(),
            info.clone(),
            StakeChangedHookMsg::Unstake {
                addr: Addr::unchecked("fungible-unstake"),
                amount: Uint128::one(),
            },
        )
        .unwrap();
        execute::nft_stake_changed(
            deps.as_mut(),
            info.clone(),
            NftStakeChangedHookMsg::Stake {
                addr: Addr::unchecked("nft-stake"),
                token_id: "nft".to_owned(),
            },
        )
        .unwrap();
        execute::nft_stake_changed(
            deps.as_mut(),
            info,
            NftStakeChangedHookMsg::Unstake {
                addr: Addr::unchecked("nft-unstake"),
                token_ids: vec!["nft".to_owned()],
            },
        )
        .unwrap();

        for (gauge_id, voter, power) in cases {
            assert_eq!(
                votes()
                    .load(deps.as_ref().storage, &Addr::unchecked(voter), gauge_id)
                    .unwrap()
                    .power,
                power
            );
        }
    }

    #[test]
    fn gauge_health_reports_and_then_clears_index_mismatch() {
        let mut deps = mock_dependencies();
        GAUGES
            .save(
                deps.as_mut().storage,
                7,
                &Gauge {
                    title: "health".to_owned(),
                    adapter: Addr::unchecked("adapter"),
                    epoch: 600,
                    min_percent_selected: None,
                    max_options_selected: 10,
                    max_available_percentage: None,
                    is_stopped: false,
                    next_epoch: 1_000,
                    last_executed_set: None,
                    reset: None,
                },
            )
            .unwrap();
        TALLY
            .save(deps.as_mut().storage, (7, "alpha"), &10)
            .unwrap();
        TOTAL_CAST.save(deps.as_mut().storage, 7, &10).unwrap();

        let broken = query::gauge_health(deps.as_ref(), 7).unwrap();
        assert!(!broken.consistent);
        assert_eq!(broken.mismatch_count, 1);
        assert_eq!(broken.first_mismatch.as_deref(), Some("alpha"));

        OPTION_BY_POINTS
            .save(deps.as_mut().storage, (7, 10, "alpha"), &1)
            .unwrap();
        let repaired = query::gauge_health(deps.as_ref(), 7).unwrap();
        assert!(repaired.consistent);
        assert_eq!(repaired.mismatch_count, 0);
        assert_eq!(repaired.first_mismatch, None);
    }

    #[test]
    fn invalid_leader_does_not_hide_valid_candidate_below_selection_cap() {
        let mut deps = mock_dependencies();
        GAUGES
            .save(
                deps.as_mut().storage,
                7,
                &Gauge {
                    title: "adapter-validity".to_owned(),
                    adapter: Addr::unchecked("adapter"),
                    epoch: 600,
                    min_percent_selected: None,
                    max_options_selected: 1,
                    max_available_percentage: None,
                    is_stopped: false,
                    next_epoch: 1_000,
                    last_executed_set: None,
                    reset: None,
                },
            )
            .unwrap();
        for (option, power) in [("rejected-leader", 100u128), ("valid-runner-up", 50)] {
            TALLY
                .save(deps.as_mut().storage, (7, option), &power)
                .unwrap();
            OPTION_BY_POINTS
                .save(deps.as_mut().storage, (7, power, option), &1)
                .unwrap();
        }
        TOTAL_CAST.save(deps.as_mut().storage, 7, &150).unwrap();
        deps.querier.update_wasm(|query| match query {
            WasmQuery::Smart { msg, .. } => {
                let AdapterQueryMsg::CheckOption { option } = from_json(msg).unwrap() else {
                    unreachable!()
                };
                SystemResult::Ok(ContractResult::Ok(
                    to_json_binary(&CheckOptionResponse {
                        valid: option == "valid-runner-up",
                    })
                    .unwrap(),
                ))
            }
            _ => unreachable!(),
        });

        assert_eq!(
            query::selected_set(deps.as_ref(), 7).unwrap().votes,
            vec![("valid-runner-up".to_owned(), Uint128::new(50))]
        );
    }

    #[test]
    fn reset_makes_bounded_progress_across_batch_boundaries() {
        const GAUGE_ID: u64 = 7;
        const BATCH_SIZE: u32 = 4;

        for option_count in [0usize, 1, 3, 4, 5, 9] {
            let mut deps = mock_dependencies();
            let mut env = mock_env();
            env.block.time = env.block.time.plus_seconds(1_000);
            let reset_deadline = env.block.time.seconds();

            GAUGES
                .save(
                    deps.as_mut().storage,
                    GAUGE_ID,
                    &Gauge {
                        title: "reset-boundary".to_owned(),
                        adapter: Addr::unchecked("adapter"),
                        epoch: 600,
                        min_percent_selected: None,
                        max_options_selected: 10,
                        max_available_percentage: None,
                        is_stopped: false,
                        next_epoch: reset_deadline + 600,
                        last_executed_set: None,
                        reset: Some(Reset {
                            last: None,
                            reset_each: 100,
                            next: reset_deadline,
                        }),
                    },
                )
                .unwrap();

            let mut total = 0u128;
            for index in 0..option_count {
                let option = format!("option-{index:03}");
                // Deliberately vary points so reset proves both primary and
                // sorted index updates rather than only rewriting equal keys.
                let points = index as u128 + 1;
                total += points;
                TALLY
                    .save(deps.as_mut().storage, (GAUGE_ID, &option), &points)
                    .unwrap();
                OPTION_BY_POINTS
                    .save(deps.as_mut().storage, (GAUGE_ID, points, &option), &1)
                    .unwrap();
            }
            TOTAL_CAST
                .save(deps.as_mut().storage, GAUGE_ID, &total)
                .unwrap();

            let expected_calls = option_count.div_ceil(BATCH_SIZE as usize).max(1);
            for call in 0..expected_calls {
                let response = execute::reset_gauge(
                    deps.as_mut(),
                    env.clone(),
                    Addr::unchecked("keeper"),
                    GAUGE_ID,
                    BATCH_SIZE,
                )
                .unwrap();
                let complete = response
                    .attributes
                    .iter()
                    .find(|attribute| attribute.key == "complete")
                    .unwrap()
                    .value
                    == "true";
                assert_eq!(complete, call + 1 == expected_calls, "count={option_count}");

                if !complete {
                    assert!(RESET_CURSOR.has(deps.as_ref().storage, GAUGE_ID));
                    assert!(GAUGES
                        .load(deps.as_ref().storage, GAUGE_ID)
                        .unwrap()
                        .is_resetting());
                }
            }

            let gauge = GAUGES.load(deps.as_ref().storage, GAUGE_ID).unwrap();
            assert!(!gauge.is_resetting(), "count={option_count}");
            assert_eq!(gauge.reset.unwrap().next, reset_deadline + 100);
            assert!(!RESET_CURSOR.has(deps.as_ref().storage, GAUGE_ID));
            assert_eq!(TOTAL_CAST.load(deps.as_ref().storage, GAUGE_ID).unwrap(), 0);

            let tallies = TALLY
                .prefix(GAUGE_ID)
                .range(deps.as_ref().storage, None, None, Order::Ascending)
                .collect::<StdResult<Vec<_>>>()
                .unwrap();
            assert_eq!(tallies.len(), option_count);
            assert!(tallies.iter().all(|(_, points)| *points == 0));

            let sorted = OPTION_BY_POINTS
                .sub_prefix(GAUGE_ID)
                .range(deps.as_ref().storage, None, None, Order::Ascending)
                .collect::<StdResult<Vec<_>>>()
                .unwrap();
            assert_eq!(sorted.len(), option_count);
            assert!(sorted.iter().all(|((points, _), _)| *points == 0));
        }
    }

    #[test]
    fn member_changes_update_tombstoned_option_without_restoring_its_index() {
        use crate::state::{votes, Vote, WeightedVotes};
        use cw4::MemberDiff;

        const GAUGE_ID: u64 = 7;
        let mut deps = mock_dependencies();
        let hook = Addr::unchecked("membership-hook");
        CONFIG
            .save(
                deps.as_mut().storage,
                &Config {
                    voting_powers: Addr::unchecked("voting-powers"),
                    hook_caller: hook.clone(),
                    owner: Addr::unchecked("owner"),
                    dao_core: Addr::unchecked("dao"),
                },
            )
            .unwrap();
        GAUGES
            .save(
                deps.as_mut().storage,
                GAUGE_ID,
                &Gauge {
                    title: "removed-option-hook".to_owned(),
                    adapter: Addr::unchecked("adapter"),
                    epoch: 600,
                    min_percent_selected: None,
                    max_options_selected: 10,
                    max_available_percentage: None,
                    is_stopped: false,
                    next_epoch: 1_000,
                    last_executed_set: None,
                    reset: None,
                },
            )
            .unwrap();

        let option = "removed";
        TALLY
            .save(deps.as_mut().storage, (GAUGE_ID, option), &300)
            .unwrap();
        TOTAL_CAST
            .save(deps.as_mut().storage, GAUGE_ID, &300)
            .unwrap();
        INVALID_OPTIONS
            .save(deps.as_mut().storage, (GAUGE_ID, option), &true)
            .unwrap();

        for (voter, power) in [("alice", 100u128), ("bob", 200u128)] {
            let voter = Addr::unchecked(voter);
            votes()
                .save(
                    deps.as_mut().storage,
                    &voter,
                    GAUGE_ID,
                    &WeightedVotes {
                        gauge_id: GAUGE_ID,
                        power: Uint128::new(power),
                        votes: vec![Vote {
                            option: option.to_owned(),
                            weight: Decimal::one(),
                        }],
                        cast: Some(mock_env().block.time.seconds()),
                    },
                )
                .unwrap();
        }

        let response = execute::member_changed(
            deps.as_mut(),
            hook,
            vec![
                MemberDiff {
                    key: "alice".to_owned(),
                    old: Some(100),
                    new: Some(150),
                },
                MemberDiff {
                    key: "bob".to_owned(),
                    old: Some(200),
                    new: None,
                },
            ],
        )
        .unwrap();

        assert!(response
            .attributes
            .iter()
            .any(|attribute| attribute.key == "updated_votes" && attribute.value == "2"));
        assert_eq!(
            TALLY
                .load(deps.as_ref().storage, (GAUGE_ID, option))
                .unwrap(),
            150
        );
        assert_eq!(
            TOTAL_CAST.load(deps.as_ref().storage, GAUGE_ID).unwrap(),
            150
        );
        assert!(INVALID_OPTIONS.has(deps.as_ref().storage, (GAUGE_ID, option)));
        assert!(!OPTION_BY_POINTS.has(deps.as_ref().storage, (GAUGE_ID, 150, option)));
        assert_eq!(
            votes()
                .load(deps.as_ref().storage, &Addr::unchecked("alice"), GAUGE_ID,)
                .unwrap()
                .power,
            Uint128::new(150)
        );
        assert_eq!(
            votes()
                .load(deps.as_ref().storage, &Addr::unchecked("bob"), GAUGE_ID,)
                .unwrap()
                .power,
            Uint128::zero()
        );
    }

    #[test]
    fn zero_power_then_restake_cannot_reactivate_tombstoned_option() {
        use crate::state::{votes, Vote, WeightedVotes};
        use cw4::MemberDiff;
        use dao_hooks::stake::StakeChangedHookMsg;

        const GAUGE_ID: u64 = 7;
        let mut deps = mock_dependencies();
        let hook = Addr::unchecked("staking-hook");
        CONFIG
            .save(
                deps.as_mut().storage,
                &Config {
                    voting_powers: Addr::unchecked("voting-powers"),
                    hook_caller: hook.clone(),
                    owner: Addr::unchecked("owner"),
                    dao_core: Addr::unchecked("dao"),
                },
            )
            .unwrap();
        GAUGES
            .save(
                deps.as_mut().storage,
                GAUGE_ID,
                &Gauge {
                    title: "removed-option-restake".to_owned(),
                    adapter: Addr::unchecked("adapter"),
                    epoch: 600,
                    min_percent_selected: None,
                    max_options_selected: 10,
                    max_available_percentage: None,
                    is_stopped: false,
                    next_epoch: 1_000,
                    last_executed_set: None,
                    reset: None,
                },
            )
            .unwrap();

        let voter = Addr::unchecked("alice");
        let option = "removed";
        TALLY
            .save(deps.as_mut().storage, (GAUGE_ID, option), &100)
            .unwrap();
        TOTAL_CAST
            .save(deps.as_mut().storage, GAUGE_ID, &100)
            .unwrap();
        INVALID_OPTIONS
            .save(deps.as_mut().storage, (GAUGE_ID, option), &true)
            .unwrap();
        votes()
            .save(
                deps.as_mut().storage,
                &voter,
                GAUGE_ID,
                &WeightedVotes {
                    gauge_id: GAUGE_ID,
                    power: Uint128::new(100),
                    votes: vec![Vote {
                        option: option.to_owned(),
                        weight: Decimal::one(),
                    }],
                    cast: Some(mock_env().block.time.seconds()),
                },
            )
            .unwrap();

        execute::member_changed(
            deps.as_mut(),
            hook.clone(),
            vec![MemberDiff {
                key: voter.to_string(),
                old: Some(100),
                new: None,
            }],
        )
        .unwrap();
        assert_eq!(
            TALLY
                .load(deps.as_ref().storage, (GAUGE_ID, option))
                .unwrap(),
            0
        );
        assert!(INVALID_OPTIONS.has(deps.as_ref().storage, (GAUGE_ID, option)));
        assert!(!OPTION_BY_POINTS.has(deps.as_ref().storage, (GAUGE_ID, 0, option)));

        execute::stake_changed(
            deps.as_mut(),
            MessageInfo {
                sender: hook,
                funds: vec![],
            },
            StakeChangedHookMsg::Stake {
                addr: voter,
                amount: Uint128::new(50),
            },
        )
        .unwrap();

        assert_eq!(
            TALLY
                .load(deps.as_ref().storage, (GAUGE_ID, option))
                .unwrap(),
            50
        );
        assert_eq!(
            TOTAL_CAST.load(deps.as_ref().storage, GAUGE_ID).unwrap(),
            50
        );
        assert!(INVALID_OPTIONS.has(deps.as_ref().storage, (GAUGE_ID, option)));
        assert!(!OPTION_BY_POINTS.has(deps.as_ref().storage, (GAUGE_ID, 50, option)));
    }

    #[test]
    fn power_hooks_reject_oversized_batches_before_mutating_state() {
        use cw4::MemberDiff;
        use dao_hooks::nft_stake::NftStakeChangedHookMsg;

        let mut deps = mock_dependencies();
        let hook = Addr::unchecked("hook-caller");
        CONFIG
            .save(
                deps.as_mut().storage,
                &Config {
                    voting_powers: Addr::unchecked("voting-powers"),
                    hook_caller: hook.clone(),
                    owner: Addr::unchecked("owner"),
                    dao_core: Addr::unchecked("dao"),
                },
            )
            .unwrap();

        for count in [MAX_HOOK_MEMBERS - 1, MAX_HOOK_MEMBERS] {
            let diffs = (0..count)
                .map(|index| MemberDiff {
                    key: format!("accepted-member-{count}-{index}"),
                    old: None,
                    new: Some(1),
                })
                .collect::<Vec<_>>();
            execute::member_changed(deps.as_mut(), hook.clone(), diffs).unwrap();
        }

        let diffs = (0..=MAX_HOOK_MEMBERS)
            .map(|index| MemberDiff {
                key: format!("member-{index}"),
                old: None,
                new: Some(1),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            execute::member_changed(deps.as_mut(), hook.clone(), diffs).unwrap_err(),
            ContractError::TooManyHookMembers {
                count: MAX_HOOK_MEMBERS + 1,
                max: MAX_HOOK_MEMBERS,
            }
        );

        for count in [MAX_NFT_HOOK_TOKENS - 1, MAX_NFT_HOOK_TOKENS] {
            let token_ids = (0..count)
                .map(|index| format!("accepted-nft-{count}-{index}"))
                .collect::<Vec<_>>();
            execute::nft_stake_changed(
                deps.as_mut(),
                MessageInfo {
                    sender: hook.clone(),
                    funds: vec![],
                },
                NftStakeChangedHookMsg::Unstake {
                    addr: Addr::unchecked("accepted-member"),
                    token_ids,
                },
            )
            .unwrap();
        }

        let token_ids = (0..=MAX_NFT_HOOK_TOKENS)
            .map(|index| format!("nft-{index}"))
            .collect::<Vec<_>>();
        assert_eq!(
            execute::nft_stake_changed(
                deps.as_mut(),
                MessageInfo {
                    sender: hook,
                    funds: vec![],
                },
                NftStakeChangedHookMsg::Unstake {
                    addr: Addr::unchecked("member"),
                    token_ids,
                },
            )
            .unwrap_err(),
            ContractError::TooManyNftHookTokens {
                count: MAX_NFT_HOOK_TOKENS + 1,
                max: MAX_NFT_HOOK_TOKENS,
            }
        );

        assert!(GAUGES
            .range(deps.as_ref().storage, None, None, Order::Ascending)
            .next()
            .is_none());
    }

    #[test]
    fn execution_rejects_overlong_adapter_message_list_without_advancing_epoch() {
        let mut deps = mock_dependencies();
        CONFIG
            .save(
                deps.as_mut().storage,
                &Config {
                    voting_powers: Addr::unchecked("voting"),
                    hook_caller: Addr::unchecked("hook"),
                    owner: Addr::unchecked("owner"),
                    dao_core: Addr::unchecked("core"),
                },
            )
            .unwrap();
        GAUGES
            .save(
                deps.as_mut().storage,
                7,
                &Gauge {
                    title: "bounded adapter".to_owned(),
                    adapter: Addr::unchecked("adapter"),
                    epoch: 600,
                    min_percent_selected: None,
                    max_options_selected: 10,
                    max_available_percentage: None,
                    is_stopped: false,
                    next_epoch: 0,
                    last_executed_set: None,
                    reset: None,
                },
            )
            .unwrap();
        TALLY
            .save(deps.as_mut().storage, (7, "alpha"), &100)
            .unwrap();
        OPTION_BY_POINTS
            .save(deps.as_mut().storage, (7, 100, "alpha"), &1)
            .unwrap();
        TOTAL_CAST.save(deps.as_mut().storage, 7, &100).unwrap();

        deps.querier.update_wasm(|query| match query {
            WasmQuery::Smart { msg, .. } => {
                let query: AdapterQueryMsg = from_json(msg).unwrap();
                let response = match query {
                    AdapterQueryMsg::CheckOption { .. } => {
                        to_json_binary(&CheckOptionResponse { valid: true }).unwrap()
                    }
                    AdapterQueryMsg::SampleGaugeMsgs { .. } => {
                        let message = CosmosMsg::Bank(BankMsg::Send {
                            to_address: "recipient".to_owned(),
                            amount: vec![],
                        });
                        to_json_binary(&SampleGaugeMsgsResponse {
                            execute: vec![message; MAX_ADAPTER_MESSAGES],
                            emitted_value: None,
                            retained_value: None,
                        })
                        .unwrap()
                    }
                    AdapterQueryMsg::AllOptions { .. } => unreachable!(),
                };
                SystemResult::Ok(ContractResult::Ok(response))
            }
            _ => unreachable!(),
        });
        let accepted =
            execute::execute(deps.as_mut(), mock_env(), Addr::unchecked("keeper"), 7).unwrap();
        assert!(accepted.attributes.iter().any(|attribute| {
            attribute.key == "message_count" && attribute.value == MAX_ADAPTER_MESSAGES.to_string()
        }));
        GAUGES
            .update(deps.as_mut().storage, 7, |gauge| -> StdResult<_> {
                let mut gauge = gauge.unwrap();
                gauge.next_epoch = 0;
                Ok(gauge)
            })
            .unwrap();

        deps.querier.update_wasm(|query| match query {
            WasmQuery::Smart { msg, .. } => {
                let query: AdapterQueryMsg = from_json(msg).unwrap();
                let response = match query {
                    AdapterQueryMsg::CheckOption { .. } => {
                        to_json_binary(&CheckOptionResponse { valid: true }).unwrap()
                    }
                    AdapterQueryMsg::SampleGaugeMsgs { .. } => {
                        let message = CosmosMsg::Bank(BankMsg::Send {
                            to_address: "recipient".to_owned(),
                            amount: vec![],
                        });
                        to_json_binary(&SampleGaugeMsgsResponse {
                            execute: vec![message; MAX_ADAPTER_MESSAGES + 1],
                            emitted_value: None,
                            retained_value: None,
                        })
                        .unwrap()
                    }
                    AdapterQueryMsg::AllOptions { .. } => unreachable!(),
                };
                SystemResult::Ok(ContractResult::Ok(response))
            }
            _ => unreachable!(),
        });

        let error =
            execute::execute(deps.as_mut(), mock_env(), Addr::unchecked("keeper"), 7).unwrap_err();
        assert_eq!(
            error,
            ContractError::TooManyAdapterMessages {
                count: MAX_ADAPTER_MESSAGES + 1,
                max: MAX_ADAPTER_MESSAGES,
            }
        );
        assert_eq!(GAUGES.load(deps.as_ref().storage, 7).unwrap().next_epoch, 0);
    }

    #[test]
    fn attachment_rejects_initial_epoch_overflow_before_allocating_state() {
        let mut deps = mock_dependencies();

        let error = execute::attach_gauge(
            deps.as_mut(),
            mock_env(),
            GaugeConfig {
                title: "overflow".to_owned(),
                adapter: "adapter".to_owned(),
                epoch_size: u64::MAX,
                min_percent_selected: None,
                max_options_selected: 1,
                max_available_percentage: None,
                reset_epoch: None,
                snapshot_policy: None,
            },
        )
        .unwrap_err();

        assert_eq!(error, ContractError::EpochScheduleOverflow {});
        assert!(GAUGES
            .range(deps.as_ref().storage, None, None, Order::Ascending)
            .next()
            .is_none());
        assert_eq!(fetch_last_id(deps.as_mut().storage).unwrap(), 0);
    }

    #[test]
    fn attachment_imports_all_options_when_adapter_clamps_page_size() {
        const ADAPTER_PAGE_CAP: usize = 30;
        const OPTION_COUNT: usize = 75;

        let mut deps = mock_dependencies();
        let adapter_options = (0..OPTION_COUNT)
            .map(|index| format!("option-{index:03}"))
            .collect::<Vec<_>>();
        deps.querier.update_wasm(move |query| match query {
            WasmQuery::Smart { msg, .. } => {
                let AdapterQueryMsg::AllOptions { start_after, limit } = from_json(msg).unwrap()
                else {
                    unreachable!()
                };
                let start = start_after
                    .and_then(|cursor| {
                        adapter_options
                            .iter()
                            .position(|option| option == &cursor)
                            .map(|index| index + 1)
                    })
                    .unwrap_or_default();
                let options = adapter_options
                    .iter()
                    .skip(start)
                    .take((limit.unwrap_or(30) as usize).min(ADAPTER_PAGE_CAP))
                    .cloned()
                    .collect();
                SystemResult::Ok(ContractResult::Ok(
                    to_json_binary(&AllOptionsResponse { options }).unwrap(),
                ))
            }
            _ => unreachable!(),
        });

        let (gauge_id, _) = execute::attach_gauge(
            deps.as_mut(),
            mock_env(),
            GaugeConfig {
                title: "paginated import".to_owned(),
                adapter: "adapter".to_owned(),
                epoch_size: 600,
                min_percent_selected: None,
                max_options_selected: 10,
                max_available_percentage: None,
                reset_epoch: None,
                snapshot_policy: None,
            },
        )
        .unwrap();

        let health = query::gauge_health(deps.as_ref(), gauge_id).unwrap();
        assert_eq!(health.option_count, OPTION_COUNT as u32);
        assert_eq!(health.indexed_option_count, OPTION_COUNT as u32);
        assert!(health.scan_complete);
        assert!(health.consistent);
    }

    #[test]
    fn attachment_rejects_adapter_that_repeats_a_nonempty_page() {
        let mut deps = mock_dependencies();
        let hidden_options = (0..=MAX_OPTIONS_PER_GAUGE)
            .map(|index| format!("option-{index:03}"))
            .collect::<Vec<_>>();
        deps.querier.update_wasm(move |query| match query {
            WasmQuery::Smart { .. } => SystemResult::Ok(ContractResult::Ok(
                to_json_binary(&AllOptionsResponse {
                    // This malformed adapter has 101 options but ignores the
                    // cursor and always repeats its first clamped page.
                    options: hidden_options.iter().take(30).cloned().collect(),
                })
                .unwrap(),
            )),
            _ => unreachable!(),
        });

        let error = execute::attach_gauge(
            deps.as_mut(),
            mock_env(),
            GaugeConfig {
                title: "stalled pagination".to_owned(),
                adapter: "adapter".to_owned(),
                epoch_size: 600,
                min_percent_selected: None,
                max_options_selected: 10,
                max_available_percentage: None,
                reset_epoch: None,
                snapshot_policy: None,
            },
        )
        .unwrap_err();

        assert_eq!(error, ContractError::AdapterPaginationStalled {});
        assert!(GAUGES
            .range(deps.as_ref().storage, None, None, Order::Ascending)
            .next()
            .is_none());
        assert_eq!(fetch_last_id(deps.as_mut().storage).unwrap(), 0);
    }

    #[test]
    fn attachment_accepts_one_hundred_options_and_rejects_lookahead_101_atomically() {
        let config = GaugeConfig {
            title: "bounded import".to_owned(),
            adapter: "adapter".to_owned(),
            epoch_size: 600,
            min_percent_selected: None,
            max_options_selected: 10,
            max_available_percentage: None,
            reset_epoch: None,
            snapshot_policy: None,
        };

        let mut exact = mock_dependencies();
        let exact_options = (0..MAX_OPTIONS_PER_GAUGE)
            .map(|index| format!("option-{index:03}"))
            .collect::<Vec<_>>();
        exact.querier.update_wasm(move |query| match query {
            WasmQuery::Smart { msg, .. } => {
                let AdapterQueryMsg::AllOptions { start_after, limit } = from_json(msg).unwrap()
                else {
                    unreachable!()
                };
                let start = start_after
                    .and_then(|cursor| {
                        exact_options
                            .iter()
                            .position(|option| option == &cursor)
                            .map(|index| index + 1)
                    })
                    .unwrap_or_default();
                let options = exact_options
                    .iter()
                    .skip(start)
                    .take(limit.unwrap_or(30) as usize)
                    .cloned()
                    .collect();
                SystemResult::Ok(ContractResult::Ok(
                    to_json_binary(&AllOptionsResponse { options }).unwrap(),
                ))
            }
            _ => unreachable!(),
        });
        let (gauge_id, _) =
            execute::attach_gauge(exact.as_mut(), mock_env(), config.clone()).unwrap();
        let health = query::gauge_health(exact.as_ref(), gauge_id).unwrap();
        assert_eq!(health.option_count, MAX_OPTIONS_PER_GAUGE as u32);
        assert_eq!(health.indexed_option_count, MAX_OPTIONS_PER_GAUGE as u32);
        assert!(health.scan_complete);
        assert!(health.consistent);

        let mut over = mock_dependencies();
        let over_options = (0..=MAX_OPTIONS_PER_GAUGE)
            .map(|index| format!("option-{index:03}"))
            .collect::<Vec<_>>();
        over.querier.update_wasm(move |query| match query {
            WasmQuery::Smart { msg, .. } => {
                let AdapterQueryMsg::AllOptions { start_after, limit } = from_json(msg).unwrap()
                else {
                    unreachable!()
                };
                let start = start_after
                    .and_then(|cursor| {
                        over_options
                            .iter()
                            .position(|option| option == &cursor)
                            .map(|index| index + 1)
                    })
                    .unwrap_or_default();
                let options = over_options
                    .iter()
                    .skip(start)
                    .take((limit.unwrap_or(30) as usize).min(30))
                    .cloned()
                    .collect();
                SystemResult::Ok(ContractResult::Ok(
                    to_json_binary(&AllOptionsResponse { options }).unwrap(),
                ))
            }
            _ => unreachable!(),
        });
        let error = execute::attach_gauge(over.as_mut(), mock_env(), config).unwrap_err();
        assert_eq!(
            error,
            ContractError::TooManyOptions {
                count: MAX_OPTIONS_PER_GAUGE + 1,
                max: MAX_OPTIONS_PER_GAUGE,
            }
        );
        assert!(GAUGES
            .range(over.as_ref().storage, None, None, Order::Ascending)
            .next()
            .is_none());
        assert_eq!(fetch_last_id(over.as_mut().storage).unwrap(), 0);
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    nonpayable(&info)?;
    match msg {
        ExecuteMsg::StakeChangeHook(msg) => {
            ensure_hook_mode(deps.storage)?;
            execute::stake_changed(deps, info, msg)
        }
        ExecuteMsg::NftStakeChangeHook(msg) => {
            ensure_hook_mode(deps.storage)?;
            execute::nft_stake_changed(deps, info, msg)
        }
        ExecuteMsg::MemberChangedHook(hook_msg) => {
            ensure_hook_mode(deps.storage)?;
            execute::member_changed(deps, info.sender, hook_msg.diffs)
        }
        ExecuteMsg::CreateGauge(options) => execute::create_gauge(deps, env, info.sender, options),
        ExecuteMsg::UpdateGauge {
            gauge_id,
            epoch_size,
            min_percent_selected,
            max_options_selected,
            max_available_percentage,
        } => execute::update_gauge(
            deps,
            info.sender,
            gauge_id,
            epoch_size,
            min_percent_selected,
            max_options_selected,
            max_available_percentage,
        ),
        ExecuteMsg::StopGauge { gauge } => execute::stop_gauge(deps, info.sender, gauge),
        ExecuteMsg::ResumeGauge { gauge } => execute::resume_gauge(deps, info.sender, gauge),
        ExecuteMsg::OpenEpoch { gauge } => execute::open_epoch(deps, env, info.sender, gauge),
        ExecuteMsg::ExpireEpoch { gauge } => execute::expire_epoch(deps, env, info.sender, gauge),
        ExecuteMsg::AbortEpoch { gauge, reason } => {
            execute::abort_epoch(deps, env, info.sender, gauge, reason)
        }
        ExecuteMsg::UpdateSnapshotPolicy { gauge, policy } => {
            execute::update_snapshot_policy(deps, info.sender, gauge, policy)
        }
        ExecuteMsg::CleanupEpoch {
            gauge,
            epoch,
            limit,
        } => execute::cleanup_epoch(deps, info.sender, gauge, epoch, limit),
        ExecuteMsg::ResetGauge { gauge, batch_size } => {
            execute::reset_gauge(deps, env, info.sender, gauge, batch_size)
        }
        ExecuteMsg::AddOption { gauge, option } => {
            execute::add_option(deps, info.sender, gauge, option)
        }
        ExecuteMsg::RemoveOption { gauge, option } => {
            execute::remove_option(deps, info.sender, gauge, option)
        }
        ExecuteMsg::PlaceVotes { gauge, votes } => {
            execute::place_votes(deps, env, info.sender, gauge, votes)
        }
        ExecuteMsg::Execute { gauge } => execute::execute(deps, env, info.sender, gauge),
        ExecuteMsg::AddHook { addr } => execute::add_hook(deps, info.sender, addr),
        ExecuteMsg::RemoveHook { addr } => execute::remove_hook(deps, info.sender, addr),
    }
}

fn ensure_hook_mode(storage: &dyn cosmwasm_std::Storage) -> Result<(), ContractError> {
    if matches!(
        load_power_source(storage)?,
        PowerSource::EpochSnapshot { .. }
    ) {
        return Err(ContractError::HooksDisabledInSnapshotMode {});
    }
    Ok(())
}

/// Deployments predating epoch snapshots do not have this item. Treating the
/// missing value as hook mode preserves their behavior both before and during
/// migration, while all new instantiations persist an explicit value.
fn load_power_source(storage: &dyn cosmwasm_std::Storage) -> StdResult<PowerSource> {
    Ok(POWER_SOURCE.may_load(storage)?.unwrap_or(PowerSource::Hook))
}

fn validate_snapshot_policy(policy: &EpochSnapshotPolicy) -> Result<(), ContractError> {
    if policy.min_turnout_bps > 10_000 {
        return Err(ContractError::InvalidTurnoutBps {});
    }
    if policy.epoch_budget.is_zero() || policy.denom.is_empty() || policy.denom.len() > 128 {
        return Err(ContractError::InvalidEpochBudget {});
    }
    if policy.execution_window_seconds == 0 {
        return Err(ContractError::InvalidExecutionWindow {});
    }
    if policy
        .retained_option
        .as_ref()
        .is_some_and(|option| option.is_empty() || option.len() > MAX_OPTION_BYTES)
    {
        return Err(ContractError::InvalidRetainedOption {});
    }
    Ok(())
}

mod execute {
    use cw4::MemberDiff;
    use dao_hooks::{nft_stake::NftStakeChangedHookMsg, stake::StakeChangedHookMsg};

    use super::*;
    use crate::state::{update_tallies, Reset, Vote};
    use std::collections::{BTreeMap, HashMap, HashSet};

    fn bounded_adapter_options(deps: Deps, adapter: &Addr) -> Result<Vec<String>, ContractError> {
        let mut options = Vec::with_capacity(MAX_OPTIONS_PER_GAUGE + 1);
        let mut seen = HashSet::with_capacity(MAX_OPTIONS_PER_GAUGE + 1);
        let mut start_after = None;

        loop {
            let remaining = MAX_OPTIONS_PER_GAUGE + 1 - options.len();
            let response: AllOptionsResponse = deps.querier.query_wasm_smart(
                adapter,
                &AdapterQueryMsg::AllOptions {
                    start_after: start_after.clone(),
                    limit: Some(remaining as u32),
                },
            )?;

            if response.options.is_empty() {
                return Ok(options);
            }

            let option_count_before_page = options.len();
            for option in response.options {
                start_after = Some(option.clone());
                if !seen.insert(option.clone()) {
                    continue;
                }
                options.push(option);

                if options.len() > MAX_OPTIONS_PER_GAUGE {
                    return Err(ContractError::TooManyOptions {
                        count: MAX_OPTIONS_PER_GAUGE + 1,
                        max: MAX_OPTIONS_PER_GAUGE,
                    });
                }
            }

            // A non-empty page must advance the unique option set. Otherwise
            // the adapter may be ignoring the cursor or cycling, and treating
            // the partial set as complete could silently omit valid options or
            // hide that the adapter exceeds the hard option bound.
            if options.len() == option_count_before_page {
                return Err(ContractError::AdapterPaginationStalled {});
            }
        }
    }

    pub fn member_changed(
        deps: DepsMut,
        sender: Addr,
        diffs: Vec<MemberDiff>,
    ) -> Result<Response, ContractError> {
        // make sure only hook caller contract can activate this endpoint
        if sender != CONFIG.load(deps.storage)?.hook_caller {
            return Err(ContractError::Unauthorized {});
        }
        if diffs.len() > MAX_HOOK_MEMBERS {
            return Err(ContractError::TooManyHookMembers {
                count: diffs.len(),
                max: MAX_HOOK_MEMBERS,
            });
        }

        let member_count = diffs.len();
        let mut updated_votes = 0usize;
        let mut response = Response::new()
            .add_attribute("action", "member_changed_hook")
            .add_attribute("hook_caller", &sender)
            .add_attribute("member_count", member_count.to_string());
        let mut gauges = HashMap::new();

        for diff in diffs {
            response = response.add_attribute("member", &diff.key);
            let voter = deps.api.addr_validate(&diff.key)?;

            // for each gauge this user voted on,
            // update the tallies and update the users vote power
            for mut vote in votes().power_change_votes(deps.as_ref(), &voter)? {
                // find change of vote powers
                let old = Uint128::new(diff.old.unwrap_or_default().into());
                let new = Uint128::new(diff.new.unwrap_or_default().into());

                // Load gauge if not already cached for this batch. We cannot use
                // `or_insert_with(|| GAUGES.load(...).unwrap())` here because the
                // staking-hook caller (x/cw-hooks) treats any panic as a failure
                // and counts it toward the auto-unregister threshold; propagate
                // the error instead.
                if let std::collections::hash_map::Entry::Vacant(e) = gauges.entry(vote.gauge_id) {
                    e.insert(GAUGES.load(deps.storage, vote.gauge_id)?);
                }
                let gauge = &gauges[&vote.gauge_id];

                if vote.is_expired(gauge) {
                    continue;
                }

                // calculate updates and adjust tallies
                let updates: Vec<_> = vote
                    .votes
                    .iter()
                    .filter(|v| TALLY.has(deps.storage, (vote.gauge_id, v.option.as_str())))
                    .map(|v| {
                        (
                            v.option.as_str(),
                            (old * v.weight).u128(),
                            (new * v.weight).u128(),
                        )
                    })
                    .collect();
                update_tallies(deps.storage, vote.gauge_id, updates)?;

                // store new vote power for this user
                vote.power = new;
                votes().save(deps.storage, &voter, vote.gauge_id, &vote)?;
                updated_votes += 1;
            }
        }

        Ok(response.add_attribute("updated_votes", updated_votes.to_string()))
    }

    pub fn stake_changed(
        deps: DepsMut,
        info: MessageInfo,
        msg: StakeChangedHookMsg,
    ) -> Result<Response, ContractError> {
        // make sure only hook caller contract can activate this endpoint
        if info.sender != CONFIG.load(deps.storage)?.hook_caller {
            return Err(ContractError::Unauthorized {});
        }

        match msg {
            StakeChangedHookMsg::Stake { addr, amount } => {
                let mut updated_votes = 0usize;
                // for each gauge this user voted on,
                // update the tallies and update the users vote power
                for mut vote in votes().power_change_votes(deps.as_ref(), &addr)? {
                    let gauge = GAUGES.load(deps.storage, vote.gauge_id)?;

                    if vote.is_expired(&gauge) {
                        continue;
                    }

                    let old = vote.power;

                    // Voting power increases with staking amount
                    let new = vote.power.checked_add(amount).map_err(|_| {
                        ContractError::VotingPowerOverflow {
                            voter: addr.to_string(),
                        }
                    })?;

                    // calculate updates and adjust tallies
                    let updates: Vec<_> = vote
                        .votes
                        .iter()
                        .filter(|v| TALLY.has(deps.storage, (vote.gauge_id, v.option.as_str())))
                        .map(|v| {
                            (
                                v.option.as_str(),
                                (old * v.weight).u128(),
                                (new * v.weight).u128(),
                            )
                        })
                        .collect();
                    update_tallies(deps.storage, vote.gauge_id, updates)?;

                    // Update and store new vote power for this user
                    vote.power = new;
                    votes().save(deps.storage, &addr, vote.gauge_id, &vote)?;
                    updated_votes += 1;
                }

                Ok(Response::new()
                    .add_attribute("action", "stake_change_hook")
                    .add_attribute("hook_caller", &info.sender)
                    .add_attribute("kind", "stake")
                    .add_attribute("voter", &addr)
                    .add_attribute("amount", amount)
                    .add_attribute("updated_votes", updated_votes.to_string()))
            }
            StakeChangedHookMsg::Unstake { addr, amount } => {
                let mut updated_votes = 0usize;
                // for each gauge this user voted on,
                // update the tallies and update the users vote power
                for mut vote in votes().power_change_votes(deps.as_ref(), &addr)? {
                    let gauge = GAUGES.load(deps.storage, vote.gauge_id)?;

                    if vote.is_expired(&gauge) {
                        continue;
                    }

                    let old = vote.power;

                    // Decrease voting power by unstaked amount
                    let new = vote.power.checked_sub(amount).map_err(|_| {
                        ContractError::VotingPowerUnderflow {
                            voter: addr.to_string(),
                        }
                    })?;

                    // calculate updates and adjust tallies
                    let updates: Vec<_> = vote
                        .votes
                        .iter()
                        .filter(|v| TALLY.has(deps.storage, (vote.gauge_id, v.option.as_str())))
                        .map(|v| {
                            (
                                v.option.as_str(),
                                (old * v.weight).u128(),
                                (new * v.weight).u128(),
                            )
                        })
                        .collect();
                    update_tallies(deps.storage, vote.gauge_id, updates)?;

                    // Update and store new vote power for this user
                    vote.power = new;
                    votes().save(deps.storage, &addr, vote.gauge_id, &vote)?;
                    updated_votes += 1;
                }

                Ok(Response::new()
                    .add_attribute("action", "stake_change_hook")
                    .add_attribute("hook_caller", &info.sender)
                    .add_attribute("kind", "unstake")
                    .add_attribute("voter", &addr)
                    .add_attribute("amount", amount)
                    .add_attribute("updated_votes", updated_votes.to_string()))
            }
        }
    }

    pub fn nft_stake_changed(
        deps: DepsMut,
        info: MessageInfo,
        msg: NftStakeChangedHookMsg,
    ) -> Result<Response, ContractError> {
        // make sure only hook caller contract can activate this endpoint
        if info.sender != CONFIG.load(deps.storage)?.hook_caller {
            return Err(ContractError::Unauthorized {});
        }

        match msg {
            NftStakeChangedHookMsg::Stake { addr, token_id } => {
                let mut updated_votes = 0usize;
                // for each gauge this user voted on,
                // update the tallies and update the users vote power
                for mut vote in votes().power_change_votes(deps.as_ref(), &addr)? {
                    let gauge = GAUGES.load(deps.storage, vote.gauge_id)?;

                    if vote.is_expired(&gauge) {
                        continue;
                    }

                    let old = vote.power;
                    // Voting power increases by one (only one token_id staked at a time)
                    let new = vote.power.checked_add(Uint128::one()).map_err(|_| {
                        ContractError::VotingPowerOverflow {
                            voter: addr.to_string(),
                        }
                    })?;

                    // calculate updates and adjust tallies
                    let updates: Vec<_> = vote
                        .votes
                        .iter()
                        .filter(|v| TALLY.has(deps.storage, (vote.gauge_id, v.option.as_str())))
                        .map(|v| {
                            (
                                v.option.as_str(),
                                (old * v.weight).u128(),
                                (new * v.weight).u128(),
                            )
                        })
                        .collect();
                    update_tallies(deps.storage, vote.gauge_id, updates)?;

                    // Update and store new vote power for this user
                    vote.power = new;
                    votes().save(deps.storage, &addr, vote.gauge_id, &vote)?;
                    updated_votes += 1;
                }

                Ok(Response::new()
                    .add_attribute("action", "nft_stake_change_hook")
                    .add_attribute("hook_caller", &info.sender)
                    .add_attribute("kind", "stake")
                    .add_attribute("voter", &addr)
                    .add_attribute("token_id", token_id)
                    .add_attribute("token_count", "1")
                    .add_attribute("updated_votes", updated_votes.to_string()))
            }
            NftStakeChangedHookMsg::Unstake { addr, token_ids } => {
                if token_ids.len() > MAX_NFT_HOOK_TOKENS {
                    return Err(ContractError::TooManyNftHookTokens {
                        count: token_ids.len(),
                        max: MAX_NFT_HOOK_TOKENS,
                    });
                }
                let token_count = token_ids.len();
                let mut updated_votes = 0usize;
                // for each gauge this user voted on,
                // update the tallies and update the users vote power
                for mut vote in votes().power_change_votes(deps.as_ref(), &addr)? {
                    let gauge = GAUGES.load(deps.storage, vote.gauge_id)?;

                    if vote.is_expired(&gauge) {
                        continue;
                    }

                    let old = vote.power;

                    // Decrease voting power by number of token_ids.
                    // `usize` is always representable in u128.
                    let amount = token_ids.len() as u128;
                    let new = vote.power.checked_sub(Uint128::new(amount)).map_err(|_| {
                        ContractError::VotingPowerUnderflow {
                            voter: addr.to_string(),
                        }
                    })?;

                    // calculate updates and adjust tallies
                    let updates: Vec<_> = vote
                        .votes
                        .iter()
                        .filter(|v| TALLY.has(deps.storage, (vote.gauge_id, v.option.as_str())))
                        .map(|v| {
                            (
                                v.option.as_str(),
                                (old * v.weight).u128(),
                                (new * v.weight).u128(),
                            )
                        })
                        .collect();
                    update_tallies(deps.storage, vote.gauge_id, updates)?;

                    // Update and store new vote power for this user
                    vote.power = new;
                    votes().save(deps.storage, &addr, vote.gauge_id, &vote)?;
                    updated_votes += 1;
                }

                Ok(Response::new()
                    .add_attribute("action", "nft_stake_change_hook")
                    .add_attribute("hook_caller", &info.sender)
                    .add_attribute("kind", "unstake")
                    .add_attribute("voter", &addr)
                    .add_attribute("token_count", token_count.to_string())
                    .add_attribute("updated_votes", updated_votes.to_string()))
            }
        }
    }

    pub fn create_gauge(
        deps: DepsMut,
        env: Env,
        sender: Addr,
        options: GaugeConfig,
    ) -> Result<Response, ContractError> {
        let config = CONFIG.load(deps.storage)?;
        if sender != config.owner {
            return Err(ContractError::Unauthorized {});
        }

        let (gauge_id, adapter) = attach_gauge(deps, env, options)?;

        Ok(Response::new()
            .add_attribute("action", "create_gauge")
            .add_attribute("sender", &sender)
            .add_attribute("adapter", adapter)
            .add_attribute("gauge_id", gauge_id.to_string())
            .set_data(to_json_binary(&CreateGaugeReply { id: gauge_id })?))
    }

    pub fn attach_gauge(
        mut deps: DepsMut,
        env: Env,
        GaugeConfig {
            title,
            adapter,
            epoch_size,
            min_percent_selected,
            max_options_selected,
            max_available_percentage,
            reset_epoch,
            snapshot_policy,
        }: GaugeConfig,
    ) -> Result<(GaugeId, Addr), ContractError> {
        let adapter = deps.api.addr_validate(&adapter)?;
        if title.len() > MAX_TITLE_BYTES {
            return Err(ContractError::StringTooLong {
                field: "title".to_owned(),
                max: MAX_TITLE_BYTES,
            });
        }
        // gauge parameter validation
        ensure!(epoch_size > 60u64, ContractError::EpochSizeTooShort {});
        if reset_epoch == Some(0) {
            return Err(ContractError::InvalidResetInterval {});
        }
        if let Some(min_percent_selected) = min_percent_selected {
            ensure!(
                min_percent_selected < Decimal::one(),
                ContractError::MinPercentSelectedTooBig {}
            );
        }
        ensure!(
            max_options_selected > 0,
            ContractError::MaxOptionsSelectedTooSmall {}
        );
        if let Some(max_available_percentage) = max_available_percentage {
            // update_gauge already enforces this; mirror the check here so the
            // invariant cannot be bypassed via create_gauge.
            ensure!(
                max_available_percentage < Decimal::one(),
                ContractError::MaxAvailablePercentTooBig {}
            );
        }
        match load_power_source(deps.storage)? {
            PowerSource::Hook => {
                if snapshot_policy.is_some() {
                    return Err(ContractError::InvalidSnapshotPolicy {});
                }
            }
            PowerSource::EpochSnapshot { .. } => {
                let policy = snapshot_policy
                    .as_ref()
                    .ok_or(ContractError::InvalidSnapshotPolicy {})?;
                validate_snapshot_policy(policy)?;
                if reset_epoch.is_some() {
                    return Err(ContractError::InvalidSnapshotPolicy {});
                }
            }
        }
        let initial_next_epoch = match load_power_source(deps.storage)? {
            PowerSource::Hook => env
                .block
                .time
                .seconds()
                .checked_add(epoch_size)
                .ok_or(ContractError::EpochScheduleOverflow {})?,
            PowerSource::EpochSnapshot { .. } => env.block.time.seconds(),
        };
        let gauge = Gauge {
            title,
            adapter: adapter.clone(),
            epoch: epoch_size,
            min_percent_selected,
            max_options_selected,
            max_available_percentage,
            is_stopped: false,
            next_epoch: initial_next_epoch,
            last_executed_set: None,
            reset: reset_epoch
                .map(|r| {
                    Ok::<Reset, ContractError>(Reset {
                        last: None,
                        reset_each: r,
                        next: env
                            .block
                            .time
                            .seconds()
                            .checked_add(r)
                            .ok_or(ContractError::ResetScheduleOverflow {})?,
                    })
                })
                .transpose()?,
        };
        // Fetch adapter options and bulk-register them. The adapter is the
        // source of truth for what options exist; no per-option validation
        // or voting-power check applies here. Complete all external reads and
        // validation before allocating an ID or writing partial gauge state.
        let adapter_options = bounded_adapter_options(deps.as_ref(), &adapter)?;

        let last_id: GaugeId = fetch_last_id(deps.storage)?;
        if last_id >= MAX_GAUGES {
            return Err(ContractError::TooManyGauges { max: MAX_GAUGES });
        }
        GAUGES.save(deps.storage, last_id, &gauge)?;
        if let Some(policy) = snapshot_policy {
            SNAPSHOT_POLICIES.save(deps.storage, last_id, &policy)?;
            SNAPSHOT_POLICY_VERSIONS.save(deps.storage, last_id, &1)?;
            NEXT_EPOCH_ID.save(deps.storage, last_id, &1)?;
        }
        execute::add_adapter_options(deps.branch(), last_id, adapter_options)?;

        Ok((last_id, adapter))
    }

    pub fn update_gauge(
        deps: DepsMut,
        sender: Addr,
        gauge_id: u64,
        epoch_size: Option<u64>,
        min_percent_selected: Option<Decimal>,
        max_options_selected: Option<u32>,
        max_available_percentage: Option<Decimal>,
    ) -> Result<Response, ContractError> {
        let config = CONFIG.load(deps.storage)?;
        if sender != config.owner {
            return Err(ContractError::Unauthorized {});
        }

        // Snapshot selection parameters are epoch policy. They may change
        // between epochs, but never after ballots have begun for the current
        // one. Hook mode retains its existing mutable behavior.
        if matches!(
            load_power_source(deps.storage)?,
            PowerSource::EpochSnapshot { .. }
        ) {
            if let Some(epoch_id) = CURRENT_EPOCH.may_load(deps.storage, gauge_id)? {
                if EPOCHS
                    .may_load(deps.storage, (gauge_id, epoch_id))?
                    .map(|epoch| epoch.outcome == EpochOutcome::Open)
                    .unwrap_or(false)
                {
                    return Err(ContractError::SnapshotGaugeConfigLocked { gauge_id });
                }
            }
        }

        let mut gauge = GAUGES.load(deps.storage, gauge_id)?;
        if let Some(epoch_size) = epoch_size {
            ensure!(epoch_size > 60u64, ContractError::EpochSizeTooShort {});
            gauge.epoch = epoch_size;
        }
        if let Some(min_percent_selected) = min_percent_selected {
            if min_percent_selected.is_zero() {
                gauge.min_percent_selected = None
            } else {
                ensure!(
                    min_percent_selected < Decimal::one(),
                    ContractError::MinPercentSelectedTooBig {}
                );
                gauge.min_percent_selected = Some(min_percent_selected)
            };
        }
        if let Some(max_options_selected) = max_options_selected {
            ensure!(
                max_options_selected > 0,
                ContractError::MaxOptionsSelectedTooSmall {}
            );
            gauge.max_options_selected = max_options_selected;
        }
        if let Some(max_available_percentage) = max_available_percentage {
            if max_available_percentage.is_zero() {
                gauge.max_available_percentage = None
            } else {
                ensure!(
                    max_available_percentage < Decimal::one(),
                    ContractError::MaxAvailablePercentTooBig {}
                );
                gauge.max_available_percentage = Some(max_available_percentage)
            };
        }
        GAUGES.save(deps.storage, gauge_id, &gauge)?;

        Ok(Response::new()
            .add_attribute("action", "update_gauge")
            .add_attribute("sender", &sender)
            .add_attribute("gauge_id", gauge_id.to_string())
            .add_attribute("epoch_size", gauge.epoch.to_string())
            .add_attribute(
                "min_percent_selected",
                gauge
                    .min_percent_selected
                    .map_or_else(|| "none".to_owned(), |value| value.to_string()),
            )
            .add_attribute(
                "max_options_selected",
                gauge.max_options_selected.to_string(),
            )
            .add_attribute(
                "max_available_percentage",
                gauge
                    .max_available_percentage
                    .map_or_else(|| "none".to_owned(), |value| value.to_string()),
            ))
    }

    pub fn stop_gauge(
        deps: DepsMut,
        sender: Addr,
        gauge_id: GaugeId,
    ) -> Result<Response, ContractError> {
        let config = CONFIG.load(deps.storage)?;
        let authorized = sender == config.owner
            || matches!(
                load_power_source(deps.storage)?,
                PowerSource::EpochSnapshot { guardian } if sender == guardian
            );
        if !authorized {
            return Err(ContractError::Unauthorized {});
        }

        let gauge = GAUGES.load(deps.storage, gauge_id)?;
        let gauge = Gauge {
            is_stopped: true,
            ..gauge
        };
        GAUGES.save(deps.storage, gauge_id, &gauge)?;

        Ok(Response::new()
            .add_attribute("action", "stop_gauge")
            .add_attribute("sender", &sender)
            .add_attribute("gauge_id", gauge_id.to_string()))
    }

    pub fn resume_gauge(
        deps: DepsMut,
        sender: Addr,
        gauge_id: GaugeId,
    ) -> Result<Response, ContractError> {
        let config = CONFIG.load(deps.storage)?;
        if sender != config.owner {
            return Err(ContractError::Unauthorized {});
        }

        let mut gauge = GAUGES.load(deps.storage, gauge_id)?;
        gauge.is_stopped = false;
        GAUGES.save(deps.storage, gauge_id, &gauge)?;

        Ok(Response::new()
            .add_attribute("action", "resume_gauge")
            .add_attribute("sender", &sender)
            .add_attribute("gauge_id", gauge_id.to_string()))
    }

    pub fn update_snapshot_policy(
        deps: DepsMut,
        sender: Addr,
        gauge_id: GaugeId,
        policy: EpochSnapshotPolicy,
    ) -> Result<Response, ContractError> {
        if !matches!(
            load_power_source(deps.storage)?,
            PowerSource::EpochSnapshot { .. }
        ) {
            return Err(ContractError::SnapshotModeRequired {});
        }
        if sender != CONFIG.load(deps.storage)?.owner {
            return Err(ContractError::Unauthorized {});
        }
        GAUGES.load(deps.storage, gauge_id)?;
        validate_snapshot_policy(&policy)?;
        let version = SNAPSHOT_POLICY_VERSIONS
            .may_load(deps.storage, gauge_id)?
            .unwrap_or(1)
            .checked_add(1)
            .ok_or(ContractError::SnapshotArithmetic {})?;
        SNAPSHOT_POLICIES.save(deps.storage, gauge_id, &policy)?;
        SNAPSHOT_POLICY_VERSIONS.save(deps.storage, gauge_id, &version)?;
        Ok(Response::new()
            .add_attribute("action", "update_snapshot_policy")
            .add_attribute("sender", sender)
            .add_attribute("gauge_id", gauge_id.to_string())
            .add_attribute("policy_version", version.to_string())
            .add_attribute("min_turnout_bps", policy.min_turnout_bps.to_string())
            .add_attribute("epoch_budget", policy.epoch_budget)
            .add_attribute("denom", &policy.denom)
            .add_attribute(
                "retained_option",
                policy.retained_option.as_deref().unwrap_or("none"),
            )
            .add_attribute(
                "execution_window_seconds",
                policy.execution_window_seconds.to_string(),
            ))
    }

    pub fn open_epoch(
        deps: DepsMut,
        env: Env,
        sender: Addr,
        gauge_id: GaugeId,
    ) -> Result<Response, ContractError> {
        if !matches!(
            load_power_source(deps.storage)?,
            PowerSource::EpochSnapshot { .. }
        ) {
            return Err(ContractError::SnapshotModeRequired {});
        }
        let mut gauge = GAUGES.load(deps.storage, gauge_id)?;
        if gauge.is_stopped {
            return Err(ContractError::GaugeStopped(gauge_id));
        }
        if let Some(current) = CURRENT_EPOCH.may_load(deps.storage, gauge_id)? {
            let current_epoch = EPOCHS.load(deps.storage, (gauge_id, current))?;
            if current_epoch.outcome == EpochOutcome::Open {
                return Err(ContractError::EpochAlreadyOpen(gauge_id));
            }
        }
        let now = env.block.time.seconds();
        if now < gauge.next_epoch {
            return Err(ContractError::EpochNotReached {
                gauge_id,
                current_epoch: now,
                next_epoch: gauge.next_epoch,
            });
        }
        let policy = SNAPSHOT_POLICIES.load(deps.storage, gauge_id)?;
        validate_snapshot_policy(&policy)?;
        let policy_version = SNAPSHOT_POLICY_VERSIONS
            .may_load(deps.storage, gauge_id)?
            .unwrap_or(1);
        let config = CONFIG.load(deps.storage)?;
        let available_balance = deps
            .querier
            .query_balance(config.dao_core.clone(), policy.denom.clone())?
            .amount;
        if available_balance < policy.epoch_budget {
            return Err(ContractError::InsufficientEpochFunding {
                required: policy.epoch_budget,
                available: available_balance,
                denom: policy.denom,
            });
        }
        let snapshot_height = env.block.height;
        let total: TotalPowerAtHeightResponse = deps.querier.query_wasm_smart(
            config.voting_powers,
            &DaoQuery::TotalPowerAtHeight {
                height: Some(snapshot_height),
            },
        )?;
        if total.height != snapshot_height {
            return Err(ContractError::SnapshotHeightMismatch {
                expected: snapshot_height,
                actual: total.height,
            });
        }
        if total.power.is_zero() {
            return Err(ContractError::ZeroSnapshotTotalPower {
                height: snapshot_height,
            });
        }
        let options = bounded_adapter_options(deps.as_ref(), &gauge.adapter)?;
        let mut seen = std::collections::HashSet::with_capacity(options.len());
        for option in &options {
            if option.is_empty() || option.len() > MAX_OPTION_BYTES || !seen.insert(option.as_str())
            {
                return Err(ContractError::OptionAlreadyExists {
                    option: option.clone(),
                    gauge_id,
                });
            }
        }
        if let Some(retained_option) = &policy.retained_option {
            if !seen.contains(retained_option.as_str()) {
                return Err(ContractError::RetainedOptionMissing {
                    gauge: gauge_id,
                    option: retained_option.clone(),
                });
            }
        }

        let epoch_id = NEXT_EPOCH_ID.may_load(deps.storage, gauge_id)?.unwrap_or(1);
        let next_id = epoch_id
            .checked_add(1)
            .ok_or(ContractError::SnapshotArithmetic {})?;
        let closes_at = now
            .checked_add(gauge.epoch)
            .ok_or(ContractError::SnapshotArithmetic {})?;
        let execution_deadline = closes_at
            .checked_add(policy.execution_window_seconds)
            .ok_or(ContractError::SnapshotArithmetic {})?;
        let epoch = SnapshotEpoch {
            gauge_id,
            epoch_id,
            snapshot_height,
            snapshot_total_power: total.power,
            participating_power: Uint128::zero(),
            total_cast: Uint128::zero(),
            retained_option: policy.retained_option,
            retained_option_power: Uint128::zero(),
            selected_project_power: Uint128::zero(),
            emitted_value: Uint128::zero(),
            retained_value: Uint128::zero(),
            min_turnout_bps: policy.min_turnout_bps,
            policy_version,
            epoch_budget: policy.epoch_budget,
            denom: policy.denom,
            opens_at: now,
            closes_at,
            execution_deadline,
            voter_count: 0,
            receipt_count: 0,
            option_count: options.len() as u32,
            outcome: EpochOutcome::Open,
            cleanup: CleanupProgress {
                phase: CleanupPhase::Ballots,
                cursor: 0,
                complete: false,
            },
        };
        for (index, option) in options.into_iter().enumerate() {
            let position =
                u32::try_from(index + 1).map_err(|_| ContractError::SnapshotArithmetic {})?;
            EPOCH_OPTIONS.save(deps.storage, (gauge_id, epoch_id, &option), &())?;
            EPOCH_TALLY.save(deps.storage, (gauge_id, epoch_id, &option), &0)?;
            EPOCH_OPTION_INDEX.save(deps.storage, (gauge_id, epoch_id, position), &option)?;
        }
        EPOCHS.save(deps.storage, (gauge_id, epoch_id), &epoch)?;
        CURRENT_EPOCH.save(deps.storage, gauge_id, &epoch_id)?;
        NEXT_EPOCH_ID.save(deps.storage, gauge_id, &next_id)?;
        gauge.next_epoch = closes_at;
        GAUGES.save(deps.storage, gauge_id, &gauge)?;
        Ok(Response::new()
            .add_attribute("action", "open_snapshot_epoch")
            .add_attribute("sender", sender)
            .add_attribute("gauge_id", gauge_id.to_string())
            .add_attribute("epoch_id", epoch_id.to_string())
            .add_attribute("snapshot_height", snapshot_height.to_string())
            .add_attribute("snapshot_total_power", total.power)
            .add_attribute("opens_at", now.to_string())
            .add_attribute("closes_at", closes_at.to_string())
            .add_attribute("execution_deadline", execution_deadline.to_string())
            .add_attribute("policy_version", epoch.policy_version.to_string())
            .add_attribute("min_turnout_bps", epoch.min_turnout_bps.to_string())
            .add_attribute("epoch_budget", epoch.epoch_budget)
            .add_attribute("denom", &epoch.denom)
            .add_attribute(
                "retained_option",
                epoch.retained_option.as_deref().unwrap_or("none"),
            )
            .add_attribute("option_count", epoch.option_count.to_string()))
    }

    pub fn expire_epoch(
        deps: DepsMut,
        env: Env,
        sender: Addr,
        gauge_id: GaugeId,
    ) -> Result<Response, ContractError> {
        if !matches!(
            load_power_source(deps.storage)?,
            PowerSource::EpochSnapshot { .. }
        ) {
            return Err(ContractError::SnapshotModeRequired {});
        }
        let epoch_id =
            CURRENT_EPOCH
                .may_load(deps.storage, gauge_id)?
                .ok_or(ContractError::EpochNotOpen {
                    gauge: gauge_id,
                    epoch: 0,
                })?;
        let mut epoch = EPOCHS.load(deps.storage, (gauge_id, epoch_id))?;
        if epoch.outcome != EpochOutcome::Open {
            return Err(ContractError::EpochNotOpen {
                gauge: gauge_id,
                epoch: epoch_id,
            });
        }
        let now = env.block.time.seconds();
        if now < epoch.execution_deadline {
            return Err(ContractError::ExecutionDeadlineNotReached {
                deadline: epoch.execution_deadline,
                current: now,
            });
        }
        let mut gauge = GAUGES.load(deps.storage, gauge_id)?;
        epoch.outcome = EpochOutcome::Expired;
        epoch.retained_value = epoch.epoch_budget;
        gauge.last_executed_set = Some(vec![]);
        gauge.next_epoch = now;
        EPOCHS.save(deps.storage, (gauge_id, epoch_id), &epoch)?;
        GAUGES.save(deps.storage, gauge_id, &gauge)?;
        Ok(snapshot_terminal_response(
            "expire_snapshot_epoch",
            &sender,
            &epoch,
            "expired",
            0,
        ))
    }

    pub fn abort_epoch(
        deps: DepsMut,
        env: Env,
        sender: Addr,
        gauge_id: GaugeId,
        reason: String,
    ) -> Result<Response, ContractError> {
        if !matches!(
            load_power_source(deps.storage)?,
            PowerSource::EpochSnapshot { .. }
        ) {
            return Err(ContractError::SnapshotModeRequired {});
        }
        if sender != CONFIG.load(deps.storage)?.owner {
            return Err(ContractError::Unauthorized {});
        }
        if reason.is_empty() || reason.len() > MAX_ABORT_REASON_BYTES {
            return Err(ContractError::InvalidAbortReason {});
        }
        let epoch_id =
            CURRENT_EPOCH
                .may_load(deps.storage, gauge_id)?
                .ok_or(ContractError::EpochNotOpen {
                    gauge: gauge_id,
                    epoch: 0,
                })?;
        let mut epoch = EPOCHS.load(deps.storage, (gauge_id, epoch_id))?;
        if epoch.outcome != EpochOutcome::Open {
            return Err(ContractError::EpochNotOpen {
                gauge: gauge_id,
                epoch: epoch_id,
            });
        }
        let mut gauge = GAUGES.load(deps.storage, gauge_id)?;
        epoch.outcome = EpochOutcome::Aborted {
            reason: reason.clone(),
        };
        epoch.retained_value = epoch.epoch_budget;
        gauge.last_executed_set = Some(vec![]);
        gauge.next_epoch = env.block.time.seconds();
        EPOCHS.save(deps.storage, (gauge_id, epoch_id), &epoch)?;
        GAUGES.save(deps.storage, gauge_id, &gauge)?;
        Ok(
            snapshot_terminal_response("abort_snapshot_epoch", &sender, &epoch, "aborted", 0)
                .add_attribute("reason", reason),
        )
    }

    pub fn cleanup_epoch(
        deps: DepsMut,
        sender: Addr,
        gauge_id: GaugeId,
        epoch_id: u64,
        limit: u32,
    ) -> Result<Response, ContractError> {
        if !matches!(
            load_power_source(deps.storage)?,
            PowerSource::EpochSnapshot { .. }
        ) {
            return Err(ContractError::SnapshotModeRequired {});
        }
        if limit == 0 || limit > 100 {
            return Err(ContractError::InvalidCleanupLimit {});
        }
        let mut epoch = EPOCHS.load(deps.storage, (gauge_id, epoch_id))?;
        if epoch.outcome == EpochOutcome::Open {
            return Err(ContractError::EpochNotTerminal {});
        }
        let mut processed = 0u32;
        match epoch.cleanup.phase {
            CleanupPhase::Ballots => {
                while processed < limit && epoch.cleanup.cursor < epoch.receipt_count {
                    epoch.cleanup.cursor += 1;
                    if let Some(voter) = EPOCH_BALLOT_INDEX
                        .may_load(deps.storage, (gauge_id, epoch_id, epoch.cleanup.cursor))?
                    {
                        EPOCH_BALLOTS.remove(deps.storage, (gauge_id, epoch_id, &voter));
                        EPOCH_BALLOT_SEEN.remove(deps.storage, (gauge_id, epoch_id, &voter));
                        EPOCH_BALLOT_POSITION.remove(deps.storage, (gauge_id, epoch_id, &voter));
                        EPOCH_VOTER_POWER.remove(deps.storage, (gauge_id, epoch_id, &voter));
                        EPOCH_BALLOT_INDEX
                            .remove(deps.storage, (gauge_id, epoch_id, epoch.cleanup.cursor));
                    }
                    processed += 1;
                }
                if epoch.cleanup.cursor >= epoch.receipt_count {
                    epoch.cleanup.phase = CleanupPhase::Options;
                    epoch.cleanup.cursor = 0;
                }
            }
            CleanupPhase::Options => {
                while processed < limit && epoch.cleanup.cursor < epoch.option_count {
                    epoch.cleanup.cursor += 1;
                    if let Some(option) = EPOCH_OPTION_INDEX
                        .may_load(deps.storage, (gauge_id, epoch_id, epoch.cleanup.cursor))?
                    {
                        EPOCH_OPTIONS.remove(deps.storage, (gauge_id, epoch_id, &option));
                        EPOCH_TALLY.remove(deps.storage, (gauge_id, epoch_id, &option));
                        EPOCH_OPTION_INDEX
                            .remove(deps.storage, (gauge_id, epoch_id, epoch.cleanup.cursor));
                    }
                    processed += 1;
                }
                if epoch.cleanup.cursor >= epoch.option_count {
                    epoch.cleanup.phase = CleanupPhase::Complete;
                    epoch.cleanup.complete = true;
                }
            }
            CleanupPhase::Complete => {}
        }
        EPOCHS.save(deps.storage, (gauge_id, epoch_id), &epoch)?;
        Ok(Response::new()
            .add_attribute("action", "cleanup_snapshot_epoch")
            .add_attribute("sender", sender)
            .add_attribute("gauge_id", gauge_id.to_string())
            .add_attribute("epoch_id", epoch_id.to_string())
            .add_attribute("processed", processed.to_string())
            .add_attribute("phase", format!("{:?}", epoch.cleanup.phase).to_lowercase())
            .add_attribute("complete", epoch.cleanup.complete.to_string()))
    }

    pub fn remove_option(
        deps: DepsMut,
        sender: Addr,
        gauge_id: GaugeId,
        option: String,
    ) -> Result<Response, ContractError> {
        // check if such option even exists
        if !TALLY.has(deps.as_ref().storage, (gauge_id, &option)) {
            return Err(ContractError::OptionDoesNotExists { option, gauge_id });
        };

        // only owner can remove option for now
        if sender != CONFIG.load(deps.storage)?.owner {
            return Err(ContractError::Unauthorized {});
        }

        let points = TALLY.load(deps.storage, (gauge_id, &option))?;
        OPTION_BY_POINTS.remove(deps.storage, (gauge_id, points, &option));
        TALLY.remove(deps.storage, (gauge_id, &option));
        // Defensive cleanup for tombstones created by earlier unreleased
        // revisions of this branch.
        INVALID_OPTIONS.remove(deps.storage, (gauge_id, &option));
        TOTAL_CAST.update(
            deps.storage,
            gauge_id,
            |total| -> Result<_, ContractError> {
                total
                    .unwrap_or_default()
                    .checked_sub(points)
                    .ok_or(ContractError::TotalCastUnderflow { gauge_id })
            },
        )?;

        Ok(Response::new()
            .add_attribute("action", "remove_option")
            .add_attribute("sender", &sender)
            .add_attribute("gauge_id", gauge_id.to_string())
            .add_attribute("option", option))
    }

    pub fn reset_gauge(
        deps: DepsMut,
        env: Env,
        sender: Addr,
        gauge_id: GaugeId,
        batch_size: u32,
    ) -> Result<Response, ContractError> {
        const MAX_RESET_BATCH_SIZE: u32 = 100;
        if batch_size == 0 || batch_size > MAX_RESET_BATCH_SIZE {
            return Err(ContractError::InvalidResetBatchSize {
                size: batch_size,
                max: MAX_RESET_BATCH_SIZE,
            });
        }

        let mut gauge = GAUGES.load(deps.storage, gauge_id)?;
        if gauge.is_stopped {
            return Err(ContractError::GaugeStopped(gauge_id));
        }
        match gauge.reset {
            Some(ref mut reset) if reset.next <= env.block.time.seconds() => {
                if reset.reset_each == 0 {
                    return Err(ContractError::InvalidResetInterval {});
                }
                let starting = reset.last != Some(reset.next);
                if starting {
                    reset.last = Some(reset.next);
                    RESET_CURSOR.remove(deps.storage, gauge_id);
                }

                // Scan the primary option namespace in stable lexical order.
                // Read one lookahead key so a batch that exactly consumes the
                // remaining work can complete without an extra keeper call.
                let cursor = RESET_CURSOR.may_load(deps.storage, gauge_id)?;
                let start = cursor.as_deref().map(Bound::exclusive);
                let options = TALLY
                    .prefix(gauge_id)
                    .keys(deps.storage, start, None, Order::Ascending)
                    .take(batch_size as usize + 1)
                    .collect::<StdResult<Vec<_>>>()?;
                let processed = options.len().min(batch_size as usize);
                for option in options.iter().take(processed) {
                    let points = TALLY.load(deps.storage, (gauge_id, option))?;
                    OPTION_BY_POINTS.remove(deps.storage, (gauge_id, points, option));
                    if INVALID_OPTIONS.has(deps.storage, (gauge_id, option)) {
                        TALLY.remove(deps.storage, (gauge_id, option));
                        INVALID_OPTIONS.remove(deps.storage, (gauge_id, option));
                    } else {
                        OPTION_BY_POINTS.save(deps.storage, (gauge_id, 0, option), &1)?;
                        TALLY.save(deps.storage, (gauge_id, option), &0)?;
                    }
                }

                let complete = options.len() <= batch_size as usize;
                if complete {
                    TOTAL_CAST.save(deps.storage, gauge_id, &0)?;
                    RESET_CURSOR.remove(deps.storage, gauge_id);

                    // Catch up from the prior deadline to the first deadline
                    // strictly after the current block, avoiding reset storms
                    // after long downtime while preserving schedule cadence.
                    let elapsed = env
                        .block
                        .time
                        .seconds()
                        .checked_sub(reset.next)
                        .ok_or(ContractError::ResetScheduleOverflow {})?;
                    let intervals = elapsed
                        .checked_div(reset.reset_each)
                        .and_then(|n| n.checked_add(1))
                        .ok_or(ContractError::ResetScheduleOverflow {})?;
                    let advance = reset
                        .reset_each
                        .checked_mul(intervals)
                        .ok_or(ContractError::ResetScheduleOverflow {})?;
                    reset.next = reset
                        .next
                        .checked_add(advance)
                        .ok_or(ContractError::ResetScheduleOverflow {})?;
                } else if let Some(last) = options.get(processed.saturating_sub(1)) {
                    RESET_CURSOR.save(deps.storage, gauge_id, last)?;
                }

                let next_reset = reset.next;
                GAUGES.save(deps.storage, gauge_id, &gauge)?;
                Ok(Response::new()
                    .add_attribute("action", "reset_gauge")
                    .add_attribute("sender", sender)
                    .add_attribute("gauge_id", gauge_id.to_string())
                    .add_attribute("processed", processed.to_string())
                    .add_attribute("complete", complete.to_string())
                    .add_attribute("next_reset", next_reset.to_string()))
            }
            Some(_) => Err(ContractError::ResetEpochNotPassed {}),
            None => Err(ContractError::Unauthorized {}),
        }
    }

    /// Handler for `ExecuteMsg::AddOption`. Validates the option against the
    /// gauge's adapter and requires the sender to hold nonzero voting power
    /// (anti-spam). Use `add_adapter_options` for the trusted bulk-add path
    /// during gauge attachment.
    pub fn add_option(
        deps: DepsMut,
        sender: Addr,
        gauge_id: GaugeId,
        option: String,
    ) -> Result<Response, ContractError> {
        if option.len() > MAX_OPTION_BYTES {
            return Err(ContractError::StringTooLong {
                field: "option".to_owned(),
                max: MAX_OPTION_BYTES,
            });
        }
        // Tombstones remain in TALLY to keep stored zero-power votes from
        // reactivating removed options. Capacity is the bounded active index,
        // not that historical safety namespace.
        let option_count = OPTION_BY_POINTS
            .sub_prefix(gauge_id)
            .keys(deps.storage, None, None, Order::Ascending)
            .take(MAX_OPTIONS_PER_GAUGE + 1)
            .collect::<StdResult<Vec<_>>>()?
            .len();
        if option_count >= MAX_OPTIONS_PER_GAUGE {
            return Err(ContractError::TooManyOptions {
                count: option_count + 1,
                max: MAX_OPTIONS_PER_GAUGE,
            });
        }
        if TALLY.has(deps.as_ref().storage, (gauge_id, &option)) {
            return Err(ContractError::OptionAlreadyExists { option, gauge_id });
        };

        let gauge = GAUGES.load(deps.storage, gauge_id)?;
        let adapter_option: CheckOptionResponse = deps
            .querier
            .query_wasm_smart(
                gauge.adapter,
                &AdapterQueryMsg::CheckOption {
                    option: option.clone(),
                },
            )
            .map_err(|_| ContractError::OptionInvalidByAdapter {
                option: option.clone(),
                gauge_id,
            })?;
        if !adapter_option.valid {
            return Err(ContractError::OptionInvalidByAdapter { option, gauge_id });
        }

        // Anti-spam: require sender to hold nonzero voting power.
        let voting_power = deps
            .querier
            .query::<VotingPowerAtHeightResponse>(&QueryRequest::Wasm(WasmQuery::Smart {
                contract_addr: CONFIG.load(deps.storage)?.voting_powers.to_string(),
                msg: to_json_binary(&DaoQuery::VotingPowerAtHeight {
                    address: sender.to_string(),
                    height: None,
                })?,
            }))?
            .power;
        if voting_power.is_zero() {
            return Err(ContractError::NoVotingPower(sender.to_string()));
        }

        register_option(deps.storage, gauge_id, &option)?;

        Ok(Response::new()
            .add_attribute("action", "add_option")
            .add_attribute("sender", &sender)
            .add_attribute("gauge_id", gauge_id.to_string())
            .add_attribute("option", option))
    }

    /// Bulk-add options reported by the gauge's adapter during attachment.
    /// Trusts the adapter's option list — skips per-option validation and
    /// voting-power checks. Must only be called by trusted internal code.
    pub(super) fn add_adapter_options(
        mut deps: DepsMut,
        gauge_id: GaugeId,
        options: Vec<String>,
    ) -> Result<(), ContractError> {
        for option in options {
            if option.is_empty() || option.len() > MAX_OPTION_BYTES {
                return Err(ContractError::StringTooLong {
                    field: "option".to_owned(),
                    max: MAX_OPTION_BYTES,
                });
            }
            if TALLY.has(deps.as_ref().storage, (gauge_id, &option)) {
                return Err(ContractError::OptionAlreadyExists { option, gauge_id });
            }
            register_option(deps.branch().storage, gauge_id, &option)?;
        }
        Ok(())
    }

    fn register_option(
        storage: &mut dyn cosmwasm_std::Storage,
        gauge_id: GaugeId,
        option: &str,
    ) -> Result<(), ContractError> {
        update_tally(storage, gauge_id, option, 0u128, 0u128)?;
        Ok(())
    }

    pub fn place_votes(
        mut deps: DepsMut,
        env: Env,
        sender: Addr,
        gauge_id: GaugeId,
        new_votes: Option<Vec<Vote>>,
    ) -> Result<Response, ContractError> {
        let gauge = match GAUGES.may_load(deps.storage, gauge_id)? {
            Some(gauge) => gauge,
            None => return Err(ContractError::GaugeMissing(gauge_id)),
        };

        if gauge.is_stopped {
            return Err(ContractError::GaugeStopped(gauge_id));
        }

        if matches!(
            load_power_source(deps.storage)?,
            PowerSource::EpochSnapshot { .. }
        ) {
            return place_snapshot_votes(deps, env, sender, gauge_id, new_votes);
        }

        // Once the reset deadline is reached, no vote may be accepted until a
        // keeper completes reset and advances `next`. Otherwise a delayed
        // reset would erase votes cast in the gap before its first batch.
        let reset_due = gauge
            .reset
            .as_ref()
            .map(|reset| reset.next <= env.block.time.seconds())
            .unwrap_or_default();
        if reset_due || gauge.is_resetting() {
            return Err(ContractError::GaugeResetting(gauge_id));
        }

        // Validate the complete payload before querying power or touching tally
        // state. Partial allocation is intentional: unallocated weight is not
        // counted and is never redistributed by vote accounting.
        let new_votes = new_votes.unwrap_or_default();
        const MAX_VOTES_PER_GAUGE: usize = 100;
        if new_votes.len() > MAX_VOTES_PER_GAUGE {
            return Err(ContractError::TooManyVoteEntries {
                count: new_votes.len(),
                max: MAX_VOTES_PER_GAUGE,
            });
        }
        let mut seen = std::collections::HashSet::with_capacity(new_votes.len());
        for vote in &new_votes {
            if vote.option.is_empty() {
                return Err(ContractError::EmptyVoteOption {});
            }
            if vote.option.len() > MAX_OPTION_BYTES {
                return Err(ContractError::StringTooLong {
                    field: "option".to_owned(),
                    max: MAX_OPTION_BYTES,
                });
            }
            if vote.weight.is_zero() {
                return Err(ContractError::ZeroVoteWeight {
                    option: vote.option.clone(),
                });
            }
            if !seen.insert(vote.option.as_str()) {
                return Err(ContractError::DuplicateVoteOption {
                    option: vote.option.clone(),
                });
            }
        }
        let mut total_weight = Decimal::zero();
        for vote in &new_votes {
            total_weight = total_weight
                .checked_add(vote.weight)
                .map_err(|_| ContractError::VoteWeightOverflow {})?;
            // Check incrementally. Besides failing earlier, this prevents a
            // vector of enormous Decimal values from overflowing inside an
            // unchecked iterator sum before reaching the >100% validation.
            if total_weight > Decimal::one() {
                return Err(ContractError::TooMuchVotingWeight(total_weight));
            }
        }

        // load voter power from voting powers contract (DAO)
        let voting_power = deps
            .querier
            .query::<VotingPowerAtHeightResponse>(&QueryRequest::Wasm(WasmQuery::Smart {
                contract_addr: CONFIG.load(deps.storage)?.voting_powers.to_string(),
                msg: to_json_binary(&DaoQuery::VotingPowerAtHeight {
                    address: sender.to_string(),
                    height: None,
                })?,
            }))?
            .power;
        if voting_power.is_zero() {
            return Err(ContractError::NoVotingPower(sender.to_string()));
        }

        // Reject votes whose per-option weight rounds to zero against the
        // voter's power. Without this, a user with e.g. 1 staked NFT and a
        // 50/50 split would have *both* options counted as 0 — silently
        // erasing their voice. Fail loudly instead so they can retry with
        // larger weights or fewer options.
        for v in new_votes.iter() {
            if !v.weight.is_zero() && (voting_power * v.weight).is_zero() {
                return Err(ContractError::VoteWeightRoundsToZero {
                    weight: v.weight,
                    voting_power,
                });
            }
        }

        let mut previous_vote = votes().may_load(deps.storage, &sender, gauge_id)?;
        if let Some(v) = &previous_vote {
            if v.is_expired(&gauge) {
                previous_vote = None;
            }
        }
        if previous_vote.is_none() && new_votes.is_empty() {
            return Err(ContractError::CannotRemoveNonexistingVote {});
        }
        if previous_vote.is_none() {
            let existing = votes().power_change_votes(deps.as_ref(), &sender)?;
            if existing.len() >= MAX_GAUGE_VOTES_PER_VOTER {
                return Err(ContractError::TooManyGaugeVotes {
                    // power_change_votes returns at most the enforced maximum;
                    // this branch represents the one additional attempted
                    // record, so the reported count is deterministic.
                    count: MAX_GAUGE_VOTES_PER_VOTER.saturating_add(1),
                    max: MAX_GAUGE_VOTES_PER_VOTER,
                });
            }
        }

        // first, calculate a diff between new_vote and previous_vote (option -> (old, new))
        let previous_vote = previous_vote.unwrap_or_default();
        let power = previous_vote.power;
        let mut diff: HashMap<&str, (u128, u128)> = previous_vote
            .votes
            .iter()
            .filter(|v| TALLY.has(deps.storage, (gauge_id, v.option.as_str())))
            .map(|v| (v.option.as_str(), ((power * v.weight).u128(), 0u128)))
            .collect();
        for v in new_votes.iter() {
            let new = (voting_power * v.weight).u128();
            let add = match diff.remove(v.option.as_str()) {
                Some((old, _)) => (old, new),
                None => (0, new),
            };
            diff.insert(&v.option, add);
        }

        // Every option in the replacement must still be active. An option may
        // have been tombstoned since this voter last submitted it.
        for new_opt in new_votes.iter().map(|vote| vote.option.as_str()) {
            if !TALLY.has(deps.storage, (gauge_id, new_opt))
                || INVALID_OPTIONS.has(deps.storage, (gauge_id, new_opt))
            {
                return Err(ContractError::OptionDoesNotExists {
                    option: new_opt.to_string(),
                    gauge_id,
                });
            }
        }

        // third, update tally based on diff
        let updates: Vec<(&str, u128, u128)> = diff
            .iter()
            .map(|(&k, (old, new))| (k, *old, *new))
            .collect();
        update_tallies(deps.storage, gauge_id, updates)?;

        // finally, update the votes for this user
        if new_votes.is_empty() {
            // completely remove sender's votes
            votes().remove_votes(deps.storage, &sender, gauge_id)?;
        } else {
            // store sender's new votes (overwriting old votes)
            votes().set_votes(
                deps.storage,
                &env,
                &sender,
                gauge_id,
                new_votes,
                voting_power,
            )?;
        }

        // Snapshot of the new state to ship to any registered vote-hook
        // subscribers. `votes` is the *new* set (empty means abstain).
        let snapshot_votes = votes()
            .may_load(deps.storage, &sender, gauge_id)?
            .map(|v| v.votes)
            .unwrap_or_default();
        let option_count = snapshot_votes.len();
        let hook_msgs = new_vote_hook_msgs(
            VOTE_HOOKS,
            deps.branch(),
            gauge_id,
            sender.clone(),
            snapshot_votes,
            voting_power,
            env.block.height,
        )?;

        let response = Response::new()
            .add_attribute("action", "place_vote")
            .add_attribute("sender", &sender)
            .add_attribute("gauge_id", gauge_id.to_string())
            .add_attribute("option_count", option_count.to_string())
            .add_attribute("voting_power", voting_power)
            .add_submessages(hook_msgs);
        Ok(response)
    }

    fn place_snapshot_votes(
        deps: DepsMut,
        env: Env,
        sender: Addr,
        gauge_id: GaugeId,
        new_votes: Option<Vec<Vote>>,
    ) -> Result<Response, ContractError> {
        let epoch_id =
            CURRENT_EPOCH
                .may_load(deps.storage, gauge_id)?
                .ok_or(ContractError::EpochNotOpen {
                    gauge: gauge_id,
                    epoch: 0,
                })?;
        let mut epoch = EPOCHS.load(deps.storage, (gauge_id, epoch_id))?;
        if epoch.outcome != EpochOutcome::Open {
            return Err(ContractError::EpochNotOpen {
                gauge: gauge_id,
                epoch: epoch_id,
            });
        }
        let now = env.block.time.seconds();
        if now >= epoch.closes_at {
            return Err(ContractError::SnapshotVotingClosed {
                closes_at: epoch.closes_at,
                current: now,
            });
        }
        let new_votes = new_votes.unwrap_or_default();
        if new_votes.len() > MAX_OPTIONS_PER_GAUGE {
            return Err(ContractError::TooManyVoteEntries {
                count: new_votes.len(),
                max: MAX_OPTIONS_PER_GAUGE,
            });
        }
        let mut seen_options = std::collections::HashSet::with_capacity(new_votes.len());
        let mut total_weight = Decimal::zero();
        for vote in &new_votes {
            if vote.option.is_empty() {
                return Err(ContractError::EmptyVoteOption {});
            }
            if vote.option.len() > MAX_OPTION_BYTES {
                return Err(ContractError::StringTooLong {
                    field: "option".to_owned(),
                    max: MAX_OPTION_BYTES,
                });
            }
            if vote.weight.is_zero() {
                return Err(ContractError::ZeroVoteWeight {
                    option: vote.option.clone(),
                });
            }
            if !seen_options.insert(vote.option.as_str()) {
                return Err(ContractError::DuplicateVoteOption {
                    option: vote.option.clone(),
                });
            }
            if !EPOCH_OPTIONS.has(deps.storage, (gauge_id, epoch_id, &vote.option)) {
                return Err(ContractError::OptionDoesNotExists {
                    option: vote.option.clone(),
                    gauge_id,
                });
            }
            total_weight = total_weight
                .checked_add(vote.weight)
                .map_err(|_| ContractError::VoteWeightOverflow {})?;
            if total_weight > Decimal::one() {
                return Err(ContractError::TooMuchVotingWeight(total_weight));
            }
        }

        let previous = EPOCH_BALLOTS.may_load(deps.storage, (gauge_id, epoch_id, &sender))?;
        if previous.is_none() && new_votes.is_empty() {
            return Err(ContractError::CannotRemoveNonexistingVote {});
        }
        let power = if let Some(power) =
            EPOCH_VOTER_POWER.may_load(deps.storage, (gauge_id, epoch_id, &sender))?
        {
            power
        } else {
            let response: VotingPowerAtHeightResponse = deps.querier.query_wasm_smart(
                CONFIG.load(deps.storage)?.voting_powers,
                &DaoQuery::VotingPowerAtHeight {
                    address: sender.to_string(),
                    height: Some(epoch.snapshot_height),
                },
            )?;
            if response.height != epoch.snapshot_height {
                return Err(ContractError::SnapshotHeightMismatch {
                    expected: epoch.snapshot_height,
                    actual: response.height,
                });
            }
            if response.power.is_zero() {
                return Err(ContractError::NoVotingPower(sender.to_string()));
            }
            EPOCH_VOTER_POWER.save(deps.storage, (gauge_id, epoch_id, &sender), &response.power)?;
            response.power
        };
        for vote in &new_votes {
            if (power * vote.weight).is_zero() {
                return Err(ContractError::VoteWeightRoundsToZero {
                    weight: vote.weight,
                    voting_power: power,
                });
            }
        }

        // This loop writes consensus state, so keep its option traversal
        // deterministic rather than relying on a randomized hash iteration.
        let mut diff: BTreeMap<String, (u128, u128)> = previous
            .as_ref()
            .map(|ballot| {
                ballot
                    .votes
                    .iter()
                    .map(|vote| (vote.option.clone(), ((power * vote.weight).u128(), 0u128)))
                    .collect()
            })
            .unwrap_or_default();
        for vote in &new_votes {
            let new = (power * vote.weight).u128();
            let old = diff.remove(&vote.option).map(|entry| entry.0).unwrap_or(0);
            diff.insert(vote.option.clone(), (old, new));
        }
        let mut old_allocated = 0u128;
        let mut new_allocated = 0u128;
        for (option, (old, new)) in &diff {
            old_allocated = old_allocated
                .checked_add(*old)
                .ok_or(ContractError::TotalCastOverflow { gauge_id })?;
            new_allocated = new_allocated
                .checked_add(*new)
                .ok_or(ContractError::TotalCastOverflow { gauge_id })?;
            let tally = EPOCH_TALLY.load(deps.storage, (gauge_id, epoch_id, option))?;
            let tally = tally
                .checked_add(*new)
                .ok_or_else(|| ContractError::TallyOverflow {
                    gauge_id,
                    option: option.clone(),
                })?
                .checked_sub(*old)
                .ok_or_else(|| ContractError::TallyUnderflow {
                    gauge_id,
                    option: option.clone(),
                })?;
            EPOCH_TALLY.save(deps.storage, (gauge_id, epoch_id, option), &tally)?;
        }
        epoch.total_cast = epoch
            .total_cast
            .checked_add(Uint128::new(new_allocated))
            .map_err(|_| ContractError::TotalCastOverflow { gauge_id })?
            .checked_sub(Uint128::new(old_allocated))
            .map_err(|_| ContractError::TotalCastUnderflow { gauge_id })?;
        epoch.retained_option_power = match epoch.retained_option.as_deref() {
            Some(option) => {
                Uint128::new(EPOCH_TALLY.load(deps.storage, (gauge_id, epoch_id, option))?)
            }
            None => Uint128::zero(),
        };

        let revisions = match &previous {
            Some(ballot) => ballot
                .revisions
                .checked_add(1)
                .ok_or(ContractError::SnapshotArithmetic {})?,
            None => 0,
        };
        if new_votes.is_empty() {
            EPOCH_BALLOTS.remove(deps.storage, (gauge_id, epoch_id, &sender));
            epoch.participating_power =
                epoch.participating_power.checked_sub(power).map_err(|_| {
                    ContractError::VotingPowerUnderflow {
                        voter: sender.to_string(),
                    }
                })?;
            epoch.voter_count = epoch
                .voter_count
                .checked_sub(1)
                .ok_or(ContractError::SnapshotArithmetic {})?;
        } else {
            if previous.is_none() {
                epoch.participating_power =
                    epoch.participating_power.checked_add(power).map_err(|_| {
                        ContractError::VotingPowerOverflow {
                            voter: sender.to_string(),
                        }
                    })?;
                epoch.voter_count = epoch
                    .voter_count
                    .checked_add(1)
                    .ok_or(ContractError::SnapshotArithmetic {})?;
            }
            let receipt_index = if EPOCH_BALLOT_SEEN
                .may_load(deps.storage, (gauge_id, epoch_id, &sender))?
                .unwrap_or(false)
            {
                EPOCH_BALLOT_POSITION.load(deps.storage, (gauge_id, epoch_id, &sender))?
            } else {
                epoch.receipt_count = epoch
                    .receipt_count
                    .checked_add(1)
                    .ok_or(ContractError::SnapshotArithmetic {})?;
                EPOCH_BALLOT_INDEX.save(
                    deps.storage,
                    (gauge_id, epoch_id, epoch.receipt_count),
                    &sender,
                )?;
                EPOCH_BALLOT_SEEN.save(deps.storage, (gauge_id, epoch_id, &sender), &true)?;
                EPOCH_BALLOT_POSITION.save(
                    deps.storage,
                    (gauge_id, epoch_id, &sender),
                    &epoch.receipt_count,
                )?;
                epoch.receipt_count
            };
            EPOCH_BALLOTS.save(
                deps.storage,
                (gauge_id, epoch_id, &sender),
                &SnapshotBallot {
                    voter: sender.clone(),
                    power,
                    votes: new_votes.clone(),
                    cast_at: previous
                        .as_ref()
                        .map(|ballot| ballot.cast_at)
                        .unwrap_or(now),
                    revised_at: now,
                    revisions,
                    receipt_index,
                },
            )?;
        }
        if epoch.total_cast > epoch.participating_power
            || epoch.participating_power > epoch.snapshot_total_power
        {
            return Err(ContractError::SnapshotArithmetic {});
        }
        EPOCHS.save(deps.storage, (gauge_id, epoch_id), &epoch)?;
        Ok(Response::new()
            .add_attribute("action", "place_snapshot_vote")
            .add_attribute("sender", sender)
            .add_attribute("gauge_id", gauge_id.to_string())
            .add_attribute("epoch_id", epoch_id.to_string())
            .add_attribute("snapshot_height", epoch.snapshot_height.to_string())
            .add_attribute("voting_power", power)
            .add_attribute("option_count", new_votes.len().to_string())
            .add_attribute("participating_power", epoch.participating_power)
            .add_attribute("allocated_power", epoch.total_cast)
            .add_attribute("total_cast", epoch.total_cast)
            .add_attribute("retained_option_power", epoch.retained_option_power)
            .add_attribute(
                "unallocated_power",
                epoch.participating_power.saturating_sub(epoch.total_cast),
            ))
    }

    pub fn add_hook(deps: DepsMut, sender: Addr, addr: String) -> Result<Response, ContractError> {
        ensure_hook_mode(deps.storage)?;
        if sender != CONFIG.load(deps.storage)?.owner {
            return Err(ContractError::Unauthorized {});
        }
        if VOTE_HOOKS.hook_count(deps.storage)? >= MAX_VOTE_HOOKS {
            return Err(ContractError::TooManyHooks {
                max: MAX_VOTE_HOOKS,
            });
        }
        let hook = deps.api.addr_validate(&addr)?;
        VOTE_HOOKS.add_hook(deps.storage, hook)?;
        Ok(Response::new()
            .add_attribute("action", "add_hook")
            .add_attribute("sender", &sender)
            .add_attribute("hook", addr))
    }

    pub fn remove_hook(
        deps: DepsMut,
        sender: Addr,
        addr: String,
    ) -> Result<Response, ContractError> {
        ensure_hook_mode(deps.storage)?;
        if sender != CONFIG.load(deps.storage)?.owner {
            return Err(ContractError::Unauthorized {});
        }
        let hook = deps.api.addr_validate(&addr)?;
        VOTE_HOOKS.remove_hook(deps.storage, hook)?;
        Ok(Response::new()
            .add_attribute("action", "remove_hook")
            .add_attribute("sender", &sender)
            .add_attribute("hook", addr))
    }

    pub fn execute(
        deps: DepsMut,
        env: Env,
        sender: Addr,
        gauge_id: u64,
    ) -> Result<Response, ContractError> {
        let mut gauge = GAUGES.load(deps.storage, gauge_id)?;

        if gauge.is_stopped {
            return Err(ContractError::GaugeStopped(gauge_id));
        }
        if matches!(
            load_power_source(deps.storage)?,
            PowerSource::EpochSnapshot { .. }
        ) {
            return execute_snapshot_epoch(deps, env, sender, gauge_id, gauge);
        }
        if gauge.is_resetting() {
            return Err(ContractError::GaugeResetting(gauge_id));
        }

        let current_epoch = env.block.time.seconds();
        if current_epoch < gauge.next_epoch {
            return Err(ContractError::EpochNotReached {
                gauge_id,
                current_epoch,
                next_epoch: gauge.next_epoch,
            });
        }
        gauge.next_epoch = current_epoch
            .checked_add(gauge.epoch)
            .ok_or(ContractError::EpochScheduleOverflow {})?;

        // this set contains tuple (option, total_voted_power)
        // for adapter query, this needs to be transformed into (option, voted_weight)
        let selected_set_with_powers = query::selected_set(deps.as_ref(), gauge_id)?.votes;
        let total_cast = TOTAL_CAST.load(deps.storage, gauge_id)?;

        // save the selected options and their powers for the frontend to display
        gauge.last_executed_set = Some(selected_set_with_powers.clone());

        if selected_set_with_powers.is_empty() {
            GAUGES.save(deps.storage, gauge_id, &gauge)?;
            return Ok(Response::new()
                .add_attribute("action", "execute_tally")
                .add_attribute("sender", &sender)
                .add_attribute("gauge_id", gauge_id.to_string())
                .add_attribute("next_epoch", gauge.next_epoch.to_string())
                .add_attribute("selected_count", "0")
                .add_attribute("message_count", "0"));
        }

        // Preserve global allocation shares. Capped or unselected power is
        // intentionally unallocated; selected entries are not renormalized.
        let selected = selected_set_with_powers
            .into_iter()
            .map(|(option, power)| (option, Decimal::from_ratio(power, total_cast)))
            .collect::<Vec<(String, Decimal)>>();
        let selected_count = selected.len();

        // query gauge adapter for execute messages for DAO
        let execute_messages: SampleGaugeMsgsResponse = deps.querier.query_wasm_smart(
            gauge.adapter.clone(),
            &AdapterQueryMsg::SampleGaugeMsgs {
                selected,
                epoch_budget: None,
                available_balance: None,
                denom: None,
            },
        )?;
        if execute_messages.execute.len() > MAX_ADAPTER_MESSAGES {
            return Err(ContractError::TooManyAdapterMessages {
                count: execute_messages.execute.len(),
                max: MAX_ADAPTER_MESSAGES,
            });
        }
        let message_count = execute_messages.execute.len();

        let config = CONFIG.load(deps.storage)?;
        let execute_msg = WasmMsg::Execute {
            contract_addr: config.dao_core.to_string(),
            msg: to_json_binary(&DaoExecuteMsg::ExecuteProposalHook {
                msgs: execute_messages.execute,
            })?,
            funds: vec![],
        };

        GAUGES.save(deps.storage, gauge_id, &gauge)?;

        Ok(Response::new()
            .add_attribute("action", "execute_tally")
            .add_attribute("sender", &sender)
            .add_attribute("gauge_id", gauge_id.to_string())
            .add_attribute("next_epoch", gauge.next_epoch.to_string())
            .add_attribute("selected_count", selected_count.to_string())
            .add_attribute("message_count", message_count.to_string())
            .add_message(execute_msg))
    }

    fn snapshot_terminal_response(
        action: &str,
        sender: &Addr,
        epoch: &SnapshotEpoch,
        outcome: &str,
        message_count: u32,
    ) -> Response {
        Response::new()
            .add_attribute("action", action)
            .add_attribute("sender", sender)
            .add_attribute("gauge_id", epoch.gauge_id.to_string())
            .add_attribute("epoch_id", epoch.epoch_id.to_string())
            .add_attribute("snapshot_height", epoch.snapshot_height.to_string())
            .add_attribute("snapshot_total_power", epoch.snapshot_total_power)
            .add_attribute("participating_power", epoch.participating_power)
            .add_attribute("allocated_power", epoch.total_cast)
            .add_attribute("total_cast", epoch.total_cast)
            .add_attribute("retained_option_power", epoch.retained_option_power)
            .add_attribute(
                "unallocated_power",
                epoch.participating_power.saturating_sub(epoch.total_cast),
            )
            .add_attribute("selected_project_power", epoch.selected_project_power)
            .add_attribute("emitted_value", epoch.emitted_value)
            .add_attribute("retained_value", epoch.retained_value)
            .add_attribute("min_turnout_bps", epoch.min_turnout_bps.to_string())
            .add_attribute("policy_version", epoch.policy_version.to_string())
            .add_attribute("epoch_budget", epoch.epoch_budget)
            .add_attribute("denom", &epoch.denom)
            .add_attribute("execution_deadline", epoch.execution_deadline.to_string())
            .add_attribute("outcome", outcome)
            .add_attribute("message_count", message_count.to_string())
    }

    fn execute_snapshot_epoch(
        deps: DepsMut,
        env: Env,
        sender: Addr,
        gauge_id: GaugeId,
        mut gauge: Gauge,
    ) -> Result<Response, ContractError> {
        let epoch_id =
            CURRENT_EPOCH
                .may_load(deps.storage, gauge_id)?
                .ok_or(ContractError::EpochNotOpen {
                    gauge: gauge_id,
                    epoch: 0,
                })?;
        let mut epoch = EPOCHS.load(deps.storage, (gauge_id, epoch_id))?;
        if epoch.outcome != EpochOutcome::Open {
            return Err(ContractError::EpochNotOpen {
                gauge: gauge_id,
                epoch: epoch_id,
            });
        }
        let now = env.block.time.seconds();
        if now < epoch.closes_at {
            return Err(ContractError::SnapshotVotingOpen {
                closes_at: epoch.closes_at,
                current: now,
            });
        }
        if now >= epoch.execution_deadline {
            return Err(ContractError::ExecutionDeadlineReached {
                deadline: epoch.execution_deadline,
                current: now,
            });
        }
        if epoch.participating_power.is_zero() {
            epoch.outcome = EpochOutcome::NoDistributionZeroParticipation;
            epoch.retained_value = epoch.epoch_budget;
            gauge.last_executed_set = Some(vec![]);
            gauge.next_epoch = now;
            EPOCHS.save(deps.storage, (gauge_id, epoch_id), &epoch)?;
            GAUGES.save(deps.storage, gauge_id, &gauge)?;
            return Ok(snapshot_terminal_response(
                "execute_snapshot_epoch",
                &sender,
                &epoch,
                "no_distribution_zero_participation",
                0,
            ));
        }
        let turnout_left = Uint256::from(epoch.participating_power) * Uint256::from(10_000u128);
        let turnout_right = Uint256::from(epoch.snapshot_total_power)
            * Uint256::from(epoch.min_turnout_bps as u128);
        if turnout_left < turnout_right {
            epoch.outcome = EpochOutcome::NoDistributionTurnout;
            epoch.retained_value = epoch.epoch_budget;
            gauge.last_executed_set = Some(vec![]);
            gauge.next_epoch = now;
            EPOCHS.save(deps.storage, (gauge_id, epoch_id), &epoch)?;
            GAUGES.save(deps.storage, gauge_id, &gauge)?;
            return Ok(snapshot_terminal_response(
                "execute_snapshot_epoch",
                &sender,
                &epoch,
                "no_distribution_turnout",
                0,
            ));
        }

        let mut candidates = EPOCH_TALLY
            .prefix((gauge_id, epoch_id))
            .range(deps.storage, None, None, Order::Ascending)
            .take(MAX_OPTIONS_PER_GAUGE + 1)
            .collect::<StdResult<Vec<_>>>()?;
        if candidates.len() > MAX_OPTIONS_PER_GAUGE {
            return Err(ContractError::TooManyOptions {
                count: candidates.len(),
                max: MAX_OPTIONS_PER_GAUGE,
            });
        }
        candidates.retain(|(option, _)| epoch.retained_option.as_ref() != Some(option));
        candidates.sort_by(|(left_option, left_power), (right_option, right_power)| {
            right_power
                .cmp(left_power)
                .then_with(|| left_option.cmp(right_option))
        });
        let mut selected_with_powers = Vec::new();
        for (option, power) in candidates {
            if power == 0 {
                continue;
            }
            if let Some(minimum) = gauge.min_percent_selected {
                if Decimal::from_ratio(power, epoch.participating_power) < minimum {
                    continue;
                }
            }
            let validity: CheckOptionResponse = deps.querier.query_wasm_smart(
                gauge.adapter.clone(),
                &AdapterQueryMsg::CheckOption {
                    option: option.clone(),
                },
            )?;
            if !validity.valid {
                continue;
            }
            let capped = gauge
                .max_available_percentage
                .map_or(Uint128::new(power), |maximum| {
                    let limit = epoch.participating_power * maximum;
                    Uint128::new(power).min(limit)
                });
            if capped.is_zero() {
                continue;
            }
            selected_with_powers.push((option, capped));
            if selected_with_powers.len() >= gauge.max_options_selected as usize {
                break;
            }
        }
        epoch.selected_project_power = selected_with_powers
            .iter()
            .try_fold(Uint128::zero(), |total, (_, power)| {
                total.checked_add(*power)
            })
            .map_err(|_| ContractError::SnapshotArithmetic {})?;
        gauge.last_executed_set = Some(selected_with_powers.clone());
        if selected_with_powers.is_empty() {
            epoch.outcome = EpochOutcome::NoEligibleOptions;
            epoch.retained_value = epoch.epoch_budget;
            gauge.next_epoch = now;
            EPOCHS.save(deps.storage, (gauge_id, epoch_id), &epoch)?;
            GAUGES.save(deps.storage, gauge_id, &gauge)?;
            return Ok(snapshot_terminal_response(
                "execute_snapshot_epoch",
                &sender,
                &epoch,
                "no_eligible_options",
                0,
            ));
        }
        let selected = selected_with_powers
            .iter()
            .map(|(option, power)| {
                (
                    option.clone(),
                    Decimal::from_ratio(*power, epoch.participating_power),
                )
            })
            .collect::<Vec<_>>();
        let config = CONFIG.load(deps.storage)?;
        let available_balance = deps
            .querier
            .query_balance(config.dao_core.clone(), epoch.denom.clone())?
            .amount;
        let adapter_response: SampleGaugeMsgsResponse = deps.querier.query_wasm_smart(
            gauge.adapter.clone(),
            &AdapterQueryMsg::SampleGaugeMsgs {
                selected,
                epoch_budget: Some(epoch.epoch_budget),
                available_balance: Some(available_balance),
                denom: Some(epoch.denom.clone()),
            },
        )?;
        if adapter_response.execute.len() > MAX_ADAPTER_MESSAGES {
            return Err(ContractError::TooManyAdapterMessages {
                count: adapter_response.execute.len(),
                max: MAX_ADAPTER_MESSAGES,
            });
        }
        let emitted_value = adapter_response
            .emitted_value
            .ok_or(ContractError::MissingAdapterAccounting {})?;
        let retained_value = adapter_response
            .retained_value
            .ok_or(ContractError::MissingAdapterAccounting {})?;
        let accounted = emitted_value
            .checked_add(retained_value)
            .map_err(|_| ContractError::InvalidAdapterAccounting {})?;
        if accounted != epoch.epoch_budget
            || emitted_value > epoch.epoch_budget
            || (adapter_response.execute.is_empty() != emitted_value.is_zero())
        {
            return Err(ContractError::InvalidAdapterAccounting {});
        }
        let message_count = u32::try_from(adapter_response.execute.len())
            .map_err(|_| ContractError::SnapshotArithmetic {})?;
        if available_balance < emitted_value {
            epoch.outcome = EpochOutcome::InsufficientFunds {
                required: emitted_value,
                available: available_balance,
            };
            epoch.emitted_value = Uint128::zero();
            epoch.retained_value = epoch.epoch_budget;
            gauge.last_executed_set = Some(selected_with_powers);
            gauge.next_epoch = now;
            EPOCHS.save(deps.storage, (gauge_id, epoch_id), &epoch)?;
            GAUGES.save(deps.storage, gauge_id, &gauge)?;
            return Ok(snapshot_terminal_response(
                "execute_snapshot_epoch",
                &sender,
                &epoch,
                "insufficient_funds",
                0,
            )
            .add_attribute("required_value", emitted_value)
            .add_attribute("available_balance", available_balance));
        }
        epoch.emitted_value = emitted_value;
        epoch.retained_value = retained_value;
        if message_count == 0 {
            epoch.outcome = EpochOutcome::NoEligibleOptions;
        } else {
            epoch.outcome = EpochOutcome::Distributed { message_count };
        }
        gauge.next_epoch = now;
        EPOCHS.save(deps.storage, (gauge_id, epoch_id), &epoch)?;
        GAUGES.save(deps.storage, gauge_id, &gauge)?;
        let outcome = if message_count == 0 {
            "no_eligible_options"
        } else {
            "distributed"
        };
        let mut response = snapshot_terminal_response(
            "execute_snapshot_epoch",
            &sender,
            &epoch,
            outcome,
            message_count,
        );
        if message_count > 0 {
            response = response.add_message(WasmMsg::Execute {
                contract_addr: config.dao_core.to_string(),
                msg: to_json_binary(&DaoExecuteMsg::ExecuteProposalHook {
                    msgs: adapter_response.execute,
                })?,
                funds: vec![],
            });
        }
        Ok(response)
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => Ok(to_json_binary(&query::config(deps)?)?),
        QueryMsg::Info {} => Ok(to_json_binary(&query::info(deps)?)?),
        QueryMsg::Gauge { id } => Ok(to_json_binary(&query::gauge(deps, id)?)?),
        QueryMsg::ListGauges { start_after, limit } => Ok(to_json_binary(&query::list_gauges(
            deps,
            start_after,
            limit,
        )?)?),
        QueryMsg::Vote { gauge, voter } => Ok(to_json_binary(&query::vote(deps, gauge, voter)?)?),
        QueryMsg::ListVotes {
            gauge,
            start_after,
            limit,
        } => Ok(to_json_binary(&query::list_votes(
            deps,
            gauge,
            start_after,
            limit,
        )?)?),
        QueryMsg::ListOptions {
            gauge,
            start_after,
            limit,
        } => Ok(to_json_binary(&query::list_options(
            deps,
            gauge,
            start_after,
            limit,
        )?)?),
        QueryMsg::SelectedSet { gauge } => Ok(to_json_binary(&query::selected_set(deps, gauge)?)?),
        QueryMsg::LastExecutedSet { gauge } => {
            Ok(to_json_binary(&query::last_executed_set(deps, gauge)?)?)
        }
        QueryMsg::GaugeHealth { gauge } => Ok(to_json_binary(&query::gauge_health(deps, gauge)?)?),
        QueryMsg::GetHooks {} => Ok(to_json_binary(&GetHooksResponse {
            hooks: VOTE_HOOKS.query_hooks(deps)?.hooks,
        })?),
        QueryMsg::Epoch { gauge, epoch } => Ok(to_json_binary(&query::epoch(deps, gauge, epoch)?)?),
        QueryMsg::ListEpochs {
            gauge,
            start_after,
            limit,
        } => Ok(to_json_binary(&query::list_epochs(
            deps,
            gauge,
            start_after,
            limit,
        )?)?),
        QueryMsg::EpochBallot {
            gauge,
            epoch,
            voter,
        } => Ok(to_json_binary(&query::epoch_ballot(
            deps, gauge, epoch, voter,
        )?)?),
        QueryMsg::ListEpochBallots {
            gauge,
            epoch,
            start_after,
            limit,
        } => Ok(to_json_binary(&query::list_epoch_ballots(
            deps,
            gauge,
            epoch,
            start_after,
            limit,
        )?)?),
        QueryMsg::EpochAllocations {
            gauge,
            epoch,
            start_after,
            limit,
        } => Ok(to_json_binary(&query::epoch_allocations(
            deps,
            gauge,
            epoch,
            start_after,
            limit,
        )?)?),
    }
}

/// Cleans up a stable vote-hook reply association and auto-unregisters only
/// the subscriber whose call failed.
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn reply(deps: DepsMut, _env: Env, msg: Reply) -> Result<Response, ContractError> {
    let hook = VOTE_HOOK_REPLIES
        .may_load(deps.storage, msg.id)?
        .ok_or(ContractError::UnknownVoteHookReply(msg.id))?;
    VOTE_HOOK_REPLIES.remove(deps.storage, msg.id);

    if msg.result.is_ok() {
        return Ok(Response::new()
            .add_attribute("action", "vote_hook_succeeded")
            .add_attribute("hook", hook)
            .add_attribute("reply_id", msg.id.to_string()));
    }

    VOTE_HOOKS.remove_hook(deps.storage, hook.clone())?;
    Ok(Response::new()
        .add_attribute("action", "remove_failed_vote_hook")
        .add_attribute("hook", hook)
        .add_attribute("reply_id", msg.id.to_string()))
}

mod query {
    use super::*;

    use crate::msg::{GaugeHealthResponse, LastExecutedSetResponse, VoteInfo, VoteResponse};
    use dao_interface::voting::InfoResponse;

    pub fn info(deps: Deps) -> StdResult<InfoResponse> {
        let info = cw2::get_contract_version(deps.storage)?;
        Ok(InfoResponse { info })
    }

    pub fn config(deps: Deps) -> StdResult<ConfigResponse> {
        let config = CONFIG.load(deps.storage)?;
        let hook_caller = config.hook_caller.to_string();
        let power_source = match load_power_source(deps.storage)? {
            PowerSource::Hook => PowerSourceResponse::Hook {
                hook_caller: hook_caller.clone(),
            },
            PowerSource::EpochSnapshot { guardian } => PowerSourceResponse::EpochSnapshot {
                guardian: guardian.into_string(),
            },
        };
        Ok(ConfigResponse {
            owner: config.owner.into_string(),
            dao_core: config.dao_core.into_string(),
            voting_powers: config.voting_powers.into_string(),
            hook_caller,
            power_source,
        })
    }

    fn to_gauge_response(deps: Deps, gauge_id: GaugeId, gauge: Gauge) -> StdResult<GaugeResponse> {
        Ok(GaugeResponse {
            id: gauge_id,
            title: gauge.title,
            adapter: gauge.adapter.to_string(),
            epoch_size: gauge.epoch,
            min_percent_selected: gauge.min_percent_selected,
            max_options_selected: gauge.max_options_selected,
            max_available_percentage: gauge.max_available_percentage,
            is_stopped: gauge.is_stopped,
            next_epoch: gauge.next_epoch,
            reset: gauge.reset,
            snapshot_policy: SNAPSHOT_POLICIES.may_load(deps.storage, gauge_id)?,
            current_epoch: CURRENT_EPOCH.may_load(deps.storage, gauge_id)?,
        })
    }

    pub fn gauge(deps: Deps, gauge_id: GaugeId) -> StdResult<GaugeResponse> {
        let gauge = GAUGES.load(deps.storage, gauge_id)?;
        to_gauge_response(deps, gauge_id, gauge)
    }

    // settings for pagination
    pub const MAX_LIMIT: u32 = 100;
    pub const DEFAULT_LIMIT: u32 = 30;

    pub fn epoch(
        deps: Deps,
        gauge_id: GaugeId,
        epoch_id: u64,
    ) -> StdResult<crate::msg::EpochResponse> {
        Ok(EPOCHS.load(deps.storage, (gauge_id, epoch_id))?.response())
    }

    pub fn list_epochs(
        deps: Deps,
        gauge_id: GaugeId,
        start_after: Option<u64>,
        limit: Option<u32>,
    ) -> StdResult<ListEpochsResponse> {
        let epochs = EPOCHS
            .prefix(gauge_id)
            .range(
                deps.storage,
                start_after.map(Bound::exclusive),
                None,
                Order::Ascending,
            )
            .take(limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize)
            .map(|item| item.map(|(_, epoch)| epoch.response()))
            .collect::<StdResult<Vec<_>>>()?;
        Ok(ListEpochsResponse { epochs })
    }

    fn ballot_info(ballot: SnapshotBallot) -> EpochBallotInfo {
        EpochBallotInfo {
            voter: ballot.voter.into_string(),
            power: ballot.power,
            votes: ballot.votes,
            cast_at: ballot.cast_at,
            revised_at: ballot.revised_at,
            revisions: ballot.revisions,
            receipt_index: ballot.receipt_index,
        }
    }

    pub fn epoch_ballot(
        deps: Deps,
        gauge_id: GaugeId,
        epoch_id: u64,
        voter: String,
    ) -> StdResult<EpochBallotResponse> {
        EPOCHS.load(deps.storage, (gauge_id, epoch_id))?;
        let voter = deps.api.addr_validate(&voter)?;
        let ballot = EPOCH_BALLOTS
            .may_load(deps.storage, (gauge_id, epoch_id, &voter))?
            .map(ballot_info);
        Ok(EpochBallotResponse { ballot })
    }

    pub fn list_epoch_ballots(
        deps: Deps,
        gauge_id: GaugeId,
        epoch_id: u64,
        start_after: Option<u32>,
        limit: Option<u32>,
    ) -> StdResult<ListEpochBallotsResponse> {
        let epoch = EPOCHS.load(deps.storage, (gauge_id, epoch_id))?;
        let scan_limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;
        let indexed = EPOCH_BALLOT_INDEX
            .prefix((gauge_id, epoch_id))
            .range(
                deps.storage,
                start_after.map(Bound::exclusive),
                None,
                Order::Ascending,
            )
            .take(scan_limit)
            .collect::<StdResult<Vec<_>>>()?;
        let mut ballots = Vec::with_capacity(indexed.len());
        let mut last_scanned = None;
        for (receipt_index, voter) in indexed {
            last_scanned = Some(receipt_index);
            if let Some(ballot) =
                EPOCH_BALLOTS.may_load(deps.storage, (gauge_id, epoch_id, &voter))?
            {
                ballots.push(ballot_info(ballot));
            }
        }
        let next_start_after = last_scanned.filter(|index| *index < epoch.receipt_count);
        Ok(ListEpochBallotsResponse {
            ballots,
            next_start_after,
        })
    }

    pub fn epoch_allocations(
        deps: Deps,
        gauge_id: GaugeId,
        epoch_id: u64,
        start_after: Option<String>,
        limit: Option<u32>,
    ) -> StdResult<EpochAllocationsResponse> {
        EPOCHS.load(deps.storage, (gauge_id, epoch_id))?;
        let allocations = EPOCH_TALLY
            .prefix((gauge_id, epoch_id))
            .range(
                deps.storage,
                start_after.as_deref().map(Bound::exclusive),
                None,
                Order::Ascending,
            )
            .take(limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize)
            .map(|item| item.map(|(option, power)| (option, Uint128::new(power))))
            .collect::<StdResult<Vec<_>>>()?;
        Ok(EpochAllocationsResponse { allocations })
    }

    pub fn list_gauges(
        deps: Deps,
        start_after: Option<u64>,
        limit: Option<u32>,
    ) -> StdResult<ListGaugesResponse> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;
        let start = start_after.map(Bound::exclusive);

        Ok(ListGaugesResponse {
            gauges: GAUGES
                .range(deps.storage, start, None, Order::Ascending)
                .map(|item| {
                    let (id, gauge) = item?;
                    to_gauge_response(deps, id, gauge)
                })
                .take(limit)
                .collect::<StdResult<Vec<GaugeResponse>>>()?,
        })
    }

    pub fn vote(deps: Deps, gauge_id: u64, voter: String) -> StdResult<VoteResponse> {
        let voter_addr = deps.api.addr_validate(&voter)?;
        let gauge = GAUGES.load(deps.storage, gauge_id)?;

        let vote = votes()
            .may_load(deps.storage, &voter_addr, gauge_id)?
            .filter(|v| !v.is_expired(&gauge))
            .map(|v| VoteInfo {
                voter,
                votes: v.votes,
                cast: v.cast,
            });
        Ok(VoteResponse { vote })
    }

    pub fn list_votes(
        deps: Deps,
        gauge_id: u64,
        start_after: Option<String>,
        limit: Option<u32>,
    ) -> StdResult<ListVotesResponse> {
        Ok(ListVotesResponse {
            votes: votes().query_votes_by_gauge(deps, gauge_id, start_after, limit)?,
        })
    }

    pub fn list_options(
        deps: Deps,
        gauge_id: u64,
        start_after: Option<String>,
        limit: Option<u32>,
    ) -> StdResult<ListOptionsResponse> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;
        let start_after = start_after.as_ref().map(|s| Bound::exclusive(s.as_str()));

        Ok(ListOptionsResponse {
            options: TALLY
                .prefix(gauge_id)
                .range(deps.storage, start_after, None, Order::Ascending)
                .filter(|item| match item {
                    Ok((option, _)) => {
                        !INVALID_OPTIONS.has(deps.storage, (gauge_id, option.as_str()))
                    }
                    Err(_) => true,
                })
                .map(|option| {
                    let (option, power) = option?;
                    Ok((option, Uint128::new(power)))
                })
                .take(limit)
                .collect::<StdResult<Vec<(String, Uint128)>>>()?,
        })
    }

    pub fn gauge_health(deps: Deps, gauge_id: GaugeId) -> StdResult<GaugeHealthResponse> {
        GAUGES.load(deps.storage, gauge_id)?;

        let options = TALLY
            .prefix(gauge_id)
            .range(deps.storage, None, None, Order::Ascending)
            .take(MAX_OPTIONS_PER_GAUGE + 1)
            .collect::<StdResult<Vec<_>>>()?;
        let scan_complete = options.len() <= MAX_OPTIONS_PER_GAUGE;
        let options = options
            .into_iter()
            .take(MAX_OPTIONS_PER_GAUGE)
            .collect::<Vec<_>>();

        let mut tally_sum = Uint128::zero();
        let mut active_option_count = 0u32;
        let mut invalid_option_count = 0u32;
        let mut mismatch_count = 0u32;
        let mut first_mismatch = None;
        for (option, points) in &options {
            tally_sum = tally_sum
                .checked_add(Uint128::new(*points))
                .map_err(StdError::overflow)?;
            let invalid = INVALID_OPTIONS.has(deps.storage, (gauge_id, option.as_str()));
            if invalid {
                invalid_option_count += 1;
            } else {
                active_option_count += 1;
            }
            let indexed = OPTION_BY_POINTS.has(deps.storage, (gauge_id, *points, option));
            if indexed == invalid {
                mismatch_count += 1;
                first_mismatch.get_or_insert_with(|| option.clone());
            }
        }

        let index_entries = OPTION_BY_POINTS
            .sub_prefix(gauge_id)
            .range(deps.storage, None, None, Order::Ascending)
            .take(MAX_OPTIONS_PER_GAUGE + 1)
            .collect::<StdResult<Vec<_>>>()?;
        let index_scan_complete = index_entries.len() <= MAX_OPTIONS_PER_GAUGE;
        let indexed_option_count = index_entries.len().min(MAX_OPTIONS_PER_GAUGE) as u32;
        for ((points, option), _) in index_entries.into_iter().take(MAX_OPTIONS_PER_GAUGE) {
            let valid = TALLY
                .may_load(deps.storage, (gauge_id, option.as_str()))?
                .is_some_and(|stored| stored == points)
                && !INVALID_OPTIONS.has(deps.storage, (gauge_id, option.as_str()));
            if !valid {
                mismatch_count += 1;
                first_mismatch.get_or_insert(option);
            }
        }

        let total_cast = Uint128::new(TOTAL_CAST.load(deps.storage, gauge_id)?);
        let scan_complete = scan_complete && index_scan_complete;
        let consistent = scan_complete
            && mismatch_count == 0
            && tally_sum == total_cast
            && indexed_option_count == active_option_count;
        Ok(GaugeHealthResponse {
            gauge_id,
            option_count: options.len() as u32,
            active_option_count,
            invalid_option_count,
            indexed_option_count,
            tally_sum,
            total_cast,
            mismatch_count,
            first_mismatch,
            reset_cursor: RESET_CURSOR.may_load(deps.storage, gauge_id)?,
            scan_complete,
            consistent,
        })
    }

    pub fn selected_set(deps: Deps, gauge_id: u64) -> StdResult<SelectedSetResponse> {
        let gauge = GAUGES.load(deps.storage, gauge_id)?;
        let total_cast = TOTAL_CAST.load(deps.storage, gauge_id)?;

        if gauge.is_resetting() || total_cast == 0 {
            return Ok(SelectedSetResponse { votes: vec![] });
        }

        // This is sorted index, but requires manual filtering - cannot be prefixed
        // given our requirements. Storage iteration errors are not consumed
        // here; they pass through the filter and are propagated by the `?`
        // inside the `.map(...)` below.
        let candidates = OPTION_BY_POINTS
            .sub_prefix(gauge_id)
            .range(deps.storage, None, None, Order::Descending)
            .filter(|item| match item {
                Ok(((power, _), _)) => {
                    if let Some(min_percent_selected) = gauge.min_percent_selected {
                        Decimal::from_ratio(*power, total_cast) >= min_percent_selected
                    } else {
                        // filter out options without a vote
                        *power != 0u128
                    }
                }
                // Let errors through so they propagate via `?` in the map.
                Err(_) => true,
            })
            .map(|o| {
                let ((power, option), _) = o?;
                // If gauge has max_available_percentage set, discard all power
                // above that percentage
                if let Some(max_available_percentage) = gauge.max_available_percentage {
                    // Equality produces the same power either way; make the
                    // inclusive boundary explicit and deterministic.
                    if Decimal::from_ratio(power, total_cast) >= max_available_percentage {
                        // If power is above available percentage, cut power down to max available
                        return Ok((option, Uint128::new(total_cast) * max_available_percentage));
                    }
                }
                Ok((option, Uint128::new(power)))
            })
            // The option namespace is contract-bounded. Scan at most that
            // bound here, then apply `max_options_selected` after adapter
            // validity filtering so rejected leaders cannot consume slots
            // that should go to valid lower-ranked candidates.
            .take(MAX_OPTIONS_PER_GAUGE)
            .collect::<StdResult<Vec<(String, Uint128)>>>()?;

        // Pull-sync adapter validity for the bounded candidate set. This
        // prevents a marketing rejection/removal from remaining payable even
        // when the orchestrator still retains its local tally tombstone.
        let mut votes = candidates
            .into_iter()
            .map(|(option, power)| {
                let validity: CheckOptionResponse = deps.querier.query_wasm_smart(
                    gauge.adapter.clone(),
                    &AdapterQueryMsg::CheckOption {
                        option: option.clone(),
                    },
                )?;
                Ok(validity.valid.then_some((option, power)))
            })
            .collect::<StdResult<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        // Tiny caps can round otherwise qualifying entries to zero. Exclude
        // them so execution becomes the documented no-op rather than emitting
        // zero-amount adapter messages.
        votes.retain(|(_, power)| !power.is_zero());
        votes.truncate(gauge.max_options_selected as usize);

        Ok(SelectedSetResponse { votes })
    }

    pub fn last_executed_set(deps: Deps, gauge_id: u64) -> StdResult<LastExecutedSetResponse> {
        let gauge = GAUGES.load(deps.storage, gauge_id)?;
        Ok(LastExecutedSetResponse {
            votes: gauge.last_executed_set,
        })
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(deps: DepsMut, env: Env, msg: MigrateMsg) -> Result<Response, ContractError> {
    let previous = get_contract_version(deps.storage)?;
    if previous.contract != CONTRACT_NAME {
        return Err(StdError::generic_err(format!(
            "cannot migrate contract {}; expected {CONTRACT_NAME}",
            previous.contract
        ))
        .into());
    }
    let from = semver::Version::parse(&previous.version)
        .map_err(|error| StdError::generic_err(format!("invalid stored version: {error}")))?;
    let to = semver::Version::parse(CONTRACT_VERSION)
        .map_err(|error| StdError::generic_err(format!("invalid target version: {error}")))?;
    if from >= to {
        return Err(StdError::generic_err(format!(
            "migration requires an older version; stored {from}, target {to}"
        ))
        .into());
    }
    if !SUPPORTED_MIGRATION_SOURCES.contains(&previous.version.as_str()) {
        return Err(ContractError::UnsupportedMigrationSource {
            version: previous.version,
        });
    }
    let gauge_configs = msg.gauge_config.unwrap_or_default();
    if gauge_configs.len() > MAX_GAUGES as usize {
        return Err(ContractError::TooManyGaugeMigrationConfigs {
            count: gauge_configs.len(),
            max: MAX_GAUGES as usize,
        });
    }
    let migrated_records = gauge_configs.len();
    let mut seen = std::collections::HashSet::with_capacity(migrated_records);
    let mut updates = Vec::with_capacity(migrated_records);
    for (gauge_id, config) in gauge_configs {
        if !seen.insert(gauge_id) {
            return Err(ContractError::DuplicateGaugeMigrationConfig { gauge_id });
        }
        let mut gauge = GAUGES
            .may_load(deps.storage, gauge_id)?
            .ok_or(StdError::NotFound {
                kind: format!("Gauge with id {}", gauge_id),
            })?;
        if let Some(next_epoch) = config.next_epoch {
            if next_epoch < env.block.time.seconds() {
                return Err(StdError::GenericErr {
                    msg: "Next epoch value cannot be earlier then current epoch!".to_owned(),
                }
                .into());
            }
            gauge.next_epoch = next_epoch;
        }
        if let Some(reset_config) = config.reset {
            if reset_config.reset_epoch == 0 {
                return Err(StdError::GenericErr {
                    msg: "Reset epoch must be greater than zero".to_owned(),
                }
                .into());
            }
            if reset_config.next_reset < env.block.time.seconds() {
                return Err(StdError::GenericErr {
                    msg: "Next reset value cannot be earlier then current epoch!".to_owned(),
                }
                .into());
            }
            gauge.reset = Some(Reset {
                last: gauge.reset.map(|r| r.last).unwrap_or_default(),
                reset_each: reset_config.reset_epoch,
                next: reset_config.next_reset,
            });
        }
        updates.push((gauge_id, gauge));
    }

    // All application-level validation is complete before cw2 or gauge state
    // is changed, which also makes direct unit tests observe atomic failures.
    ensure_from_older_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    POWER_SOURCE.save(deps.storage, &PowerSource::Hook)?;
    for (gauge_id, gauge) in updates {
        GAUGES.save(deps.storage, gauge_id, &gauge)?;
    }

    Ok(Response::new()
        .add_attribute("action", "migrate")
        .add_attribute("from_version", previous.version)
        .add_attribute("to_version", CONTRACT_VERSION)
        .add_attribute("migrated_records", migrated_records.to_string()))
}

#[cfg(test)]
mod migration_tests {
    use super::*;
    use crate::{
        msg::GaugeMigrationConfig,
        state::{Vote, WeightedVotes},
    };
    use cosmwasm_std::testing::{mock_dependencies, mock_env, mock_info};

    fn sample_gauge(next_epoch: u64) -> Gauge {
        Gauge {
            title: "populated".to_owned(),
            adapter: Addr::unchecked("adapter"),
            epoch: 604_800,
            min_percent_selected: Some(Decimal::percent(5)),
            max_options_selected: 10,
            max_available_percentage: None,
            is_stopped: false,
            next_epoch,
            last_executed_set: Some(vec![("option".to_owned(), Uint128::new(25))]),
            reset: None,
        }
    }

    #[test]
    fn migration_preserves_populated_state_and_reports_versions() {
        let mut deps = mock_dependencies();
        let env = mock_env();
        instantiate(
            deps.as_mut(),
            env.clone(),
            mock_info("dao", &[]),
            InstantiateMsg {
                voting_powers: "powers".to_owned(),
                hook_caller: "hook".to_owned(),
                epoch_snapshot: None,
                owner: "owner".to_owned(),
                gauges: None,
            },
        )
        .unwrap();
        let old_epoch = env.block.time.seconds() + 100;
        let new_epoch = env.block.time.seconds() + 200;
        GAUGES
            .save(deps.as_mut().storage, 0, &sample_gauge(old_epoch))
            .unwrap();
        let voter = Addr::unchecked("voter");
        let weighted = WeightedVotes {
            gauge_id: 0,
            power: Uint128::new(25),
            votes: vec![Vote {
                option: "option".to_owned(),
                weight: Decimal::one(),
            }],
            cast: Some(env.block.time.seconds()),
        };
        votes()
            .save(deps.as_mut().storage, &voter, 0, &weighted)
            .unwrap();
        TALLY
            .save(deps.as_mut().storage, (0, "option"), &25)
            .unwrap();
        TOTAL_CAST.save(deps.as_mut().storage, 0, &25).unwrap();
        OPTION_BY_POINTS
            .save(deps.as_mut().storage, (0, 25, "option"), &1)
            .unwrap();
        // Historical deployments have no explicit power-source item.
        POWER_SOURCE.remove(deps.as_mut().storage);
        set_contract_version(deps.as_mut().storage, CONTRACT_NAME, "2.5.0").unwrap();

        let response = migrate(
            deps.as_mut(),
            env,
            MigrateMsg {
                gauge_config: Some(vec![(
                    0,
                    GaugeMigrationConfig {
                        next_epoch: Some(new_epoch),
                        reset: None,
                    },
                )]),
            },
        )
        .unwrap();

        assert_eq!(
            GAUGES.load(deps.as_ref().storage, 0).unwrap().next_epoch,
            new_epoch
        );
        assert_eq!(
            votes().load(deps.as_ref().storage, &voter, 0).unwrap(),
            weighted
        );
        assert_eq!(
            TALLY.load(deps.as_ref().storage, (0, "option")).unwrap(),
            25
        );
        assert_eq!(TOTAL_CAST.load(deps.as_ref().storage, 0).unwrap(), 25);
        assert!(OPTION_BY_POINTS.has(deps.as_ref().storage, (0, 25, "option")));
        assert_eq!(
            POWER_SOURCE.load(deps.as_ref().storage).unwrap(),
            PowerSource::Hook
        );
        assert!(response
            .attributes
            .iter()
            .any(|attribute| { attribute.key == "from_version" && attribute.value == "2.5.0" }));
        assert!(response
            .attributes
            .iter()
            .any(|attribute| { attribute.key == "migrated_records" && attribute.value == "1" }));
    }

    #[test]
    fn migration_rejects_identity_versions_and_unbounded_or_duplicate_configs() {
        for (contract, version) in [
            ("wrong-contract", "0.1.0"),
            (CONTRACT_NAME, "2.4.1"),
            (CONTRACT_NAME, CONTRACT_VERSION),
            (CONTRACT_NAME, "99.0.0"),
        ] {
            let mut deps = mock_dependencies();
            set_contract_version(deps.as_mut().storage, contract, version).unwrap();
            assert!(migrate(deps.as_mut(), mock_env(), MigrateMsg { gauge_config: None }).is_err());
            assert_eq!(
                get_contract_version(deps.as_ref().storage).unwrap().version,
                version
            );
        }

        // The exact migration bound is accepted, and timestamps equal to the
        // current block are valid for both epoch and reset schedules.
        let mut exact = mock_dependencies();
        let env = mock_env();
        set_contract_version(exact.as_mut().storage, CONTRACT_NAME, "2.5.0").unwrap();
        for id in 0..MAX_GAUGES {
            GAUGES
                .save(exact.as_mut().storage, id, &sample_gauge(u64::MAX))
                .unwrap();
        }
        let exact_configs = (0..MAX_GAUGES)
            .map(|id| {
                let config = if id == 0 {
                    GaugeMigrationConfig {
                        next_epoch: Some(env.block.time.seconds()),
                        reset: Some(crate::msg::ResetMigrationConfig {
                            reset_epoch: 1,
                            next_reset: env.block.time.seconds(),
                        }),
                    }
                } else {
                    GaugeMigrationConfig::default()
                };
                (id, config)
            })
            .collect();
        let response = migrate(
            exact.as_mut(),
            env.clone(),
            MigrateMsg {
                gauge_config: Some(exact_configs),
            },
        )
        .unwrap();
        assert!(response.attributes.iter().any(|attribute| {
            attribute.key == "migrated_records" && attribute.value == MAX_GAUGES.to_string()
        }));
        let gauge = GAUGES.load(exact.as_ref().storage, 0).unwrap();
        assert_eq!(gauge.next_epoch, env.block.time.seconds());
        assert_eq!(gauge.reset.unwrap().next, env.block.time.seconds());

        let mut deps = mock_dependencies();
        set_contract_version(deps.as_mut().storage, CONTRACT_NAME, "2.5.0").unwrap();
        let oversized = (0..=MAX_GAUGES)
            .map(|id| (id, GaugeMigrationConfig::default()))
            .collect();
        assert_eq!(
            migrate(
                deps.as_mut(),
                mock_env(),
                MigrateMsg {
                    gauge_config: Some(oversized),
                },
            )
            .unwrap_err(),
            ContractError::TooManyGaugeMigrationConfigs {
                count: MAX_GAUGES as usize + 1,
                max: MAX_GAUGES as usize,
            }
        );

        GAUGES
            .save(deps.as_mut().storage, 0, &sample_gauge(u64::MAX))
            .unwrap();
        assert_eq!(
            migrate(
                deps.as_mut(),
                mock_env(),
                MigrateMsg {
                    gauge_config: Some(vec![
                        (0, GaugeMigrationConfig::default()),
                        (0, GaugeMigrationConfig::default()),
                    ]),
                },
            )
            .unwrap_err(),
            ContractError::DuplicateGaugeMigrationConfig { gauge_id: 0 }
        );
        assert_eq!(
            get_contract_version(deps.as_ref().storage).unwrap().version,
            "2.5.0"
        );

        let env = mock_env();
        GAUGES
            .save(deps.as_mut().storage, 1, &sample_gauge(u64::MAX - 1))
            .unwrap();
        let err = migrate(
            deps.as_mut(),
            env.clone(),
            MigrateMsg {
                gauge_config: Some(vec![
                    (
                        0,
                        GaugeMigrationConfig {
                            next_epoch: Some(env.block.time.seconds() + 10),
                            reset: None,
                        },
                    ),
                    (
                        1,
                        GaugeMigrationConfig {
                            next_epoch: Some(env.block.time.seconds() - 1),
                            reset: None,
                        },
                    ),
                ]),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("Next epoch value"));
        assert_eq!(
            GAUGES.load(deps.as_ref().storage, 0).unwrap().next_epoch,
            u64::MAX
        );
        assert_eq!(
            GAUGES.load(deps.as_ref().storage, 1).unwrap().next_epoch,
            u64::MAX - 1
        );
        assert_eq!(
            get_contract_version(deps.as_ref().storage).unwrap().version,
            "2.5.0"
        );
    }

    #[test]
    fn migration_accepts_supported_2_4_2_populated_state() {
        let mut deps = mock_dependencies();
        let gauge = sample_gauge(1_234);
        GAUGES.save(deps.as_mut().storage, 0, &gauge).unwrap();
        TALLY
            .save(deps.as_mut().storage, (0, "option"), &42)
            .unwrap();
        TOTAL_CAST.save(deps.as_mut().storage, 0, &42).unwrap();
        OPTION_BY_POINTS
            .save(deps.as_mut().storage, (0, 42, "option"), &1)
            .unwrap();
        set_contract_version(deps.as_mut().storage, CONTRACT_NAME, "2.4.2").unwrap();

        let response =
            migrate(deps.as_mut(), mock_env(), MigrateMsg { gauge_config: None }).unwrap();
        assert_eq!(GAUGES.load(deps.as_ref().storage, 0).unwrap(), gauge);
        assert_eq!(
            TALLY.load(deps.as_ref().storage, (0, "option")).unwrap(),
            42
        );
        assert_eq!(TOTAL_CAST.load(deps.as_ref().storage, 0).unwrap(), 42);
        assert!(OPTION_BY_POINTS.has(deps.as_ref().storage, (0, 42, "option")));
        assert!(response
            .attributes
            .iter()
            .any(|attribute| attribute.key == "from_version" && attribute.value == "2.4.2"));
    }
}
