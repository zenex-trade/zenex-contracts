//! Recorded testnet economics with explicit capacity-only test overrides.
//!
//! All economic settings are copied from the deployment record
//! `zenex-ops/deployments/testnet/2026-09-05-testnet-v2`.
//! Integration reports use the real verifier with public test signers. Provider
//! and network-specific token identities remain deployment evidence.

use crate::test_fixture::StackDials;
use serde::{de::Error, Deserialize, Deserializer};
use soroban_sdk::{BytesN, Env};
use std::str::FromStr;

pub const TESTNET_PROFILE_JSON: &str = include_str!("../fixtures/testnet-2026-09-05.json");

/// Financial integers are strings so JavaScript deployment tools preserve
/// all digits. Reject non-canonical values rather than quietly normalizing.
fn decimal<'de, D: Deserializer<'de>, T: FromStr>(deserializer: D) -> Result<T, D::Error> {
    let value = String::deserialize(deserializer)?;
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return Err(D::Error::custom(
            "expected a canonical unsigned decimal string",
        ));
    }
    value
        .parse()
        .map_err(|_| D::Error::custom("integer out of range"))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TestnetProfile {
    pub schema_version: u32,
    pub profile_id: String,
    pub capacity_multiplier: u32,
    pub provenance: Provenance,
    pub settlement: Settlement,
    pub feed: Feed,
    pub market: MarketParameters,
    pub oracle: OracleParameters,
    pub treasury: TreasuryParameters,
    pub vault: VaultParameters,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Provenance {
    pub baseline: String,
    pub branch_commit: String,
    pub changes: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settlement {
    pub symbol: String,
    pub decimals: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Feed {
    pub pair_id: String,
    pub base: String,
    pub quote: String,
    pub id: String,
    pub report_version: u32,
    pub decimals: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OracleParameters {
    #[serde(deserialize_with = "decimal")]
    pub trade_staleness: u64,
    #[serde(deserialize_with = "decimal")]
    pub close_staleness: u64,
    #[serde(deserialize_with = "decimal")]
    pub spread_reduction_factor: i128,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreasuryParameters {
    #[serde(deserialize_with = "decimal")]
    pub rate: i128,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VaultParameters {
    pub name: String,
    pub symbol: String,
    pub decimals_offset: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MarketParameters {
    #[serde(deserialize_with = "decimal")]
    pub keeper_rate: i128,
    #[serde(deserialize_with = "decimal")]
    pub min_position_notional: i128,
    #[serde(deserialize_with = "decimal")]
    pub max_position_notional: i128,
    #[serde(deserialize_with = "decimal")]
    pub max_open_interest: i128,
    #[serde(deserialize_with = "decimal")]
    pub min_order_notional: i128,
    #[serde(deserialize_with = "decimal")]
    pub min_order_margin: i128,
    #[serde(deserialize_with = "decimal")]
    pub exec_fee: i128,
    #[serde(deserialize_with = "decimal")]
    pub fee_dom: i128,
    #[serde(deserialize_with = "decimal")]
    pub fee_non_dom: i128,
    #[serde(deserialize_with = "decimal")]
    pub impact_scalar: i128,
    #[serde(deserialize_with = "decimal")]
    pub max_util_open: i128,
    #[serde(deserialize_with = "decimal")]
    pub max_util_withdraw: i128,
    #[serde(deserialize_with = "decimal")]
    pub init_margin: i128,
    #[serde(deserialize_with = "decimal")]
    pub maintenance_margin: i128,
    #[serde(deserialize_with = "decimal")]
    pub liq_fee: i128,
    #[serde(deserialize_with = "decimal")]
    pub notional_lock: u64,
    #[serde(deserialize_with = "decimal")]
    pub target_util: i128,
    #[serde(deserialize_with = "decimal")]
    pub borrow_rate: i128,
    #[serde(deserialize_with = "decimal")]
    pub increased_borrow_rate: i128,
    #[serde(deserialize_with = "decimal")]
    pub funding_increase: i128,
    #[serde(deserialize_with = "decimal")]
    pub funding_decrease: i128,
    #[serde(deserialize_with = "decimal")]
    pub threshold_stable_funding: i128,
    #[serde(deserialize_with = "decimal")]
    pub threshold_decrease_funding: i128,
    #[serde(deserialize_with = "decimal")]
    pub funding_min: i128,
    #[serde(deserialize_with = "decimal")]
    pub funding_max: i128,
    #[serde(deserialize_with = "decimal")]
    pub adl_max_pnl: i128,
    #[serde(deserialize_with = "decimal")]
    pub adl_clear_target: i128,
    #[serde(deserialize_with = "decimal")]
    pub max_pnl_trader: i128,
    #[serde(deserialize_with = "decimal")]
    pub max_pnl_withdraw: i128,
    #[serde(deserialize_with = "decimal")]
    pub redeem_lock: u64,
    #[serde(deserialize_with = "decimal")]
    pub deposit_fee: i128,
    #[serde(deserialize_with = "decimal")]
    pub redeem_fee: i128,
    #[serde(deserialize_with = "decimal")]
    pub min_deposit: i128,
    #[serde(deserialize_with = "decimal")]
    pub max_vault_balance: i128,
}

impl MarketParameters {
    pub fn config(&self) -> market::Config {
        market::Config {
            keeper_rate: self.keeper_rate,
            min_position_notional: self.min_position_notional,
            max_position_notional: self.max_position_notional,
            max_open_interest: self.max_open_interest,
            min_order_notional: self.min_order_notional,
            min_order_margin: self.min_order_margin,
            exec_fee: self.exec_fee,
            fee_dom: self.fee_dom,
            fee_non_dom: self.fee_non_dom,
            impact_scalar: self.impact_scalar,
            max_util_open: self.max_util_open,
            max_util_withdraw: self.max_util_withdraw,
            init_margin: self.init_margin,
            maintenance_margin: self.maintenance_margin,
            liq_fee: self.liq_fee,
            notional_lock: self.notional_lock,
            target_util: self.target_util,
            borrow_rate: self.borrow_rate,
            increased_borrow_rate: self.increased_borrow_rate,
            funding_increase: self.funding_increase,
            funding_decrease: self.funding_decrease,
            threshold_stable_funding: self.threshold_stable_funding,
            threshold_decrease_funding: self.threshold_decrease_funding,
            funding_min: self.funding_min,
            funding_max: self.funding_max,
            adl_max_pnl: self.adl_max_pnl,
            adl_clear_target: self.adl_clear_target,
            max_pnl_trader: self.max_pnl_trader,
            max_pnl_withdraw: self.max_pnl_withdraw,
            redeem_lock: self.redeem_lock,
            deposit_fee: self.deposit_fee,
            redeem_fee: self.redeem_fee,
            min_deposit: self.min_deposit,
            max_vault_balance: self.max_vault_balance,
        }
    }
}

impl TestnetProfile {
    pub fn load() -> Self {
        let inputs: Self =
            serde_json::from_str(TESTNET_PROFILE_JSON).expect("valid testnet profile JSON");
        assert_eq!(inputs.schema_version, 1);
        assert_eq!(inputs.settlement.decimals, 7, "fixture SAC is 7-decimal");
        assert_eq!(inputs.feed.decimals, 18, "V3 price precision");
        assert_eq!(inputs.feed.report_version, 3);
        // The vault fixes its offset at 6; the record must agree.
        assert_eq!(inputs.vault.decimals_offset, 6);
        assert!(inputs.market.notional_lock >= inputs.oracle.trade_staleness);
        inputs
    }

    /// Expand test capacity only. The recorded margin ladder, rates, ADL
    /// thresholds and every other economic setting remain unchanged.
    pub fn test_config(&self) -> market::Config {
        let mut config = self.market.config();
        let multiplier = i128::from(self.capacity_multiplier);
        assert!(multiplier > 0);
        config.max_position_notional = config
            .max_position_notional
            .checked_mul(multiplier)
            .unwrap();
        config.max_open_interest = config.max_open_interest.checked_mul(multiplier).unwrap();
        config.max_vault_balance = config.max_vault_balance.checked_mul(multiplier).unwrap();
        config
    }

    pub fn dials(&self) -> StackDials {
        StackDials {
            trade_staleness: self.oracle.trade_staleness,
            close_staleness: self.oracle.close_staleness,
            spread_reduction: self.oracle.spread_reduction_factor,
            treasury_rate: self.treasury.rate,
        }
    }

    pub fn feed_id(&self, env: &Env) -> BytesN<32> {
        assert_eq!(self.feed.id.len(), 64);
        assert!(self.feed.id.starts_with("0003"));
        let mut bytes = [0u8; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&self.feed.id[index * 2..index * 2 + 2], 16)
                .expect("hex feed id");
        }
        BytesN::from_array(env, &bytes)
    }
}
