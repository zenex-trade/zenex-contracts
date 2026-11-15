use soroban_sdk::testutils::{Address as _, Events};
use soroban_sdk::{map, vec, Address, Env, IntoVal, Map, Symbol, Val};

use crate::{TreasuryClient, TreasuryContract};

fn setup() -> (Env, TreasuryClient<'static>, Address) {
    let e = Env::default();
    e.mock_all_auths();
    let owner = Address::generate(&e);
    let contract_id = e.register(TreasuryContract, (&owner, &100_000_000_000_000_000i128));
    let client = TreasuryClient::new(&e, &contract_id);
    (e, client, owner)
}

#[test]
fn test_get_rate() {
    let (_e, client, _owner) = setup();
    assert_eq!(client.get_rate(), 100_000_000_000_000_000);
}

#[test]
fn test_set_rate() {
    let (_e, client, _owner) = setup();
    client.set_rate(&200_000_000_000_000_000);
    assert_eq!(client.get_rate(), 200_000_000_000_000_000);
}

/// The market reads the rate at every settlement, so the change is attested
/// where a watcher can see it.
#[test]
fn set_rate_attests_the_new_rate() {
    let (e, client, _owner) = setup();
    client.set_rate(&200_000_000_000_000_000);

    let data: Map<Symbol, Val> = map![
        &e,
        (
            Symbol::new(&e, "rate"),
            200_000_000_000_000_000_i128.into_val(&e)
        ),
    ];
    assert_eq!(
        e.events().all().filter_by_contract(&client.address),
        vec![
            &e,
            (
                client.address.clone(),
                (Symbol::new(&e, "rate_update"),).into_val(&e),
                data.into_val(&e),
            ),
        ],
    );
}

/// A rejected rate leaves no trace.
#[test]
fn a_rejected_rate_attests_nothing() {
    let (e, client, _owner) = setup();
    assert!(client.try_set_rate(&(500_000_000_000_000_001)).is_err());
    assert_eq!(
        e.events().all().filter_by_contract(&client.address),
        vec![&e]
    );
}

#[test]
fn test_withdraw() {
    let (e, client, owner) = setup();
    let token_admin = Address::generate(&e);
    let token_id = e.register_stellar_asset_contract_v2(token_admin.clone());
    let token_client = soroban_sdk::token::StellarAssetClient::new(&e, &token_id.address());

    // Mint tokens to the treasury contract
    token_client.mint(&client.address, &1_000);

    // Withdraw to owner
    client.withdraw(&token_id.address(), &owner, &500);
    // Read before the balance calls, which reset the event buffer.
    let events = e.events().all().filter_by_contract(&client.address);

    let balance_client = soroban_sdk::token::TokenClient::new(&e, &token_id.address());
    assert_eq!(balance_client.balance(&owner), 500);
    assert_eq!(balance_client.balance(&client.address), 500);

    // The withdrawal is attested with the token and recipient as topics.
    let data: Map<Symbol, Val> = map![&e, (Symbol::new(&e, "amount"), 500_i128.into_val(&e))];
    assert_eq!(
        events,
        vec![
            &e,
            (
                client.address.clone(),
                (
                    Symbol::new(&e, "withdraw"),
                    token_id.address().clone(),
                    owner.clone()
                )
                    .into_val(&e),
                data.into_val(&e),
            ),
        ],
    );
}
