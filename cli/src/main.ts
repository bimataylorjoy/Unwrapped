// Unwrapped command line: deploy, operate and inspect the protocol.
//
//   npm run cli -- <command> [args] [--as <account>] [--deployment <file>]
//
// Run without arguments for the command list.

import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
  DepositNotReady,
  Esplora,
  Unwrapped,
  addressToScript,
  buildDepositProof,
  formatBtc,
  formatUsdc,
  parseBtc,
  parseUsdc,
  planWithdrawal,
  verifyWithdrawal,
  fetchBtcUsd,
  type BtcNetwork,
  type SignedWithdrawal,
  type TxCall,
  type Vault,
} from "@unwrapped/sdk";
import { actions } from "near-api-js";
import { NEAR, Near } from "./near.ts";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "../..");

interface Deployment {
  network: "testnet" | "mainnet";
  rpcUrl: string;
  btcNetwork: BtcNetwork;
  /** Account that deployed the contracts and can change risk parameters. */
  operator: string;
  /** Price reporters, each reading a different exchange. */
  oracles?: { account: string; source: string }[];
  contract: string;
  usdc: string;
  mpcSigner: string;
  btcLightClient: string;
  explorer: string;
  deployedAt: string;
}

const USAGE = `Unwrapped CLI

Setup
  account create <name.testnet>          create a funded testnet account (faucet)
  deploy --operator <account>            deploy test USDC + Unwrapped under <account>
Read
  status                                 pool, price and risk parameters
  vault <id>                             one vault, with its Bitcoin balance
  vaults [--owner <account>]             all vaults, or one account's
Vault
  mint                                   create a vault (an NFT with a Bitcoin address)
  prove <vault_id> [txid]                register confirmed BTC deposits as collateral
  withdraw-btc <vault_id> <address> <btc|max> [--fee-rate <sat/vB>] [--no-broadcast]
  transfer <vault_id> <account>          send the vault NFT (and its Bitcoin key)
  selftest <vault_id>                    have the MPC network sign a dummy Bitcoin
                                         transaction and verify the signature locally
Lending
  faucet                                 get 10,000 test USDC
  supply <usdc>                          lend to the pool
  withdraw <usdc|all>                    take liquidity back
  borrow <vault_id> <usdc>
  repay <vault_id> <usdc|all>
  liquidate <vault_id>
  claim                                  collect USDC held for you
Oracle
  price                                  the aggregated price and every reporter's report
  keeper [--interval <sec>] [--once]     every reporter posts its exchange's price, in a loop
  report <usd> --as <reporter>           post one report by hand
Operator
  set-config <field> <number>            change one risk or oracle parameter
  upgrade-oracle                         upgrade a single-oracle deployment to three reporters

Options
  --as <account>          act as this account (default: UNWRAPPED_ACCOUNT, else the operator)
  --deployment <file>     default: deployments/testnet.json
  --rpc <url>             override the NEAR RPC endpoint
`;

function parseArgs(argv: string[]): { positional: string[]; flags: Record<string, string | true> } {
  const positional: string[] = [];
  const flags: Record<string, string | true> = {};
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i]!;
    if (a.startsWith("--")) {
      const next = argv[i + 1];
      if (next !== undefined && !next.startsWith("--")) {
        flags[a.slice(2)] = next;
        i++;
      } else {
        flags[a.slice(2)] = true;
      }
    } else {
      positional.push(a);
    }
  }
  return { positional, flags };
}

const { positional, flags } = parseArgs(process.argv.slice(2));
const flag = (name: string): string | undefined => (typeof flags[name] === "string" ? (flags[name] as string) : undefined);

function need(value: string | undefined, what: string): string {
  if (!value) throw new Error(`missing ${what}\n\n${USAGE}`);
  return value;
}

function vaultIdArg(value: string | undefined): number {
  const id = Number(need(value, "vault id"));
  if (!Number.isInteger(id) || id < 1) throw new Error("vault id must be a positive integer");
  return id;
}

const deploymentFile = resolve(ROOT, flag("deployment") ?? "deployments/testnet.json");

function loadDeployment(): Deployment {
  if (!existsSync(deploymentFile)) {
    throw new Error(`${deploymentFile} not found. Run "deploy" first, or pass --deployment.`);
  }
  return JSON.parse(readFileSync(deploymentFile, "utf8"));
}

