//! The integration fixture: a real market + strategy-vault pair deployed
//! from compiled WASM, including the factory, oracle, treasury and real
//! verifier. Each top-level invocation enforces shared resource limits.
//!
//! Deploy with [`TestFixture::create`] (default config),
//! [`TestFixture::create_with`] (custom config), or
//! [`TestFixture::create_from_wasm`] to force WASM even when the explicit
//! `native-contracts` coverage feature is enabled. Prices are signed through
//! [`crate::chainlink_helper`] and verified by the real verifier. The env mocks
//! every auth, non-root auth included.

use crate::chainlink_helper::{self, ReportTimes};
use crate::dependencies::factory::FACTORY_WASM;
use crate::dependencies::market::MARKET_WASM;
use crate::dependencies::oracle::ORACLE_WASM;
use crate::dependencies::treasury::TREASURY_WASM;
use crate::dependencies::vault::{VaultClient, VAULT_WASM};
use crate::dependencies::verifier;
use crate::token::create_stellar_token;
use crate::{constants, default_config, to_factory_config};

use factory::{FactoryClient, FactoryContract, FactoryInitMeta};
use market::{Config, MarketClient, OrderKind, Position, VaultOrderKind};
use oracle::{Oracle, OracleClient};
use soroban_sdk::testutils::{Address as _, BytesN as _, Events, Ledger, LedgerInfo};
use soroban_sdk::token::StellarAssetClient;
use soroban_sdk::{
    xdr::ScErrorType, Address, Bytes, BytesN, Env, Error, IntoVal, String, Symbol, Val,
    Vec as SdkVec,
};
use treasury::{TreasuryClient, TreasuryContract};

/// Strict trade-class staleness window the fixture's oracle applies to
/// fills (seconds). The oracle caps it at `MAX_TRADE_STALENESS_SECONDS`
/// = 15.
pub const TRADE_STALENESS: u64 = 15;

/// Protective close-class window for gap-closing calls (seconds):
/// liquidation, ADL, and accrue. The oracle caps it at
/// `MAX_CLOSE_STALENESS_SECONDS` = 120.
pub const CLOSE_STALENESS: u64 = 60;

/// Protocol fee rate handed to the treasury (5%, SCALAR_18). The market reads it
/// at every fee split, so settlement asserts derive their treasury legs from it.
pub const TREASURY_RATE: i128 = 50_000_000_000_000_000;

/// Trade-class staleness window the deployed oracle applies to fills
/// (seconds), from `zenex-ops/deployments/testnet/2026-09-05-testnet-v2`.
pub const DEPLOYED_TRADE_STALENESS: u64 = 10;

/// Close-class staleness window the deployed oracle applies to gap-closing
/// calls (seconds), from `zenex-ops/deployments/testnet/2026-09-05-testnet-v2`.
pub const DEPLOYED_CLOSE_STALENESS: u64 = 60;

/// Symmetric bid/ask narrowing the deployed oracle applies (50%,
/// `SCALAR_18`), from `zenex-ops/deployments/testnet/2026-09-05-testnet-v2`.
pub const DEPLOYED_SPREAD_REDUCTION: i128 = 500_000_000_000_000_000;

/// Protocol fee rate the deployed treasury charges (30%, `SCALAR_18`), from
/// `zenex-ops/deployments/testnet/2026-09-05-testnet-v2`.
pub const DEPLOYED_TREASURY_RATE: i128 = 300_000_000_000_000_000;

/// Ledgers between the current ledger and an order's default expiration.
const DEFAULT_EXPIRATION_LEDGERS: u32 = 100_000;

