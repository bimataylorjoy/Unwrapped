// Bitcoin side of the client: read the chain through an Esplora API, build
// the SPV proof a vault deposit needs, and plan withdrawals.
//
// Runs unchanged in Node and in the browser (fetch + pure-JS hashing).
// Hashes are hex strings in block-explorer byte order throughout.

import { secp256k1 } from "@noble/curves/secp256k1.js";
import { sha256 } from "@noble/hashes/sha2";
import { base58, bech32, bech32m, hex } from "@scure/base";

export type BtcNetwork = "testnet" | "mainnet";

export const ESPLORA_URLS: Record<BtcNetwork, string[]> = {
  testnet: ["https://mempool.space/testnet/api", "https://blockstream.info/testnet/api"],
  mainnet: ["https://mempool.space/api", "https://blockstream.info/api"],
};

export interface EsploraTx {
  txid: string;
  vout: { scriptpubkey: string; value: number }[];
  status: { confirmed: boolean; block_height?: number; block_hash?: string };
}

export interface EsploraUtxo {
  txid: string;
  vout: number;
  value: number;
  status: { confirmed: boolean; block_height?: number; block_hash?: string };
}

/** Minimal Esplora client with failover across public endpoints. */
export class Esplora {
  constructor(readonly urls: string[]) {
    if (urls.length === 0) throw new Error("at least one Esplora URL is required");
  }

  static for(network: BtcNetwork): Esplora {
    return new Esplora(ESPLORA_URLS[network]);
  }

  private async request(path: string, init?: RequestInit): Promise<string> {
    let last: unknown;
    for (const base of this.urls) {
      try {
        const res = await fetch(base + path, init);
        const text = await res.text();
        if (res.ok) return text;
        last = new Error(`${base}${path}: HTTP ${res.status} ${text.slice(0, 200)}`);
        // A 4xx is an answer, not an outage: do not ask the next server.
        if (res.status >= 400 && res.status < 500 && res.status !== 429) break;
      } catch (e) {
        last = e;
      }
    }
    throw last instanceof Error ? last : new Error(String(last));
  }

  async tx(txid: string): Promise<EsploraTx> {
    return JSON.parse(await this.request(`/tx/${txid}`));
  }

  txHex(txid: string): Promise<string> {
    return this.request(`/tx/${txid}/hex`);
  }

  async blockTxids(blockHash: string): Promise<string[]> {
    return JSON.parse(await this.request(`/block/${blockHash}/txids`));
  }

  async tipHeight(): Promise<number> {
    return Number(await this.request("/blocks/tip/height"));
  }

  async utxos(address: string): Promise<EsploraUtxo[]> {
    return JSON.parse(await this.request(`/address/${address}/utxo`));
  }

  /** Fee rate in sat/vB for confirmation within about `blocks` blocks. */
  async feeRate(blocks = 3): Promise<number> {
    try {
      const est = JSON.parse(await this.request("/fee-estimates")) as Record<string, number>;
      const rate = est[String(blocks)] ?? Object.values(est)[0];
      return Math.max(1, Math.ceil(rate ?? 1));
    } catch {
      return 2;
    }
  }

  /** Broadcasts a raw transaction and returns its txid. */
  broadcast(rawTxHex: string): Promise<string> {
    return this.request("/tx", { method: "POST", body: rawTxHex });
  }
}

function sha256d(data: Uint8Array): Uint8Array {
  return sha256(sha256(data));
}

/** Display-order hex <-> internal byte order. */
function toInternal(displayHex: string): Uint8Array {
  return hex.decode(displayHex).reverse();
}
function toDisplay(internal: Uint8Array): string {
  return hex.encode(Uint8Array.from(internal).reverse());
}

/**
 * Bitcoin merkle branch for the transaction at `index`: the sibling hash at
 * every level from the leaf up to (not including) the root. An odd level
 * pairs its last node with itself, as Bitcoin does.
 */
export function merkleProof(txids: string[], index: number): { proof: string[]; root: string } {
  if (index < 0 || index >= txids.length) throw new Error("transaction index out of range");
  let level = txids.map(toInternal);
  let pos = index;
  const proof: string[] = [];
  while (level.length > 1) {
    if (level.length % 2 === 1) level.push(level[level.length - 1]!);
    proof.push(toDisplay(level[pos ^ 1]!));
    pos >>= 1;
    const next: Uint8Array[] = [];
    for (let i = 0; i < level.length; i += 2) {
      const pair = new Uint8Array(64);
      pair.set(level[i]!, 0);
      pair.set(level[i + 1]!, 32);
      next.push(sha256d(pair));
    }
    level = next;
  }
  return { proof, root: toDisplay(level[0]!) };
}

