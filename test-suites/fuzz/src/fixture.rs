//! The natively-registered v2 stack the stateful targets fuzz.
//!
//! [`test_suites::test_fixture::TestFixture`] deploys the market and the vault
//! from their compiled WASM through the factory. That is what the integration
//! suite wants (it exercises the shipped bytecode) and exactly what a fuzzer
//! does not: uploading and instantiating two WASM modules dominates every
//! iteration. This fixture registers the same contracts natively with
//! [`Env::register_at`], reproducing the factory's circular wiring (the vault
//! names the market as its immutable strategy, the market names the vault as its
//! collateral vault) by generating both addresses up front.
//!
//! The real oracle, the real treasury, and a Stellar asset contract for
//! settlement match the integration fixture. The verifier does not: this
//! fixture registers the echoing `MockChainlinkVerifier`, and the integration
//! fixture runs the real verifier on signed reports. Both suites build report
//! bodies with the shared [`test_suites::chainlink_helper`], so the oracle
//! parses identical bodies.

use soroban_sdk::testutils::{Address as _, Ledger, LedgerInfo};
use soroban_sdk::token::{StellarAssetClient, TokenClient};
use soroban_sdk::{contractclient, Address, Env, String};

use crate::price::{START_PRICE, TESTNET_START_PRICE};
use market::{Config, MarketClient, MarketContract};
use oracle::Oracle as OracleContract;
use strategy_vault::StrategyVaultContract;
use test_suites::mock_chainlink::MockChainlinkVerifier;
use treasury::TreasuryContract;

/// The slice of the vault's interface the harness reads.
///
/// `test_suites::dependencies::vault::VaultClient` covers the integration
/// suite's needs. The share-conservation invariant also needs `total_supply`,
/// which the vault exports through its OpenZeppelin `FungibleToken` impl.
#[allow(dead_code)] // consumed by the generated `VaultClient`
#[contractclient(name = "VaultClient")]
pub trait VaultReads {
    /// Raw underlying asset balance held by the vault (token-dec).
    fn total_assets(e: Env) -> i128;
    /// Share supply outstanding (share decimals).
    fn total_supply(e: Env) -> i128;
    /// Share balance of `account`.
    fn balance(e: Env, account: Address) -> i128;
}

/// Strict trade-class staleness window for fills (seconds).
pub const MAX_STALENESS: u64 = 15;

/// Protective close-class window for gap-closing calls (seconds).
pub const CLOSE_STALENESS: u64 = 60;

/// Protocol fee rate handed to the treasury (5%, SCALAR_18).
pub const TREASURY_RATE: i128 = 50_000_000_000_000_000;

/// Settlement-token unit (the SAC is 7-decimal).
pub const SCALAR_7: i128 = 10_000_000;

/// The single dimensionless fixed-point scalar.
pub const SCALAR_18: i128 = 1_000_000_000_000_000_000;

/// Oracle feed id of the fuzzed market.
/// Stream id of the fuzzed feed (shared with the integration fixture).
pub fn feed_id(env: &Env) -> soroban_sdk::BytesN<32> {
    test_suites::constants::feed_btc(env)
}

/// Starting ledger timestamp, matching the integration fixture.
pub const START_TIMESTAMP: u64 = 1_700_000_000;

/// Starting ledger sequence.
pub const START_SEQUENCE: u32 = 100;

/// Vault liquidity seeded by [`Fixture::create`] ($200k, token-dec).
pub const VAULT_SEED: i128 = 200_000 * SCALAR_7;

/// Seconds between the seed LP's deposit order and its fill.
///
/// The commit-then-execute gate requires the fill to strictly postdate the
/// order, so the bootstrap cannot happen in the creation ledger.
const SEED_FILL_DELAY: u64 = 5;

/// Settlement balance minted to each trader ($50k, token-dec).
pub const USER_BALANCE: i128 = 50_000 * SCALAR_7;

/// Traders the stateful targets drive.
pub const USER_COUNT: usize = 3;

