#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    from_json, to_json_binary, Addr, Binary, Deps, DepsMut, Env, MessageInfo, Order, Response,
    StdError, StdResult, Uint128,
};
use cw2::{ensure_from_older_version, get_contract_version, set_contract_version};
use cw20::Cw20ReceiveMsg;
use cw_denom::UncheckedDenom;
use cw_storage_plus::Bound;
use cw_utils::{nonpayable, one_coin, PaymentError};
use gauge_interface::validate_selected_allocations;

use crate::{
    error::ContractError,
    msg::{AssetUnchecked, ExecuteMsg, InstantiateMsg, MigrateMsg, QueryMsg, ReceiveMsg},
    state::{
        Asset, Bond, BondState, Config, Submission, CONFIG, REFUNDS_COMPLETE, REFUND_CURSOR,
        SUBMISSIONS, SUBMISSION_BY_SENDER, TOTAL_LIABILITIES,
    },
};

// Version info for migration info.
const CONTRACT_NAME: &str = "crates.io:marketing-gauge-adapter";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");
const SUPPORTED_MIGRATION_SOURCES: &[&str] = &["2.4.2", "2.5.0"];
const MAX_SUBMISSIONS: usize = 1_000;
const MAX_NAME_BYTES: usize = 128;
const MAX_URL_BYTES: usize = 512;
const MAX_SELECTED_OPTIONS: usize = 100;

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    cw_ownable::initialize_owner(deps.storage, deps.api, Some(&msg.owner))?;

    let community_pool = deps.api.addr_validate(&msg.community_pool)?;
    SUBMISSIONS.save(
        deps.storage,
        community_pool.clone(),
        &Submission {
            sender: env.contract.address.clone(),
            name: "Unimpressed".to_owned(),
            url: "Those funds go back to the community pool".to_owned(),
            bond: None,
        },
    )?;
    SUBMISSION_BY_SENDER.save(deps.storage, (&env.contract.address, &community_pool), &())?;

    let required_deposit = msg
        .required_deposit
        .map(|x| x.into_checked(deps.as_ref()))
        .transpose()?;
    if required_deposit
        .as_ref()
        .is_some_and(|deposit| deposit.amount.is_zero())
    {
        return Err(ContractError::ZeroRequiredDeposit {});
    }
    let config = Config {
        required_deposit,
        community_pool,
        reward: msg.reward.into_checked(deps.as_ref())?,
    };
    CONFIG.save(deps.storage, &config)?;
    TOTAL_LIABILITIES.save(deps.storage, &Uint128::zero())?;
    REFUNDS_COMPLETE.save(deps.storage, &false)?;

    let mut response = Response::new()
        .add_attribute("action", "instantiate")
        .add_attribute("owner", msg.owner)
        .add_attribute("community_pool", config.community_pool)
        .add_attribute("reward_denom", config.reward.denom.to_string())
        .add_attribute("reward_amount", config.reward.amount);
    if let Some(deposit) = config.required_deposit {
        response = response
            .add_attribute("bond_denom", deposit.denom.to_string())
            .add_attribute("bond_amount", deposit.amount);
    } else {
        response = response.add_attribute("bond_amount", "0");
    }
    Ok(response)
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    match msg {
        ExecuteMsg::Receive(msg) => {
            nonpayable(&info)?;
            receive_cw20_message(deps, env, info, msg)
        }
        ExecuteMsg::CreateSubmission { name, url, address } => {
            let received = match one_coin(&info) {
                Ok(coin) => Ok(Some(coin)),
                Err(PaymentError::NoFunds {}) => Ok(None),
                Err(error) => Err(error),
            }?
            .map(|x| AssetUnchecked {
                denom: UncheckedDenom::Native(x.denom),
                amount: x.amount,
            });

            execute::create_submission(deps, env, info.sender, name, url, address, received)
        }
        ExecuteMsg::ReturnDeposits {} => {
            nonpayable(&info)?;
            execute::return_deposits(deps, env, info.sender)
        }
        ExecuteMsg::Reject { submission, soft } => {
            nonpayable(&info)?;
            execute::reject(deps, env, info.sender, submission, soft)
        }
        ExecuteMsg::UpdateOwnership(action) => {
            nonpayable(&info)?;
            let ownership = cw_ownable::update_ownership(deps, &env.block, &info.sender, action)?;
            Ok(Response::new()
                .add_attribute("action", "update_ownership")
                .add_attribute("sender", &info.sender)
                .add_attributes(ownership.into_attributes()))
        }
    }
}

fn receive_cw20_message(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: Cw20ReceiveMsg,
) -> Result<Response, ContractError> {
    match from_json(&msg.msg)? {
        ReceiveMsg::CreateSubmission { name, url, address } => execute::create_submission(
            deps,
            env,
            Addr::unchecked(msg.sender),
            name,
            url,
            address,
            Some(AssetUnchecked::new_cw20(
                info.sender.as_str(),
                msg.amount.u128(),
            )),
        ),
    }
}

pub mod execute {
    use super::*;

    use cosmwasm_std::CosmosMsg;