/// The stack dials the oracle, treasury, and vault take at deployment, which
/// no `market::Config` carries.
///
/// The market config alone does not decide how a fill behaves. The oracle's
/// two staleness windows gate which prices a call accepts. The spread
/// reduction moves the entry and exit marks. The treasury rate takes its cut
/// before the vault's.
#[derive(Clone, Copy)]
pub struct StackDials {
    /// Maximum report age for strict (fill) verifications (seconds).
    pub trade_staleness: u64,
    /// Maximum report age for protective verifications (seconds).
    pub close_staleness: u64,
    /// Symmetric bid/ask narrowing toward the mid (`SCALAR_18`).
    pub spread_reduction: i128,
    /// The treasury's fee rate (`SCALAR_18`).
    pub treasury_rate: i128,
}

impl StackDials {
    /// The default fixture dials, which most suites deploy against.
    pub fn fixture() -> Self {
        StackDials {
            trade_staleness: TRADE_STALENESS,
            close_staleness: CLOSE_STALENESS,
            spread_reduction: 0,
            treasury_rate: TREASURY_RATE,
        }
    }

    /// The dials the testnet stack runs, from the deployment record
    /// `zenex-ops/deployments/testnet/2026-09-05-testnet-v2` of 2026-09-05.
    ///
    /// Every field mirrors the record. Refer to [`crate::deployed_config`]
    /// for the market config from the same record, and for the two
    /// categories that diverge from it.
    pub fn deployed() -> Self {
        StackDials {
            trade_staleness: DEPLOYED_TRADE_STALENESS,
            close_staleness: DEPLOYED_CLOSE_STALENESS,
            spread_reduction: DEPLOYED_SPREAD_REDUCTION,
            treasury_rate: DEPLOYED_TREASURY_RATE,
        }
    }
}

/// A decoded create-then-try-fill outcome, for test assertions.
///
/// [`TestFixture::create_and_try_fill`] and the vault flows create an order,
/// then try its fill, and record the created id and the fill outcome (payout
/// on a landed fill, or the contract error when the order rests) here.
pub struct FillAttempt {
    pub id: u32,        // the created order id
    pub filled: bool,   // the fill landed
    pub payout: i128,   // the fill payout when filled or rejected (token-dec)
    pub error: u32,     // 0 unless the fill failed: then its contract error code, u32::MAX untyped
    pub rejected: bool, // a vault fill below min_out removed the order and paid the keeper
}

/// Every deployed contract, funded users, and the signing/oracle anchors.
pub struct TestFixture<'a> {
    pub env: Env,
    pub owner: Address,
    pub keeper: Address,
    pub users: std::vec::Vec<Address>,
    pub token: StellarAssetClient<'a>,
    pub token_address: Address,
    pub oracle: OracleClient<'a>,
    pub treasury: TreasuryClient<'a>,
    pub factory: FactoryClient<'a>,
    pub market: MarketClient<'a>,
    pub vault: VaultClient<'a>,
    pub chainlink_verifier: Address,
    pub feed_id: BytesN<32>,
    verifier_fault_tolerance: u32,
}

