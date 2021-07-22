use cosmwasm_std::{
    attr, entry_point, from_binary, to_binary, Addr, Api, Binary, CosmosMsg, Decimal, Deps,
    DepsMut, Env, MessageInfo, Order, Querier, QuerierWrapper, QueryRequest, Response, StdError,
    StdResult, Storage, SubMsg, Uint128, WasmMsg, WasmQuery,
};
use cw_storage_plus::{Bound, U64Key};

use cw20::{Cw20ExecuteMsg, Cw20ReceiveMsg};
use mars::address_provider;
use mars::address_provider::msg::MarsContract;
use mars::error::MarsError;
use mars::helpers::{option_string_to_addr, read_be_u64};
use mars::xmars_token;

use crate::error::ContractError;
use crate::msg::{
    ConfigResponse, CreateOrUpdateConfig, ExecuteMsg, InstantiateMsg, MigrateMsg, MsgExecuteCall,
    ProposalExecuteCallResponse, ProposalInfo, ProposalVoteResponse, ProposalVotesResponse,
    ProposalsListResponse, QueryMsg, ReceiveMsg,
};
use crate::state::{
    Config, GlobalState, Proposal, ProposalExecuteCall, ProposalStatus, ProposalVote,
    ProposalVoteOption, CONFIG, GLOBAL_STATE, PROPOSALS, PROPOSAL_VOTES,
};

// Proposal validation attributes
const MIN_TITLE_LENGTH: usize = 4;
const MAX_TITLE_LENGTH: usize = 64;
const MIN_DESC_LENGTH: usize = 4;
const MAX_DESC_LENGTH: usize = 1024;
const MIN_LINK_LENGTH: usize = 12;
const MAX_LINK_LENGTH: usize = 128;

// INIT

#[entry_point]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> StdResult<Response> {
    // Destructuring a struct’s fields into separate variables in order to force
    // compile error if we add more params
    let CreateOrUpdateConfig {
        address_provider_address,
        proposal_voting_period,
        proposal_effective_delay,
        proposal_expiration_period,
        proposal_required_deposit,
        proposal_required_quorum,
        proposal_required_threshold,
    } = msg.config;

    // Check required fields are available
    let available = address_provider_address.is_some()
        && proposal_voting_period.is_some()
        && proposal_effective_delay.is_some()
        && proposal_expiration_period.is_some()
        && proposal_required_deposit.is_some()
        && proposal_required_quorum.is_some()
        && proposal_required_threshold.is_some();

    if !available {
        return Err(StdError::generic_err(
            "All params should be available during initialization",
        ));
    };

    // initialize Config
    let config = Config {
        address_provider_address: option_string_to_addr(
            deps.api,
            address_provider_address,
            Addr::unchecked(""),
        )?,
        proposal_voting_period: proposal_voting_period.unwrap(),
        proposal_effective_delay: proposal_effective_delay.unwrap(),
        proposal_expiration_period: proposal_expiration_period.unwrap(),
        proposal_required_deposit: proposal_required_deposit.unwrap(),
        proposal_required_quorum: proposal_required_quorum.unwrap(),
        proposal_required_threshold: proposal_required_threshold.unwrap(),
    };

    // Validate config
    config.validate()?;

    CONFIG.save(deps.storage, &config)?;

    // initialize State
    GLOBAL_STATE.save(deps.storage, &GlobalState { proposal_count: 0 })?;

    // Prepare response, should instantiate Mars and use the Register hook
    Ok(Response::default())
}

// HANDLERS

pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    match msg {
        ExecuteMsg::Receive(cw20_msg) => execute_receive_cw20(deps, env, info, cw20_msg),

        ExecuteMsg::CastVote { proposal_id, vote } => {
            execute_cast_vote(deps, env, info, proposal_id, vote)
        }
        ExecuteMsg::EndProposal { proposal_id } => {
            execute_end_proposal(deps, env, info, proposal_id)
        }

        ExecuteMsg::ExecuteProposal { proposal_id } => {
            execute_execute_proposal(deps, env, info, proposal_id)
        }

        ExecuteMsg::UpdateConfig { config } => execute_update_config(deps, env, info, config),
    }
}

/// cw20 receive implementation
pub fn execute_receive_cw20(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    cw20_msg: Cw20ReceiveMsg,
) -> Result<Response, ContractError> {
    match from_binary(&cw20_msg.msg)? {
        ReceiveMsg::SubmitProposal {
            title,
            description,
            link,
            execute_calls,
        } => execute_submit_proposal(
            deps,
            env,
            info,
            cw20_msg.sender,
            cw20_msg.amount,
            title,
            description,
            link,
            execute_calls,
        ),
    }
}

pub fn execute_submit_proposal(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    submitter_address_unchecked: String,
    deposit_amount: Uint128,
    title: String,
    description: String,
    option_link: Option<String>,
    option_msg_execute_calls: Option<Vec<MsgExecuteCall>>,
) -> Result<Response, ContractError> {
    // Validate title
    if title.len() < MIN_TITLE_LENGTH {
        return Err(ContractError::invalid_proposal("title too short"));
    }
    if title.len() > MAX_TITLE_LENGTH {
        return Err(ContractError::invalid_proposal("title too long"));
    }

    // Validate description
    if description.len() < MIN_DESC_LENGTH {
        return Err(ContractError::invalid_proposal("description too short"));
    }
    if description.len() > MAX_DESC_LENGTH {
        return Err(ContractError::invalid_proposal("description too long"));
    }

    // Validate Link
    if let Some(link) = &option_link {
        if link.len() < MIN_LINK_LENGTH {
            return Err(ContractError::invalid_proposal("Link too short"));
        }
        if link.len() > MAX_LINK_LENGTH {
            return Err(ContractError::invalid_proposal("Link too long"));
        }
    }

    let config = CONFIG.load(deps.storage)?;
    let mars_token_address = address_provider::helpers::query_address(
        &deps.querier,
        config.address_provider_address,
        MarsContract::MarsToken,
    )?;

    let is_mars = info.sender == mars_token_address;
    // Validate deposit amount
    if (deposit_amount < config.proposal_required_deposit) || !is_mars {
        return Err(ContractError::invalid_proposal(format!(
            "Must deposit at least {} Mars tokens",
            config.proposal_required_deposit
        )));
    }

    // Update proposal totals
    let mut global_state = GLOBAL_STATE.load(deps.storage)?;
    global_state.proposal_count += 1;
    GLOBAL_STATE.save(deps.storage, &global_state)?;

    // Transform MsgExecuteCalls into ProposalExecuteCalls by validating the contract address
    let option_proposal_execute_calls = if let Some(calls) = option_msg_execute_calls {
        let mut proposal_execute_calls: Vec<ProposalExecuteCall> = vec![];
        for call in calls {
            proposal_execute_calls.push(ProposalExecuteCall {
                execution_order: call.execution_order,
                target_contract_address: deps.api.addr_validate(&call.target_contract_address)?,
                msg: call.msg,
            });
        }
        Some(proposal_execute_calls)
    } else {
        None
    };

    let new_proposal = Proposal {
        submitter_address: deps.api.addr_validate(&submitter_address_unchecked)?,
        status: ProposalStatus::Active,
        for_votes: Uint128::zero(),
        against_votes: Uint128::zero(),
        start_height: env.block.height,
        end_height: env.block.height + config.proposal_voting_period,
        title,
        description,
        link: option_link,
        execute_calls: option_proposal_execute_calls,
        deposit_amount,
    };
    PROPOSALS.save(
        deps.storage,
        U64Key::new(global_state.proposal_count),
        &new_proposal,
    )?;

    Ok(Response {
        messages: vec![],
        attributes: vec![
            attr("action", "submit_proposal"),
            attr("proposal_submitter", submitter_address_unchecked),
            attr("proposal_id", &global_state.proposal_count),
            attr("proposal_end_height", &new_proposal.end_height),
        ],
        events: vec![],
        data: None,
    })
}

