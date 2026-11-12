//! Vault lifecycle: mint, prove a Bitcoin deposit, sign a Bitcoin withdrawal.

use crate::btc::{self, OutPoint, TxOut};
use crate::*;
use near_contract_standards::non_fungible_token::metadata::TokenMetadata;
use near_sdk::{Gas, Promise};

/// Each input needs its own MPC signature; the limits keep one withdrawal
/// inside a single 300 Tgas transaction.
pub const MAX_INPUTS: usize = 8;
pub const MAX_OUTPUTS: usize = 8;
/// Proven deposits a vault may hold at once. Consolidate on Bitcoin to free slots.
pub const MAX_UTXOS: usize = 16;
pub const MAX_SCRIPT_LEN: usize = 100;
pub const MAX_PROOF_DEPTH: usize = 32;
/// Deposits proven by someone other than the vault owner must be at least
/// this large, so a stranger cannot fill a vault's deposit slots with dust.
pub const MIN_THIRD_PARTY_DEPOSIT_SATS: u64 = 10_000;

const GAS_DERIVE_KEY: Gas = Gas::from_tgas(10);
const GAS_ON_VAULT_KEY: Gas = Gas::from_tgas(30);
const GAS_VERIFY_PROOF: Gas = Gas::from_tgas(30);
const GAS_ON_DEPOSIT: Gas = Gas::from_tgas(20);
/// The MPC contract requires at least 15 Tgas attached to `sign`.
const GAS_MPC_SIGN: Gas = Gas::from_tgas(25);
const GAS_ON_SIGNED: Gas = Gas::from_tgas(40);

#[near(serializers = [json])]
#[derive(Clone, Debug)]
pub struct BtcInput {
    /// Transaction id as block explorers show it.
    pub txid: String,
    pub vout: u32,
    /// Value of the output being spent, in satoshis.
    pub value: U64,
}

#[near(serializers = [json])]
#[derive(Clone, Debug)]
pub struct BtcOutput {
    /// Hex of the destination scriptPubKey.
    pub script_pubkey: String,
    pub value: U64,
}

#[near(serializers = [json])]
#[derive(Clone, Debug)]
pub struct UtxoView {
    pub txid: String,
    pub vout: u32,
    pub value: U64,
}

#[near(serializers = [json])]
#[derive(Clone, Debug)]
pub struct VaultView {
    pub vault_id: u64,
    pub owner_id: AccountId,
    /// Native Bitcoin address controlled by this vault.
    pub btc_address: String,
    pub public_key: String,
    pub script_pubkey: String,
    /// Chain Signatures derivation path under this contract's account.
    pub derivation_path: String,
    /// Proven deposits with no signature ever issued: the collateral.
    pub utxos: Vec<UtxoView>,
    pub collateral_sats: U64,
    /// Debt in micro-USDC including interest accrued up to now.
    pub debt: U128,
    /// A vault with debt cannot be transferred and cannot sign freely.
    pub locked: bool,
    /// Collateral value in micro-USDC; `None` if the price is stale.
    pub collateral_value: Option<U128>,
    pub ltv_bps: Option<u32>,
    pub liquidatable: bool,
}

/// A fully signed Bitcoin transaction, ready for `sendrawtransaction`.
#[near(serializers = [json])]
#[derive(Clone, Debug)]
pub struct SignedWithdrawal {
    pub vault_id: u64,
    pub txid: String,
    pub raw_tx: String,
    pub fee_sats: U64,
}

/// Arguments of the light client's `verify_transaction_inclusion_v2`.
#[near(serializers = [borsh])]
struct ProofArgsV2 {
    tx_id: [u8; 32],
    tx_block_blockhash: [u8; 32],
    tx_index: u64,
    merkle_proof: Vec<[u8; 32]>,
    coinbase_tx_id: [u8; 32],
    coinbase_merkle_proof: Vec<[u8; 32]>,
    confirmations: u64,
}

pub(crate) fn vault_path(vault_id: u64) -> String {
    format!("vault-{vault_id}")
}

pub(crate) fn collateral_sats(vault: &Vault) -> u64 {
    vault.utxos.iter().map(|u| u.value).sum()
}

