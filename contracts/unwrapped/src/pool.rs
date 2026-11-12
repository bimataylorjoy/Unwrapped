//! A single-asset lending pool: lenders supply USDC and receive shares,
//! vault owners borrow against proven BTC, liquidators buy unhealthy vaults.
//!
//! USDC enters only through `ft_transfer_call` with a JSON `msg`:
//! `{"action":"supply"}`, `{"action":"repay","vault_id":1}` or
//! `{"action":"liquidate","vault_id":1}`. Whatever is not used is refunded
//! by the token contract.

use crate::vault::collateral_sats;
use crate::*;
use near_contract_standards::fungible_token::receiver::FungibleTokenReceiver;
use near_sdk::serde::Deserialize;
use near_sdk::{assert_one_yocto, Gas, Promise, PromiseOrValue};

const GAS_FT_TRANSFER: Gas = Gas::from_tgas(15);
const GAS_ON_PAYOUT: Gas = Gas::from_tgas(10);

/// Virtual shares and assets make the first-depositor share-price
/// manipulation unprofitable (same idea as ERC-4626's decimals offset).
const VIRTUAL_SHARES: u128 = 1_000;
const VIRTUAL_ASSETS: u128 = 1;

#[derive(Deserialize)]
#[serde(crate = "near_sdk::serde", tag = "action", rename_all = "snake_case")]
enum PoolAction {
    Supply,
    Repay { vault_id: u64 },
    Liquidate { vault_id: u64 },
}

#[near(serializers = [json])]
#[derive(Clone, Debug)]
pub struct PoolView {
    /// Idle USDC available to borrow or withdraw.
    pub cash: U128,
    /// Outstanding loans including accrued interest.
    pub total_debt: U128,
    pub total_assets: U128,
    pub total_shares: U128,
    pub utilization_bps: u32,
    pub borrow_apr_bps: u32,
    pub supply_apr_bps: u32,
    pub borrow_index: U128,
}

/// What a liquidator pays for a vault right now, and where it goes.
#[near(serializers = [json])]
#[derive(Clone, Debug)]
pub struct LiquidationQuote {
    pub liquidatable: bool,
    pub collateral_sats: U64,
    pub collateral_value: U128,
    pub debt: U128,
    /// USDC the liquidator must send.
    pub cost: U128,
    pub to_pool: U128,
    /// Surplus credited to the borrower.
    pub to_borrower: U128,
    /// Debt the pool writes off if the vault is under water.
    pub bad_debt: U128,
}

fn mul_div_ceil(a: u128, b: u128, c: u128) -> u128 {
    (a * b).div_ceil(c)
}

impl Pool {
    pub fn debt_from_scaled(&self, scaled: u128) -> u128 {
        mul_div_ceil(scaled, self.borrow_index, INDEX_ONE)
    }

    pub fn total_debt(&self) -> u128 {
        self.debt_from_scaled(self.total_debt_scaled)
    }

    pub fn total_assets(&self) -> u128 {
        self.cash + self.total_debt()
    }

    pub fn utilization_bps(&self) -> u128 {
        let assets = self.total_assets();
        if assets == 0 {
            0
        } else {
            (self.total_debt() * BPS / assets).min(BPS)
        }
    }

    fn shares_for(&self, assets: u128) -> u128 {
        assets * (self.total_shares + VIRTUAL_SHARES) / (self.total_assets() + VIRTUAL_ASSETS)
    }

    fn assets_for(&self, shares: u128) -> u128 {
        shares * (self.total_assets() + VIRTUAL_ASSETS) / (self.total_shares + VIRTUAL_SHARES)
    }
}

