use crate::contract::ContractResponse;
use crate::error::contract_error;
use crate::msg::{AcceptSubscription, AssetExchange};
use crate::state::{accepted_subscriptions, config_read, pending_subscriptions};
use crate::state::{asset_exchange_storage, eligible_subscriptions};
use crate::sub_msg::{SubInstantiateMsg, SubQueryMsg, SubState};
use cosmwasm_std::MessageInfo;
use cosmwasm_std::Response;
use cosmwasm_std::{to_binary, Addr, Env, SubMsg, WasmMsg};
use cosmwasm_std::{Deps, DepsMut};
use provwasm_std::ProvenanceQuerier;
use provwasm_std::ProvenanceQuery;
use std::collections::HashSet;
use std::convert::TryInto;

pub fn try_propose_subscription(
    deps: DepsMut<ProvenanceQuery>,
    env: Env,
    info: MessageInfo,
    initial_commitment: Option<u64>,
) -> ContractResponse {
    let state = config_read(deps.storage).load()?;

    let eligible = if state.acceptable_accreditations.is_empty() {
        true
    } else {
        let attributes = attributes(deps.as_ref(), &info.sender);

        attributes
            .intersection(&state.acceptable_accreditations)
            .count()
            > 0
    };

    let create_sub = SubMsg::reply_always(
        WasmMsg::Instantiate {
            admin: Some(env.contract.address.into_string()),
            code_id: state.subscription_code_id,
            msg: to_binary(&SubInstantiateMsg {
                admin: state.recovery_admin,
                lp: info.sender,
                commitment_denom: state.commitment_denom,
                investment_denom: state.investment_denom,
                capital_denom: state.capital_denom,
                capital_per_share: state.capital_per_share,
                initial_commitment,
            })?,
            funds: vec![],
            label: String::from("establish subscription"),
        },
        if eligible { 1 } else { 0 },
    );

    Ok(Response::new()
        .add_submessage(create_sub)
        .add_attribute("eligible", format!("{}", eligible)))
}

fn attributes(deps: Deps<ProvenanceQuery>, lp: &Addr) -> HashSet<String> {
    ProvenanceQuerier::new(&deps.querier)
        .get_attributes(lp.clone(), None as Option<String>)
        .unwrap()
        .attributes
        .into_iter()
        .map(|attribute| attribute.name)
        .collect()
}

pub fn try_close_subscriptions(
    deps: DepsMut<ProvenanceQuery>,
    info: MessageInfo,
    subscriptions: HashSet<Addr>,
) -> ContractResponse {
    let state = config_read(deps.storage).load()?;
    let mut pending = pending_subscriptions(deps.storage)
        .may_load()?
        .unwrap_or_default();
    let mut eligible = eligible_subscriptions(deps.storage)
        .may_load()?
        .unwrap_or_default();
    let mut accepted = accepted_subscriptions(deps.storage)
        .may_load()?
        .unwrap_or_default();

    if info.sender != state.gp {
        return contract_error("only gp can close subscriptions");
    }

    for subscription in subscriptions {
        if !pending.remove(&subscription) && !eligible.remove(&subscription) {
            if accepted.contains(&subscription) {
                let remaining_commitment = deps
                    .querier
                    .query_balance(subscription.as_str(), state.commitment_denom.clone())
                    .map(|coin| coin.amount.u128())?;
                if remaining_commitment == 0 {
                    accepted.remove(&subscription);
                    asset_exchange_storage(deps.storage).remove(subscription.as_bytes());
                } else {
                    return contract_error("sub still has remaining commitment");
                }
            } else {
                return contract_error("no subscription pending or accepted to close");
            }
        }
    }

    pending_subscriptions(deps.storage).save(&pending)?;
    eligible_subscriptions(deps.storage).save(&eligible)?;
    accepted_subscriptions(deps.storage).save(&accepted)?;

    Ok(Response::new())
}

