// Key handling and transaction submission for the CLI.

import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { Account, JsonRpcProvider, KeyPair, actions, type KeyPairString } from "near-api-js";
import { rpcCall, signAndSend, type Action, type TxCall, type TxResult } from "@unwrapped/sdk";

export const NEAR = 10n ** 24n;

export function credentialsDir(network: string): string {
  return join(process.env.NEAR_CREDENTIALS_DIR ?? join(homedir(), ".near-credentials"), network);
}

export function readKey(network: string, accountId: string): KeyPair {
  const file = join(credentialsDir(network), `${accountId}.json`);
  if (!existsSync(file)) {
    throw new Error(`no key for ${accountId}: expected ${file} (same format as near-cli)`);
  }
  return KeyPair.fromString(JSON.parse(readFileSync(file, "utf8")).private_key as KeyPairString);
}

export function saveKey(network: string, accountId: string, keyPair: KeyPair): string {
  const dir = credentialsDir(network);
  mkdirSync(dir, { recursive: true, mode: 0o700 });
  const file = join(dir, `${accountId}.json`);
  writeFileSync(
    file,
    JSON.stringify({ account_id: accountId, public_key: keyPair.getPublicKey().toString(), private_key: keyPair.toString() }),
    { mode: 0o600 },
  );
  return file;
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

export class Near {
  readonly provider: JsonRpcProvider;

  constructor(
    readonly network: string,
    readonly rpcUrl: string,
  ) {
    this.provider = new JsonRpcProvider({ url: rpcUrl });
  }

  account(accountId: string, keyPair?: KeyPair): Account {
    const key = keyPair ?? readKey(this.network, accountId);
    return new Account(accountId, this.provider, key.toString() as KeyPairString);
  }

  private rpc(method: string, params: unknown): Promise<any> {
    return rpcCall(this.rpcUrl, method, params);
  }

  async exists(accountId: string): Promise<boolean> {
    const r = await this.rpc("query", { request_type: "view_account", finality: "final", account_id: accountId });
    return r.result !== undefined;
  }

  async balance(accountId: string): Promise<bigint> {
    const r = await this.rpc("query", { request_type: "view_account", finality: "optimistic", account_id: accountId });
    if (!r.result) throw new Error(`account ${accountId} does not exist`);
    return BigInt(r.result.amount);
  }

  /** Signs, broadcasts and waits for every receipt. */
  send(account: Account, receiverId: string, acts: Action[], timeoutMs = 240_000): Promise<TxResult> {
    return signAndSend(this.rpcUrl, account, receiverId, acts, timeoutMs);
  }

  call(account: Account, c: TxCall): Promise<TxResult> {
    return this.send(account, c.receiverId, [actions.functionCall(c.method, c.args, c.gas, c.deposit)]);
  }

  /** Like `call`, but throws unless every receipt succeeded. */
  async must(account: Account, c: TxCall): Promise<TxResult> {
    const out = await this.call(account, c);
    if (!out.ok) throw new Error(`${c.receiverId}.${c.method} failed: ${out.error}${out.txHash ? ` (tx ${out.txHash})` : ""}`);
    return out;
  }

  /** Creates a top-level testnet account through the public faucet helper. */
  async createFaucetAccount(accountId: string): Promise<KeyPair> {
    if (this.network !== "testnet") throw new Error("the faucet exists only on testnet");
    if (await this.exists(accountId)) throw new Error(`${accountId} already exists; pick another name`);
    const keyPair = KeyPair.fromRandom("ed25519");
    const res = await fetch("https://helper.testnet.near.org/account", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ newAccountId: accountId, newAccountPublicKey: keyPair.getPublicKey().toString() }),
    });
    if (!res.ok) throw new Error(`faucet: HTTP ${res.status} ${(await res.text()).slice(0, 200)}`);
    for (let i = 0; i < 20 && !(await this.exists(accountId)); i++) await sleep(1000);
    // The helper can answer 200 for a name someone else just took.
    const keys = await this.rpc("query", { request_type: "view_access_key_list", finality: "final", account_id: accountId });
    const mine = keyPair.getPublicKey().toString();
    if (!(keys.result?.keys ?? []).some((k: { public_key: string }) => k.public_key === mine)) {
      throw new Error(`${accountId} exists but is not controlled by the new key`);
    }
    saveKey(this.network, accountId, keyPair);
    return keyPair;
  }

  /** Creates `<prefix>.<parent>` with its own key and `near` NEAR. */
  async createSubAccount(parent: Account, prefix: string, amount: bigint): Promise<Account> {
    const accountId = `${prefix}.${parent.accountId}`;
    if (await this.exists(accountId)) return this.account(accountId);
    const keyPair = KeyPair.fromRandom("ed25519");
    const out = await this.send(parent, accountId, [
      actions.createAccount(),
      actions.transfer(amount),
      actions.addFullAccessKey(keyPair.getPublicKey()),
    ]);
    if (!out.ok) throw new Error(`creating ${accountId} failed: ${out.error}`);
    saveKey(this.network, accountId, keyPair);
    return this.account(accountId, keyPair);
  }

  async deploy(account: Account, wasm: Uint8Array, init?: { method: string; args: Record<string, unknown> }): Promise<TxResult> {
    const acts: Action[] = [actions.deployContract(wasm)];
    if (init) acts.push(actions.functionCall(init.method, init.args, 100n * 10n ** 12n, 0n));
    const out = await this.send(account, account.accountId, acts);
    if (!out.ok) throw new Error(`deploying to ${account.accountId} failed: ${out.error}`);
    return out;
  }
}
