import { useCallback, useEffect, useState } from "react";
import {
  DepositNotReady,
  addressToScript,
  buildDepositProof,
  formatBtc,
  formatUsdc,
  parseBtc,
  parseUsdc,
  planWithdrawal,
  verifyWithdrawal,
  type EsploraUtxo,
  type OutpointState,
  type SignedWithdrawal,
  type Vault,
} from "@unwrapped/sdk";
import type { Ctx } from "./App.tsx";
import { D, app, btcAddressUrl, btcTxUrl, esplora, nearTxUrl, pct, short } from "./chain.ts";
import { FlowDiagram } from "./FlowDiagram.tsx";

type Action = "borrow" | "repay" | "withdraw" | "transfer";

interface ChainOutput extends EsploraUtxo {
  state: OutpointState;
  confirmations: number;
}

const BPS = 10_000n;
const SATS = 100_000_000n;
/** NEP-145 storage deposit that registers an account on a NEP-141 token. */
const FT_STORAGE_DEPOSIT = 1_250_000_000_000_000_000_000n;

export function VaultCard({ ctx, vault }: { ctx: Ctx; vault: Vault }) {
  const { cfg, price, priceFresh, pool, balances } = ctx;
  const [action, setAction] = useState<Action>("borrow");
  const [outputs, setOutputs] = useState<ChainOutput[]>();
  const [copied, setCopied] = useState(false);

  const debt = BigInt(vault.debt);
  const sats = BigInt(vault.collateral_sats);
  const value = vault.collateral_value === null ? null : BigInt(vault.collateral_value);
  const maxDebt = value === null ? 0n : (value * BigInt(cfg.max_ltv_bps)) / BPS;
  const headroom = maxDebt > debt ? maxDebt - debt : 0n;
  const borrowable = headroom < BigInt(pool.cash) ? headroom : BigInt(pool.cash);
  // BTC price at which the vault crosses the liquidation threshold.
  const liquidationPrice = debt > 0n && sats > 0n ? (debt * BPS * SATS) / (sats * BigInt(cfg.liq_threshold_bps)) : null;

  const loadOutputs = useCallback(async () => {
    try {
      const [utxos, tip] = await Promise.all([esplora.utxos(vault.btc_address), esplora.tipHeight()]);
      const proven = new Set(vault.utxos.map((u) => `${u.txid}:${u.vout}`));
      const list = await Promise.all(
        utxos.map(async (u): Promise<ChainOutput> => ({
          ...u,
          confirmations: u.status.confirmed && u.status.block_height ? tip - u.status.block_height + 1 : 0,
          state: proven.has(`${u.txid}:${u.vout}`) ? "collateral" : await app.outpointState(vault.vault_id, u.txid, u.vout),
        })),
      );
      setOutputs(list.sort((a, b) => b.value - a.value));
    } catch {
      // Esplora hiccup: keep showing what we had.
    }
  }, [vault.btc_address, vault.vault_id, vault.utxos]);

  useEffect(() => {
    loadOutputs();
    const timer = setInterval(loadOutputs, 30_000);
    return () => clearInterval(timer);
  }, [loadOutputs]);

  const prove = (o: ChainOutput) =>
    ctx.run(`Proving ${formatBtc(o.value)} BTC to NEAR`, async () => {
      try {
        const dep = await buildDepositProof(esplora, o.txid, vault.script_pubkey, o.vout);
        const res = await ctx.send(app.proveDeposit(vault.vault_id, dep.proof));
        await loadOutputs();
        return { detail: `Block ${dep.blockHeight} verified by the on-chain Bitcoin light client.`, href: res.txHash && nearTxUrl(res.txHash) };
      } catch (e) {
        if (e instanceof DepositNotReady) throw new Error("The Bitcoin transaction is not confirmed yet.");
        if (e instanceof Error && /Not enough blocks|does not belong/.test(e.message)) {
          throw new Error("The light client on NEAR has not caught up with this block yet. Try again in a few minutes.");
        }
        throw e;
      }
    });

  const onBitcoin = outputs === undefined ? null : outputs.reduce((sum, o) => sum + o.value, 0);
  const status = vault.liquidatable ? "Liquidatable" : vault.locked ? "Secures a loan" : "Unencumbered";

  return (
    <article className={`vault ${vault.liquidatable ? "danger" : ""}`}>
      <div className="vault-main">
        <header className="vault-head">
          <p className="kicker">
            <span className={`dot ${vault.liquidatable ? "red" : vault.locked ? "ochre" : "olive"}`} aria-hidden="true" />
            {status}
          </p>
          <h2 className="vault-title">
            Vault <span className="no">No.</span> {vault.vault_id}
          </h2>
          <div className="address">
            <code title={vault.btc_address}>{vault.btc_address}</code>
            <span className="address-actions">
              <button
                className="link"
                onClick={async () => {
                  await navigator.clipboard.writeText(vault.btc_address);
                  setCopied(true);
                  setTimeout(() => setCopied(false), 1500);
                }}
              >
                {copied ? "Copied" : "Copy"}
              </button>
              <a className="link" href={btcAddressUrl(vault.btc_address)} target="_blank" rel="noreferrer">
                Explorer
              </a>
            </span>
          </div>
          <p className="address-note">
            Native {D.btcNetwork} address. Its key is derived by NEAR's MPC network for this contract alone.
          </p>
        </header>

        <FlowDiagram vault={vault} onBitcoinSats={onBitcoin} />

        <section className="deposits">
          <div className="deposits-head">
            <h3>Bitcoin on this address</h3>
            <button className="link" onClick={loadOutputs}>
              Refresh
            </button>
          </div>
          {outputs === undefined && <p className="muted small">Reading Bitcoin…</p>}
          {outputs?.length === 0 && (
            <p className="muted small">
              Nothing here yet. Send {D.btcNetwork} BTC to the address above; after {cfg.min_confirmations} confirmations
              prove the deposit to NEAR and borrow against it.
            </p>
          )}
          {outputs?.map((o) => (
            <div className="deposit" key={`${o.txid}:${o.vout}`}>
              <a href={btcTxUrl(o.txid)} target="_blank" rel="noreferrer" title={o.txid}>
                {short(o.txid)}:{o.vout}
              </a>
              <span className="num">{formatBtc(o.value)} BTC</span>
              <DepositStatus o={o} min={cfg.min_confirmations} busy={ctx.busy} onProve={() => prove(o)} />
            </div>
          ))}
        </section>
      </div>

      <aside className="vault-side">
        <dl className="figures">
          <div className="lead-figure">
            <dt>Collateral</dt>
            <dd>
              {formatBtc(sats)} <small>BTC</small>
            </dd>
            <span className="sub">{value === null ? "no price right now" : `$${formatUsdc(value)} at the median price`}</span>
          </div>
          <div>
            <dt>Debt</dt>
            <dd>
              {formatUsdc(debt)} <small>USDC</small>
            </dd>
            <span className="sub">{pct(pool.borrow_apr_bps, 2)} APR</span>
          </div>
          <div>
            <dt>Available</dt>
            <dd>
              {formatUsdc(borrowable)} <small>USDC</small>
            </dd>
            <span className="sub">to {pct(cfg.max_ltv_bps, 0)} LTV</span>
          </div>
          <div>
            <dt>Liquidation price</dt>
            <dd>{liquidationPrice === null ? "—" : `$${formatUsdc(liquidationPrice, 0)}`}</dd>
            <span className="sub">{liquidationPrice === null ? "no loan open" : `above ${pct(cfg.liq_threshold_bps, 0)} LTV`}</span>
          </div>
          <div>
            <dt>BTC price</dt>
            <dd>{price ? `$${formatUsdc(price.usdc_per_btc, 0)}` : "—"}</dd>
            <span className="sub">{price ? `median of ${price.sources} reporters` : "reporters unavailable"}</span>
          </div>
        </dl>

        <LtvBar ltvBps={vault.ltv_bps} maxBps={cfg.max_ltv_bps} liqBps={cfg.liq_threshold_bps} />

        <div className="actions">
        <div className="segmented" role="tablist">
          {(
            [
              ["borrow", "Borrow"],
              ["repay", "Repay"],
              ["withdraw", "Withdraw BTC"],
              ["transfer", "Transfer"],
            ] as [Action, string][]
          ).map(([id, label]) => (
            <button key={id} className={action === id ? "on" : ""} onClick={() => setAction(id)}>
              {label}
            </button>
          ))}
        </div>

        {action === "borrow" && (
          <AmountForm
            unit="USDC"
            cta="Borrow"
            max={borrowable}
            hint={
              !priceFresh
                ? `No BTC price right now (${ctx.priceError ?? "reporters unavailable"}); borrowing is paused until reporters agree again.`
                : sats === 0n
                  ? "Prove a Bitcoin deposit first."
                  : `Up to ${formatUsdc(borrowable)} USDC. The vault stays locked until the loan is repaid.`
            }
            disabled={ctx.busy || !priceFresh || borrowable === 0n}
            onSubmit={(amount) =>
              ctx.run(`Borrowing ${formatUsdc(amount)} USDC`, async () => {
                if (!balances.registered) {
                  await ctx.send({
                    receiverId: cfg.usdc,
                    method: "storage_deposit",
                    args: { account_id: ctx.session!.accountId, registration_only: true },
                    gas: 30_000_000_000_000n,
                    deposit: FT_STORAGE_DEPOSIT,
                  });
                }
                const res = await ctx.send(app.borrow(vault.vault_id, amount));
                return {
                  detail: res.value === false ? "Held for you in the contract: claim it under Lend." : "USDC is in your wallet.",
                  href: res.txHash && nearTxUrl(res.txHash),
                };
              })
            }
          />
        )}

        {action === "repay" && (
          <AmountForm
            unit="USDC"
            cta="Repay"
            // A little extra covers interest accruing meanwhile; the contract
            // takes only the debt and the token contract refunds the rest.
            max={debt === 0n ? 0n : minBig(debt + debt / 1000n + 1n, balances.usdc)}
            hint={
              debt === 0n
                ? "This vault has no debt."
                : `Debt ${formatUsdc(debt)} USDC · wallet ${formatUsdc(balances.usdc)} USDC. Repaying everything unlocks the vault.`
            }
            disabled={ctx.busy || debt === 0n}
            onSubmit={(amount) =>
              ctx.run(`Repaying ${formatUsdc(amount)} USDC`, async () => {
                if (amount > balances.usdc) throw new Error("Not enough USDC in your wallet.");
                const res = await ctx.send(app.repay(cfg.usdc, vault.vault_id, amount));
                return { href: res.txHash && nearTxUrl(res.txHash) };
              })
            }
          />
        )}

        {action === "withdraw" && <WithdrawForm ctx={ctx} vault={vault} onDone={loadOutputs} />}

        {action === "transfer" && (
          <TransferForm
            disabled={ctx.busy || vault.locked}
            hint={
              vault.locked
                ? "A vault that secures a loan cannot change hands. Repay it first."
                : "Whoever holds this NFT controls the Bitcoin key. Outputs marked “signed” do not count as collateral for the new owner."
            }
            onSubmit={(receiver) =>
              ctx.run(`Transferring vault #${vault.vault_id}`, async () => {
                const res = await ctx.send({
                  receiverId: D.contract,
                  method: "nft_transfer",
                  args: { receiver_id: receiver, token_id: String(vault.vault_id) },
                  gas: 50_000_000_000_000n,
                  deposit: 1n,
                });
                return { detail: `Now owned by ${receiver}.`, href: res.txHash && nearTxUrl(res.txHash) };
              })
            }
          />
        )}
        </div>
      </aside>
    </article>
  );
}

