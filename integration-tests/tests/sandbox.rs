//! End-to-end tests on a local NEAR sandbox node.
//!
//! Bitcoin is not mocked away: deposits are real Bitcoin transactions placed
//! in merkle trees and proven with real SPV proofs, and every withdrawal the
//! contract returns is decoded and signature-checked with the `bitcoin`
//! crate (the Rust reference implementation). Only two things are stand-ins:
//! the source of block headers (`mock-btc-client`) and the MPC network
//! (`mock-signer`, which signs with real secp256k1 keys).

use bitcoin::absolute::LockTime;
use bitcoin::consensus::encode::{deserialize, serialize_hex};
use bitcoin::hashes::{sha256d, Hash};
use bitcoin::secp256k1::{Message, Secp256k1};
use bitcoin::sighash::{EcdsaSighashType, SighashCache};
use bitcoin::transaction::Version;
use bitcoin::{
    Address, Amount, CompressedPublicKey, Network, OutPoint, ScriptBuf, Sequence, Transaction,
    TxIn, TxOut, Txid, Witness,
};
use near_workspaces::network::Sandbox;
use near_workspaces::result::ExecutionFinalResult;
use near_workspaces::types::{Gas, NearToken};
use near_workspaces::{Account, AccountId, Contract, Worker};
use serde_json::{json, Value};
use tokio::sync::OnceCell;

const USDC: u128 = 1_000_000;
const BTC: u64 = 100_000_000;
/// 100,000 USDC per BTC, in micro-USDC.
const PRICE_100K: u128 = 100_000 * USDC;
const MIN_CONFIRMATIONS: u64 = 2;
/// NEAR attached per withdrawal input for its permanent ledger entry.
const LEDGER_FEE: NearToken = NearToken::from_millinear(1);

struct Wasm {
    app: Vec<u8>,
    ft: Vec<u8>,
    signer: Vec<u8>,
    btc_client: Vec<u8>,
}

static WASM: OnceCell<Wasm> = OnceCell::const_new();

async fn wasm() -> &'static Wasm {
    WASM.get_or_init(|| async {
        let build = |p: &'static str| async move {
            near_workspaces::compile_project(p)
                .await
                .unwrap_or_else(|e| panic!("building {p}: {e:?}"))
        };
        Wasm {
            app: build("../contracts/unwrapped").await,
            ft: build("../contracts/mock-ft").await,
            signer: build("../contracts/mock-signer").await,
            btc_client: build("../contracts/mock-btc-client").await,
        }
    })
    .await
}

struct Lab {
    worker: Worker<Sandbox>,
    operator: Account,
    /// Three independent price reporters; two must agree.
    oracles: Vec<Account>,
    /// Borrower.
    alice: Account,
    /// Lender.
    bob: Account,
    /// Liquidator / buyer.
    carol: Account,
    usdc: Contract,
    signer: Contract,
    btc_client: Contract,
    app: Contract,
    /// Height of the next fake Bitcoin block.
    next_height: std::cell::Cell<u64>,
}

fn config(lab_ids: (&AccountId, &AccountId, &AccountId, &[AccountId])) -> Value {
    let (signer, btc_client, usdc, oracles) = lab_ids;
    json!({
        "mpc_signer": signer,
        "mpc_domain_id": 0,
        "btc_light_client": btc_client,
        "btc_hrp": "tb",
        "min_confirmations": MIN_CONFIRMATIONS,
        "usdc": usdc,
        "oracle_ids": oracles,
        "min_oracle_reports": 2,
        "max_oracle_spread_bps": 200,
        "max_price_age_sec": 3600,
        "max_ltv_bps": 5000,
        "liq_threshold_bps": 7500,
        "liq_bonus_bps": 500,
        "base_rate_bps": 200,
        "slope_rate_bps": 1800,
        "mint_fee": NearToken::from_millinear(100).as_yoctonear().to_string(),
        "ledger_fee": LEDGER_FEE.as_yoctonear().to_string(),
    })
}

async fn sub(root: &Account, name: &str) -> anyhow::Result<Account> {
    Ok(root
        .create_subaccount(name)
        .initial_balance(NearToken::from_near(20))
        .transact()
        .await?
        .into_result()?)
}

async fn setup() -> anyhow::Result<Lab> {
    setup_with(false).await
}

/// The contract as deployed on testnet on 2026-11-12, before the
/// multi-reporter oracle: one oracle, `set_price`, no quorum.
const V1_WASM: &[u8] = include_bytes!("../fixtures/unwrapped-v1-single-oracle.wasm");

/// `legacy` deploys `V1_WASM` with its single-oracle configuration instead
/// of the current contract, so a test can migrate it.
async fn setup_with(legacy: bool) -> anyhow::Result<Lab> {
    let w = wasm().await;
    let worker = near_workspaces::sandbox().await?;
    let root = worker.root_account()?;
    let operator = sub(&root, "operator").await?;
    let mut oracles = Vec::new();
    for name in ["oracle-a", "oracle-b", "oracle-c"] {
        oracles.push(sub(&root, name).await?);
    }
    let oracle_ids: Vec<AccountId> = oracles.iter().map(|o| o.id().clone()).collect();
    let alice = sub(&root, "alice").await?;
    let bob = sub(&root, "bob").await?;
    let carol = sub(&root, "carol").await?;

    let usdc = worker.dev_deploy(&w.ft).await?;
    usdc.call("new")
        .args_json(json!({ "owner_id": operator.id(), "name": "USD Coin (test)", "symbol": "USDC", "decimals": 6 }))
        .transact()
        .await?
        .into_result()?;
    let signer = worker.dev_deploy(&w.signer).await?;
    signer.call("new").transact().await?.into_result()?;
    let btc_client = worker.dev_deploy(&w.btc_client).await?;
    btc_client.call("new").transact().await?.into_result()?;

    let mut cfg = config((signer.id(), btc_client.id(), usdc.id(), &oracle_ids));
    let app = if legacy {
        let obj = cfg.as_object_mut().unwrap();
        obj.remove("min_oracle_reports");
        obj.remove("max_oracle_spread_bps");
        obj.insert("oracle_ids".into(), json!([oracle_ids[0]]));
        worker.dev_deploy(V1_WASM).await?
    } else {
        worker.dev_deploy(&w.app).await?
    };
    app.call("new")
        .args_json(json!({ "owner_id": operator.id(), "config": cfg }))
        .transact()
        .await?
        .into_result()?;

    let lab = Lab {
        worker,
        operator,
        oracles,
        alice,
        bob,
        carol,
        usdc,
        signer,
        btc_client,
        app,
        next_height: std::cell::Cell::new(800_000),
    };
    lab.operator
        .call(lab.usdc.id(), "register_accounts")
        .args_json(
            json!({ "account_ids": [lab.app.id(), lab.alice.id(), lab.bob.id(), lab.carol.id()] }),
        )
        .transact()
        .await?
        .into_result()?;
    for (who, amount) in [(&lab.bob, 1_000_000 * USDC), (&lab.carol, 1_000_000 * USDC)] {
        lab.operator
            .call(lab.usdc.id(), "mint")
            .args_json(json!({ "account_id": who.id(), "amount": amount.to_string() }))
            .transact()
            .await?
            .into_result()?;
    }
    if legacy {
        must(
            lab.oracles[0]
                .call(lab.app.id(), "set_price")
                .args_json(json!({ "usdc_per_btc": PRICE_100K.to_string() }))
                .transact()
                .await?,
        )?;
    } else {
        lab.set_price(PRICE_100K).await?;
    }
    Ok(lab)
}

/// Everything that went wrong in a transaction, including failed receipts
/// further down the promise chain.
fn errors(res: &ExecutionFinalResult) -> String {
    format!("{:?} {:?}", res.failures(), res.clone().into_result().err())
}

fn assert_fails(res: &ExecutionFinalResult, needle: &str) {
    let all = errors(res);
    assert!(
        !res.failures().is_empty(),
        "expected a failure containing {needle:?}, but everything succeeded"
    );
    assert!(
        all.contains(needle),
        "expected failure containing {needle:?}, got: {all}"
    );
}

