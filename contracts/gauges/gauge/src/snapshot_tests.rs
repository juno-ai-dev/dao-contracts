use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use cosmwasm_std::{
    coin, from_json,
    testing::{mock_dependencies, mock_env, mock_info, MockApi, MockQuerier, MockStorage},
    to_json_binary, Addr, BankMsg, ContractResult, CosmosMsg, Decimal, Empty, Env, OwnedDeps,
    QuerierResult, Response, SystemResult, Uint128, WasmQuery,
};
use dao_hooks::stake::StakeChangedHookMsg;
use dao_interface::voting::{
    Query as DaoQuery, TotalPowerAtHeightResponse, VotingPowerAtHeightResponse,
};
use proptest::prelude::*;

use crate::state::Vote;
use crate::{
    contract::{execute, instantiate, query},
    msg::{
        AdapterQueryMsg, AllOptionsResponse, CheckOptionResponse, EpochAllocationsResponse,
        EpochBallotResponse, EpochOutcome, EpochResponse, EpochSnapshotModeConfig,
        EpochSnapshotPolicy, ExecuteMsg, GaugeConfig, GaugeResponse, InstantiateMsg,
        ListEpochBallotsResponse, ListEpochsResponse, QueryMsg, SampleGaugeMsgsResponse,
    },
    state::{
        CURRENT_EPOCH, EPOCHS, EPOCH_BALLOTS, EPOCH_BALLOT_INDEX, EPOCH_OPTIONS, EPOCH_TALLY,
        EPOCH_VOTER_POWER, GAUGES, NEXT_EPOCH_ID,
    },
    ContractError,
};

type TestDeps = OwnedDeps<MockStorage, MockApi, MockQuerier, Empty>;

fn assert_snapshot_event(response: &Response, action: &str, attribute_keys: &[&str]) {
    assert_eq!(
        response
            .attributes
            .iter()
            .map(|attribute| attribute.key.as_str())
            .collect::<Vec<_>>(),
        attribute_keys
    );
    assert_eq!(response.attributes[0].key, "action");
    assert_eq!(response.attributes[0].value, action);
    assert!(
        response
            .attributes
            .iter()
            .all(|attribute| !attribute.value.is_empty()),
        "snapshot event attributes must have nonempty wire values"
    );
}

fn attribute<'a>(response: &'a Response, key: &str) -> &'a str {
    response
        .attributes
        .iter()
        .find(|attribute| attribute.key == key)
        .map(|attribute| attribute.value.as_str())
        .unwrap_or_else(|| panic!("missing response attribute {key}"))
}

#[derive(Clone, Debug, PartialEq)]
struct SampleContext {
    selected: Vec<(String, Decimal)>,
    epoch_budget: Option<Uint128>,
    available_balance: Option<Uint128>,
    denom: Option<String>,
}

#[derive(Default)]
struct QueryState {
    totals: HashMap<u64, Uint128>,
    powers: HashMap<(String, u64), Uint128>,
    total_queries: Vec<u64>,
    voter_queries: Vec<(String, u64)>,
    options: Vec<String>,
    invalid_options: HashSet<String>,
    samples: Vec<SampleContext>,
    fail_total: bool,
    mismatch_total_height: bool,
    fail_sample: bool,
    oversized_sample: bool,
    sample_emitted_value: Option<Uint128>,
    omit_sample_accounting: bool,
    corrupt_sample_accounting: bool,
}

fn query_ok<T: serde::Serialize>(response: &T) -> QuerierResult {
    SystemResult::Ok(ContractResult::Ok(to_json_binary(response).unwrap()))
}

fn install_queries(deps: &mut TestDeps, state: Arc<Mutex<QueryState>>) {
    deps.querier.update_wasm(move |query| {
        let WasmQuery::Smart { contract_addr, msg } = query else {
            return SystemResult::Ok(ContractResult::Err("unsupported wasm query".to_owned()));
        };
        if contract_addr == "powers" {
            let query: DaoQuery = from_json(msg).unwrap();
            let mut state = state.lock().unwrap();
            return match query {
                DaoQuery::TotalPowerAtHeight {
                    height: Some(height),
                } => {
                    state.total_queries.push(height);
                    if state.fail_total {
                        SystemResult::Ok(ContractResult::Err(
                            "historical total unavailable".to_owned(),
                        ))
                    } else {
                        let actual_height = if state.mismatch_total_height {
                            height.saturating_sub(1)
                        } else {
                            height
                        };
                        query_ok(&TotalPowerAtHeightResponse {
                            power: state.totals.get(&height).copied().unwrap_or_default(),
                            height: actual_height,
                        })
                    }
                }
                DaoQuery::VotingPowerAtHeight {
                    address,
                    height: Some(height),
                } => {
                    state.voter_queries.push((address.clone(), height));
                    query_ok(&VotingPowerAtHeightResponse {
                        power: state
                            .powers
                            .get(&(address, height))
                            .copied()
                            .unwrap_or_default(),
                        height,
                    })
                }
                _ => SystemResult::Ok(ContractResult::Err(
                    "snapshot query omitted its height".to_owned(),
                )),
            };
        }

        if contract_addr == "adapter" {
            let query: AdapterQueryMsg = from_json(msg).unwrap();
            let mut state = state.lock().unwrap();
            return match query {
                AdapterQueryMsg::AllOptions { start_after, limit } => {
                    let start = start_after
                        .and_then(|cursor| {
                            state
                                .options
                                .iter()
                                .position(|option| option == &cursor)
                                .map(|position| position + 1)
                        })
                        .unwrap_or_default();
                    let options = state
                        .options
                        .iter()
                        .skip(start)
                        .take(limit.unwrap_or(30) as usize)
                        .cloned()
                        .collect();
                    query_ok(&AllOptionsResponse { options })
                }
                AdapterQueryMsg::CheckOption { option } => query_ok(&CheckOptionResponse {
                    valid: state.options.contains(&option)
                        && !state.invalid_options.contains(&option),
                }),
                AdapterQueryMsg::SampleGaugeMsgs {
                    selected,
                    epoch_budget,
                    available_balance,
                    denom,
                } => {
                    state.samples.push(SampleContext {
                        selected,
                        epoch_budget,
                        available_balance,
                        denom,
                    });
                    if state.fail_sample {
                        SystemResult::Ok(ContractResult::Err("adapter failed".to_owned()))
                    } else {
                        let emitted_value = state.sample_emitted_value.unwrap_or(Uint128::new(1));
                        let message_count = if state.oversized_sample {
                            101
                        } else if emitted_value.is_zero() {
                            0
                        } else {
                            1
                        };
                        let retained_value = if state.corrupt_sample_accounting {
                            Uint128::zero()
                        } else {
                            epoch_budget.unwrap().checked_sub(emitted_value).unwrap()
                        };
                        query_ok(&SampleGaugeMsgsResponse {
                            execute: (0..message_count)
                                .map(|_| {
                                    CosmosMsg::Bank(BankMsg::Send {
                                        to_address: "project".to_owned(),
                                        amount: vec![coin(1, "ujuno")],
                                    })
                                })
                                .collect(),
                            emitted_value: (!state.omit_sample_accounting).then_some(emitted_value),
                            retained_value: (!state.omit_sample_accounting)
                                .then_some(retained_value),
                        })
                    }
                }
            };
        }

        SystemResult::Ok(ContractResult::Err(format!(
            "unknown contract: {contract_addr}"
        )))
    });
}