function minBig(a: bigint, b: bigint): bigint {
  return a < b ? a : b;
}

function DepositStatus({ o, min, busy, onProve }: { o: ChainOutput; min: number; busy: boolean; onProve(): void }) {
  if (o.state === "collateral") return <span className="status ok">Collateral</span>;
  if (o.state === "signed") {
    return (
      <span className="status warn" title="The contract has issued a signature spending this output, so it can never count as collateral.">
        Signed for spending
      </span>
    );
  }
  if (!o.status.confirmed) return <span className="status">Unconfirmed</span>;
  if (o.confirmations < min) {
    return (
      <span className="status">
        {o.confirmations}/{min} confirmations
      </span>
    );
  }
  return (
    <button className="btn small" disabled={busy} onClick={onProve}>
      Prove to NEAR
    </button>
  );
}

export function LtvBar({ ltvBps, maxBps, liqBps }: { ltvBps: number | null; maxBps: number; liqBps: number }) {
  const ltv = ltvBps === null ? 0 : Math.min(ltvBps, 10_000);
  const tone = ltv > liqBps ? "red" : ltv > maxBps ? "ochre" : "olive";
  return (
    <div className="ltv">
      <div className="ltv-track">
        <div className={`ltv-fill ${tone}`} style={{ width: `${ltv / 100}%` }} />
        <span className="ltv-mark" style={{ left: `${maxBps / 100}%` }} title="Maximum loan-to-value" />
        <span className="ltv-mark liq" style={{ left: `${liqBps / 100}%` }} title="Liquidation threshold" />
      </div>
      <div className="ltv-legend">
        <span>
          Loan-to-value <strong>{ltvBps === null ? "—" : pct(ltv)}</strong>
        </span>
        <span>
          borrow limit {pct(maxBps, 0)} · liquidation {pct(liqBps, 0)}
        </span>
      </div>
    </div>
  );
}