/// A market config with borrowing and funding switched on.
///
/// `test_suites::default_config` carries the deployed pair's curves, which are
/// tuned for a live book and move too little over a target's jumps to reach
/// the funding-pool and claim paths. These values keep every bound in
/// `Config::check_valid` satisfied while making both accruals move on the
/// timescales the targets jump across (seconds to weeks).
pub fn fuzz_config() -> Config {
    Config {
        keeper_rate: SCALAR_18 / 10,              // 10%
        min_position_notional: 10_000_000,        // $1
        max_position_notional: 1_000_000_000_000, // $100k
        max_open_interest: 10_000_000_000_000,    // $1M
        min_order_notional: 1_000_000,            // $0.10
        min_order_margin: 1_000_000,              // $0.10
        exec_fee: 100_000,                        // $0.01 flat keeper fee
        fee_dom: SCALAR_18 / 200,                 // 0.5%
        fee_non_dom: 3 * SCALAR_18 / 1000,        // 0.3%
        impact_scalar: 100_000_000_000_000,       // fee rate = notional / $10M
        max_util_open: 8 * SCALAR_18 / 10,        // 80%
        max_util_withdraw: 9 * SCALAR_18 / 10,    // 90%
        init_margin: SCALAR_18 / 10,              // 10% -> 10x max leverage
        maintenance_margin: SCALAR_18 / 20,       // 5%
        liq_fee: SCALAR_18 / 100,                 // 1%
        // The floor (MIN_NOTIONAL_LOCK). A longer lock would spend most of the
        // fuzzer's time bouncing off `NotionalLocked` instead of settling.
        notional_lock: 15,
        target_util: SCALAR_18 / 2, // 50% kink
        // 10% APR slope below the kink, 200% at full utilization.
        borrow_rate: 3_170_979_198,
        increased_borrow_rate: 63_419_583_967,
        // Velocity: at full skew the rate gains ~1e6/s per second, so an hour
        // of one-sided book moves it to ~3.6e9 (roughly 11% APR).
        funding_increase: 1_000_000,
        funding_decrease: 500_000,
        threshold_stable_funding: 5 * SCALAR_18 / 100, // 5%
        threshold_decrease_funding: 2 * SCALAR_18 / 100, // 2%
        funding_min: 0,
        funding_max: 31_709_791_983,            // ~100% APR cap
        adl_max_pnl: SCALAR_18 / 2,             // 50%
        adl_clear_target: 4 * SCALAR_18 / 10,   // 40%
        max_pnl_trader: 9 * SCALAR_18 / 10,     // 90%
        max_pnl_withdraw: 15 * SCALAR_18 / 100, // 15%
        // Non-zero so the `VaultOrderLocked` gate is reachable. The vault
        // target's time jumps straddle it.
        redeem_lock: 60,
        deposit_fee: SCALAR_18 / 1000,         // 10 bps
        redeem_fee: SCALAR_18 / 1000,          // 10 bps
        min_deposit: 1_000_000,                // $0.10
        max_vault_balance: 10_000_000_000_000, // $1M
    }
}

/// The deployed stack plus the actors the targets drive.
pub struct Fixture<'a> {
    pub env: Env,
    pub owner: Address,
    /// The keeper actor: names itself as the reward recipient on every fill.
    pub keeper: Address,
    /// The bootstrap LP. Economic acceptance redeems all its shares at exit.
    pub seed_lp: Address,
    /// Traders (and, in the vault target, LPs).
    pub users: std::vec::Vec<Address>,
    pub token: TokenClient<'a>,
    pub token_admin: StellarAssetClient<'a>,
    pub token_address: Address,
    pub market: MarketClient<'a>,
    pub market_address: Address,
    pub vault: VaultClient<'a>,
    pub vault_address: Address,
    pub treasury_address: Address,
    pub oracle_address: Address,
    pub feed: soroban_sdk::BytesN<32>,
    pub share_scale: i128,
}