pub fn execute_cast_vote(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    proposal_id: u64,
    vote_option: ProposalVoteOption,
) -> Result<Response, ContractError> {
    let proposal_path = PROPOSALS.key(U64Key::new(proposal_id));
    let mut proposal = proposal_path.load(deps.storage)?;
    if proposal.status != ProposalStatus::Active {
        return Err(ContractError::ProposalNotActive {});
    }

    if env.block.height > proposal.end_height {
        return Err(ContractError::VoteVotingPeriodEnded {});
    }

    let proposal_vote_path = PROPOSAL_VOTES.key((U64Key::new(proposal_id), &info.sender));

    if proposal_vote_path.may_load(deps.storage)?.is_some() {
        return Err(ContractError::VoteUserAlreadyVoted {});
    }

    let config = CONFIG.load(deps.storage)?;
    let xmars_token_address = address_provider::helpers::query_address(
        &deps.querier,
        config.address_provider_address,
        MarsContract::XMarsToken,
    )?;

    let balance_at_block = proposal.start_height - 1;
    let voting_power = xmars_get_balance_at(
        &deps.querier,
        xmars_token_address,
        info.sender.clone(),
        balance_at_block,
    )?;

    if voting_power == Uint128::zero() {
        return Err(ContractError::VoteNoVotingPower {
            block: balance_at_block,
        });
    }

    match vote_option {
        ProposalVoteOption::For => proposal.for_votes += voting_power,
        ProposalVoteOption::Against => proposal.against_votes += voting_power,
    };

    proposal_vote_path.save(
        deps.storage,
        &ProposalVote {
            option: vote_option.clone(),
            power: voting_power,
        },
    )?;

    proposal_path.save(deps.storage, &proposal)?;

    Ok(Response {
        messages: vec![],
        attributes: vec![
            attr("action", "cast_vote"),
            attr("proposal_id", proposal_id),
            attr("voter", &info.sender),
            attr("vote", vote_option),
            attr("voting_power", voting_power),
        ],
        events: vec![],
        data: None,
    })
}

pub fn execute_end_proposal(
    deps: DepsMut,
    env: Env,
    _info: MessageInfo,
    proposal_id: u64,
) -> Result<Response, ContractError> {
    let proposal_path = PROPOSALS.key(U64Key::new(proposal_id));
    let mut proposal = proposal_path.load(deps.storage)?;

    if proposal.status != ProposalStatus::Active {
        return Err(ContractError::ProposalNotActive {});
    }

    if env.block.height <= proposal.end_height {
        return Err(ContractError::EndProposalVotingPeriodNotEnded {});
    }

    let config = CONFIG.load(deps.storage)?;
    let mars_contracts = vec![
        MarsContract::MarsToken,
        MarsContract::Staking,
        MarsContract::XMarsToken,
    ];
    let mut addresses_query = address_provider::helpers::query_addresses(
        &deps.querier,
        config.address_provider_address,
        mars_contracts,
    )?;
    let xmars_token_address = addresses_query.pop().unwrap();
    let staking_address = addresses_query.pop().unwrap();
    let mars_token_address = addresses_query.pop().unwrap();

    // Compute proposal quorum and threshold
    let for_votes = proposal.for_votes;
    let against_votes = proposal.against_votes;
    let total_votes = for_votes + against_votes;
    let total_voting_power = xmars_get_total_supply_at(
        &deps.querier,
        xmars_token_address,
        proposal.start_height - 1,
    )?;

    let mut proposal_quorum: Decimal = Decimal::zero();
    let mut proposal_threshold: Decimal = Decimal::zero();
    if total_voting_power > Uint128::zero() {
        proposal_quorum = Decimal::from_ratio(total_votes, total_voting_power);
    }
    if total_votes > Uint128::zero() {
        proposal_threshold = Decimal::from_ratio(for_votes, total_votes);
    }

    // Determine proposal result
    let (new_proposal_status, log_proposal_result, messages) = if proposal_quorum
        >= config.proposal_required_quorum
        && proposal_threshold >= config.proposal_required_threshold
    {
        // if quorum and threshold are met then proposal passes
        // refund deposit amount to submitter
        let msg = SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: mars_token_address.into(),
            funds: vec![],
            msg: to_binary(&Cw20ExecuteMsg::Transfer {
                recipient: proposal.submitter_address.to_string(),
                amount: proposal.deposit_amount,
            })?,
        }));

        (ProposalStatus::Passed, "passed", vec![msg])
    } else {
        // Else proposal is rejected
        let msg = SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: mars_token_address.into(),
            msg: to_binary(&Cw20ExecuteMsg::Transfer {
                recipient: staking_address.into(),
                amount: proposal.deposit_amount,
            })?,
            funds: vec![],
        }));

        (ProposalStatus::Rejected, "rejected", vec![msg])
    };

    // Update proposal status
    proposal.status = new_proposal_status;
    proposal_path.save(deps.storage, &proposal)?;

    Ok(Response {
        messages,
        attributes: vec![
            attr("action", "end_proposal"),
            attr("proposal_id", proposal_id),
            attr("proposal_result", log_proposal_result),
        ],
        events: vec![],
        data: None,
    })
}

pub fn execute_execute_proposal(
    deps: DepsMut,
    env: Env,
    _info: MessageInfo,
    proposal_id: u64,
) -> Result<Response, ContractError> {
    let proposal_path = PROPOSALS.key(U64Key::new(proposal_id));
    let mut proposal = proposal_path.load(deps.storage)?;

    if proposal.status != ProposalStatus::Passed {
        return Err(ContractError::ExecuteProposalNotPassed {});
    }

    let config = CONFIG.load(deps.storage)?;
    if env.block.height < (proposal.end_height + config.proposal_effective_delay) {
        return Err(ContractError::ExecuteProposalDelayNotEnded {});
    }
    if env.block.height
        > (proposal.end_height
            + config.proposal_effective_delay
            + config.proposal_expiration_period)
    {
        return Err(ContractError::ExecuteProposalExpired {});
    }

    proposal.status = ProposalStatus::Executed;
    proposal_path.save(deps.storage, &proposal)?;

    let messages: Vec<SubMsg> = if let Some(mut proposal_execute_calls) = proposal.execute_calls {
        let mut ret = Vec::<SubMsg>::with_capacity(proposal_execute_calls.len());

        proposal_execute_calls.sort_by(|a, b| a.execution_order.cmp(&b.execution_order));

        for execute_call in proposal_execute_calls {
            ret.push(SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: execute_call.target_contract_address.into(),
                msg: execute_call.msg,
                funds: vec![],
            })));
        }

        ret
    } else {
        vec![]
    };

    Ok(Response {
        messages,
        events: vec![],
        attributes: vec![
            attr("action", "execute_proposal"),
            attr("proposal_id", proposal_id),
        ],
        data: None,
    })
}

/// Update config
pub fn execute_update_config(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    new_config: CreateOrUpdateConfig,
) -> Result<Response, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;

    // In council, config can be updated only by itself (through an approved proposal)
    // instead of by it's owner
    if info.sender != env.contract.address {
        return Err(MarsError::Unauthorized {}.into());
    }

    // Destructuring a struct’s fields into separate variables in order to force
    // compile error if we add more params
    let CreateOrUpdateConfig {
        address_provider_address,

        proposal_voting_period,
        proposal_effective_delay,
        proposal_expiration_period,
        proposal_required_deposit,
        proposal_required_quorum,
        proposal_required_threshold,
    } = new_config;

    // Update config
    config.address_provider_address = option_string_to_addr(
        deps.api,
        address_provider_address,
        config.address_provider_address,
    )?;

    config.proposal_voting_period = proposal_voting_period.unwrap_or(config.proposal_voting_period);
    config.proposal_effective_delay =
        proposal_effective_delay.unwrap_or(config.proposal_effective_delay);
    config.proposal_expiration_period =
        proposal_expiration_period.unwrap_or(config.proposal_expiration_period);
    config.proposal_required_deposit =
        proposal_required_deposit.unwrap_or(config.proposal_required_deposit);
    config.proposal_required_quorum =
        proposal_required_quorum.unwrap_or(config.proposal_required_quorum);
    config.proposal_required_threshold =
        proposal_required_threshold.unwrap_or(config.proposal_required_threshold);

    // Validate config
    config.validate()?;

    CONFIG.save(deps.storage, &config)?;

    Ok(Response::default())
}

// QUERIES

// Pagination defaults
const PAGINATION_DEFAULT_LIMIT: u32 = 10;
const PAGINATION_MAX_LIMIT: u32 = 30;

pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => to_binary(&query_config(deps)?),
        QueryMsg::Proposals { start, limit } => to_binary(&query_proposals(deps, start, limit)?),
        QueryMsg::Proposal { proposal_id } => to_binary(&query_proposal(deps, proposal_id)?),
        QueryMsg::ProposalVotes {
            proposal_id,
            start_after,
            limit,
        } => to_binary(&query_proposal_votes(
            deps,
            proposal_id,
            start_after,
            limit,
        )?),
    }
}