fn must(res: ExecutionFinalResult) -> anyhow::Result<Value> {
    let failures = format!("{:?}", res.failures());
    let ok = res.into_result().map_err(|e| anyhow::anyhow!("{e:?}"))?;
    anyhow::ensure!(failures == "[]", "a receipt failed: {failures}");
    Ok(ok.json().unwrap_or(Value::Null))
}

/// Bitcoin merkle tree over txids in internal byte order. Returns the root
/// and, for each leaf, its proof (siblings from leaf to root).
fn merkle(leaves: &[[u8; 32]]) -> ([u8; 32], Vec<Vec<[u8; 32]>>) {
    let mut proofs = vec![Vec::new(); leaves.len()];
    let mut level: Vec<[u8; 32]> = leaves.to_vec();
    let mut positions: Vec<usize> = (0..leaves.len()).collect();
    while level.len() > 1 {
        if level.len() % 2 == 1 {
            level.push(*level.last().unwrap());
        }
        for (leaf, pos) in positions.iter_mut().enumerate() {
            proofs[leaf].push(level[*pos ^ 1]);
            *pos /= 2;
        }
        level = level
            .chunks(2)
            .map(|pair| {
                let mut buf = [0u8; 64];
                buf[..32].copy_from_slice(&pair[0]);
                buf[32..].copy_from_slice(&pair[1]);
                sha256d::Hash::hash(&buf).to_byte_array()
            })
            .collect();
    }
    (level[0], proofs)
}

fn display(h: &[u8; 32]) -> String {
    let mut r = *h;
    r.reverse();
    hex::encode(r)
}

fn dummy_tx(tag: u32) -> Transaction {
    Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: Txid::from_byte_array([tag as u8; 32]),
                vout: tag,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(1_000 + tag as u64),
            script_pubkey: ScriptBuf::new_op_return([1, 2, 3]),
        }],
    }
}

/// A Bitcoin transaction paying `sats` to `script` at output index 1.
fn funding_tx(script: &ScriptBuf, sats: u64, tag: u32) -> Transaction {
    let mut tx = dummy_tx(tag);
    tx.output.push(TxOut {
        value: Amount::from_sat(sats),
        script_pubkey: script.clone(),
    });
    tx
}

struct Deposit {
    tx: Transaction,
    vout: u32,
    args: Value,
}

struct VaultRef {
    id: u64,
    pubkey: CompressedPublicKey,
    script: ScriptBuf,
}

impl Lab {
    /// Two of the three reporters post the same price: a quorum.
    async fn set_price(&self, usdc_per_btc: u128) -> anyhow::Result<()> {
        self.report(0, usdc_per_btc).await?;
        self.report(1, usdc_per_btc).await
    }

    async fn report(&self, oracle: usize, usdc_per_btc: u128) -> anyhow::Result<()> {
        must(
            self.oracles[oracle]
                .call(self.app.id(), "report_price")
                .args_json(json!({ "usdc_per_btc": usdc_per_btc.to_string() }))
                .transact()
                .await?,
        )?;
        Ok(())
    }

    async fn price(&self) -> anyhow::Result<Value> {
        Ok(self.app.view("get_price").await?.json()?)
    }

    async fn vault(&self, id: u64) -> anyhow::Result<Value> {
        Ok(self
            .app
            .view("get_vault")
            .args_json(json!({ "vault_id": id }))
            .await?
            .json()?)
    }

    async fn pool(&self) -> anyhow::Result<Value> {
        Ok(self.app.view("get_pool").await?.json()?)
    }

    async fn usdc_of(&self, who: &AccountId) -> anyhow::Result<u128> {
        let v: Value = self
            .usdc
            .view("ft_balance_of")
            .args_json(json!({ "account_id": who }))
            .await?
            .json()?;
        Ok(v.as_str().unwrap().parse()?)
    }

    async fn outpoint_state(
        &self,
        vault_id: u64,
        txid: &Txid,
        vout: u32,
    ) -> anyhow::Result<String> {
        Ok(self
            .app
            .view("outpoint_state")
            .args_json(json!({ "vault_id": vault_id, "txid": txid.to_string(), "vout": vout }))
            .await?
            .json()?)
    }

    async fn mint_vault(&self, owner: &Account) -> anyhow::Result<VaultRef> {
        let v = must(
            owner
                .call(self.app.id(), "mint_vault")
                .deposit(NearToken::from_millinear(100))
                .gas(Gas::from_tgas(100))
                .transact()
                .await?,
        )?;
        let pubkey =
            CompressedPublicKey::from_slice(&hex::decode(v["public_key"].as_str().unwrap())?)?;
        Ok(VaultRef {
            id: v["vault_id"].as_u64().unwrap(),
            pubkey,
            script: ScriptBuf::new_p2wpkh(&pubkey.wpubkey_hash()),
        })
    }

    /// Mines `tx` into a fake block (with a coinbase and filler
    /// transactions), registers the header with the light client and returns
    /// the arguments for `prove_deposit`. `confirmations` counts the block
    /// itself.
    async fn mine(
        &self,
        vault: &VaultRef,
        tx: Transaction,
        vout: u32,
        confirmations: u64,
    ) -> anyhow::Result<Deposit> {
        let height = self.next_height.get();
        self.next_height.set(height + 10);
        let coinbase = dummy_tx(200 + (height % 50) as u32);
        let fillers = [dummy_tx(11), dummy_tx(12), dummy_tx(13)];
        let txs = [&coinbase, &fillers[0], &fillers[1], &tx, &fillers[2]];
        let leaves: Vec<[u8; 32]> = txs
            .iter()
            .map(|t| t.compute_txid().to_byte_array())
            .collect();
        let (root, proofs) = merkle(&leaves);
        let block_hash = sha256d::Hash::hash(&height.to_le_bytes()).to_byte_array();

        must(
            self.operator
                .call(self.btc_client.id(), "add_block")
                .args_json(json!({ "block_hash": display(&block_hash), "merkle_root": display(&root), "height": height }))
                .transact()
                .await?,
        )?;
        must(
            self.operator
                .call(self.btc_client.id(), "set_tip_height")
                .args_json(json!({ "height": height + confirmations - 1 }))
                .transact()
                .await?,
        )?;

        let hexes = |p: &Vec<[u8; 32]>| p.iter().map(display).collect::<Vec<_>>();
        let args = json!({
            "vault_id": vault.id,
            "tx_hex": serialize_hex(&tx),
            "vout": vout,
            "block_hash": display(&block_hash),
            "tx_index": 3,
            "merkle_proof": hexes(&proofs[3]),
            "coinbase_tx_id": display(&leaves[0]),
            "coinbase_merkle_proof": hexes(&proofs[0]),
        });
        Ok(Deposit { tx, vout, args })
    }

    async fn prove(&self, caller: &Account, args: &Value) -> anyhow::Result<ExecutionFinalResult> {
        Ok(caller
            .call(self.app.id(), "prove_deposit")
            .args_json(args)
            .gas(Gas::from_tgas(150))
            .transact()
            .await?)
    }

    /// Funds the vault with `sats` on (fake) Bitcoin and registers the proof.
    async fn deposit(&self, vault: &VaultRef, sats: u64, tag: u32) -> anyhow::Result<Deposit> {
        let d = self
            .mine(
                vault,
                funding_tx(&vault.script, sats, tag),
                1,
                MIN_CONFIRMATIONS,
            )
            .await?;
        must(self.prove(&self.carol, &d.args).await?)?;
        Ok(d)
    }

    async fn ft_call(
        &self,
        from: &Account,
        amount: u128,
        msg: Value,
    ) -> anyhow::Result<ExecutionFinalResult> {
        Ok(from
            .call(self.usdc.id(), "ft_transfer_call")
            .args_json(json!({ "receiver_id": self.app.id(), "amount": amount.to_string(), "msg": msg.to_string() }))
            .deposit(NearToken::from_yoctonear(1))
            .gas(Gas::from_tgas(300))
            .transact()
            .await?)
    }

    async fn supply(&self, from: &Account, amount: u128) -> anyhow::Result<()> {
        must(
            self.ft_call(from, amount, json!({ "action": "supply" }))
                .await?,
        )?;
        Ok(())
    }

