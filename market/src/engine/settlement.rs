//! The settlement layer: a position-change outcome becomes its four settled
//! legs, and `Settlement::settle` moves them.

use crate::dependencies::VaultClient;
use crate::engine::market::Market;
use crate::engine::market_data::MarketData;
use crate::engine::math;
use crate::engine::position::{Decrease, Fees, Increase, Liquidation};
use crate::errors::MarketError;
use crate::storage;
use soroban_sdk::{panic_with_error, token, Address, Env};

/// The four settled legs of a position change (token-dec).
pub(crate) struct Settlement {
    /// Paid to the trader.
    pub trader: i128,
    /// Positive is paid to the vault. Negative is drawn from the vault.
    pub vault: i128,
    /// Paid to the keeper: the `keeper_rate` cut plus `exec_fee`.
    pub keeper: i128,
    /// Paid to `market.treasury`.
    pub treasury: i128,
}

impl Settlement {
    /// Splits an increase fill's legs (token-dec). The margin was
    /// pre-escrowed and stays posted, so an increase settles only its fee
    /// legs plus the keeper's `exec_fee`.
    pub(crate) fn compute_increase_order(
        e: &Env,
        market: &Market,
        increase: &Increase,
        exec_fee: i128,
    ) -> Self {
        let mut legs = Self::fee_split(e, market, &increase.fees, 0);
        legs.keeper += exec_fee;
        legs
    }

    /// Splits a decrease fill's legs (token-dec). The trader takes the payout
    /// plus the closure `refund`, and the vault funds the PnL and absorbs the
    /// bad debt. `exec_fee` is `0` for ADL.
    pub(crate) fn compute_decrease_order(
        e: &Env,
        market: &Market,
        decrease: &Decrease,
        refund: i128,
        exec_fee: i128,
    ) -> Self {
        let mut legs = Self::fee_split(e, market, &decrease.fees, 0);
        legs.trader = decrease.returned + refund;
        legs.vault -= decrease.pnl + decrease.bad_debt;
        legs.keeper += exec_fee;
        legs
    }

    /// Splits a liquidation's legs (token-dec): a decrease plus the
    /// liquidation fee, which splits like the trade fee. The post-fee
    /// remainder and the closure `refund` ride the trader leg. There is no
    /// order, so no `exec_fee`.
    pub(crate) fn compute_liquidation(
        e: &Env,
        market: &Market,
        liquidation: &Liquidation,
        refund: i128,
    ) -> Self {
        let mut legs = Self::fee_split(e, market, &liquidation.fees, liquidation.liq_fee);
        legs.trader = liquidation.returned + refund;
        legs.vault -= liquidation.pnl + liquidation.bad_debt;
        legs
    }

    /// Distributes a fill's fee inflows across the keeper, treasury, and
    /// vault legs (token-dec), with a zero trader leg. The trade fee
    /// `base + impact` and `liq_fee` split alike: the keeper takes
    /// `keeper_rate`, the treasury its rate, and the vault the rest. The
    /// borrowing fee skips the keeper.
    fn fee_split(e: &Env, market: &Market, fees: &Fees, liq_fee: i128) -> Self {
        let split_fee = fees.base + fees.impact + liq_fee;
        // A flat keeper rate: a capped fee that shrinks with equity never
        // rewards waiting to liquidate.
        let keeper = math::apply_factor_floor(e, split_fee, market.config.keeper_rate);
        // The treasury cuts are on a gross basis, so the deployment's
        // `keeper_rate` cannot dilute them.
        let treasury = math::apply_factor_floor(e, split_fee, market.treasury_rate)
            + math::apply_factor_floor(e, fees.borrowing, market.treasury_rate);
        let vault = split_fee + fees.borrowing - keeper - treasury;
        Settlement {
            trader: 0,
            vault,
            keeper,
            treasury,
        }
    }

    /// Splits a vault deposit or redeem fee (token-dec). `vault_fee` divides
    /// into the keeper, treasury, and vault cuts, and the flat `exec_fee`
    /// adds to the keeper leg. `trader` is the redeemer's net payout, `0` for
    /// a deposit since the depositor is paid in shares.
    pub(crate) fn compute_vault_order(
        e: &Env,
        market: &Market,
        vault_fee: i128,
        trader: i128,
        exec_fee: i128,
    ) -> Self {
        let keeper_cut = math::apply_factor_floor(e, vault_fee, market.config.keeper_rate);
        let treasury = math::apply_factor_floor(e, vault_fee, market.treasury_rate);
        Settlement {
            trader,
            vault: vault_fee - keeper_cut - treasury,
            keeper: keeper_cut + exec_fee,
            treasury,
        }
    }

