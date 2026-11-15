//! Convenience constructors that return a fixture pre-seeded with vault liquidity.

use crate::constants::SCALAR_7;
use crate::test_fixture::TestFixture;

/// Default vault liquidity seeded into `create_fixture_with_data` ($10M, token-dec).
pub const DEFAULT_VAULT_LIQUIDITY: i128 = 10_000_000 * SCALAR_7;

/// A deployed fixture with the vault pre-funded so opens clear the utilization cap.
///
/// Liquidity is minted straight to the vault (a donation), which is enough for
/// tests that only need backing. Tests exercising share accounting should route
/// through [`TestFixture::vault_deposit`] instead.
pub fn create_fixture_with_data<'a>() -> TestFixture<'a> {
    let fixture = TestFixture::create();
    fixture.seed_vault(DEFAULT_VAULT_LIQUIDITY);
    fixture
}

/// A deployed-config fixture with the vault pre-funded, the deployed-stack
/// counterpart of [`create_fixture_with_data`].
///
/// The stack retains the broader [`crate::deployed_config`] stress config
/// (six deliberate changes from its deployment record) and the
/// [`StackDials::deployed`](crate::test_fixture::StackDials::deployed)
/// oracle, treasury, and vault dials.
///
/// Liquidity is the same [`DEFAULT_VAULT_LIQUIDITY`] donation the default
/// fixture uses, which keeps tests from being liquidity-constrained. Two
/// consequences follow, and a test that cares must handle them:
///
/// - No LP shares exist against the donated balance, so the first depositor
///   prices against the vault's virtual shares alone. At this seeding one
///   share is worth $10, so a deposit rounds down by up to $10. Deposit for
///   the shares a share-accounting test needs.
/// - The donated balance is 100x the record's `max_vault_balance`. The
///   sizing limits in [`crate::deployed_config`] are 1000x the record, so
///   the donation sits at a tenth of that raised cap and a deposit fill
///   still clears it.
pub fn create_deployed_fixture_with_data<'a>() -> TestFixture<'a> {
    let fixture = TestFixture::create_deployed();
    fixture.seed_vault(DEFAULT_VAULT_LIQUIDITY);
    fixture
}

#[cfg(test)]
mod test {
    use super::*;

    /// Factory deployment wires the market, vault, oracle, and treasury
    /// together, and end-to-end price verification parses a built payload.
    #[test]
    fn fixture_wires_contracts_together() {
        let fixture = create_fixture_with_data();

        assert_eq!(fixture.market.get_vault(), fixture.vault.address);
        assert_eq!(fixture.vault.query_asset(), fixture.token_address);
        assert_eq!(fixture.market.get_oracle(), fixture.oracle.address);
        assert!(fixture.factory.is_deployed(&fixture.market.address));
        assert_eq!(fixture.vault.total_assets(), DEFAULT_VAULT_LIQUIDITY);

        let price = fixture.price(crate::constants::BTC_PRICE);
        let data = fixture
            .oracle
            .verify_price(&price, &fixture.feed_id, &false);
        assert_eq!(data.bid, crate::constants::BTC_PRICE);
        assert_eq!(data.ask, crate::constants::BTC_PRICE);
    }
}