    pub fn create_submission(
        deps: DepsMut,
        env: Env,
        sender: Addr,
        name: String,
        url: String,
        address: String,
        received: Option<AssetUnchecked>,
    ) -> Result<Response, ContractError> {
        let address = deps.api.addr_validate(&address)?;
        if name.len() > MAX_NAME_BYTES {
            return Err(ContractError::StringTooLong {
                field: "name".to_owned(),
                max: MAX_NAME_BYTES,
            });
        }
        if url.len() > MAX_URL_BYTES {
            return Err(ContractError::StringTooLong {
                field: "url".to_owned(),
                max: MAX_URL_BYTES,
            });
        }

        let old_submission = SUBMISSIONS.may_load(deps.storage, address.clone())?;
        if let Some(old) = &old_submission {
            if old.sender != sender {
                return Err(ContractError::UnauthorizedSubmission {});
            }
            if received
                .as_ref()
                .is_some_and(|asset| !asset.amount.is_zero())
            {
                return Err(ContractError::DepositOnMetadataUpdate {});
            }
            let bond_state = match old.bond.as_ref().map(|bond| &bond.state) {
                Some(BondState::Active) => {
                    let bond = old.bond.as_ref().unwrap();
                    let liabilities = TOTAL_LIABILITIES
                        .may_load(deps.storage)?
                        .unwrap_or_default();
                    assert_escrow(
                        deps.as_ref(),
                        &env,
                        &bond.asset,
                        liabilities,
                        Uint128::zero(),
                    )?;
                    "active"
                }
                Some(BondState::Refunded) => "refunded",
                Some(BondState::Forfeited) => "forfeited",
                None => "none",
            };
            SUBMISSIONS.save(
                deps.storage,
                address.clone(),
                &Submission {
                    sender: sender.clone(),
                    name,
                    url,
                    bond: old.bond.clone(),
                },
            )?;
            return Ok(Response::new()
                .add_attribute("action", "update_submission")
                .add_attribute("sender", &sender)
                .add_attribute("bond_state", bond_state)
                .add_attribute("submission", address));
        }
        if REFUND_CURSOR.may_load(deps.storage)?.is_some()
            || REFUNDS_COMPLETE.may_load(deps.storage)?.unwrap_or(false)
        {
            return Err(ContractError::RefundInProgress {});
        }
        let submission_count = SUBMISSIONS
            .keys(deps.storage, None, None, Order::Ascending)
            .take(MAX_SUBMISSIONS)
            .collect::<StdResult<Vec<_>>>()?
            .len();
        if submission_count >= MAX_SUBMISSIONS {
            return Err(ContractError::TooManySubmissions {
                max: MAX_SUBMISSIONS,
            });
        }

        let Config {
            required_deposit,
            community_pool: _,
            reward: _,
        } = CONFIG.load(deps.storage)?;
        let bond = if let Some(required_deposit) = required_deposit {
            if let Some(received) = received {
                let received_denom = received.denom.into_checked(deps.as_ref())?;

                if required_deposit.denom != received_denom {
                    return Err(ContractError::InvalidDepositType {});
                }
                if received.amount != required_deposit.amount {
                    return Err(ContractError::InvalidDepositAmount {
                        correct_amount: required_deposit.amount,
                    });
                }
                Some(Bond {
                    asset: required_deposit.clone(),
                    depositor: sender.clone(),
                    state: BondState::Active,
                })
            } else {
                return Err(ContractError::PaymentError(PaymentError::NoFunds {}));
            }
        } else if let Some(received) = received {
            // If no deposit is required, then any deposit invalidates a submission.
            if !received.amount.is_zero() {
                return Err(ContractError::InvalidDepositAmount {
                    correct_amount: Uint128::zero(),
                });
            }
            None
        } else {
            None
        };

        let mut liabilities = TOTAL_LIABILITIES
            .may_load(deps.storage)?
            .unwrap_or_default();
        if let Some(bond) = &bond {
            liabilities = liabilities
                .checked_add(bond.asset.amount)
                .map_err(|_| ContractError::LiabilityOverflow {})?;
            assert_escrow(
                deps.as_ref(),
                &env,
                &bond.asset,
                liabilities,
                Uint128::zero(),
            )?;
        }
        TOTAL_LIABILITIES.save(deps.storage, &liabilities)?;
        let bond_event = bond.as_ref().map(|bond| {
            (
                bond.depositor.to_string(),
                bond.asset.denom.to_string(),
                bond.asset.amount.to_string(),
            )
        });
        SUBMISSIONS.save(
            deps.storage,
            address.clone(),
            &Submission {
                sender: sender.clone(),
                name,
                url,
                bond,
            },
        )?;
        SUBMISSION_BY_SENDER.save(deps.storage, (&sender, &address), &())?;
        let mut response = Response::new()
            .add_attribute("action", "create_submission")
            .add_attribute("sender", &sender)
            .add_attribute("submission", address)
            .add_attribute(
                "bond_state",
                if bond_event.is_some() {
                    "active"
                } else {
                    "none"
                },
            )
            .add_attribute("liabilities", liabilities);
        if let Some((depositor, denom, amount)) = bond_event {
            response = response
                .add_attribute("depositor", depositor)
                .add_attribute("bond_denom", denom)
                .add_attribute("bond_amount", amount);
        }
        Ok(response)
    }

