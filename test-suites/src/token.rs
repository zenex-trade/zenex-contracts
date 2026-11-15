use soroban_sdk::testutils::IssuerFlags;
use soroban_sdk::token::StellarAssetClient;
use soroban_sdk::{Address, Env};

pub fn create_stellar_token<'a>(e: &Env, admin: &Address) -> (Address, StellarAssetClient<'a>) {
    let sac = e.register_stellar_asset_contract_v2(admin.clone());
    // AUTH_REVOCABLE, so suites can deauthorize a trustline with
    // `set_authorized` and exercise the failed-payout path.
    sac.issuer().set_flag(IssuerFlags::RevocableFlag);
    let address = sac.address();
    let client = StellarAssetClient::new(e, &address);
    (address, client)
}
