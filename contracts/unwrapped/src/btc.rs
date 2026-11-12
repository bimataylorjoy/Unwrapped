//! Minimal Bitcoin primitives the contract needs, and nothing more.
//!
//! - parse a raw transaction and compute its txid (to check a deposit),
//! - compute the BIP143 sighash of a P2WPKH spend (so the contract signs only
//!   transactions it has built itself and knows every outpoint it authorised),
//! - assemble the signed witness transaction from MPC signatures,
//! - encode a P2WPKH address.
//!
//! Byte order: txids and block hashes are kept in *internal* order (the raw
//! double-SHA256 output). Hex strings at the API boundary use the reversed
//! *display* order that block explorers show.

use near_sdk::env;

/// Fixed transaction fields. Keeping them constant means a withdrawal is
/// fully described by its inputs and outputs.
pub const TX_VERSION: u32 = 2;
pub const TX_LOCKTIME: u32 = 0;
/// Signals BIP125 replace-by-fee, so a stuck withdrawal can be re-signed.
pub const TX_SEQUENCE: u32 = 0xffff_fffd;
pub const SIGHASH_ALL: u32 = 1;

pub type Hash256 = [u8; 32];

pub fn sha256(data: &[u8]) -> Hash256 {
    env::sha256_array(data)
}

pub fn dsha256(data: &[u8]) -> Hash256 {
    env::sha256_array(env::sha256_array(data))
}

pub fn hash160(data: &[u8]) -> [u8; 20] {
    env::ripemd160_array(env::sha256_array(data))
}

/// Hex in display order -> internal order.
pub fn hash_from_display_hex(s: &str) -> Result<Hash256, &'static str> {
    let mut out = [0u8; 32];
    hex::decode_to_slice(s, &mut out).map_err(|_| "expected 32 bytes of hex")?;
    out.reverse();
    Ok(out)
}