/** Arguments of the contract's `prove_deposit`, minus `vault_id`. */
export interface DepositProof {
  tx_hex: string;
  vout: number;
  block_hash: string;
  tx_index: number;
  merkle_proof: string[];
  coinbase_tx_id: string;
  coinbase_merkle_proof: string[];
}

export interface DepositInfo {
  txid: string;
  vout: number;
  value: number;
  confirmations: number;
  blockHeight: number;
  proof: DepositProof;
}

export class DepositNotReady extends Error {}

/**
 * Builds the SPV proof for the output of `txid` that pays `vaultScriptHex`.
 * Throws `DepositNotReady` while the transaction is unconfirmed.
 */
export async function buildDepositProof(
  esplora: Esplora,
  txid: string,
  vaultScriptHex: string,
  vout?: number,
): Promise<DepositInfo> {
  const tx = await esplora.tx(txid);
  const index =
    vout ?? tx.vout.findIndex((o) => o.scriptpubkey.toLowerCase() === vaultScriptHex.toLowerCase());
  const output = tx.vout[index];
  if (!output || output.scriptpubkey.toLowerCase() !== vaultScriptHex.toLowerCase()) {
    throw new Error(`transaction ${txid} has no output paying this vault`);
  }
  if (!tx.status.confirmed || !tx.status.block_hash || tx.status.block_height === undefined) {
    throw new DepositNotReady(`transaction ${txid} is not confirmed yet`);
  }

  const [txHex, txids, tip] = await Promise.all([
    esplora.txHex(txid),
    esplora.blockTxids(tx.status.block_hash),
    esplora.tipHeight(),
  ]);
  const txIndex = txids.indexOf(txid);
  if (txIndex < 0) throw new Error("the block does not list this transaction (reorg?)");
  if (txids.length < 2) throw new Error("a block with a single transaction cannot contain a deposit");

  return {
    txid,
    vout: index,
    value: output.value,
    confirmations: tip - tx.status.block_height + 1,
    blockHeight: tx.status.block_height,
    proof: {
      tx_hex: txHex.trim(),
      vout: index,
      block_hash: tx.status.block_hash,
      tx_index: txIndex,
      merkle_proof: merkleProof(txids, txIndex).proof,
      coinbase_tx_id: txids[0]!,
      coinbase_merkle_proof: merkleProof(txids, 0).proof,
    },
  };
}

const BASE58_VERSIONS: Record<BtcNetwork, { p2pkh: number; p2sh: number; hrp: string }> = {
  mainnet: { p2pkh: 0x00, p2sh: 0x05, hrp: "bc" },
  testnet: { p2pkh: 0x6f, p2sh: 0xc4, hrp: "tb" },
};

/** scriptPubKey (hex) of a Bitcoin address: P2PKH, P2SH, P2WPKH, P2WSH or P2TR. */
export function addressToScript(address: string, network: BtcNetwork): string {
  const v = BASE58_VERSIONS[network];
  const lower = address.toLowerCase();
  if (lower.startsWith(v.hrp + "1")) {
    const witnessVersion = "qpzry9x8gf2tvdw0s3jn54khce6mua7l".indexOf(lower[v.hrp.length + 1]!);
    if (witnessVersion < 0 || witnessVersion > 16) throw new Error("invalid witness version");
    const codec = witnessVersion === 0 ? bech32 : bech32m;
    const { prefix, words } = codec.decode(lower as `${string}1${string}`);
    if (prefix !== v.hrp) throw new Error(`address is not for ${network}`);
    const program = codec.fromWords(words.slice(1));
    const okLength = witnessVersion === 0 ? program.length === 20 || program.length === 32 : program.length >= 2 && program.length <= 40;
    if (!okLength) throw new Error("invalid witness program length");
    const opcode = witnessVersion === 0 ? 0x00 : 0x50 + witnessVersion;
    return hex.encode(Uint8Array.from([opcode, program.length, ...program]));
  }

  let raw: Uint8Array;
  try {
    raw = base58.decode(address);
  } catch {
    throw new Error("unrecognised Bitcoin address");
  }
  if (raw.length !== 25) throw new Error("unrecognised Bitcoin address");
  const payload = raw.subarray(0, 21);
  const checksum = sha256d(payload).subarray(0, 4);
  if (!checksum.every((b, i) => b === raw[21 + i])) throw new Error("bad address checksum");
  const hash = payload.subarray(1);
  if (payload[0] === v.p2pkh) return hex.encode(Uint8Array.from([0x76, 0xa9, 0x14, ...hash, 0x88, 0xac]));
  if (payload[0] === v.p2sh) return hex.encode(Uint8Array.from([0xa9, 0x14, ...hash, 0x87]));
  throw new Error(`address is not for ${network}`);
}

