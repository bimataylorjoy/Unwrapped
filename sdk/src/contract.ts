// Typed client for the Unwrapped contract.
//
// Reads go straight to a NEAR RPC. Writes are returned as plain `TxCall`
// descriptions, so the same code drives a key held by the CLI and a browser
// wallet.

import { JsonRpcProvider } from "near-api-js";
import type { DepositProof, WithdrawalPlan } from "./btc.ts";

export const TGAS = 1_000_000_000_000n;
export const ONE_YOCTO = 1n;
export const USDC_DECIMALS = 6;

export interface Config {
  mpc_signer: string;
  mpc_domain_id: number;
  btc_light_client: string;
  btc_hrp: "bc" | "tb";
  min_confirmations: number;
  usdc: string;
  oracle_ids: string[];
  min_oracle_reports: number;
  max_oracle_spread_bps: number;
  max_price_age_sec: number;
  max_ltv_bps: number;
  liq_threshold_bps: number;
  liq_bonus_bps: number;
  base_rate_bps: number;
  slope_rate_bps: number;
  /** yoctoNEAR */
  mint_fee: string;
  /** yoctoNEAR per withdrawal input */
  ledger_fee: string;
}

export interface Utxo {
  txid: string;
  vout: number;
  value: string;
}

export interface Vault {
  vault_id: number;
  owner_id: string;
  btc_address: string;
  public_key: string;
  script_pubkey: string;
  derivation_path: string;
  utxos: Utxo[];
  collateral_sats: string;
  debt: string;
  locked: boolean;
  collateral_value: string | null;
  ltv_bps: number | null;
  liquidatable: boolean;
}

export interface Pool {
  cash: string;
  total_debt: string;
  total_assets: string;
  total_shares: string;
  utilization_bps: number;
  borrow_apr_bps: number;
  supply_apr_bps: number;
  borrow_index: string;
}

/** Median of the fresh reports; `null` from the contract while there is no quorum. */
export interface Price {
  usdc_per_btc: string;
  /** Timestamp of the oldest report used. */
  updated_at_ns: string;
  sources: number;
  spread_bps: number;
}

export interface PriceReport {
  oracle_id: string;
  usdc_per_btc: string;
  updated_at_ns: string;
  fresh: boolean;
}

export interface LiquidationQuote {
  liquidatable: boolean;
  collateral_sats: string;
  collateral_value: string;
  debt: string;
  cost: string;
  to_pool: string;
  to_borrower: string;
  bad_debt: string;
}

export interface SignedWithdrawal {
  vault_id: number;
  txid: string;
  raw_tx: string;
  fee_sats: string;
}

export type OutpointState = "collateral" | "signed" | "unknown";

/** One function call, ready to be signed by whatever holds the key. */
export interface TxCall {
  receiverId: string;
  method: string;
  args: Record<string, unknown>;
  gas: bigint;
  deposit: bigint;
}

export class Unwrapped {
  constructor(
    readonly provider: JsonRpcProvider,
    readonly contractId: string,
  ) {}

  private view<T>(method: string, args: Record<string, unknown> = {}): Promise<T> {
    return this.provider.callFunction({ contractId: this.contractId, method, args }) as Promise<T>;
  }

  config = () => this.view<Config>("get_config");
  price = () => this.view<Price | null>("get_price");
  isPriceFresh = () => this.view<boolean>("is_price_fresh");
  /** Why there is no price right now, or null. */
  priceError = () => this.view<string | null>("price_error");
  priceReports = () => this.view<PriceReport[]>("get_price_reports");
  pool = () => this.view<Pool>("get_pool");
  vault = (vaultId: number) => this.view<Vault | null>("get_vault", { vault_id: vaultId });
  vaults = (fromId = 1, limit = 100) => this.view<Vault[]>("get_vaults", { from_id: fromId, limit });
  vaultsOf = (accountId: string) => this.view<Vault[]>("vaults_of", { account_id: accountId });
  totalVaults = () => this.view<number>("total_vaults");
  sharesOf = (accountId: string) => this.view<string>("shares_of", { account_id: accountId });
  suppliedOf = (accountId: string) => this.view<string>("supplied_of", { account_id: accountId });
  claimableOf = (accountId: string) => this.view<string>("claimable_of", { account_id: accountId });
  liquidationQuote = (vaultId: number) =>
    this.view<LiquidationQuote | null>("liquidation_quote", { vault_id: vaultId });
  outpointState = (vaultId: number, txid: string, vout: number) =>
    this.view<OutpointState>("outpoint_state", { vault_id: vaultId, txid, vout });

  usdcBalance(usdc: string, accountId: string): Promise<string> {
    return this.provider.callFunction({
      contractId: usdc,
      method: "ft_balance_of",
      args: { account_id: accountId },
    }) as Promise<string>;
  }

  async isRegisteredOn(usdc: string, accountId: string): Promise<boolean> {
    const balance = await this.provider.callFunction({
      contractId: usdc,
      method: "storage_balance_of",
      args: { account_id: accountId },
    });
    return balance !== null && balance !== undefined;
  }

  // --- writes ---------------------------------------------------------------

