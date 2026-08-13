#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    coin, to_json_binary, BankMsg, Binary, Coin, CosmosMsg, Decimal, Deps, DepsMut, Env,
    MessageInfo, Order, Response, StdError, StdResult, Uint128,
};
use cw2::set_contract_version;
use cw_storage_plus::Bound;
use cw_utils::nonpayable;
use gauge_interface::validate_selected_allocations;

use crate::{
    error::ContractError,
    msg::{
        AdapterQueryMsg, AllOptionsResponse, CheckOptionResponse, ExecuteMsg, InstantiateMsg,
        QueryMsg, SampleGaugeMsgsResponse,
    },
    state::{Config, CONFIG, OPTIONS},
};

const CONTRACT_NAME: &str = "crates.io:gauge-budget-allocator";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_OPTIONS: usize = 100;

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    if msg.options.is_empty() {
        return Err(ContractError::NoOptions {});
    }
    if msg.options.len() > MAX_OPTIONS {
        return Err(ContractError::TooManyOptions {
            count: msg.options.len(),
            max: MAX_OPTIONS,
        });
    }
    let option_count = msg.options.len();
    let budget_denom = msg.epoch_budget.denom.clone();
    let budget_amount = msg.epoch_budget.amount;

    cw_ownable::initialize_owner(deps.storage, deps.api, Some(&msg.owner))?;

    CONFIG.save(
        deps.storage,
        &Config {
            epoch_budget: msg.epoch_budget,
        },
    )?;

    for option in msg.options {
        let option = deps.api.addr_validate(&option)?;
        OPTIONS.save(deps.storage, option.as_str(), &())?;
    }

    Ok(Response::new()
        .add_attribute("action", "instantiate")
        .add_attribute("owner", msg.owner)
        .add_attribute("denom", budget_denom)
        .add_attribute("amount", budget_amount)
        .add_attribute("option_count", option_count.to_string()))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    nonpayable(&info)?;
    // UpdateOwnership runs its own auth — gate everything else here.
    if !matches!(msg, ExecuteMsg::UpdateOwnership(_)) {
        cw_ownable::assert_owner(deps.storage, &info.sender)?;
    }

    match msg {
        ExecuteMsg::AddOption { option } => {
            let option_count = OPTIONS
                .keys(deps.storage, None, None, Order::Ascending)
                .take(MAX_OPTIONS)
                .collect::<StdResult<Vec<_>>>()?
                .len();
            if option_count >= MAX_OPTIONS {
                return Err(ContractError::TooManyOptions {
                    count: option_count.saturating_add(1),
                    max: MAX_OPTIONS,
                });
            }
            let option = deps.api.addr_validate(&option)?.into_string();
            if OPTIONS.has(deps.storage, option.as_str()) {
                return Err(ContractError::OptionAlreadyExists(option));
            }
            OPTIONS.save(deps.storage, option.as_str(), &())?;
            Ok(Response::new()
                .add_attribute("action", "add_option")
                .add_attribute("sender", &info.sender)
                .add_attribute("option", option))
        }
        ExecuteMsg::RemoveOption { option } => {
            let option = deps.api.addr_validate(&option)?.into_string();
            if !OPTIONS.has(deps.storage, option.as_str()) {
                return Err(ContractError::OptionDoesNotExist(option));
            }
            OPTIONS.remove(deps.storage, option.as_str());
            Ok(Response::new()
                .add_attribute("action", "remove_option")
                .add_attribute("sender", &info.sender)
                .add_attribute("option", option))
        }
        ExecuteMsg::UpdateBudget { epoch_budget } => {
            CONFIG.update(deps.storage, |mut c| -> StdResult<_> {
                c.epoch_budget = epoch_budget.clone();
                Ok(c)
            })?;
            Ok(Response::new()
                .add_attribute("action", "update_budget")
                .add_attribute("sender", &info.sender)
                .add_attribute("denom", &epoch_budget.denom)
                .add_attribute("amount", epoch_budget.amount.to_string()))
        }
        ExecuteMsg::UpdateOwnership(action) => {
            let ownership = cw_ownable::update_ownership(deps, &env.block, &info.sender, action)?;
            Ok(Response::new()
                .add_attribute("action", "update_ownership")
                .add_attribute("sender", &info.sender)
                .add_attributes(ownership.into_attributes()))
        }
    }
}