    pub fn reject(
        deps: DepsMut,
        env: Env,
        sender: Addr,
        submission: String,
        soft: bool,
    ) -> Result<Response, ContractError> {
        cw_ownable::assert_owner(deps.storage, &sender)?;

        let config = CONFIG.load(deps.storage)?;

        let submission_addr = deps.api.addr_validate(&submission)?;
        if submission_addr == config.community_pool {
            return Err(ContractError::CannotRejectDefault {});
        }
        let stored = SUBMISSIONS
            .may_load(deps.storage, submission_addr.clone())?
            .ok_or_else(|| ContractError::SubmissionNotFound(submission.clone()))?;

        let mut liabilities = TOTAL_LIABILITIES
            .may_load(deps.storage)?
            .unwrap_or_default();
        let mut transfer = None;
        let mut bond_denom = None;
        let mut bond_amount = Uint128::zero();
        let mut bond_state = "none";
        if let Some(mut bond) = stored.bond {
            bond_denom = Some(bond.asset.denom.to_string());
            bond_amount = bond.asset.amount;
            bond_state = match bond.state {
                BondState::Active => "active",
                BondState::Refunded => "refunded",
                BondState::Forfeited => "forfeited",
            };
            if bond.state == BondState::Active {
                liabilities = liabilities
                    .checked_sub(bond.asset.amount)
                    .map_err(|_| ContractError::LiabilityUnderflow {})?;
                let destination = if soft {
                    &bond.depositor
                } else {
                    &config.community_pool
                };
                transfer = Some(
                    bond.asset
                        .denom
                        .get_transfer_to_message(destination, bond.asset.amount)?,
                );
                assert_escrow(
                    deps.as_ref(),
                    &env,
                    &bond.asset,
                    liabilities,
                    bond.asset.amount,
                )?;
                bond.state = if soft {
                    BondState::Refunded
                } else {
                    BondState::Forfeited
                };
                bond_state = if soft { "refunded" } else { "forfeited" };
            }
        }

        TOTAL_LIABILITIES.save(deps.storage, &liabilities)?;
        SUBMISSION_BY_SENDER.remove(deps.storage, (&stored.sender, &submission_addr));
        SUBMISSIONS.remove(deps.storage, submission_addr.clone());

        let mut resp = Response::new()
            .add_attribute("action", "reject")
            .add_attribute("sender", &sender)
            .add_attribute("submission", submission_addr.to_string())
            .add_attribute("kind", if soft { "soft" } else { "hard" })
            .add_attribute("bond_state", bond_state)
            .add_attribute("bond_amount", bond_amount);
        if let Some(denom) = bond_denom {
            resp = resp.add_attribute("bond_denom", denom);
        }

        if let Some(msg) = transfer {
            resp = resp.add_message(msg);
        }

        Ok(resp.add_attribute("liabilities", liabilities))
    }

    pub fn return_deposits(
        deps: DepsMut,
        env: Env,
        sender: Addr,
    ) -> Result<Response, ContractError> {
        cw_ownable::assert_owner(deps.storage, &sender)?;

        let Config {
            required_deposit,
            community_pool: _,
            reward: _,
        } = CONFIG.load(deps.storage)?;

        // No refund if no deposit was required.
        let required_deposit = required_deposit.ok_or(ContractError::NoDepositToRefund {})?;

        if REFUNDS_COMPLETE.may_load(deps.storage)?.unwrap_or(false) {
            let liabilities = TOTAL_LIABILITIES
                .may_load(deps.storage)?
                .unwrap_or_default();
            return Ok(Response::new()
                .add_attribute("action", "return_deposits")
                .add_attribute("sender", &sender)
                .add_attribute("processed", "0")
                .add_attribute("complete", "true")
                .add_attribute("next_cursor", "none")
                .add_attribute("message_count", "0")
                .add_attribute("refunded_amount", "0")
                .add_attribute("liabilities", liabilities));
        }

        const REFUND_BATCH_SIZE: usize = 50;
        let cursor = REFUND_CURSOR.may_load(deps.storage)?;
        let start = cursor.map(Bound::exclusive);
        let records = SUBMISSIONS
            .range(deps.storage, start, None, Order::Ascending)
            .take(REFUND_BATCH_SIZE + 1)
            .collect::<StdResult<Vec<_>>>()?;
        let processed = records.len().min(REFUND_BATCH_SIZE);
        let complete = records.len() <= REFUND_BATCH_SIZE;
        let mut liabilities = TOTAL_LIABILITIES
            .may_load(deps.storage)?
            .unwrap_or_default();
        let mut outgoing = Uint128::zero();
        let mut msgs: Vec<CosmosMsg> = vec![];

        for (address, mut submission) in records.iter().take(processed).cloned() {
            if let Some(bond) = submission.bond.as_mut() {
                if bond.state == BondState::Active {
                    liabilities = liabilities
                        .checked_sub(bond.asset.amount)
                        .map_err(|_| ContractError::LiabilityUnderflow {})?;
                    outgoing = outgoing
                        .checked_add(bond.asset.amount)
                        .map_err(|_| ContractError::LiabilityOverflow {})?;
                    msgs.push(
                        bond.asset
                            .denom
                            .get_transfer_to_message(&bond.depositor, bond.asset.amount)?,
                    );
                    bond.state = BondState::Refunded;
                    SUBMISSIONS.save(deps.storage, address, &submission)?;
                }
            }
        }

        assert_escrow(
            deps.as_ref(),
            &env,
            &required_deposit,
            liabilities,
            outgoing,
        )?;
        TOTAL_LIABILITIES.save(deps.storage, &liabilities)?;
        let next_cursor = if complete {
            REFUND_CURSOR.remove(deps.storage);
            REFUNDS_COMPLETE.save(deps.storage, &true)?;
            "none".to_owned()
        } else if let Some((last, _)) = records.get(processed.saturating_sub(1)) {
            REFUND_CURSOR.save(deps.storage, last)?;
            last.to_string()
        } else {
            "none".to_owned()
        };
        let message_count = msgs.len();

        Ok(Response::new()
            .add_messages(msgs)
            .add_attribute("action", "return_deposits")
            .add_attribute("sender", &sender)
            .add_attribute("processed", processed.to_string())
            .add_attribute("complete", complete.to_string())
            .add_attribute("next_cursor", next_cursor)
            .add_attribute("message_count", message_count.to_string())
            .add_attribute("refunded_amount", outgoing)
            .add_attribute("liabilities", liabilities))
    }

