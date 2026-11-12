//! Unwrapped: borrow stablecoins against native BTC. No bridge, no wrapped token.
//!
//! A vault is an NFT (NEP-171) that controls a Bitcoin address through NEAR
//! Chain Signatures. The BTC never leaves Bitcoin. Three rules make that BTC
//! usable as collateral:
//!
//! 1. **Only this contract can sign for a vault.** The MPC network derives the
//!    vault key from `(this contract's account, "vault-<id>")`, so no user and
//!    no other contract can ever request a signature for it.
//! 2. **The contract signs only transactions it builds itself.** A withdrawal
//!    is described by its inputs and outputs; the contract computes the BIP143
//!    digests on chain and records every outpoint it authorises in a permanent
//!    *signature ledger*.
//! 3. **Collateral = proven deposits − the signature ledger.** A deposit counts
//!    only after an SPV proof against the on-chain Bitcoin light client, and
//!    only if no signature spending that outpoint was ever issued. A signature
//!    the owner obtained earlier and kept in a drawer therefore cannot drain a
//!    vault after it has been pledged, liquidated or sold.
//!
//! On top of that sits a small lending pool (see `pool.rs`): lenders supply
//! USDC, vault owners borrow against proven BTC, and an unhealthy vault is
//! liquidated by transferring the NFT, which hands over the Bitcoin key.

pub mod btc;
mod nft;
mod oracle;
mod pool;
mod vault;

use near_contract_standards::non_fungible_token::NonFungibleToken;
use near_sdk::json_types::{U128, U64};
use near_sdk::serde_json::{json, Value};
use near_sdk::store::{LookupMap, LookupSet};
use near_sdk::{env, near, require, AccountId, BorshStorageKey, NearToken, PanicOnDefault};

pub use oracle::{Price, Report, ReportView};
pub use pool::PoolView;
pub use vault::{BtcInput, BtcOutput, SignedWithdrawal, UtxoView, VaultView};

/// Fixed-point scale of the borrow index.
pub const INDEX_ONE: u128 = 1_000_000_000_000_000_000;
pub const BPS: u128 = 10_000;
pub const SATS_PER_BTC: u128 = 100_000_000;
pub const SECONDS_PER_YEAR: u128 = 31_536_000;

#[near(serializers = [borsh])]
#[derive(BorshStorageKey)]
enum StorageKey {
    NftOwners,
    NftMetadata,
    NftEnumeration,
    NftApprovals,
    Vaults,
    Collateral,
    SignatureLedger,
    Shares,
    Claimable,
}

/// Everything an operator can tune. Stored on chain and readable through
/// `get_config`, so the trust assumptions are visible.
#[near(serializers = [borsh, json])]
#[derive(Clone, Debug)]
pub struct Config {
    /// Chain Signatures contract: `v1.signer` on mainnet,
    /// `v1.signer-prod.testnet` on testnet.
    pub mpc_signer: AccountId,
    /// MPC domain of the secp256k1 key (0 on both networks today).
    pub mpc_domain_id: u64,
    /// Bitcoin light client on NEAR used to verify deposit proofs.
    pub btc_light_client: AccountId,
    /// Bech32 prefix of vault addresses: `bc` (mainnet) or `tb` (testnet).
    pub btc_hrp: String,
    /// Bitcoin confirmations a deposit needs before it counts.
    pub min_confirmations: u64,
    /// NEP-141 stablecoin that is lent out (6 decimals expected).
    pub usdc: AccountId,
    /// Independent price reporters (see `oracle.rs`).
    pub oracle_ids: Vec<AccountId>,
    /// Fresh reports needed before a price exists at all.
    pub min_oracle_reports: u32,
    /// Largest allowed gap between the highest and lowest fresh report,
    /// relative to their median. Wider disagreement means no price.
    pub max_oracle_spread_bps: u32,
    /// A report older than this does not count.
    pub max_price_age_sec: u64,
    /// Highest loan-to-value at which new debt or a collateral withdrawal is allowed.
    pub max_ltv_bps: u32,
    /// Loan-to-value above which a vault can be liquidated.
    pub liq_threshold_bps: u32,
    /// Discount to the oracle price at which a liquidator buys the vault.
    pub liq_bonus_bps: u32,
    /// Borrow APR at 0% utilisation.
    pub base_rate_bps: u32,
    /// Additional borrow APR at 100% utilisation (linear in between).
    pub slope_rate_bps: u32,
    /// NEAR paid on mint; covers the vault's storage.
    pub mint_fee: NearToken,
    /// NEAR paid per input of a withdrawal; covers its permanent entry in
    /// the signature ledger, so the ledger cannot be used to drain the
    /// contract's storage balance.
    pub ledger_fee: NearToken,
}

impl Config {
    fn assert_valid(&self) {
        require!(
            self.btc_hrp == "bc" || self.btc_hrp == "tb",
            "btc_hrp must be bc or tb"
        );
        require!(
            self.min_confirmations >= 1,
            "min_confirmations must be at least 1"
        );
        require!(
            self.max_ltv_bps > 0
                && self.max_ltv_bps < self.liq_threshold_bps
                && (self.liq_threshold_bps as u128) < BPS,
            "need 0 < max_ltv < liq_threshold < 100%"
        );
        // A liquidator pays (1 - bonus) of the collateral value. At the
        // threshold that must still cover the debt, or liquidations start
        // out with bad debt.
        require!(
            self.liq_threshold_bps as u128 + self.liq_bonus_bps as u128 <= BPS,
            "liq_threshold + liq_bonus must not exceed 100%"
        );
        require!(
            !self.oracle_ids.is_empty(),
            "at least one oracle is required"
        );
        let mut ids = self.oracle_ids.clone();
        ids.sort();
        ids.dedup();
        require!(ids.len() == self.oracle_ids.len(), "duplicate oracle");
        require!(
            self.min_oracle_reports >= 1 && self.min_oracle_reports as usize <= self.oracle_ids.len(),
            "min_oracle_reports must be between 1 and the number of oracles"
        );
        require!(
            self.max_oracle_spread_bps > 0 && self.max_oracle_spread_bps as u128 <= BPS,
            "max_oracle_spread_bps must be between 1 and 10000"
        );
        require!(
            self.max_price_age_sec > 0,
            "max_price_age_sec must be positive"
        );
    }
}

