use cosmwasm_schema::{cw_serde, QueryResponses};
use cosmwasm_std::{CosmosMsg, Decimal, StdError, StdResult, Uint128};
use std::collections::HashSet;

/// Minimal protocol every gauge adapter implements.
#[cw_serde]
#[derive(QueryResponses)]
pub enum AdapterQueryMsg {
    #[returns(AllOptionsResponse)]
    AllOptions {
        start_after: Option<String>,
        limit: Option<u32>,
    },
    #[returns(CheckOptionResponse)]
    CheckOption { option: String },
    #[returns(SampleGaugeMsgsResponse)]
    SampleGaugeMsgs {
        /// Global allocation shares. These may sum to less than one when
        /// caps, thresholds, or selection limits leave funds unallocated.
        /// Option keys must be unique and nonempty, every share must be
        /// positive, and the total must not exceed one.
        selected: Vec<(String, Decimal)>,
        /// Explicit budget context for epoch-snapshot gauges. Hook-mode
        /// callers leave these fields unset and retain legacy adapter policy.
        epoch_budget: Option<Uint128>,
        available_balance: Option<Uint128>,
        denom: Option<String>,
    },
}

#[cw_serde]
pub struct AllOptionsResponse {
    pub options: Vec<String>,
}

#[cw_serde]
pub struct CheckOptionResponse {
    pub valid: bool,
}

#[cw_serde]
pub struct SampleGaugeMsgsResponse {
    pub execute: Vec<CosmosMsg>,
    /// Total native value represented by `execute` for an epoch-snapshot
    /// request. Legacy/hook-mode adapters may omit this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emitted_value: Option<Uint128>,
    /// Portion of the supplied epoch budget that remains unspent. Snapshot
    /// adapters return this together with `emitted_value`; legacy adapters may
    /// omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retained_value: Option<Uint128>,
}

/// Validates the allocation invariant adapters rely on before constructing
/// payment messages. Orchestrators should already produce this shape, but
/// adapters enforce it as a trust-boundary check for every direct query.
pub fn validate_selected_allocations(selected: &[(String, Decimal)]) -> StdResult<()> {
    let mut seen = HashSet::with_capacity(selected.len());
    let mut total = Decimal::zero();
    for (option, share) in selected {
        if option.is_empty() {
            return Err(StdError::generic_err("selected option must not be empty"));
        }
        if share.is_zero() {
            return Err(StdError::generic_err(format!(
                "selected share for {option} must be greater than zero"
            )));
        }
        if !seen.insert(option.as_str()) {
            return Err(StdError::generic_err(format!(
                "duplicate selected option: {option}"
            )));
        }
        total = total
            .checked_add(*share)
            .map_err(|_| StdError::generic_err("selected share sum overflowed"))?;
        if total > Decimal::one() {
            return Err(StdError::generic_err(format!(
                "selected shares exceed one: {total}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_allocation_validation_enforces_protocol_invariants() {
        validate_selected_allocations(&[
            ("one".to_owned(), Decimal::percent(60)),
            ("two".to_owned(), Decimal::percent(40)),
        ])
        .unwrap();
        validate_selected_allocations(&[("partial".to_owned(), Decimal::percent(25))]).unwrap();

        for invalid in [
            vec![(String::new(), Decimal::one())],
            vec![("zero".to_owned(), Decimal::zero())],
            vec![
                ("duplicate".to_owned(), Decimal::percent(20)),
                ("duplicate".to_owned(), Decimal::percent(20)),
            ],
            vec![
                ("one".to_owned(), Decimal::percent(60)),
                ("two".to_owned(), Decimal::percent(41)),
            ],
            vec![
                ("huge-one".to_owned(), Decimal::MAX),
                ("huge-two".to_owned(), Decimal::MAX),
            ],
        ] {
            assert!(validate_selected_allocations(&invalid).is_err());
        }
    }
}