function pct(bps: number): string {
  return `${(bps / 100).toFixed(2)}%`;
}

function printResult(label: string, out: { txHash?: string }, explorer: string): void {
  console.log(`${label}${out.txHash ? `\n  tx: ${explorer}/txns/${out.txHash}` : ""}`);
}

class Ctx {
  readonly near: Near;
  readonly app: Unwrapped;
  readonly esplora: Esplora;
  readonly actor: string;

  constructor(readonly d: Deployment) {
    this.near = new Near(d.network, flag("rpc") ?? process.env.NEAR_RPC_URL ?? d.rpcUrl);
    this.app = new Unwrapped(this.near.provider, d.contract);
    this.esplora = Esplora.for(d.btcNetwork);
    this.actor = flag("as") ?? process.env.UNWRAPPED_ACCOUNT ?? d.operator;
  }

  send(call: TxCall) {
    return this.near.must(this.near.account(this.actor), call);
  }

  async vault(id: number): Promise<Vault> {
    const v = await this.app.vault(id);
    if (!v) throw new Error(`vault ${id} does not exist`);
    return v;
  }
}

function describeVault(v: Vault): string {
  const lines = [
    `Vault #${v.vault_id}  owner ${v.owner_id}${v.locked ? "  [locked: secures a loan]" : ""}`,
    `  bitcoin address   ${v.btc_address}`,
    `  collateral        ${formatBtc(v.collateral_sats)} BTC in ${v.utxos.length} proven deposit(s)` +
      (v.collateral_value !== null ? `  (~$${formatUsdc(v.collateral_value)})` : "  (price stale)"),
    `  debt              ${formatUsdc(v.debt)} USDC` +
      (v.ltv_bps !== null && BigInt(v.debt) > 0n ? `  LTV ${pct(v.ltv_bps)}` : "") +
      (v.liquidatable ? "  ** LIQUIDATABLE **" : ""),
  ];
  for (const u of v.utxos) lines.push(`    ${u.txid}:${u.vout}  ${formatBtc(u.value)} BTC`);
  return lines.join("\n");
}

async function cmdAccount(): Promise<void> {
  if (positional[1] !== "create") throw new Error(USAGE);
  const accountId = need(positional[2], "account name");
  const near = new Near("testnet", flag("rpc") ?? process.env.NEAR_RPC_URL ?? "https://test.rpc.fastnear.com");
  await near.createFaucetAccount(accountId);
  console.log(`created ${accountId} with ${Number((await near.balance(accountId)) / (NEAR / 1000n)) / 1000} NEAR`);
  console.log("key saved under ~/.near-credentials/testnet/");
}