    async fn borrow(
        &self,
        who: &Account,
        vault_id: u64,
        amount: u128,
    ) -> anyhow::Result<ExecutionFinalResult> {
        Ok(who
            .call(self.app.id(), "borrow")
            .args_json(json!({ "vault_id": vault_id, "amount": amount.to_string() }))
            .deposit(NearToken::from_yoctonear(1))
            .gas(Gas::from_tgas(100))
            .transact()
            .await?)
    }

    async fn sign(
        &self,
        who: &Account,
        vault_id: u64,
        inputs: &[(&Deposit, u64)],
        outputs: &[(ScriptBuf, u64)],
    ) -> anyhow::Result<ExecutionFinalResult> {
        let inputs: Vec<Value> = inputs
            .iter()
            .map(|(d, value)| json!({ "txid": d.tx.compute_txid().to_string(), "vout": d.vout, "value": value.to_string() }))
            .collect();
        let outputs: Vec<Value> = outputs
            .iter()
            .map(|(script, value)| json!({ "script_pubkey": hex::encode(script.as_bytes()), "value": value.to_string() }))
            .collect();
        Ok(who
            .call(self.app.id(), "sign_withdrawal")
            .args_json(json!({ "vault_id": vault_id, "inputs": inputs, "outputs": outputs }))
            .deposit(LEDGER_FEE.saturating_mul(inputs.len() as u128))
            .gas(Gas::from_tgas(300))
            .transact()
            .await?)
    }

    async fn transfer_nft(
        &self,
        from: &Account,
        to: &AccountId,
        vault_id: u64,
    ) -> anyhow::Result<ExecutionFinalResult> {
        Ok(from
            .call(self.app.id(), "nft_transfer")
            .args_json(json!({ "receiver_id": to, "token_id": vault_id.to_string() }))
            .deposit(NearToken::from_yoctonear(1))
            .gas(Gas::from_tgas(50))
            .transact()
            .await?)
    }
}

fn destination() -> ScriptBuf {
    // Any address not controlled by a vault.
    let pk = CompressedPublicKey::from_slice(
        &hex::decode("0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798").unwrap(),
    )
    .unwrap();
    ScriptBuf::new_p2wpkh(&pk.wpubkey_hash())
}

/// Checks that `signed` is a broadcastable Bitcoin transaction: it decodes,
/// spends exactly `inputs`, and every witness is a valid signature by the
/// vault key over the BIP143 digest.
fn assert_valid_bitcoin_tx(
    signed: &Value,
    vault: &VaultRef,
    inputs: &[(&Deposit, u64)],
    outputs: &[(ScriptBuf, u64)],
) {
    let raw = hex::decode(signed["raw_tx"].as_str().unwrap()).unwrap();
    let tx: Transaction = deserialize(&raw).expect("contract returned a decodable transaction");
    assert_eq!(
        tx.compute_txid().to_string(),
        signed["txid"].as_str().unwrap()
    );
    assert_eq!(tx.input.len(), inputs.len());
    assert_eq!(tx.output.len(), outputs.len());
    for (out, (script, value)) in tx.output.iter().zip(outputs) {
        assert_eq!(&out.script_pubkey, script);
        assert_eq!(out.value.to_sat(), *value);
    }

    let secp = Secp256k1::verification_only();
    let mut cache = SighashCache::new(&tx);
    for (i, (deposit, value)) in inputs.iter().enumerate() {
        assert_eq!(
            tx.input[i].previous_output,
            OutPoint {
                txid: deposit.tx.compute_txid(),
                vout: deposit.vout
            }
        );
        let witness = &tx.input[i].witness;
        assert_eq!(witness.len(), 2, "P2WPKH witness is <signature> <pubkey>");
        assert_eq!(&witness[1], vault.pubkey.to_bytes().as_slice());
        let sig = bitcoin::ecdsa::Signature::from_slice(&witness[0]).expect("strict DER signature");
        assert_eq!(sig.sighash_type, EcdsaSighashType::All);
        let digest = cache
            .p2wpkh_signature_hash(
                i,
                &vault.script,
                Amount::from_sat(*value),
                EcdsaSighashType::All,
            )
            .unwrap();
        secp.verify_ecdsa(
            &Message::from_digest(digest.to_byte_array()),
            &sig.signature,
            &vault.pubkey.0,
        )
        .expect("witness signature verifies under the vault key");
    }
}

