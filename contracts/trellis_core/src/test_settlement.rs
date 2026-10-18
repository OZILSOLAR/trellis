//! #590 (balance-delta check on lock) and #591 (O(1) settlement paths).

use soroban_sdk::{contract, contractimpl, token, Address, Env, Map};

use crate::{
    errors::TrellisError,
    test_utils::{agreement_id, milestones_from_amounts, setup_mocked},
    types::EscrowStatus,
};

/// Token that keeps 1% of every transfer, so the receiver is credited less
/// than the amount requested.
#[contract]
struct FeeToken;

#[contractimpl]
impl FeeToken {
    pub fn mint(env: Env, to: Address, amount: i128) {
        let mut b: Map<Address, i128> = env
            .storage()
            .instance()
            .get(&0u32)
            .unwrap_or(Map::new(&env));
        b.set(to.clone(), b.get(to).unwrap_or(0) + amount);
        env.storage().instance().set(&0u32, &b);
    }

    pub fn symbol(_env: Env) -> soroban_sdk::String {
        soroban_sdk::String::from_str(&_env, "FEE")
    }

    pub fn balance(env: Env, id: Address) -> i128 {
        let b: Map<Address, i128> = env
            .storage()
            .instance()
            .get(&0u32)
            .unwrap_or(Map::new(&env));
        b.get(id).unwrap_or(0)
    }

    pub fn transfer(env: Env, from: Address, to: Address, amount: i128) {
        let mut b: Map<Address, i128> = env
            .storage()
            .instance()
            .get(&0u32)
            .unwrap_or(Map::new(&env));
        b.set(from.clone(), b.get(from).unwrap_or(0) - amount);
        b.set(to.clone(), b.get(to).unwrap_or(0) + amount - amount / 100);
        env.storage().instance().set(&0u32, &b);
    }
}

#[test]
fn fee_on_transfer_token_cannot_lock_funds() {
    let (env, payer, payee, resolver, _token, client) = setup_mocked();
    let fee_token = env.register(FeeToken, ());
    FeeTokenClient::new(&env, &fee_token).mint(&payer, &10_000);

    let id = agreement_id(&env, 1);
    client.init(
        &id,
        &payer,
        &payee,
        &fee_token,
        &milestones_from_amounts(&env, &[1_000, 1_000]),
        &resolver,
    );

    assert_eq!(
        client.try_lock_funds(&id, &0),
        Err(Ok(TrellisError::TransferShortfall))
    );
    assert_eq!(
        client.try_batch_lock_funds(&id, &soroban_sdk::vec![&env, 0u32, 1u32]),
        Err(Ok(TrellisError::TransferShortfall))
    );
    assert_eq!(
        client.get_milestone(&id, &0).unwrap().status,
        EscrowStatus::Pending
    );
}

#[test]
fn standard_token_still_locks() {
    let (env, payer, payee, resolver, token_address, client) = setup_mocked();
    let id = agreement_id(&env, 2);
    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &milestones_from_amounts(&env, &[1_000, 1_000]),
        &resolver,
    );
    client.lock_funds(&id, &0);
    client.batch_lock_funds(&id, &soroban_sdk::vec![&env, 1u32]);
    assert_eq!(
        client.get_milestone(&id, &1).unwrap().status,
        EscrowStatus::Funded
    );
    assert_eq!(
        token::Client::new(&env, &token_address).balance(&client.address),
        2_000
    );
}

/// CPU cost of one settlement call on milestone 0 of an `n`-milestone
/// agreement: `approve_and_release` when `approve`, else `lock_funds` (already
/// O(1) per #401, used as the reference).
fn cost(n: usize, approve: bool) -> u64 {
    let (env, payer, payee, resolver, token_address, client) = setup_mocked();
    let id = agreement_id(&env, 3);
    client.init(
        &id,
        &payer,
        &payee,
        &token_address,
        &milestones_from_amounts(&env, &[10i128; 50][..n]),
        &resolver,
    );
    if approve {
        client.lock_funds(&id, &0);
        client.submit_work(&id, &0, &None);
    }
    env.cost_estimate().budget().reset_default();
    if approve {
        client.approve_and_release(&id, &0);
    } else {
        client.lock_funds(&id, &0);
    }
    env.cost_estimate().budget().cpu_instruction_cost()
}

/// The test host charges every call for the size of the ledger snapshot, so
/// absolute cost grows slightly with milestone count for any entrypoint. What
/// must not happen is `approve_and_release` growing faster than an entrypoint
/// that is known to touch a single milestone.
#[test]
fn approve_and_release_cost_does_not_scale_with_milestone_count() {
    let approve_growth = cost(50, true) - cost(3, true);
    let reference_growth = cost(50, false) - cost(3, false);
    assert!(
        approve_growth <= reference_growth + reference_growth / 10,
        "approve grew {approve_growth}, reference grew {reference_growth}"
    );
}