fn setup_snapshot(
    min_turnout_bps: u16,
    height: u64,
    total: u128,
) -> (TestDeps, Env, Arc<Mutex<QueryState>>) {
    setup_snapshot_with_retained(min_turnout_bps, height, total, None)
}

fn setup_snapshot_with_retained(
    min_turnout_bps: u16,
    height: u64,
    total: u128,
    retained_option: Option<&str>,
) -> (TestDeps, Env, Arc<Mutex<QueryState>>) {
    let mut deps = mock_dependencies();
    let mut env = mock_env();
    env.block.height = height;
    let state = Arc::new(Mutex::new(QueryState {
        totals: HashMap::from([(height, Uint128::new(total))]),
        options: [Some("project-a"), Some("project-b"), retained_option]
            .into_iter()
            .flatten()
            .map(str::to_owned)
            .collect(),
        ..QueryState::default()
    }));
    install_queries(&mut deps, state.clone());
    deps.querier
        .update_balance("dao", vec![coin(9_000, "ujuno")]);

    instantiate(
        deps.as_mut(),
        env.clone(),
        mock_info("dao", &[]),
        InstantiateMsg {
            voting_powers: "powers".to_owned(),
            hook_caller: String::new(),
            epoch_snapshot: Some(EpochSnapshotModeConfig {
                guardian: "guardian".to_owned(),
            }),
            owner: "governor".to_owned(),
            gauges: Some(vec![GaugeConfig {
                title: "Hack Juno".to_owned(),
                adapter: "adapter".to_owned(),
                epoch_size: 100,
                min_percent_selected: None,
                max_options_selected: 10,
                max_available_percentage: None,
                reset_epoch: None,
                snapshot_policy: Some(EpochSnapshotPolicy {
                    min_turnout_bps,
                    epoch_budget: Uint128::new(1_000),
                    denom: "ujuno".to_owned(),
                    retained_option: retained_option.map(str::to_owned),
                    execution_window_seconds: 100,
                }),
            }]),
        },
    )
    .unwrap();

    (deps, env, state)
}

fn open(deps: &mut TestDeps, env: &Env) {
    let response = execute(
        deps.as_mut(),
        env.clone(),
        mock_info("keeper", &[]),
        ExecuteMsg::OpenEpoch { gauge: 0 },
    )
    .unwrap();
    assert_snapshot_event(
        &response,
        "open_snapshot_epoch",
        &[
            "action",
            "sender",
            "gauge_id",
            "epoch_id",
            "snapshot_height",
            "snapshot_total_power",
            "opens_at",
            "closes_at",
            "execution_deadline",
            "policy_version",
            "min_turnout_bps",
            "epoch_budget",
            "denom",
            "retained_option",
            "option_count",
        ],
    );
}

fn vote(deps: &mut TestDeps, env: &Env, voter: &str, option: &str, weight: Decimal) {
    let response = execute(
        deps.as_mut(),
        env.clone(),
        mock_info(voter, &[]),
        ExecuteMsg::PlaceVotes {
            gauge: 0,
            votes: Some(vec![Vote {
                option: option.to_owned(),
                weight,
            }]),
        },
    )
    .unwrap();
    assert_snapshot_event(
        &response,
        "place_snapshot_vote",
        &[
            "action",
            "sender",
            "gauge_id",
            "epoch_id",
            "snapshot_height",
            "voting_power",
            "option_count",
            "participating_power",
            "allocated_power",
            "total_cast",
            "retained_option_power",
            "unallocated_power",
        ],
    );
}

fn close(env: &mut Env) {
    env.block.time = env.block.time.plus_seconds(100);
}

#[test]
fn snapshot_power_is_fixed_across_partial_vote_revision_and_removal() {
    let (mut deps, env, state) = setup_snapshot(0, 123, 100);
    state
        .lock()
        .unwrap()
        .powers
        .insert(("alice".to_owned(), env.block.height), Uint128::new(60));
    open(&mut deps, &env);

    vote(&mut deps, &env, "alice", "project-a", Decimal::percent(50));
    let epoch = EPOCHS.load(deps.as_ref().storage, (0, 1)).unwrap();
    assert_eq!(epoch.snapshot_height, 123);
    assert_eq!(epoch.snapshot_total_power, Uint128::new(100));
    assert_eq!(epoch.participating_power, Uint128::new(60));
    assert_eq!(epoch.total_cast, Uint128::new(30));
    assert_eq!(
        EPOCH_TALLY
            .load(deps.as_ref().storage, (0, 1, "project-a"))
            .unwrap(),
        30
    );

    // Even a faulty mutable backend cannot change a power already fixed for
    // this epoch: revisions do not re-query the voter.
    state
        .lock()
        .unwrap()
        .powers
        .insert(("alice".to_owned(), env.block.height), Uint128::new(5));
    vote(&mut deps, &env, "alice", "project-b", Decimal::one());
    let epoch = EPOCHS.load(deps.as_ref().storage, (0, 1)).unwrap();
    let ballot = EPOCH_BALLOTS
        .load(deps.as_ref().storage, (0, 1, &Addr::unchecked("alice")))
        .unwrap();
    assert_eq!(ballot.power, Uint128::new(60));
    assert_eq!(ballot.revisions, 1);
    assert_eq!(epoch.participating_power, Uint128::new(60));
    assert_eq!(epoch.total_cast, Uint128::new(60));
    assert_eq!(
        EPOCH_TALLY
            .load(deps.as_ref().storage, (0, 1, "project-a"))
            .unwrap(),
        0
    );
    assert_eq!(
        EPOCH_TALLY
            .load(deps.as_ref().storage, (0, 1, "project-b"))
            .unwrap(),
        60
    );

    let response = execute(
        deps.as_mut(),
        env,
        mock_info("alice", &[]),
        ExecuteMsg::PlaceVotes {
            gauge: 0,
            votes: None,
        },
    )
    .unwrap();
    assert_snapshot_event(
        &response,
        "place_snapshot_vote",
        &[
            "action",
            "sender",
            "gauge_id",
            "epoch_id",
            "snapshot_height",
            "voting_power",
            "option_count",
            "participating_power",
            "allocated_power",
            "total_cast",
            "retained_option_power",
            "unallocated_power",
        ],
    );
    assert_eq!(attribute(&response, "option_count"), "0");
    assert_eq!(attribute(&response, "participating_power"), "0");
    assert_eq!(attribute(&response, "allocated_power"), "0");
    let epoch = EPOCHS.load(deps.as_ref().storage, (0, 1)).unwrap();
    assert_eq!(epoch.participating_power, Uint128::zero());
    assert_eq!(epoch.total_cast, Uint128::zero());
    assert!(!EPOCH_BALLOTS.has(deps.as_ref().storage, (0, 1, &Addr::unchecked("alice"))));
    let state = state.lock().unwrap();
    assert_eq!(state.total_queries, vec![123]);
    assert_eq!(state.voter_queries, vec![("alice".to_owned(), 123)]);
}