pub fn try_accept_subscriptions(
    deps: DepsMut<ProvenanceQuery>,
    info: MessageInfo,
    accepts: Vec<AcceptSubscription>,
) -> ContractResponse {
    let state = config_read(deps.storage).load()?;
    let mut pending = pending_subscriptions(deps.storage)
        .may_load()?
        .unwrap_or_default();
    let mut eligible = eligible_subscriptions(deps.storage)
        .may_load()?
        .unwrap_or_default();
    let mut accepted = accepted_subscriptions(deps.storage)
        .may_load()?
        .unwrap_or_default();

    if info.sender != state.gp {
        return contract_error("only gp can accept subscriptions");
    }

    for accept in accepts.iter() {
        if state.not_evenly_divisble(accept.commitment_in_capital) {
            return contract_error("accept amount must be evenly divisble by capital per share");
        }

        if eligible.contains(&accept.subscription) {
            eligible.remove(&accept.subscription);
        } else if pending.contains(&accept.subscription) {
            if !state.acceptable_accreditations.is_empty() {
                let sub_state: SubState = deps
                    .querier
                    .query_wasm_smart(accept.subscription.clone(), &SubQueryMsg::GetState {})?;

                let attributes: HashSet<String> = attributes(deps.as_ref(), &sub_state.lp);

                if attributes
                    .intersection(&state.acceptable_accreditations)
                    .count()
                    == 0
                {
                    return contract_error(
                        "subscription owner must have one of acceptable accreditations",
                    );
                }
            }

            pending.remove(&accept.subscription);
        } else {
            return contract_error("subscription must either be pending or eligible");
        }

        accepted.insert(accept.subscription.clone());
        asset_exchange_storage(deps.storage).save(
            accept.subscription.as_bytes(),
            &vec![AssetExchange {
                investment: None,
                commitment_in_shares: Some(
                    state
                        .capital_to_shares(accept.commitment_in_capital)
                        .try_into()?,
                ),
                capital: None,
                date: None,
            }],
        )?;
    }

    pending_subscriptions(deps.storage).save(&pending)?;
    eligible_subscriptions(deps.storage).save(&eligible)?;
    accepted_subscriptions(deps.storage).save(&accepted)?;

    Ok(Response::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::execute;
    use crate::contract::tests::default_deps;
    use crate::mock::{
        instantiate_args, msg_at_index, wasm_smart_mock_dependencies, MockContractQuerier,
    };
    use crate::msg::HandleMsg;
    use crate::msg::QueryMsg;
    use crate::msg::RaiseState;
    use crate::query::query;
    use crate::state::config;
    use crate::state::pending_subscriptions_read;
    use crate::state::tests::to_addresses;
    use crate::state::tests::{asset_exchange_storage_read, set_accepted};
    use crate::state::tests::{set_eligible, set_pending};
    use crate::state::State;
    use crate::state::{accepted_subscriptions_read, eligible_subscriptions_read};
    use cosmwasm_std::coins;
    use cosmwasm_std::from_binary;
    use cosmwasm_std::testing::mock_env;
    use cosmwasm_std::testing::mock_info;
    use cosmwasm_std::testing::MockApi;
    use cosmwasm_std::to_binary;
    use cosmwasm_std::Addr;
    use cosmwasm_std::ContractResult;
    use cosmwasm_std::MemoryStorage;
    use cosmwasm_std::OwnedDeps;
    use cosmwasm_std::SystemResult;

    pub fn mock_sub_state(
    ) -> OwnedDeps<MemoryStorage, MockApi, MockContractQuerier, ProvenanceQuery> {
        wasm_smart_mock_dependencies(&vec![], |_, _| {
            SystemResult::Ok(ContractResult::Ok(
                to_binary(&SubState {
                    admin: Addr::unchecked("marketpalace"),
                    lp: Addr::unchecked("lp"),
                    raise: Addr::unchecked("raise_1"),
                    commitment_denom: String::from("raise_1.commitment"),
                    investment_denom: String::from("raise_1.investment"),
                    capital_denom: String::from("stable_coin"),
                    capital_per_share: 1,
                })
                .unwrap(),
            ))
        })
    }

    #[test]
    fn propose_pending_subscription() {
        let mut deps = default_deps(None);

        // propose a sub as lp
        let res = execute(
            deps.as_mut(),
            mock_env(),
            mock_info("lp", &[]),
            HandleMsg::ProposeSubscription {
                initial_commitment: Some(100),
            },
        )
        .unwrap();

        // verify instantiate message
        assert_eq!(1, res.messages.len());
        let (admin, code_id, msg, funds, label) =
            instantiate_args::<SubInstantiateMsg>(msg_at_index(&res, 0));
        assert_eq!("cosmos2contract", admin.clone().unwrap());
        assert_eq!(&100, code_id);
        assert_eq!(
            SubInstantiateMsg {
                admin: Addr::unchecked("marketpalace"),
                lp: Addr::unchecked("lp"),
                commitment_denom: String::from("commitment_coin"),
                investment_denom: String::from("investment_coin"),
                capital_denom: String::from("stable_coin"),
                capital_per_share: 100,
                initial_commitment: Some(100),
            },
            msg
        );
        assert_eq!(0, funds.len());
        assert_eq!("establish subscription", label);
        assert_eq!(
            "false",
            res.attributes
                .iter()
                .find(|attr| attr.key == "eligible")
                .unwrap()
                .value
        );
    }

    #[test]
    fn propose_eligible_subscription() {
        let mut deps = default_deps(None);
        deps.querier.with_attributes("lp", &[("506c", "", "")]);

        // propose a sub as lp
        let res = execute(
            deps.as_mut(),
            mock_env(),
            mock_info("lp", &[]),
            HandleMsg::ProposeSubscription {
                initial_commitment: Some(100),
            },
        )
        .unwrap();

        // verify instantiate message
        assert_eq!(1, res.messages.len());
        let (admin, code_id, msg, funds, label) =
            instantiate_args::<SubInstantiateMsg>(msg_at_index(&res, 0));
        assert_eq!("cosmos2contract", admin.clone().unwrap());
        assert_eq!(&100, code_id);
        assert_eq!(
            SubInstantiateMsg {
                admin: Addr::unchecked("marketpalace"),
                lp: Addr::unchecked("lp"),
                commitment_denom: String::from("commitment_coin"),
                investment_denom: String::from("investment_coin"),
                capital_denom: String::from("stable_coin"),
                capital_per_share: 100,
                initial_commitment: Some(100),
            },
            msg
        );
        assert_eq!(0, funds.len());
        assert_eq!("establish subscription", label);
        assert_eq!(
            "true",
            res.attributes
                .iter()
                .find(|attr| attr.key == "eligible")
                .unwrap()
                .value
        );
    }

    #[test]
    fn close_pending_subscriptions() {
        let mut deps = default_deps(None);
        set_pending(&mut deps.storage, vec!["sub_1"]);

        // close sub as gp
        execute(
            deps.as_mut(),
            mock_env(),
            mock_info("gp", &[]),
            HandleMsg::CloseSubscriptions {
                subscriptions: to_addresses(vec!["sub_1"]),
            },
        )
        .unwrap();

        // verify pending sub is removed
        assert_eq!(
            0,
            pending_subscriptions_read(&deps.storage)
                .load()
                .unwrap()
                .len()
        )
    }

    #[test]
    fn close_eligible_subscriptions() {
        let mut deps = default_deps(None);
        set_eligible(&mut deps.storage, vec!["sub_1"]);

        // close sub as gp
        execute(
            deps.as_mut(),
            mock_env(),
            mock_info("gp", &[]),
            HandleMsg::CloseSubscriptions {
                subscriptions: to_addresses(vec!["sub_1"]),
            },
        )
        .unwrap();

        // verify pending sub is removed
        assert_eq!(
            0,
            eligible_subscriptions_read(&deps.storage)
                .load()
                .unwrap()
                .len()
        )
    }

    #[test]
    fn close_subscriptions_accepted_no_commitment() {
        let mut deps = default_deps(None);
        set_accepted(&mut deps.storage, vec!["sub_1"]);
        asset_exchange_storage(&mut deps.storage)
            .save(
                Addr::unchecked("sub_1").as_bytes(),
                &vec![AssetExchange {
                    investment: Some(1_000),
                    commitment_in_shares: Some(-1_000),
                    capital: Some(-1_000),
                    date: None,
                }],
            )
            .unwrap();

        // close sub as gp
        execute(
            deps.as_mut(),
            mock_env(),
            mock_info("gp", &[]),
            HandleMsg::CloseSubscriptions {
                subscriptions: to_addresses(vec!["sub_1"]),
            },
        )
        .unwrap();

        // verify accepted sub is removed
        assert_eq!(
            0,
            accepted_subscriptions_read(&deps.storage)
                .load()
                .unwrap()
                .len()
        );

        // verify remove any outstanding asset exchanges
        assert!(asset_exchange_storage_read(&deps.storage)
            .may_load(Addr::unchecked("sub_1").as_bytes())
            .unwrap()
            .is_none());
    }

    #[test]
    fn close_subscriptions_accepted_commitment() {
        let mut deps = default_deps(None);
        config(&mut deps.storage)
            .save(&&State::test_default())
            .unwrap();
        set_accepted(&mut deps.storage, vec!["sub_1"]);
        deps.querier
            .base
            .update_balance(Addr::unchecked("sub_1"), coins(100, "commitment_coin"));

        // close sub as gp
        let res = execute(
            deps.as_mut(),
            mock_env(),
            mock_info("gp", &[]),
            HandleMsg::CloseSubscriptions {
                subscriptions: to_addresses(vec!["sub_1"]),
            },
        );

        // verify error
        assert!(res.is_err());
    }

    #[test]
    fn close_subscriptions_bad_actor() {
        let mut deps = default_deps(None);
        set_accepted(&mut deps.storage, vec!["sub_1"]);

        // close sub as gp
        let res = execute(
            deps.as_mut(),
            mock_env(),
            mock_info("bad_actor", &[]),
            HandleMsg::CloseSubscriptions {
                subscriptions: to_addresses(vec!["sub_1"]),
            },
        );

        assert!(res.is_err());
    }

    #[test]
    fn close_subscriptions_not_found() {
        let mut deps = default_deps(None);
        set_accepted(&mut deps.storage, vec!["sub_1"]);

        // close sub as gp
        let res = execute(
            deps.as_mut(),
            mock_env(),
            mock_info("gp", &[]),
            HandleMsg::CloseSubscriptions {
                subscriptions: to_addresses(vec!["sub_2"]),
            },
        );

        assert!(res.is_err());
    }

    #[test]
    fn accept_pending_subscription() {
        let mut deps = mock_sub_state();
        deps.querier.base.with_attributes("lp", &[("506c", "", "")]);
        config(&mut deps.storage)
            .save(&State::test_default())
            .unwrap();
        set_pending(&mut deps.storage, vec!["sub_1"]);

        // accept pending sub as gp
        execute(
            deps.as_mut(),
            mock_env(),
            mock_info("gp", &[]),
            HandleMsg::AcceptSubscriptions {
                subscriptions: vec![AcceptSubscription {
                    subscription: Addr::unchecked("sub_1"),
                    commitment_in_capital: 20_000,
                }]
                .into_iter()
                .collect(),
            },
        )
        .unwrap();

        // assert that the sub has moved from pending review to accepted
        let res = query(deps.as_ref(), mock_env(), QueryMsg::GetState {}).unwrap();
        let state: RaiseState = from_binary(&res).unwrap();
        assert_eq!(0, state.pending_subscriptions.len());
        assert_eq!(1, state.accepted_subscriptions.len());

        // verify asset exchange exists
        assert_eq!(
            &AssetExchange {
                investment: None,
                commitment_in_shares: Some(200),
                capital: None,
                date: None,
            },
            asset_exchange_storage_read(&mut deps.storage)
                .load(Addr::unchecked("sub_1").as_bytes())
                .unwrap()
                .iter()
                .next()
                .unwrap()
        )
    }

    #[test]
    fn accept_eligible_subscription() {
        let mut deps = mock_sub_state();
        deps.querier.base.with_attributes("lp", &[("506c", "", "")]);
        config(&mut deps.storage)
            .save(&State::test_default())
            .unwrap();
        set_eligible(&mut deps.storage, vec!["sub_1"]);

        // accept pending sub as gp
        execute(
            deps.as_mut(),
            mock_env(),
            mock_info("gp", &[]),
            HandleMsg::AcceptSubscriptions {
                subscriptions: vec![AcceptSubscription {
                    subscription: Addr::unchecked("sub_1"),
                    commitment_in_capital: 20_000,
                }]
                .into_iter()
                .collect(),
            },
        )
        .unwrap();

        // assert that the sub has moved from pending review to accepted
        let res = query(deps.as_ref(), mock_env(), QueryMsg::GetState {}).unwrap();
        let state: RaiseState = from_binary(&res).unwrap();
        assert_eq!(0, state.eligible_subscriptions.len());
        assert_eq!(1, state.accepted_subscriptions.len());

        // verify asset exchange exists
        assert_eq!(
            &AssetExchange {
                investment: None,
                commitment_in_shares: Some(200),
                capital: None,
                date: None,
            },
            asset_exchange_storage_read(&mut deps.storage)
                .load(Addr::unchecked("sub_1").as_bytes())
                .unwrap()
                .iter()
                .next()
                .unwrap()
        )
    }

    #[test]
    fn accept_subscription_bad_actor() {
        let mut deps = mock_sub_state();
        set_pending(&mut deps.storage, vec!["sub_1"]);

        // accept pending sub as bad actor
        let res = execute(
            deps.as_mut(),
            mock_env(),
            mock_info("bad_actor", &[]),
            HandleMsg::AcceptSubscriptions {
                subscriptions: vec![AcceptSubscription {
                    subscription: Addr::unchecked("sub_1"),
                    commitment_in_capital: 20_000,
                }]
                .into_iter()
                .collect(),
            },
        );
        assert!(res.is_err());
    }

    #[test]
    fn accept_subscription_not_pending_or_eligible() {
        let mut deps = mock_sub_state();

        // accept pending sub as gp
        let res = execute(
            deps.as_mut(),
            mock_env(),
            mock_info("bad_actor", &[]),
            HandleMsg::AcceptSubscriptions {
                subscriptions: vec![AcceptSubscription {
                    subscription: Addr::unchecked("sub_1"),
                    commitment_in_capital: 20_000,
                }]
                .into_iter()
                .collect(),
            },
        );
        assert!(res.is_err());
    }

    #[test]
    fn accept_subscription_missing_acceptable_accreditation() {
        let mut deps = mock_sub_state();

        let mut state = State::test_default();
        state.acceptable_accreditations = vec![String::from("506c")].into_iter().collect();
        config(&mut deps.storage).save(&state).unwrap();

        set_pending(&mut deps.storage, vec!["sub_1"]);

        // accept pending sub as gp
        let res = execute(
            deps.as_mut(),
            mock_env(),
            mock_info("bad_actor", &[]),
            HandleMsg::AcceptSubscriptions {
                subscriptions: vec![AcceptSubscription {
                    subscription: Addr::unchecked("sub_1"),
                    commitment_in_capital: 20_000,
                }]
                .into_iter()
                .collect(),
            },
        );
        assert!(res.is_err());
    }

    #[test]
    fn accept_subscription_with_bad_amount() {
        let mut deps = mock_sub_state();
        set_pending(&mut deps.storage, vec!["sub_1"]);

        // accept pending sub as gp
        let res = execute(
            deps.as_mut(),
            mock_env(),
            mock_info("gp", &[]),
            HandleMsg::AcceptSubscriptions {
                subscriptions: vec![AcceptSubscription {
                    subscription: Addr::unchecked("sub_1"),
                    commitment_in_capital: 20_001,
                }]
                .into_iter()
                .collect(),
            },
        );
        assert!(res.is_err());
    }
}