export function AmountForm(props: {
  unit: string;
  cta: string;
  max: bigint;
  hint: string;
  disabled: boolean;
  onSubmit(amount: bigint): void;
}) {
  const [text, setText] = useState("");
  const [error, setError] = useState<string>();
  return (
    <form
      className="row-form"
      onSubmit={(e) => {
        e.preventDefault();
        try {
          const amount = parseUsdc(text);
          if (amount === 0n) throw new Error("Enter an amount.");
          setError(undefined);
          props.onSubmit(amount);
          setText("");
        } catch (err) {
          setError(err instanceof Error ? err.message : String(err));
        }
      }}
    >
      <div className="field">
        <input value={text} onChange={(e) => setText(e.target.value)} placeholder="0.00" inputMode="decimal" aria-label={`Amount in ${props.unit}`} />
        <span className="unit">{props.unit}</span>
        <button
          type="button"
          className="link"
          onClick={() => setText((Number(props.max) / 1e6).toFixed(6).replace(/\.?0+$/, ""))}
          disabled={props.max === 0n}
        >
          Max
        </button>
      </div>
      <button className="btn primary" type="submit" disabled={props.disabled}>
        {props.cta}
      </button>
      <p className={`hint ${error ? "error" : ""}`}>{error ?? props.hint}</p>
    </form>
  );
}