#[test]
fn ballot_pagination_advances_across_removed_ballot_holes() {
    let (mut deps, env, state) = setup_snapshot(0, 124, 1_000);
    {
        let mut state = state.lock().unwrap();
        for voter in ["alice", "bob", "carol"] {
            state
                .powers
                .insert((voter.to_owned(), 124), Uint128::new(100));
        }
    }
    open(&mut deps, &env);
    for voter in ["alice", "bob", "carol"] {
        vote(&mut deps, &env, voter, "project-a", Decimal::one());
    }
    execute(
        deps.as_mut(),
        env.clone(),
        mock_info("alice", &[]),
        ExecuteMsg::PlaceVotes {
            gauge: 0,
            votes: None,
        },
    )
    .unwrap();

    let first: ListEpochBallotsResponse = from_json(
        query(
            deps.as_ref(),
            env.clone(),
            QueryMsg::ListEpochBallots {
                gauge: 0,
                epoch: 1,
                start_after: None,
                limit: Some(2),
            },
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(first.ballots.len(), 1);
    assert_eq!(first.ballots[0].voter, "bob");
    assert_eq!(first.ballots[0].receipt_index, 2);
    assert_eq!(first.next_start_after, Some(2));

    let second: ListEpochBallotsResponse = from_json(
        query(
            deps.as_ref(),
            env,
            QueryMsg::ListEpochBallots {
                gauge: 0,
                epoch: 1,
                start_after: first.next_start_after,
                limit: Some(2),
            },
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(second.ballots.len(), 1);
    assert_eq!(second.ballots[0].voter, "carol");
    assert_eq!(second.ballots[0].receipt_index, 3);
    assert_eq!(second.next_start_after, None);
}

#[test]
fn snapshot_public_queries_expose_policy_ballots_allocations_outcomes_and_pagination() {
    let (mut deps, mut env, state) = setup_snapshot(0, 125, 100);
    {
        let mut state = state.lock().unwrap();
        state
            .powers
            .insert(("alice".to_owned(), 125), Uint128::new(60));
        state
            .powers
            .insert(("bobby".to_owned(), 125), Uint128::new(40));
    }
    open(&mut deps, &env);
    vote(&mut deps, &env, "alice", "project-a", Decimal::one());
    vote(&mut deps, &env, "bobby", "project-b", Decimal::one());

    let gauge: GaugeResponse =
        from_json(query(deps.as_ref(), env.clone(), QueryMsg::Gauge { id: 0 }).unwrap()).unwrap();
    assert_eq!(gauge.current_epoch, Some(1));
    assert_eq!(
        gauge.snapshot_policy.unwrap().epoch_budget,
        Uint128::new(1_000)
    );

    let epoch: EpochResponse = from_json(
        query(
            deps.as_ref(),
            env.clone(),
            QueryMsg::Epoch { gauge: 0, epoch: 1 },
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(epoch.snapshot_height, 125);
    assert_eq!(epoch.snapshot_total_power, Uint128::new(100));
    assert_eq!(epoch.participating_power, Uint128::new(100));
    assert_eq!(epoch.total_cast, Uint128::new(100));
    assert_eq!(epoch.outcome, EpochOutcome::Open);

    let ballot: EpochBallotResponse = from_json(
        query(
            deps.as_ref(),
            env.clone(),
            QueryMsg::EpochBallot {
                gauge: 0,
                epoch: 1,
                voter: "alice".to_owned(),
            },
        )
        .unwrap(),
    )
    .unwrap();
    let ballot = ballot.ballot.unwrap();
    assert_eq!(ballot.voter, "alice");
    assert_eq!(ballot.power, Uint128::new(60));
    assert_eq!(ballot.receipt_index, 1);

    let absent: EpochBallotResponse = from_json(
        query(
            deps.as_ref(),
            env.clone(),
            QueryMsg::EpochBallot {
                gauge: 0,
                epoch: 1,
                voter: "carol".to_owned(),
            },
        )
        .unwrap(),
    )
    .unwrap();
    assert!(absent.ballot.is_none());

    let first: EpochAllocationsResponse = from_json(
        query(
            deps.as_ref(),
            env.clone(),
            QueryMsg::EpochAllocations {
                gauge: 0,
                epoch: 1,
                start_after: None,
                limit: Some(1),
            },
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        first.allocations,
        vec![("project-a".to_owned(), Uint128::new(60))]
    );
    let second: EpochAllocationsResponse = from_json(
        query(
            deps.as_ref(),
            env.clone(),
            QueryMsg::EpochAllocations {
                gauge: 0,
                epoch: 1,
                start_after: Some(first.allocations[0].0.clone()),
                limit: Some(1),
            },
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        second.allocations,
        vec![("project-b".to_owned(), Uint128::new(40))]
    );

    close(&mut env);
    execute(
        deps.as_mut(),
        env.clone(),
        mock_info("keeper", &[]),
        ExecuteMsg::Execute { gauge: 0 },
    )
    .unwrap();
    let completed: EpochResponse = from_json(
        query(
            deps.as_ref(),
            env.clone(),
            QueryMsg::Epoch { gauge: 0, epoch: 1 },
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        completed.outcome,
        EpochOutcome::Distributed { message_count: 1 }
    );

    env.block.height = 126;
    state.lock().unwrap().totals.insert(126, Uint128::new(200));
    open(&mut deps, &env);
    let first_page: ListEpochsResponse = from_json(
        query(
            deps.as_ref(),
            env.clone(),
            QueryMsg::ListEpochs {
                gauge: 0,
                start_after: None,
                limit: Some(1),
            },
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(first_page.epochs.len(), 1);
    assert_eq!(first_page.epochs[0].epoch_id, 1);
    let second_page: ListEpochsResponse = from_json(
        query(
            deps.as_ref(),
            env,
            QueryMsg::ListEpochs {
                gauge: 0,
                start_after: Some(first_page.epochs[0].epoch_id),
                limit: Some(1),
            },
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(second_page.epochs.len(), 1);
    assert_eq!(second_page.epochs[0].epoch_id, 2);
}

#[test]
fn turnout_boundary_is_exact_and_failed_turnout_is_terminal_without_adapter_call() {
    for (power, expected) in [
        (49, EpochOutcome::NoDistributionTurnout),
        (50, EpochOutcome::Distributed { message_count: 1 }),
        (51, EpochOutcome::Distributed { message_count: 1 }),
    ] {
        let (mut deps, mut env, state) = setup_snapshot(5_000, 321, 100);
        state
            .lock()
            .unwrap()
            .powers
            .insert(("alice".to_owned(), 321), Uint128::new(power));
        open(&mut deps, &env);
        vote(&mut deps, &env, "alice", "project-a", Decimal::one());
        close(&mut env);
        let response = execute(
            deps.as_mut(),
            env,
            mock_info("keeper", &[]),
            ExecuteMsg::Execute { gauge: 0 },
        )
        .unwrap();
        assert_snapshot_event(
            &response,
            "execute_snapshot_epoch",
            &[
                "action",
                "sender",
                "gauge_id",
                "epoch_id",
                "snapshot_height",
                "snapshot_total_power",
                "participating_power",
                "allocated_power",
                "total_cast",
                "retained_option_power",
                "unallocated_power",
                "selected_project_power",
                "emitted_value",
                "retained_value",
                "min_turnout_bps",
                "policy_version",
                "epoch_budget",
                "denom",
                "execution_deadline",
                "outcome",
                "message_count",
            ],
        );
        assert_eq!(
            attribute(&response, "outcome"),
            if power == 49 {
                "no_distribution_turnout"
            } else {
                "distributed"
            }
        );

        let epoch = EPOCHS.load(deps.as_ref().storage, (0, 1)).unwrap();
        assert_eq!(epoch.outcome, expected);
        assert_eq!(epoch.epoch_budget, Uint128::new(1_000));
        let state = state.lock().unwrap();
        if power == 49 {
            assert!(response.messages.is_empty());
            assert!(state.samples.is_empty());
        } else {
            assert_eq!(response.messages.len(), 1);
            assert_eq!(state.samples.len(), 1);
            assert_eq!(state.samples[0].epoch_budget, Some(Uint128::new(1_000)));
            assert_eq!(
                state.samples[0].available_balance,
                Some(Uint128::new(9_000))
            );
            assert_eq!(state.samples[0].denom.as_deref(), Some("ujuno"));
            assert_eq!(
                state.samples[0].selected,
                vec![("project-a".to_owned(), Decimal::one())]
            );
        }
    }
}

#[test]
fn consecutive_epochs_use_distinct_heights_and_isolated_power() {
    let (mut deps, mut env, state) = setup_snapshot(0, 100, 100);
    {
        let mut state = state.lock().unwrap();
        state
            .powers
            .insert(("alice".to_owned(), 100), Uint128::new(60));
        state.totals.insert(200, Uint128::new(250));
        state
            .powers
            .insert(("alice".to_owned(), 200), Uint128::new(20));
    }
    open(&mut deps, &env);
    vote(&mut deps, &env, "alice", "project-a", Decimal::one());
    close(&mut env);
    execute(
        deps.as_mut(),
        env.clone(),
        mock_info("keeper", &[]),
        ExecuteMsg::Execute { gauge: 0 },
    )
    .unwrap();

    env.block.height = 200;
    open(&mut deps, &env);
    vote(&mut deps, &env, "alice", "project-b", Decimal::one());

    let first = EPOCHS.load(deps.as_ref().storage, (0, 1)).unwrap();
    let second = EPOCHS.load(deps.as_ref().storage, (0, 2)).unwrap();
    assert_eq!(
        (first.snapshot_height, first.snapshot_total_power),
        (100, Uint128::new(100))
    );
    assert_eq!(
        (first.participating_power, first.total_cast),
        (Uint128::new(60), Uint128::new(60))
    );
    assert_eq!(
        (second.snapshot_height, second.snapshot_total_power),
        (200, Uint128::new(250))
    );
    assert_eq!(
        (second.participating_power, second.total_cast),
        (Uint128::new(20), Uint128::new(20))
    );
    assert_eq!(
        EPOCH_TALLY
            .load(deps.as_ref().storage, (0, 1, "project-a"))
            .unwrap(),
        60
    );
    assert_eq!(
        EPOCH_TALLY
            .load(deps.as_ref().storage, (0, 2, "project-b"))
            .unwrap(),
        20
    );
    let state = state.lock().unwrap();
    assert_eq!(state.total_queries, vec![100, 200]);
    assert_eq!(
        state.voter_queries,
        vec![("alice".to_owned(), 100), ("alice".to_owned(), 200)]
    );
}

#[test]
fn zero_failed_and_mismatched_total_queries_cannot_open_an_epoch() {
    for mode in 0..3 {
        let (mut deps, env, state) = setup_snapshot(0, 777, 0);
        if mode == 1 {
            state.lock().unwrap().fail_total = true;
        }
        if mode == 2 {
            let mut state = state.lock().unwrap();
            state.totals.insert(777, Uint128::new(100));
            state.mismatch_total_height = true;
        }
        let error = execute(
            deps.as_mut(),
            env,
            mock_info("keeper", &[]),
            ExecuteMsg::OpenEpoch { gauge: 0 },
        )
        .unwrap_err();
        match mode {
            0 => assert_eq!(error, ContractError::ZeroSnapshotTotalPower { height: 777 }),
            1 => assert!(matches!(error, ContractError::Std(_))),
            2 => assert_eq!(
                error,
                ContractError::SnapshotHeightMismatch {
                    expected: 777,
                    actual: 776,
                }
            ),
            _ => unreachable!(),
        }
        assert!(!EPOCHS.has(deps.as_ref().storage, (0, 1)));
    }
}

#[test]
fn snapshot_mode_rejects_hooks_and_guardian_can_stop_but_not_resume() {
    let (mut deps, env, _) = setup_snapshot(0, 123, 100);
    let error = execute(
        deps.as_mut(),
        env.clone(),
        mock_info("governor", &[]),
        ExecuteMsg::AddHook {
            addr: "subscriber".to_owned(),
        },
    )
    .unwrap_err();
    assert_eq!(error, ContractError::HooksDisabledInSnapshotMode {});

    let error = execute(
        deps.as_mut(),
        env.clone(),
        mock_info("powers", &[]),
        ExecuteMsg::StakeChangeHook(StakeChangedHookMsg::Stake {
            addr: Addr::unchecked("alice"),
            amount: Uint128::new(10),
        }),
    )
    .unwrap_err();
    assert_eq!(error, ContractError::HooksDisabledInSnapshotMode {});

    execute(
        deps.as_mut(),
        env.clone(),
        mock_info("guardian", &[]),
        ExecuteMsg::StopGauge { gauge: 0 },
    )
    .unwrap();
    assert!(GAUGES.load(deps.as_ref().storage, 0).unwrap().is_stopped);
    let error = execute(
        deps.as_mut(),
        env.clone(),
        mock_info("guardian", &[]),
        ExecuteMsg::ResumeGauge { gauge: 0 },
    )
    .unwrap_err();
    assert_eq!(error, ContractError::Unauthorized {});
    execute(
        deps.as_mut(),
        env,
        mock_info("governor", &[]),
        ExecuteMsg::ResumeGauge { gauge: 0 },
    )
    .unwrap();
    assert!(!GAUGES.load(deps.as_ref().storage, 0).unwrap().is_stopped);
}

#[test]
fn snapshot_policy_updates_are_future_only_and_selection_config_locks_while_open() {
    let (mut deps, mut env, state) = setup_snapshot(0, 234, 100);
    state
        .lock()
        .unwrap()
        .powers
        .insert(("alice".to_owned(), 234), Uint128::new(100));
    open(&mut deps, &env);

    let error = execute(
        deps.as_mut(),
        env.clone(),
        mock_info("governor", &[]),
        ExecuteMsg::UpdateGauge {
            gauge_id: 0,
            epoch_size: None,
            min_percent_selected: Some(Decimal::percent(10)),
            max_options_selected: None,
            max_available_percentage: None,
        },
    )
    .unwrap_err();
    assert_eq!(
        error,
        ContractError::SnapshotGaugeConfigLocked { gauge_id: 0 }
    );

    let response = execute(
        deps.as_mut(),
        env.clone(),
        mock_info("governor", &[]),
        ExecuteMsg::UpdateSnapshotPolicy {
            gauge: 0,
            policy: EpochSnapshotPolicy {
                min_turnout_bps: 9_000,
                epoch_budget: Uint128::new(2_000),
                denom: "ujuno".to_owned(),
                retained_option: None,
                execution_window_seconds: 100,
            },
        },
    )
    .unwrap();
    assert_snapshot_event(
        &response,
        "update_snapshot_policy",
        &[
            "action",
            "sender",
            "gauge_id",
            "policy_version",
            "min_turnout_bps",
            "epoch_budget",
            "denom",
            "retained_option",
            "execution_window_seconds",
        ],
    );
    let first = EPOCHS.load(deps.as_ref().storage, (0, 1)).unwrap();
    assert_eq!(first.min_turnout_bps, 0);
    assert_eq!(first.epoch_budget, Uint128::new(1_000));

    vote(&mut deps, &env, "alice", "project-a", Decimal::one());
    close(&mut env);
    execute(
        deps.as_mut(),
        env.clone(),
        mock_info("keeper", &[]),
        ExecuteMsg::Execute { gauge: 0 },
    )
    .unwrap();
    env.block.height = 235;
    {
        let mut state = state.lock().unwrap();
        state.totals.insert(235, Uint128::new(100));
    }
    open(&mut deps, &env);
    let second = EPOCHS.load(deps.as_ref().storage, (0, 2)).unwrap();
    assert_eq!(second.min_turnout_bps, 9_000);
    assert_eq!(second.epoch_budget, Uint128::new(2_000));
}

#[test]
fn adapter_failure_leaves_epoch_open_and_cleanup_makes_bounded_progress() {
    let (mut deps, mut env, state) = setup_snapshot(0, 456, 100);
    {
        let mut state = state.lock().unwrap();
        state
            .powers
            .insert(("alice".to_owned(), 456), Uint128::new(50));
        state
            .powers
            .insert(("bobby".to_owned(), 456), Uint128::new(50));
    }
    open(&mut deps, &env);
    vote(&mut deps, &env, "alice", "project-a", Decimal::one());
    vote(&mut deps, &env, "bobby", "project-b", Decimal::one());
    close(&mut env);

    state.lock().unwrap().fail_sample = true;
    assert!(execute(
        deps.as_mut(),
        env.clone(),
        mock_info("keeper", &[]),
        ExecuteMsg::Execute { gauge: 0 },
    )
    .is_err());
    assert_eq!(
        EPOCHS.load(deps.as_ref().storage, (0, 1)).unwrap().outcome,
        EpochOutcome::Open
    );
    assert!(GAUGES
        .load(deps.as_ref().storage, 0)
        .unwrap()
        .last_executed_set
        .is_none());

    state.lock().unwrap().fail_sample = false;
    execute(
        deps.as_mut(),
        env.clone(),
        mock_info("keeper", &[]),
        ExecuteMsg::Execute { gauge: 0 },
    )
    .unwrap();

    for (expected_phase, expected_complete) in [
        ("ballots", "false"),
        ("options", "false"),
        ("options", "false"),
        ("complete", "true"),
    ] {
        let response = execute(
            deps.as_mut(),
            env.clone(),
            mock_info("keeper", &[]),
            ExecuteMsg::CleanupEpoch {
                gauge: 0,
                epoch: 1,
                limit: 1,
            },
        )
        .unwrap();
        assert_snapshot_event(
            &response,
            "cleanup_snapshot_epoch",
            &[
                "action",
                "sender",
                "gauge_id",
                "epoch_id",
                "processed",
                "phase",
                "complete",
            ],
        );
        assert_eq!(
            response
                .attributes
                .iter()
                .find(|a| a.key == "processed")
                .unwrap()
                .value,
            "1"
        );
        assert_eq!(response.attributes[5].value, expected_phase);
        assert_eq!(response.attributes[6].value, expected_complete);
    }
    let epoch = EPOCHS.load(deps.as_ref().storage, (0, 1)).unwrap();
    assert!(epoch.cleanup.complete);
    assert!(!EPOCH_BALLOT_INDEX.has(deps.as_ref().storage, (0, 1, 1)));
    assert!(!EPOCH_VOTER_POWER.has(deps.as_ref().storage, (0, 1, &Addr::unchecked("alice"))));
    assert!(!EPOCH_OPTIONS.has(deps.as_ref().storage, (0, 1, "project-a")));
    assert!(!EPOCH_TALLY.has(deps.as_ref().storage, (0, 1, "project-a")));

    let response = execute(
        deps.as_mut(),
        env,
        mock_info("keeper", &[]),
        ExecuteMsg::CleanupEpoch {
            gauge: 0,
            epoch: 1,
            limit: 1,
        },
    )
    .unwrap();
    assert_snapshot_event(
        &response,
        "cleanup_snapshot_epoch",
        &[
            "action",
            "sender",
            "gauge_id",
            "epoch_id",
            "processed",
            "phase",
            "complete",
        ],
    );
    assert_eq!(response.attributes[5].value, "complete");
    assert_eq!(response.attributes[6].value, "true");
    assert_eq!(
        response
            .attributes
            .iter()
            .find(|a| a.key == "processed")
            .unwrap()
            .value,
        "0"
    );
}

#[test]
fn suspended_project_between_ballot_and_execution_receives_nothing() {
    let (mut deps, mut env, state) = setup_snapshot(0, 888, 100);
    state
        .lock()
        .unwrap()
        .powers
        .insert(("alice".to_owned(), 888), Uint128::new(100));
    open(&mut deps, &env);
    vote(&mut deps, &env, "alice", "project-a", Decimal::one());
    state
        .lock()
        .unwrap()
        .invalid_options
        .insert("project-a".to_owned());
    close(&mut env);

    let response = execute(
        deps.as_mut(),
        env,
        mock_info("keeper", &[]),
        ExecuteMsg::Execute { gauge: 0 },
    )
    .unwrap();
    assert_snapshot_event(
        &response,
        "execute_snapshot_epoch",
        &[
            "action",
            "sender",
            "gauge_id",
            "epoch_id",
            "snapshot_height",
            "snapshot_total_power",
            "participating_power",
            "allocated_power",
            "total_cast",
            "retained_option_power",
            "unallocated_power",
            "selected_project_power",
            "emitted_value",
            "retained_value",
            "min_turnout_bps",
            "policy_version",
            "epoch_budget",
            "denom",
            "execution_deadline",
            "outcome",
            "message_count",
        ],
    );
    assert_eq!(attribute(&response, "outcome"), "no_eligible_options");
    assert_eq!(attribute(&response, "message_count"), "0");
    assert!(response.messages.is_empty());
    assert_eq!(
        EPOCHS.load(deps.as_ref().storage, (0, 1)).unwrap().outcome,
        EpochOutcome::NoEligibleOptions
    );
    assert!(state.lock().unwrap().samples.is_empty());
}

#[test]
fn snapshot_open_and_execution_enforce_option_and_message_bounds_atomically() {
    let (mut deps, env, state) = setup_snapshot(0, 654, 100);
    state.lock().unwrap().options = (0..=100)
        .map(|index| format!("project-{index:03}"))
        .collect();
    assert_eq!(
        execute(
            deps.as_mut(),
            env,
            mock_info("keeper", &[]),
            ExecuteMsg::OpenEpoch { gauge: 0 },
        )
        .unwrap_err(),
        ContractError::TooManyOptions {
            count: 101,
            max: 100,
        }
    );
    assert!(!EPOCHS.has(deps.as_ref().storage, (0, 1)));

    let (mut deps, mut env, state) = setup_snapshot(0, 655, 100);
    {
        let mut state = state.lock().unwrap();
        state
            .powers
            .insert(("alice".to_owned(), 655), Uint128::new(100));
        state.oversized_sample = true;
    }
    open(&mut deps, &env);
    vote(&mut deps, &env, "alice", "project-a", Decimal::one());
    close(&mut env);
    assert_eq!(
        execute(
            deps.as_mut(),
            env,
            mock_info("keeper", &[]),
            ExecuteMsg::Execute { gauge: 0 },
        )
        .unwrap_err(),
        ContractError::TooManyAdapterMessages {
            count: 101,
            max: 100,
        }
    );
    assert_eq!(
        EPOCHS.load(deps.as_ref().storage, (0, 1)).unwrap().outcome,
        EpochOutcome::Open
    );
}

#[test]
fn partial_and_retained_ballots_use_participating_power_without_renormalizing() {
    for (retained_weight, expected_allocated, expected_retained, expected_unallocated) in
        [(Some(Decimal::percent(95)), 100, 95, 0), (None, 5, 0, 95)]
    {
        let (mut deps, mut env, state) =
            setup_snapshot_with_retained(0, 700, 100, Some("retained"));
        {
            let mut state = state.lock().unwrap();
            state
                .powers
                .insert(("alice".to_owned(), 700), Uint128::new(100));
            state.sample_emitted_value = Some(Uint128::new(50));
        }
        open(&mut deps, &env);
        let mut votes = vec![Vote {
            option: "project-a".to_owned(),
            weight: Decimal::percent(5),
        }];
        if let Some(weight) = retained_weight {
            votes.push(Vote {
                option: "retained".to_owned(),
                weight,
            });
        }
        execute(
            deps.as_mut(),
            env.clone(),
            mock_info("alice", &[]),
            ExecuteMsg::PlaceVotes {
                gauge: 0,
                votes: Some(votes),
            },
        )
        .unwrap();
        let open_epoch = EPOCHS.load(deps.as_ref().storage, (0, 1)).unwrap();
        assert_eq!(open_epoch.participating_power, Uint128::new(100));
        assert_eq!(open_epoch.total_cast, Uint128::new(expected_allocated));
        assert_eq!(
            open_epoch.retained_option_power,
            Uint128::new(expected_retained)
        );
        assert_eq!(
            open_epoch
                .participating_power
                .checked_sub(open_epoch.total_cast)
                .unwrap(),
            Uint128::new(expected_unallocated)
        );

        close(&mut env);
        execute(
            deps.as_mut(),
            env,
            mock_info("keeper", &[]),
            ExecuteMsg::Execute { gauge: 0 },
        )
        .unwrap();
        let state = state.lock().unwrap();
        assert_eq!(
            state.samples[0].selected,
            vec![("project-a".to_owned(), Decimal::percent(5))]
        );
        let epoch = EPOCHS.load(deps.as_ref().storage, (0, 1)).unwrap();
        assert_eq!(epoch.emitted_value, Uint128::new(50));
        assert_eq!(epoch.retained_value, Uint128::new(950));
    }
}

#[test]
fn retained_only_ballot_counts_turnout_and_never_consumes_a_project_slot() {
    let (mut deps, mut env, state) =
        setup_snapshot_with_retained(5_000, 701, 100, Some("retained"));
    state
        .lock()
        .unwrap()
        .powers
        .insert(("alice".to_owned(), 701), Uint128::new(100));
    open(&mut deps, &env);
    vote(&mut deps, &env, "alice", "retained", Decimal::one());
    close(&mut env);
    let response = execute(
        deps.as_mut(),
        env,
        mock_info("keeper", &[]),
        ExecuteMsg::Execute { gauge: 0 },
    )
    .unwrap();
    assert_eq!(attribute(&response, "outcome"), "no_eligible_options");
    assert!(response.messages.is_empty());
    let epoch = EPOCHS.load(deps.as_ref().storage, (0, 1)).unwrap();
    assert_eq!(epoch.participating_power, Uint128::new(100));
    assert_eq!(epoch.retained_option_power, Uint128::new(100));
    assert_eq!(epoch.selected_project_power, Uint128::zero());
    assert_eq!(epoch.retained_value, Uint128::new(1_000));
    assert!(state.lock().unwrap().samples.is_empty());

    let (mut deps, mut env, state) = setup_snapshot_with_retained(0, 702, 100, Some("retained"));
    {
        let mut state = state.lock().unwrap();
        state
            .powers
            .insert(("alice".to_owned(), 702), Uint128::new(100));
        state.sample_emitted_value = Some(Uint128::new(100));
    }
    execute(
        deps.as_mut(),
        env.clone(),
        mock_info("governor", &[]),
        ExecuteMsg::UpdateGauge {
            gauge_id: 0,
            epoch_size: None,
            min_percent_selected: None,
            max_options_selected: Some(1),
            max_available_percentage: None,
        },
    )
    .unwrap();
    open(&mut deps, &env);
    execute(
        deps.as_mut(),
        env.clone(),
        mock_info("alice", &[]),
        ExecuteMsg::PlaceVotes {
            gauge: 0,
            votes: Some(vec![
                Vote {
                    option: "retained".to_owned(),
                    weight: Decimal::percent(90),
                },
                Vote {
                    option: "project-a".to_owned(),
                    weight: Decimal::percent(6),
                },
                Vote {
                    option: "project-b".to_owned(),
                    weight: Decimal::percent(4),
                },
            ]),
        },
    )
    .unwrap();
    close(&mut env);
    execute(
        deps.as_mut(),
        env,
        mock_info("keeper", &[]),
        ExecuteMsg::Execute { gauge: 0 },
    )
    .unwrap();
    assert_eq!(
        state.lock().unwrap().samples[0].selected,
        vec![("project-a".to_owned(), Decimal::percent(6))]
    );
}

#[test]
fn project_threshold_and_cap_boundaries_use_participating_power() {
    for (allocated, should_select, expected_share) in [
        (99u64, false, None),
        (100, true, Some(Decimal::percent(10))),
        (101, true, Some(Decimal::permille(101))),
        (199, true, Some(Decimal::permille(199))),
        (200, true, Some(Decimal::percent(20))),
        (201, true, Some(Decimal::percent(20))),
    ] {
        let (mut deps, mut env, state) = setup_snapshot(0, 710 + allocated, 1_000);
        state
            .lock()
            .unwrap()
            .powers
            .insert(("alice".to_owned(), 710 + allocated), Uint128::new(1_000));
        execute(
            deps.as_mut(),
            env.clone(),
            mock_info("governor", &[]),
            ExecuteMsg::UpdateGauge {
                gauge_id: 0,
                epoch_size: None,
                min_percent_selected: Some(Decimal::percent(10)),
                max_options_selected: None,
                max_available_percentage: Some(Decimal::percent(20)),
            },
        )
        .unwrap();
        open(&mut deps, &env);
        vote(
            &mut deps,
            &env,
            "alice",
            "project-a",
            Decimal::from_ratio(allocated, 1_000u64),
        );
        close(&mut env);
        execute(
            deps.as_mut(),
            env,
            mock_info("keeper", &[]),
            ExecuteMsg::Execute { gauge: 0 },
        )
        .unwrap();
        let state = state.lock().unwrap();
        assert_eq!(
            !state.samples.is_empty(),
            should_select,
            "allocation {allocated}"
        );
        if let Some(expected_share) = expected_share {
            assert_eq!(
                state.samples[0].selected,
                vec![("project-a".to_owned(), expected_share)],
                "allocation {allocated}"
            );
        }
    }
}

#[test]
fn opening_requires_the_complete_budget_and_rejection_is_atomic() {
    for (balance, opens) in [(999u128, false), (1_000, true), (1_001, true)] {
        let (mut deps, env, state) = setup_snapshot(0, 900, 100);
        deps.querier
            .update_balance("dao", vec![coin(balance, "ujuno")]);
        let initial_schedule = GAUGES.load(deps.as_ref().storage, 0).unwrap().next_epoch;
        let result = execute(
            deps.as_mut(),
            env,
            mock_info("direct-caller", &[]),
            ExecuteMsg::OpenEpoch { gauge: 0 },
        );
        assert_eq!(result.is_ok(), opens, "balance {balance}");
        if opens {
            assert!(EPOCHS.has(deps.as_ref().storage, (0, 1)));
        } else {
            assert_eq!(
                result.unwrap_err(),
                ContractError::InsufficientEpochFunding {
                    required: Uint128::new(1_000),
                    available: Uint128::new(999),
                    denom: "ujuno".to_owned(),
                }
            );
            assert!(!EPOCHS.has(deps.as_ref().storage, (0, 1)));
            assert!(!CURRENT_EPOCH.has(deps.as_ref().storage, 0));
            assert_eq!(NEXT_EPOCH_ID.load(deps.as_ref().storage, 0).unwrap(), 1);
            assert_eq!(
                GAUGES.load(deps.as_ref().storage, 0).unwrap().next_epoch,
                initial_schedule
            );
            assert!(state.lock().unwrap().total_queries.is_empty());
        }
    }
}

#[test]
fn execution_requires_only_emitted_value_and_underfunding_is_terminal() {
    for (balance, distributed) in [(100u128, true), (99, false)] {
        let (mut deps, mut env, state) = setup_snapshot(0, 901, 100);
        {
            let mut state = state.lock().unwrap();
            state
                .powers
                .insert(("alice".to_owned(), 901), Uint128::new(100));
            state.sample_emitted_value = Some(Uint128::new(100));
        }
        open(&mut deps, &env);
        vote(&mut deps, &env, "alice", "project-a", Decimal::percent(10));
        deps.querier
            .update_balance("dao", vec![coin(balance, "ujuno")]);
        close(&mut env);
        let response = execute(
            deps.as_mut(),
            env.clone(),
            mock_info("keeper", &[]),
            ExecuteMsg::Execute { gauge: 0 },
        )
        .unwrap();
        let epoch = EPOCHS.load(deps.as_ref().storage, (0, 1)).unwrap();
        if distributed {
            assert_eq!(attribute(&response, "outcome"), "distributed");
            assert_eq!(response.messages.len(), 1);
            assert_eq!(epoch.emitted_value, Uint128::new(100));
            assert_eq!(epoch.retained_value, Uint128::new(900));
        } else {
            assert_eq!(attribute(&response, "outcome"), "insufficient_funds");
            assert!(response.messages.is_empty());
            assert_eq!(
                epoch.outcome,
                EpochOutcome::InsufficientFunds {
                    required: Uint128::new(100),
                    available: Uint128::new(99),
                }
            );
            assert_eq!(epoch.emitted_value, Uint128::zero());
            assert_eq!(epoch.retained_value, Uint128::new(1_000));
            deps.querier
                .update_balance("dao", vec![coin(1_000, "ujuno")]);
            assert_eq!(
                execute(
                    deps.as_mut(),
                    env,
                    mock_info("keeper", &[]),
                    ExecuteMsg::Execute { gauge: 0 },
                )
                .unwrap_err(),
                ContractError::EpochNotOpen { gauge: 0, epoch: 1 }
            );
        }
    }
}

#[test]
fn expiry_abort_and_following_epoch_schedule_are_terminal_and_idempotent() {
    let (mut deps, mut env, state) = setup_snapshot(0, 902, 100);
    open(&mut deps, &env);
    env.block.time = env.block.time.plus_seconds(199);
    assert!(matches!(
        execute(
            deps.as_mut(),
            env.clone(),
            mock_info("keeper", &[]),
            ExecuteMsg::ExpireEpoch { gauge: 0 },
        ),
        Err(ContractError::ExecutionDeadlineNotReached { .. })
    ));
    env.block.time = env.block.time.plus_seconds(1);
    let expired = execute(
        deps.as_mut(),
        env.clone(),
        mock_info("keeper", &[]),
        ExecuteMsg::ExpireEpoch { gauge: 0 },
    )
    .unwrap();
    assert_eq!(attribute(&expired, "outcome"), "expired");
    assert_eq!(
        EPOCHS.load(deps.as_ref().storage, (0, 1)).unwrap().outcome,
        EpochOutcome::Expired
    );
    assert!(matches!(
        execute(
            deps.as_mut(),
            env.clone(),
            mock_info("keeper", &[]),
            ExecuteMsg::ExpireEpoch { gauge: 0 },
        ),
        Err(ContractError::EpochNotOpen { .. })
    ));

    env.block.height = 903;
    state.lock().unwrap().totals.insert(903, Uint128::new(100));
    open(&mut deps, &env);
    assert_eq!(CURRENT_EPOCH.load(deps.as_ref().storage, 0).unwrap(), 2);
    assert_eq!(
        execute(
            deps.as_mut(),
            env.clone(),
            mock_info("guardian", &[]),
            ExecuteMsg::AbortEpoch {
                gauge: 0,
                reason: "adapter recovery".to_owned(),
            },
        )
        .unwrap_err(),
        ContractError::Unauthorized {}
    );
    let aborted = execute(
        deps.as_mut(),
        env.clone(),
        mock_info("governor", &[]),
        ExecuteMsg::AbortEpoch {
            gauge: 0,
            reason: "adapter recovery".to_owned(),
        },
    )
    .unwrap();
    assert_eq!(attribute(&aborted, "outcome"), "aborted");
    assert_eq!(attribute(&aborted, "reason"), "adapter recovery");
    assert!(matches!(
        execute(
            deps.as_mut(),
            env.clone(),
            mock_info("governor", &[]),
            ExecuteMsg::AbortEpoch {
                gauge: 0,
                reason: "again".to_owned(),
            },
        ),
        Err(ContractError::EpochNotOpen { .. })
    ));

    env.block.height = 904;
    state.lock().unwrap().totals.insert(904, Uint128::new(100));
    open(&mut deps, &env);
    assert_eq!(CURRENT_EPOCH.load(deps.as_ref().storage, 0).unwrap(), 3);
}

#[test]
fn adapter_accounting_must_be_complete_and_consistent() {
    for omit in [true, false] {
        let (mut deps, mut env, state) = setup_snapshot(0, 905, 100);
        {
            let mut state = state.lock().unwrap();
            state
                .powers
                .insert(("alice".to_owned(), 905), Uint128::new(100));
            state.omit_sample_accounting = omit;
            state.corrupt_sample_accounting = !omit;
        }
        open(&mut deps, &env);
        vote(&mut deps, &env, "alice", "project-a", Decimal::one());
        close(&mut env);
        let error = execute(
            deps.as_mut(),
            env,
            mock_info("keeper", &[]),
            ExecuteMsg::Execute { gauge: 0 },
        )
        .unwrap_err();
        assert_eq!(
            error,
            if omit {
                ContractError::MissingAdapterAccounting {}
            } else {
                ContractError::InvalidAdapterAccounting {}
            }
        );
        assert_eq!(
            EPOCHS.load(deps.as_ref().storage, (0, 1)).unwrap().outcome,
            EpochOutcome::Open
        );
    }
}

proptest! {
    #[test]
    fn arbitrary_snapshot_revisions_preserve_tallies_and_participation(
        operations in prop::collection::vec((any::<u8>(), any::<u8>(), any::<bool>()), 1..80)
    ) {
        let (mut deps, env, state) = setup_snapshot(0, 999, 1_000);
        state
            .lock()
            .unwrap()
            .powers
            .insert(("alice".to_owned(), 999), Uint128::new(100));
        open(&mut deps, &env);
        let mut active = false;

        for (raw_a, raw_b, abstain) in operations {
            let a = raw_a % 101;
            let b = raw_b % (101 - a);
            if abstain || (a == 0 && b == 0) {
                if !active {
                    continue;
                }
                execute(
                    deps.as_mut(),
                    env.clone(),
                    mock_info("alice", &[]),
                    ExecuteMsg::PlaceVotes { gauge: 0, votes: None },
                )?;
                active = false;
                prop_assert_eq!(EPOCH_TALLY.load(deps.as_ref().storage, (0, 1, "project-a"))?, 0);
                prop_assert_eq!(EPOCH_TALLY.load(deps.as_ref().storage, (0, 1, "project-b"))?, 0);
            } else {
                let mut votes = Vec::with_capacity(2);
                if a > 0 {
                    votes.push(Vote {
                        option: "project-a".to_owned(),
                        weight: Decimal::percent(u64::from(a)),
                    });
                }
                if b > 0 {
                    votes.push(Vote {
                        option: "project-b".to_owned(),
                        weight: Decimal::percent(u64::from(b)),
                    });
                }
                execute(
                    deps.as_mut(),
                    env.clone(),
                    mock_info("alice", &[]),
                    ExecuteMsg::PlaceVotes {
                        gauge: 0,
                        votes: Some(votes),
                    },
                )?;
                active = true;
                prop_assert_eq!(
                    EPOCH_TALLY.load(deps.as_ref().storage, (0, 1, "project-a"))?,
                    u128::from(a)
                );
                prop_assert_eq!(
                    EPOCH_TALLY.load(deps.as_ref().storage, (0, 1, "project-b"))?,
                    u128::from(b)
                );
            }

            let epoch = EPOCHS.load(deps.as_ref().storage, (0, 1))?;
            let tally_a = EPOCH_TALLY.load(deps.as_ref().storage, (0, 1, "project-a"))?;
            let tally_b = EPOCH_TALLY.load(deps.as_ref().storage, (0, 1, "project-b"))?;
            prop_assert_eq!(epoch.total_cast, Uint128::new(tally_a + tally_b));
            prop_assert_eq!(
                epoch.participating_power,
                if active { Uint128::new(100) } else { Uint128::zero() }
            );
            prop_assert_eq!(epoch.voter_count, u32::from(active));
        }

        let state = state.lock().unwrap();
        prop_assert_eq!(state.total_queries.as_slice(), &[999]);
        prop_assert!(state.voter_queries.len() <= 1);
    }
}