    /// Transfers the four legs to their recipients, advances the market's
    /// tracked vault balance by the vault leg (token-dec), and returns the
    /// keeper leg.
    ///
    /// A failed trader payout credits `market.data` through [`pay_trader`],
    /// so the caller must store the market after the settlement.
    ///
    /// # Errors
    /// - [`MarketError::VaultInsolvent`] if the vault draw exceeds the
    ///   vault's balance.
    pub(crate) fn settle(
        &self,
        e: &Env,
        market: &mut Market,
        keeper: &Address,
        user: &Address,
    ) -> i128 {
        let token = token::Client::new(e, &market.token);
        let this = e.current_contract_address();

        // The vault draw precedes every payout.
        if self.vault < 0 {
            // A draw past the vault's balance is insolvency. It traps with
            // a diagnosable error instead of the vault's own token panic
            // deep in the transfer.
            if self.vault.abs() > market.vault_balance {
                panic_with_error!(e, MarketError::VaultInsolvent);
            }
            VaultClient::new(e, &market.vault).strategy_withdraw(&self.vault.abs());
        }

        // Payouts.
        if self.vault > 0 {
            token.transfer(&this, &market.vault, &self.vault);
        }
        if self.keeper > 0 {
            token.transfer(&this, keeper, &self.keeper);
        }
        if self.treasury > 0 {
            token.transfer(&this, &market.treasury, &self.treasury);
        }
        if self.trader > 0 {
            pay_trader(e, &token, &mut market.data, user, self.trader);
        }

        market.vault_balance += self.vault;
        self.keeper
    }
}

/// Pays `amount` (token-dec) to `user`, or parks it as a claimable credit
/// when the direct transfer fails.
///
/// A third-party fill or liquidation therefore never stalls on its receiver,
/// for example on a dropped trustline. The user collects through
/// [`claim_credit`](crate::engine::funding::claim_credit).
pub(crate) fn pay_trader(
    e: &Env,
    token: &token::Client,
    data: &mut MarketData,
    user: &Address,
    amount: i128,
) {
    let this = e.current_contract_address();
    if token.try_transfer(&this, user, &amount).is_err() {
        storage::add_claimable_credit(e, user, amount);
        // The unpaid tokens stay on the contract, so the credit keeps the
        // pool ledger backed. Owed and pool rise together, so the retirement
        // surplus (`credit_pool - credit_owed`) is unchanged.
        data.credit_owed += amount;
        data.credit_pool += amount;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dependencies::PriceData;
    use crate::engine::constants::{BASE_UNIT, PRICE_SCALAR};
    use crate::engine::status::Status;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::{contract, contractimpl};

    #[contract]
    struct Harness;

    // Reports a zero treasury fee rate.
    #[contract]
    struct MockTreasury;

    #[contractimpl]
    impl MockTreasury {
        pub fn get_rate(_e: Env) -> i128 {
            0
        }
    }

    // The outcome of a fee-eroded ADL partial on a 10.0-margin long: the
    // 10.0 chunk realizes 2.0 of profit against 24.031 of fees (0.03 base,
    // 0.001 impact, 24.0 borrowing). The profit covers 2.0, and the 22.031
    // rest overruns the margin by 12.031 of bad debt. The vault leg banks the
    // 24.0279 of fees left after the keeper's 0.0031 cut, funds the 2.0
    // profit, and absorbs the bad debt: 9.9969. With the keeper leg, the legs
    // pay out exactly the 10.0 freed margin.
    #[test]
    fn decrease_legs_absorb_bad_debt_in_the_vault_leg() {
        let e = Env::default();
        let contract_id = e.register(Harness, ());
        let treasury = e.register(MockTreasury, ());
        let market = Market {
            config: crate::engine::config::test_config(),
            data: MarketData::default(),
            status: Status::Active,
            price: PriceData {
                bid: 12 * PRICE_SCALAR,
                ask: 12 * PRICE_SCALAR,
                publish_time: 0,
            },
            token: Address::generate(&e),
            vault: Address::generate(&e),
            vault_balance: 1000_0000000,
            treasury: treasury.clone(),
            treasury_rate: 0,
            now: 0,
        };
        let decrease = Decrease {
            notional: 10_0000000,
            tokens: BASE_UNIT,
            margin: 0,
            pnl: 2_0000000,
            returned: 0,
            bad_debt: 12_0310000,
            fees: Fees {
                base: 300_000,
                impact: 10_000,
                funding: 0,
                borrowing: 24_0000000,
            },
        };

        e.as_contract(&contract_id, || {
            storage::set_treasury(&e, &treasury);
            let legs = Settlement::compute_decrease_order(&e, &market, &decrease, 0, 0);
            assert_eq!(legs.trader, 0);
            assert_eq!(legs.keeper, 31_000);
            assert_eq!(legs.treasury, 0);
            assert_eq!(legs.vault, 9_9969000);
            assert_eq!(
                legs.trader + legs.vault + legs.keeper + legs.treasury,
                10_0000000,
                "the legs pay out exactly the freed margin"
            );
        });
    }
}
