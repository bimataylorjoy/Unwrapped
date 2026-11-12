// Transaction submission that works the same in Node and in the browser.
//
// A Chain Signatures request can outlast a single RPC long-poll, so instead
// of one blocking "send and wait" call the transaction is broadcast and its
// status polled by hash.

import { sha256 } from "@noble/hashes/sha2";
import { base58, base64 } from "@scure/base";
import { actions, encodeTransaction, type Account } from "near-api-js";
import { parseOutcome, type TxCall, type TxResult } from "./contract.ts";

export type Action = ReturnType<typeof actions.transfer>;

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

// eslint-disable-next-line @typescript-eslint/no-explicit-any
export async function rpcCall(rpcUrl: string, method: string, params: unknown): Promise<any> {
  const res = await fetch(rpcUrl, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ jsonrpc: "2.0", id: "unwrapped", method, params }),
  });
  return res.json();
}

/** Polls until every receipt of the transaction has executed. */
export async function waitForTx(
  rpcUrl: string,
  txHash: string,
  senderId: string,
  timeoutMs = 240_000,
): Promise<TxResult> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const r = await rpcCall(rpcUrl, "tx", {
      tx_hash: txHash,
      sender_account_id: senderId,
      wait_until: "EXECUTED_OPTIMISTIC",
    }).catch(() => undefined);
    const status = r?.result?.status;
    if (status && typeof status === "object" && ("SuccessValue" in status || "Failure" in status)) {
      return { ...parseOutcome(r.result), txHash };
    }
    const cause = r?.error?.cause?.name;
    if (r?.error && cause !== "TIMEOUT_ERROR" && cause !== "UNKNOWN_TRANSACTION") {
      return { ok: false, error: JSON.stringify(r.error.data ?? r.error), logs: [], txHash };
    }
    await sleep(1500);
  }
  return { ok: false, error: `timed out waiting for transaction ${txHash}`, logs: [], txHash };
}

/** Signs with the account's key, broadcasts, and waits for the outcome. */
export async function signAndSend(
  rpcUrl: string,
  account: Account,
  receiverId: string,
  acts: Action[],
  timeoutMs?: number,
): Promise<TxResult> {
  const signed = await account.createSignedTransaction({ receiverId, actions: acts });
  const txHash = base58.encode(sha256(encodeTransaction(signed.transaction)));
  const sent = await rpcCall(rpcUrl, "broadcast_tx_async", [base64.encode(encodeTransaction(signed))]);
  if (sent.error) {
    return { ok: false, error: JSON.stringify(sent.error.data ?? sent.error), logs: [], txHash };
  }
  return waitForTx(rpcUrl, txHash, account.accountId, timeoutMs);
}

export function callToAction(call: TxCall): Action {
  return actions.functionCall(call.method, call.args, call.gas, call.deposit);
}