/// One proven, unspent output sitting on the vault's Bitcoin address.
#[near(serializers = [borsh])]
#[derive(Clone, Debug)]
pub struct Utxo {
    pub txid: [u8; 32],
    pub vout: u32,
    pub value: u64,
}

#[near(serializers = [borsh])]
#[derive(Clone, Debug)]
pub struct Vault {
    /// Compressed secp256k1 key the MPC network derived for this vault.
    pub pubkey: Vec<u8>,
    /// Proven deposits that no signature has ever been issued for. This is
    /// the collateral.
    pub utxos: Vec<Utxo>,
    /// Debt in index-scaled units; `debt = debt_scaled * borrow_index`.
    pub debt_scaled: u128,
}

/// A proven deposit currently counted as collateral.
#[near(serializers = [borsh])]
#[derive(Clone, Debug, PartialEq)]
pub struct CollateralEntry {
    pub vault_id: u64,
    pub value: u64,
}

#[near(serializers = [borsh])]
#[derive(Clone, Debug)]
pub struct Pool {
    /// USDC held by the contract on behalf of lenders.
    pub cash: u128,
    pub total_debt_scaled: u128,
    pub borrow_index: u128,
    pub last_accrual_ns: u64,
    pub total_shares: u128,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Unwrapped {
    owner_id: AccountId,
    cfg: Config,
    tokens: NonFungibleToken,
    vaults: LookupMap<u64, Vault>,
    next_vault_id: u64,
    /// Proven deposits by outpoint. An outpoint pays exactly one script, so
    /// it can belong to at most one vault.
    collateral: LookupMap<[u8; 36], CollateralEntry>,
    /// The signature ledger: every `(vault, outpoint)` the contract has ever
    /// requested a signature for. Entries are never removed.
    ///
    /// It is keyed per vault because a signature only matters for outputs
    /// locked to that vault's key; a global key would let one vault owner
    /// "burn" a stranger's unproven deposit by naming it as an input.
    signature_ledger: LookupSet<(u64, [u8; 36])>,
    pool: Pool,
    shares: LookupMap<AccountId, u128>,
    /// USDC owed to accounts (liquidation surplus, failed payouts). Pull-based
    /// so a liquidation can never be blocked by the borrower.
    claimable: LookupMap<AccountId, u128>,
    /// Latest report of each oracle; the price is aggregated on read.
    reports: Vec<oracle::Report>,
}

#[near]
impl Unwrapped {
    #[init]
    pub fn new(owner_id: AccountId, config: Config) -> Self {
        config.assert_valid();
        Self {
            tokens: NonFungibleToken::new(
                StorageKey::NftOwners,
                owner_id.clone(),
                Some(StorageKey::NftMetadata),
                Some(StorageKey::NftEnumeration),
                Some(StorageKey::NftApprovals),
            ),
            owner_id,
            cfg: config,
            vaults: LookupMap::new(StorageKey::Vaults),
            next_vault_id: 1,
            collateral: LookupMap::new(StorageKey::Collateral),
            signature_ledger: LookupSet::new(StorageKey::SignatureLedger),
            pool: Pool {
                cash: 0,
                total_debt_scaled: 0,
                borrow_index: INDEX_ONE,
                last_accrual_ns: env::block_timestamp(),
                total_shares: 0,
            },
            shares: LookupMap::new(StorageKey::Shares),
            claimable: LookupMap::new(StorageKey::Claimable),
            reports: Vec::new(),
        }
    }

    pub fn get_config(&self) -> Config {
        self.cfg.clone()
    }

    pub fn get_owner(&self) -> AccountId {
        self.owner_id.clone()
    }

    /// Risk and oracle parameters can be changed by the operator. The MPC
    /// signer, the light client, the Bitcoin network and the lent token are
    /// fixed at deployment: changing any of them could strand or misprice
    /// existing vaults.
    pub fn update_config(&mut self, config: Config) {
        self.assert_owner();
        require!(
            config.mpc_signer == self.cfg.mpc_signer
                && config.mpc_domain_id == self.cfg.mpc_domain_id
                && config.btc_light_client == self.cfg.btc_light_client
                && config.btc_hrp == self.cfg.btc_hrp
                && config.usdc == self.cfg.usdc,
            "signer, light client, network and token are immutable"
        );
        config.assert_valid();
        self.accrue();
        self.cfg = config;
        emit("config_updated", json!({}));
    }
}

impl Unwrapped {
    fn assert_owner(&self) {
        require!(
            env::predecessor_account_id() == self.owner_id,
            "only the operator can do this"
        );
    }
}

/// Value of `sats` in micro-USDC at `usdc_per_btc`, rounded down.
pub(crate) fn sats_value(sats: u64, usdc_per_btc: u128) -> u128 {
    sats as u128 * usdc_per_btc / SATS_PER_BTC
}

/// NEP-297 event log.
pub(crate) fn emit(event: &str, data: Value) {
    env::log_str(&format!(
        "EVENT_JSON:{}",
        json!({ "standard": "unwrapped", "version": "1.0.0", "event": event, "data": [data] })
    ));
}
