// Where the app points: one deployment, fixed at build time.

import { JsonRpcProvider } from "near-api-js";
import { Esplora, Unwrapped, type BtcNetwork } from "@unwrapped/sdk";
import deployment from "../../deployments/testnet.json";

export interface Deployment {
  network: string;
  rpcUrl: string;
  btcNetwork: BtcNetwork;
  operator: string;
  /** Price reporters and the exchange each one reads. */
  oracles?: { account: string; source: string }[];
  contract: string;
  usdc: string;
  mpcSigner: string;
  btcLightClient: string;
  explorer: string;
}

export const D = deployment as Deployment;
export const provider = new JsonRpcProvider({ url: D.rpcUrl });
export const app = new Unwrapped(provider, D.contract);
export const esplora = Esplora.for(D.btcNetwork);

const MEMPOOL = D.btcNetwork === "testnet" ? "https://mempool.space/testnet" : "https://mempool.space";
export const nearTxUrl = (hash: string) => `${D.explorer}/txns/${hash}`;
export const nearAccountUrl = (id: string) => `${D.explorer}/address/${id}`;
export const btcTxUrl = (txid: string) => `${MEMPOOL}/tx/${txid}`;
export const btcAddressUrl = (address: string) => `${MEMPOOL}/address/${address}`;

export function short(text: string, head = 8, tail = 6): string {
  return text.length <= head + tail + 1 ? text : `${text.slice(0, head)}…${text.slice(-tail)}`;
}

export function pct(bps: number, digits = 1): string {
  return `${(bps / 100).toFixed(digits)}%`;
}