    pub(super) fn assert_escrow(
        deps: Deps,
        env: &Env,
        asset: &crate::state::Asset,
        liabilities: Uint128,
        outgoing: Uint128,
    ) -> Result<(), ContractError> {
        let balance = asset
            .denom
            .query_balance(&deps.querier, &env.contract.address)?;
        let required = liabilities
            .checked_add(outgoing)
            .map_err(|_| ContractError::LiabilityOverflow {})?;
        if balance < required {
            return Err(ContractError::EscrowShortfall {
                balance,
                liabilities: required,
            });
        }
        Ok(())
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => to_json_binary(&CONFIG.load(deps.storage)?),
        QueryMsg::AllOptions { start_after, limit } => {
            to_json_binary(&query::all_options(deps, start_after, limit)?)
        }
        QueryMsg::CheckOption { option } => to_json_binary(&query::check_option(deps, option)?),
        QueryMsg::SampleGaugeMsgs { selected, .. } => {
            to_json_binary(&query::sample_gauge_msgs(deps, selected)?)
        }
        QueryMsg::Submission { address } => to_json_binary(&query::submission(deps, address)?),
        QueryMsg::AllSubmissions { start_after, limit } => {
            to_json_binary(&query::all_submissions(deps, start_after, limit)?)
        }
        QueryMsg::SubmissionsBySender {
            sender,
            start_after,
            limit,
        } => to_json_binary(&query::submissions_by_sender(
            deps,
            sender,
            start_after,
            limit,
        )?),
        QueryMsg::Liabilities {} => to_json_binary(&query::liabilities(deps, env)?),
        QueryMsg::Ownership {} => to_json_binary(&cw_ownable::get_ownership(deps.storage)?),
    }
}

mod query {
    use cosmwasm_std::{CosmosMsg, Decimal, StdError};

    use crate::msg::{
        AllOptionsResponse, AllSubmissionsResponse, CheckOptionResponse, LiabilitiesResponse,
        SampleGaugeMsgsResponse, SubmissionResponse,
    };

    use super::*;

    const DEFAULT_LIMIT: u32 = 30;
    const MAX_LIMIT: u32 = 100;

    pub fn all_options(
        deps: Deps,
        start_after: Option<String>,
        limit: Option<u32>,
    ) -> StdResult<AllOptionsResponse> {
        let start = start_after
            .map(|address| deps.api.addr_validate(&address))
            .transpose()?
            .map(Bound::exclusive);
        let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;
        Ok(AllOptionsResponse {
            options: SUBMISSIONS
                .keys(deps.storage, start, None, Order::Ascending)
                .take(limit)
                .map(|key| Ok(key?.to_string()))
                .collect::<StdResult<Vec<String>>>()?,
        })
    }

    pub fn check_option(deps: Deps, option: String) -> StdResult<CheckOptionResponse> {
        Ok(CheckOptionResponse {
            valid: SUBMISSIONS.has(deps.storage, deps.api.addr_validate(&option)?),
        })
    }

    pub fn sample_gauge_msgs(
        deps: Deps,
        winners: Vec<(String, Decimal)>,
    ) -> StdResult<SampleGaugeMsgsResponse> {
        if winners.len() > MAX_SELECTED_OPTIONS {
            return Err(StdError::generic_err(format!(
                "too many selected options: {}; maximum is {}",
                winners.len(),
                MAX_SELECTED_OPTIONS
            )));
        }
        validate_selected_allocations(&winners)?;
        let reward = CONFIG.load(deps.storage)?.reward;

        let execute = winners
            .into_iter()
            .map(|(to_address, fraction)| {
                // Gauge already sends chosen tally to this query by using results we send in
                // all_options query; they are already validated
                let to_address = deps.api.addr_validate(&to_address)?;

                reward.denom.get_transfer_to_message(
                    &to_address,
                    reward
                        .amount
                        .checked_mul_floor(fraction)
                        .map_err(|x| StdError::generic_err(x.to_string()))?,
                )
            })
            .collect::<StdResult<Vec<CosmosMsg>>>()?;
        Ok(SampleGaugeMsgsResponse {
            execute,
            emitted_value: None,
            retained_value: None,
        })
    }

    pub fn submission(deps: Deps, address: String) -> StdResult<SubmissionResponse> {
        let address = deps.api.addr_validate(&address)?;
        let submission = SUBMISSIONS.load(deps.storage, address.clone())?;
        Ok(SubmissionResponse {
            sender: submission.sender,
            name: submission.name,
            url: submission.url,
            address,
        })
    }

    pub fn all_submissions(
        deps: Deps,
        start_after: Option<String>,
        limit: Option<u32>,
    ) -> StdResult<AllSubmissionsResponse> {
        let start = start_after
            .map(|address| deps.api.addr_validate(&address))
            .transpose()?
            .map(Bound::exclusive);
        let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;
        Ok(AllSubmissionsResponse {
            submissions: SUBMISSIONS
                .range(deps.storage, start, None, Order::Ascending)
                .take(limit)
                .map(|s| {
                    let (address, submission) = s?;
                    Ok(SubmissionResponse {
                        sender: submission.sender,
                        name: submission.name,
                        url: submission.url,
                        address,
                    })
                })
                .collect::<StdResult<Vec<SubmissionResponse>>>()?,
        })
    }