/// Internal order -> hex in display order.
pub fn hash_to_display_hex(h: &Hash256) -> String {
    let mut r = *h;
    r.reverse();
    hex::encode(r)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutPoint {
    pub txid: Hash256,
    pub vout: u32,
}

impl OutPoint {
    /// 36-byte wire encoding, also used as the storage key.
    pub fn key(&self) -> [u8; 36] {
        let mut k = [0u8; 36];
        k[..32].copy_from_slice(&self.txid);
        k[32..].copy_from_slice(&self.vout.to_le_bytes());
        k
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxOut {
    pub value: u64,
    pub script_pubkey: Vec<u8>,
}

#[derive(Debug)]
pub struct ParsedTx {
    pub txid: Hash256,
    pub outputs: Vec<TxOut>,
    /// Length of the serialization without witness data.
    pub stripped_len: usize,
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], &'static str> {
        let end = self.pos.checked_add(n).ok_or("length overflow")?;
        if end > self.buf.len() {
            return Err("transaction is truncated");
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, &'static str> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, &'static str> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, &'static str> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn varint(&mut self) -> Result<u64, &'static str> {
        Ok(match self.u8()? {
            0xfd => u16::from_le_bytes(self.take(2)?.try_into().unwrap()) as u64,
            0xfe => self.u32()? as u64,
            0xff => self.u64()?,
            n => n as u64,
        })
    }
    fn var_bytes(&mut self) -> Result<&'a [u8], &'static str> {
        let n = self.varint()?;
        // Cheap bound: nothing inside a transaction is longer than the buffer.
        if n > self.buf.len() as u64 {
            return Err("transaction is truncated");
        }
        self.take(n as usize)
    }
}

pub fn write_varint(out: &mut Vec<u8>, n: u64) {
    match n {
        0..=0xfc => out.push(n as u8),
        0xfd..=0xffff => {
            out.push(0xfd);
            out.extend_from_slice(&(n as u16).to_le_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(0xfe);
            out.extend_from_slice(&(n as u32).to_le_bytes());
        }
        _ => {
            out.push(0xff);
            out.extend_from_slice(&n.to_le_bytes());
        }
    }
}

/// Parses a raw transaction (legacy or segwit serialization) and returns its
/// txid and outputs. The txid is always computed over the serialization
/// *without* witness data, as Bitcoin defines it.
pub fn parse_tx(raw: &[u8]) -> Result<ParsedTx, &'static str> {
    let mut r = Reader { buf: raw, pos: 0 };
    let mut stripped: Vec<u8> = Vec::with_capacity(raw.len());

    stripped.extend_from_slice(r.take(4)?); // version

    // BIP144: marker 0x00 and flag 0x01 follow the version in segwit form.
    let segwit = raw.len() > 6 && raw[4] == 0x00 && raw[5] == 0x01;
    if segwit {
        r.take(2)?;
    }

    let body_start = r.pos;
    let n_in = r.varint()?;
    if n_in == 0 {
        return Err("transaction has no inputs");
    }
    for _ in 0..n_in {
        r.take(36)?; // outpoint
        r.var_bytes()?; // scriptSig
        r.take(4)?; // sequence
    }
    let n_out = r.varint()?;
    if n_out == 0 {
        return Err("transaction has no outputs");
    }
    let mut outputs = Vec::new();
    for _ in 0..n_out {
        let value = r.u64()?;
        let script_pubkey = r.var_bytes()?.to_vec();
        outputs.push(TxOut {
            value,
            script_pubkey,
        });
    }
    stripped.extend_from_slice(&raw[body_start..r.pos]);

    if segwit {
        for _ in 0..n_in {
            let items = r.varint()?;
            for _ in 0..items {
                r.var_bytes()?;
            }
        }
    }
    stripped.extend_from_slice(r.take(4)?); // locktime
    if r.pos != raw.len() {
        return Err("trailing bytes after transaction");
    }

    Ok(ParsedTx {
        txid: dsha256(&stripped),
        outputs,
        stripped_len: stripped.len(),
    })
}

/// scriptPubKey of a P2WPKH output: OP_0 PUSH20 <hash160(pubkey)>.
pub fn p2wpkh_script(pubkey_hash: &[u8; 20]) -> Vec<u8> {
    let mut s = Vec::with_capacity(22);
    s.push(0x00);
    s.push(0x14);
    s.extend_from_slice(pubkey_hash);
    s
}

/// Compresses a 64-byte (x || y) secp256k1 public key to SEC1 form.
pub fn compress_pubkey(xy: &[u8]) -> Result<[u8; 33], &'static str> {
    if xy.len() != 64 {
        return Err("expected a 64-byte secp256k1 public key");
    }
    let mut out = [0u8; 33];
    out[0] = if xy[63] & 1 == 0 { 0x02 } else { 0x03 };
    out[1..].copy_from_slice(&xy[..32]);
    Ok(out)
}

fn write_outputs(out: &mut Vec<u8>, outputs: &[TxOut]) {
    for o in outputs {
        out.extend_from_slice(&o.value.to_le_bytes());
        write_varint(out, o.script_pubkey.len() as u64);
        out.extend_from_slice(&o.script_pubkey);
    }
}

/// BIP143 signature hash for input `index` of a transaction whose inputs are
/// all P2WPKH outputs of the same key, signed with SIGHASH_ALL.
///
/// The digest commits to every input outpoint, the value being spent and
/// every output, so a signature is valid for exactly this transaction.
pub fn bip143_sighash(
    inputs: &[(OutPoint, u64)],
    outputs: &[TxOut],
    index: usize,
    pubkey_hash: &[u8; 20],
) -> Hash256 {
    let mut prevouts = Vec::with_capacity(inputs.len() * 36);
    let mut sequences = Vec::with_capacity(inputs.len() * 4);
    for (op, _) in inputs {
        prevouts.extend_from_slice(&op.key());
        sequences.extend_from_slice(&TX_SEQUENCE.to_le_bytes());
    }
    let mut outs = Vec::new();
    write_outputs(&mut outs, outputs);

    let (op, value) = &inputs[index];
    let mut pre = Vec::with_capacity(182);
    pre.extend_from_slice(&TX_VERSION.to_le_bytes());
    pre.extend_from_slice(&dsha256(&prevouts));
    pre.extend_from_slice(&dsha256(&sequences));
    pre.extend_from_slice(&op.key());
    // scriptCode of P2WPKH: the classic pay-to-pubkey-hash script.
    pre.extend_from_slice(&[0x19, 0x76, 0xa9, 0x14]);
    pre.extend_from_slice(pubkey_hash);
    pre.extend_from_slice(&[0x88, 0xac]);
    pre.extend_from_slice(&value.to_le_bytes());
    pre.extend_from_slice(&TX_SEQUENCE.to_le_bytes());
    pre.extend_from_slice(&dsha256(&outs));
    pre.extend_from_slice(&TX_LOCKTIME.to_le_bytes());
    pre.extend_from_slice(&SIGHASH_ALL.to_le_bytes());
    dsha256(&pre)
}

/// Serializes the transaction. With `witnesses` (one `(der_sig_with_hashtype,
/// pubkey)` pair per input) the result is the broadcastable segwit form;
/// without, it is the stripped form whose double-SHA256 is the txid.
pub fn serialize_tx(
    inputs: &[(OutPoint, u64)],
    outputs: &[TxOut],
    witnesses: Option<&[(Vec<u8>, [u8; 33])]>,
) -> Vec<u8> {
    let mut tx = Vec::new();
    tx.extend_from_slice(&TX_VERSION.to_le_bytes());
    if witnesses.is_some() {
        tx.extend_from_slice(&[0x00, 0x01]);
    }
    write_varint(&mut tx, inputs.len() as u64);
    for (op, _) in inputs {
        tx.extend_from_slice(&op.key());
        tx.push(0x00); // empty scriptSig
        tx.extend_from_slice(&TX_SEQUENCE.to_le_bytes());
    }
    write_varint(&mut tx, outputs.len() as u64);
    write_outputs(&mut tx, outputs);
    if let Some(ws) = witnesses {
        for (sig, pubkey) in ws {
            tx.push(0x02);
            write_varint(&mut tx, sig.len() as u64);
            tx.extend_from_slice(sig);
            tx.push(33);
            tx.extend_from_slice(pubkey);
        }
    }
    tx.extend_from_slice(&TX_LOCKTIME.to_le_bytes());
    tx
}

pub fn txid_of(inputs: &[(OutPoint, u64)], outputs: &[TxOut]) -> Hash256 {
    dsha256(&serialize_tx(inputs, outputs, None))
}

/// Order of the secp256k1 group, big-endian.
const SECP256K1_N: [u8; 32] = [
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe,
    0xba, 0xae, 0xdc, 0xe6, 0xaf, 0x48, 0xa0, 0x3b, 0xbf, 0xd2, 0x5e, 0x8c, 0xd0, 0x36, 0x41, 0x41,
];
/// N / 2, big-endian.
const SECP256K1_HALF_N: [u8; 32] = [
    0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0x5d, 0x57, 0x6e, 0x73, 0x57, 0xa4, 0x50, 0x1d, 0xdf, 0xe9, 0x2f, 0x46, 0x68, 0x1b, 0x20, 0xa0,
];

/// Bitcoin relays only signatures with `s <= N/2` (BIP146). Both `s` and
/// `N - s` are valid for the same message, so flipping is always safe.
pub fn normalize_s(s: &[u8; 32]) -> [u8; 32] {
    if s <= &SECP256K1_HALF_N {
        return *s;
    }
    let mut out = [0u8; 32];
    let mut borrow = 0i16;
    for i in (0..32).rev() {
        let d = SECP256K1_N[i] as i16 - s[i] as i16 - borrow;
        if d < 0 {
            out[i] = (d + 256) as u8;
            borrow = 1;
        } else {
            out[i] = d as u8;
            borrow = 0;
        }
    }
    out
}

fn der_int(out: &mut Vec<u8>, v: &[u8; 32]) {
    let first = v.iter().position(|b| *b != 0).unwrap_or(31);
    let body = &v[first..];
    let pad = body[0] & 0x80 != 0;
    out.push(0x02);
    out.push(body.len() as u8 + pad as u8);
    if pad {
        out.push(0x00);
    }
    out.extend_from_slice(body);
}

/// DER-encodes an ECDSA signature (low-S) and appends SIGHASH_ALL, which is
/// the form a P2WPKH witness expects.
pub fn witness_signature(r: &[u8; 32], s: &[u8; 32]) -> Vec<u8> {
    let s = normalize_s(s);
    let mut body = Vec::with_capacity(70);
    der_int(&mut body, r);
    der_int(&mut body, &s);
    let mut sig = Vec::with_capacity(body.len() + 3);
    sig.push(0x30);
    sig.push(body.len() as u8);
    sig.extend_from_slice(&body);
    sig.push(SIGHASH_ALL as u8);
    sig
}

const BECH32_CHARSET: &[u8; 32] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";

fn bech32_polymod(values: &[u8]) -> u32 {
    const GEN: [u32; 5] = [
        0x3b6a_57b2,
        0x2650_8e6d,
        0x1ea1_19fa,
        0x3d42_33dd,
        0x2a14_62b3,
    ];
    let mut chk: u32 = 1;
    for v in values {
        let top = chk >> 25;
        chk = ((chk & 0x01ff_ffff) << 5) ^ (*v as u32);
        for (i, g) in GEN.iter().enumerate() {
            if (top >> i) & 1 == 1 {
                chk ^= g;
            }
        }
    }
    chk
}

/// Bech32 (BIP173) address of a version-0 witness program: `bc1q…` on
/// mainnet, `tb1q…` on testnet.
pub fn p2wpkh_address(hrp: &str, pubkey_hash: &[u8; 20]) -> String {
    // Witness version 0, then the program regrouped from 8-bit to 5-bit.
    let mut data: Vec<u8> = vec![0];
    let (mut acc, mut bits) = (0u32, 0u32);
    for b in pubkey_hash {
        acc = (acc << 8) | *b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            data.push(((acc >> bits) & 31) as u8);
        }
    }
    if bits > 0 {
        data.push(((acc << (5 - bits)) & 31) as u8);
    }

    let mut values: Vec<u8> = hrp.bytes().map(|c| c >> 5).collect();
    values.push(0);
    values.extend(hrp.bytes().map(|c| c & 31));
    values.extend_from_slice(&data);
    values.extend_from_slice(&[0; 6]);
    let polymod = bech32_polymod(&values) ^ 1;

    let mut out = String::with_capacity(hrp.len() + 1 + data.len() + 6);
    out.push_str(hrp);
    out.push('1');
    for d in &data {
        out.push(BECH32_CHARSET[*d as usize] as char);
    }
    for i in 0..6 {
        out.push(BECH32_CHARSET[((polymod >> (5 * (5 - i))) & 31) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::absolute::LockTime;
    use bitcoin::consensus::{deserialize, serialize};
    use bitcoin::hashes::Hash;
    use bitcoin::secp256k1::{Message, Secp256k1, SecretKey};
    use bitcoin::sighash::{EcdsaSighashType, SighashCache};
    use bitcoin::transaction::Version;
    use bitcoin::{
        Address, Amount, CompressedPublicKey, Network, ScriptBuf, Sequence, Transaction, TxIn,
        Txid, Witness,
    };

    fn key() -> (SecretKey, CompressedPublicKey) {
        let secp = Secp256k1::new();
        let sk = SecretKey::from_slice(&[7u8; 32]).unwrap();
        let pk = CompressedPublicKey(sk.public_key(&secp));
        (sk, pk)
    }

    fn sample() -> (Vec<(OutPoint, u64)>, Vec<TxOut>) {
        let inputs = vec![
            (
                OutPoint {
                    txid: [0x11; 32],
                    vout: 0,
                },
                150_000,
            ),
            (
                OutPoint {
                    txid: [0x22; 32],
                    vout: 3,
                },
                70_000,
            ),
        ];
        let outputs = vec![
            TxOut {
                value: 200_000,
                script_pubkey: p2wpkh_script(&[0xab; 20]),
            },
            TxOut {
                value: 19_000,
                script_pubkey: vec![0x51, 0x20].into_iter().chain([0xcd; 32]).collect(),
            },
        ];
        (inputs, outputs)
    }

    fn reference_tx(inputs: &[(OutPoint, u64)], outputs: &[TxOut]) -> Transaction {
        Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: inputs
                .iter()
                .map(|(op, _)| TxIn {
                    previous_output: bitcoin::OutPoint {
                        txid: Txid::from_byte_array(op.txid),
                        vout: op.vout,
                    },
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence(TX_SEQUENCE),
                    witness: Witness::new(),
                })
                .collect(),
            output: outputs
                .iter()
                .map(|o| bitcoin::TxOut {
                    value: Amount::from_sat(o.value),
                    script_pubkey: ScriptBuf::from_bytes(o.script_pubkey.clone()),
                })
                .collect(),
        }
    }

    #[test]
    fn address_and_script_match_reference_implementation() {
        let (_, pk) = key();
        let h: [u8; 20] = hash160(&pk.to_bytes());
        assert_eq!(h, pk.wpubkey_hash().to_byte_array());
        assert_eq!(
            p2wpkh_script(&h),
            ScriptBuf::new_p2wpkh(&pk.wpubkey_hash()).to_bytes()
        );
        assert_eq!(
            p2wpkh_address("tb", &h),
            Address::p2wpkh(&pk, Network::Testnet).to_string()
        );
        assert_eq!(
            p2wpkh_address("bc", &h),
            Address::p2wpkh(&pk, Network::Bitcoin).to_string()
        );
    }

    #[test]
    fn bech32_matches_bip173_vector() {
        // BIP173 test vector for the P2WPKH program 751e76e8…
        let h: [u8; 20] = hex::decode("751e76e8199196d454941c45d1b3a323f1433bd6")
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(
            p2wpkh_address("bc", &h),
            "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4"
        );
        assert_eq!(
            p2wpkh_address("tb", &h),
            "tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx"
        );
    }

    #[test]
    fn compress_pubkey_matches_reference_implementation() {
        let (sk, pk) = key();
        let secp = Secp256k1::new();
        let uncompressed = sk.public_key(&secp).serialize_uncompressed();
        assert_eq!(compress_pubkey(&uncompressed[1..]).unwrap(), pk.to_bytes());
        assert!(compress_pubkey(&[0u8; 63]).is_err());
    }

    #[test]
    fn sighash_matches_reference_implementation() {
        let (_, pk) = key();
        let h = hash160(&pk.to_bytes());
        let (inputs, outputs) = sample();
        let tx = reference_tx(&inputs, &outputs);
        let script = ScriptBuf::new_p2wpkh(&pk.wpubkey_hash());
        let mut cache = SighashCache::new(&tx);
        for i in 0..inputs.len() {
            let expected = cache
                .p2wpkh_signature_hash(
                    i,
                    &script,
                    Amount::from_sat(inputs[i].1),
                    EcdsaSighashType::All,
                )
                .unwrap();
            assert_eq!(
                bip143_sighash(&inputs, &outputs, i, &h),
                expected.to_byte_array()
            );
        }
    }

    #[test]
    fn txid_and_serialization_match_reference_implementation() {
        let (inputs, outputs) = sample();
        let tx = reference_tx(&inputs, &outputs);
        assert_eq!(serialize_tx(&inputs, &outputs, None), serialize(&tx));
        assert_eq!(
            txid_of(&inputs, &outputs),
            tx.compute_txid().to_byte_array()
        );
        assert_eq!(
            hash_to_display_hex(&txid_of(&inputs, &outputs)),
            tx.compute_txid().to_string()
        );
    }

    #[test]
    fn signed_transaction_is_valid_bitcoin() {
        let secp = Secp256k1::new();
        let (sk, pk) = key();
        let h = hash160(&pk.to_bytes());
        let (inputs, outputs) = sample();

        let mut witnesses = Vec::new();
        for i in 0..inputs.len() {
            let digest = bip143_sighash(&inputs, &outputs, i, &h);
            let sig = secp
                .sign_ecdsa(&Message::from_digest(digest), &sk)
                .serialize_compact();
            let (r, s): ([u8; 32], [u8; 32]) =
                (sig[..32].try_into().unwrap(), sig[32..].try_into().unwrap());
            witnesses.push((witness_signature(&r, &s), pk.to_bytes()));
        }
        let raw = serialize_tx(&inputs, &outputs, Some(&witnesses));

        // The reference implementation decodes it and every witness verifies.
        let tx: Transaction = deserialize(&raw).expect("valid segwit transaction");
        assert_eq!(
            tx.compute_txid().to_byte_array(),
            txid_of(&inputs, &outputs)
        );
        let script = ScriptBuf::new_p2wpkh(&pk.wpubkey_hash());
        let mut cache = SighashCache::new(&tx);
        for i in 0..inputs.len() {
            let w = &tx.input[i].witness;
            assert_eq!(w.len(), 2);
            let sig =
                bitcoin::ecdsa::Signature::from_slice(&w[0]).expect("strict DER + sighash type");
            assert_eq!(sig.sighash_type, EcdsaSighashType::All);
            let digest = cache
                .p2wpkh_signature_hash(
                    i,
                    &script,
                    Amount::from_sat(inputs[i].1),
                    EcdsaSighashType::All,
                )
                .unwrap();
            secp.verify_ecdsa(
                &Message::from_digest(digest.to_byte_array()),
                &sig.signature,
                &pk.0,
            )
            .expect("signature verifies");
        }

        // And our own parser round-trips what we produced.
        let parsed = parse_tx(&raw).unwrap();
        assert_eq!(parsed.txid, txid_of(&inputs, &outputs));
        assert_eq!(parsed.outputs, outputs);
    }

    #[test]
    fn high_s_is_flipped_to_low_s() {
        let low = {
            let mut s = [0u8; 32];
            s[31] = 5;
            s
        };
        assert_eq!(normalize_s(&low), low);
        assert_eq!(normalize_s(&SECP256K1_HALF_N), SECP256K1_HALF_N);
        // N - 5 is high; it must come back as 5.
        let mut high = SECP256K1_N;
        high[31] -= 5;
        assert_eq!(normalize_s(&high), low);
        // DER of a high-bit integer gets a leading zero.
        let sig = witness_signature(&[0x80; 32], &low);
        assert_eq!(sig[..5], [0x30, 0x26, 0x02, 0x21, 0x00]);
        assert_eq!(sig[sig.len() - 4..], [0x02, 0x01, 0x05, 0x01]);
    }

    #[test]
    fn parser_handles_legacy_and_segwit_and_rejects_garbage() {
        // A real mainnet segwit transaction (BIP143 example, P2WPKH spend).
        let segwit = hex::decode("01000000000102fff7f7881a8099afa6940d42d1e7f6362bec38171ea3edf433541db4e4ad969f00000000494830450221008b9d1dc26ba6a9cb62127b02742fa9d754cd3bebf337f7a55d114c8e5cdd30be022040529b194ba3f9281a99f2b1c0a19c0489bc22ede944ccf4ecbab4cc618ef3ed01eeffffffef51e1b804cc89d182d279655c3aa89e815b1b309fe287d9b2b55d57b90ec68a0100000000ffffffff02202cb206000000001976a9148280b37df378db99f66f85c95a783a76ac7a6d5988ac9093510d000000001976a9143bde42dbee7e4dbe6a21b2d50ce2f0167faa815988ac000247304402203609e17b84f6a7d30c80bfa610b5b4542f32a8a0d5447a12fb1366d7f01cc44a0220573a954c4518331561406f90300e8f3358f51928d43c212a8caed02de67eebee0121025476c2e83188368da1ff3e292e7acafcdb3566bb0ad253f62fc70f07aeee635711000000").unwrap();
        let reference: Transaction = deserialize(&segwit).unwrap();
        let parsed = parse_tx(&segwit).unwrap();
        assert_eq!(parsed.txid, reference.compute_txid().to_byte_array());
        assert_eq!(parsed.outputs.len(), 2);
        assert_eq!(parsed.outputs[0].value, 112_340_000);
        assert_eq!(
            parsed.outputs[1].script_pubkey,
            reference.output[1].script_pubkey.to_bytes()
        );

        // The same transaction without witness data has the same txid.
        let mut stripped_tx = reference.clone();
        for i in stripped_tx.input.iter_mut() {
            i.witness = Witness::new();
        }
        let legacy = serialize(&stripped_tx);
        assert_eq!(parse_tx(&legacy).unwrap().txid, parsed.txid);
        assert_eq!(parse_tx(&legacy).unwrap().stripped_len, legacy.len());

        assert!(parse_tx(&[]).is_err());
        assert!(parse_tx(&segwit[..segwit.len() - 1]).is_err());
        let mut trailing = segwit.clone();
        trailing.push(0);
        assert!(parse_tx(&trailing).is_err());
    }

    #[test]
    fn display_hex_round_trips() {
        let s = "000000000000000000024bead8df69990852c202db0e0097c1a12ea637d7e96d";
        let h = hash_from_display_hex(s).unwrap();
        assert_eq!(h[31], 0x00);
        assert_eq!(h[0], 0x6d);
        assert_eq!(hash_to_display_hex(&h), s);
        assert!(hash_from_display_hex("abcd").is_err());
    }
}
