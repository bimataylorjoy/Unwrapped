//! Sandbox stand-in for the NEAR Bitcoin light client
//! (github.com/Near-One/btc-light-client-contract).
//!
//! The production contract follows Bitcoin's proof-of-work chain. This one
//! lets a test register `(block hash, merkle root, height)` directly, then
//! answers `verify_transaction_inclusion_v2` with the same arguments, the
//! same merkle and coinbase checks and the same confirmation rule. Proofs
//! are therefore verified for real; only the source of headers is mocked.

use near_sdk::store::LookupMap;
use near_sdk::{env, near, require, PanicOnDefault};

type H256 = [u8; 32];

#[near(serializers = [borsh])]
pub struct ProofArgsV2 {
    pub tx_id: H256,
    pub tx_block_blockhash: H256,
    pub tx_index: u64,
    pub merkle_proof: Vec<H256>,
    pub coinbase_tx_id: H256,
    pub coinbase_merkle_proof: Vec<H256>,
    pub confirmations: u64,
}

#[near(serializers = [borsh])]
pub struct Header {
    merkle_root: H256,
    height: u64,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct MockBtcClient {
    headers: LookupMap<H256, Header>,
    tip_height: u64,
}

fn from_display_hex(s: &str) -> H256 {
    let mut out = [0u8; 32];
    hex::decode_to_slice(s, &mut out)
        .unwrap_or_else(|_| env::panic_str("expected 32 bytes of hex"));
    out.reverse();
    out
}

fn merkle_root(leaf: H256, mut position: u64, proof: &[H256]) -> H256 {
    let mut current = leaf;
    for sibling in proof {
        let mut buf = [0u8; 64];
        if position % 2 == 0 {
            buf[..32].copy_from_slice(&current);
            buf[32..].copy_from_slice(sibling);
        } else {
            buf[..32].copy_from_slice(sibling);
            buf[32..].copy_from_slice(&current);
        }
        current = env::sha256_array(env::sha256_array(buf));
        position /= 2;
    }
    current
}

#[near]
impl MockBtcClient {
    #[init]
    pub fn new() -> Self {
        Self {
            headers: LookupMap::new(b"h"),
            tip_height: 0,
        }
    }

    /// Registers a block header. Hashes are hex in block-explorer order.
    pub fn add_block(&mut self, block_hash: String, merkle_root: String, height: u64) {
        self.headers.insert(
            from_display_hex(&block_hash),
            Header {
                merkle_root: from_display_hex(&merkle_root),
                height,
            },
        );
        if height > self.tip_height {
            self.tip_height = height;
        }
    }

    /// Moves the chain tip, i.e. adds confirmations on top of earlier blocks.
    pub fn set_tip_height(&mut self, height: u64) {
        self.tip_height = height;
    }

    pub fn get_last_block_height(&self) -> u64 {
        self.tip_height
    }

    pub fn verify_transaction_inclusion_v2(&self, #[serializer(borsh)] args: ProofArgsV2) -> bool {
        require!(
            args.merkle_proof.len() == args.coinbase_merkle_proof.len(),
            "Coinbase merkle proof and transaction merkle proof should have the same length"
        );
        let header = self
            .headers
            .get(&args.tx_block_blockhash)
            .unwrap_or_else(|| env::panic_str("block does not belong to the current main chain"));
        require!(
            merkle_root(args.coinbase_tx_id, 0, &args.coinbase_merkle_proof) == header.merkle_root,
            "Incorrect coinbase merkle proof"
        );
        require!(!args.merkle_proof.is_empty(), "Merkle proof is empty");
        if merkle_root(args.tx_id, args.tx_index, &args.merkle_proof) != header.merkle_root {
            return false;
        }
        require!(
            self.tip_height.saturating_sub(header.height) + 1 >= args.confirmations,
            "Not enough blocks confirmed"
        );
        true
    }
}
