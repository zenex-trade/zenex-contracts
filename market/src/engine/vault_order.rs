//! Vault deposit and redeem orders: the [`VaultOrder`] row and its create
//! and cancel entries. Keepers fill them in [`crate::engine::keeper`].

use crate::dependencies::VaultClient;
use crate::engine::config::Config;
use crate::engine::status::Status;
use crate::errors::MarketError;
use crate::events::{CancelVaultOrder, CreateVaultOrder, RedeemFill};
use crate::storage;
use soroban_sdk::{contracttype, panic_with_error, token::TokenClient, Address, Env, MuxedAddress};

/// The vault liquidity action the user requests. It crosses the ABI as its
/// `u32` discriminant, [`VaultOrder::kind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum VaultOrderKind {
    /// Escrows `amount` assets at creation and mints shares at fill.
    Deposit = 0,
    /// Escrows `amount` shares at creation, then burns them and pays assets
    /// at fill.
    Redeem = 1,
}

impl VaultOrderKind {
    /// Decodes a raw `u32` vault-order-kind discriminant.
    ///
    /// # Errors
    /// - [`MarketError::UnknownKind`] if `raw` is not a vault-order-kind
    ///   discriminant.
    pub(crate) fn from_u32(e: &Env, raw: u32) -> VaultOrderKind {
        match raw {
            0 => VaultOrderKind::Deposit,
            1 => VaultOrderKind::Redeem,
            _ => panic_with_error!(e, MarketError::UnknownKind),
        }
    }
}

/// A pending vault deposit or redeem. Persistent storage, keyed
/// `(user, id)`.
#[derive(Clone, Debug)]
#[contracttype]
pub struct VaultOrder {
    /// The `VaultOrderKind` discriminant.
    pub kind: u32,
    /// The escrowed principal: assets for a deposit (token-dec), or shares
    /// for a redeem (share decimals).
    pub amount: i128,
    /// The slippage bound: the minimum received at fill, net of the vault
    /// fee. Shares for a deposit (share decimals), or assets for a redeem
    /// (token-dec). `0` is unset. A fill quoted below it rejects the order.
    pub min_out: i128,
    /// The keeper execution fee escrowed at creation (token-dec).
    pub exec_fee: i128,
    /// The creation timestamp (seconds), the fill's anti-replay and
    /// `redeem_lock` anchor.
    pub created_at: u64,
}

impl VaultOrder {
    /// Traps on the first rule this order violates at `status` and returns
    /// the decoded kind.
    ///
    /// # Errors
    /// - [`MarketError::UnknownKind`] if `kind` is not a vault-order-kind
    ///   discriminant.
    /// - Refer to [`VaultOrder::check_valid`] errors.
    pub(crate) fn require_valid(&self, e: &Env, status: Status, config: &Config) -> VaultOrderKind {
        let kind = VaultOrderKind::from_u32(e, self.kind);
        self.check_valid(kind, status, config)
            .unwrap_or_else(|err| panic_with_error!(e, err));
        kind
    }

    /// Returns the first rule this order violates at `status`, or `Ok(())`.
    ///
    /// # Errors
    /// - [`MarketError::NegativeValueNotAllowed`] if `amount` or `min_out` is
    ///   negative.
    /// - [`MarketError::MarketFrozen`] if `status` is [`Status::Frozen`].
    /// - [`MarketError::InvalidStatus`] if a deposit is created on a
    ///   [`Status::Retired`] market.
    /// - [`MarketError::InvalidOrder`] if `amount` is zero, a deposit falls
    ///   under `min_deposit`, or its `amount + exec_fee` escrow sum
    ///   overflows.
    fn check_valid(
        &self,
        kind: VaultOrderKind,
        status: Status,
        config: &Config,
    ) -> Result<(), MarketError> {
        if self.amount < 0 || self.min_out < 0 {
            return Err(MarketError::NegativeValueNotAllowed);
        }
        if status == Status::Frozen {
            return Err(MarketError::MarketFrozen);
        }
        // A retired market takes no new deposits.
        if status == Status::Retired && kind == VaultOrderKind::Deposit {
            return Err(MarketError::InvalidStatus);
        }
        // A zero order can never fill.
        if self.amount == 0 {
            return Err(MarketError::InvalidOrder);
        }
        // A redeem has no floor: the flat `exec_fee` already deters dust.
        if kind == VaultOrderKind::Deposit && self.amount < config.min_deposit {
            return Err(MarketError::InvalidOrder);
        }
        // The escrow sum `amount + exec_fee` is checked here rather than
        // trapping in the transfer arithmetic.
        if kind == VaultOrderKind::Deposit && self.amount.checked_add(self.exec_fee).is_none() {
            return Err(MarketError::InvalidOrder);
        }
        Ok(())
    }