fn pubkey_hash(vault: &Vault) -> [u8; 20] {
    btc::hash160(&vault.pubkey)
}

fn unwrap_or_panic<T>(r: Result<T, &'static str>) -> T {
    r.unwrap_or_else(|e| env::panic_str(e))
}

fn decode_hashes(list: &[String]) -> Vec<[u8; 32]> {
    list.iter()
        .map(|h| unwrap_or_panic(btc::hash_from_display_hex(h)))
        .collect()
}

/// Validates a withdrawal description and converts it to wire types.
fn decode_withdrawal(
    inputs: &[BtcInput],
    outputs: &[BtcOutput],
) -> (Vec<(OutPoint, u64)>, Vec<TxOut>, u64) {
    require!(
        !inputs.is_empty() && inputs.len() <= MAX_INPUTS,
        "need 1 to 8 inputs"
    );
    require!(
        !outputs.is_empty() && outputs.len() <= MAX_OUTPUTS,
        "need 1 to 8 outputs"
    );

    let mut ins: Vec<(OutPoint, u64)> = Vec::with_capacity(inputs.len());
    let mut total_in: u64 = 0;
    for i in inputs {
        let op = OutPoint {
            txid: unwrap_or_panic(btc::hash_from_display_hex(&i.txid)),
            vout: i.vout,
        };
        require!(ins.iter().all(|(o, _)| *o != op), "duplicate input");
        total_in = total_in
            .checked_add(i.value.0)
            .unwrap_or_else(|| env::panic_str("input overflow"));
        ins.push((op, i.value.0));
    }

    let mut outs: Vec<TxOut> = Vec::with_capacity(outputs.len());
    let mut total_out: u64 = 0;
    for o in outputs {
        let script_pubkey = hex::decode(&o.script_pubkey)
            .unwrap_or_else(|_| env::panic_str("script_pubkey must be hex"));
        require!(
            !script_pubkey.is_empty() && script_pubkey.len() <= MAX_SCRIPT_LEN,
            "script_pubkey has an invalid length"
        );
        total_out = total_out
            .checked_add(o.value.0)
            .unwrap_or_else(|| env::panic_str("output overflow"));
        outs.push(TxOut {
            value: o.value.0,
            script_pubkey,
        });
    }
    require!(total_out <= total_in, "outputs exceed inputs");
    (ins, outs, total_in - total_out)
}

#[near]
impl Unwrapped {
    /// Creates a vault: an NFT that controls a fresh native Bitcoin address.
    ///
    /// The address belongs to a key that only this contract can ask the MPC
    /// network to sign with. Attach at least `mint_fee` NEAR for storage.
    #[payable]
    pub fn mint_vault(&mut self) -> Promise {
        let deposit = env::attached_deposit();
        require!(deposit >= self.cfg.mint_fee, "attach at least the mint fee");
        let owner_id = env::predecessor_account_id();
        let vault_id = self.next_vault_id;
        self.next_vault_id += 1;

        Promise::new(self.cfg.mpc_signer.clone())
            .function_call(
                "derived_public_key".to_string(),
                json!({
                    "path": vault_path(vault_id),
                    "predecessor": env::current_account_id(),
                    "domain_id": self.cfg.mpc_domain_id,
                })
                .to_string()
                .into_bytes(),
                NearToken::from_yoctonear(0),
                GAS_DERIVE_KEY,
            )
            .then(Promise::new(env::current_account_id()).function_call(
                "on_vault_key".to_string(),
                json!({ "vault_id": vault_id, "owner_id": owner_id, "deposit": U128(deposit.as_yoctonear()) })
                    .to_string()
                    .into_bytes(),
                NearToken::from_yoctonear(0),
                GAS_ON_VAULT_KEY,
            ))
    }