/// Native (non-orchestrator) query entrypoint. Accepts `QueryMsg`, which is
/// a superset of `AdapterQueryMsg` (it adds `Config {}`). The orchestrator
/// only ever sends `AdapterQueryMsg` variants, which we translate below.
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => to_json_binary(&CONFIG.load(deps.storage)?),
        QueryMsg::AllOptions { start_after, limit } => {
            to_json_binary(&all_options(deps, start_after, limit)?)
        }
        QueryMsg::CheckOption { option } => to_json_binary(&check_option(deps, option)),
        QueryMsg::SampleGaugeMsgs { selected, .. } => {
            to_json_binary(&sample_gauge_msgs(deps, selected)?)
        }
        QueryMsg::Ownership {} => to_json_binary(&cw_ownable::get_ownership(deps.storage)?),
    }
}

/// Convenience: dispatch a raw `AdapterQueryMsg` (what the orchestrator
/// sends) through the same handlers. Useful for integration tests that
/// want to confirm orchestrator-compat without writing two variants.
pub fn answer_adapter(deps: Deps, msg: AdapterQueryMsg) -> StdResult<Binary> {
    match msg {
        AdapterQueryMsg::AllOptions { start_after, limit } => {
            to_json_binary(&all_options(deps, start_after, limit)?)
        }
        AdapterQueryMsg::CheckOption { option } => to_json_binary(&check_option(deps, option)),
        AdapterQueryMsg::SampleGaugeMsgs { selected, .. } => {
            to_json_binary(&sample_gauge_msgs(deps, selected)?)
        }
    }
}

fn all_options(
    deps: Deps,
    start_after: Option<String>,
    limit: Option<u32>,
) -> StdResult<AllOptionsResponse> {
    let start = start_after.as_deref().map(Bound::exclusive);
    let limit = limit.unwrap_or(30).min(MAX_OPTIONS as u32) as usize;
    Ok(AllOptionsResponse {
        options: OPTIONS
            .keys(deps.storage, start, None, Order::Ascending)
            .take(limit)
            .collect::<StdResult<Vec<_>>>()?,
    })
}

fn check_option(deps: Deps, option: String) -> CheckOptionResponse {
    CheckOptionResponse {
        valid: OPTIONS.has(deps.storage, option.as_str()),
    }
}

fn sample_gauge_msgs(
    deps: Deps,
    selected: Vec<(String, Decimal)>,
) -> StdResult<SampleGaugeMsgsResponse> {
    if selected.len() > MAX_OPTIONS {
        return Err(StdError::generic_err(format!(
            "too many selected options: {}; maximum is {MAX_OPTIONS}",
            selected.len()
        )));
    }
    validate_selected_allocations(&selected)?;
    let Config { epoch_budget, .. } = CONFIG.load(deps.storage)?;
    let execute = selected
        .into_iter()
        .filter_map(|(to_address, weight)| {
            let amount = epoch_budget
                .amount
                .checked_mul_floor(weight)
                .map_err(|e| StdError::generic_err(e.to_string()));
            match amount {
                Ok(amount) if amount.is_zero() => None,
                Ok(amount) => Some(
                    deps.api
                        .addr_validate(&to_address)
                        .map(|address| send_message(address.into_string(), &epoch_budget, amount)),
                ),
                Err(error) => Some(Err(error)),
            }
        })
        .collect::<StdResult<Vec<CosmosMsg>>>()?;
    Ok(SampleGaugeMsgsResponse {
        execute,
        emitted_value: None,
        retained_value: None,
    })
}

fn send_message(to: String, budget: &Coin, amount: Uint128) -> CosmosMsg {
    CosmosMsg::Bank(BankMsg::Send {
        to_address: to,
        amount: vec![coin(amount.u128(), budget.denom.clone())],
    })
}