    /// Escrows this order's principal and `exec_fee` from `user`. A deposit
    /// moves its assets plus the fee in one settlement-token transfer. A
    /// redeem moves its shares in the vault token and the fee in the
    /// settlement token.
    fn escrow(&self, e: &Env, user: &Address, kind: VaultOrderKind) {
        let market_address = e.current_contract_address();
        match kind {
            VaultOrderKind::Deposit => {
                TokenClient::new(e, &storage::get_token(e)).transfer(
                    user,
                    MuxedAddress::from(market_address),
                    &(self.amount + self.exec_fee),
                );
            }
            VaultOrderKind::Redeem => {
                VaultClient::new(e, &storage::get_vault(e)).transfer(
                    user,
                    &market_address,
                    &self.amount,
                );
                if self.exec_fee > 0 {
                    TokenClient::new(e, &storage::get_token(e)).transfer(
                        user,
                        MuxedAddress::from(market_address),
                        &self.exec_fee,
                    );
                }
            }
        }
    }

    /// Refunds this order's escrow to `user`, the mirror of
    /// [`VaultOrder::escrow`].
    fn refund(&self, e: &Env, user: &Address, kind: VaultOrderKind) {
        let market_address = e.current_contract_address();
        match kind {
            VaultOrderKind::Deposit => {
                TokenClient::new(e, &storage::get_token(e)).transfer(
                    &market_address,
                    MuxedAddress::from(user.clone()),
                    &(self.amount + self.exec_fee),
                );
            }
            VaultOrderKind::Redeem => {
                VaultClient::new(e, &storage::get_vault(e)).transfer(
                    &market_address,
                    user,
                    &self.amount,
                );
                if self.exec_fee > 0 {
                    TokenClient::new(e, &storage::get_token(e)).transfer(
                        &market_address,
                        MuxedAddress::from(user.clone()),
                        &self.exec_fee,
                    );
                }
            }
        }
    }
}

/// Creates a [`VaultOrder`] for the keeper to fill and returns its id. Refer
/// to [`Market::create_vault_order`](crate::Market::create_vault_order).
pub fn create_vault_order(e: &Env, user: &Address, kind: u32, amount: i128, min_out: i128) -> u32 {
    user.require_auth();
    let status = storage::get_status(e);
    let config = storage::get_config(e);
    let order = VaultOrder {
        kind,
        amount,
        min_out,
        exec_fee: config.exec_fee,
        created_at: e.ledger().timestamp(),
    };
    let order_kind = order.require_valid(e, status, &config);
    if status == Status::Retired {
        return instant_redeem(e, user, amount);
    }
    order.escrow(e, user, order_kind);
    store_new_order(e, user, &order)
}

/// Redeems `amount` shares from `user` at once on a retired market and
/// returns id `0`. The retirement clear-out left a flat book, so
/// `net_pnl = 0` is exact.
///
/// # Events
/// - [`RedeemFill`] with id `0`: the receipt.
fn instant_redeem(e: &Env, user: &Address, amount: i128) -> u32 {
    // The shares move here first, as the vault burns from its strategy.
    let market_address = e.current_contract_address();
    let vault_client = VaultClient::new(e, &storage::get_vault(e));
    vault_client.transfer(user, &market_address, &amount);
    let assets = vault_client.strategy_redeem(&amount, user, &market_address, &0);
    RedeemFill {
        user: user.clone(),
        id: 0,
        keeper: user.clone(),
        shares: amount,
        assets,
        fee: 0,
        net_pnl: 0,
    }
    .publish(e);
    0
}

/// Cancels the pending [`VaultOrder`] `(user, id)` and returns the escrowed
/// principal paid back. Refer to
/// [`Market::cancel_vault_order`](crate::Market::cancel_vault_order).
pub fn cancel_vault_order(e: &Env, user: &Address, id: u32) -> i128 {
    user.require_auth();
    if storage::get_status(e) == Status::Frozen {
        panic_with_error!(e, MarketError::MarketFrozen);
    }
    let order = storage::get_vault_order(e, user, id);
    order.refund(e, user, VaultOrderKind::from_u32(e, order.kind));
    storage::remove_vault_order(e, user, id);

    CancelVaultOrder {
        user: user.clone(),
        id,
    }
    .publish(e);
    order.amount
}