async function cmdDeploy(): Promise<void> {
  const operatorId = need(flag("operator"), "--operator <account>");
  const network = (flag("network") ?? "testnet") as "testnet";
  if (network !== "testnet") throw new Error("this command deploys test contracts; it only supports testnet");
  const rpcUrl = flag("rpc") ?? process.env.NEAR_RPC_URL ?? "https://test.rpc.fastnear.com";
  const near = new Near(network, rpcUrl);
  const operator = near.account(operatorId);

  const wasm = (name: string) => {
    const file = join(ROOT, `target/near/${name}/${name}.wasm`);
    if (!existsSync(file)) throw new Error(`${file} not found; run ./scripts/build.sh first`);
    return readFileSync(file);
  };
  const appWasm = wasm("unwrapped");
  const ftWasm = wasm("mock_ft");
  // Storage costs 1 NEAR per 100 kB; add room for state and gas.
  const cost = (bytes: number) => (BigInt(bytes) * NEAR) / 100_000n + NEAR / 2n;

  console.log(`operator ${operatorId}: ${Number((await near.balance(operatorId)) / (NEAR / 100n)) / 100} NEAR`);
  const usdc = await near.createSubAccount(operator, flag("usdc-prefix") ?? "usdc", cost(ftWasm.length));
  await near.deploy(usdc, ftWasm, {
    method: "new",
    args: { owner_id: operatorId, name: "USD Coin (Unwrapped testnet)", symbol: "USDC", decimals: 6 },
  });
  console.log(`test USDC deployed at ${usdc.accountId}`);

  const oracles = await createReporters(near, operator);

  const mpcSigner = flag("mpc-signer") ?? "v1.signer-prod.testnet";
  const btcLightClient = flag("btc-light-client") ?? "btc-client-v4.testnet";
  const app = await near.createSubAccount(operator, flag("app-prefix") ?? "vault", cost(appWasm.length));
  await near.deploy(app, appWasm, {
    method: "new",
    args: {
      owner_id: operatorId,
      config: {
        mpc_signer: mpcSigner,
        mpc_domain_id: 0,
        btc_light_client: btcLightClient,
        btc_hrp: "tb",
        min_confirmations: Number(flag("min-confirmations") ?? 2),
        usdc: usdc.accountId,
        oracle_ids: oracles.map((o) => o.account),
        min_oracle_reports: ORACLE_QUORUM,
        max_oracle_spread_bps: ORACLE_SPREAD_BPS,
        max_price_age_sec: Number(flag("max-price-age") ?? ORACLE_MAX_AGE_SEC),
        max_ltv_bps: 5000,
        liq_threshold_bps: 7500,
        liq_bonus_bps: 500,
        base_rate_bps: 200,
        slope_rate_bps: 1800,
        mint_fee: (NEAR / 10n).toString(),
        ledger_fee: (NEAR / 1000n).toString(),
      },
    },
  });
  console.log(`Unwrapped deployed at ${app.accountId}`);

  // The pool and the operator must be able to hold the test stablecoin.
  await near.must(operator, {
    receiverId: usdc.accountId,
    method: "register_accounts",
    args: { account_ids: [app.accountId, operatorId] },
    gas: 30n * 10n ** 12n,
    deposit: 0n,
  });

  const deployment: Deployment = {
    network,
    rpcUrl,
    btcNetwork: "testnet",
    operator: operatorId,
    oracles,
    contract: app.accountId,
    usdc: usdc.accountId,
    mpcSigner,
    btcLightClient,
    explorer: "https://testnet.nearblocks.io",
    deployedAt: new Date().toISOString(),
  };
  mkdirSync(dirname(deploymentFile), { recursive: true });
  writeFileSync(deploymentFile, JSON.stringify(deployment, null, 2) + "\n");
  console.log(`wrote ${deploymentFile}`);
}

/** Three reporters, three exchanges; two must agree within 2%. */
const ORACLE_SOURCES = ["coinbase", "kraken", "bitstamp"];
const ORACLE_QUORUM = 2;
const ORACLE_SPREAD_BPS = 200;
const ORACLE_MAX_AGE_SEC = 3600;

/** Creates `oracle-<source>.<operator>` for each source, each with its own key. */
async function createReporters(near: Near, operator: ReturnType<Near["account"]>) {
  const out: { account: string; source: string }[] = [];
  for (const source of ORACLE_SOURCES) {
    const acct = await near.createSubAccount(operator, `oracle-${source}`, NEAR / 2n);
    out.push({ account: acct.accountId, source });
    console.log(`reporter ${acct.accountId} reads ${source}`);
  }
  return out;
}

async function cmdUpgradeOracle(): Promise<void> {
  const d = loadDeployment();
  const near = new Near(d.network, flag("rpc") ?? process.env.NEAR_RPC_URL ?? d.rpcUrl);
  const file = join(ROOT, "target/near/unwrapped/unwrapped.wasm");
  if (!existsSync(file)) throw new Error(`${file} not found; run ./scripts/build.sh first`);

  const oracles = d.oracles ?? (await createReporters(near, near.account(d.operator)));
  // Deploy the new code and migrate the state in one transaction, signed by
  // the contract account itself (the migration method is private).
  const contract = near.account(d.contract);
  const out = await near.send(contract, d.contract, [
    actions.deployContract(readFileSync(file)),
    actions.functionCall(
      "migrate_to_multi_oracle",
      {
        oracle_ids: oracles.map((o) => o.account),
        min_oracle_reports: ORACLE_QUORUM,
        max_oracle_spread_bps: ORACLE_SPREAD_BPS,
        max_price_age_sec: ORACLE_MAX_AGE_SEC,
      },
      100n * 10n ** 12n,
      0n,
    ),
  ]);
  if (!out.ok) throw new Error(`upgrade failed: ${out.error} (tx ${out.txHash})`);
  d.oracles = oracles;
  writeFileSync(deploymentFile, JSON.stringify(d, null, 2) + "\n");
  console.log(`upgraded ${d.contract} to the multi-reporter oracle\n  tx: ${d.explorer}/txns/${out.txHash}`);
}