    #[private]
    pub fn on_vault_key(
        &mut self,
        vault_id: u64,
        owner_id: AccountId,
        deposit: U128,
    ) -> Option<VaultView> {
        let pubkey = env::promise_result_checked(0, 256).ok().and_then(|bytes| {
            let s: String = near_sdk::serde_json::from_slice(&bytes).ok()?;
            let pk: near_sdk::PublicKey = s.parse().ok()?;
            // near_sdk encodes secp256k1 keys as curve byte 1 followed by x || y.
            let raw = pk.as_bytes();
            (raw.len() == 65 && raw[0] == 1).then(|| btc::compress_pubkey(&raw[1..]).ok())?
        });
        let Some(pubkey) = pubkey else {
            // The signer was unreachable: give the deposit back, mint nothing.
            Promise::new(owner_id)
                .transfer(NearToken::from_yoctonear(deposit.0))
                .detach();
            env::log_str("could not derive the vault key; deposit refunded");
            return None;
        };

        let vault = Vault {
            pubkey: pubkey.to_vec(),
            utxos: Vec::new(),
            debt_scaled: 0,
        };
        let address = btc::p2wpkh_address(&self.cfg.btc_hrp, &pubkey_hash(&vault));
        self.vaults.insert(vault_id, vault);
        self.tokens.internal_mint_with_refund(
            vault_id.to_string(),
            owner_id.clone(),
            Some(TokenMetadata {
                title: Some(format!("Unwrapped Vault #{vault_id}")),
                description: Some(format!(
                    "Controls the native Bitcoin address {address}. Whoever owns this NFT can spend its BTC."
                )),
                media: None,
                media_hash: None,
                copies: Some(1),
                issued_at: Some(env::block_timestamp_ms().to_string()),
                expires_at: None,
                starts_at: None,
                updated_at: None,
                extra: Some(json!({ "btc_address": address, "path": vault_path(vault_id) }).to_string()),
                reference: None,
                reference_hash: None,
            }),
            None,
        );
        near_contract_standards::non_fungible_token::events::NftMint {
            owner_id: &owner_id,
            token_ids: &[&vault_id.to_string()],
            memo: None,
        }
        .emit();
        emit(
            "vault_minted",
            json!({ "vault_id": vault_id, "owner_id": owner_id, "btc_address": address }),
        );
        self.get_vault(vault_id)
    }

    /// Registers a confirmed Bitcoin deposit as collateral of a vault.
    ///
    /// Anyone may call this. The transaction must pay the vault's address,
    /// the SPV proof must check out against the Bitcoin light client with
    /// `min_confirmations`, and the outpoint must be absent from the
    /// signature ledger.
    ///
    /// Hashes are hex in block-explorer byte order. `merkle_proof` and
    /// `coinbase_merkle_proof` are the sibling hashes from leaf to root; the
    /// coinbase proof defends against the 64-byte-transaction SPV forgery.
    #[allow(clippy::too_many_arguments)]
    pub fn prove_deposit(
        &mut self,
        vault_id: u64,
        tx_hex: String,
        vout: u32,
        block_hash: String,
        tx_index: u64,
        merkle_proof: Vec<String>,
        coinbase_tx_id: String,
        coinbase_merkle_proof: Vec<String>,
    ) -> Promise {
        let vault = self
            .vaults
            .get(&vault_id)
            .unwrap_or_else(|| env::panic_str("no such vault"));
        require!(
            vault.utxos.len() < MAX_UTXOS,
            "vault holds too many deposits; consolidate first"
        );
        require!(
            !merkle_proof.is_empty() && merkle_proof.len() <= MAX_PROOF_DEPTH,
            "merkle proof has an invalid length"
        );

        let raw = hex::decode(&tx_hex).unwrap_or_else(|_| env::panic_str("tx_hex must be hex"));
        let tx = unwrap_or_panic(btc::parse_tx(&raw));
        // A 64-byte transaction is indistinguishable from an inner merkle node.
        require!(
            tx.stripped_len != 64,
            "64-byte transactions are not accepted"
        );
        let out = tx
            .outputs
            .get(vout as usize)
            .unwrap_or_else(|| env::panic_str("no such output"));
        require!(
            out.script_pubkey == btc::p2wpkh_script(&pubkey_hash(vault)),
            "this output does not pay the vault address"
        );
        require!(out.value > 0, "output is empty");
        require!(
            out.value >= MIN_THIRD_PARTY_DEPOSIT_SATS
                || env::predecessor_account_id() == self.vault_owner(vault_id),
            "only the vault owner can register a deposit this small"
        );

        let outpoint = OutPoint {
            txid: tx.txid,
            vout,
        };
        self.assert_can_be_collateral(vault_id, &outpoint);

        let args = ProofArgsV2 {
            tx_id: tx.txid,
            tx_block_blockhash: unwrap_or_panic(btc::hash_from_display_hex(&block_hash)),
            tx_index,
            merkle_proof: decode_hashes(&merkle_proof),
            coinbase_tx_id: unwrap_or_panic(btc::hash_from_display_hex(&coinbase_tx_id)),
            coinbase_merkle_proof: decode_hashes(&coinbase_merkle_proof),
            confirmations: self.cfg.min_confirmations,
        };

        Promise::new(self.cfg.btc_light_client.clone())
            .function_call(
                "verify_transaction_inclusion_v2".to_string(),
                near_sdk::borsh::to_vec(&args).unwrap(),
                NearToken::from_yoctonear(0),
                GAS_VERIFY_PROOF,
            )
            .then(
                Promise::new(env::current_account_id()).function_call(
                    "on_deposit_verified".to_string(),
                    json!({
                        "vault_id": vault_id,
                        "txid": btc::hash_to_display_hex(&tx.txid),
                        "vout": vout,
                        "value": U64(out.value),
                    })
                    .to_string()
                    .into_bytes(),
                    NearToken::from_yoctonear(0),
                    GAS_ON_DEPOSIT,
                ),
            )
    }

