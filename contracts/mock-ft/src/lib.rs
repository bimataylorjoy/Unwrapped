//! Test stablecoin for sandbox and testnet. Anyone can call `faucet` to
//! receive tokens, so it has no value by construction. Never use on mainnet.

use near_contract_standards::fungible_token::events::FtMint;
use near_contract_standards::fungible_token::metadata::{
    FungibleTokenMetadata, FungibleTokenMetadataProvider, FT_METADATA_SPEC,
};
use near_contract_standards::fungible_token::{
    FungibleToken, FungibleTokenCore, FungibleTokenResolver,
};
use near_contract_standards::storage_management::{
    StorageBalance, StorageBalanceBounds, StorageManagement,
};
use near_sdk::json_types::U128;
use near_sdk::{env, near, require, AccountId, NearToken, PanicOnDefault, PromiseOrValue};

/// 10,000 tokens per faucet call at 6 decimals.
const FAUCET_AMOUNT: u128 = 10_000_000_000;

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct MockFt {
    owner_id: AccountId,
    token: FungibleToken,
    name: String,
    symbol: String,
    decimals: u8,
}

#[near]
impl MockFt {
    #[init]
    pub fn new(owner_id: AccountId, name: String, symbol: String, decimals: u8) -> Self {
        Self {
            owner_id,
            token: FungibleToken::new(b"t"),
            name,
            symbol,
            decimals,
        }
    }

    /// Mints `FAUCET_AMOUNT` to the caller, registering them if needed.
    pub fn faucet(&mut self) -> U128 {
        let account_id = env::predecessor_account_id();
        self.internal_mint(&account_id, FAUCET_AMOUNT);
        U128(FAUCET_AMOUNT)
    }

    /// Owner-only mint of an arbitrary amount.
    pub fn mint(&mut self, account_id: AccountId, amount: U128) {
        require!(env::predecessor_account_id() == self.owner_id, "owner only");
        self.internal_mint(&account_id, amount.0);
    }

    /// Registers accounts without a storage deposit (test token only).
    pub fn register_accounts(&mut self, account_ids: Vec<AccountId>) {
        for id in account_ids {
            if !self.token.accounts.contains_key(&id) {
                self.token.internal_register_account(&id);
            }
        }
    }

    fn internal_mint(&mut self, account_id: &AccountId, amount: u128) {
        if !self.token.accounts.contains_key(account_id) {
            self.token.internal_register_account(account_id);
        }
        self.token.internal_deposit(account_id, amount);
        FtMint {
            owner_id: account_id,
            amount: U128(amount),
            memo: Some("mock mint"),
        }
        .emit();
    }
}

#[near]
impl FungibleTokenCore for MockFt {
    #[payable]
    fn ft_transfer(&mut self, receiver_id: AccountId, amount: U128, memo: Option<String>) {
        self.token.ft_transfer(receiver_id, amount, memo)
    }

    #[payable]
    fn ft_transfer_call(
        &mut self,
        receiver_id: AccountId,
        amount: U128,
        memo: Option<String>,
        msg: String,
    ) -> PromiseOrValue<U128> {
        self.token.ft_transfer_call(receiver_id, amount, memo, msg)
    }

    fn ft_total_supply(&self) -> U128 {
        self.token.ft_total_supply()
    }

    fn ft_balance_of(&self, account_id: AccountId) -> U128 {
        self.token.ft_balance_of(account_id)
    }
}

#[near]
impl FungibleTokenResolver for MockFt {
    #[private]
    fn ft_resolve_transfer(
        &mut self,
        sender_id: AccountId,
        receiver_id: AccountId,
        amount: U128,
    ) -> U128 {
        let (used, _burned) =
            self.token
                .internal_ft_resolve_transfer(&sender_id, receiver_id, amount);
        used.into()
    }
}

#[near]
impl StorageManagement for MockFt {
    #[payable]
    fn storage_deposit(
        &mut self,
        account_id: Option<AccountId>,
        registration_only: Option<bool>,
    ) -> StorageBalance {
        self.token.storage_deposit(account_id, registration_only)
    }

    #[payable]
    fn storage_withdraw(&mut self, amount: Option<NearToken>) -> StorageBalance {
        self.token.storage_withdraw(amount)
    }

    #[payable]
    fn storage_unregister(&mut self, force: Option<bool>) -> bool {
        self.token.internal_storage_unregister(force).is_some()
    }

    fn storage_balance_bounds(&self) -> StorageBalanceBounds {
        self.token.storage_balance_bounds()
    }

    fn storage_balance_of(&self, account_id: AccountId) -> Option<StorageBalance> {
        self.token.storage_balance_of(account_id)
    }
}

#[near]
impl FungibleTokenMetadataProvider for MockFt {
    fn ft_metadata(&self) -> FungibleTokenMetadata {
        FungibleTokenMetadata {
            spec: FT_METADATA_SPEC.to_string(),
            name: self.name.clone(),
            symbol: self.symbol.clone(),
            icon: None,
            reference: None,
            reference_hash: None,
            decimals: self.decimals,
        }
    }
}