#[near]
impl Unwrapped {
    /// Burns `shares` and pays out their USDC value. Requires 1 yoctoNEAR.
    #[payable]
    pub fn withdraw(&mut self, shares: U128) -> Promise {
        assert_one_yocto();
        self.accrue();
        let account_id = env::predecessor_account_id();
        let held = self.shares.get(&account_id).copied().unwrap_or(0);
        require!(shares.0 > 0 && shares.0 <= held, "not enough shares");
        let amount = self.pool.assets_for(shares.0);
        require!(amount > 0, "amount rounds to zero");
        require!(
            amount <= self.pool.cash,
            "not enough idle liquidity; withdraw less or wait for repayments"
        );

        if held == shares.0 {
            self.shares.remove(&account_id);
        } else {
            self.shares.insert(account_id.clone(), held - shares.0);
        }
        self.pool.total_shares -= shares.0;
        self.pool.cash -= amount;
        emit(
            "withdraw",
            json!({ "account_id": account_id, "shares": shares, "amount": U128(amount) }),
        );
        self.pay(&account_id, amount)
    }

    /// Borrows USDC against the vault's proven BTC. Requires 1 yoctoNEAR.
    ///
    /// From this moment until the debt is repaid the vault cannot be
    /// transferred and can only sign withdrawals that keep it within
    /// `max_ltv`.
    #[payable]
    pub fn borrow(&mut self, vault_id: u64, amount: U128) -> Promise {
        assert_one_yocto();
        let borrower = self.vault_owner(vault_id);
        require!(
            env::predecessor_account_id() == borrower,
            "only the vault owner can borrow"
        );
        require!(amount.0 > 0, "amount must be positive");
        self.accrue();
        let price = self.fresh_price();

        let mut vault = self.vaults.get(&vault_id).unwrap().clone();
        let new_debt = self.debt_of(&vault) + amount.0;
        let value = sats_value(collateral_sats(&vault), price);
        require!(
            new_debt * BPS <= value * self.cfg.max_ltv_bps as u128,
            "loan would exceed the maximum loan-to-value"
        );
        require!(
            amount.0 <= self.pool.cash,
            "not enough liquidity in the pool"
        );

        let scaled = mul_div_ceil(amount.0, INDEX_ONE, self.pool.borrow_index);
        vault.debt_scaled += scaled;
        self.pool.total_debt_scaled += scaled;
        self.pool.cash -= amount.0;
        self.vaults.insert(vault_id, vault);
        emit(
            "borrow",
            json!({ "vault_id": vault_id, "borrower": borrower, "amount": amount }),
        );
        self.pay(&borrower, amount.0)
    }

    /// Pays out USDC credited to the caller (liquidation surplus, or a
    /// payout that could not be delivered). Requires 1 yoctoNEAR.
    #[payable]
    pub fn claim(&mut self) -> Promise {
        assert_one_yocto();
        let account_id = env::predecessor_account_id();
        let amount = self.claimable.remove(&account_id).unwrap_or(0);
        require!(amount > 0, "nothing to claim");
        self.pay(&account_id, amount)
    }

    #[private]
    pub fn on_payout(&mut self, account_id: AccountId, amount: U128) -> bool {
        if env::promise_result_checked(0, 64).is_ok() {
            return true;
        }
        // Typically the receiver has no storage on the token contract. The
        // money stays here, earmarked, until they `claim` it.
        self.credit(&account_id, amount.0);
        emit(
            "payout_deferred",
            json!({ "account_id": account_id, "amount": amount }),
        );
        false
    }

    pub fn get_pool(&self) -> PoolView {
        let pool = self.accrued_pool();
        let utilization = pool.utilization_bps();
        let borrow_apr = self.borrow_rate_bps(&pool);
        PoolView {
            cash: U128(pool.cash),
            total_debt: U128(pool.total_debt()),
            total_assets: U128(pool.total_assets()),
            total_shares: U128(pool.total_shares),
            utilization_bps: utilization as u32,
            borrow_apr_bps: borrow_apr as u32,
            supply_apr_bps: (borrow_apr * utilization / BPS) as u32,
            borrow_index: U128(pool.borrow_index),
        }
    }

    pub fn shares_of(&self, account_id: AccountId) -> U128 {
        U128(self.shares.get(&account_id).copied().unwrap_or(0))
    }

    /// USDC value of the account's shares right now.
    pub fn supplied_of(&self, account_id: AccountId) -> U128 {
        U128(
            self.accrued_pool()
                .assets_for(self.shares.get(&account_id).copied().unwrap_or(0)),
        )
    }