    #[private]
    pub fn on_deposit_verified(
        &mut self,
        vault_id: u64,
        txid: String,
        vout: u32,
        value: U64,
    ) -> VaultView {
        let verified = matches!(env::promise_result_checked(0, 16), Ok(bytes) if bytes == b"true");
        require!(verified, "the Bitcoin light client rejected the proof");

        let outpoint = OutPoint {
            txid: unwrap_or_panic(btc::hash_from_display_hex(&txid)),
            vout,
        };
        // Re-checked here: a signature may have been issued while the proof
        // was being verified.
        self.assert_can_be_collateral(vault_id, &outpoint);

        let vault = self
            .vaults
            .get_mut(&vault_id)
            .unwrap_or_else(|| env::panic_str("no such vault"));
        require!(
            vault.utxos.len() < MAX_UTXOS,
            "vault holds too many deposits; consolidate first"
        );
        vault.utxos.push(Utxo {
            txid: outpoint.txid,
            vout,
            value: value.0,
        });
        self.collateral.insert(
            outpoint.key(),
            CollateralEntry {
                vault_id,
                value: value.0,
            },
        );

        emit(
            "deposit_proven",
            json!({ "vault_id": vault_id, "txid": txid, "vout": vout, "value": value }),
        );
        self.get_vault(vault_id).unwrap()
    }