    pub fn submissions_by_sender(
        deps: Deps,
        sender: String,
        start_after: Option<String>,
        limit: Option<u32>,
    ) -> StdResult<AllSubmissionsResponse> {
        let sender = deps.api.addr_validate(&sender)?;
        let start_after = start_after
            .map(|address| deps.api.addr_validate(&address))
            .transpose()?;
        let start = start_after.as_ref().map(Bound::exclusive);
        let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;
        Ok(AllSubmissionsResponse {
            submissions: SUBMISSION_BY_SENDER
                .prefix(&sender)
                .keys(deps.storage, start, None, Order::Ascending)
                .take(limit)
                .map(|address| {
                    let address = address?;
                    let submission = SUBMISSIONS.load(deps.storage, address.clone())?;
                    Ok(SubmissionResponse {
                        sender: submission.sender,
                        name: submission.name,
                        url: submission.url,
                        address,
                    })
                })
                .collect::<StdResult<Vec<SubmissionResponse>>>()?,
        })
    }

    pub fn liabilities(deps: Deps, env: Env) -> StdResult<LiabilitiesResponse> {
        let config = CONFIG.load(deps.storage)?;
        let amount = TOTAL_LIABILITIES
            .may_load(deps.storage)?
            .unwrap_or_default();
        let asset = config.required_deposit.map(|mut asset| {
            asset.amount = amount;
            asset
        });
        let escrow_balance = match &asset {
            Some(asset) => asset
                .denom
                .query_balance(&deps.querier, &env.contract.address)?,
            None => Uint128::zero(),
        };
        Ok(LiabilitiesResponse {
            asset,
            escrow_balance,
            refund_cursor: REFUND_CURSOR.may_load(deps.storage)?,
            refunds_complete: REFUNDS_COMPLETE.may_load(deps.storage)?.unwrap_or(false),
        })
    }
}

/// Manages the contract migration.
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(deps: DepsMut, env: Env, _msg: MigrateMsg) -> Result<Response, ContractError> {
    #[cosmwasm_schema::cw_serde]
    struct LegacyConfig {
        admin: Addr,
        required_deposit: Option<Asset>,
        community_pool: Addr,
        reward: Asset,
    }

    let previous = get_contract_version(deps.storage)?;
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
    if previous.contract != CONTRACT_NAME {
        return Err(StdError::generic_err(format!(
            "cannot migrate contract {}; expected {CONTRACT_NAME}",
            previous.contract
        ))
        .into());
    }
    if !SUPPORTED_MIGRATION_SOURCES.contains(&previous.version.as_str()) {
        return Err(ContractError::UnsupportedMigrationSource {
            version: previous.version,
        });
    }

    // Decode the raw item before rewriting it. The former layout stored its
    // administrator here; modern ownership lives in cw-ownable storage.
    let raw_config = deps
        .storage
        .get(b"config")
        .ok_or_else(|| StdError::not_found("Config"))?;
    let legacy_config = from_json::<LegacyConfig>(&raw_config).ok();
    let config = match &legacy_config {
        Some(legacy) => Config {
            required_deposit: legacy.required_deposit.clone(),
            community_pool: legacy.community_pool.clone(),
            reward: legacy.reward.clone(),
        },
        None => CONFIG.load(deps.storage)?,
    };

    let mut submissions = SUBMISSIONS
        .range(deps.storage, None, None, Order::Ascending)
        .take(MAX_SUBMISSIONS.saturating_add(1))
        .collect::<StdResult<Vec<_>>>()?;
    if submissions.len() > MAX_SUBMISSIONS {
        return Err(ContractError::TooManySubmissions {
            max: MAX_SUBMISSIONS,
        });
    }
    let mut liabilities = Uint128::zero();
    let mut migrated_bonds = 0usize;
    for (address, submission) in &mut submissions {
        if submission.bond.is_none() && address != &config.community_pool {
            if let Some(asset) = &config.required_deposit {
                submission.bond = Some(Bond {
                    asset: asset.clone(),
                    depositor: submission.sender.clone(),
                    state: BondState::Active,
                });
                migrated_bonds += 1;
            }
        }
        if let Some(Bond {
            asset,
            state: BondState::Active,
            ..
        }) = &submission.bond
        {
            liabilities = liabilities
                .checked_add(asset.amount)
                .map_err(|_| ContractError::LiabilityOverflow {})?;
        }
    }

    // Validate solvency before any state changes. A failed migration then
    // leaves both cw2 and application state untouched.
    if let Some(asset) = &config.required_deposit {
        execute::assert_escrow(deps.as_ref(), &env, asset, liabilities, Uint128::zero())?;
    }

    ensure_from_older_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    if let Some(legacy) = legacy_config {
        CONFIG.save(deps.storage, &config)?;
        cw_ownable::initialize_owner(deps.storage, deps.api, Some(legacy.admin.as_str()))?;
    }
    for (address, submission) in &submissions {
        SUBMISSIONS.save(deps.storage, address.clone(), submission)?;
        SUBMISSION_BY_SENDER.save(deps.storage, (&submission.sender, address), &())?;
    }
    TOTAL_LIABILITIES.save(deps.storage, &liabilities)?;
    if REFUNDS_COMPLETE.may_load(deps.storage)?.is_none() {
        REFUNDS_COMPLETE.save(deps.storage, &false)?;
    }
    Ok(Response::new()
        .add_attribute("action", "migrate")
        .add_attribute("from_version", previous.version)
        .add_attribute("to_version", CONTRACT_VERSION)
        .add_attribute("migrated_records", submissions.len().to_string())
        .add_attribute("migrated_bonds", migrated_bonds.to_string())
        .add_attribute("liabilities", liabilities))
}