impl Fixture<'_> {
    /// Deploy the stack with [`fuzz_config`], seed the vault, and fund
    /// [`USER_COUNT`] traders.
    pub fn create<'a>() -> Fixture<'a> {
        Self::create_with(fuzz_config())
    }

    /// Deploy the stack with `config` (which must pass `Config::check_valid`).
    pub fn create_with<'a>(config: Config) -> Fixture<'a> {
        Self::deploy(config, None)
    }

    /// Recorded testnet economic settings, XLM identity and all stack dials.
    /// Only the position, open-interest and vault caps are multiplied by 1000.
    pub fn create_testnet<'a>() -> Fixture<'a> {
        let profile = test_suites::testnet_profile::TestnetProfile::load();
        Self::deploy(profile.test_config(), Some(profile))
    }

    fn deploy<'a>(
        config: Config,
        profile: Option<test_suites::testnet_profile::TestnetProfile>,
    ) -> Fixture<'a> {
        let env = Env::default();
        test_suites::resources::enforce(&env);
        // Liquidation and vault fills nest the trader's auth under the
        // keeper's call.
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set(LedgerInfo {
            timestamp: START_TIMESTAMP,
            protocol_version: 26,
            sequence_number: START_SEQUENCE,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 16,
            min_persistent_entry_ttl: 16,
            max_entry_ttl: 3_110_400,
        });

        let dials = profile.as_ref().map(|p| p.dials());
        let trade = dials.as_ref().map_or(MAX_STALENESS, |d| d.trade_staleness);
        let close = dials
            .as_ref()
            .map_or(CLOSE_STALENESS, |d| d.close_staleness);
        let reduction = dials.as_ref().map_or(0, |d| d.spread_reduction);
        let treasury_rate = dials.as_ref().map_or(TREASURY_RATE, |d| d.treasury_rate);
        let feed = profile
            .as_ref()
            .map_or_else(|| feed_id(&env), |p| p.feed_id(&env));
        let owner = Address::generate(&env);
        let keeper = Address::generate(&env);

        let sac = env.register_stellar_asset_contract_v2(owner.clone());
        let token_address = sac.address();
        let token = TokenClient::new(&env, &token_address);
        let token_admin = StellarAssetClient::new(&env, &token_address);

        let chainlink = env.register(MockChainlinkVerifier, ());
        let oracle_address = env.register(
            OracleContract,
            (&owner, &chainlink, trade, close, reduction),
        );
        let treasury_address = env.register(TreasuryContract, (&owner, treasury_rate));

        // The factory resolves the market <-> vault cycle by deriving both
        // addresses before either deploys. `register_at` is the native
        // equivalent. The vault must exist first: the market's constructor stores
        // the address but the first `Market::load` calls into it.
        let market_address = Address::generate(&env);
        let vault_address = Address::generate(&env);
        env.register_at(
            &vault_address,
            StrategyVaultContract,
            (
                String::from_str(&env, "Zenex LP"),
                String::from_str(&env, "zLP"),
                token_address.clone(),
                market_address.clone(),
            ),
        );
        env.register_at(
            &market_address,
            MarketContract,
            (
                owner.clone(),
                token_address.clone(),
                vault_address.clone(),
                oracle_address.clone(),
                treasury_address.clone(),
                feed.clone(),
                config,
            ),
        );

        let market = MarketClient::new(&env, &market_address);
        let vault = VaultClient::new(&env, &vault_address);

        // Seed the vault through a real deposit order rather than by minting
        // tokens straight to it. A donation raises `total_assets` without
        // minting shares, which leaves the share price at (assets + 1) per
        // share and makes every later deposit mint zero. The vault-order
        // target would then never price a share at all.
        let seed_lp = Address::generate(&env);
        // A deposit order escrows `amount + exec_fee`, so the seed LP needs a
        // cushion above the amount it deposits.
        token_admin.mint(&seed_lp, &(VAULT_SEED + SCALAR_7));

        let mut users = std::vec::Vec::with_capacity(USER_COUNT);
        for _ in 0..USER_COUNT {
            let user = Address::generate(&env);
            token_admin.mint(&user, &USER_BALANCE);
            users.push(user);
        }

        let f = Fixture {
            env,
            owner,
            keeper,
            seed_lp,
            users,
            token,
            token_admin,
            token_address,
            market,
            market_address,
            vault,
            vault_address,
            treasury_address,
            oracle_address,
            feed,
            share_scale: 10i128.pow(strategy_vault::strategy::DECIMALS_OFFSET),
        };
        f.bootstrap_vault(if profile.is_some() {
            TESTNET_START_PRICE
        } else {
            START_PRICE
        });
        f
    }

    /// Mint the seed LP's shares at a 1:1 price against an empty vault.
    ///
    /// The stress grammar keeps this initial backing. Economic acceptance
    /// explicitly redeems the seed LP after flattening the book.
    fn bootstrap_vault(&self, mark: i128) {
        let id = self
            .market
            .create_vault_order(&self.seed_lp, &0u32, &VAULT_SEED, &0);
        self.jump(SEED_FILL_DELAY);
        self.market
            .execute_vault_order(&self.keeper, &self.seed_lp, &id, &self.seed_price(mark));
    }

    /// A flat, fresh report at the selected feed's bootstrap mark.
    fn seed_price(&self, mark: i128) -> soroban_sdk::Bytes {
        test_suites::chainlink_helper::build_report(
            &self.env,
            &self.feed,
            test_suites::chainlink_helper::ReportTimes::at(self.env.ledger().timestamp()),
            mark,
            mark,
            mark,
        )
    }

    /// The trader at `idx`, wrapped into range.
    pub fn user(&self, idx: usize) -> Address {
        self.users[idx % self.users.len()].clone()
    }

    /// Advance the ledger by `seconds`, moving the sequence one ledger per five
    /// seconds so timestamp accruals and sequence-bounded order expirations
    /// progress together.
    pub fn jump(&self, seconds: u64) {
        let blocks = (seconds / 5) as u32;
        self.env.ledger().set(LedgerInfo {
            timestamp: self.env.ledger().timestamp().saturating_add(seconds),
            protocol_version: 26,
            sequence_number: self.env.ledger().sequence().saturating_add(blocks),
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 16,
            min_persistent_entry_ttl: 16,
            max_entry_ttl: 3_110_400,
        });
    }

    /// An order expiration comfortably ahead of the current sequence.
    pub fn far_expiration(&self) -> u32 {
        self.env.ledger().sequence().saturating_add(100_000)
    }

    /// The settlement-token balance held by the market contract.
    pub fn contract_balance(&self) -> i128 {
        self.token.balance(&self.market_address)
    }
}