/// Stores `order` under the user's next order id, publishes the creation
/// receipt, and returns the id.
fn store_new_order(e: &Env, user: &Address, order: &VaultOrder) -> u32 {
    let id = storage::next_order_id(e, user);
    storage::set_vault_order(e, user, id, order);

    CreateVaultOrder {
        user: user.clone(),
        id,
        order: order.clone(),
    }
    .publish(e);

    id
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::{contract, contractimpl, symbol_short, token};

    #[contract]
    struct Harness;

    // Pays `shares` assets 1:1 to `receiver` from its own token balance.
    #[contract]
    struct MockVault;

    #[contractimpl]
    impl MockVault {
        pub fn __constructor(e: Env, token: Address) {
            e.storage().instance().set(&symbol_short!("token"), &token);
        }

        pub fn strategy_redeem(
            e: Env,
            shares: i128,
            receiver: Address,
            _owner: Address,
            _net_pnl: i128,
        ) -> i128 {
            let token: Address = e.storage().instance().get(&symbol_short!("token")).unwrap();
            token::Client::new(&e, &token).transfer(
                &e.current_contract_address(),
                &receiver,
                &shares,
            );
            shares
        }

        pub fn preview_redeem(_e: Env, shares: i128, _net_pnl: i128) -> i128 {
            shares
        }

        pub fn transfer(_e: Env, _from: Address, _to: Address, _amount: i128) {}
    }

    // A negative fill bound is rejected at creation.
    #[test]
    #[should_panic(expected = "#710")]
    fn create_order_rejects_negative_fill_bound() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let user = Address::generate(&e);
        e.as_contract(&contract_id, || {
            storage::set_status(&e, &Status::Active);
            storage::set_config(&e, &crate::engine::config::test_config());
            create_vault_order(&e, &user, VaultOrderKind::Deposit as u32, 5_000_000, -1);
        });
    }

    // A deposit under the min_deposit floor is rejected at creation.
    #[test]
    #[should_panic(expected = "#732")]
    fn create_deposit_rejected_below_minimum() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let user = Address::generate(&e);
        e.as_contract(&contract_id, || {
            storage::set_status(&e, &Status::Active);
            storage::set_config(&e, &crate::engine::config::test_config());
            create_vault_order(&e, &user, VaultOrderKind::Deposit as u32, 999_999, 0);
        });
    }

    // On a Retired market a redeem forwards straight to the vault. The user
    // is paid directly, and no order is stored (id 0). A `RedeemFill`
    // receipt carries the id-0 marker, the assets paid, and a zero fee and
    // PnL mark.
    #[test]
    fn retired_redeem_forwards_to_vault() {
        use soroban_sdk::testutils::Events as _;
        use soroban_sdk::{map, vec, IntoVal, Map, Symbol, Val};

        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        let vault = e.register(MockVault, (token.address(),));
        token::StellarAssetClient::new(&e, &token.address()).mint(&vault, &500);

        let user = Address::generate(&e);
        e.as_contract(&contract_id, || {
            storage::set_vault(&e, &vault);
            storage::set_status(&e, &Status::Retired);
            storage::set_config(&e, &crate::engine::config::test_config());
            let id = create_vault_order(&e, &user, VaultOrderKind::Redeem as u32, 200, 0);
            assert_eq!(id, 0);
        });

        // The receipt's topics name the event, the user, and the id-0
        // marker. The data carries the self-executing user as the keeper and
        // the redeemed legs with a zero fee and PnL mark.
        let redeem_data: Map<Symbol, Val> = map![
            &e,
            (Symbol::new(&e, "keeper"), user.clone().into_val(&e)),
            (Symbol::new(&e, "shares"), 200_i128.into_val(&e)),
            (Symbol::new(&e, "assets"), 200_i128.into_val(&e)),
            (Symbol::new(&e, "fee"), 0_i128.into_val(&e)),
            (Symbol::new(&e, "net_pnl"), 0_i128.into_val(&e)),
        ];
        assert_eq!(
            e.events().all().filter_by_contract(&contract_id),
            vec![
                &e,
                (
                    contract_id,
                    (Symbol::new(&e, "redeem_fill"), user.clone(), 0u32).into_val(&e),
                    redeem_data.into_val(&e),
                ),
            ],
        );

        assert_eq!(token::Client::new(&e, &token.address()).balance(&user), 200);
    }

    // A zero-share retired redeem is rejected like its pending sibling
    // instead of emitting a spurious zero `RedeemFill` receipt.
    #[test]
    #[should_panic(expected = "#732")]
    fn retired_redeem_rejects_zero_amount() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let user = Address::generate(&e);
        e.as_contract(&contract_id, || {
            storage::set_status(&e, &Status::Retired);
            storage::set_config(&e, &crate::engine::config::test_config());
            create_vault_order(&e, &user, VaultOrderKind::Redeem as u32, 0, 0);
        });
    }

    #[test]
    #[should_panic(expected = "#702")]
    fn deposit_rejected_on_retired_market() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let user = Address::generate(&e);
        e.as_contract(&contract_id, || {
            storage::set_status(&e, &Status::Retired);
            storage::set_config(&e, &crate::engine::config::test_config());
            create_vault_order(&e, &user, VaultOrderKind::Deposit as u32, 200, 0);
        });
    }

    // A wind-down market still accepts deposits. The vault keeps taking
    // liquidity to fund the remaining payouts.
    #[test]
    fn deposit_allowed_on_delisted_market() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        let user = Address::generate(&e);
        token::StellarAssetClient::new(&e, &token.address()).mint(&user, &10_000_000);

        e.as_contract(&contract_id, || {
            storage::set_token(&e, &token.address());
            storage::set_status(&e, &Status::Delisted);
            storage::set_config(&e, &crate::engine::config::test_config());
            let id = create_vault_order(&e, &user, VaultOrderKind::Deposit as u32, 5_000_000, 0);
            assert_eq!(id, 1);
            assert_eq!(storage::get_vault_order(&e, &user, id).amount, 5_000_000);
        });
        // The escrow carries the exec_fee alongside the deposit amount.
        assert_eq!(
            token::Client::new(&e, &token.address()).balance(&contract_id),
            5_100_000
        );
    }

    // Cancelling a pending deposit refunds the escrowed assets in full.
    #[test]
    fn cancel_deposit_refunds_and_releases_escrow() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        let user = Address::generate(&e);
        token::StellarAssetClient::new(&e, &token.address()).mint(&user, &10_000_000);

        let id = e.as_contract(&contract_id, || {
            storage::set_token(&e, &token.address());
            storage::set_status(&e, &Status::Active);
            storage::set_config(&e, &crate::engine::config::test_config());
            create_vault_order(&e, &user, VaultOrderKind::Deposit as u32, 5_000_000, 0)
        });
        e.as_contract(&contract_id, || {
            assert_eq!(cancel_vault_order(&e, &user, id), 5_000_000);
        });
        assert_eq!(
            token::Client::new(&e, &token.address()).balance(&user),
            10_000_000
        );
    }

    // A cancel is halted while the market is Frozen. The escrow stays put.
    #[test]
    #[should_panic(expected = "#704")]
    fn cancel_rejected_while_frozen() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        let user = Address::generate(&e);
        token::StellarAssetClient::new(&e, &token.address()).mint(&user, &10_000_000);

        let id = e.as_contract(&contract_id, || {
            storage::set_token(&e, &token.address());
            storage::set_status(&e, &Status::Active);
            storage::set_config(&e, &crate::engine::config::test_config());
            create_vault_order(&e, &user, VaultOrderKind::Deposit as u32, 5_000_000, 0)
        });
        e.as_contract(&contract_id, || {
            storage::set_status(&e, &Status::Frozen);
            cancel_vault_order(&e, &user, id);
        });
    }

    // Creating a redeem escrows the shares. Cancelling returns them in full.
    #[test]
    fn redeem_create_and_cancel_track_share_escrow() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        let vault = e.register(MockVault, (token.address(),));
        let user = Address::generate(&e);
        token::StellarAssetClient::new(&e, &token.address()).mint(&user, &1_000_000);

        let id = e.as_contract(&contract_id, || {
            storage::set_vault(&e, &vault);
            storage::set_token(&e, &token.address());
            storage::set_status(&e, &Status::Active);
            storage::set_config(&e, &crate::engine::config::test_config());
            create_vault_order(&e, &user, VaultOrderKind::Redeem as u32, 5_000_000, 0)
        });
        e.as_contract(&contract_id, || {
            assert_eq!(cancel_vault_order(&e, &user, id), 5_000_000);
        });
    }

    // Cancelling requires the order user's auth.
    #[test]
    #[should_panic(expected = "Error(Auth, InvalidAction)")]
    fn cancel_requires_user_auth() {
        let e = Env::default();
        let contract_id = e.register(Harness, ());
        let user = Address::generate(&e);
        e.as_contract(&contract_id, || {
            storage::set_vault_order(
                &e,
                &user,
                1,
                &VaultOrder {
                    kind: VaultOrderKind::Deposit as u32,
                    amount: 5_000_000,
                    min_out: 0,
                    exec_fee: 0,
                    created_at: 0,
                },
            );
            cancel_vault_order(&e, &user, 1);
        });
    }

    // Core validation fixtures: a deposit at the floor and a one-share redeem.
    fn order_with(kind: VaultOrderKind, amount: i128, min_out: i128) -> VaultOrder {
        VaultOrder {
            kind: kind as u32,
            amount,
            min_out,
            exec_fee: 0,
            created_at: 0,
        }
    }

    fn config() -> Config {
        crate::engine::config::test_config()
    }

    // A negative amount or bound rejects first, before any status rule.
    #[test]
    fn check_valid_rejects_negatives_first() {
        let config = config();
        for kind in [VaultOrderKind::Deposit, VaultOrderKind::Redeem] {
            assert_eq!(
                order_with(kind, -1, 0).check_valid(kind, Status::Active, &config),
                Err(MarketError::NegativeValueNotAllowed)
            );
            assert_eq!(
                order_with(kind, 1, -1).check_valid(kind, Status::Active, &config),
                Err(MarketError::NegativeValueNotAllowed)
            );
            assert_eq!(
                order_with(kind, -1, 0).check_valid(kind, Status::Frozen, &config),
                Err(MarketError::NegativeValueNotAllowed)
            );
        }
    }

    // A frozen market takes no vault order of either kind.
    #[test]
    fn check_valid_frozen_rejects_both_kinds() {
        let config = config();
        for kind in [VaultOrderKind::Deposit, VaultOrderKind::Redeem] {
            assert_eq!(
                order_with(kind, config.min_deposit, 0).check_valid(kind, Status::Frozen, &config),
                Err(MarketError::MarketFrozen)
            );
        }
    }

    // A retired market rejects deposits and still accepts a redeem.
    #[test]
    fn check_valid_retired_rejects_deposit_only() {
        let config = config();
        assert_eq!(
            order_with(VaultOrderKind::Deposit, config.min_deposit, 0).check_valid(
                VaultOrderKind::Deposit,
                Status::Retired,
                &config
            ),
            Err(MarketError::InvalidStatus)
        );
        assert_eq!(
            order_with(VaultOrderKind::Redeem, 1, 0).check_valid(
                VaultOrderKind::Redeem,
                Status::Retired,
                &config
            ),
            Ok(())
        );
    }

    // A zero order can never fill, whatever the kind or status.
    #[test]
    fn check_valid_rejects_zero_amount() {
        let config = config();
        for kind in [VaultOrderKind::Deposit, VaultOrderKind::Redeem] {
            assert_eq!(
                order_with(kind, 0, 0).check_valid(kind, Status::Active, &config),
                Err(MarketError::InvalidOrder)
            );
        }
        assert_eq!(
            order_with(VaultOrderKind::Redeem, 0, 0).check_valid(
                VaultOrderKind::Redeem,
                Status::Retired,
                &config
            ),
            Err(MarketError::InvalidOrder)
        );
    }

    // The deposit floor rejects one unit under and passes exactly at it. A
    // redeem has no floor.
    #[test]
    fn check_valid_deposit_floor_and_redeem_without_one() {
        let config = config();
        assert_eq!(
            order_with(VaultOrderKind::Deposit, config.min_deposit - 1, 0).check_valid(
                VaultOrderKind::Deposit,
                Status::Active,
                &config
            ),
            Err(MarketError::InvalidOrder)
        );
        assert_eq!(
            order_with(VaultOrderKind::Deposit, config.min_deposit, 0).check_valid(
                VaultOrderKind::Deposit,
                Status::Active,
                &config
            ),
            Ok(())
        );
        assert_eq!(
            order_with(VaultOrderKind::Redeem, 1, 0).check_valid(
                VaultOrderKind::Redeem,
                Status::Active,
                &config
            ),
            Ok(())
        );
    }

    // A deposit whose `amount + exec_fee` escrow sum overflows is rejected
    // here, not in the transfer arithmetic.
    #[test]
    fn check_valid_rejects_deposit_escrow_overflow() {
        let config = config();
        let order = VaultOrder {
            exec_fee: 1,
            ..order_with(VaultOrderKind::Deposit, i128::MAX, 0)
        };
        assert_eq!(
            order.check_valid(VaultOrderKind::Deposit, Status::Active, &config),
            Err(MarketError::InvalidOrder)
        );
    }
}