fn query_config(deps: Deps) -> StdResult<ConfigResponse> {
    let config = CONFIG.load(deps.storage)?;

    Ok(ConfigResponse {
        address_provider_address: config.address_provider_address.into(),
        proposal_voting_period: config.proposal_voting_period,
        proposal_effective_delay: config.proposal_effective_delay,
        proposal_expiration_period: config.proposal_expiration_period,
        proposal_required_deposit: config.proposal_required_deposit,
        proposal_required_quorum: config.proposal_required_quorum,
        proposal_required_threshold: config.proposal_required_threshold,
    })
}

fn query_proposals(
    deps: Deps,
    start_from: Option<u64>,
    option_limit: Option<u32>,
) -> StdResult<ProposalsListResponse> {
    let global_state = GLOBAL_STATE.load(deps.storage)?;

    let option_start = start_from.map(|start| Bound::inclusive(U64Key::new(start)));
    let limit = option_limit
        .unwrap_or(PAGINATION_DEFAULT_LIMIT)
        .min(PAGINATION_MAX_LIMIT) as usize;

    let proposals_list: StdResult<Vec<_>> = PROPOSALS
        .range(deps.storage, option_start, None, Order::Ascending)
        .take(limit)
        .map(|item| {
            let (k, v) = item?;

            Ok(ProposalInfo {
                proposal_id: read_be_u64(&k)?,
                submitter_address: v.submitter_address.into(),
                status: v.status,
                for_votes: v.for_votes,
                against_votes: v.against_votes,
                start_height: v.start_height,
                end_height: v.end_height,
                title: v.title,
                description: v.description,
                link: v.link,
                execute_calls: map_execute_calls_response(v.execute_calls)?,
                deposit_amount: v.deposit_amount,
            })
        })
        .collect();

    Ok(ProposalsListResponse {
        proposal_count: global_state.proposal_count,
        proposal_list: proposals_list?,
    })
}

fn query_proposal(deps: Deps, proposal_id: u64) -> StdResult<ProposalInfo> {
    let proposal = PROPOSALS.load(deps.storage, U64Key::new(proposal_id))?;

    Ok(ProposalInfo {
        proposal_id,
        submitter_address: proposal.submitter_address.into(),
        status: proposal.status,
        for_votes: proposal.for_votes,
        against_votes: proposal.against_votes,
        start_height: proposal.start_height,
        end_height: proposal.end_height,
        title: proposal.title,
        description: proposal.description,
        link: proposal.link,
        execute_calls: map_execute_calls_response(proposal.execute_calls)?,
        deposit_amount: proposal.deposit_amount,
    })
}

fn query_proposal_votes(
    deps: Deps,
    proposal_id: u64,
    start_after: Option<String>,
    option_limit: Option<u32>,
) -> StdResult<ProposalVotesResponse> {
    let limit = option_limit
        .unwrap_or(PAGINATION_DEFAULT_LIMIT)
        .min(PAGINATION_MAX_LIMIT) as usize;
    let option_start = start_after.map(Bound::exclusive);

    let votes: StdResult<Vec<ProposalVoteResponse>> = PROPOSAL_VOTES
        .prefix(U64Key::new(proposal_id))
        .range(deps.storage, option_start, None, Order::Ascending)
        .take(limit)
        .map(|vote| {
            let (k, v) = vote?;
            let voter_address = String::from_utf8(k)?;

            Ok(ProposalVoteResponse {
                voter_address,
                option: v.option,
                power: v.power,
            })
        })
        .collect();

    Ok(ProposalVotesResponse {
        proposal_id,
        votes: votes?,
    })
}

// MIGRATION

pub fn migrate<S: Storage, A: Api, Q: Querier>(
    _deps: DepsMut,
    _env: Env,
    _msg: MigrateMsg,
) -> StdResult<Response> {
    Ok(Response::default())
}

// HELPERS
//
fn xmars_get_total_supply_at(
    querier: &QuerierWrapper,
    xmars_address: Addr,
    block: u64,
) -> StdResult<Uint128> {
    let query: xmars_token::msg::TotalSupplyResponse =
        querier.query(&QueryRequest::Wasm(WasmQuery::Smart {
            contract_addr: xmars_address.into(),
            msg: to_binary(&xmars_token::msg::QueryMsg::TotalSupplyAt { block })?,
        }))?;

    Ok(query.total_supply)
}

fn xmars_get_balance_at(
    querier: &QuerierWrapper,
    xmars_address: Addr,
    user_address: Addr,
    block: u64,
) -> StdResult<Uint128> {
    let query: cw20::BalanceResponse = querier.query(&QueryRequest::Wasm(WasmQuery::Smart {
        contract_addr: xmars_address.into(),
        msg: to_binary(&xmars_token::msg::QueryMsg::BalanceAt {
            address: user_address.to_string(),
            block,
        })?,
    }))?;

    Ok(query.balance)
}

fn map_execute_calls_response(
    execute_calls: Option<Vec<ProposalExecuteCall>>,
) -> Result<Option<Vec<ProposalExecuteCallResponse>>, StdError> {
    Ok(match execute_calls {
        Some(execute_calls) => execute_calls
            .iter()
            .map(|execute_call| {
                Some(ProposalExecuteCallResponse {
                    execution_order: execute_call.execution_order,
                    target_contract_address: execute_call.target_contract_address.to_string(),
                    msg: execute_call.msg.clone(),
                })
            })
            .collect(),
        None => Option::None,
    })
}

// TESTS

#[cfg(test)]
mod tests {
    use super::*;
    use cosmwasm_std::testing::{MockApi, MockStorage, MOCK_CONTRACT_ADDR};
    use cosmwasm_std::{Coin, OwnedDeps};
    use mars::testing::{
        assert_generic_error_message, mock_dependencies, mock_env, mock_info, MarsMockQuerier,
        MockEnvParams,
    };

    use crate::msg::ExecuteMsg::UpdateConfig;

    const TEST_PROPOSAL_VOTING_PERIOD: u64 = 2000;
    const TEST_PROPOSAL_EFFECTIVE_DELAY: u64 = 200;
    const TEST_PROPOSAL_EXPIRATION_PERIOD: u64 = 300;
    const TEST_PROPOSAL_REQUIRED_DEPOSIT: Uint128 = Uint128::new(10000);

    #[test]
    fn test_proper_initialization() {
        let mut deps = mock_dependencies(&[]);

        // init config with empty params
        {
            let empty_config = CreateOrUpdateConfig {
                address_provider_address: None,

                proposal_voting_period: None,
                proposal_effective_delay: None,
                proposal_expiration_period: None,
                proposal_required_deposit: None,
                proposal_required_threshold: None,
                proposal_required_quorum: None,
            };
            let msg = InstantiateMsg {
                config: empty_config,
            };
            let info = mock_info("someone");
            let env = cosmwasm_std::testing::mock_env();
            let response = instantiate(deps.as_mut(), env, info, msg);
            assert_generic_error_message(
                response,
                "All params should be available during initialization",
            );
        }

        // init with proposal_required_quorum, proposal_required_threshold greater than 1
        {
            let config = CreateOrUpdateConfig {
                address_provider_address: Some(String::from("address_provider")),

                proposal_voting_period: Some(1),
                proposal_effective_delay: Some(1),
                proposal_expiration_period: Some(1),
                proposal_required_deposit: Some(Uint128::new(1)),
                proposal_required_quorum: Some(Decimal::from_ratio(11u128, 10u128)),
                proposal_required_threshold: Some(Decimal::from_ratio(11u128, 10u128)),
            };
            let msg = InstantiateMsg { config };
            let env = cosmwasm_std::testing::mock_env();
            let info = mock_info("someone");
            let response = instantiate(deps.as_mut(), env, info, msg);
            assert_generic_error_message(
                response,
                "[proposal_required_quorum, proposal_required_threshold] should be less or equal 1. \
                    Invalid params: [proposal_required_quorum, proposal_required_threshold]",
            );
        }

        // Successful Init
        {
            let config = CreateOrUpdateConfig {
                address_provider_address: Some(String::from("address_provider")),

                proposal_voting_period: Some(1),
                proposal_effective_delay: Some(1),
                proposal_expiration_period: Some(1),
                proposal_required_deposit: Some(Uint128::new(1)),
                proposal_required_threshold: Some(Decimal::one()),
                proposal_required_quorum: Some(Decimal::one()),
            };
            let msg = InstantiateMsg { config };
            let env = mock_env(MockEnvParams::default());
            let info = mock_info("someone");

            let res = instantiate(deps.as_mut(), env, info, msg).unwrap();
            assert_eq!(0, res.messages.len());

            let config = CONFIG.load(&deps.storage).unwrap();
            assert_eq!(
                Addr::unchecked("address_provider"),
                config.address_provider_address
            );

            let global_state = GLOBAL_STATE.load(&deps.storage).unwrap();
            assert_eq!(global_state.proposal_count, 0);
        }
    }