#[cfg(test)]
mod tests {
    use super::*;

    use cosmwasm_std::{
        coins,
        testing::{mock_dependencies, mock_env, mock_info, MockApi, MockQuerier, MockStorage},
        to_json_vec, BankMsg, CosmosMsg, Decimal, OwnedDeps, Uint128,
    };
    use cw_denom::CheckedDenom;

    use crate::{msg::AssetUnchecked, state::Asset};

    #[test]
    fn proper_initialization() {
        let mut deps = mock_dependencies();
        let msg = InstantiateMsg {
            owner: "admin".to_owned(),
            required_deposit: Some(AssetUnchecked::new_native("wynd", 10_000_000)),
            community_pool: "community".to_owned(),
            reward: AssetUnchecked::new_native("ujuno", 150_000_000_000),
        };
        instantiate(
            deps.as_mut(),
            mock_env(),
            mock_info("user", &[]),
            msg.clone(),
        )
        .unwrap();

        // Check if the config is stored.
        let config = CONFIG.load(deps.as_ref().storage).unwrap();
        let ownership = cw_ownable::get_ownership(deps.as_ref().storage).unwrap();
        assert_eq!(ownership.owner, Some(Addr::unchecked("admin")));
        assert_eq!(
            config.required_deposit,
            Some(Asset {
                denom: CheckedDenom::Native(String::from("wynd")),
                amount: Uint128::new(10_000_000)
            })
        );
        assert_eq!(config.community_pool, "community".to_owned());
        assert_eq!(
            config.reward,
            Asset {
                denom: CheckedDenom::Native("ujuno".to_owned()),
                amount: Uint128::new(150_000_000_000)
            }
        );

        let msg = InstantiateMsg {
            reward: AssetUnchecked::new_native("ujuno", 10_000_000),
            ..msg
        };
        instantiate(
            deps.as_mut(),
            mock_env(),
            mock_info("user", &[]),
            msg.clone(),
        )
        .unwrap();
        let config = CONFIG.load(deps.as_ref().storage).unwrap();
        assert_eq!(
            config.reward,
            Asset {
                denom: CheckedDenom::Native("ujuno".to_owned()),
                amount: Uint128::new(10_000_000)
            }
        );

        let msg = InstantiateMsg {
            required_deposit: None,
            ..msg
        };
        instantiate(deps.as_mut(), mock_env(), mock_info("user", &[]), msg).unwrap();
        let config = CONFIG.load(deps.as_ref().storage).unwrap();
        assert_eq!(config.required_deposit, None);
    }

    #[test]
    fn sample_gauge_msgs_native() {
        let mut deps = mock_dependencies();

        let reward = Uint128::new(150_000_000_000);
        let msg = InstantiateMsg {
            owner: "admin".to_owned(),
            required_deposit: Some(AssetUnchecked::new_native("wynd", 10_000_000)),
            community_pool: "community".to_owned(),
            reward: AssetUnchecked::new_native("ujuno", reward.into()),
        };
        instantiate(deps.as_mut(), mock_env(), mock_info("user", &[]), msg).unwrap();

        let selected = vec![
            (
                "juno1t8ehvswxjfn3ejzkjtntcyrqwvmvuknzy3ajxy".to_string(),
                Decimal::percent(41),
            ),
            (
                "juno196ax4vc0lwpxndu9dyhvca7jhxp70rmcl99tyh".to_string(),
                Decimal::percent(33),
            ),
            (
                "juno1y0us8xvsvfvqkk9c6nt5cfyu5au5tww23dmh40".to_string(),
                Decimal::percent(26),
            ),
        ];
        let res = query::sample_gauge_msgs(deps.as_ref(), selected).unwrap();
        assert_eq!(res.execute.len(), 3);
        assert_eq!(
            res.execute,
            [
                CosmosMsg::Bank(BankMsg::Send {
                    to_address: "juno1t8ehvswxjfn3ejzkjtntcyrqwvmvuknzy3ajxy".to_string(),
                    amount: coins((reward * Decimal::percent(41)).u128(), "ujuno")
                }),
                CosmosMsg::Bank(BankMsg::Send {
                    to_address: "juno196ax4vc0lwpxndu9dyhvca7jhxp70rmcl99tyh".to_string(),
                    amount: coins((reward * Decimal::percent(33)).u128(), "ujuno")
                }),
                CosmosMsg::Bank(BankMsg::Send {
                    to_address: "juno1y0us8xvsvfvqkk9c6nt5cfyu5au5tww23dmh40".to_string(),
                    amount: coins((reward * Decimal::percent(26)).u128(), "ujuno")
                }),
            ]
        );
    }

    #[test]
    fn return_deposits_authorization() {
        let mut deps = mock_dependencies();
        let msg = InstantiateMsg {
            owner: "admin".to_owned(),
            required_deposit: None,
            community_pool: "community".to_owned(),
            reward: AssetUnchecked::new_native("ujuno", 150_000_000_000),
        };
        instantiate(
            deps.as_mut(),
            mock_env(),
            mock_info("user", &[]),
            msg.clone(),
        )
        .unwrap();

        let err = execute::return_deposits(deps.as_mut(), mock_env(), Addr::unchecked("admin"))
            .unwrap_err();
        assert_eq!(err, ContractError::NoDepositToRefund {});

        let msg = InstantiateMsg {
            required_deposit: Some(AssetUnchecked::new_native("ujuno", 10_000_000)),
            ..msg
        };
        instantiate(deps.as_mut(), mock_env(), mock_info("user", &[]), msg).unwrap();

        let err = execute::return_deposits(deps.as_mut(), mock_env(), Addr::unchecked("user"))
            .unwrap_err();
        assert_eq!(
            err,
            ContractError::Ownership(cw_ownable::OwnershipError::NotOwner)
        );
    }