export interface WithdrawalInput {
  txid: string;
  vout: number;
  /** Satoshis, as a decimal string (the contract's JSON format). */
  value: string;
}

export interface WithdrawalOutput {
  script_pubkey: string;
  value: string;
}

export interface WithdrawalPlan {
  inputs: WithdrawalInput[];
  outputs: WithdrawalOutput[];
  feeSats: number;
  vsize: number;
}

/** Outputs below this are not relayed; smaller change is left to the fee. */
export const DUST_SATS = 546;

/** Virtual size of a transaction with `nIn` P2WPKH inputs and the given outputs. */
export function estimateVsize(nIn: number, outputScripts: string[]): number {
  const outputs = outputScripts.reduce((sum, s) => sum + 8 + 1 + s.length / 2, 0);
  // version+locktime+counts (10) + segwit marker/flag (0.5) + inputs
  // (41 non-witness + ~27 witness vbytes each).
  return Math.ceil(10.5 + nIn * 68 + outputs);
}

/**
 * Plans a withdrawal of `amountSats` (or everything, with `"max"`) from the
 * given UTXOs to `destinationScript`. Change returns to `changeScript`, the
 * vault's own address, where it can be proven again as collateral.
 */
export function planWithdrawal(
  utxos: { txid: string; vout: number; value: number }[],
  destinationScript: string,
  amountSats: number | "max",
  changeScript: string,
  feeRate: number,
): WithdrawalPlan {
  if (utxos.length === 0) throw new Error("the vault has no spendable outputs");
  const sorted = [...utxos].sort((a, b) => b.value - a.value);
  const toInput = (u: (typeof sorted)[number]): WithdrawalInput => ({ txid: u.txid, vout: u.vout, value: String(u.value) });

  if (amountSats === "max") {
    const picked = sorted.slice(0, 8);
    const total = picked.reduce((s, u) => s + u.value, 0);
    const vsize = estimateVsize(picked.length, [destinationScript]);
    const fee = Math.ceil(vsize * feeRate);
    if (total - fee < DUST_SATS) throw new Error("balance is too small to cover the network fee");
    return {
      inputs: picked.map(toInput),
      outputs: [{ script_pubkey: destinationScript, value: String(total - fee) }],
      feeSats: fee,
      vsize,
    };
  }

  if (!Number.isSafeInteger(amountSats) || amountSats < DUST_SATS) throw new Error(`amount must be at least ${DUST_SATS} sats`);
  const picked: typeof sorted = [];
  let total = 0;
  for (const u of sorted) {
    if (picked.length === 8) break;
    picked.push(u);
    total += u.value;
    const withChange = estimateVsize(picked.length, [destinationScript, changeScript]);
    const feeWithChange = Math.ceil(withChange * feeRate);
    const change = total - amountSats - feeWithChange;
    if (change >= DUST_SATS) {
      return {
        inputs: picked.map(toInput),
        outputs: [
          { script_pubkey: destinationScript, value: String(amountSats) },
          { script_pubkey: changeScript, value: String(change) },
        ],
        feeSats: feeWithChange,
        vsize: withChange,
      };
    }
    const noChange = estimateVsize(picked.length, [destinationScript]);
    const feeNoChange = Math.ceil(noChange * feeRate);
    if (total - amountSats >= feeNoChange) {
      // Too little left for a change output: it goes to the miners.
      return {
        inputs: picked.map(toInput),
        outputs: [{ script_pubkey: destinationScript, value: String(amountSats) }],
        feeSats: total - amountSats,
        vsize: noChange,
      };
    }
  }
  throw new Error("not enough BTC in the vault for this amount plus the network fee");
}

function concat(...parts: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
}
function le32(x: number): Uint8Array {
  const b = new Uint8Array(4);
  new DataView(b.buffer).setUint32(0, x, true);
  return b;
}
function le64(x: bigint): Uint8Array {
  const b = new Uint8Array(8);
  new DataView(b.buffer).setBigUint64(0, x, true);
  return b;
}

/** Sequence number the contract puts on every input (BIP125 replaceable). */
const SEQUENCE = 0xfffffffd;

