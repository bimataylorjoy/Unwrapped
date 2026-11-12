//! Sandbox stand-in for the NEAR Chain Signatures contract.
//!
//! It exposes the two methods Unwrapped uses, with the same argument and
//! result shapes as `v1.signer`:
//!
//! - `derived_public_key({path, predecessor, domain_id})` -> `"secp256k1:<base58>"`
//! - `sign({request: {path, payload_v2: {Ecdsa: <hex>}, domain_id}})` ->
//!   `{"scheme":"Secp256k1","big_r":{"affine_point":..},"s":{"scalar":..},"recovery_id":..}`
//!
//! Like the real contract, the key is bound to `(caller, path)`: `sign` uses
//! the *predecessor* account, so only the contract that owns a path can sign
//! with it. Unlike the real contract the keys come from a public seed and the
//! answer is immediate. Tests only.

use k256::ecdsa::SigningKey;
use near_sdk::serde_json::{json, Value};
use near_sdk::{env, near, require, AccountId, CurveType, PanicOnDefault, PublicKey};

#[near(serializers = [json])]
pub struct SignRequest {
    pub path: String,
    pub payload_v2: Value,
    pub domain_id: Option<u64>,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct MockSigner {
    /// When set, `sign` fails. Lets tests exercise the failure path.
    fail_signing: bool,
    sign_count: u64,
}

fn derive_key(predecessor: &AccountId, path: &str) -> SigningKey {
    // Not the production derivation (which is additive on the MPC root key);
    // any deterministic (account, path) -> key mapping serves the tests.
    let seed = env::sha256_array(format!("unwrapped-mock-signer:{predecessor},{path}").as_bytes());
    SigningKey::from_slice(&seed).unwrap_or_else(|_| env::panic_str("unusable derived key"))
}

#[near]
impl MockSigner {
    #[init]
    pub fn new() -> Self {
        Self {
            fail_signing: false,
            sign_count: 0,
        }
    }

    pub fn derived_public_key(
        &self,
        path: String,
        predecessor: Option<AccountId>,
        domain_id: Option<u64>,
    ) -> PublicKey {
        let _ = domain_id;
        let predecessor = predecessor.unwrap_or_else(env::predecessor_account_id);
        let point = derive_key(&predecessor, &path)
            .verifying_key()
            .to_encoded_point(false);
        // Uncompressed SEC1 is 0x04 || x || y; NEAR keeps only x || y.
        PublicKey::from_parts(CurveType::SECP256K1, point.as_bytes()[1..].to_vec())
            .unwrap_or_else(|_| env::panic_str("invalid public key"))
    }

    #[payable]
    pub fn sign(&mut self, request: SignRequest) -> Value {
        require!(
            env::attached_deposit().as_yoctonear() >= 1,
            "attach 1 yoctoNEAR"
        );
        require!(!self.fail_signing, "signing disabled by test");
        let payload_hex = request
            .payload_v2
            .get("Ecdsa")
            .and_then(Value::as_str)
            .unwrap_or_else(|| env::panic_str("Payload is not Ecdsa"));
        let mut payload = [0u8; 32];
        hex::decode_to_slice(payload_hex, &mut payload)
            .unwrap_or_else(|_| env::panic_str("Ecdsa payload must be 32 bytes of hex"));

        let key = derive_key(&env::predecessor_account_id(), &request.path);
        let (signature, recovery_id) = key
            .sign_prehash_recoverable(&payload)
            .unwrap_or_else(|_| env::panic_str("signing failed"));
        let (r, s) = signature.split_bytes();
        let mut big_r = Vec::with_capacity(33);
        big_r.push(if recovery_id.is_y_odd() { 0x03 } else { 0x02 });
        big_r.extend_from_slice(&r);

        self.sign_count += 1;
        json!({
            "scheme": "Secp256k1",
            "big_r": { "affine_point": hex::encode(big_r) },
            "s": { "scalar": hex::encode(s) },
            "recovery_id": recovery_id.to_byte(),
        })
    }

    pub fn set_fail_signing(&mut self, fail: bool) {
        self.fail_signing = fail;
    }

    pub fn sign_count(&self) -> u64 {
        self.sign_count
    }
}