    #[test]
    fn test_update_config() {
        let mut deps = mock_dependencies(&[]);

        // *
        // init config with valid params
        // *
        let init_config = CreateOrUpdateConfig {
            address_provider_address: Some(String::from("address_provider")),

            proposal_voting_period: Some(10),
            proposal_effective_delay: Some(11),
            proposal_expiration_period: Some(12),
            proposal_required_deposit: Some(Uint128::new(111)),
            proposal_required_threshold: Some(Decimal::one()),
            proposal_required_quorum: Some(Decimal::one()),
        };
        let msg = InstantiateMsg {
            config: init_config.clone(),
        };
        let env = cosmwasm_std::testing::mock_env();
        let info = mock_info(MOCK_CONTRACT_ADDR);
        let _res = instantiate(deps.as_mut(), env, info, msg).unwrap();

        // *
        // update config with proposal_required_quorum, proposal_required_threshold greater than 1
        // *
        {
            let config = CreateOrUpdateConfig {
                proposal_required_quorum: Some(Decimal::from_ratio(11u128, 10u128)),
                proposal_required_threshold: Some(Decimal::from_ratio(11u128, 10u128)),
                ..init_config.clone()
            };
            let msg = UpdateConfig { config };
            let env = cosmwasm_std::testing::mock_env();
            let info = mock_info(MOCK_CONTRACT_ADDR);
            let response = execute(deps.as_mut(), env, info, msg).unwrap_err();
            assert_eq!(
                response,
                StdError::generic_err("[proposal_required_quorum, proposal_required_threshold] should be less or equal 1. \
                    Invalid params: [proposal_required_quorum, proposal_required_threshold]",
                ).into()
            );
        }

        // *
        // only council itself is authorized
        // *
        {
            let msg = UpdateConfig {
                config: init_config,
            };
            let env = cosmwasm_std::testing::mock_env();
            let info = mock_info("somebody");
            let error_res = execute(deps.as_mut(), env, info, msg).unwrap_err();
            assert_eq!(error_res, MarsError::Unauthorized {}.into());
        }

        // *
        // update config with all new params
        // *
        {
            let config = CreateOrUpdateConfig {
                address_provider_address: Some(String::from("new_address_provider")),

                proposal_voting_period: Some(101),
                proposal_effective_delay: Some(111),
                proposal_expiration_period: Some(121),
                proposal_required_deposit: Some(Uint128::new(1111)),
                proposal_required_threshold: Some(Decimal::from_ratio(4u128, 5u128)),
                proposal_required_quorum: Some(Decimal::from_ratio(1u128, 5u128)),
            };
            let msg = UpdateConfig {
                config: config.clone(),
            };
            let env = cosmwasm_std::testing::mock_env();
            let info = mock_info(MOCK_CONTRACT_ADDR);
            let res = execute(deps.as_mut(), env, info, msg).unwrap();
            assert_eq!(0, res.messages.len());

            // Read config from state
            let new_config = CONFIG.load(&deps.storage).unwrap();

            assert_eq!(
                new_config.address_provider_address,
                Addr::unchecked("new_address_provider")
            );
            assert_eq!(
                new_config.proposal_voting_period,
                config.proposal_voting_period.unwrap()
            );
            assert_eq!(
                new_config.proposal_effective_delay,
                config.proposal_effective_delay.unwrap()
            );
            assert_eq!(
                new_config.proposal_expiration_period,
                config.proposal_expiration_period.unwrap()
            );
            assert_eq!(
                new_config.proposal_required_deposit,
                config.proposal_required_deposit.unwrap()
            );
            assert_eq!(
                new_config.proposal_required_threshold,
                config.proposal_required_threshold.unwrap()
            );
            assert_eq!(
                new_config.proposal_required_quorum,
                config.proposal_required_quorum.unwrap()
            );
        }
    }