    /// Signs a Bitcoin transaction that spends from the vault.
    ///
    /// The caller describes the transaction; the contract computes the BIP143
    /// digests itself, asks the MPC network for one signature per input and
    /// returns the complete raw transaction. Every input is written to the
    /// signature ledger *before* the MPC request leaves, so it is recorded
    /// even if the request later fails or the result is never broadcast.
    ///
    /// While the vault has debt, the collateral left after this withdrawal
    /// must still satisfy `max_ltv`. Change sent back to the vault address is
    /// not counted until it is proven with `prove_deposit`.
    ///
    /// Attach `ledger_fee` NEAR per input (see `get_config`).
    #[payable]
    pub fn sign_withdrawal(
        &mut self,
        vault_id: u64,
        inputs: Vec<BtcInput>,
        outputs: Vec<BtcOutput>,
    ) -> Promise {
        require!(
            env::predecessor_account_id() == self.vault_owner(vault_id),
            "only the vault owner can sign"
        );
        let (ins, outs, _fee) = decode_withdrawal(&inputs, &outputs);
        let ledger_fee = self.cfg.ledger_fee.as_yoctonear() * ins.len() as u128;
        require!(
            env::attached_deposit().as_yoctonear() >= ledger_fee.max(1),
            format!(
                "attach {ledger_fee} yoctoNEAR: the ledger fee for {} input(s)",
                ins.len()
            )
        );

        let mut vault = self.vaults.get(&vault_id).unwrap().clone();
        for (outpoint, value) in &ins {
            let key = outpoint.key();
            if let Some(entry) = self.collateral.get(&key).cloned() {
                require!(
                    entry.vault_id == vault_id,
                    "an input is collateral of another vault"
                );
                require!(
                    entry.value == *value,
                    "an input value differs from the proven deposit"
                );
                self.collateral.remove(&key);
                vault
                    .utxos
                    .retain(|u| !(u.txid == outpoint.txid && u.vout == outpoint.vout));
            }
            self.signature_ledger.insert((vault_id, key));
        }

        if vault.debt_scaled > 0 {
            self.accrue();
            let debt = self.debt_of(&vault);
            let remaining = sats_value(collateral_sats(&vault), self.fresh_price());
            require!(
                debt * BPS <= remaining * self.cfg.max_ltv_bps as u128,
                "withdrawal would leave the loan above the maximum loan-to-value"
            );
        }
        let pkh = pubkey_hash(&vault);
        self.vaults.insert(vault_id, vault);

        let path = vault_path(vault_id);
        let sign = |index: usize| {
            let digest = btc::bip143_sighash(&ins, &outs, index, &pkh);
            Promise::new(self.cfg.mpc_signer.clone()).function_call(
                "sign".to_string(),
                json!({ "request": {
                    "path": path,
                    "payload_v2": { "Ecdsa": hex::encode(digest) },
                    "domain_id": self.cfg.mpc_domain_id,
                }})
                .to_string()
                .into_bytes(),
                NearToken::from_yoctonear(1),
                GAS_MPC_SIGN,
            )
        };
        let mut requests = sign(0);
        for index in 1..ins.len() {
            requests = requests.and(sign(index));
        }

        emit(
            "withdrawal_requested",
            json!({ "vault_id": vault_id, "txid": btc::hash_to_display_hex(&btc::txid_of(&ins, &outs)), "inputs": inputs }),
        );
        requests.then(
            Promise::new(env::current_account_id()).function_call(
                "on_withdrawal_signed".to_string(),
                json!({ "vault_id": vault_id, "inputs": inputs, "outputs": outputs })
                    .to_string()
                    .into_bytes(),
                NearToken::from_yoctonear(0),
                GAS_ON_SIGNED,
            ),
        )
    }

    #[private]
    pub fn on_withdrawal_signed(
        &self,
        vault_id: u64,
        inputs: Vec<BtcInput>,
        outputs: Vec<BtcOutput>,
    ) -> SignedWithdrawal {
        let (ins, outs, fee) = decode_withdrawal(&inputs, &outputs);
        let pubkey: [u8; 33] = self
            .vaults
            .get(&vault_id)
            .unwrap()
            .pubkey
            .clone()
            .try_into()
            .unwrap();

        let mut witnesses = Vec::with_capacity(ins.len());
        for index in 0..ins.len() {
            let bytes = env::promise_result_checked(index as u64, 1024)
                .unwrap_or_else(|_| env::panic_str("the MPC network did not return a signature"));
            let (r, s) = parse_mpc_signature(&bytes).unwrap_or_else(|| {
                env::panic_str("unexpected signature format from the MPC contract")
            });
            witnesses.push((btc::witness_signature(&r, &s), pubkey));
        }

        let signed = SignedWithdrawal {
            vault_id,
            txid: btc::hash_to_display_hex(&btc::txid_of(&ins, &outs)),
            raw_tx: hex::encode(btc::serialize_tx(&ins, &outs, Some(&witnesses))),
            fee_sats: U64(fee),
        };
        emit(
            "withdrawal_signed",
            json!({ "vault_id": vault_id, "txid": signed.txid, "fee_sats": signed.fee_sats }),
        );
        signed
    }