function TransferForm(props: { disabled: boolean; hint: string; onSubmit(receiver: string): void }) {
  const [receiver, setReceiver] = useState("");
  return (
    <form
      className="row-form"
      onSubmit={(e) => {
        e.preventDefault();
        if (receiver.trim()) props.onSubmit(receiver.trim());
      }}
    >
      <div className="field">
        <input value={receiver} onChange={(e) => setReceiver(e.target.value)} placeholder="receiver.testnet" aria-label="Receiver account" />
      </div>
      <button className="btn primary" type="submit" disabled={props.disabled}>
        Transfer
      </button>
      <p className="hint">{props.hint}</p>
    </form>
  );
}

function WithdrawForm({ ctx, vault, onDone }: { ctx: Ctx; vault: Vault; onDone(): void }) {
  const [address, setAddress] = useState("");
  const [amount, setAmount] = useState("");
  const [error, setError] = useState<string>();
  const [result, setResult] = useState<{ txid: string; fee: string }>();

  const submit = () =>
    ctx.run("Signing a Bitcoin withdrawal", async () => {
      const destination = addressToScript(address.trim(), D.btcNetwork);
      const [utxos, feeRate] = await Promise.all([esplora.utxos(vault.btc_address), esplora.feeRate()]);
      const confirmed = utxos.filter((u) => u.status.confirmed);
      const plan = planWithdrawal(
        confirmed,
        destination,
        amount.trim() === "max" ? "max" : parseBtc(amount.trim()),
        vault.script_pubkey,
        feeRate,
      );

      const res = await ctx.send(app.signWithdrawal(vault.vault_id, plan, ctx.cfg.ledger_fee));
      const signed = res.value as SignedWithdrawal;
      // Trust nothing: recompute the digests and check the MPC signatures
      // before anything is broadcast.
      const check = verifyWithdrawal(signed.raw_tx, plan, vault);
      if (!check.pubkeyMatches || !check.signaturesValid) {
        throw new Error("The returned transaction did not verify against the vault key; not broadcasting.");
      }
      const txid = await esplora.broadcast(signed.raw_tx);
      setResult({ txid, fee: signed.fee_sats });
      onDone();
      return { detail: `Signed by NEAR's MPC network, verified locally, broadcast to Bitcoin.`, href: btcTxUrl(txid) };
    });

  return (
    <form
      className="stack-form"
      onSubmit={(e) => {
        e.preventDefault();
        try {
          addressToScript(address.trim(), D.btcNetwork);
          if (amount.trim() !== "max") parseBtc(amount.trim());
          setError(undefined);
          setResult(undefined);
          submit();
        } catch (err) {
          setError(err instanceof Error ? err.message : String(err));
        }
      }}
    >
      <div className="field">
        <input value={address} onChange={(e) => setAddress(e.target.value)} placeholder={D.btcNetwork === "testnet" ? "tb1q… destination address" : "bc1q… destination address"} aria-label="Destination Bitcoin address" />
      </div>
      <div className="row-form">
        <div className="field">
          <input value={amount} onChange={(e) => setAmount(e.target.value)} placeholder="0.00000000" aria-label="Amount in BTC" />
          <span className="unit">BTC</span>
          <button type="button" className="link" onClick={() => setAmount("max")}>
            Max
          </button>
        </div>
        <button className="btn primary" type="submit" disabled={ctx.busy}>
          Sign &amp; broadcast
        </button>
      </div>
      <p className={`hint ${error ? "error" : ""}`}>
        {error ??
          (vault.locked
            ? `While the loan is open, the contract signs only if the remaining collateral keeps it under ${pct(ctx.cfg.max_ltv_bps, 0)} LTV. Change returns to the vault and must be proven again.`
            : "The contract builds the transaction, NEAR's MPC network signs it, and your browser verifies the signatures before broadcasting.")}
      </p>
      {result && (
        <p className="notice ok">
          Broadcast{" "}
          <a href={btcTxUrl(result.txid)} target="_blank" rel="noreferrer">
            {short(result.txid, 10, 10)}
          </a>{" "}
          · network fee {result.fee} sats
        </p>
      )}
    </form>
  );
}