    #[test]
    fn migrate_enforces_identity_and_strictly_older_version() {
        #[cosmwasm_schema::cw_serde]
        struct LegacyConfig {
            admin: Addr,
            required_deposit: Option<Asset>,
            community_pool: Addr,
            reward: Asset,
        }

        let mut deps = mock_dependencies();
        let env = mock_env();
        let legacy_sender = Addr::unchecked("legacy-sender");
        let legacy_destination = Addr::unchecked("legacy-destination");
        let legacy_admin = Addr::unchecked("legacy-admin");
        let community_pool = Addr::unchecked("community");
        let deposit = Asset {
            denom: CheckedDenom::Native("ujuno".to_owned()),
            amount: Uint128::new(10),
        };
        deps.as_mut().storage.set(
            b"config",
            &to_json_vec(&LegacyConfig {
                admin: legacy_admin.clone(),
                required_deposit: Some(deposit.clone()),
                community_pool: community_pool.clone(),
                reward: Asset {
                    denom: CheckedDenom::Native("ureward".to_owned()),
                    amount: Uint128::new(1_000),
                },
            })
            .unwrap(),
        );
        SUBMISSIONS
            .save(
                deps.as_mut().storage,
                community_pool.clone(),
                &Submission {
                    sender: env.contract.address.clone(),
                    name: "default".to_owned(),
                    url: "default".to_owned(),
                    bond: None,
                },
            )
            .unwrap();
        SUBMISSIONS
            .save(
                deps.as_mut().storage,
                legacy_destination.clone(),
                &Submission {
                    sender: legacy_sender.clone(),
                    name: "legacy".to_owned(),
                    url: "https://legacy.example".to_owned(),
                    bond: None,
                },
            )
            .unwrap();
        deps.querier
            .update_balance(env.contract.address.clone(), coins(10, "ujuno"));
        set_contract_version(deps.as_mut().storage, CONTRACT_NAME, "2.5.0").unwrap();
        let response = migrate(deps.as_mut(), env, MigrateMsg {}).unwrap();
        let version = get_contract_version(deps.as_ref().storage).unwrap();
        assert_eq!(version.contract, CONTRACT_NAME);
        assert_eq!(version.version, CONTRACT_VERSION);
        assert!(response
            .attributes
            .iter()
            .any(|attribute| attribute.key == "from_version" && attribute.value == "2.5.0"));
        assert!(
            SUBMISSION_BY_SENDER.has(deps.as_ref().storage, (&legacy_sender, &legacy_destination))
        );
        let ownership = cw_ownable::get_ownership(deps.as_ref().storage).unwrap();
        assert_eq!(ownership.owner, Some(legacy_admin));
        let migrated = SUBMISSIONS
            .load(deps.as_ref().storage, legacy_destination)
            .unwrap();
        assert_eq!(
            migrated.bond,
            Some(Bond {
                asset: deposit,
                depositor: legacy_sender,
                state: BondState::Active,
            })
        );
        assert_eq!(
            TOTAL_LIABILITIES.load(deps.as_ref().storage).unwrap(),
            Uint128::new(10)
        );
        assert!(response
            .attributes
            .iter()
            .any(|attribute| { attribute.key == "migrated_bonds" && attribute.value == "1" }));

        let mut wrong = mock_dependencies();
        set_contract_version(wrong.as_mut().storage, "wrong-contract", "0.1.0").unwrap();
        assert!(migrate(wrong.as_mut(), mock_env(), MigrateMsg {}).is_err());
        assert_eq!(
            get_contract_version(wrong.as_ref().storage)
                .unwrap()
                .contract,
            "wrong-contract"
        );

        let mut same = mock_dependencies();
        set_contract_version(same.as_mut().storage, CONTRACT_NAME, CONTRACT_VERSION).unwrap();
        assert!(migrate(same.as_mut(), mock_env(), MigrateMsg {}).is_err());

        let mut unsupported = mock_dependencies();
        set_contract_version(unsupported.as_mut().storage, CONTRACT_NAME, "2.4.1").unwrap();
        assert_eq!(
            migrate(unsupported.as_mut(), mock_env(), MigrateMsg {}).unwrap_err(),
            ContractError::UnsupportedMigrationSource {
                version: "2.4.1".to_owned(),
            }
        );
        assert_eq!(
            get_contract_version(unsupported.as_ref().storage)
                .unwrap()
                .version,
            "2.4.1"
        );
    }

    #[test]
    fn migrate_preserves_modern_ownership() {
        let mut deps = mock_dependencies();
        let env = mock_env();
        instantiate(
            deps.as_mut(),
            env.clone(),
            mock_info("sender", &[]),
            InstantiateMsg {
                owner: "modern-owner".to_owned(),
                required_deposit: None,
                community_pool: "community".to_owned(),
                reward: AssetUnchecked::new_native("ureward", 1_000),
            },
        )
        .unwrap();
        cw_ownable::update_ownership(
            deps.as_mut(),
            &env.block,
            &Addr::unchecked("modern-owner"),
            cw_ownable::Action::TransferOwnership {
                new_owner: "pending-owner".to_owned(),
                expiry: None,
            },
        )
        .unwrap();
        let ownership_before = cw_ownable::get_ownership(deps.as_ref().storage).unwrap();
        set_contract_version(deps.as_mut().storage, CONTRACT_NAME, "2.5.0").unwrap();

        migrate(deps.as_mut(), env, MigrateMsg {}).unwrap();

        assert_eq!(
            cw_ownable::get_ownership(deps.as_ref().storage).unwrap(),
            ownership_before
        );
    }