  private call(method: string, args: Record<string, unknown>, gasTgas: bigint, deposit: bigint): TxCall {
    return { receiverId: this.contractId, method, args, gas: gasTgas * TGAS, deposit };
  }

  mintVault = (mintFeeYocto: string | bigint) => this.call("mint_vault", {}, 100n, BigInt(mintFeeYocto));

  proveDeposit = (vaultId: number, proof: DepositProof) =>
    this.call("prove_deposit", { vault_id: vaultId, ...proof }, 150n, 0n);

  /** `ledgerFeeYocto` is `Config.ledger_fee`; it is charged once per input. */
  signWithdrawal = (vaultId: number, plan: Pick<WithdrawalPlan, "inputs" | "outputs">, ledgerFeeYocto: string | bigint) => {
    const fee = BigInt(ledgerFeeYocto) * BigInt(plan.inputs.length);
    return this.call(
      "sign_withdrawal",
      { vault_id: vaultId, inputs: plan.inputs, outputs: plan.outputs },
      300n,
      fee > 0n ? fee : ONE_YOCTO,
    );
  };

  borrow = (vaultId: number, amount: bigint) =>
    this.call("borrow", { vault_id: vaultId, amount: amount.toString() }, 100n, ONE_YOCTO);

  withdraw = (shares: bigint) => this.call("withdraw", { shares: shares.toString() }, 100n, ONE_YOCTO);

  claim = () => this.call("claim", {}, 100n, ONE_YOCTO);

  /** Posts the caller's own report; only accounts in `oracle_ids`. */
  reportPrice = (usdcPerBtc: bigint) => this.call("report_price", { usdc_per_btc: usdcPerBtc.toString() }, 30n, 0n);

  private viaUsdc(usdc: string, amount: bigint, msg: object): TxCall {
    return {
      receiverId: usdc,
      method: "ft_transfer_call",
      args: { receiver_id: this.contractId, amount: amount.toString(), msg: JSON.stringify(msg) },
      gas: 200n * TGAS,
      deposit: ONE_YOCTO,
    };
  }

  supply = (usdc: string, amount: bigint) => this.viaUsdc(usdc, amount, { action: "supply" });
  repay = (usdc: string, vaultId: number, amount: bigint) =>
    this.viaUsdc(usdc, amount, { action: "repay", vault_id: vaultId });
  liquidate = (usdc: string, vaultId: number, amount: bigint) =>
    this.viaUsdc(usdc, amount, { action: "liquidate", vault_id: vaultId });
}

/** What a transaction did, as reported by the chain. */
export interface TxResult {
  ok: boolean;
  /** Return value of the last receipt, JSON-decoded. */
  value?: unknown;
  /** First failure anywhere in the receipt tree. */
  error?: string;
  logs: string[];
  txHash?: string;
}

/** Pulls the human-readable panic message out of a NEAR failure object. */
function failureText(failure: unknown): string {
  const text = JSON.stringify(failure);
  const m = /Smart contract panicked: (.*?)\\?"[,}]/.exec(text) ?? /"ExecutionError":"(.*?)"/.exec(text);
  return m?.[1] ?? text;
}

/** Normalises a final execution outcome from near-api-js or a wallet. */
// eslint-disable-next-line @typescript-eslint/no-explicit-any
export function parseOutcome(res: any): TxResult {
  const logs: string[] = [];
  let failure: unknown;
  for (const r of [res?.transaction_outcome, ...(res?.receipts_outcome ?? [])]) {
    if (!r) continue;
    logs.push(...(r.outcome?.logs ?? []));
    const st = r.outcome?.status;
    if (st && typeof st === "object" && "Failure" in st && failure === undefined) failure = st.Failure;
  }
  const status = res?.status ?? {};
  if (typeof status === "object" && "Failure" in status && failure === undefined) failure = status.Failure;

  let value: unknown;
  if (typeof status?.SuccessValue === "string" && status.SuccessValue.length > 0) {
    const text = new TextDecoder().decode(Uint8Array.from(atob(status.SuccessValue), (c) => c.charCodeAt(0)));
    try {
      value = JSON.parse(text);
    } catch {
      value = text;
    }
  }
  return {
    ok: failure === undefined,
    value,
    error: failure === undefined ? undefined : failureText(failure),
    logs,
    txHash: res?.transaction?.hash ?? res?.transaction_outcome?.id,
  };
}

export function formatUsdc(micro: string | bigint, digits = 2): string {
  const n = BigInt(micro);
  const whole = n / 1_000_000n;
  const frac = (n % 1_000_000n).toString().padStart(6, "0").slice(0, digits);
  return `${whole.toLocaleString("en-US")}${digits > 0 ? "." + frac : ""}`;
}

export function parseUsdc(text: string): bigint {
  const t = text.trim().replace(/,/g, "");
  if (!/^\d+(\.\d{1,6})?$/.test(t)) throw new Error(`not a USDC amount: ${text}`);
  const [whole, frac = ""] = t.split(".");
  return BigInt(whole!) * 1_000_000n + BigInt(frac.padEnd(6, "0"));
}