    pub fn get_vault(&self, vault_id: u64) -> Option<VaultView> {
        let vault = self.vaults.get(&vault_id)?;
        let owner_id = self.tokens.owner_by_id.get(&vault_id.to_string())?;
        let pkh = pubkey_hash(vault);
        let sats = collateral_sats(vault);
        let debt = self.accrued_pool().debt_from_scaled(vault.debt_scaled);
        let value = self.try_fresh_price().map(|p| sats_value(sats, p));
        let ltv_bps = value.and_then(|v| {
            if debt == 0 {
                Some(0)
            } else if v == 0 {
                Some(u32::MAX)
            } else {
                Some(u32::try_from(debt * BPS / v).unwrap_or(u32::MAX))
            }
        });
        Some(VaultView {
            vault_id,
            owner_id,
            btc_address: btc::p2wpkh_address(&self.cfg.btc_hrp, &pkh),
            public_key: hex::encode(&vault.pubkey),
            script_pubkey: hex::encode(btc::p2wpkh_script(&pkh)),
            derivation_path: vault_path(vault_id),
            utxos: vault
                .utxos
                .iter()
                .map(|u| UtxoView {
                    txid: btc::hash_to_display_hex(&u.txid),
                    vout: u.vout,
                    value: U64(u.value),
                })
                .collect(),
            collateral_sats: U64(sats),
            debt: U128(debt),
            locked: vault.debt_scaled > 0,
            collateral_value: value.map(U128),
            ltv_bps,
            liquidatable: value
                .map(|v| debt > 0 && debt * BPS > v * self.cfg.liq_threshold_bps as u128)
                .unwrap_or(false),
        })
    }

    /// Vaults by id, starting at `from_id` (ids start at 1).
    pub fn get_vaults(&self, from_id: Option<u64>, limit: Option<u64>) -> Vec<VaultView> {
        let from = from_id.unwrap_or(1).max(1);
        let to = from
            .saturating_add(limit.unwrap_or(50).min(100))
            .min(self.next_vault_id);
        (from..to).filter_map(|id| self.get_vault(id)).collect()
    }

    pub fn vaults_of(&self, account_id: AccountId) -> Vec<VaultView> {
        self.tokens
            .tokens_per_owner
            .as_ref()
            .and_then(|by_owner| by_owner.get(&account_id))
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| self.get_vault(id.parse().ok()?))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn total_vaults(&self) -> u64 {
        self.next_vault_id - 1
    }

    /// What the contract knows about an output on the vault's address:
    /// `"collateral"` (proven, never signed for), `"signed"` (a signature
    /// spending it was requested at some point, so it can never be
    /// collateral) or `"unknown"`. A buyer or liquidator can check any
    /// output this way before taking over a vault.
    pub fn outpoint_state(&self, vault_id: u64, txid: String, vout: u32) -> String {
        let key = OutPoint {
            txid: unwrap_or_panic(btc::hash_from_display_hex(&txid)),
            vout,
        }
        .key();
        if self.signature_ledger.contains(&(vault_id, key)) {
            "signed"
        } else if matches!(self.collateral.get(&key), Some(e) if e.vault_id == vault_id) {
            "collateral"
        } else {
            "unknown"
        }
        .to_string()
    }
}

impl Unwrapped {
    pub(crate) fn vault_owner(&self, vault_id: u64) -> AccountId {
        self.tokens
            .owner_by_id
            .get(&vault_id.to_string())
            .unwrap_or_else(|| env::panic_str("no such vault"))
    }

    fn assert_can_be_collateral(&self, vault_id: u64, outpoint: &OutPoint) {
        let key = outpoint.key();
        require!(
            self.collateral.get(&key).is_none(),
            "this deposit is already registered"
        );
        require!(
            !self.signature_ledger.contains(&(vault_id, key)),
            "a signature spending this output was already issued, so it cannot be collateral"
        );
    }
}

/// Extracts `(r, s)` from the MPC contract's secp256k1 signature response:
/// `{"scheme":"Secp256k1","big_r":{"affine_point":"<33 bytes hex>"},"s":{"scalar":"<32 bytes hex>"},"recovery_id":0}`.
fn parse_mpc_signature(bytes: &[u8]) -> Option<([u8; 32], [u8; 32])> {
    let v: Value = near_sdk::serde_json::from_slice(bytes).ok()?;
    let mut big_r = [0u8; 33];
    hex::decode_to_slice(v.get("big_r")?.get("affine_point")?.as_str()?, &mut big_r).ok()?;
    let mut s = [0u8; 32];
    hex::decode_to_slice(v.get("s")?.get("scalar")?.as_str()?, &mut s).ok()?;
    // r is the x coordinate of the compressed point R.
    let mut r = [0u8; 32];
    r.copy_from_slice(&big_r[1..]);
    Some((r, s))
}