    #[test]
    fn migrate_accepts_supported_2_4_2_populated_state() {
        let mut deps = mock_dependencies();
        let env = mock_env();
        instantiate(
            deps.as_mut(),
            env.clone(),
            mock_info("creator", &[]),
            InstantiateMsg {
                owner: "owner".to_owned(),
                required_deposit: None,
                community_pool: "community-pool".to_owned(),
                reward: AssetUnchecked {
                    denom: UncheckedDenom::Native("ujuno".to_owned()),
                    amount: Uint128::new(1_000),
                },
            },
        )
        .unwrap();
        let destination = Addr::unchecked("destination");
        let sender = Addr::unchecked("sender");
        SUBMISSIONS
            .save(
                deps.as_mut().storage,
                destination.clone(),
                &Submission {
                    sender: sender.clone(),
                    name: "name".to_owned(),
                    url: "https://example.invalid".to_owned(),
                    bond: None,
                },
            )
            .unwrap();
        set_contract_version(deps.as_mut().storage, CONTRACT_NAME, "2.4.2").unwrap();

        let response = migrate(deps.as_mut(), env, MigrateMsg {}).unwrap();
        assert!(SUBMISSIONS.has(deps.as_ref().storage, destination.clone()));
        assert!(SUBMISSION_BY_SENDER.has(deps.as_ref().storage, (&sender, &destination)));
        assert!(response
            .attributes
            .iter()
            .any(|attribute| attribute.key == "from_version" && attribute.value == "2.4.2"));
    }

    #[test]
    fn migration_submission_bound_accepts_exact_and_rejects_over() {
        fn populated(count: usize) -> OwnedDeps<MockStorage, MockApi, MockQuerier> {
            let mut deps = mock_dependencies();
            instantiate(
                deps.as_mut(),
                mock_env(),
                mock_info("creator", &[]),
                InstantiateMsg {
                    owner: "owner".to_owned(),
                    required_deposit: None,
                    community_pool: "community".to_owned(),
                    reward: AssetUnchecked::new_native("ureward", 1_000),
                },
            )
            .unwrap();
            // Instantiate creates the synthetic community submission.
            for index in 1..count {
                let address = Addr::unchecked(format!("destination-{index:04}"));
                SUBMISSIONS
                    .save(
                        deps.as_mut().storage,
                        address,
                        &Submission {
                            sender: Addr::unchecked(format!("sender-{index:04}")),
                            name: "name".to_owned(),
                            url: "url".to_owned(),
                            bond: None,
                        },
                    )
                    .unwrap();
            }
            set_contract_version(deps.as_mut().storage, CONTRACT_NAME, "2.5.0").unwrap();
            deps
        }

        let mut exact = populated(MAX_SUBMISSIONS);
        let response = migrate(exact.as_mut(), mock_env(), MigrateMsg {}).unwrap();
        assert!(response.attributes.iter().any(|attribute| {
            attribute.key == "migrated_records" && attribute.value == MAX_SUBMISSIONS.to_string()
        }));

        let mut over = populated(MAX_SUBMISSIONS + 1);
        assert_eq!(
            migrate(over.as_mut(), mock_env(), MigrateMsg {}).unwrap_err(),
            ContractError::TooManySubmissions {
                max: MAX_SUBMISSIONS,
            }
        );
        assert_eq!(
            get_contract_version(over.as_ref().storage).unwrap().version,
            "2.5.0"
        );
    }

    #[test]
    fn migrate_rejects_underfunded_reconstructed_bonds_without_bumping_version() {
        let mut deps = mock_dependencies();
        instantiate(
            deps.as_mut(),
            mock_env(),
            mock_info("sender", &[]),
            InstantiateMsg {
                owner: "owner".to_owned(),
                required_deposit: Some(AssetUnchecked::new_native("ujuno", 10)),
                community_pool: "community".to_owned(),
                reward: AssetUnchecked::new_native("ureward", 1_000),
            },
        )
        .unwrap();
        let destination = Addr::unchecked("destination");
        SUBMISSIONS
            .save(
                deps.as_mut().storage,
                destination.clone(),
                &Submission {
                    sender: Addr::unchecked("depositor"),
                    name: "legacy".to_owned(),
                    url: "https://legacy.example".to_owned(),
                    bond: None,
                },
            )
            .unwrap();
        set_contract_version(deps.as_mut().storage, CONTRACT_NAME, "2.5.0").unwrap();

        assert_eq!(
            migrate(deps.as_mut(), mock_env(), MigrateMsg {}).unwrap_err(),
            ContractError::EscrowShortfall {
                balance: Uint128::zero(),
                liabilities: Uint128::new(10),
            }
        );
        assert_eq!(
            get_contract_version(deps.as_ref().storage).unwrap().version,
            "2.5.0"
        );
        assert_eq!(
            SUBMISSIONS
                .load(deps.as_ref().storage, destination)
                .unwrap()
                .bond,
            None
        );
        assert_eq!(
            TOTAL_LIABILITIES.load(deps.as_ref().storage).unwrap(),
            Uint128::zero()
        );
    }
}