    pub fn claimable_of(&self, account_id: AccountId) -> U128 {
        U128(self.claimable.get(&account_id).copied().unwrap_or(0))
    }

    /// `None` if the vault does not exist or the price is stale.
    pub fn liquidation_quote(&self, vault_id: u64) -> Option<LiquidationQuote> {
        let vault = self.vaults.get(&vault_id)?;
        let price = self.try_fresh_price()?;
        let debt = self.accrued_pool().debt_from_scaled(vault.debt_scaled);
        Some(self.quote(collateral_sats(vault), debt, price))
    }
}

#[near]
impl FungibleTokenReceiver for Unwrapped {
    fn ft_on_transfer(
        &mut self,
        sender_id: AccountId,
        amount: U128,
        msg: String,
    ) -> PromiseOrValue<U128> {
        require!(
            env::predecessor_account_id() == self.cfg.usdc,
            "only the configured stablecoin is accepted"
        );
        let action: PoolAction = near_sdk::serde_json::from_str(&msg).unwrap_or_else(|_| {
            env::panic_str(r#"msg must be {"action":"supply"}, {"action":"repay","vault_id":N} or {"action":"liquidate","vault_id":N}"#)
        });
        self.accrue();
        let unused = match action {
            PoolAction::Supply => self.supply(&sender_id, amount.0),
            PoolAction::Repay { vault_id } => self.repay(vault_id, &sender_id, amount.0),
            PoolAction::Liquidate { vault_id } => self.liquidate(vault_id, &sender_id, amount.0),
        };
        PromiseOrValue::Value(U128(unused))
    }
}

impl Unwrapped {
    pub(crate) fn borrow_rate_bps(&self, pool: &Pool) -> u128 {
        self.cfg.base_rate_bps as u128
            + self.cfg.slope_rate_bps as u128 * pool.utilization_bps() / BPS
    }

    /// The pool as it would be after charging interest up to this block.
    pub(crate) fn accrued_pool(&self) -> Pool {
        let mut pool = self.pool.clone();
        let elapsed_sec =
            (env::block_timestamp().saturating_sub(pool.last_accrual_ns) / 1_000_000_000) as u128;
        if elapsed_sec == 0 {
            return pool;
        }
        if pool.total_debt_scaled > 0 {
            let rate_bps = self.borrow_rate_bps(&pool);
            pool.borrow_index +=
                pool.borrow_index * rate_bps * elapsed_sec / (BPS * SECONDS_PER_YEAR);
        }
        pool.last_accrual_ns += (elapsed_sec as u64) * 1_000_000_000;
        pool
    }

    pub(crate) fn accrue(&mut self) {
        self.pool = self.accrued_pool();
    }

    /// Current debt of a vault. Call `accrue` first.
    pub(crate) fn debt_of(&self, vault: &Vault) -> u128 {
        self.pool.debt_from_scaled(vault.debt_scaled)
    }

    fn credit(&mut self, account_id: &AccountId, amount: u128) {
        if amount > 0 {
            let current = self.claimable.get(account_id).copied().unwrap_or(0);
            self.claimable.insert(account_id.clone(), current + amount);
        }
    }

    fn pay(&self, account_id: &AccountId, amount: u128) -> Promise {
        Promise::new(self.cfg.usdc.clone())
            .function_call(
                "ft_transfer".to_string(),
                json!({ "receiver_id": account_id, "amount": U128(amount) })
                    .to_string()
                    .into_bytes(),
                NearToken::from_yoctonear(1),
                GAS_FT_TRANSFER,
            )
            .then(
                Promise::new(env::current_account_id()).function_call(
                    "on_payout".to_string(),
                    json!({ "account_id": account_id, "amount": U128(amount) })
                        .to_string()
                        .into_bytes(),
                    NearToken::from_yoctonear(0),
                    GAS_ON_PAYOUT,
                ),
            )
    }