impl TestFixture<'_> {
    /// Deploy the full stack with the shared [`default_config`].
    pub fn create<'a>() -> TestFixture<'a> {
        Self::create_with(default_config())
    }

    /// Deploy the full stack with `config` (must pass `Config::check_valid`).
    pub fn create_with<'a>(config: Config) -> TestFixture<'a> {
        Self::build(
            config,
            StackDials::fixture(),
            !cfg!(any(feature = "native-contracts", coverage)),
        )
    }

    /// Force the complete WASM stack, including under `native-contracts`.
    /// Resource and code/instance TTL tests must always use this constructor.
    pub fn create_from_wasm<'a>(config: Config) -> TestFixture<'a> {
        Self::build(config, StackDials::fixture(), true)
    }

    /// Deploy the broad stress stack: the deliberately enlarged
    /// [`crate::deployed_config`] market config and the recorded
    /// [`StackDials::deployed`] oracle, treasury, and vault dials. The vault
    /// holds nothing, so the caller seeds it. Use [`Self::create_testnet`]
    /// for the recorded testnet economics and its XLM feed metadata.
    pub fn create_deployed<'a>() -> TestFixture<'a> {
        Self::build(
            crate::deployed_config(),
            StackDials::deployed(),
            !cfg!(any(feature = "native-contracts", coverage)),
        )
    }

    /// Deploy the full stack with `config` and explicit `dials`.
    pub fn create_with_dials<'a>(config: Config, dials: StackDials) -> TestFixture<'a> {
        Self::build(
            config,
            dials,
            !cfg!(any(feature = "native-contracts", coverage)),
        )
    }

    fn build<'a>(config: Config, dials: StackDials, from_wasm: bool) -> TestFixture<'a> {
        Self::build_for_inputs(config, dials, from_wasm, None)
    }

    /// Deploy the recorded economics with explicit test-capacity overrides, feed and
    /// vault metadata. The vault starts empty. Seed it with real LP deposits.
    pub fn create_testnet<'a>() -> TestFixture<'a> {
        let inputs = crate::testnet_profile::TestnetProfile::load();
        Self::build_for_inputs(
            inputs.test_config(),
            inputs.dials(),
            !cfg!(any(feature = "native-contracts", coverage)),
            Some(&inputs),
        )
    }

    /// Deploy the recorded economics with every runtime Zenex contract in WASM.
    pub fn create_testnet_from_wasm<'a>() -> TestFixture<'a> {
        let inputs = crate::testnet_profile::TestnetProfile::load();
        Self::build_for_inputs(inputs.test_config(), inputs.dials(), true, Some(&inputs))
    }

    fn build_for_inputs<'a>(
        config: Config,
        dials: StackDials,
        from_wasm: bool,
        inputs: Option<&crate::testnet_profile::TestnetProfile>,
    ) -> TestFixture<'a> {
        let env = Env::default();
        crate::resources::enforce(&env);
        env.mock_all_auths_allowing_non_root_auth();

        // A nonzero starting ledger with a generous entry TTL: order expirations
        // are sequence-bounded and price staleness is timestamp-bounded.
        env.ledger().set(LedgerInfo {
            timestamp: 1_700_000_000,
            protocol_version: 26,
            sequence_number: 100,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 16,
            min_persistent_entry_ttl: 16,
            max_entry_ttl: 3_110_400,
        });

        let owner = Address::generate(&env);
        let keeper = Address::generate(&env);

        let (token_address, token) = create_stellar_token(&env, &owner);

        let chainlink_verifier = env.register(verifier::WASM, (&owner,));
        let verifier_fault_tolerance = chainlink_helper::DEFAULT_FAULT_TOLERANCE;
        verifier::Client::new(&env, &chainlink_verifier).set_config(
            &BytesN::from_array(&env, &chainlink_helper::CONFIG),
            &chainlink_helper::signer_addresses(&env, 1, 3 * verifier_fault_tolerance + 1),
            &verifier_fault_tolerance,
        );
        let oracle_args = (
            &owner,
            &chainlink_verifier,
            dials.trade_staleness,
            dials.close_staleness,
            dials.spread_reduction,
        );
        let oracle_address = if from_wasm {
            env.register(ORACLE_WASM, oracle_args)
        } else {
            env.register(Oracle, oracle_args)
        };
        let oracle = OracleClient::new(&env, &oracle_address);

        let treasury_args = (&owner, dials.treasury_rate);
        let treasury_address = if from_wasm {
            env.register(TREASURY_WASM, treasury_args)
        } else {
            env.register(TreasuryContract, treasury_args)
        };
        let treasury = TreasuryClient::new(&env, &treasury_address);

        let market_hash = env.deployer().upload_contract_wasm(MARKET_WASM);
        let vault_hash = env.deployer().upload_contract_wasm(VAULT_WASM);
        let init_meta = FactoryInitMeta {
            market_hash,
            vault_hash,
            treasury: treasury_address,
        };
        let factory_address = if from_wasm {
            env.register(FACTORY_WASM, (&owner, init_meta))
        } else {
            env.register(FactoryContract {}, (&owner, init_meta))
        };
        let factory = FactoryClient::new(&env, &factory_address);

        let feed_id = inputs.map_or_else(|| constants::feed_btc(&env), |input| input.feed_id(&env));
        let salt = BytesN::<32>::random(&env);
        let (market_address, vault_address) = factory.deploy(
            &owner,
            &salt,
            &token_address,
            &oracle_address,
            &feed_id,
            &to_factory_config(&config),
            &String::from_str(&env, inputs.map_or("Zenex LP", |input| &input.vault.name)),
            &String::from_str(&env, inputs.map_or("zLP", |input| &input.vault.symbol)),
        );
        let market = MarketClient::new(&env, &market_address);
        let vault = VaultClient::new(&env, &vault_address);

        TestFixture {
            env,
            owner,
            keeper,
            users: std::vec::Vec::new(),
            token,
            token_address,
            oracle,
            treasury,
            factory,
            market,
            vault,
            chainlink_verifier,
            feed_id,
            verifier_fault_tolerance,
        }
    }

    // ---- users and balances ----

    /// Mint `amount` (token-dec) of the settlement token to `to`.
    pub fn mint(&self, to: &Address, amount: i128) {
        self.token.mint(to, &amount);
    }

    /// Create a fresh user, fund it with `balance` (token-dec), record it in
    /// `users`, and return its address.
    pub fn new_user(&mut self, balance: i128) -> Address {
        let user = Address::generate(&self.env);
        self.token.mint(&user, &balance);
        self.users.push(user.clone());
        user
    }

    /// Seed vault liquidity by minting `amount` (token-dec) directly to the vault
    /// address. `total_assets` reads the vault's token balance, so this raises
    /// backing without minting shares (a donation, sufficient for test liquidity).
    pub fn seed_vault(&self, amount: i128) {
        self.token.mint(&self.vault.address, &amount);
    }

    /// Read `who`'s settlement-token balance (token-dec).
    pub fn balance(&self, who: &Address) -> i128 {
        soroban_sdk::token::TokenClient::new(&self.env, &self.token_address).balance(who)
    }

    /// Read `who`'s vault share balance (share decimals).
    pub fn share_balance(&self, who: &Address) -> i128 {
        self.vault.balance(who)
    }

    // ---- prices ----

    /// A single-feed price blob at `dollars` (whole USD) at the current ledger
    /// timestamp: raw price = `dollars * PRICE_SCALAR`.
    pub fn price_usd(&self, dollars: i128) -> Bytes {
        self.price(dollars * constants::PRICE_SCALAR)
    }

    /// A report blob at the current ledger timestamp with
    /// `bid == ask == price` (`price` in [`constants::PRICE_SCALAR`] units).
    pub fn price(&self, price: i128) -> Bytes {
        self.price_bid_ask(price, price, price)
    }

    /// A report blob with an explicit `bid`/`ask` spread around `mid`, at the
    /// current ledger timestamp.
    pub fn price_bid_ask(&self, mid: i128, bid: i128, ask: i128) -> Bytes {
        self.report_at(
            ReportTimes::at(self.env.ledger().timestamp()),
            mid,
            bid,
            ask,
        )
    }

    /// A report blob with explicit [`ReportTimes`], for the timing suites.
    pub fn report_at(&self, times: ReportTimes, mid: i128, bid: i128, ask: i128) -> Bytes {
        self.sign_report(&chainlink_helper::build_report(
            &self.env,
            &self.feed_id,
            times,
            mid,
            bid,
            ask,
        ))
    }

    /// Sign a report body, including custom feeds or malformed fields in rejection tests.
    pub fn sign_report(&self, body: &Bytes) -> Bytes {
        chainlink_helper::envelope(
            &self.env,
            body,
            chainlink_helper::CONFIG,
            1,
            self.verifier_fault_tolerance + 1,
        )
    }

    /// Client for the real verifier shared by all integration fixtures.
    pub fn verifier(&self) -> verifier::Client<'_> {
        verifier::Client::new(&self.env, &self.chainlink_verifier)
    }

    /// Change the test signer threshold. Reports then carry exactly f+1 signatures.
    pub fn set_verifier_fault_tolerance(&mut self, f: u32) {
        assert!((1..=10).contains(&f));
        self.verifier().update_config(
            &BytesN::from_array(&self.env, &chainlink_helper::CONFIG),
            &chainlink_helper::signer_addresses(
                &self.env,
                1,
                3 * self.verifier_fault_tolerance + 1,
            ),
            &chainlink_helper::signer_addresses(&self.env, 1, 3 * f + 1),
            &f,
        );
        self.verifier_fault_tolerance = f;
    }

    pub fn verifier_fault_tolerance(&self) -> u32 {
        self.verifier_fault_tolerance
    }

    /// Verifier events in the last top-level invocation. Read before another client call.
    pub fn verified_count(&self) -> u32 {
        self.env
            .events()
            .all()
            .filter_by_contract(&self.chainlink_verifier)
            .events()
            .iter()
            .filter(|event| {
                let soroban_sdk::xdr::ContractEventBody::V0(body) = &event.body;
                body.topics.first()
                    == Some(&soroban_sdk::xdr::ScVal::Symbol(
                        soroban_sdk::xdr::ScSymbol("verified".try_into().unwrap()),
                    ))
            })
            .count() as u32
    }

    // ---- position flows ----

    /// Open or grow a position: create a market order and fill it in the same
    /// ledger, with the user as its own keeper (the fill reward round-trips). Returns the keeper
    /// payout (token-dec).
    pub fn open(
        &self,
        user: &Address,
        is_long: bool,
        notional: i128,
        margin: i128,
        price: &Bytes,
    ) -> i128 {
        self.create_and_fill(
            user,
            user,
            is_long,
            OrderKind::MarketIncrease as u32,
            notional,
            margin,
            0,
            0,
            self.far_expiration(),
            price,
        )
    }

    /// Close (or reduce) a position: create a market order and fill it in the
    /// same ledger, with the user as keeper. `notional`/`margin` are the decrease magnitudes.
    /// Returns the keeper payout (token-dec).
    pub fn close(
        &self,
        user: &Address,
        is_long: bool,
        notional: i128,
        margin: i128,
        price: &Bytes,
    ) -> i128 {
        self.create_and_fill(
            user,
            user,
            is_long,
            OrderKind::MarketDecrease as u32,
            notional,
            margin,
            0,
            0,
            self.far_expiration(),
            price,
        )
    }

    /// Create an order on the market, then fill it in the same ledger
    /// (fill-or-kill: a failed fill panics). A market order may fill in its
    /// creation ledger. Returns the keeper payout (token-dec).
    #[allow(clippy::too_many_arguments)]
    pub fn create_and_fill(
        &self,
        keeper: &Address,
        user: &Address,
        is_long: bool,
        kind: u32,
        notional: i128,
        margin: i128,
        trigger_price: i128,
        price_bound: i128,
        expiration: u32,
        price: &Bytes,
    ) -> i128 {
        let id = self.market.create_order(
            user,
            &is_long,
            &kind,
            &notional,
            &margin,
            &trigger_price,
            &price_bound,
            &expiration,
        );
        self.market.execute_order(keeper, user, &id, price)
    }

    /// Create an order on the market, then try its fill in the same ledger.
    /// A failed fill leaves the order resting. Returns the [`FillAttempt`].
    #[allow(clippy::too_many_arguments)]
    pub fn create_and_try_fill(
        &self,
        keeper: &Address,
        user: &Address,
        is_long: bool,
        kind: u32,
        notional: i128,
        margin: i128,
        trigger_price: i128,
        price_bound: i128,
        expiration: u32,
        price: &Bytes,
    ) -> FillAttempt {
        let id = self.market.create_order(
            user,
            &is_long,
            &kind,
            &notional,
            &margin,
            &trigger_price,
            &price_bound,
            &expiration,
        );
        match self.market.try_execute_order(keeper, user, &id, price) {
            Ok(Ok(payout)) => FillAttempt {
                id,
                filled: true,
                payout,
                error: 0,
                rejected: false,
            },
            Err(Ok(error)) => FillAttempt {
                id,
                filled: false,
                payout: 0,
                error: contract_error_code(error),
                rejected: false,
            },
            _ => FillAttempt {
                id,
                filled: false,
                payout: 0,
                error: u32::MAX,
                rejected: false,
            },
        }
    }

    // ---- order flows (direct to the market) ----

    /// Submit a resting order directly on the market contract. Returns the
    /// order id. The escrow (an increase's margin plus `exec_fee`, a decrease's
    /// `exec_fee`) moves by direct transfer under the user's mocked auth.
    #[allow(clippy::too_many_arguments)]
    pub fn submit_order(
        &self,
        user: &Address,
        is_long: bool,
        kind: OrderKind,
        notional: i128,
        margin: i128,
        trigger_price: i128,
        price_bound: i128,
        expiration: u32,
    ) -> u32 {
        self.market.create_order(
            user,
            &is_long,
            &(kind as u32),
            &notional,
            &margin,
            &trigger_price,
            &price_bound,
            &expiration,
        )
    }

    /// Fill a resting order at `price`. Returns the keeper payout (token-dec).
    pub fn execute_order(&self, keeper: &Address, user: &Address, id: u32, price: &Bytes) -> i128 {
        self.market.execute_order(keeper, user, &id, price)
    }

    // ---- vault flows ----

    /// Create a vault deposit order and fill it a ledger later, satisfying
    /// the commit-then-execute price-time gate. The fill price must not
    /// predate the order's creation. The fill must also run in a later
    /// ledger, so no single-call path can price it.
    ///
    /// `fill_price` is the mark the fill leg verifies at (price_scalar units).
    pub fn vault_deposit(
        &self,
        keeper: &Address,
        user: &Address,
        amount: i128,
        min_out: i128,
        fill_price: i128,
    ) -> FillAttempt {
        self.create_and_fill_vault_order(
            keeper,
            user,
            VaultOrderKind::Deposit,
            amount,
            min_out,
            fill_price,
        )
    }

    /// Create a vault redeem order and fill it a ledger later, satisfying
    /// the commit-then-execute price-time gate. Refer to
    /// [`TestFixture::vault_deposit`] for the two-step rationale.
    pub fn vault_redeem(
        &self,
        keeper: &Address,
        user: &Address,
        shares: i128,
        min_out: i128,
        fill_price: i128,
    ) -> FillAttempt {
        self.create_and_fill_vault_order(
            keeper,
            user,
            VaultOrderKind::Redeem,
            shares,
            min_out,
            fill_price,
        )
    }

    /// Create a vault order directly, jump one ledger, and fill it at
    /// `fill_price`. Returns the [`FillAttempt`].
    ///
    /// A [`Status::Retired`](market::Status)-market redeem pays out at
    /// creation (id `0`). The helper reports it already filled.
    fn create_and_fill_vault_order(
        &self,
        keeper: &Address,
        user: &Address,
        kind: VaultOrderKind,
        amount: i128,
        min_out: i128,
        fill_price: i128,
    ) -> FillAttempt {
        let id = self
            .market
            .create_vault_order(user, &(kind as u32), &amount, &min_out);
        if id == 0 {
            return FillAttempt {
                id,
                filled: true,
                payout: 0,
                error: 0,
                rejected: false,
            };
        }
        self.jump(5);
        let price = self.price(fill_price);
        match self
            .market
            .try_execute_vault_order(keeper, user, &id, &price)
        {
            Ok(Ok(payout)) => {
                let rejected = self.vault_order_rejected(user, id);
                FillAttempt {
                    id,
                    filled: !rejected,
                    payout,
                    error: 0,
                    rejected,
                }
            }
            Err(Ok(error)) => FillAttempt {
                id,
                filled: false,
                payout: 0,
                error: contract_error_code(error),
                rejected: false,
            },
            _ => FillAttempt {
                id,
                filled: false,
                payout: 0,
                error: u32::MAX,
                rejected: false,
            },
        }
    }

    /// Whether the last invocation published the market's `reject_vault_order`
    /// receipt for `(user, id)`: the fill quoted below the order's `min_out`.
    pub fn vault_order_rejected(&self, user: &Address, id: u32) -> bool {
        let topic: SdkVec<Val> = (
            Symbol::new(&self.env, "reject_vault_order"),
            user.clone(),
            id,
        )
            .into_val(&self.env);
        let expected: soroban_sdk::xdr::VecM<soroban_sdk::xdr::ScVal> = topic.into();
        self.env
            .events()
            .all()
            .filter_by_contract(&self.market.address)
            .events()
            .iter()
            .any(|event| {
                let soroban_sdk::xdr::ContractEventBody::V0(body) = &event.body;
                body.topics == expected
            })
    }

    /// The `net_pnl` (token-dec) of the last vault fill receipt, a
    /// `deposit_fill` or `redeem_fill`, in the last invocation: the mark the
    /// market handed the vault to price the shares.
    pub fn last_vault_fill_net_pnl(&self) -> i128 {
        use soroban_sdk::xdr::{ContractEventBody, ScSymbol, ScVal};
        use soroban_sdk::TryFromVal;
        let fill_topics = ["deposit_fill", "redeem_fill"]
            .map(|name| ScVal::Symbol(ScSymbol(name.try_into().unwrap())));
        let events = self
            .env
            .events()
            .all()
            .filter_by_contract(&self.market.address);
        let body = events
            .events()
            .iter()
            .rev()
            .find_map(|event| {
                let ContractEventBody::V0(body) = &event.body;
                body.topics
                    .first()
                    .filter(|topic| fill_topics.contains(topic))
                    .map(|_| body.clone())
            })
            .expect("the last invocation published a vault fill receipt");
        let data = Val::try_from_val(&self.env, &body.data).unwrap();
        let data = soroban_sdk::Map::<Symbol, Val>::try_from_val(&self.env, &data).unwrap();
        i128::try_from_val(
            &self.env,
            &data.get(Symbol::new(&self.env, "net_pnl")).unwrap(),
        )
        .unwrap()
    }

    // ---- views ----

    /// The netted position for `(user, is_long)` (zeroed if none is open).
    pub fn position(&self, user: &Address, is_long: bool) -> Position {
        self.market.get_position(user, &is_long)
    }

    /// Asserts the borrowing checkpoint books against the positions of
    /// `traders`, which must include every address with an open position.
    ///
    /// Per side, the stored open interest must equal the summed `notional`,
    /// the borrowing weight the summed `notional * borrowing_idx`, and no
    /// position's checkpoint may sit above the side's borrowing index.
    pub fn assert_borrowing_books<'b>(&self, traders: impl IntoIterator<Item = &'b Address>) {
        let traders: std::vec::Vec<&Address> = traders.into_iter().collect();
        let data = self.market.get_market_data();
        for is_long in [true, false] {
            let index = data.borrowing_idx.get(is_long);
            let (mut open_interest, mut weight) = (0_i128, 0_i128);
            for trader in &traders {
                let position = self.position(trader, is_long);
                assert!(
                    position.borrowing_idx <= index,
                    "a checkpoint sits above the borrowing index (is_long {is_long})"
                );
                open_interest += position.notional;
                weight += position.notional * position.borrowing_idx;
            }
            assert_eq!(
                data.notional.get(is_long),
                open_interest,
                "open interest is the summed notional (is_long {is_long})"
            );
            assert_eq!(
                data.borrowing_weight.get(is_long),
                weight,
                "borrowing weight is the summed notional * checkpoint (is_long {is_long})"
            );
        }
    }

    /// A comfortable default order expiration: the current sequence plus roughly
    /// six days of ledgers, well inside the ledger's max entry TTL.
    pub fn far_expiration(&self) -> u32 {
        self.env.ledger().sequence() + DEFAULT_EXPIRATION_LEDGERS
    }

    // ---- time ----

    /// Advance the ledger by `seconds`, moving the sequence forward one ledger per
    /// five seconds so both timestamp accruals and sequence-bounded order
    /// expirations progress together.
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
}