/** Every reporter reads its own exchange and reports with its own key. */
async function postReports(ctx: Ctx): Promise<void> {
  const reporters = ctx.d.oracles ?? [];
  if (reporters.length === 0) throw new Error("this deployment has no reporters; run upgrade-oracle");
  await Promise.all(
    reporters.map(async ({ account, source }) => {
      try {
        const usd = await fetchBtcUsd(source);
        await ctx.near.must(ctx.near.account(account), ctx.app.reportPrice(parseUsdc(usd.toFixed(2))));
        console.log(`${new Date().toISOString()}  ${source.padEnd(9)} $${usd.toFixed(2)}  (${account})`);
      } catch (e) {
        console.error(`${new Date().toISOString()}  ${source.padEnd(9)} failed: ${e instanceof Error ? e.message : e}`);
      }
    }),
  );
  const [price, why] = await Promise.all([ctx.app.price(), ctx.app.priceError()]);
  console.log(
    price
      ? `  -> median $${formatUsdc(price.usdc_per_btc)} from ${price.sources} reports, spread ${pct(price.spread_bps)}`
      : `  -> no price: ${why}`,
  );
}

async function main(): Promise<void> {
  const command = positional[0];
  if (!command || command === "help" || flags.help) {
    console.log(USAGE);
    return;
  }
  if (command === "account") return cmdAccount();
  if (command === "deploy") return cmdDeploy();
  if (command === "upgrade-oracle") return cmdUpgradeOracle();

  const ctx = new Ctx(loadDeployment());
  const { app, d } = ctx;

  switch (command) {
    case "status": {
      const [cfg, pool, price, why, total] = await Promise.all([
        app.config(),
        app.pool(),
        app.price(),
        app.priceError(),
        app.totalVaults(),
      ]);
      console.log(`Unwrapped at ${d.contract} (${d.network}, Bitcoin ${d.btcNetwork})`);
      console.log(`  vaults            ${total}`);
      console.log(
        `  BTC price         ${price ? `$${formatUsdc(price.usdc_per_btc)}  (median of ${price.sources} reports)` : `unavailable: ${why}`}`,
      );
      console.log(`  pool assets       ${formatUsdc(pool.total_assets)} USDC  (idle ${formatUsdc(pool.cash)}, lent ${formatUsdc(pool.total_debt)})`);
      console.log(`  utilisation       ${pct(pool.utilization_bps)}   borrow APR ${pct(pool.borrow_apr_bps)}   supply APR ${pct(pool.supply_apr_bps)}`);
      console.log(`  max LTV           ${pct(cfg.max_ltv_bps)}   liquidation at ${pct(cfg.liq_threshold_bps)}   liquidator discount ${pct(cfg.liq_bonus_bps)}`);
      console.log(
        `  oracle            ${cfg.oracle_ids.length} reporters, ${cfg.min_oracle_reports} must agree within ${pct(cfg.max_oracle_spread_bps)}, max age ${cfg.max_price_age_sec}s`,
      );
      console.log(`  MPC signer        ${cfg.mpc_signer}`);
      console.log(`  BTC light client  ${cfg.btc_light_client}  (${cfg.min_confirmations} confirmations)`);
      console.log(`  stablecoin        ${cfg.usdc}`);
      return;
    }

    case "vault": {
      const v = await ctx.vault(vaultIdArg(positional[1]));
      console.log(describeVault(v));
      const onChain = await ctx.esplora.utxos(v.btc_address);
      const proven = new Set(v.utxos.map((u) => `${u.txid}:${u.vout}`));
      console.log(`  on Bitcoin        ${formatBtc(onChain.reduce((s, u) => s + u.value, 0))} BTC in ${onChain.length} output(s)`);
      for (const u of onChain) {
        const state = proven.has(`${u.txid}:${u.vout}`) ? "collateral" : await app.outpointState(v.vault_id, u.txid, u.vout);
        const conf = u.status.confirmed ? "confirmed" : "unconfirmed";
        console.log(`    ${u.txid}:${u.vout}  ${formatBtc(u.value)} BTC  ${conf}  ledger: ${state}`);
      }
      return;
    }

    case "vaults": {
      const owner = flag("owner");
      const list = owner ? await app.vaultsOf(owner) : await app.vaults();
      if (list.length === 0) console.log("no vaults");
      for (const v of list) console.log(describeVault(v) + "\n");
      return;
    }

    case "mint": {
      const cfg = await app.config();
      const out = await ctx.send(app.mintVault(cfg.mint_fee));
      const v = out.value as Vault | null;
      if (!v) throw new Error("the signer did not return a key; the mint fee was refunded");
      printResult(`minted vault #${v.vault_id} for ${ctx.actor}`, out, d.explorer);
      console.log(`  send Bitcoin (${d.btcNetwork}) to: ${v.btc_address}`);
      return;
    }

    case "prove": {
      const id = vaultIdArg(positional[1]);
      const v = await ctx.vault(id);
      const cfg = await app.config();
      const txids = positional[2]
        ? [positional[2]]
        : [...new Set((await ctx.esplora.utxos(v.btc_address)).map((u) => u.txid))];
      if (txids.length === 0) console.log(`no Bitcoin has arrived at ${v.btc_address} yet`);
      for (const txid of txids) {
        try {
          const dep = await buildDepositProof(ctx.esplora, txid, v.script_pubkey);
          const state = await app.outpointState(id, dep.txid, dep.vout);
          if (state !== "unknown") {
            console.log(`${txid}:${dep.vout} is already in the ledger as "${state}"`);
            continue;
          }
          if (dep.confirmations < cfg.min_confirmations) {
            console.log(`${txid} has ${dep.confirmations}/${cfg.min_confirmations} confirmations; try again later`);
            continue;
          }
          const out = await ctx.send(app.proveDeposit(id, dep.proof));
          printResult(`proved ${formatBtc(dep.value)} BTC (${txid}:${dep.vout}, block ${dep.blockHeight})`, out, d.explorer);
        } catch (e) {
          if (e instanceof DepositNotReady) console.log(e.message);
          else throw e;
        }
      }
      console.log(describeVault(await ctx.vault(id)));
      return;
    }

    case "withdraw-btc": {
      const id = vaultIdArg(positional[1]);
      const address = need(positional[2], "destination address");
      const amountArg = need(positional[3], "amount in BTC, or max");
      const v = await ctx.vault(id);
      const destination = addressToScript(address, d.btcNetwork);
      const feeRate = flag("fee-rate") ? Number(flag("fee-rate")) : await ctx.esplora.feeRate();

      // Spend what Bitcoin says is there, skipping outputs that are neither
      // confirmed nor known to the contract.
      const onChain = (await ctx.esplora.utxos(v.btc_address)).filter((u) => u.status.confirmed);
      const plan = planWithdrawal(
        onChain,
        destination,
        amountArg === "max" ? "max" : parseBtc(amountArg),
        v.script_pubkey,
        feeRate,
      );
      console.log(
        `signing: ${plan.inputs.length} input(s) -> ${formatBtc(plan.outputs[0]!.value)} BTC to ${address}` +
          (plan.outputs[1] ? `, ${formatBtc(plan.outputs[1].value)} BTC change to the vault` : "") +
          `, fee ${plan.feeSats} sats (${feeRate} sat/vB)`,
      );
      const out = await ctx.send(app.signWithdrawal(id, plan, (await app.config()).ledger_fee));
      const signed = out.value as SignedWithdrawal;
      printResult(`MPC network signed Bitcoin transaction ${signed.txid}`, out, d.explorer);
      if (flags["no-broadcast"]) {
        console.log(`raw transaction:\n${signed.raw_tx}`);
        return;
      }
      const txid = await ctx.esplora.broadcast(signed.raw_tx);
      const base = d.btcNetwork === "testnet" ? "https://mempool.space/testnet" : "https://mempool.space";
      console.log(`broadcast to Bitcoin: ${base}/tx/${txid}`);
      return;
    }

    case "selftest": {
      const id = vaultIdArg(positional[1]);
      const v = await ctx.vault(id);
      // A made-up outpoint: nothing real is spent, but the whole signing path
      // (contract digest -> MPC network -> assembled transaction) is exercised.
      const txid = [...crypto.getRandomValues(new Uint8Array(32))].map((b) => b.toString(16).padStart(2, "0")).join("");
      const plan = {
        inputs: [{ txid, vout: 0, value: "50000" }],
        outputs: [{ script_pubkey: v.script_pubkey, value: "49000" }],
      };
      const started = Date.now();
      const out = await ctx.send(app.signWithdrawal(id, plan, (await app.config()).ledger_fee));
      const signed = out.value as SignedWithdrawal;
      printResult(`MPC network answered in ${((Date.now() - started) / 1000).toFixed(1)}s`, out, d.explorer);
      const check = verifyWithdrawal(signed.raw_tx, plan, v);
      console.log(`  bitcoin txid            ${signed.txid}`);
      console.log(`  witness public key      ${check.pubkeyMatches ? "matches the vault key" : "DOES NOT MATCH"}`);
      console.log(`  ECDSA signature (low-S) ${check.signaturesValid ? "valid for the BIP143 digest" : "INVALID"}`);
      console.log(`  ledger entry            ${await app.outpointState(id, txid, 0)}`);
      if (!check.pubkeyMatches || !check.signaturesValid) process.exit(2);
      return;
    }

    case "transfer": {
      const id = vaultIdArg(positional[1]);
      const to = need(positional[2], "receiver account");
      const out = await ctx.send({
        receiverId: d.contract,
        method: "nft_transfer",
        args: { receiver_id: to, token_id: String(id) },
        gas: 50n * 10n ** 12n,
        deposit: 1n,
      });
      printResult(`vault #${id} now belongs to ${to}`, out, d.explorer);
      return;
    }

    case "faucet": {
      const out = await ctx.send({ receiverId: d.usdc, method: "faucet", args: {}, gas: 30n * 10n ** 12n, deposit: 0n });
      printResult(`${ctx.actor} received 10,000 test USDC (balance ${formatUsdc(await app.usdcBalance(d.usdc, ctx.actor))})`, out, d.explorer);
      return;
    }

    case "supply": {
      const amount = parseUsdc(need(positional[1], "amount"));
      const out = await ctx.send(app.supply(d.usdc, amount));
      printResult(`supplied ${formatUsdc(amount)} USDC; position is now ${formatUsdc(await app.suppliedOf(ctx.actor))} USDC`, out, d.explorer);
      return;
    }

    case "withdraw": {
      const arg = need(positional[1], "amount, or all");
      const shares = BigInt(await app.sharesOf(ctx.actor));
      let burn = shares;
      if (arg !== "all") {
        const supplied = BigInt(await app.suppliedOf(ctx.actor));
        const amount = parseUsdc(arg);
        if (amount > supplied) throw new Error(`you have ${formatUsdc(supplied)} USDC supplied`);
        burn = (shares * amount) / supplied;
      }
      const out = await ctx.send(app.withdraw(burn));
      printResult(`withdrew; wallet balance ${formatUsdc(await app.usdcBalance(d.usdc, ctx.actor))} USDC`, out, d.explorer);
      return;
    }

    case "borrow": {
      const id = vaultIdArg(positional[1]);
      const amount = parseUsdc(need(positional[2], "amount"));
      if (!(await app.isRegisteredOn(d.usdc, ctx.actor))) {
        await ctx.send({ receiverId: d.usdc, method: "register_accounts", args: { account_ids: [ctx.actor] }, gas: 30n * 10n ** 12n, deposit: 0n });
      }
      const out = await ctx.send(app.borrow(id, amount));
      printResult(`borrowed ${formatUsdc(amount)} USDC against vault #${id}`, out, d.explorer);
      console.log(describeVault(await ctx.vault(id)));
      return;
    }

    case "repay": {
      const id = vaultIdArg(positional[1]);
      const arg = need(positional[2], "amount, or all");
      const v = await ctx.vault(id);
      // "all" sends a little extra for interest accruing meanwhile; the
      // contract takes only the debt and the token refunds the rest.
      // Capped at the wallet balance: if that is short of the debt, the
      // repayment is partial and the vault stays locked.
      const balance = BigInt(await app.usdcBalance(d.usdc, ctx.actor));
      const wanted = arg === "all" ? BigInt(v.debt) + BigInt(v.debt) / 1000n + 1n : parseUsdc(arg);
      const amount = arg === "all" && wanted > balance ? balance : wanted;
      if (amount === 0n) throw new Error("no USDC in the wallet; try the faucet command");
      if (amount < BigInt(v.debt) && arg === "all") {
        console.log(`wallet holds ${formatUsdc(balance)} USDC, less than the debt; repaying what is there`);
      }
      const out = await ctx.send(app.repay(d.usdc, id, amount));
      printResult(`repaid`, out, d.explorer);
      console.log(describeVault(await ctx.vault(id)));
      return;
    }

    case "liquidate": {
      const id = vaultIdArg(positional[1]);
      const quote = await app.liquidationQuote(id);
      if (!quote) throw new Error("no quote: the vault does not exist or the price is stale");
      if (!quote.liquidatable) throw new Error("vault is healthy");
      console.log(
        `buying ${formatBtc(quote.collateral_sats)} BTC for ${formatUsdc(quote.cost)} USDC ` +
          `(debt ${formatUsdc(quote.debt)}, surplus to borrower ${formatUsdc(quote.to_borrower)})`,
      );
      // Send 1% extra in case the price moves; the unused part is refunded.
      const out = await ctx.send(app.liquidate(d.usdc, id, BigInt(quote.cost) + BigInt(quote.cost) / 100n));
      printResult(`liquidated`, out, d.explorer);
      console.log(describeVault(await ctx.vault(id)));
      return;
    }

    case "claim": {
      const out = await ctx.send(app.claim());
      printResult(`claimed; wallet balance ${formatUsdc(await app.usdcBalance(d.usdc, ctx.actor))} USDC`, out, d.explorer);
      return;
    }

    case "report": {
      const usd = parseUsdc(need(positional[1], "price in USD"));
      const out = await ctx.send(app.reportPrice(usd));
      printResult(`${ctx.actor} reported $${formatUsdc(usd)}`, out, d.explorer);
      return;
    }

    case "price": {
      const [price, why, reports, cfg] = await Promise.all([
        app.price(),
        app.priceError(),
        app.priceReports(),
        app.config(),
      ]);
      console.log(
        price
          ? `BTC $${formatUsdc(price.usdc_per_btc)}: median of ${price.sources} fresh reports, spread ${pct(price.spread_bps)}`
          : `No price: ${why}`,
      );
      const now = Date.now();
      for (const id of cfg.oracle_ids) {
        const r = reports.find((x) => x.oracle_id === id);
        const source = d.oracles?.find((o) => o.account === id)?.source ?? "?";
        const age = r ? `${Math.round((now - Number(BigInt(r.updated_at_ns) / 1_000_000n)) / 1000)}s ago` : "never";
        const value = r ? "$" + formatUsdc(r.usdc_per_btc) : "-";
        console.log(`  ${id.padEnd(42)} ${source.padEnd(9)} ${value.padEnd(12)} ${age}${r && !r.fresh ? "  (stale)" : ""}`);
      }
      return;
    }

    case "set-config": {
      const field = need(positional[1], "config field");
      const value = Number(need(positional[2], "value"));
      const cfg = (await app.config()) as unknown as Record<string, unknown>;
      if (typeof cfg[field] !== "number") throw new Error(`"${field}" is not a numeric config field`);
      if (!Number.isInteger(value) || value < 0) throw new Error("value must be a non-negative integer");
      const out = await ctx.send({
        receiverId: d.contract,
        method: "update_config",
        args: { config: { ...cfg, [field]: value } },
        gas: 30n * 10n ** 12n,
        deposit: 0n,
      });
      printResult(`${field}: ${cfg[field]} -> ${value}`, out, d.explorer);
      return;
    }

    case "keeper": {
      const interval = Number(flag("interval") ?? 300) * 1000;
      for (;;) {
        await postReports(ctx);
        if (flags.once) return;
        await new Promise((r) => setTimeout(r, interval));
      }
    }

    default:
      throw new Error(`unknown command "${command}"\n\n${USAGE}`);
  }
}

main().catch((e) => {
  console.error(e instanceof Error ? e.message : e);
  process.exit(1);
});
