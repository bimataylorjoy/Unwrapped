import { formatUsdc } from "@unwrapped/sdk";
import type { Ctx } from "./App.tsx";
import { AmountForm } from "./VaultCard.tsx";
import { D, app, nearTxUrl, pct } from "./chain.ts";

export function LendPanel({ ctx }: { ctx: Ctx }) {
  const { cfg, pool, balances } = ctx;
  const cash = BigInt(pool.cash);
  const withdrawable = balances.supplied < cash ? balances.supplied : cash;

  const faucet = () =>
    ctx.run("Getting test USDC", async () => {
      const res = await ctx.send({ receiverId: cfg.usdc, method: "faucet", args: {}, gas: 30_000_000_000_000n, deposit: 0n });
      return { detail: "10,000 test USDC added to your wallet.", href: res.txHash && nearTxUrl(res.txHash) };
    });

  return (
    <section>
      <div className="section-head">
        <h2>Lend USDC</h2>
        {ctx.session && D.network === "testnet" && (
          <button className="btn" disabled={ctx.busy} onClick={faucet}>
            Get 10,000 test USDC
          </button>
        )}
      </div>

      <div className="grid-2">
        <article className="card">
          <h3>The pool</h3>
          <dl className="metrics two">
            <div>
              <dt>Supply APR</dt>
              <dd>{pct(pool.supply_apr_bps, 2)}</dd>
              <span className="sub">paid by borrowers</span>
            </div>
            <div>
              <dt>Utilisation</dt>
              <dd>{pct(pool.utilization_bps)}</dd>
              <span className="sub">borrow APR {pct(pool.borrow_apr_bps, 2)}</span>
            </div>
            <div>
              <dt>Idle</dt>
              <dd>${formatUsdc(pool.cash, 0)}</dd>
              <span className="sub">available now</span>
            </div>
            <div>
              <dt>Lent out</dt>
              <dd>${formatUsdc(pool.total_debt, 0)}</dd>
              <span className="sub">against native BTC</span>
            </div>
          </dl>
          <p className="muted small">
            Borrow APR runs from {pct(cfg.base_rate_bps, 0)} at 0% utilisation to{" "}
            {pct(cfg.base_rate_bps + cfg.slope_rate_bps, 0)} at 100%. Every loan is backed by Bitcoin proven on chain at
            no more than {pct(cfg.max_ltv_bps, 0)} loan-to-value, and can be liquidated above {pct(cfg.liq_threshold_bps, 0)}.
          </p>
        </article>

        <article className="card">
          <h3>Your position</h3>
          {!ctx.session ? (
            <div className="empty inline">
              <p>Connect to supply USDC and earn the interest borrowers pay.</p>
              <button className="btn primary" onClick={ctx.connect}>
                Connect
              </button>
            </div>
          ) : (
            <>
              <dl className="metrics two">
                <div>
                  <dt>Supplied</dt>
                  <dd>
                    {formatUsdc(balances.supplied)} <small>USDC</small>
                  </dd>
                </div>
                <div>
                  <dt>In wallet</dt>
                  <dd>
                    {formatUsdc(balances.usdc)} <small>USDC</small>
                  </dd>
                </div>
              </dl>

              <span className="label">Supply</span>
              <AmountForm
                unit="USDC"
                cta="Supply"
                max={balances.usdc}
                hint={`Wallet balance ${formatUsdc(balances.usdc)} USDC.`}
                disabled={ctx.busy || balances.usdc === 0n}
                onSubmit={(amount) =>
                  ctx.run(`Supplying ${formatUsdc(amount)} USDC`, async () => {
                    if (amount > balances.usdc) throw new Error("Not enough USDC in your wallet.");
                    const res = await ctx.send(app.supply(cfg.usdc, amount));
                    return { href: res.txHash && nearTxUrl(res.txHash) };
                  })
                }
              />

              <span className="label">Withdraw</span>
              <AmountForm
                unit="USDC"
                cta="Withdraw"
                max={withdrawable}
                hint={
                  withdrawable < balances.supplied
                    ? `${formatUsdc(withdrawable)} USDC is idle right now; the rest is lent out.`
                    : `Up to ${formatUsdc(withdrawable)} USDC.`
                }
                disabled={ctx.busy || balances.shares === 0n}
                onSubmit={(amount) =>
                  ctx.run(`Withdrawing ${formatUsdc(amount)} USDC`, async () => {
                    if (amount > balances.supplied) throw new Error("That is more than you have supplied.");
                    // Burn shares in proportion; the full amount burns them all.
                    const shares = amount === balances.supplied ? balances.shares : (balances.shares * amount) / balances.supplied;
                    const res = await ctx.send(app.withdraw(shares));
                    return { href: res.txHash && nearTxUrl(res.txHash) };
                  })
                }
              />

              {balances.claimable > 0n && (
                <p className="notice ok">
                  {formatUsdc(balances.claimable)} USDC is held for you (liquidation surplus or an undelivered payout).{" "}
                  <button
                    className="link"
                    disabled={ctx.busy}
                    onClick={() =>
                      ctx.run("Claiming USDC", async () => {
                        const res = await ctx.send(app.claim());
                        return { href: res.txHash && nearTxUrl(res.txHash) };
                      })
                    }
                  >
                    Claim
                  </button>
                </p>
              )}
            </>
          )}
        </article>
      </div>
    </section>
  );
}