/// The `u32` discriminant of a contract error, or `u32::MAX` for any other failure.
fn contract_error_code(error: Error) -> u32 {
    if error.is_type(ScErrorType::Contract) {
        error.get_code()
    } else {
        u32::MAX
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::{BTC_PRICE, SCALAR_7};

    /// End-to-end smoke test: deploy the stack, seed liquidity, and open a lone
    /// long. The test then asserts the netted position
    /// against independently derived values.
    ///
    /// Config anchors: BTC $100k (ask == mid == $100k), 7-dec token. Opening
    /// $1,000 notional on $200 margin (5x) into an empty book worsens the
    /// whole size, so `fee_dom` (0.5%) applies, plus the size-quadratic impact
    /// fee (notional^2 / the $10M impact_scalar).
    #[test]
    fn open_position() {
        let mut fixture = TestFixture::create();

        // $5,000 vault liquidity: capacity = 80% * 5,000 / 2 = $2,000 >= the $1,000 mark.
        fixture.seed_vault(5_000 * SCALAR_7);
        let user = fixture.new_user(1_000 * SCALAR_7);

        let notional = 1_000 * SCALAR_7; // $1,000
        let margin = 200 * SCALAR_7; // $200 -> 5x
        let price = fixture.price(BTC_PRICE);

        let payout = fixture.open(&user, true, notional, margin, &price);

        // tokens = floor(notional * 1e18 / ask) = floor(1e10 * 1e18 / 1e23) = 1e5.
        // The 18-dec feed makes one base unit worth a cent at $100k, so $1,000
        // buys 1e5 of them.
        let expected_tokens = 100_000_i128;
        // base = ceil(0.5% * 1e10) = 5e7. impact = ceil((1e10)^2 / 1e14) = 1e6.
        let base_fee = 50_000_000_i128;
        let impact_fee = 1_000_000_i128;
        let expected_margin = margin - base_fee - impact_fee;
        // keeper payout = floor(10% * (base + impact)) plus the flat exec_fee.
        let exec_fee = 100_000_i128;
        let expected_payout = (base_fee + impact_fee) / 10 + exec_fee;

        let position = fixture.position(&user, true);
        assert_eq!(position.notional, notional, "position notional");
        assert_eq!(
            position.tokens, expected_tokens,
            "position tokens (base-dec)"
        );
        assert_eq!(position.margin, expected_margin, "margin net of fees");
        assert_eq!(payout, expected_payout, "keeper payout (fee cut)");

        let market = fixture.market.get_market_data();
        assert_eq!(market.notional.long, notional, "long open interest");
        assert_eq!(market.notional.short, 0, "short open interest");

        // The vault banks the trade-fee remainder after the keeper (10%) and
        // treasury (5%) cuts: 85% of the 5.1 fee.
        let vault_fee_leg = (base_fee + impact_fee) * 85 / 100;
        assert_eq!(
            fixture.vault.total_assets(),
            5_000 * SCALAR_7 + vault_fee_leg,
            "vault banks the fee remainder on open",
        );
    }
}