/**
 * Independently checks a transaction returned by `sign_withdrawal`: it
 * recomputes each BIP143 digest from the plan and verifies every witness
 * signature against the vault's public key. Nothing is taken on trust from
 * the contract except the raw bytes being checked.
 */
export function verifyWithdrawal(
  rawTxHex: string,
  plan: Pick<WithdrawalPlan, "inputs" | "outputs">,
  vault: { public_key: string; script_pubkey: string },
): { pubkeyMatches: boolean; signaturesValid: boolean } {
  const raw = hex.decode(rawTxHex);
  const outpoint = (i: WithdrawalInput) => concat(toInternal(i.txid), le32(i.vout));
  const hashPrevouts = sha256d(concat(...plan.inputs.map(outpoint)));
  const hashSequence = sha256d(concat(...plan.inputs.map(() => le32(SEQUENCE))));
  const serializedOutputs = concat(
    ...plan.outputs.map((o) => {
      const script = hex.decode(o.script_pubkey);
      return concat(le64(BigInt(o.value)), Uint8Array.of(script.length), script);
    }),
  );
  const hashOutputs = sha256d(serializedOutputs);
  const pubkeyHash = hex.decode(vault.script_pubkey).subarray(2);

  // Layout: version, marker+flag, input count, inputs (41 bytes each, empty
  // scriptSig), output count, outputs, then one witness stack per input.
  let at = 4 + 2 + 1 + 41 * plan.inputs.length + 1 + serializedOutputs.length;
  let pubkeyMatches = true;
  let signaturesValid = true;
  for (const input of plan.inputs) {
    if (raw[at] !== 2) return { pubkeyMatches: false, signaturesValid: false };
    const sigLen = raw[at + 1]!;
    const der = raw.subarray(at + 2, at + 2 + sigLen - 1); // drop the sighash byte
    const pubkey = raw.subarray(at + 2 + sigLen + 1, at + 2 + sigLen + 34);
    at += 2 + sigLen + 34;
    const preimage = concat(
      le32(2),
      hashPrevouts,
      hashSequence,
      outpoint(input),
      Uint8Array.of(0x19, 0x76, 0xa9, 0x14),
      pubkeyHash,
      Uint8Array.of(0x88, 0xac),
      le64(BigInt(input.value)),
      le32(SEQUENCE),
      hashOutputs,
      le32(0),
      le32(1),
    );
    if (hex.encode(pubkey) !== vault.public_key) pubkeyMatches = false;
    let ok = false;
    try {
      ok = secp256k1.verify(der, sha256d(preimage), pubkey, { prehash: false, format: "der", lowS: true });
    } catch {
      ok = false;
    }
    if (!ok) signaturesValid = false;
  }
  return { pubkeyMatches, signaturesValid };
}

/** Where each price reporter reads BTC/USD. Each source is a different exchange. */
export const PRICE_SOURCES: Record<string, { url: string; pick: (json: any) => number }> = {
  coinbase: { url: "https://api.coinbase.com/v2/prices/BTC-USD/spot", pick: (j) => Number(j.data.amount) },
  kraken: { url: "https://api.kraken.com/0/public/Ticker?pair=XBTUSD", pick: (j) => Number(j.result.XXBTZUSD.c[0]) },
  bitstamp: { url: "https://www.bitstamp.net/api/v2/ticker/btcusd/", pick: (j) => Number(j.last) },
  gemini: { url: "https://api.gemini.com/v1/pubticker/btcusd", pick: (j) => Number(j.last) },
};

/** BTC price in USD from one named exchange. */
export async function fetchBtcUsd(source: string): Promise<number> {
  const s = PRICE_SOURCES[source];
  if (!s) throw new Error(`unknown price source "${source}"; use one of ${Object.keys(PRICE_SOURCES).join(", ")}`);
  const res = await fetch(s.url);
  if (!res.ok) throw new Error(`${source}: HTTP ${res.status}`);
  const price = s.pick(await res.json());
  if (!Number.isFinite(price) || price <= 0) throw new Error(`${source}: no usable price`);
  return price;
}

export function formatBtc(sats: number | bigint | string): string {
  const n = BigInt(sats);
  const whole = n / 100_000_000n;
  const frac = (n % 100_000_000n).toString().padStart(8, "0");
  return `${whole}.${frac}`;
}

export function parseBtc(text: string): number {
  if (!/^\d+(\.\d{1,8})?$/.test(text.trim())) throw new Error(`not a BTC amount: ${text}`);
  const [whole, frac = ""] = text.trim().split(".");
  return Number(BigInt(whole!) * 100_000_000n + BigInt(frac.padEnd(8, "0")));
}