    #[test]
    fn test_submit_proposal_invalid_params() {
        let mut deps = th_setup(&[]);

        // *
        // Invalid title
        // *
        {
            let msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
                msg: to_binary(&ReceiveMsg::SubmitProposal {
                    title: "a".to_string(),
                    description: "A valid description".to_string(),
                    link: None,
                    execute_calls: None,
                })
                .unwrap(),
                sender: String::from("submitter"),
                amount: Uint128::new(2_000_000),
            });
            let env = mock_env(MockEnvParams::default());
            let info = mock_info("mars_token");
            let response = execute(deps.as_mut(), env, info, msg).unwrap_err();
            assert_eq!(response, ContractError::invalid_proposal("title too short"));
        }

        {
            let msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
                msg: to_binary(&ReceiveMsg::SubmitProposal {
                    title: (0..100).map(|_| "a").collect::<String>(),
                    description: "A valid description".to_string(),
                    link: None,
                    execute_calls: None,
                })
                .unwrap(),
                sender: String::from("submitter"),
                amount: Uint128::new(2_000_000),
            });
            let env = mock_env(MockEnvParams::default());
            let info = mock_info("mars_token");
            let response = execute(deps.as_mut(), env, info, msg).unwrap_err();
            assert_eq!(response, ContractError::invalid_proposal("title too long"));
        }

        // *
        // Invalid description
        // *
        {
            let msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
                msg: to_binary(&ReceiveMsg::SubmitProposal {
                    title: "A valid Title".to_string(),
                    description: "a".to_string(),
                    link: None,
                    execute_calls: None,
                })
                .unwrap(),
                sender: String::from("submitter"),
                amount: Uint128::new(2_000_000),
            });
            let env = mock_env(MockEnvParams::default());
            let info = mock_info("mars_token");
            let response = execute(deps.as_mut(), env, info, msg).unwrap_err();
            assert_eq!(
                response,
                ContractError::invalid_proposal("description too short")
            );
        }

        {
            let msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
                msg: to_binary(&ReceiveMsg::SubmitProposal {
                    title: "A valid Title".to_string(),
                    description: (0..1030).map(|_| "a").collect::<String>(),
                    link: None,
                    execute_calls: None,
                })
                .unwrap(),
                sender: String::from("submitter"),
                amount: Uint128::new(2_000_000),
            });
            let env = mock_env(MockEnvParams::default());
            let info = mock_info("mars_token");
            let response = execute(deps.as_mut(), env, info, msg).unwrap_err();
            assert_eq!(
                response,
                ContractError::invalid_proposal("description too long")
            );
        }

        // *
        // Invalid link
        // *
        {
            let msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
                msg: to_binary(&ReceiveMsg::SubmitProposal {
                    title: "A valid Title".to_string(),
                    description: "A valid description".to_string(),
                    link: Some("a".to_string()),
                    execute_calls: None,
                })
                .unwrap(),
                sender: String::from("submitter"),
                amount: Uint128::new(2_000_000),
            });
            let env = mock_env(MockEnvParams::default());
            let info = mock_info("mars_token");
            let response = execute(deps.as_mut(), env, info, msg).unwrap_err();
            assert_eq!(response, ContractError::invalid_proposal("Link too short"));
        }

        {
            let msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
                msg: to_binary(&ReceiveMsg::SubmitProposal {
                    title: "A valid Title".to_string(),
                    description: "A valid description".to_string(),
                    link: Some((0..150).map(|_| "a").collect::<String>()),
                    execute_calls: None,
                })
                .unwrap(),
                sender: String::from("submitter"),
                amount: Uint128::new(2_000_000),
            });
            let env = mock_env(MockEnvParams::default());
            let info = mock_info("mars_token");
            let response = execute(deps.as_mut(), env, info, msg).unwrap_err();
            assert_eq!(response, ContractError::invalid_proposal("Link too long"));
        }

        // *
        // Invalid deposit amount
        // *
        {
            let msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
                msg: to_binary(&ReceiveMsg::SubmitProposal {
                    title: "A valid Title".to_string(),
                    description: "A valid description".to_string(),
                    link: None,
                    execute_calls: None,
                })
                .unwrap(),
                sender: String::from("submitter"),
                amount: TEST_PROPOSAL_REQUIRED_DEPOSIT - Uint128::new(100),
            });
            let env = mock_env(MockEnvParams::default());
            let info = mock_info("mars_token");
            let response = execute(deps.as_mut(), env, info, msg).unwrap_err();
            assert_eq!(
                response,
                ContractError::invalid_proposal("Must deposit at least 10000 Mars tokens")
            );
        }

        // *
        // Invalid deposit currency
        // *
        {
            let msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
                msg: to_binary(&ReceiveMsg::SubmitProposal {
                    title: "A valid Title".to_string(),
                    description: "A valid description".to_string(),
                    link: None,
                    execute_calls: None,
                })
                .unwrap(),
                sender: String::from("submitter"),
                amount: TEST_PROPOSAL_REQUIRED_DEPOSIT,
            });
            let env = mock_env(MockEnvParams::default());
            let info = mock_info("other_token");
            let res_error = execute(deps.as_mut(), env, info, msg).unwrap_err();
            assert_eq!(
                res_error,
                ContractError::invalid_proposal("Must deposit at least 10000 Mars tokens")
            );
        }
    }

    #[test]
    fn test_submit_proposal() {
        let mut deps = th_setup(&[]);
        let submitter_address = Addr::unchecked("submitter");

        // Submit Proposal without link or call data
        let msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
            msg: to_binary(&ReceiveMsg::SubmitProposal {
                title: "A valid title".to_string(),
                description: "A valid description".to_string(),
                link: None,
                execute_calls: None,
            })
            .unwrap(),
            sender: submitter_address.to_string(),
            amount: TEST_PROPOSAL_REQUIRED_DEPOSIT,
        });
        let env = mock_env(MockEnvParams {
            block_height: 100_000,
            ..Default::default()
        });
        let info = mock_info("mars_token");
        let res = execute(deps.as_mut(), env, info, msg).unwrap();
        let expected_end_height = 100_000 + TEST_PROPOSAL_VOTING_PERIOD;
        assert_eq!(
            res.attributes,
            vec![
                attr("action", "submit_proposal"),
                attr("proposal_submitter", "submitter"),
                attr("proposal_id", 1),
                attr("proposal_end_height", expected_end_height),
            ]
        );

        let global_state = GLOBAL_STATE.load(&deps.storage).unwrap();
        assert_eq!(global_state.proposal_count, 1);

        let proposal = PROPOSALS.load(&deps.storage, U64Key::new(1_u64)).unwrap();
        assert_eq!(proposal.submitter_address, submitter_address);
        assert_eq!(proposal.status, ProposalStatus::Active);
        assert_eq!(proposal.for_votes, Uint128::new(0));
        assert_eq!(proposal.against_votes, Uint128::new(0));
        assert_eq!(proposal.start_height, 100_000);
        assert_eq!(proposal.end_height, expected_end_height);
        assert_eq!(proposal.title, "A valid title");
        assert_eq!(proposal.description, "A valid description");
        assert_eq!(proposal.link, None);
        assert_eq!(proposal.execute_calls, None);
        assert_eq!(proposal.deposit_amount, TEST_PROPOSAL_REQUIRED_DEPOSIT);

        // Submit Proposal with link and call data
        let msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
            msg: to_binary(&ReceiveMsg::SubmitProposal {
                title: "A valid title".to_string(),
                description: "A valid description".to_string(),
                link: Some("https://www.avalidlink.com".to_string()),
                execute_calls: Some(vec![MsgExecuteCall {
                    execution_order: 0,
                    target_contract_address: String::from(MOCK_CONTRACT_ADDR),
                    msg: to_binary(&ExecuteMsg::UpdateConfig {
                        config: CreateOrUpdateConfig::default(),
                    })
                    .unwrap(),
                }]),
            })
            .unwrap(),
            sender: submitter_address.to_string(),
            amount: TEST_PROPOSAL_REQUIRED_DEPOSIT,
        });
        let env = mock_env(MockEnvParams {
            block_height: 100_000,
            ..Default::default()
        });
        let info = mock_info("mars_token");
        let res = execute(deps.as_mut(), env, info, msg).unwrap();
        let expected_end_height = 100_000 + TEST_PROPOSAL_VOTING_PERIOD;
        assert_eq!(
            res.attributes,
            vec![
                attr("action", "submit_proposal"),
                attr("proposal_submitter", "submitter"),
                attr("proposal_id", 2),
                attr("proposal_end_height", expected_end_height),
            ]
        );

        let global_state = GLOBAL_STATE.load(&deps.storage).unwrap();
        assert_eq!(global_state.proposal_count, 2);

        let proposal = PROPOSALS.load(&deps.storage, U64Key::new(2_u64)).unwrap();
        assert_eq!(
            proposal.link,
            Some("https://www.avalidlink.com".to_string())
        );
        assert_eq!(
            proposal.execute_calls,
            Some(vec![ProposalExecuteCall {
                execution_order: 0,
                target_contract_address: Addr::unchecked(MOCK_CONTRACT_ADDR),
                msg: to_binary(&ExecuteMsg::UpdateConfig {
                    config: CreateOrUpdateConfig::default()
                })
                .unwrap(),
            }])
        );
    }

    #[test]
    fn test_invalid_cast_votes() {
        let mut deps = th_setup(&[]);
        let voter_address = Addr::unchecked("valid_voter");
        let invalid_voter_address = Addr::unchecked("invalid_voter");

        deps.querier
            .set_xmars_address(Addr::unchecked("xmars_token"));
        deps.querier
            .set_xmars_balance_at(voter_address, 99_999, Uint128::new(100));
        deps.querier
            .set_xmars_balance_at(invalid_voter_address, 99_999, Uint128::zero());

        let active_proposal_id = 1_u64;
        th_build_mock_proposal(
            deps.as_mut(),
            MockProposal {
                id: active_proposal_id,
                status: ProposalStatus::Active,
                start_height: 100_000,
                end_height: 100_100,
                ..Default::default()
            },
        );

        let executed_proposal_id = 2_u64;
        th_build_mock_proposal(
            deps.as_mut(),
            MockProposal {
                id: executed_proposal_id,
                status: ProposalStatus::Executed,
                start_height: 100_000,
                end_height: 100_100,
                ..Default::default()
            },
        );

        // *
        // voting on a non-existent proposal should fail
        // *
        {
            let msg = ExecuteMsg::CastVote {
                proposal_id: 3,
                vote: ProposalVoteOption::For,
            };
            let env = mock_env(MockEnvParams {
                block_height: 100_001,
                ..Default::default()
            });
            let info = mock_info("valid_voter");
            let res_error = execute(deps.as_mut(), env, info, msg).unwrap_err();
            assert_eq!(
                res_error,
                StdError::NotFound {
                    kind: "council::state::Proposal".to_string(),
                }
                .into()
            );
        }

        // *
        // voting on an inactive proposal should fail
        // *
        {
            let msg = ExecuteMsg::CastVote {
                proposal_id: executed_proposal_id,
                vote: ProposalVoteOption::For,
            };
            let env = mock_env(MockEnvParams {
                block_height: 100_001,
                ..Default::default()
            });
            let info = mock_info("valid_voter");
            let response = execute(deps.as_mut(), env, info, msg).unwrap_err();
            assert_eq!(response, ContractError::ProposalNotActive {});
        }

        // *
        // voting after proposal end should fail
        // *
        {
            let msg = ExecuteMsg::CastVote {
                proposal_id: active_proposal_id,
                vote: ProposalVoteOption::For,
            };
            let env = mock_env(MockEnvParams {
                block_height: 100_200,
                ..Default::default()
            });
            let info = mock_info("valid_voter");
            let response = execute(deps.as_mut(), env, info, msg).unwrap_err();
            assert_eq!(response, ContractError::VoteVotingPeriodEnded {});
        }

        // *
        // voting without any voting power should fail
        // *
        {
            let msg = ExecuteMsg::CastVote {
                proposal_id: active_proposal_id,
                vote: ProposalVoteOption::For,
            };
            let env = mock_env(MockEnvParams {
                block_height: 100_001,
                ..Default::default()
            });
            let info = mock_info("invalid_voter");
            let response = execute(deps.as_mut(), env, info, msg).unwrap_err();
            assert_eq!(response, ContractError::VoteNoVotingPower { block: 99_999 });
        }
    }

    #[test]
    fn test_cast_vote() {
        // setup
        let mut deps = th_setup(&[]);
        let voter_address = Addr::unchecked("voter");

        let active_proposal_id = 1_u64;

        deps.querier
            .set_xmars_address(Addr::unchecked("xmars_token"));
        deps.querier
            .set_xmars_balance_at(voter_address.clone(), 99_999, Uint128::new(100));

        let active_proposal = th_build_mock_proposal(
            deps.as_mut(),
            MockProposal {
                id: active_proposal_id,
                status: ProposalStatus::Active,
                start_height: 100_000,
                end_height: 100_100,
                ..Default::default()
            },
        );

        // Add another vote on an extra proposal to voter to validate voting on multiple proposals
        // is valid
        PROPOSAL_VOTES
            .save(
                &mut deps.storage,
                (U64Key::new(4_u64), &voter_address),
                &ProposalVote {
                    option: ProposalVoteOption::Against,
                    power: Uint128::new(100),
                },
            )
            .unwrap();

        // Valid vote for
        let msg = ExecuteMsg::CastVote {
            proposal_id: active_proposal_id,
            vote: ProposalVoteOption::For,
        };

        let env = mock_env(MockEnvParams {
            block_height: active_proposal.start_height + 1,
            ..Default::default()
        });
        let info = mock_info("voter");
        let res = execute(deps.as_mut(), env, info, msg).unwrap();

        assert_eq!(
            vec![
                attr("action", "cast_vote"),
                attr("proposal_id", active_proposal_id),
                attr("voter", "voter"),
                attr("vote", "for"),
                attr("voting_power", 100),
            ],
            res.attributes
        );

        let proposal = PROPOSALS
            .load(&deps.storage, U64Key::new(active_proposal_id))
            .unwrap();
        assert_eq!(proposal.for_votes, Uint128::new(100));
        assert_eq!(proposal.against_votes, Uint128::new(0));

        let proposal_vote = PROPOSAL_VOTES
            .load(
                &deps.storage,
                (U64Key::new(active_proposal_id), &voter_address),
            )
            .unwrap();

        assert_eq!(proposal_vote.option, ProposalVoteOption::For);
        assert_eq!(proposal_vote.power, Uint128::new(100));

        // Voting again with same address should fail
        let msg = ExecuteMsg::CastVote {
            proposal_id: active_proposal_id,
            vote: ProposalVoteOption::For,
        };

        let env = mock_env(MockEnvParams {
            block_height: active_proposal.start_height + 1,
            ..Default::default()
        });
        let info = mock_info("voter");
        let response = execute(deps.as_mut(), env, info, msg).unwrap_err();
        assert_eq!(response, ContractError::VoteUserAlreadyVoted {});

        // Valid against vote
        {
            let msg = ExecuteMsg::CastVote {
                proposal_id: active_proposal_id,
                vote: ProposalVoteOption::Against,
            };

            deps.querier.set_xmars_balance_at(
                Addr::unchecked("voter2"),
                active_proposal.start_height - 1,
                Uint128::new(200),
            );

            let env = mock_env(MockEnvParams {
                block_height: active_proposal.start_height + 1,
                ..Default::default()
            });
            let info = mock_info("voter2");
            let res = execute(deps.as_mut(), env, info, msg).unwrap();
            assert_eq!(
                vec![
                    attr("action", "cast_vote"),
                    attr("proposal_id", active_proposal_id),
                    attr("voter", "voter2"),
                    attr("vote", "against"),
                    attr("voting_power", 200),
                ],
                res.attributes
            );
        }

        // Extra for and against votes to check aggregates are computed correctly
        deps.querier.set_xmars_balance_at(
            Addr::unchecked("voter3"),
            active_proposal.start_height - 1,
            Uint128::new(300),
        );

        deps.querier.set_xmars_balance_at(
            Addr::unchecked("voter4"),
            active_proposal.start_height - 1,
            Uint128::new(400),
        );

        {
            let msg = ExecuteMsg::CastVote {
                proposal_id: active_proposal_id,
                vote: ProposalVoteOption::For,
            };
            let env = mock_env(MockEnvParams {
                block_height: active_proposal.start_height + 1,
                ..Default::default()
            });
            let info = mock_info("voter3");
            execute(deps.as_mut(), env, info, msg).unwrap();
        }

        {
            let msg = ExecuteMsg::CastVote {
                proposal_id: active_proposal_id,
                vote: ProposalVoteOption::Against,
            };
            let env = mock_env(MockEnvParams {
                block_height: active_proposal.start_height + 1,
                ..Default::default()
            });
            let info = mock_info("voter4");
            execute(deps.as_mut(), env, info, msg).unwrap();
        }

        let proposal = PROPOSALS
            .load(&deps.storage, U64Key::new(active_proposal_id))
            .unwrap();
        assert_eq!(proposal.for_votes, Uint128::new(100 + 300));
        assert_eq!(proposal.against_votes, Uint128::new(200 + 400));
    }

    #[test]
    fn test_query_proposals() {
        // Arrange
        let mut deps = th_setup(&[]);

        let active_proposal_1_id = 1_u64;
        th_build_mock_proposal(
            deps.as_mut(),
            MockProposal {
                id: active_proposal_1_id,
                status: ProposalStatus::Active,
                start_height: 100_000,
                end_height: 100_100,
                ..Default::default()
            },
        );

        let active_proposal_2_id = 2_u64;
        let execute_calls = Option::from(vec![ProposalExecuteCall {
            execution_order: 0,
            target_contract_address: Addr::unchecked("test_address"),
            msg: Binary::from(br#"{"some":123}"#),
        }]);
        th_build_mock_proposal(
            deps.as_mut(),
            MockProposal {
                id: active_proposal_2_id,
                status: ProposalStatus::Active,
                start_height: 100_000,
                end_height: 100_100,
                execute_calls,
                ..Default::default()
            },
        );

        let global_state = GlobalState {
            proposal_count: 2_u64,
        };
        GLOBAL_STATE.save(&mut deps.storage, &global_state).unwrap();
        // Assert corectly sorts asc
        let res = query_proposals(deps.as_ref(), None, None).unwrap();
        assert_eq!(res.proposal_count, 2);
        assert_eq!(res.proposal_list.len(), 2);
        assert_eq!(res.proposal_list[0].proposal_id, active_proposal_1_id);
        assert_eq!(res.proposal_list[1].proposal_id, active_proposal_2_id);
        assert_eq!(
            res.proposal_list[1].execute_calls.clone().unwrap()[0].target_contract_address,
            String::from("test_address")
        );

        // Assert start != 0
        let res = query_proposals(deps.as_ref(), Some(2), None).unwrap();
        assert_eq!(res.proposal_count, 2);
        assert_eq!(res.proposal_list.len(), 1);
        assert_eq!(res.proposal_list[0].proposal_id, active_proposal_2_id);

        // Assert start > length of collection
        let res = query_proposals(deps.as_ref(), Some(99), None).unwrap();
        assert_eq!(res.proposal_count, 2);
        assert_eq!(res.proposal_list.len(), 0);

        // Assert limit
        let res = query_proposals(deps.as_ref(), None, Some(1)).unwrap();
        assert_eq!(res.proposal_count, 2);
        assert_eq!(res.proposal_list.len(), 1);
        assert_eq!(res.proposal_list[0].proposal_id, active_proposal_1_id);

        // Assert limit greater than length of collection
        let res = query_proposals(deps.as_ref(), None, Some(99)).unwrap();
        assert_eq!(res.proposal_count, 2);
        assert_eq!(res.proposal_list.len(), 2);
    }

    #[test]
    fn test_invalid_end_proposals() {
        let mut deps = th_setup(&[]);

        let active_proposal_id = 1_u64;
        let executed_proposal_id = 2_u64;

        deps.querier
            .set_xmars_address(Addr::unchecked("xmars_token"));
        deps.querier
            .set_xmars_total_supply_at(99_999, Uint128::new(100));

        th_build_mock_proposal(
            deps.as_mut(),
            MockProposal {
                id: active_proposal_id,
                status: ProposalStatus::Active,
                end_height: 100_000,
                ..Default::default()
            },
        );
        th_build_mock_proposal(
            deps.as_mut(),
            MockProposal {
                id: executed_proposal_id,
                status: ProposalStatus::Executed,
                ..Default::default()
            },
        );

        // cannot end a proposal that has not ended its voting period
        let msg = ExecuteMsg::EndProposal {
            proposal_id: active_proposal_id,
        };
        let env = mock_env(MockEnvParams {
            block_height: 100_000,
            ..Default::default()
        });
        let info = mock_info("sender");
        let response = execute(deps.as_mut(), env, info, msg).unwrap_err();
        assert_eq!(response, ContractError::EndProposalVotingPeriodNotEnded {});

        // cannot end a non active proposal
        let msg = ExecuteMsg::EndProposal {
            proposal_id: executed_proposal_id,
        };
        let env = mock_env(MockEnvParams {
            block_height: 100_001,
            ..Default::default()
        });
        let info = mock_info("sender");
        let response = execute(deps.as_mut(), env, info, msg).unwrap_err();
        assert_eq!(response, ContractError::ProposalNotActive {});
    }

    #[test]
    fn test_end_proposal() {
        let mut deps = th_setup(&[]);

        deps.querier
            .set_xmars_address(Addr::unchecked("xmars_token"));
        deps.querier
            .set_xmars_total_supply_at(89_999, Uint128::new(100_000));
        let proposal_threshold = Decimal::from_ratio(51_u128, 100_u128);
        let proposal_quorum = Decimal::from_ratio(2_u128, 100_u128);
        let proposal_end_height = 100_000u64;

        CONFIG
            .update(&mut deps.storage, |mut config| -> StdResult<Config> {
                config.proposal_required_threshold = proposal_threshold;
                config.proposal_required_quorum = proposal_quorum;
                Ok(config)
            })
            .unwrap();

        // end passed proposal
        let initial_passed_proposal = th_build_mock_proposal(
            deps.as_mut(),
            MockProposal {
                id: 1,
                status: ProposalStatus::Active,
                for_votes: Uint128::new(11_000),
                against_votes: Uint128::new(10_000),
                start_height: 90_000,
                end_height: proposal_end_height + 1,
                ..Default::default()
            },
        );

        let msg = ExecuteMsg::EndProposal { proposal_id: 1 };

        let env = mock_env(MockEnvParams {
            block_height: initial_passed_proposal.end_height + 1,
            ..Default::default()
        });
        let info = mock_info("sender");

        let res = execute(deps.as_mut(), env, info, msg).unwrap();

        assert_eq!(
            res.attributes,
            vec![
                attr("action", "end_proposal"),
                attr("proposal_id", 1),
                attr("proposal_result", "passed"),
            ]
        );

        assert_eq!(
            res.messages,
            vec![SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: String::from("mars_token"),
                funds: vec![],
                msg: to_binary(&Cw20ExecuteMsg::Transfer {
                    recipient: String::from("submitter"),
                    amount: TEST_PROPOSAL_REQUIRED_DEPOSIT,
                })
                .unwrap(),
            })),]
        );

        let final_passed_proposal = PROPOSALS.load(&deps.storage, U64Key::new(1u64)).unwrap();
        assert_eq!(final_passed_proposal.status, ProposalStatus::Passed);

        // end rejected proposal (no quorum)
        let initial_passed_proposal = th_build_mock_proposal(
            deps.as_mut(),
            MockProposal {
                id: 2,
                status: ProposalStatus::Active,
                for_votes: Uint128::new(11),
                against_votes: Uint128::new(10),
                end_height: proposal_end_height + 1,
                start_height: 90_000,
                ..Default::default()
            },
        );

        let msg = ExecuteMsg::EndProposal { proposal_id: 2 };

        let env = mock_env(MockEnvParams {
            block_height: initial_passed_proposal.end_height + 1,
            ..Default::default()
        });
        let info = mock_info("sender");

        let res = execute(deps.as_mut(), env, info, msg).unwrap();

        assert_eq!(
            res.attributes,
            vec![
                attr("action", "end_proposal"),
                attr("proposal_id", 2),
                attr("proposal_result", "rejected"),
            ]
        );

        assert_eq!(
            res.messages,
            vec![SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: String::from("mars_token"),
                msg: to_binary(&Cw20ExecuteMsg::Transfer {
                    recipient: String::from("staking"),
                    amount: TEST_PROPOSAL_REQUIRED_DEPOSIT,
                })
                .unwrap(),
                funds: vec![],
            }))]
        );

        let final_passed_proposal = PROPOSALS.load(&deps.storage, U64Key::new(2_u64)).unwrap();
        assert_eq!(final_passed_proposal.status, ProposalStatus::Rejected);

        // end rejected proposal (no threshold)
        let initial_passed_proposal = th_build_mock_proposal(
            deps.as_mut(),
            MockProposal {
                id: 3,
                status: ProposalStatus::Active,
                for_votes: Uint128::new(10_000),
                against_votes: Uint128::new(11_000),
                start_height: 90_000,
                end_height: proposal_end_height + 1,
                ..Default::default()
            },
        );

        let msg = ExecuteMsg::EndProposal { proposal_id: 3 };

        let env = mock_env(MockEnvParams {
            block_height: initial_passed_proposal.end_height + 1,
            ..Default::default()
        });
        let info = mock_info("sender");

        let res = execute(deps.as_mut(), env, info, msg).unwrap();

        assert_eq!(
            res.attributes,
            vec![
                attr("action", "end_proposal"),
                attr("proposal_id", 3),
                attr("proposal_result", "rejected"),
            ]
        );

        assert_eq!(
            res.messages,
            vec![SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: String::from("mars_token"),
                msg: to_binary(&Cw20ExecuteMsg::Transfer {
                    recipient: String::from("staking"),
                    amount: TEST_PROPOSAL_REQUIRED_DEPOSIT,
                })
                .unwrap(),
                funds: vec![],
            }))]
        );

        let final_passed_proposal = PROPOSALS.load(&deps.storage, U64Key::new(3_u64)).unwrap();
        assert_eq!(final_passed_proposal.status, ProposalStatus::Rejected);
    }

    #[test]
    fn test_invalid_execute_proposals() {
        let mut deps = th_setup(&[]);

        let passed_proposal_id = 1_u64;
        let executed_proposal_id = 2_u64;

        let passed_proposal = th_build_mock_proposal(
            deps.as_mut(),
            MockProposal {
                id: passed_proposal_id,
                status: ProposalStatus::Passed,
                end_height: 100_000,
                ..Default::default()
            },
        );
        let executed_proposal = th_build_mock_proposal(
            deps.as_mut(),
            MockProposal {
                id: executed_proposal_id,
                status: ProposalStatus::Executed,
                ..Default::default()
            },
        );

        // cannot execute a non Passed proposal
        let msg = ExecuteMsg::ExecuteProposal {
            proposal_id: executed_proposal_id,
        };
        let env = mock_env(MockEnvParams {
            block_height: executed_proposal.end_height + TEST_PROPOSAL_EFFECTIVE_DELAY + 1,
            ..Default::default()
        });
        let info = mock_info("executer");
        let response = execute(deps.as_mut(), env, info, msg).unwrap_err();
        assert_eq!(response, ContractError::ExecuteProposalNotPassed {},);

        // cannot execute a proposal before the effective delay has passed
        let msg = ExecuteMsg::ExecuteProposal {
            proposal_id: passed_proposal_id,
        };
        let env = mock_env(MockEnvParams {
            block_height: passed_proposal.end_height + 1,
            ..Default::default()
        });
        let info = mock_info("executer");
        let response = execute(deps.as_mut(), env, info, msg).unwrap_err();
        assert_eq!(response, ContractError::ExecuteProposalDelayNotEnded {});

        // cannot execute an expired proposal
        let msg = ExecuteMsg::ExecuteProposal {
            proposal_id: passed_proposal_id,
        };
        let env = mock_env(MockEnvParams {
            block_height: passed_proposal.end_height
                + TEST_PROPOSAL_EFFECTIVE_DELAY
                + TEST_PROPOSAL_EXPIRATION_PERIOD
                + 1,
            ..Default::default()
        });
        let info = mock_info("executer");
        let response = execute(deps.as_mut(), env, info, msg).unwrap_err();
        assert_eq!(response, ContractError::ExecuteProposalExpired {});
    }

    #[test]
    fn test_execute_proposals() {
        let mut deps = th_setup(&[]);
        let contract_address = Addr::unchecked(MOCK_CONTRACT_ADDR);
        let other_address = Addr::unchecked("other");

        let binary_msg = Binary::from(br#"{"key": 123}"#);
        let initial_proposal = th_build_mock_proposal(
            deps.as_mut(),
            MockProposal {
                id: 1,
                status: ProposalStatus::Passed,
                end_height: 100_000,
                execute_calls: Some(vec![
                    ProposalExecuteCall {
                        execution_order: 2,
                        msg: binary_msg.clone(),
                        target_contract_address: other_address.clone(),
                    },
                    ProposalExecuteCall {
                        execution_order: 3,
                        msg: to_binary(&ExecuteMsg::UpdateConfig {
                            config: CreateOrUpdateConfig::default(),
                        })
                        .unwrap(),
                        target_contract_address: contract_address.clone(),
                    },
                    ProposalExecuteCall {
                        execution_order: 1,
                        msg: to_binary(&ExecuteMsg::UpdateConfig {
                            config: CreateOrUpdateConfig::default(),
                        })
                        .unwrap(),
                        target_contract_address: contract_address.clone(),
                    },
                ]),
                ..Default::default()
            },
        );

        let env = mock_env(MockEnvParams {
            block_height: initial_proposal.end_height + TEST_PROPOSAL_EFFECTIVE_DELAY + 1,
            ..Default::default()
        });
        let info = mock_info("executer");

        let msg = ExecuteMsg::ExecuteProposal { proposal_id: 1 };

        let res = execute(deps.as_mut(), env, info, msg).unwrap();

        assert_eq!(
            res.attributes,
            vec![attr("action", "execute_proposal"), attr("proposal_id", 1),]
        );

        assert_eq!(
            res.messages,
            vec![
                SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
                    contract_addr: contract_address.to_string(),
                    funds: vec![],
                    msg: to_binary(&ExecuteMsg::UpdateConfig {
                        config: CreateOrUpdateConfig::default()
                    })
                    .unwrap(),
                })),
                SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
                    contract_addr: other_address.to_string(),
                    funds: vec![],
                    msg: binary_msg,
                })),
                SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
                    contract_addr: contract_address.to_string(),
                    funds: vec![],
                    msg: to_binary(&ExecuteMsg::UpdateConfig {
                        config: CreateOrUpdateConfig::default()
                    })
                    .unwrap(),
                })),
            ]
        );

        let final_passed_proposal = PROPOSALS
            .load(&mut deps.storage, U64Key::new(1_u64))
            .unwrap();

        assert_eq!(ProposalStatus::Executed, final_passed_proposal.status);
    }

    #[test]
    fn test_query_proposal_votes() {
        // Arrange
        let mut deps = th_setup(&[]);

        deps.querier
            .set_xmars_address(Addr::unchecked("xmars_token"));
        let active_proposal_id = 1_u64;

        let voter_address1 = Addr::unchecked("voter1");
        let voter_address2 = Addr::unchecked("voter2");
        let voter_address3 = Addr::unchecked("voter3");
        let voter_address4 = Addr::unchecked("voter4");
        let voter_address5 = Addr::unchecked("voter5");
        deps.querier
            .set_xmars_balance_at(voter_address1, 99_999, Uint128::new(100));
        deps.querier
            .set_xmars_balance_at(voter_address2, 99_999, Uint128::new(200));
        deps.querier
            .set_xmars_balance_at(voter_address3, 99_999, Uint128::new(300));
        deps.querier
            .set_xmars_balance_at(voter_address4, 99_999, Uint128::new(400));
        deps.querier
            .set_xmars_balance_at(voter_address5, 99_999, Uint128::new(500));

        let active_proposal = th_build_mock_proposal(
            deps.as_mut(),
            MockProposal {
                id: active_proposal_id,
                status: ProposalStatus::Active,
                start_height: 100_000,
                end_height: 100_100,
                ..Default::default()
            },
        );
        PROPOSALS
            .save(
                &mut deps.storage,
                U64Key::new(active_proposal_id),
                &active_proposal,
            )
            .unwrap();

        let msg_vote_for = ExecuteMsg::CastVote {
            proposal_id: active_proposal_id,
            vote: ProposalVoteOption::For,
        };
        let msg_vote_against = ExecuteMsg::CastVote {
            proposal_id: active_proposal_id,
            vote: ProposalVoteOption::Against,
        };

        // Act
        let env = mock_env(MockEnvParams {
            block_height: active_proposal.start_height + 1,
            ..Default::default()
        });
        let info = mock_info("voter1");
        execute(deps.as_mut(), env, info, msg_vote_for.clone()).unwrap();

        let env = mock_env(MockEnvParams {
            block_height: active_proposal.start_height + 1,
            ..Default::default()
        });
        let info = mock_info("voter2");
        execute(deps.as_mut(), env, info, msg_vote_for.clone()).unwrap();

        let env = mock_env(MockEnvParams {
            block_height: active_proposal.start_height + 1,
            ..Default::default()
        });
        let info = mock_info("voter3");
        execute(deps.as_mut(), env, info, msg_vote_for.clone()).unwrap();

        let env = mock_env(MockEnvParams {
            block_height: active_proposal.start_height + 1,
            ..Default::default()
        });
        let info = mock_info("voter4");
        execute(deps.as_mut(), env, info, msg_vote_against.clone()).unwrap();

        let env = mock_env(MockEnvParams {
            block_height: active_proposal.start_height + 1,
            ..Default::default()
        });
        let info = mock_info("voter5");
        execute(deps.as_mut(), env, info, msg_vote_against.clone()).unwrap();

        // Assert default params
        let res = query_proposal_votes(
            deps.as_ref(),
            active_proposal_id,
            Option::None,
            Option::None,
        )
        .unwrap();
        assert_eq!(res.votes.len(), 5);
        assert_eq!(res.proposal_id, active_proposal_id);

        // Assert corectly sorts asc
        assert_eq!(res.votes[0].voter_address, Addr::unchecked("voter1"));
        assert_eq!(res.votes[0].option, ProposalVoteOption::For);
        assert_eq!(res.votes[0].power, Uint128::new(100));
        assert_eq!(res.votes[4].voter_address, Addr::unchecked("voter5"));
        assert_eq!(res.votes[4].option, ProposalVoteOption::Against);
        assert_eq!(res.votes[4].power, Uint128::new(500));

        // Assert start_after
        let res = query_proposal_votes(
            deps.as_ref(),
            active_proposal_id,
            Option::from(String::from("voter4")),
            Option::None,
        )
        .unwrap();
        assert_eq!(res.votes.len(), 1);
        assert_eq!(res.votes[0].voter_address, Addr::unchecked("voter5"));

        // Assert take
        let res = query_proposal_votes(
            deps.as_ref(),
            active_proposal_id,
            Option::None,
            Option::from(1),
        )
        .unwrap();
        assert_eq!(res.votes.len(), 1);
        assert_eq!(res.votes[0].voter_address, Addr::unchecked("voter1"));
    }

    // TEST HELPERS
    fn th_setup(contract_balances: &[Coin]) -> OwnedDeps<MockStorage, MockApi, MarsMockQuerier> {
        let mut deps = mock_dependencies(contract_balances);

        // TODO: Do we actually need the init to happen on tests?
        let config = CreateOrUpdateConfig {
            address_provider_address: Some(String::from("address_provider")),

            proposal_voting_period: Some(TEST_PROPOSAL_VOTING_PERIOD),
            proposal_effective_delay: Some(TEST_PROPOSAL_EFFECTIVE_DELAY),
            proposal_expiration_period: Some(TEST_PROPOSAL_EXPIRATION_PERIOD),
            proposal_required_deposit: Some(TEST_PROPOSAL_REQUIRED_DEPOSIT),
            proposal_required_quorum: Some(Decimal::one()),
            proposal_required_threshold: Some(Decimal::one()),
        };

        let msg = InstantiateMsg { config };
        let info = mock_info("initializer");
        let env = mock_env(MockEnvParams::default());
        instantiate(deps.as_mut(), env, info, msg).unwrap();

        deps
    }

    #[derive(Debug)]
    struct MockProposal {
        id: u64,
        status: ProposalStatus,
        for_votes: Uint128,
        against_votes: Uint128,
        start_height: u64,
        end_height: u64,
        execute_calls: Option<Vec<ProposalExecuteCall>>,
    }

    impl Default for MockProposal {
        fn default() -> Self {
            MockProposal {
                id: 1,
                status: ProposalStatus::Active,
                for_votes: Uint128::zero(),
                against_votes: Uint128::zero(),
                start_height: 1,
                end_height: 1,
                execute_calls: None,
            }
        }
    }

    fn th_build_mock_proposal(deps: DepsMut, mock_proposal: MockProposal) -> Proposal {
        let proposal = Proposal {
            submitter_address: Addr::unchecked("submitter"),
            status: mock_proposal.status,
            for_votes: mock_proposal.for_votes,
            against_votes: mock_proposal.against_votes,
            start_height: mock_proposal.start_height,
            end_height: mock_proposal.end_height,
            title: "A valid title".to_string(),
            description: "A description".to_string(),
            link: None,
            execute_calls: mock_proposal.execute_calls,
            deposit_amount: TEST_PROPOSAL_REQUIRED_DEPOSIT,
        };

        PROPOSALS
            .save(deps.storage, U64Key::new(mock_proposal.id), &proposal)
            .unwrap();

        proposal
    }
}