    fn supply(&mut self, account_id: &AccountId, amount: u128) -> u128 {
        require!(amount > 0, "amount must be positive");
        let shares = self.pool.shares_for(amount);
        require!(shares > 0, "amount is too small");
        let held = self.shares.get(account_id).copied().unwrap_or(0);
        self.shares.insert(account_id.clone(), held + shares);
        self.pool.total_shares += shares;
        self.pool.cash += amount;
        emit(
            "supply",
            json!({ "account_id": account_id, "amount": U128(amount), "shares": U128(shares) }),
        );
        0
    }

    /// Anyone can repay any vault. Returns the part of `amount` not needed.
    fn repay(&mut self, vault_id: u64, payer: &AccountId, amount: u128) -> u128 {
        let mut vault = self
            .vaults
            .get(&vault_id)
            .unwrap_or_else(|| env::panic_str("no such vault"))
            .clone();
        let debt = self.debt_of(&vault);
        require!(debt > 0, "this vault has no debt");

        let (paid, scaled) = if amount >= debt {
            (debt, vault.debt_scaled)
        } else {
            // Round the reduction down so a partial repayment never cancels
            // more debt than was paid.
            (
                amount,
                (amount * INDEX_ONE / self.pool.borrow_index).min(vault.debt_scaled),
            )
        };
        vault.debt_scaled -= scaled;
        self.pool.total_debt_scaled -= scaled;
        self.pool.cash += paid;
        let remaining = self.debt_of(&vault);
        self.vaults.insert(vault_id, vault);
        emit(
            "repay",
            json!({ "vault_id": vault_id, "payer": payer, "amount": U128(paid), "remaining_debt": U128(remaining) }),
        );
        amount - paid
    }

    fn quote(&self, sats: u64, debt: u128, price: u128) -> LiquidationQuote {
        let value = sats_value(sats, price);
        let cost = value * (BPS - self.cfg.liq_bonus_bps as u128) / BPS;
        let to_pool = cost.min(debt);
        LiquidationQuote {
            liquidatable: debt > 0 && debt * BPS > value * self.cfg.liq_threshold_bps as u128,
            collateral_sats: U64(sats),
            collateral_value: U128(value),
            debt: U128(debt),
            cost: U128(cost),
            to_pool: U128(to_pool),
            to_borrower: U128(cost - to_pool),
            bad_debt: U128(debt - to_pool),
        }
    }

    /// The liquidator buys the whole vault at `liq_bonus` below the oracle
    /// price. The debt is repaid to the pool first; anything above it belongs
    /// to the borrower. The NFT, and with it the Bitcoin key, moves to the
    /// liquidator.
    fn liquidate(&mut self, vault_id: u64, liquidator: &AccountId, amount: u128) -> u128 {
        let price = self.fresh_price();
        let borrower = self.vault_owner(vault_id);
        require!(
            *liquidator != borrower,
            "repay your own vault instead of liquidating it"
        );
        let mut vault = self.vaults.get(&vault_id).unwrap().clone();
        let quote = self.quote(collateral_sats(&vault), self.debt_of(&vault), price);
        require!(quote.liquidatable, "vault is healthy");
        require!(
            amount >= quote.cost.0,
            format!("liquidating this vault costs {} micro-USDC", quote.cost.0)
        );

        self.pool.total_debt_scaled -= vault.debt_scaled;
        self.pool.cash += quote.to_pool.0;
        vault.debt_scaled = 0;
        self.vaults.insert(vault_id, vault);
        self.credit(&borrower, quote.to_borrower.0);
        self.tokens.internal_transfer(
            &borrower,
            liquidator,
            &vault_id.to_string(),
            None,
            Some("liquidation".to_string()),
        );
        emit(
            "liquidate",
            json!({
                "vault_id": vault_id,
                "borrower": borrower,
                "liquidator": liquidator,
                "collateral_sats": quote.collateral_sats,
                "debt": quote.debt,
                "cost": quote.cost,
                "to_borrower": quote.to_borrower,
                "bad_debt": quote.bad_debt,
            }),
        );
        amount - quote.cost.0
    }
}