fn u(v: &Value) -> u128 {
    v.as_str()
        .unwrap_or_else(|| panic!("expected a numeric string, got {v}"))
        .parse()
        .unwrap()
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn vault_is_an_nft_that_owns_a_native_bitcoin_address() -> anyhow::Result<()> {
    let lab = setup().await?;
    let vault = lab.mint_vault(&lab.alice).await?;
    let view = lab.vault(vault.id).await?;

    // The address the contract reports is the real P2WPKH address of the key
    // the signer derived for (contract account, "vault-1").
    assert_eq!(
        view["btc_address"],
        Address::p2wpkh(&vault.pubkey, Network::Testnet).to_string()
    );
    assert_eq!(view["script_pubkey"], hex::encode(vault.script.as_bytes()));
    assert_eq!(view["derivation_path"], "vault-1");
    let derived: String = lab
        .signer
        .view("derived_public_key")
        .args_json(json!({ "path": "vault-1", "predecessor": lab.app.id() }))
        .await?
        .json()?;
    let near_key: near_workspaces::types::PublicKey = derived.parse()?;
    assert_eq!(
        &near_key.key_data()[..32],
        &vault.pubkey.to_bytes()[1..],
        "same x coordinate"
    );

    // It is a standard NEP-171 token owned by the minter.
    let token: Value = lab
        .app
        .view("nft_token")
        .args_json(json!({ "token_id": "1" }))
        .await?
        .json()?;
    assert_eq!(token["owner_id"], lab.alice.id().to_string());
    assert_eq!(token["metadata"]["title"], "Unwrapped Vault #1");
    let owned: Vec<Value> = lab
        .app
        .view("nft_tokens_for_owner")
        .args_json(json!({ "account_id": lab.alice.id() }))
        .await?
        .json()?;
    assert_eq!(owned.len(), 1);
    assert_eq!(view["collateral_sats"], "0");
    assert_eq!(view["locked"], false);

    // Each vault gets its own key.
    let second = lab.mint_vault(&lab.bob).await?;
    assert_eq!(second.id, 2);
    assert_ne!(second.pubkey, vault.pubkey);

    // Minting needs the fee.
    let res = lab
        .alice
        .call(lab.app.id(), "mint_vault")
        .gas(Gas::from_tgas(100))
        .transact()
        .await?;
    assert_fails(&res, "attach at least the mint fee");
    Ok(())
}

#[tokio::test]
async fn deposit_counts_only_with_a_valid_spv_proof() -> anyhow::Result<()> {
    let lab = setup().await?;
    let vault = lab.mint_vault(&lab.alice).await?;

    // Paying someone else's address is not a deposit.
    let stranger = lab
        .mine(
            &vault,
            funding_tx(&destination(), BTC, 1),
            1,
            MIN_CONFIRMATIONS,
        )
        .await?;
    assert_fails(
        &lab.prove(&lab.alice, &stranger.args).await?,
        "does not pay the vault address",
    );

    // A real payment to the vault, but not yet deep enough.
    let shallow = lab
        .mine(
            &vault,
            funding_tx(&vault.script, BTC, 2),
            1,
            MIN_CONFIRMATIONS - 1,
        )
        .await?;
    assert_fails(
        &lab.prove(&lab.alice, &shallow.args).await?,
        "Not enough blocks confirmed",
    );
    assert_eq!(lab.vault(vault.id).await?["collateral_sats"], "0");

    // A proof for a transaction that is not in the block.
    let honest = lab
        .mine(
            &vault,
            funding_tx(&vault.script, BTC, 3),
            1,
            MIN_CONFIRMATIONS,
        )
        .await?;
    let mut forged = honest.args.clone();
    forged["tx_hex"] = json!(serialize_hex(&funding_tx(&vault.script, 50 * BTC, 4)));
    assert_fails(&lab.prove(&lab.alice, &forged).await?, "rejected the proof");

    // A wrong position in the block.
    let mut wrong_index = honest.args.clone();
    wrong_index["tx_index"] = json!(2);
    assert_fails(
        &lab.prove(&lab.alice, &wrong_index).await?,
        "rejected the proof",
    );

    // A block the light client has never seen.
    let mut unknown_block = honest.args.clone();
    unknown_block["block_hash"] = json!(display(&[9u8; 32]));
    assert_fails(
        &lab.prove(&lab.alice, &unknown_block).await?,
        "rejected the proof",
    );

    // The honest proof is accepted, from any account.
    let view = must(lab.prove(&lab.carol, &honest.args).await?)?;
    assert_eq!(view["collateral_sats"], BTC.to_string());
    assert_eq!(
        view["utxos"][0]["txid"],
        honest.tx.compute_txid().to_string()
    );
    assert_eq!(view["utxos"][0]["vout"], 1);
    assert_eq!(
        lab.outpoint_state(vault.id, &honest.tx.compute_txid(), 1)
            .await?,
        "collateral"
    );

    // The same deposit cannot be counted twice, for this vault or another.
    assert_fails(
        &lab.prove(&lab.alice, &honest.args).await?,
        "already registered",
    );
    let other = lab.mint_vault(&lab.bob).await?;
    let mut replay = honest.args.clone();
    replay["vault_id"] = json!(other.id);
    assert_fails(
        &lab.prove(&lab.bob, &replay).await?,
        "does not pay the vault address",
    );
    Ok(())
}

#[tokio::test]
async fn withdrawal_is_a_valid_signed_bitcoin_transaction() -> anyhow::Result<()> {
    let lab = setup().await?;
    let vault = lab.mint_vault(&lab.alice).await?;
    let d1 = lab.deposit(&vault, 60_000_000, 1).await?;
    let d2 = lab.deposit(&vault, 40_000_000, 2).await?;
    assert_eq!(
        lab.vault(vault.id).await?["collateral_sats"],
        BTC.to_string()
    );

    // Only the NFT owner can ask for a signature.
    let inputs = [(&d1, 60_000_000u64), (&d2, 40_000_000u64)];
    let outputs = [
        (destination(), 70_000_000u64),
        (vault.script.clone(), 29_990_000u64),
    ];
    assert_fails(
        &lab.sign(&lab.bob, vault.id, &inputs, &outputs).await?,
        "only the vault owner can sign",
    );
    assert_eq!(
        lab.outpoint_state(vault.id, &d1.tx.compute_txid(), 1)
            .await?,
        "collateral"
    );

    // The contract will not sign a transaction that creates money, or one
    // that misstates the value of a proven deposit.
    let too_much = [(destination(), BTC + 1)];
    assert_fails(
        &lab.sign(&lab.alice, vault.id, &inputs, &too_much).await?,
        "outputs exceed inputs",
    );
    let misstated = [(&d1, 60_000_001u64)];
    assert_fails(
        &lab.sign(&lab.alice, vault.id, &misstated, &[(destination(), 1_000)])
            .await?,
        "differs from the proven deposit",
    );

    let signed = must(lab.sign(&lab.alice, vault.id, &inputs, &outputs).await?)?;
    assert_eq!(signed["fee_sats"], "10000");
    assert_valid_bitcoin_tx(&signed, &vault, &inputs, &outputs);

    // Both outpoints are now in the signature ledger and no longer collateral.
    assert_eq!(
        lab.outpoint_state(vault.id, &d1.tx.compute_txid(), 1)
            .await?,
        "signed"
    );
    assert_eq!(
        lab.outpoint_state(vault.id, &d2.tx.compute_txid(), 1)
            .await?,
        "signed"
    );
    let view = lab.vault(vault.id).await?;
    assert_eq!(view["collateral_sats"], "0");
    assert_eq!(view["utxos"].as_array().unwrap().len(), 0);

    // The change output went back to the vault. Once that Bitcoin
    // transaction confirms, it is proven like any other deposit.
    let withdrawal: Transaction = deserialize(&hex::decode(signed["raw_tx"].as_str().unwrap())?)?;
    let change = lab.mine(&vault, withdrawal, 1, MIN_CONFIRMATIONS).await?;
    let view = must(lab.prove(&lab.alice, &change.args).await?)?;
    assert_eq!(view["collateral_sats"], "29990000");
    Ok(())
}

/// The attack the signature ledger exists for.
///
/// Alice obtains a perfectly valid signature that sweeps her vault, keeps it
/// off-chain, and only then tries to use the same BTC as collateral. If the
/// contract accepted the deposit she could borrow, broadcast the old
/// signature, and leave the pool with nothing.
#[tokio::test]
async fn presigned_spend_can_never_become_collateral() -> anyhow::Result<()> {
    let lab = setup().await?;
    lab.supply(&lab.bob, 100_000 * USDC).await?;
    let vault = lab.mint_vault(&lab.alice).await?;

    // The BTC arrives and confirms on Bitcoin, but Alice does not register
    // it yet. First she has the contract sign a sweep to her own wallet.
    let funding = lab
        .mine(
            &vault,
            funding_tx(&vault.script, BTC, 1),
            1,
            MIN_CONFIRMATIONS,
        )
        .await?;
    let sweep_in = [(&funding, BTC)];
    let sweep_out = [(destination(), BTC - 5_000)];
    let drawer = must(
        lab.sign(&lab.alice, vault.id, &sweep_in, &sweep_out)
            .await?,
    )?;
    assert_valid_bitcoin_tx(&drawer, &vault, &sweep_in, &sweep_out);

    // Now she tries to pledge the same BTC.
    assert_fails(
        &lab.prove(&lab.alice, &funding.args).await?,
        "a signature spending this output was already issued",
    );
    let view = lab.vault(vault.id).await?;
    assert_eq!(view["collateral_sats"], "0");
    assert_fails(
        &lab.borrow(&lab.alice, vault.id, 1 * USDC).await?,
        "exceed the maximum loan-to-value",
    );
    assert_eq!(lab.usdc_of(lab.alice.id()).await?, 0);

    // Selling the vault does not launder it either: the ledger is keyed by
    // outpoint, not by owner.
    must(
        lab.transfer_nft(&lab.alice, lab.carol.id(), vault.id)
            .await?,
    )?;
    assert_fails(
        &lab.prove(&lab.carol, &funding.args).await?,
        "a signature spending this output was already issued",
    );

    // Fresh BTC sent to the same vault is unaffected.
    lab.deposit(&vault, BTC / 2, 2).await?;
    assert_eq!(
        lab.vault(vault.id).await?["collateral_sats"],
        (BTC / 2).to_string()
    );
    Ok(())
}

#[tokio::test]
async fn a_failed_mpc_request_still_marks_the_outpoint_as_signed() -> anyhow::Result<()> {
    let lab = setup().await?;
    let vault = lab.mint_vault(&lab.alice).await?;
    let d = lab.deposit(&vault, BTC, 1).await?;

    // The contract cannot know whether the MPC network produced a signature
    // before the request failed, so it assumes the worst.
    must(
        lab.signer
            .call("set_fail_signing")
            .args_json(json!({ "fail": true }))
            .transact()
            .await?,
    )?;
    let res = lab
        .sign(
            &lab.alice,
            vault.id,
            &[(&d, BTC)],
            &[(destination(), BTC - 1_000)],
        )
        .await?;
    assert_fails(&res, "did not return a signature");
    assert_eq!(
        lab.outpoint_state(vault.id, &d.tx.compute_txid(), 1)
            .await?,
        "signed"
    );
    assert_eq!(lab.vault(vault.id).await?["collateral_sats"], "0");

    // The owner can simply ask again once the network is back.
    must(
        lab.signer
            .call("set_fail_signing")
            .args_json(json!({ "fail": false }))
            .transact()
            .await?,
    )?;
    let inputs = [(&d, BTC)];
    let outputs = [(destination(), BTC - 1_000)];
    let signed = must(lab.sign(&lab.alice, vault.id, &inputs, &outputs).await?)?;
    assert_valid_bitcoin_tx(&signed, &vault, &inputs, &outputs);
    Ok(())
}

#[tokio::test]
async fn borrowing_locks_the_vault_and_repaying_unlocks_it() -> anyhow::Result<()> {
    let lab = setup().await?;
    lab.supply(&lab.bob, 200_000 * USDC).await?;
    let vault = lab.mint_vault(&lab.alice).await?;
    let d = lab.deposit(&vault, BTC, 1).await?;

    // 1 BTC at $100k with a 50% maximum LTV borrows at most $50k.
    assert_fails(
        &lab.borrow(&lab.bob, vault.id, 10 * USDC).await?,
        "only the vault owner can borrow",
    );
    assert_fails(
        &lab.borrow(&lab.alice, vault.id, 50_000 * USDC + 1).await?,
        "exceed the maximum loan-to-value",
    );
    must(lab.borrow(&lab.alice, vault.id, 50_000 * USDC).await?)?;
    assert_eq!(lab.usdc_of(lab.alice.id()).await?, 50_000 * USDC);

    let view = lab.vault(vault.id).await?;
    assert_eq!(view["locked"], true);
    assert_eq!(view["ltv_bps"], 5000);
    assert_eq!(view["liquidatable"], false);

    // While it secures a loan the vault cannot be sold or emptied.
    assert_fails(
        &lab.transfer_nft(&lab.alice, lab.carol.id(), vault.id)
            .await?,
        "repay it before transferring",
    );
    assert_fails(
        &lab.sign(
            &lab.alice,
            vault.id,
            &[(&d, BTC)],
            &[(destination(), BTC - 1_000)],
        )
        .await?,
        "above the maximum loan-to-value",
    );
    // The refused request left no trace in the ledger.
    assert_eq!(
        lab.outpoint_state(vault.id, &d.tx.compute_txid(), 1)
            .await?,
        "collateral"
    );

    // Anyone can repay. Carol pays most of it; the vault stays locked.
    must(
        lab.ft_call(
            &lab.carol,
            49_000 * USDC,
            json!({ "action": "repay", "vault_id": vault.id }),
        )
        .await?,
    )?;
    let view = lab.vault(vault.id).await?;
    assert_eq!(view["locked"], true);
    let debt = u(&view["debt"]);
    assert!(
        (1_000 * USDC..1_001 * USDC).contains(&debt),
        "about $1,000 left, got {debt}"
    );

    // Alice overpays; the excess comes straight back.
    let before = lab.usdc_of(lab.alice.id()).await?;
    must(
        lab.ft_call(
            &lab.alice,
            5_000 * USDC,
            json!({ "action": "repay", "vault_id": vault.id }),
        )
        .await?,
    )?;
    let paid = before - lab.usdc_of(lab.alice.id()).await?;
    assert!(
        (1_000 * USDC..1_001 * USDC).contains(&paid),
        "only the debt is taken, got {paid}"
    );
    let view = lab.vault(vault.id).await?;
    assert_eq!(view["debt"], "0");
    assert_eq!(view["locked"], false);

    // Unlocked: it can change hands, and the key follows the NFT.
    must(
        lab.transfer_nft(&lab.alice, lab.carol.id(), vault.id)
            .await?,
    )?;
    let inputs = [(&d, BTC)];
    let outputs = [(destination(), BTC - 1_000)];
    assert_fails(
        &lab.sign(&lab.alice, vault.id, &inputs, &outputs).await?,
        "only the vault owner can sign",
    );
    let signed = must(lab.sign(&lab.carol, vault.id, &inputs, &outputs).await?)?;
    assert_valid_bitcoin_tx(&signed, &vault, &inputs, &outputs);
    Ok(())
}

#[tokio::test]
async fn partial_withdrawal_is_allowed_while_the_loan_stays_safe() -> anyhow::Result<()> {
    let lab = setup().await?;
    lab.supply(&lab.bob, 200_000 * USDC).await?;
    let vault = lab.mint_vault(&lab.alice).await?;
    let d1 = lab.deposit(&vault, BTC / 2, 1).await?;
    let d2 = lab.deposit(&vault, BTC / 2, 2).await?;
    must(lab.borrow(&lab.alice, vault.id, 20_000 * USDC).await?)?;

    // Taking out half leaves 0.5 BTC ($50k) behind a $20k loan: 40% LTV.
    let inputs = [(&d1, BTC / 2)];
    let outputs = [(destination(), BTC / 2 - 1_000)];
    let signed = must(lab.sign(&lab.alice, vault.id, &inputs, &outputs).await?)?;
    assert_valid_bitcoin_tx(&signed, &vault, &inputs, &outputs);
    let view = lab.vault(vault.id).await?;
    assert_eq!(view["collateral_sats"], (BTC / 2).to_string());
    assert_eq!(view["ltv_bps"], 4000);

    // Taking out the rest would leave the loan unbacked.
    assert_fails(
        &lab.sign(
            &lab.alice,
            vault.id,
            &[(&d2, BTC / 2)],
            &[(destination(), BTC / 2 - 1_000)],
        )
        .await?,
        "above the maximum loan-to-value",
    );
    // Nor can the already-signed input be reused to sneak the second one out.
    assert_fails(
        &lab.sign(
            &lab.alice,
            vault.id,
            &[(&d1, BTC / 2), (&d2, BTC / 2)],
            &[(destination(), BTC - 1_000)],
        )
        .await?,
        "above the maximum loan-to-value",
    );
    assert_eq!(
        lab.outpoint_state(vault.id, &d2.tx.compute_txid(), 1)
            .await?,
        "collateral"
    );
    Ok(())
}

#[tokio::test]
async fn liquidation_hands_the_bitcoin_key_to_the_liquidator() -> anyhow::Result<()> {
    let lab = setup().await?;
    lab.supply(&lab.bob, 200_000 * USDC).await?;
    let vault = lab.mint_vault(&lab.alice).await?;
    let d = lab.deposit(&vault, BTC, 1).await?;
    must(lab.borrow(&lab.alice, vault.id, 50_000 * USDC).await?)?;

    // Healthy vaults cannot be liquidated; the payment is refunded in full.
    let carol_start = lab.usdc_of(lab.carol.id()).await?;
    let res = lab
        .ft_call(
            &lab.carol,
            100_000 * USDC,
            json!({ "action": "liquidate", "vault_id": vault.id }),
        )
        .await?;
    assert_fails(&res, "vault is healthy");
    assert_eq!(lab.usdc_of(lab.carol.id()).await?, carol_start);

    // BTC falls to $60k: a $50k loan on $60k of collateral is 83% LTV,
    // above the 75% liquidation threshold.
    lab.set_price(60_000 * USDC).await?;
    let view = lab.vault(vault.id).await?;
    assert_eq!(view["liquidatable"], true);
    let quote: Value = lab
        .app
        .view("liquidation_quote")
        .args_json(json!({ "vault_id": vault.id }))
        .await?
        .json()?;
    // The liquidator buys 1 BTC at 5% below the oracle price.
    assert_eq!(u(&quote["cost"]), 57_000 * USDC);
    let debt = u(&quote["debt"]);
    assert_eq!(u(&quote["to_pool"]), debt);
    assert_eq!(u(&quote["to_borrower"]), 57_000 * USDC - debt);
    assert_eq!(quote["bad_debt"], "0");

    // Alice cannot liquidate herself, and an underpayment is refused.
    must(
        lab.operator
            .call(lab.usdc.id(), "mint")
            .args_json(
                json!({ "account_id": lab.alice.id(), "amount": (10_000 * USDC).to_string() }),
            )
            .transact()
            .await?,
    )?;
    let res = lab
        .ft_call(
            &lab.alice,
            60_000 * USDC,
            json!({ "action": "liquidate", "vault_id": vault.id }),
        )
        .await?;
    assert_fails(&res, "repay your own vault");
    let res = lab
        .ft_call(
            &lab.carol,
            56_000 * USDC,
            json!({ "action": "liquidate", "vault_id": vault.id }),
        )
        .await?;
    assert_fails(&res, "liquidating this vault costs");
    assert_eq!(lab.usdc_of(lab.carol.id()).await?, carol_start);

    // Carol sends more than needed; exactly the cost is taken.
    let pool_before = lab.pool().await?;
    must(
        lab.ft_call(
            &lab.carol,
            80_000 * USDC,
            json!({ "action": "liquidate", "vault_id": vault.id }),
        )
        .await?,
    )?;
    let spent = carol_start - lab.usdc_of(lab.carol.id()).await?;
    assert!(
        (56_999 * USDC..=57_000 * USDC).contains(&spent),
        "carol paid about the quoted cost, got {spent}"
    );

    // The vault, and with it the Bitcoin key, is hers. The debt is gone.
    let view = lab.vault(vault.id).await?;
    assert_eq!(view["owner_id"], lab.carol.id().to_string());
    assert_eq!(view["debt"], "0");
    assert_eq!(view["locked"], false);
    assert_eq!(view["collateral_sats"], BTC.to_string());
    let inputs = [(&d, BTC)];
    let outputs = [(destination(), BTC - 2_000)];
    assert_fails(
        &lab.sign(&lab.alice, vault.id, &inputs, &outputs).await?,
        "only the vault owner can sign",
    );
    let signed = must(lab.sign(&lab.carol, vault.id, &inputs, &outputs).await?)?;
    assert_valid_bitcoin_tx(&signed, &vault, &inputs, &outputs);

    // Lenders were made whole: the pool holds all its assets in cash again.
    let pool_after = lab.pool().await?;
    assert_eq!(pool_after["total_debt"], "0");
    assert!(u(&pool_after["total_assets"]) >= u(&pool_before["total_assets"]));
    assert!(u(&pool_after["cash"]) >= 200_000 * USDC);

    // Alice keeps the loan and receives what her BTC fetched above the debt.
    let surplus: Value = lab
        .app
        .view("claimable_of")
        .args_json(json!({ "account_id": lab.alice.id() }))
        .await?
        .json()?;
    let surplus = u(&surplus);
    assert!(
        (6_990 * USDC..=7_000 * USDC).contains(&surplus),
        "about $7,000 surplus, got {surplus}"
    );
    let before = lab.usdc_of(lab.alice.id()).await?;
    must(
        lab.alice
            .call(lab.app.id(), "claim")
            .deposit(NearToken::from_yoctonear(1))
            .gas(Gas::from_tgas(100))
            .transact()
            .await?,
    )?;
    assert_eq!(lab.usdc_of(lab.alice.id()).await? - before, surplus);
    let res = lab
        .alice
        .call(lab.app.id(), "claim")
        .deposit(NearToken::from_yoctonear(1))
        .gas(Gas::from_tgas(100))
        .transact()
        .await?;
    assert_fails(&res, "nothing to claim");
    Ok(())
}

#[tokio::test]
async fn an_underwater_vault_is_liquidated_and_the_loss_is_shared_by_lenders() -> anyhow::Result<()>
{
    let lab = setup().await?;
    lab.supply(&lab.bob, 100_000 * USDC).await?;
    let vault = lab.mint_vault(&lab.alice).await?;
    lab.deposit(&vault, BTC, 1).await?;
    must(lab.borrow(&lab.alice, vault.id, 50_000 * USDC).await?)?;

    // A crash straight through the threshold: 1 BTC is now worth $40k.
    lab.set_price(40_000 * USDC).await?;
    let quote: Value = lab
        .app
        .view("liquidation_quote")
        .args_json(json!({ "vault_id": vault.id }))
        .await?
        .json()?;
    assert_eq!(u(&quote["cost"]), 38_000 * USDC);
    assert_eq!(quote["to_borrower"], "0");
    assert!(u(&quote["bad_debt"]) >= 12_000 * USDC);

    must(
        lab.ft_call(
            &lab.carol,
            38_000 * USDC,
            json!({ "action": "liquidate", "vault_id": vault.id }),
        )
        .await?,
    )?;
    assert_eq!(
        lab.vault(vault.id).await?["owner_id"],
        lab.carol.id().to_string()
    );

    // The pool lent 50k and recovered 38k; the share price reflects it.
    let pool = lab.pool().await?;
    assert_eq!(pool["total_debt"], "0");
    assert_eq!(u(&pool["cash"]), 88_000 * USDC);
    let supplied: Value = lab
        .app
        .view("supplied_of")
        .args_json(json!({ "account_id": lab.bob.id() }))
        .await?
        .json()?;
    assert!((87_999 * USDC..=88_000 * USDC).contains(&u(&supplied)));
    Ok(())
}

#[tokio::test]
async fn lenders_earn_the_interest_borrowers_pay() -> anyhow::Result<()> {
    let lab = setup().await?;
    lab.supply(&lab.bob, 100_000 * USDC).await?;
    let shares: Value = lab
        .app
        .view("shares_of")
        .args_json(json!({ "account_id": lab.bob.id() }))
        .await?
        .json()?;
    let vault = lab.mint_vault(&lab.alice).await?;
    lab.deposit(&vault, 2 * BTC, 1).await?;
    must(lab.borrow(&lab.alice, vault.id, 80_000 * USDC).await?)?;

    // 80% utilisation: 2% base + 80% of the 18% slope = 16.4% APR.
    let pool = lab.pool().await?;
    assert_eq!(pool["utilization_bps"], 8000);
    assert_eq!(pool["borrow_apr_bps"], 1640);
    assert_eq!(pool["supply_apr_bps"], 1312);

    // Not all of Bob's money is idle, so he cannot withdraw everything now.
    let res = lab
        .bob
        .call(lab.app.id(), "withdraw")
        .args_json(json!({ "shares": shares }))
        .deposit(NearToken::from_yoctonear(1))
        .gas(Gas::from_tgas(100))
        .transact()
        .await?;
    assert_fails(&res, "not enough idle liquidity");

    lab.worker.fast_forward(2_000).await?;
    lab.set_price(PRICE_100K).await?;
    let debt = u(&lab.vault(vault.id).await?["debt"]);
    assert!(debt > 80_000 * USDC, "interest accrued, debt is {debt}");

    // Alice repays in full (she needs a little extra to cover the interest).
    must(
        lab.operator
            .call(lab.usdc.id(), "mint")
            .args_json(
                json!({ "account_id": lab.alice.id(), "amount": (1_000 * USDC).to_string() }),
            )
            .transact()
            .await?,
    )?;
    must(
        lab.ft_call(
            &lab.alice,
            81_000 * USDC,
            json!({ "action": "repay", "vault_id": vault.id }),
        )
        .await?,
    )?;
    assert_eq!(lab.vault(vault.id).await?["locked"], false);

    // Bob withdraws everything and ends up with more than he put in. The
    // difference is what Alice paid, to the micro-USDC (minus rounding dust
    // that stays in the pool).
    let before = lab.usdc_of(lab.bob.id()).await?;
    must(
        lab.bob
            .call(lab.app.id(), "withdraw")
            .args_json(json!({ "shares": shares }))
            .deposit(NearToken::from_yoctonear(1))
            .gas(Gas::from_tgas(100))
            .transact()
            .await?,
    )?;
    let received = lab.usdc_of(lab.bob.id()).await? - before;
    assert!(
        received > 100_000 * USDC,
        "bob earned interest, got {received}"
    );
    let alice_paid = 81_000 * USDC - lab.usdc_of(lab.alice.id()).await?;
    let interest = alice_paid - 80_000 * USDC;
    let earned = received - 100_000 * USDC;
    assert!(
        earned <= interest && interest - earned <= 1_000,
        "interest {interest} vs earned {earned}"
    );
    let pool = lab.pool().await?;
    assert_eq!(pool["total_shares"], "0");
    assert!(u(&pool["cash"]) <= 1_000, "only dust remains");
    Ok(())
}

#[tokio::test]
async fn a_stale_price_blocks_borrowing_and_liquidation() -> anyhow::Result<()> {
    let lab = setup().await?;
    lab.supply(&lab.bob, 100_000 * USDC).await?;
    let vault = lab.mint_vault(&lab.alice).await?;
    lab.deposit(&vault, BTC, 1).await?;
    must(lab.borrow(&lab.alice, vault.id, 10_000 * USDC).await?)?;

    // Tighten the freshness window, then let the price age past it.
    let mut cfg: Value = lab.app.view("get_config").await?.json()?;
    cfg["max_price_age_sec"] = json!(5);
    assert_fails(
        &lab.alice
            .call(lab.app.id(), "update_config")
            .args_json(json!({ "config": cfg }))
            .transact()
            .await?,
        "only the operator",
    );
    must(
        lab.operator
            .call(lab.app.id(), "update_config")
            .args_json(json!({ "config": cfg }))
            .transact()
            .await?,
    )?;
    lab.worker.fast_forward(50).await?;

    assert_eq!(lab.app.view("is_price_fresh").await?.json::<bool>()?, false);
    assert_fails(
        &lab.borrow(&lab.alice, vault.id, 1_000 * USDC).await?,
        "BTC price is unavailable",
    );
    let res = lab
        .ft_call(
            &lab.carol,
            100_000 * USDC,
            json!({ "action": "liquidate", "vault_id": vault.id }),
        )
        .await?;
    assert_fails(&res, "BTC price is unavailable");
    // Repaying never needs a price.
    must(
        lab.ft_call(
            &lab.alice,
            10_000 * USDC,
            json!({ "action": "repay", "vault_id": vault.id }),
        )
        .await?,
    )?;

    // Only configured oracles can post prices; the immutable parts of the
    // configuration cannot be swapped out.
    let res = lab
        .alice
        .call(lab.app.id(), "report_price")
        .args_json(json!({ "usdc_per_btc": "1" }))
        .transact()
        .await?;
    assert_fails(&res, "caller is not an oracle");
    let mut evil = cfg.clone();
    evil["mpc_signer"] = json!(lab.alice.id());
    assert_fails(
        &lab.operator
            .call(lab.app.id(), "update_config")
            .args_json(json!({ "config": evil }))
            .transact()
            .await?,
        "immutable",
    );
    let mut reckless = cfg.clone();
    reckless["max_ltv_bps"] = json!(9000);
    assert_fails(
        &lab.operator
            .call(lab.app.id(), "update_config")
            .args_json(json!({ "config": reckless }))
            .transact()
            .await?,
        "max_ltv < liq_threshold",
    );
    Ok(())
}

#[tokio::test]
async fn payouts_to_unregistered_accounts_are_held_not_lost() -> anyhow::Result<()> {
    let lab = setup().await?;
    lab.supply(&lab.bob, 100_000 * USDC).await?;

    // Dave has never touched the stablecoin, so the token contract cannot
    // deliver his loan.
    let dave = sub(&lab.worker.root_account()?, "dave").await?;
    let vault = lab.mint_vault(&dave).await?;
    lab.deposit(&vault, BTC, 1).await?;
    let res = lab.borrow(&dave, vault.id, 10_000 * USDC).await?;
    assert_eq!(res.json::<bool>()?, false, "payout reported as deferred");
    let held: Value = lab
        .app
        .view("claimable_of")
        .args_json(json!({ "account_id": dave.id() }))
        .await?
        .json()?;
    assert_eq!(u(&held), 10_000 * USDC);
    // The debt is real (and already accruing) even though delivery waited.
    let debt = u(&lab.vault(vault.id).await?["debt"]);
    assert!(
        (10_000 * USDC..10_001 * USDC).contains(&debt),
        "debt is {debt}"
    );

    // After registering he collects it.
    must(
        lab.operator
            .call(lab.usdc.id(), "register_accounts")
            .args_json(json!({ "account_ids": [dave.id()] }))
            .transact()
            .await?,
    )?;
    must(
        dave.call(lab.app.id(), "claim")
            .deposit(NearToken::from_yoctonear(1))
            .gas(Gas::from_tgas(100))
            .transact()
            .await?,
    )?;
    assert_eq!(lab.usdc_of(dave.id()).await?, 10_000 * USDC);

    // Tokens other than the configured stablecoin are refused and refunded.
    let w = wasm().await;
    let other = lab.worker.dev_deploy(&w.ft).await?;
    other.call("new").args_json(json!({ "owner_id": lab.operator.id(), "name": "Other", "symbol": "OTH", "decimals": 6 })).transact().await?.into_result()?;
    must(
        lab.operator
            .call(other.id(), "register_accounts")
            .args_json(json!({ "account_ids": [lab.app.id()] }))
            .transact()
            .await?,
    )?;
    must(lab.bob.call(other.id(), "faucet").transact().await?)?;
    let res = lab
        .bob
        .call(other.id(), "ft_transfer_call")
        .args_json(json!({ "receiver_id": lab.app.id(), "amount": "1000000", "msg": r#"{"action":"supply"}"# }))
        .deposit(NearToken::from_yoctonear(1))
        .gas(Gas::from_tgas(300))
        .transact()
        .await?;
    assert_fails(&res, "only the configured stablecoin is accepted");
    let bal: Value = other
        .view("ft_balance_of")
        .args_json(json!({ "account_id": lab.bob.id() }))
        .await?
        .json()?;
    assert_eq!(u(&bal), 10_000 * USDC);
    Ok(())
}

/// The ledger must not become a weapon. A signature only matters for outputs
/// locked to the signing vault's key, so naming somebody else's deposit as an
/// input of your own withdrawal must not stop them from pledging it.
#[tokio::test]
async fn one_vault_cannot_burn_another_vaults_deposit() -> anyhow::Result<()> {
    let lab = setup().await?;
    let alices = lab.mint_vault(&lab.alice).await?;
    let mallorys = lab.mint_vault(&lab.carol).await?;

    // Alice's BTC is confirmed on Bitcoin but not registered yet. Mallory
    // sees it and has her own vault "sign" for that outpoint.
    let funding = lab
        .mine(
            &alices,
            funding_tx(&alices.script, BTC, 1),
            1,
            MIN_CONFIRMATIONS,
        )
        .await?;
    let stolen_in = [(&funding, BTC)];
    let out = [(destination(), BTC - 1_000)];
    let signed = must(lab.sign(&lab.carol, mallorys.id, &stolen_in, &out).await?)?;

    // What she got is a signature by *her* key, which Bitcoin would reject
    // for an output locked to Alice's key.
    let tx: Transaction = deserialize(&hex::decode(signed["raw_tx"].as_str().unwrap())?)?;
    assert_eq!(
        &tx.input[0].witness[1],
        mallorys.pubkey.to_bytes().as_slice()
    );
    assert_ne!(mallorys.pubkey, alices.pubkey);
    assert_eq!(
        lab.outpoint_state(mallorys.id, &funding.tx.compute_txid(), 1)
            .await?,
        "signed"
    );
    assert_eq!(
        lab.outpoint_state(alices.id, &funding.tx.compute_txid(), 1)
            .await?,
        "unknown"
    );

    // Alice's deposit is unaffected.
    let view = must(lab.prove(&lab.alice, &funding.args).await?)?;
    assert_eq!(view["collateral_sats"], BTC.to_string());
    assert_eq!(
        lab.outpoint_state(alices.id, &funding.tx.compute_txid(), 1)
            .await?,
        "collateral"
    );

    // And once it is collateral, Mallory cannot even name it.
    assert_fails(
        &lab.sign(&lab.carol, mallorys.id, &stolen_in, &out).await?,
        "collateral of another vault",
    );
    assert_eq!(
        lab.vault(alices.id).await?["collateral_sats"],
        BTC.to_string()
    );
    Ok(())
}

#[tokio::test]
async fn ledger_entries_are_paid_for_and_dust_cannot_clog_a_vault() -> anyhow::Result<()> {
    let lab = setup().await?;
    let vault = lab.mint_vault(&lab.alice).await?;
    let d1 = lab.deposit(&vault, BTC / 2, 1).await?;
    let d2 = lab.deposit(&vault, BTC / 2, 2).await?;

    // Every input leaves a permanent ledger entry; the caller pays for it.
    let inputs: Vec<Value> = [&d1, &d2]
        .iter()
        .map(|d| json!({ "txid": d.tx.compute_txid().to_string(), "vout": 1, "value": (BTC / 2).to_string() }))
        .collect();
    let outputs = json!([{ "script_pubkey": hex::encode(destination().as_bytes()), "value": (BTC - 1_000).to_string() }]);
    let res = lab
        .alice
        .call(lab.app.id(), "sign_withdrawal")
        .args_json(json!({ "vault_id": vault.id, "inputs": inputs, "outputs": outputs }))
        .deposit(LEDGER_FEE)
        .gas(Gas::from_tgas(300))
        .transact()
        .await?;
    assert_fails(&res, "the ledger fee for 2 input(s)");
    assert_eq!(
        lab.vault(vault.id).await?["collateral_sats"],
        BTC.to_string()
    );

    // A stranger cannot fill the vault's deposit slots with dust...
    let dust = lab
        .mine(
            &vault,
            funding_tx(&vault.script, 5_000, 3),
            1,
            MIN_CONFIRMATIONS,
        )
        .await?;
    assert_fails(
        &lab.prove(&lab.carol, &dust.args).await?,
        "only the vault owner can register a deposit this small",
    );
    // ...but the owner may register whatever they like.
    let view = must(lab.prove(&lab.alice, &dust.args).await?)?;
    assert_eq!(view["collateral_sats"], (BTC + 5_000).to_string());
    assert_eq!(view["utxos"].as_array().unwrap().len(), 3);
    Ok(())
}

#[tokio::test]
async fn no_single_reporter_controls_the_price() -> anyhow::Result<()> {
    let lab = setup().await?;
    lab.supply(&lab.bob, 100_000 * USDC).await?;
    let vault = lab.mint_vault(&lab.alice).await?;
    lab.deposit(&vault, BTC, 1).await?;

    // Two reporters at $100k: the price exists and comes from both.
    let p = lab.price().await?;
    assert_eq!(u(&p["usdc_per_btc"]), PRICE_100K);
    assert_eq!(p["sources"], 2);
    assert_eq!(p["spread_bps"], 0);

    // The third reporter lies, claiming BTC is worth $1m. The median of
    // three ignores it, but the reporters now disagree by far more than 2%,
    // so there is no price at all, and nobody can borrow on it.
    lab.report(2, 1_000_000 * USDC).await?;
    assert_eq!(lab.price().await?, Value::Null);
    let why: Option<String> = lab.app.view("price_error").await?.json()?;
    assert_eq!(why.as_deref(), Some("price reporters disagree"));
    assert_fails(&lab.borrow(&lab.alice, vault.id, 60_000 * USDC).await?, "price reporters disagree");

    // A liar inside the spread limit cannot move the median of three.
    lab.report(2, 101_900 * USDC).await?;
    assert_eq!(u(&lab.price().await?["usdc_per_btc"]), PRICE_100K);
    assert_eq!(lab.price().await?["sources"], 3);
    assert_fails(&lab.borrow(&lab.alice, vault.id, 50_000 * USDC + 1).await?, "maximum loan-to-value");
    must(lab.borrow(&lab.alice, vault.id, 50_000 * USDC).await?)?;

    // A reporter cannot vote twice: its new report replaces the old one.
    lab.report(2, 101_000 * USDC).await?;
    let reports: Vec<Value> = lab.app.view("get_price_reports").await?.json()?;
    assert_eq!(reports.len(), 3);
    assert!(reports.iter().all(|r| r["fresh"] == true));

    // Strangers cannot report at all.
    let res = lab.carol.call(lab.app.id(), "report_price").args_json(json!({ "usdc_per_btc": "1" })).transact().await?;
    assert_fails(&res, "caller is not an oracle");

    // The operator cannot configure a quorum larger than the reporter set,
    // nor list the same reporter twice to fake a quorum.
    let mut cfg: Value = lab.app.view("get_config").await?.json()?;
    cfg["min_oracle_reports"] = json!(4);
    assert_fails(
        &lab.operator.call(lab.app.id(), "update_config").args_json(json!({ "config": cfg.clone() })).transact().await?,
        "min_oracle_reports must be between",
    );
    cfg["min_oracle_reports"] = json!(2);
    cfg["oracle_ids"] = json!([lab.oracles[0].id(), lab.oracles[0].id()]);
    assert_fails(
        &lab.operator.call(lab.app.id(), "update_config").args_json(json!({ "config": cfg.clone() })).transact().await?,
        "duplicate oracle",
    );

    // Removing a reporter drops its report from the median immediately.
    cfg["oracle_ids"] = json!([lab.oracles[0].id(), lab.oracles[2].id()]);
    must(lab.operator.call(lab.app.id(), "update_config").args_json(json!({ "config": cfg })).transact().await?)?;
    let p = lab.price().await?;
    assert_eq!(p["sources"], 2);
    assert_eq!(u(&p["usdc_per_btc"]), 100_500 * USDC, "midpoint of $100k and $101k");
    Ok(())
}

/// The live testnet deployment holds real vaults. Upgrading it to the
/// multi-reporter oracle must keep every vault, deposit, loan, ledger entry
/// and lender position.
#[tokio::test]
async fn migration_from_the_single_oracle_version_keeps_all_state() -> anyhow::Result<()> {
    let lab = setup_with(true).await?;
    lab.supply(&lab.bob, 100_000 * USDC).await?;
    let vault = lab.mint_vault(&lab.alice).await?;
    let d1 = lab.deposit(&vault, BTC / 2, 1).await?;
    let d2 = lab.deposit(&vault, BTC / 2, 2).await?;
    must(lab.borrow(&lab.alice, vault.id, 20_000 * USDC).await?)?;
    // One output goes into the signature ledger (40% LTV left is fine).
    must(lab.sign(&lab.alice, vault.id, &[(&d1, BTC / 2)], &[(destination(), BTC / 2 - 1_000)]).await?)?;

    let before_vault = lab.vault(vault.id).await?;
    let before_pool = lab.pool().await?;
    let bob_shares: Value = lab.app.view("shares_of").args_json(json!({ "account_id": lab.bob.id() })).await?.json()?;

    // Upgrade the code and migrate the state in one transaction.
    let new_code = &wasm().await.app;
    let oracle_ids: Vec<&AccountId> = lab.oracles.iter().map(|o| o.id()).collect();
    let res = lab
        .app
        .as_account()
        .batch(lab.app.id())
        .deploy(new_code)
        .call(
            near_workspaces::operations::Function::new("migrate_to_multi_oracle")
                .args_json(json!({
                    "oracle_ids": oracle_ids,
                    "min_oracle_reports": 2,
                    "max_oracle_spread_bps": 200,
                    "max_price_age_sec": 3600,
                }))
                .gas(Gas::from_tgas(100)),
        )
        .transact()
        .await?;
    must(res)?;

    // Everything is where it was.
    let after_vault = lab.vault(vault.id).await?;
    for field in ["owner_id", "btc_address", "public_key", "utxos", "collateral_sats", "locked"] {
        assert_eq!(after_vault[field], before_vault[field], "vault field {field}");
    }
    assert!(u(&after_vault["debt"]) >= 20_000 * USDC);
    assert_eq!(after_vault["collateral_sats"], (BTC / 2).to_string());
    assert_eq!(lab.outpoint_state(vault.id, &d1.tx.compute_txid(), 1).await?, "signed");
    assert_eq!(lab.outpoint_state(vault.id, &d2.tx.compute_txid(), 1).await?, "collateral");
    let after_pool = lab.pool().await?;
    assert_eq!(after_pool["cash"], before_pool["cash"]);
    assert_eq!(after_pool["total_shares"], before_pool["total_shares"]);
    let shares_now: Value = lab.app.view("shares_of").args_json(json!({ "account_id": lab.bob.id() })).await?.json()?;
    assert_eq!(shares_now, bob_shares);
    let token: Value = lab.app.view("nft_token").args_json(json!({ "token_id": vault.id.to_string() })).await?.json()?;
    assert_eq!(token["owner_id"], lab.alice.id().to_string());

    // The old single price is gone: one reporter alone no longer suffices.
    assert_eq!(lab.price().await?, Value::Null);
    let res = lab.oracles[0].call(lab.app.id(), "set_price").args_json(json!({ "usdc_per_btc": "1" })).transact().await?;
    assert_fails(&res, "MethodNotFound");
    lab.report(0, PRICE_100K).await?;
    assert_fails(&lab.borrow(&lab.alice, vault.id, 1 * USDC).await?, "not enough fresh price reports");
    lab.report(1, PRICE_100K).await?;
    must(lab.borrow(&lab.alice, vault.id, 1 * USDC).await?)?;

    // Migration cannot be replayed by anyone, including the operator.
    let res = lab
        .operator
        .call(lab.app.id(), "migrate_to_multi_oracle")
        .args_json(json!({ "oracle_ids": [lab.operator.id()], "min_oracle_reports": 1, "max_oracle_spread_bps": 200, "max_price_age_sec": 3600 }))
        .gas(Gas::from_tgas(100))
        .transact()
        .await?;
    assert_fails(&res, "private");
    Ok(())
}
