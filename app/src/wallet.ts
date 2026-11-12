// Signing. Two ways in:
//   wallet  - any NEAR wallet through @hot-labs/near-connect.
//   dev key - a pasted testnet private key, kept in sessionStorage only. For
//             demos and automated checks; never paste a mainnet key into a page.

import { NearConnector } from "@hot-labs/near-connect";
import { Account, type KeyPairString } from "near-api-js";
import { callToAction, parseOutcome, signAndSend, waitForTx, type TxCall, type TxResult } from "@unwrapped/sdk";
import { D, provider } from "./chain.ts";

export interface Session {
  accountId: string;
  kind: "wallet" | "devkey";
  send(call: TxCall): Promise<TxResult>;
  signOut(): Promise<void>;
}

const DEV_KEY = "unwrapped.devkey";
let connector: NearConnector | undefined;

function getConnector(): NearConnector {
  connector ??= new NearConnector({ network: D.network === "mainnet" ? "mainnet" : "testnet" });
  return connector;
}

type Wallet = Awaited<ReturnType<NearConnector["wallet"]>>;

function walletSession(c: NearConnector, wallet: Wallet, accountId: string): Session {
  return {
    accountId,
    kind: "wallet",
    async send(call) {
      const res = (await wallet.signAndSendTransaction({
        receiverId: call.receiverId,
        actions: [
          {
            type: "FunctionCall",
            params: { methodName: call.method, args: call.args, gas: call.gas.toString(), deposit: call.deposit.toString() },
          },
        ],
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
      })) as any;
      // Wallets differ in how long they wait; ask the chain for the final word.
      const hash: string | undefined = res?.transaction?.hash ?? res?.transaction_outcome?.id;
      return hash ? waitForTx(D.rpcUrl, hash, accountId) : parseOutcome(res);
    },
    async signOut() {
      await c.disconnect();
    },
  };
}

function devKeySession(accountId: string, privateKey: string): Session {
  const account = new Account(accountId, provider, privateKey as KeyPairString);
  return {
    accountId,
    kind: "devkey",
    send: (call) => signAndSend(D.rpcUrl, account, call.receiverId, [callToAction(call)]),
    async signOut() {
      sessionStorage.removeItem(DEV_KEY);
    },
  };
}

export async function restoreSession(): Promise<Session | undefined> {
  const saved = sessionStorage.getItem(DEV_KEY);
  if (saved) {
    try {
      const { accountId, privateKey } = JSON.parse(saved);
      return devKeySession(accountId, privateKey);
    } catch {
      sessionStorage.removeItem(DEV_KEY);
    }
  }
  try {
    const c = getConnector();
    await c.whenManifestLoaded;
    const { wallet, accounts } = await c.getConnectedWallet();
    if (accounts[0]) return walletSession(c, wallet, accounts[0].accountId);
  } catch {
    // not connected
  }
  return undefined;
}

export async function connectWallet(): Promise<Session> {
  const c = getConnector();
  await c.whenManifestLoaded;
  const wallet = await c.connect();
  const accounts = await wallet.getAccounts();
  if (!accounts[0]) throw new Error("the wallet returned no account");
  return walletSession(c, wallet, accounts[0].accountId);
}

export function connectDevKey(accountId: string, privateKey: string): Session {
  if (D.network === "mainnet") throw new Error("dev keys are for testnet only");
  if (!/^(ed25519|secp256k1):/.test(privateKey)) throw new Error("expected a key like ed25519:…");
  const session = devKeySession(accountId.trim(), privateKey.trim());
  sessionStorage.setItem(DEV_KEY, JSON.stringify({ accountId: accountId.trim(), privateKey: privateKey.trim() }));
  return session;
}
